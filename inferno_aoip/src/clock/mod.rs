//! Media clock sources.
//!
//! The media clock follows the PTP clock of the Dante network. A [`ClockSource`] delivers it as a
//! [`ClockOverlay`]: a shift and frequency correction over an underlying local clock, which can be read
//! without a system call to the PTP daemon, on the real-time threads.
//!
//! On Unix, the default source is a PTP daemon (Statime) speaking the usrvclock protocol, see
//! [`UsrvClockSource`]. Other platforms (or applications with their own PTP implementation) supply a
//! source to [`crate::device_server::DeviceServer::try_start_with_clock`].

use std::{path::PathBuf, sync::Arc};

use futures::future::BoxFuture;
use tokio::sync::watch;

#[cfg(unix)]
mod usrvclock_source;
#[cfg(unix)]
pub use usrvclock_source::{clock_path_or_default, diagnose_missing_clock, UsrvClockSource};

/// Clock overlay: the media clock as a shift and frequency correction over an underlying local clock.
///
/// `clock_id` identifies the underlying clock: on Unix, a `clockid_t` (e.g. `CLOCK_MONOTONIC`,
/// `CLOCK_REALTIME`, or a dynamic clock of a PTP hardware clock device); elsewhere only
/// [`MONOTONIC_CLOCK_ID`] is supported.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockOverlay {
  pub clock_id: i64,
  pub last_sync: i64,
  pub shift: i64,
  pub freq_scale: f64,
}

/// `clock_id` of the platform's monotonic clock, see [`ClockOverlay`].
#[cfg(unix)]
pub const MONOTONIC_CLOCK_ID: i64 = libc::CLOCK_MONOTONIC as i64;
/// `clock_id` of the platform's monotonic clock, see [`ClockOverlay`].
#[cfg(not(unix))]
pub const MONOTONIC_CLOCK_ID: i64 = 1;

impl ClockOverlay {
  /// An overlay which is the underlying monotonic clock itself (not synchronised to anything).
  pub fn monotonic() -> Self {
    Self { clock_id: MONOTONIC_CLOCK_ID, last_sync: 0, shift: 0, freq_scale: 0.0 }
  }

  /// Calculates timestamp in overlay clock's timescale given underlying clock's timestamp.
  /// All timestamps are in nanoseconds and may wrap.
  #[inline(always)]
  pub fn underlying_to_overlay_ns(&self, timestamp: i64) -> i64 {
    let elapsed = timestamp.wrapping_sub(self.last_sync);
    let correction = ((elapsed as f64) * self.freq_scale).round() as i64;
    timestamp.wrapping_add(self.shift).wrapping_add(correction)
  }

  /// Returns current underlying clock's timestamp in nanoseconds. May wrap.
  #[inline(always)]
  pub fn now_underlying_ns(&self) -> i64 {
    underlying_clock_now_ns(self.clock_id)
  }

  /// Returns current timestamp (in nanoseconds) obtained from underlying clock, then shifted and scaled to
  /// overlay clock's timescale. May wrap.
  #[inline(always)]
  pub fn now_ns(&self) -> i64 {
    self.underlying_to_overlay_ns(self.now_underlying_ns())
  }

  /// Returns actual frequency correction factor, adjusted for hardware clock's scale.
  #[cfg(target_os = "linux")]
  pub fn freq_scale_including_hw(&self) -> f64 {
    // safety: an all-zero timex is valid, modes 0 only reads
    let mut timex: libc::timex = unsafe { std::mem::zeroed() };
    // TODO: handle errors
    let _r = unsafe { libc::clock_adjtime(self.clock_id as libc::clockid_t, &mut timex) };
    (timex.freq as f64 / 65_536_000_000.0) + self.freq_scale
  }

  /// Returns actual frequency correction factor. Hardware clocks aren't steered on this platform.
  #[cfg(not(target_os = "linux"))]
  pub fn freq_scale_including_hw(&self) -> f64 {
    self.freq_scale
  }
}

#[cfg(unix)]
#[inline(always)]
fn underlying_clock_now_ns(clock_id: i64) -> i64 {
  let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
  // safety: ts is a valid timespec. An invalid (e.g. closed) clock leaves it zeroed.
  unsafe { libc::clock_gettime(clock_id as libc::clockid_t, &mut ts) };
  (ts.tv_sec as i64).wrapping_mul(1_000_000_000).wrapping_add(ts.tv_nsec as i64)
}

/// The performance counter: monotonic and the same for all processes, so a PTP service in another
/// process can provide overlays over it.
#[cfg(windows)]
#[inline(always)]
fn underlying_clock_now_ns(_clock_id: i64) -> i64 {
  use std::sync::OnceLock;

  use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
  static FREQUENCY: OnceLock<i64> = OnceLock::new();
  let frequency = *FREQUENCY.get_or_init(|| {
    let mut f = 0;
    // safety: valid pointer; never fails on Windows XP and later
    unsafe { QueryPerformanceFrequency(&mut f) };
    f
  });
  let mut counter = 0;
  // safety: valid pointer; never fails on Windows XP and later
  unsafe { QueryPerformanceCounter(&mut counter) };
  ((counter as i128) * 1_000_000_000 / (frequency as i128)) as i64
}

#[cfg(not(any(unix, windows)))]
#[inline(always)]
fn underlying_clock_now_ns(_clock_id: i64) -> i64 {
  use std::{sync::OnceLock, time::Instant};
  static EPOCH: OnceLock<Instant> = OnceLock::new();
  EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as i64
}

/// Source of the media clock (PTP time of the Dante network) for a device.
///
/// Created inside the tokio runtime the device runs in, if it spawns tasks.
pub trait ClockSource: Send + Sync {
  /// Clock overlay updates. `None` until the clock is available.
  fn subscribe(&self) -> watch::Receiver<Option<ClockOverlay>>;
  /// Where the clock comes from, for logs.
  fn description(&self) -> String;
  /// Why no clock arrives, and what to check. For error messages when the clock doesn't become available.
  fn diagnose_missing(&self) -> String;
  /// Stops the source. Called when a device which created the source (the default one) shuts down.
  fn shutdown(&self) -> BoxFuture<'static, ()> {
    Box::pin(async {})
  }
}

/// The local monotonic clock, not synchronised to the network.
///
/// For tests, and for setups where Inferno is the only clock leader on the network: Dante devices don't
/// follow it, so audio exchanged with them will drift and glitch.
pub struct FreeRunningClockSource {
  sender: watch::Sender<Option<ClockOverlay>>,
}

impl FreeRunningClockSource {
  pub fn new() -> Self {
    Self { sender: watch::Sender::new(Some(ClockOverlay::monotonic())) }
  }
}

impl Default for FreeRunningClockSource {
  fn default() -> Self {
    Self::new()
  }
}

impl ClockSource for FreeRunningClockSource {
  fn subscribe(&self) -> watch::Receiver<Option<ClockOverlay>> {
    self.sender.subscribe()
  }
  fn description(&self) -> String {
    "local monotonic clock (free-running, not synchronised to PTP)".to_string()
  }
  fn diagnose_missing(&self) -> String {
    "BUG: the free-running clock is always available".to_string()
  }
}

/// The clock source a device uses unless given one: the PTP daemon at `clock_path` (`CLOCK_PATH`) on Unix.
/// Must be called inside a tokio runtime.
#[cfg(unix)]
pub fn default_clock_source(clock_path: Option<PathBuf>) -> Arc<dyn ClockSource> {
  Arc::new(UsrvClockSource::start(clock_path))
}

/// The clock source a device uses unless given one. There's no PTP daemon integration on this platform
/// yet, so it's the free-running local clock: give the device a [`ClockSource`] to synchronise it.
#[cfg(not(unix))]
pub fn default_clock_source(clock_path: Option<PathBuf>) -> Arc<dyn ClockSource> {
  use log::warn;
  if clock_path.is_some() {
    warn!("CLOCK_PATH is not supported on this platform, ignoring it");
  }
  warn!(
    "no PTP clock source on this platform: using the local clock, not synchronised to the Dante \
     network. Audio exchanged with other devices will drift."
  );
  Arc::new(FreeRunningClockSource::new())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn identity_overlay_follows_underlying_clock() {
    let overlay = ClockOverlay::monotonic();
    let underlying = overlay.now_underlying_ns();
    let now = overlay.now_ns();
    assert!(now >= underlying && now - underlying < 1_000_000_000, "{underlying} {now}");
  }

  #[test]
  fn overlay_shifts_and_scales() {
    let overlay =
      ClockOverlay { clock_id: MONOTONIC_CLOCK_ID, last_sync: 1000, shift: 50, freq_scale: 0.001 };
    assert_eq!(overlay.underlying_to_overlay_ns(1000), 1050);
    assert_eq!(overlay.underlying_to_overlay_ns(1_001_000), 1_001_000 + 50 + 1000);
  }

  #[test]
  fn monotonic_clock_advances() {
    let overlay = ClockOverlay::monotonic();
    let a = overlay.now_ns();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let b = overlay.now_ns();
    assert!(b - a >= 1_000_000, "{a} {b}");
  }

  #[test]
  fn free_running_source_is_available_immediately() {
    let source = FreeRunningClockSource::new();
    assert_eq!(*source.subscribe().borrow(), Some(ClockOverlay::monotonic()));
  }
}
