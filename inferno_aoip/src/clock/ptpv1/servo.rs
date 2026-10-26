//! Follows the leader's time with a line fitted through recent Sync measurements: the leader's offset from
//! the local clock and their frequency difference. Software timestamps are noisy (scheduling jitter), so
//! a fit over several seconds with outlier rejection beats steering by each measurement.

use std::collections::VecDeque;

use crate::clock::{ClockOverlay, MONOTONIC_CLOCK_ID};

/// Sync measurements in the fit.
const WINDOW: usize = 64;
/// Sync measurements before the clock is published.
const MIN_SAMPLES: usize = 16;
const DELAY_WINDOW: usize = 8;
/// A measurement this far off the fit is an outlier, or the leader's clock stepped.
const STEP_THRESHOLD_NS: i64 = 1_000_000;
/// Outliers in a row taken as a step of the leader's clock: start over.
const STEPS_TO_RESET: u32 = 3;
/// Outlier rejection within the window: residuals beyond this many median absolute deviations...
const OUTLIER_MADS: f64 = 4.0;
/// ...but never closer than this (timestamps are only so precise).
const OUTLIER_MIN_NS: f64 = 20_000.0;
/// Frequency difference beyond this is a measurement error (crystals are within ~100 ppm).
const MAX_FREQ: f64 = 500e-6;

/// The leader's time (without the path delay) as a function of local time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
  local_ref: i64,
  /// leader time minus local time at `local_ref`
  offset_ref: i64,
  freq: f64,
}

impl Fit {
  /// Leader time at which a message received at local time `local` was sent (no path delay).
  pub fn leader_without_delay(&self, local: i64) -> i64 {
    local
      .wrapping_add(self.offset_ref)
      .wrapping_add((local.wrapping_sub(self.local_ref) as f64 * self.freq).round() as i64)
  }
}

#[derive(Default)]
pub struct Servo {
  /// (local receive time, leader send time) of Syncs
  samples: VecDeque<(i64, i64)>,
  delays: VecDeque<i64>,
  fit: Option<Fit>,
  outliers_in_row: u32,
}

fn median(values: &mut [f64]) -> f64 {
  values.sort_by(|a, b| a.partial_cmp(b).unwrap());
  let n = values.len();
  if n % 2 == 1 {
    values[n / 2]
  } else {
    (values[n / 2 - 1] + values[n / 2]) / 2.0
  }
}

/// Least squares line `y = a + f x`.
fn fit_line(points: &[(f64, f64)]) -> (f64, f64) {
  let n = points.len() as f64;
  let mean_x = points.iter().map(|p| p.0).sum::<f64>() / n;
  let mean_y = points.iter().map(|p| p.1).sum::<f64>() / n;
  let var = points.iter().map(|p| (p.0 - mean_x).powi(2)).sum::<f64>();
  let cov = points.iter().map(|p| (p.0 - mean_x) * (p.1 - mean_y)).sum::<f64>();
  let f = if var > 0.0 { (cov / var).clamp(-MAX_FREQ, MAX_FREQ) } else { 0.0 };
  (mean_y - f * mean_x, f)
}

impl Servo {
  pub fn new() -> Self {
    Self::default()
  }

  /// Forgets everything (e.g. another leader is followed).
  pub fn reset(&mut self) {
    *self = Self::default();
  }

  pub fn fit(&self) -> Option<Fit> {
    self.fit
  }

  /// A Sync sent at leader time `leader` was received at local time `local`.
  pub fn add_sync(&mut self, local: i64, leader: i64) {
    if let Some(fit) = self.fit {
      let residual = leader.wrapping_sub(fit.leader_without_delay(local));
      if residual.abs() > STEP_THRESHOLD_NS {
        self.outliers_in_row += 1;
        if self.outliers_in_row < STEPS_TO_RESET {
          return;
        }
        log::warn!("PTP leader's clock stepped by {residual} ns, resynchronising");
        self.reset();
      } else {
        self.outliers_in_row = 0;
      }
    }
    self.samples.push_back((local, leader));
    if self.samples.len() > WINDOW {
      self.samples.pop_front();
    }
    self.refit();
  }

  fn refit(&mut self) {
    let Some(&(local_ref, leader_ref)) = self.samples.back() else {
      self.fit = None;
      return;
    };
    let offset_ref = leader_ref.wrapping_sub(local_ref);
    // relative to the newest measurement: small numbers, exact in f64
    let mut points: Vec<(f64, f64)> = self
      .samples
      .iter()
      .map(|&(l, m)| {
        (l.wrapping_sub(local_ref) as f64, m.wrapping_sub(l).wrapping_sub(offset_ref) as f64)
      })
      .collect();
    let (mut a, mut f) = fit_line(&points);
    if points.len() >= MIN_SAMPLES {
      let mut abs_residuals: Vec<f64> = points.iter().map(|&(x, y)| (y - a - f * x).abs()).collect();
      let limit = (OUTLIER_MADS * median(&mut abs_residuals)).max(OUTLIER_MIN_NS);
      let before = points.len();
      points.retain(|&(x, y)| (y - a - f * x).abs() <= limit);
      if points.len() != before && points.len() >= 2 {
        (a, f) = fit_line(&points);
      }
    }
    self.fit = Some(Fit { local_ref, offset_ref: offset_ref.wrapping_add(a.round() as i64), freq: f });
  }

  /// A Delay_Req sent at local time `local_sent` was received by the leader at leader time `leader_received`.
  pub fn add_delay(&mut self, local_sent: i64, leader_received: i64) {
    let Some(fit) = self.fit else {
      return;
    };
    // leader_received = (leader time at local_sent) + delay, and leader time = fit + delay
    let delay = leader_received.wrapping_sub(fit.leader_without_delay(local_sent)) / 2;
    if !(-STEP_THRESHOLD_NS..=100_000_000).contains(&delay) {
      return;
    }
    self.delays.push_back(delay.max(0));
    if self.delays.len() > DELAY_WINDOW {
      self.delays.pop_front();
    }
  }

  /// Path delay from the leader (median of recent measurements), 0 until measured.
  pub fn delay(&self) -> i64 {
    if self.delays.is_empty() {
      return 0;
    }
    let mut d: Vec<f64> = self.delays.iter().map(|&d| d as f64).collect();
    median(&mut d).round() as i64
  }

  /// The leader's time over the local monotonic clock, once there are enough measurements.
  pub fn overlay(&self) -> Option<ClockOverlay> {
    let fit = self.fit?;
    if self.samples.len() < MIN_SAMPLES {
      return None;
    }
    Some(ClockOverlay {
      clock_id: MONOTONIC_CLOCK_ID,
      last_sync: fit.local_ref,
      shift: fit.offset_ref.wrapping_add(self.delay()),
      freq_scale: fit.freq,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const OFFSET: i64 = 1_234_000_000_000_000_000;
  const PPM: f64 = 50.0;
  const DELAY: i64 = 30_000;

  fn leader_at(local: i64) -> i64 {
    local + OFFSET + (local as f64 * PPM * 1e-6) as i64
  }

  /// Deterministic jitter, ±`amplitude` ns.
  fn jitter(i: i64, amplitude: i64) -> i64 {
    ((i * 7919) % (2 * amplitude + 1)) - amplitude
  }

  fn feed(servo: &mut Servo, from: i64, count: i64, jitter_ns: i64) {
    for i in from..from + count {
      let local_received = 1_000_000_000 + i * 125_000_000;
      // sent at leader time t1, received `DELAY` later, timestamped with jitter
      let t1 = leader_at(local_received) - DELAY;
      servo.add_sync(local_received + jitter(i, jitter_ns), t1);
      let local_sent = local_received + 10_000_000;
      servo.add_delay(local_sent - jitter(i + 3, jitter_ns), leader_at(local_sent) + DELAY);
    }
  }

  #[test]
  fn not_published_before_enough_measurements() {
    let mut servo = Servo::new();
    feed(&mut servo, 0, (MIN_SAMPLES - 1) as i64, 0);
    assert!(servo.overlay().is_none());
    feed(&mut servo, MIN_SAMPLES as i64, 1, 0);
    assert!(servo.overlay().is_some());
  }

  #[test]
  fn follows_offset_frequency_and_delay() {
    let mut servo = Servo::new();
    feed(&mut servo, 0, 40, 0);
    let overlay = servo.overlay().unwrap();
    assert!((overlay.freq_scale - PPM * 1e-6).abs() < 0.1e-6, "{}", overlay.freq_scale);
    assert!((servo.delay() - DELAY).abs() < 100, "{}", servo.delay());
    let local = 1_000_000_000 + 40 * 125_000_000;
    let error = overlay.underlying_to_overlay_ns(local) - leader_at(local);
    assert!(error.abs() < 1_000, "{error} ns");
  }

  #[test]
  fn tolerates_jitter_and_outliers() {
    let mut servo = Servo::new();
    feed(&mut servo, 0, 20, 20_000);
    // one late timestamp (e.g. the thread was descheduled)
    let local = 1_000_000_000 + 20 * 125_000_000;
    servo.add_sync(local + 500_000, leader_at(local) - DELAY);
    feed(&mut servo, 21, 10, 20_000);
    let overlay = servo.overlay().unwrap();
    let local = 1_000_000_000 + 31 * 125_000_000;
    let error = overlay.underlying_to_overlay_ns(local) - leader_at(local);
    assert!(error.abs() < 30_000, "{error} ns");
    assert!((overlay.freq_scale - PPM * 1e-6).abs() < 10e-6, "{}", overlay.freq_scale);
  }

  #[test]
  fn resynchronises_when_the_leader_steps() {
    let mut servo = Servo::new();
    feed(&mut servo, 0, 20, 0);
    let step = 5_000_000_000;
    for i in 20..40 {
      let local = 1_000_000_000 + i * 125_000_000;
      servo.add_sync(local, leader_at(local) - DELAY + step);
    }
    let overlay = servo.overlay().unwrap();
    let local = 1_000_000_000 + 40 * 125_000_000;
    let error = overlay.underlying_to_overlay_ns(local) - (leader_at(local) + step);
    // the delay is measured again after the reset
    assert!(error.abs() < DELAY + 1_000, "{error} ns");
  }
}
