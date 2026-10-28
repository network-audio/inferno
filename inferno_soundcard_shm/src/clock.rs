//! The media clock in shared memory: the service publishes the device's clock overlay (a shift and frequency
//! correction over a local clock, see `inferno_aoip::clock::ClockOverlay`), the plug-in reads it lock-free on
//! Core Audio's I/O threads.

use std::sync::atomic::{fence, AtomicI64, AtomicU32, AtomicU64, Ordering};

/// The media clock over an underlying local clock: the same as `inferno_aoip::clock::ClockOverlay`, and the
/// same arithmetic, so that both processes agree on every frame's time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Overlay {
  /// on Unix, the `clockid_t` of the underlying clock
  pub clock_id: i64,
  pub last_sync: i64,
  pub shift: i64,
  pub freq_scale: f64,
}

impl Overlay {
  /// Media clock time (ns, may wrap) at `underlying_ns` of the underlying clock.
  #[inline]
  pub fn overlay_ns(&self, underlying_ns: i64) -> i64 {
    let elapsed = underlying_ns.wrapping_sub(self.last_sync);
    let correction = ((elapsed as f64) * self.freq_scale).round() as i64;
    underlying_ns.wrapping_add(self.shift).wrapping_add(correction)
  }

  /// Frames of the media clock at `rate` (since its epoch, wrapping) at `underlying_ns`, as Inferno counts them
  /// (`MediaClock::wrapping_now_in_timebase`).
  #[inline]
  pub fn frames(&self, underlying_ns: i64, rate: u32) -> usize {
    let ns = self.overlay_ns(underlying_ns) as u64;
    ((ns as u128) * (rate as u128) / 1_000_000_000) as u64 as usize
  }

  /// Underlying clock nanoseconds from `underlying_now` until the media clock reaches frame `frame` (since its
  /// epoch), negative if it has.
  pub fn underlying_ns_until(&self, underlying_now: i64, frame: usize, rate: u32) -> i64 {
    // the first nanosecond counted as `frame`
    let target = ((frame as u64 as u128) * 1_000_000_000).div_ceil(rate as u128) as u64;
    let now = self.overlay_ns(underlying_now) as u64;
    let media = target.wrapping_sub(now) as i64;
    (media as f64 / (1.0 + self.freq_scale)).round() as i64
  }
}

/// The underlying clock's time now, in nanoseconds.
#[cfg(unix)]
#[inline]
pub fn underlying_now_ns(clock_id: i64) -> i64 {
  let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
  // safety: ts is a valid timespec
  unsafe { libc::clock_gettime(clock_id as libc::clockid_t, &mut ts) };
  (ts.tv_sec as i64).wrapping_mul(1_000_000_000).wrapping_add(ts.tv_nsec as i64)
}

/// The clock overlay behind a sequence lock: one writer (the service), readers that never block it.
#[repr(C)]
#[derive(Default)]
pub struct SharedClock {
  /// odd while being written
  seq: AtomicU32,
  available: AtomicU32,
  clock_id: AtomicI64,
  last_sync: AtomicI64,
  shift: AtomicI64,
  freq_scale: AtomicU64,
}

impl SharedClock {
  /// Publishes the clock (`None`: unavailable). Only one thread may publish.
  pub fn publish(&self, overlay: Option<Overlay>) {
    let seq = self.seq.load(Ordering::Relaxed);
    self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
    fence(Ordering::Release);
    self.available.store(overlay.is_some() as u32, Ordering::Relaxed);
    if let Some(o) = overlay {
      self.clock_id.store(o.clock_id, Ordering::Relaxed);
      self.last_sync.store(o.last_sync, Ordering::Relaxed);
      self.shift.store(o.shift, Ordering::Relaxed);
      self.freq_scale.store(o.freq_scale.to_bits(), Ordering::Relaxed);
    }
    self.seq.store(seq.wrapping_add(2), Ordering::Release);
  }

  /// The clock, if available. Retries while it's being published (which takes nanoseconds).
  pub fn read(&self) -> Option<Overlay> {
    loop {
      let before = self.seq.load(Ordering::Acquire);
      if before & 1 == 1 {
        std::hint::spin_loop();
        continue;
      }
      let available = self.available.load(Ordering::Relaxed) != 0;
      let overlay = Overlay {
        clock_id: self.clock_id.load(Ordering::Relaxed),
        last_sync: self.last_sync.load(Ordering::Relaxed),
        shift: self.shift.load(Ordering::Relaxed),
        freq_scale: f64::from_bits(self.freq_scale.load(Ordering::Relaxed)),
      };
      fence(Ordering::Acquire);
      if self.seq.load(Ordering::Relaxed) == before {
        return available.then_some(overlay);
      }
    }
  }
}

/// Core Audio's clock of a device: "zero time stamps", every `period` frames of its sample time, with the host
/// time of each. The device's sample time 0 is position `origin` of the rings (frames since the engine's
/// start, which is frame `start_time` of the media clock).
#[derive(Debug, Clone, Copy)]
pub struct ZeroTimeStamps {
  pub rate: u32,
  pub start_time: usize,
  pub origin: usize,
  pub period: usize,
}

impl ZeroTimeStamps {
  /// The latest zero time stamp at `underlying_now`: its sample time, and the underlying clock nanoseconds from
  /// `underlying_now` until it (zero or negative).
  pub fn latest(&self, overlay: &Overlay, underlying_now: i64) -> (u64, i64) {
    let position = overlay.frames(underlying_now, self.rate).wrapping_sub(self.start_time);
    let since_origin = position.wrapping_sub(self.origin) as isize;
    let periods = since_origin.max(0) as usize / self.period;
    let sample_time = periods * self.period;
    let frame = self.start_time.wrapping_add(self.origin).wrapping_add(sample_time);
    (sample_time as u64, overlay.underlying_ns_until(underlying_now, frame, self.rate).min(0))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const OVERLAY: Overlay = Overlay {
    clock_id: 1,
    last_sync: 1_000_000_000,
    shift: 1_700_000_000_000_000_000,
    freq_scale: 20e-6,
  };

  #[test]
  fn frames_start_when_their_time_is_reached() {
    for rate in [44100, 48000, 96000] {
      for now in [1_000_000_000i64, 1_234_567_891, 98_765_432_101] {
        let frame = OVERLAY.frames(now, rate) + 1;
        let until = OVERLAY.underlying_ns_until(now, frame, rate);
        assert!(until > 0);
        assert_eq!(OVERLAY.frames(now + until, rate), frame, "{rate} Hz, {now}: frame reached");
        assert_eq!(OVERLAY.frames(now + until - 2, rate), frame - 1, "{rate} Hz, {now}: not before");
      }
    }
  }

  #[test]
  fn readers_see_what_was_published() {
    let clock = SharedClock::default();
    assert_eq!(clock.read(), None);
    clock.publish(Some(OVERLAY));
    assert_eq!(clock.read(), Some(OVERLAY));
    clock.publish(None);
    assert_eq!(clock.read(), None);
  }

  #[test]
  fn zero_time_stamps_are_periods_of_the_media_clock() {
    let rate = 48000;
    let now = 5_000_000_000;
    let position = OVERLAY.frames(now, rate);
    let stamps = ZeroTimeStamps { rate, start_time: position - 100_000, origin: 50_000, period: 16384 };
    let (sample_time, until) = stamps.latest(&OVERLAY, now);
    // 50 000 frames since the origin: three periods
    assert_eq!(sample_time, 3 * 16384);
    assert!(until <= 0);
    let at = now + until;
    let expected = stamps.start_time + stamps.origin + sample_time as usize;
    assert_eq!(OVERLAY.frames(at, rate), expected);
    assert_eq!(OVERLAY.frames(at - 1, rate), expected - 1);
    // before the origin: sample time 0, in the future: not reported ahead of time
    let early = ZeroTimeStamps { origin: 200_000, ..stamps };
    assert_eq!(early.latest(&OVERLAY, now), (0, 0));
  }
}
