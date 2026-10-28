//! Reading and writing the device's rings (power of 2 long, addressed by position since the start), as a sound
//! card plays and records.

use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

/// Writes `frames` frames of playback at position `start` of `ring` (`sample(i)`: the `i`th), where the
/// transmitter will read them, and that `written` is past them. Where the card hasn't played since the end of
/// the last write (after a pause or a stall), the positions in between count as written: they're silence,
/// not what the ring held a cycle ago.
#[inline]
pub fn write_playback(
  ring: &[AtomicI32],
  written: &AtomicUsize,
  start: usize,
  frames: usize,
  mut sample: impl FnMut(usize) -> i32,
) {
  let mask = ring.len() - 1;
  let gap = start.saturating_sub(written.load(Ordering::Acquire)).min(ring.len());
  for p in start - gap..start {
    ring[p & mask].store(0, Ordering::Relaxed);
  }
  for f in 0..frames {
    ring[(start + f) & mask].store(sample(f), Ordering::Relaxed);
  }
  written.store(start + frames, Ordering::Release);
}

/// Reads `frames` frames from position `start` of `ring` (`put(i, sample)`: the `i`th), as far as the receiver
/// has made them readable (`readable`): what's beyond is the audio of a ring cycle ago, silence instead. A
/// `readable` not within a ring of `start` (before a restart, or the channel isn't received) makes it all
/// silence. Returns the frames of silence (at the end).
#[inline]
pub fn read_captured(
  ring: &[AtomicI32],
  readable: usize,
  start: usize,
  frames: usize,
  mut put: impl FnMut(usize, i32),
) -> usize {
  let ahead = readable.wrapping_sub(start) as isize;
  let valid = if ahead <= 0 || ahead as usize > ring.len() { 0 } else { (ahead as usize).min(frames) };
  let mask = ring.len() - 1;
  for i in 0..valid {
    put(i, ring[(start + i) & mask].load(Ordering::Relaxed));
  }
  for i in valid..frames {
    put(i, 0);
  }
  frames - valid
}

/// [`read_captured`] into `dst`.
pub fn copy_captured(ring: &[AtomicI32], readable: usize, start: usize, dst: &mut [i32]) -> usize {
  read_captured(ring, readable, start, dst.len(), |i, s| dst[i] = s)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ring(values: &[i32]) -> Vec<AtomicI32> {
    values.iter().map(|&v| AtomicI32::new(v)).collect()
  }

  #[test]
  fn captured_frames_up_to_the_readable_position() {
    let r = ring(&[1, 2, 3, 4, 5, 6, 7, 8]);
    let mut dst = [9; 4];
    assert_eq!(copy_captured(&r, 6, 4, &mut dst), 2);
    assert_eq!(dst, [5, 6, 0, 0]);
    assert_eq!(copy_captured(&r, 12, 6, &mut dst), 0);
    assert_eq!(dst, [7, 8, 1, 2], "wrapping around the ring");
  }

  #[test]
  fn positions_of_another_run_are_silence() {
    let r = ring(&[1; 8]);
    let mut dst = [9; 4];
    assert_eq!(copy_captured(&r, 0, 100, &mut dst), 4);
    assert_eq!(copy_captured(&r, 1000, 100, &mut dst), 4, "readable more than a ring ahead");
  }

  #[test]
  fn playback_after_a_gap_leaves_silence_in_between() {
    let r = ring(&[7; 64]); // a cycle ago
    let written = AtomicUsize::new(0);
    write_playback(&r, &written, 100, 8, |_| 1);
    assert_eq!(written.load(Ordering::Relaxed), 108);
    write_playback(&r, &written, 120, 8, |_| 2);
    let at = |p: usize| r[p & 63].load(Ordering::Relaxed);
    assert_eq!(at(107), 1);
    assert!((108..120).all(|p| at(p) == 0), "the gap is silence");
    assert_eq!(at(120), 2);
    assert_eq!(written.load(Ordering::Relaxed), 128);
  }
}
