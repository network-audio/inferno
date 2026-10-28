//! A card's audio as Core Audio exchanges it: interleaved 32-bit float frames at positions of the rings.

use std::sync::atomic::Ordering;

use crate::{
  rings::{read_captured, write_playback},
  CardInfo, Region,
};

/// Full scale of the rings' 32-bit samples.
const SCALE: f32 = 2_147_483_648.0;

/// A ring sample as a float: exact for samples of up to 24 bits (what Dante carries).
#[inline]
pub fn sample_to_f32(sample: i32) -> f32 {
  sample as f32 / SCALE
}

/// A float as a ring sample, clipped at full scale: the inverse of [`sample_to_f32`].
#[inline]
pub fn f32_to_sample(value: f32) -> i32 {
  // `as` saturates (and makes NaN 0)
  (value * SCALE) as i32
}

/// The playback and recording of one card of a region.
pub struct CardIo<'a> {
  region: &'a Region,
  card: CardInfo,
}

impl<'a> CardIo<'a> {
  /// Card `index` of the region, if there is one.
  pub fn new(region: &'a Region, index: usize) -> Option<Self> {
    region.header().cards().get(index).map(|&card| Self { region, card })
  }

  pub fn info(&self) -> &CardInfo {
    &self.card
  }

  /// Plays `frames` interleaved frames of `mix` (the card's playback channels), the frames of position
  /// `position`, at `gain` (the volume: 1 leaves them exactly as they are): they're transmitted the playback
  /// lead later. `now`: the position now, to tell whether they came too late to be transmitted.
  pub fn write_mix(&self, position: usize, frames: usize, mix: &[f32], now: Option<usize>, gain: f32) {
    let channels = self.card.playback_channels as usize;
    if channels == 0 {
      return;
    }
    let frames = frames.min(mix.len() / channels);
    let header = self.region.header();
    let start = position + header.playback_lead as usize;
    let written = self.region.playback_written();
    for i in 0..channels {
      let ch = self.card.first_playback as usize + i;
      let ring = self.region.playback_ring(ch);
      write_playback(ring, &written[ch], start, frames, |f| f32_to_sample(mix[f * channels + i] * gain));
    }
    // the transmitter reads a position as soon as it's due: audio written after that goes out as silence
    if now.is_some_and(|now| now as isize - start as isize > 0) {
      header.late_playback.fetch_add(1, Ordering::Relaxed);
    }
  }

  /// Records `frames` interleaved frames (the card's recording channels) from position `position` into
  /// `input`, at `gain` (1: exactly as received): what the receiver has received, silence where it hasn't.
  pub fn read_input(&self, position: usize, frames: usize, input: &mut [f32], gain: f32) {
    let channels = self.card.capture_channels as usize;
    if channels == 0 {
      return;
    }
    let frames = frames.min(input.len() / channels);
    let readable = self.region.capture_readable();
    let mut unreceived = false;
    for i in 0..channels {
      let ch = self.card.first_capture as usize + i;
      let ring = self.region.capture_ring(ch);
      let silenced =
        read_captured(ring, readable[ch].load(Ordering::Acquire), position, frames, |f, s| {
          input[f * channels + i] = sample_to_f32(s) * gain
        });
      unreceived |= silenced > 0 && silenced < frames;
    }
    if unreceived {
      self.region.header().unreceived_capture.fetch_add(1, Ordering::Relaxed);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{layout::RegionParams, RegionCard};

  #[test]
  fn samples_of_24_bits_pass_exactly() {
    for s in [0, 1 << 8, -(1 << 8), 0x7fff_ff00, i32::MIN, 0x1234_5600, -0x1234_5600] {
      assert_eq!(f32_to_sample(sample_to_f32(s)), s, "{s:#x}");
    }
    assert_eq!(f32_to_sample(1.0), i32::MAX, "clipped");
    assert_eq!(f32_to_sample(-2.0), i32::MIN);
  }

  fn region() -> Region {
    let cards = [
      RegionCard { name: "A".into(), uid: "a".into(), playback_channels: 1, capture_channels: 1 },
      RegionCard { name: "B".into(), uid: "b".into(), playback_channels: 2, capture_channels: 2 },
    ];
    let params = RegionParams {
      sample_rate: 48000,
      ring_frames: 8192,
      generation: 1,
      playback_lead: 100,
      input_latency: 0,
      output_latency: 0,
    };
    Region::heap(&cards, params).unwrap()
  }

  #[test]
  fn a_cards_channels_are_its_range_of_the_rings() {
    let region = region();
    let b = CardIo::new(&region, 1).unwrap();
    let mix =
      [sample_to_f32(1 << 8), sample_to_f32(2 << 8), sample_to_f32(3 << 8), sample_to_f32(4 << 8)];
    b.write_mix(1000, 2, &mix, Some(1000), 1.0);
    let at = |ch: usize, p: usize| region.playback_ring(ch)[p].load(Ordering::Relaxed);
    assert_eq!([at(1, 1100), at(2, 1100), at(1, 1101), at(2, 1101)], [1 << 8, 2 << 8, 3 << 8, 4 << 8]);
    assert_eq!(at(0, 1100), 0, "not card A's");
    assert_eq!(region.header().late_playback.load(Ordering::Relaxed), 0);
    b.write_mix(1002, 2, &mix, Some(1200), 1.0);
    assert_eq!(region.header().late_playback.load(Ordering::Relaxed), 1, "too late");

    for ch in 0..3 {
      region.capture_ring(ch)[500].store((ch as i32 + 1) << 8, Ordering::Relaxed);
      region.capture_readable()[ch].store(501, Ordering::Relaxed);
    }
    let mut input = [9.0; 4];
    b.read_input(500, 2, &mut input, 1.0);
    assert_eq!(input.map(f32_to_sample), [2 << 8, 3 << 8, 0, 0], "the second frame isn't received yet");
    assert_eq!(region.header().unreceived_capture.load(Ordering::Relaxed), 1);

    // the volume scales both ways
    b.read_input(500, 1, &mut input, 0.5);
    assert_eq!(f32_to_sample(input[0]), 1 << 8);
    b.write_mix(2000, 1, &[sample_to_f32(0x40_0000 << 8), 0.0], None, 0.25);
    assert_eq!(region.playback_ring(1)[2100].load(Ordering::Relaxed), 0x10_0000 << 8);
    b.write_mix(2001, 1, &[sample_to_f32(0x12_3456 << 8), 0.0], None, 0.0);
    assert_eq!(region.playback_ring(1)[2101].load(Ordering::Relaxed), 0, "muted");
  }
}
