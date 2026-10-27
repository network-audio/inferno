use crate::common::Clock;
use crate::ring_buffer::BuffersValid;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const PAUSED: usize = usize::MAX;

/// Ties the buffer positions of an externally clocked stream (e.g. an ALSA PCM, which counts
/// frames from 0 since its start) to the media clock, and reports the stream's network latency.
///
/// The stream can be paused and re-anchored at any time without stopping the receiver or
/// transmitter, so restarting a PCM (transport stop/start, xrun recovery) doesn't tear down
/// Dante flows and subscriptions.
///
/// While paused, the receiver doesn't touch the buffer and the transmitter sends silence.
///
/// If it guards the validity flag of external buffers, the anchor marks them valid only once the
/// receiver/transmitter has applied the current start time (`activate`), so they're never accessed with
/// the addressing of a previous start; pausing marks them invalid.
#[derive(Clone)]
pub struct StreamAnchor(Arc<Shared>);

struct Shared {
  start_time: AtomicUsize,
  network_latency_samples: AtomicUsize,
  buffers_valid: Option<Arc<BuffersValid>>,
}

impl StreamAnchor {
  pub fn new_paused() -> Self {
    Self::new(None)
  }
  /// Like `new_paused`, also guarding the validity flag of the stream's external buffers (see type docs).
  pub fn new_paused_guarding(buffers_valid: Arc<BuffersValid>) -> Self {
    Self::new(Some(buffers_valid))
  }
  fn new(buffers_valid: Option<Arc<BuffersValid>>) -> Self {
    Self(Arc::new(Shared {
      start_time: AtomicUsize::new(PAUSED),
      network_latency_samples: AtomicUsize::new(0),
      buffers_valid,
    }))
  }
  pub fn new_started(start_time: Clock) -> Self {
    let r = Self::new_paused();
    r.start(start_time);
    r
  }

  /// Buffer position 0 corresponds to media clock `start_time` (in samples, wrapping).
  pub fn start(&self, start_time: Clock) {
    // PAUSED is a valid wrapping timestamp too, shift it by one sample rather than lose the start
    let value = if start_time == PAUSED { PAUSED - 1 } else { start_time };
    self.0.start_time.store(value, Ordering::Release);
  }
  /// Also marks guarded buffers invalid, waiting for any access in progress to finish.
  pub fn pause(&self) {
    self.0.start_time.store(PAUSED, Ordering::Release);
    if let Some(valid) = &self.0.buffers_valid {
      valid.invalidate();
    }
  }
  /// Called by the receiver/transmitter once it addresses the buffers according to `applied_start_time`:
  /// marks guarded buffers valid, unless the stream has been paused or re-anchored meanwhile.
  pub(crate) fn activate(&self, applied_start_time: Clock) {
    if let Some(valid) = &self.0.buffers_valid {
      // checked with changes excluded: pause() changes start_time before invalidating
      valid.validate_if(|| self.start_time() == Some(applied_start_time));
    }
  }
  /// `None` if paused
  pub fn start_time(&self) -> Option<Clock> {
    match self.0.start_time.load(Ordering::Acquire) {
      PAUSED => None,
      v => Some(v),
    }
  }

  /// Samples between media time and the time the stream's audio is captured from (RX) or
  /// played out to (TX) the network, i.e. what has to be added to buffer-level delay
  /// to get the real delay.
  pub fn network_latency_samples(&self) -> usize {
    self.0.network_latency_samples.load(Ordering::Relaxed)
  }
  pub(crate) fn set_network_latency_samples(&self, samples: usize) {
    self.0.network_latency_samples.store(samples, Ordering::Relaxed);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn starts_paused() {
    assert_eq!(StreamAnchor::new_paused().start_time(), None);
  }

  #[test]
  fn start_pause_restart() {
    let a = StreamAnchor::new_paused();
    let b = a.clone();
    a.start(1234);
    assert_eq!(b.start_time(), Some(1234));
    a.pause();
    assert_eq!(b.start_time(), None);
    a.start(5678);
    assert_eq!(b.start_time(), Some(5678));
  }

  #[test]
  fn start_at_sentinel_value_is_not_paused() {
    let a = StreamAnchor::new_started(usize::MAX);
    assert_eq!(a.start_time(), Some(usize::MAX - 1));
  }

  /// The buffers become valid only once the receiver/transmitter applied the current start time, so it can never
  /// write or read them with the addressing of a previous start.
  #[test]
  fn buffers_are_valid_only_after_activation_of_current_start() {
    let valid = BuffersValid::new(false);
    let a = StreamAnchor::new_paused_guarding(valid.clone());
    a.start(1000);
    assert!(!valid.is_valid(), "start alone must not make buffers valid");
    a.activate(999);
    assert!(!valid.is_valid(), "activation of another start");
    a.activate(1000);
    assert!(valid.is_valid());
    a.pause();
    assert!(!valid.is_valid(), "pause invalidates");
    a.activate(1000);
    assert!(!valid.is_valid(), "activation after pause");
    a.start(2000);
    a.activate(1000);
    assert!(!valid.is_valid(), "late activation of the previous start");
    a.activate(2000);
    assert!(valid.is_valid());
  }

  #[test]
  fn activation_without_guard_is_harmless() {
    let a = StreamAnchor::new_started(5);
    a.activate(5);
    a.pause();
  }

  #[test]
  fn latency_is_shared() {
    let a = StreamAnchor::new_paused();
    a.clone().set_network_latency_samples(480);
    assert_eq!(a.network_latency_samples(), 480);
  }
}
