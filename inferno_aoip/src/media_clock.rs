use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::RwLock;
use std::time::{Duration, Instant};

pub use usrvclock::AsyncClient as ClockReceiver;
pub use usrvclock::ClockOverlay;
use usrvclock::SafeClock;

use crate::common::*;
use crate::util::real_time_box_channel::{self, RealTimeBoxReceiver};
pub type RealTimeClockReceiver = RealTimeBoxReceiver<Option<ClockOverlay>>;

/// High-precision clock (nanoseconds)
pub type FineClock = u64;

/// Signed version of the high-precision clock. For clock deltas.
pub type FineClockDiff = i64;

//#[derive(Clone)]
pub struct MediaClock {
  overlay: Option<ClockOverlay>,
}

#[inline(always)]
fn timestamp_to_clock_value(ts: clock_steering::Timestamp) -> FineClock {
  (ts.seconds as FineClock).wrapping_mul(1_000_000_000).wrapping_add(ts.nanos as FineClock)
}

impl MediaClock {
  pub fn new(use_safe_clock: bool) -> Self {
    assert!(!use_safe_clock);
    Self { overlay: None }
  }
  pub fn is_ready(&self) -> bool {
    self.overlay.is_some()
  }
  pub fn get_overlay(&self) -> &Option<ClockOverlay> {
    &self.overlay
  }
  pub fn update_overlay(&mut self, overlay: ClockOverlay) {
    /* if let Some(cur_overlay) = self.overlay {
      let cur_ovl_time = cur_overlay.now_ns();
      let new_ovl_time = overlay.now_ns();
      let diff = (new_ovl_time as ClockDiff).wrapping_sub(cur_ovl_time as ClockDiff);
      /* if diff.abs() > 10_000_000 {
        error!("clock is trying to jump dangerously by {diff} ns, ignoring update");
        return;
      } */
    } */
    self.overlay = Some(overlay);
  }
  #[inline(always)]
  pub fn now_ns(&self) -> Option<FineClock> {
    self.overlay.map(|overlay| overlay.now_ns() as FineClock)
  }
  #[inline(always)]
  pub fn now_in_timebase(&self, timebase_hz: u64) -> Option<LongClock> {
    self.now_ns().map(|ns| {
      // TODO it will jump when underlying wraps
      ((ns as u128) * (timebase_hz as u128) / 1_000_000_000u128) as LongClock
    })
  }
  #[inline(always)]
  pub fn wrapping_now_in_timebase(&self, timebase_hz: u64) -> Option<Clock> {
    self.now_in_timebase(timebase_hz).map(|x| x as Clock)
  }
  pub fn system_clock_duration_until(
    &mut self,
    timestamp: LongClock,
    timebase_hz: u64,
  ) -> Option<std::time::Duration> {
    let now_ns = self.now_ns()?;
    let until_ns = (timestamp as u128 * 1_000_000_000u128 / timebase_hz as u128) as FineClock;
    let remaining = (until_ns as FineClockDiff).wrapping_sub(now_ns as FineClockDiff);
    let corr = (remaining as f64 * self.overlay?.freq_scale) as FineClockDiff;
    let duration = remaining - corr; // FIXME it should be * 1/(1+freq_scale) but should be good enough for low correction values
    if duration > 0 {
      Some(std::time::Duration::from_nanos(duration as u64))
    } else {
      Some(std::time::Duration::from_secs(0))
    }
  }
  pub fn system_clock_duration_from_until(
    &mut self,
    from: Clock,
    until: Clock,
    timebase_hz: u64,
  ) -> Option<std::time::Duration> {
    let duration_in_tb = wrapped_diff(until, from);
    let duration_ns = (duration_in_tb as i64 * 1_000_000_000i64 / timebase_hz as i64) as FineClockDiff;
    let corr = (duration_ns as f64 * self.overlay?.freq_scale) as FineClockDiff;
    let duration = duration_ns - corr; // FIXME it should be * 1/(1+freq_scale) but should be good enough for low correction values
    if duration > 0 {
      Some(std::time::Duration::from_nanos(duration as u64))
    } else {
      Some(std::time::Duration::from_secs(0))
    }
  }
}

/// The clock source of `CLOCK_PATH`: a usrvclock socket of the PTP daemon, or a PTP hardware clock device.
pub fn clock_path_or_default(path: &Option<PathBuf>) -> PathBuf {
  path.clone().unwrap_or(usrvclock::DEFAULT_SERVER_SOCKET_PATH.into())
}

/// Why no clock arrives from `path`, and what to check.
pub fn diagnose_missing_clock(path: &Path) -> String {
  use std::os::unix::fs::FileTypeExt;
  let shown = path.display();
  match std::fs::metadata(path) {
    Err(_) => format!(
      "nothing at {shown}: is the PTP daemon (Statime) running? Its usrvclock socket must be at CLOCK_PATH \
       (default {})",
      usrvclock::DEFAULT_SERVER_SOCKET_PATH
    ),
    Ok(m) if m.file_type().is_socket() => format!(
      "{shown} exists but sends no clock updates: is the PTP daemon still running, and does it have a \
       clock leader to follow? (If the application runs in a sandbox or with a private /tmp, it can't reach \
       the daemon.)"
    ),
    Ok(m) if m.file_type().is_char_device() => {
      format!("{shown} is a PTP hardware clock but gives no time: can this user read it?")
    }
    Ok(_) => format!("{shown} is neither a usrvclock socket nor a PTP hardware clock device: check CLOCK_PATH"),
  }
}

pub fn start_clock_receiver(path: Option<PathBuf>) -> ClockReceiver {
  ClockReceiver::start(clock_path_or_default(&path), Box::new(|e| warn!("clock receive error: {e:?}")))
}

/// How long without a clock before it is reported as an error.
const CLOCK_MISSING_REPORT_AFTER: Duration = Duration::from_secs(5);

/// A media clock following `receiver`. Reports once, as an error, if no clock arrives from `path` in time.
pub async fn make_shared_media_clock(
  receiver: &ClockReceiver,
  use_safe_clock: bool,
  path: PathBuf,
) -> Arc<RwLock<MediaClock>> {
  let mut rx = receiver.subscribe();
  let media_clock = MediaClock::new(use_safe_clock);
  /* loop {
    match rx.recv().await {
      Ok(overlay) => {
        media_clock.update_overlay(overlay);
        break;
      }
      Err(broadcast::error::RecvError::Closed) => {
        panic!("ClockReceiver channel closed during initial await");
      },
      Err(e) => {
        warn!("clock receive error {e:?}");
      }
    }
  } */
  // initial await makes e.g. Audacity freeze when starting when Statime is not running. TODO figure it out
  let media_clock = Arc::new(RwLock::new(media_clock));
  let media_clock1 = media_clock.clone();
  tokio::spawn(async move {
    let start = Instant::now();
    let mut available = false;
    let mut reported_missing = false;
    loop {
      let overlay_opt = rx.borrow_and_update().clone();
      if let Some(overlay) = overlay_opt {
        media_clock.write().unwrap().update_overlay(overlay);
        if !available {
          available = true;
          info!("clock available from {} after {:?}", path.display(), start.elapsed());
        }
      }
      tokio::select! {
        changed = rx.changed() => if changed.is_err() {
          break;
        },
        _ = tokio::time::sleep(CLOCK_MISSING_REPORT_AFTER), if !available && !reported_missing => {
          error!("no clock after {CLOCK_MISSING_REPORT_AFTER:?}: {}", diagnose_missing_clock(&path));
          reported_missing = true;
        }
      }
    }
  });
  media_clock1
}

pub fn async_clock_receiver_to_realtime(
  mut receiver: tokio::sync::watch::Receiver<Option<ClockOverlay>>,
  initial: Option<ClockOverlay>,
) -> RealTimeBoxReceiver<Option<ClockOverlay>> {
  let (rt_sender, rt_recv) = real_time_box_channel::channel(Box::new(initial));
  tokio::spawn(async move {
    loop {
      let overlay_opt = receiver.borrow_and_update().clone();
      if let Some(overlay) = overlay_opt {
        rt_sender.send(Box::new(Some(overlay)));
      }
      if receiver.changed().await.is_err() {
        break;
      }
    }
  });
  rt_recv
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn timestamp_to_clock_value_basic() {
    let ts = clock_steering::Timestamp { seconds: 1, nanos: 500_000_000 };
    assert_eq!(timestamp_to_clock_value(ts), 1_500_000_000);
  }

  #[test]
  fn timestamp_to_clock_value_zero() {
    let ts = clock_steering::Timestamp { seconds: 0, nanos: 0 };
    assert_eq!(timestamp_to_clock_value(ts), 0);
  }

  #[test]
  fn timestamp_to_clock_value_large() {
    let ts = clock_steering::Timestamp { seconds: i64::MAX, nanos: 999_999_999 };
    let expected = (i64::MAX as u64).wrapping_mul(1_000_000_000).wrapping_add(999_999_999);
    assert_eq!(timestamp_to_clock_value(ts), expected);
  }

  #[test]
  fn media_clock_new_not_ready() {
    let clock = MediaClock::new(false);
    assert!(!clock.is_ready());
    assert!(clock.get_overlay().is_none());
  }

  #[test]
  fn media_clock_update_overlay_becomes_ready() {
    let mut clock = MediaClock::new(false);
    let overlay = ClockOverlay { clock_id: 1, last_sync: 0, shift: 0, freq_scale: 0.0 };
    clock.update_overlay(overlay);
    assert!(clock.is_ready());
    assert!(clock.get_overlay().is_some());
  }

  #[test]
  fn media_clock_update_overlay_replaces() {
    let mut clock = MediaClock::new(false);
    let overlay1 = ClockOverlay { clock_id: 1, last_sync: 100, shift: 0, freq_scale: 0.0 };
    let overlay2 = ClockOverlay { clock_id: 1, last_sync: 200, shift: 50, freq_scale: 0.001 };
    clock.update_overlay(overlay1);
    assert_eq!(clock.get_overlay().unwrap().clock_id, 1);
    assert_eq!(clock.get_overlay().unwrap().last_sync, 100);
    clock.update_overlay(overlay2);
    assert_eq!(clock.get_overlay().unwrap().clock_id, 1);
    assert_eq!(clock.get_overlay().unwrap().last_sync, 200);
  }

  #[test]
  fn missing_clock_diagnosis_says_what_is_at_the_path() {
    let dir = std::env::temp_dir().join(format!("inferno-clock-diag-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let missing = dir.join("missing");
    let d = diagnose_missing_clock(&missing);
    assert!(d.contains(&missing.display().to_string()) && d.contains("Statime"), "{d}");

    let socket_path = dir.join("socket");
    let _ = std::fs::remove_file(&socket_path);
    let _socket = std::os::unix::net::UnixDatagram::bind(&socket_path).unwrap();
    let d = diagnose_missing_clock(&socket_path);
    assert!(d.contains("sends no clock updates"), "{d}");

    let file = dir.join("file");
    std::fs::write(&file, b"").unwrap();
    let d = diagnose_missing_clock(&file);
    assert!(d.contains("check CLOCK_PATH"), "{d}");
    let _ = std::fs::remove_dir_all(&dir);
  }
}
