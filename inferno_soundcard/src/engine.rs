//! The audio engine behind the virtual sound cards, independent of Windows (and so testable anywhere).
//!
//! One Inferno device carries all cards: its RX channels are the cards' capture (recording) channels, its TX
//! channels their playback channels, card after card. Inferno's receiver and transmitter work with ring
//! buffers addressed by media clock time (frames since the start); so do the USB audio streams, because the
//! USB/IP server completes their transfers by the media clock (`EngineClock`):
//!
//! * a capture transfer completes once the media clock has passed the frames it covers: they're copied from
//!   the capture rings, as far as the receiver made them readable (silence where it didn't, rather than the
//!   audio of a ring cycle ago)
//! * a playback transfer arrives ahead of the frames it covers (by the transfers Windows queues): it's written
//!   to the playback rings `PLAYBACK_LEAD` frames later than its frames, so that it's there when the
//!   transmitter reads them (as they're due)

use std::{
  ops::Range,
  sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc, Mutex,
  },
  time::Instant,
};

use inferno_aoip::{
  clock::ClockSource,
  device_server::{
    AtomicSample, Clock, ExternalBufferParameters, MediaClock, PositionReportDestination, Sample, Settings,
  },
};
use inferno_driver_core::{BuffersSignature, DeviceError, Direction, Stream, StreamOptions};
use log::{info, warn};
use usbip_audio::uac2::Uac2Streams;

/// How much later than its frames playback audio is transmitted: margin for the transfer that's being
/// completed when Windows submits the next one.
pub const PLAYBACK_LEAD: usize = 96;

/// The channels of one virtual sound card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CardChannels {
  /// played by Windows, transmitted
  pub playback: usize,
  /// received, recorded by Windows
  pub capture: usize,
}

type Rings = Arc<Vec<Box<[AtomicSample]>>>;

fn rings(channels: usize, frames: usize) -> Rings {
  Arc::new((0..channels).map(|_| (0..frames).map(|_| AtomicSample::new(0)).collect()).collect())
}

/// Ring length (power of 2): the network latency plus margin.
fn ring_frames(latency: usize) -> usize {
  (latency + 4096).next_power_of_two().max(8192)
}

/// What the streams of all cards share.
struct Shared {
  capture_rings: Rings,
  /// per capture ring: up to where (position since the start) the receiver has made it readable
  capture_readable: Arc<Vec<AtomicUsize>>,
  playback_rings: Rings,
  /// per playback ring: frames since the start written (silence beyond)
  playback_written: Arc<Vec<AtomicUsize>>,
  late_playback: AtomicU64,
  unreceived_capture: AtomicU64,
  clock: Arc<EngineClock>,
}

/// The media clock, in nanoseconds since the start: the clock of the USB/IP server, and so of the cards.
pub struct EngineClock {
  sample_rate: u64,
  start_time: Clock,
  state: Mutex<ClockState>,
}

struct ClockState {
  receiver: tokio::sync::watch::Receiver<Option<inferno_aoip::device_server::ClockOverlay>>,
  clock: MediaClock,
  /// the last time read, to go on from while the clock is unavailable
  last: Option<(u64, Instant)>,
}

impl EngineClock {
  fn new(source: &Arc<dyn ClockSource>, sample_rate: u32, start_time: Clock) -> Self {
    let state =
      ClockState { receiver: source.subscribe(), clock: MediaClock::new(false), last: None };
    Self { sample_rate: sample_rate as u64, start_time, state: Mutex::new(state) }
  }

  /// Frames since the start, if the clock is available.
  pub fn position(&self) -> Option<usize> {
    let mut state = self.state.lock().unwrap();
    Self::update(&mut state);
    state.clock.wrapping_now_in_timebase(self.sample_rate).map(|now| now.wrapping_sub(self.start_time))
  }

  fn update(state: &mut ClockState) {
    // borrowing takes the channel's lock: only when there's a new overlay (checking is lock-free)
    if state.receiver.has_changed().unwrap_or(false) || !state.clock.is_ready() {
      if let Some(overlay) = *state.receiver.borrow_and_update() {
        state.clock.update_overlay(overlay);
      }
    }
  }
}

impl usbip_audio::Clock for EngineClock {
  fn now_ns(&self) -> u64 {
    let mut state = self.state.lock().unwrap();
    Self::update(&mut state);
    let position = state.clock.wrapping_now_in_timebase(self.sample_rate).map(|now| now.wrapping_sub(self.start_time));
    let ns = match (position, state.last) {
      // positions before the start (the clock stepped back) count as the start
      (Some(position), _) if (position as isize) >= 0 => {
        (position as u128 * 1_000_000_000 / self.sample_rate as u128) as u64
      }
      (Some(_), _) => 0,
      // the clock is unavailable: carry on at the system clock's rate
      (None, Some((ns, at))) => ns + at.elapsed().as_nanos() as u64,
      (None, None) => 0,
    };
    state.last = Some((ns, Instant::now()));
    ns
  }
}

/// The USB audio streams of one card: its range of the device's RX and TX channels.
pub struct CardStreams {
  shared: Arc<Shared>,
  playback: Range<usize>,
  capture: Range<usize>,
}

impl Uac2Streams for CardStreams {
  fn playback(&self, frame: u64, samples: &[i32]) {
    let channels = self.playback.len();
    if channels == 0 {
      return;
    }
    let start = frame as usize + PLAYBACK_LEAD;
    let frames = samples.len() / channels;
    for (i, ch) in self.playback.clone().enumerate() {
      let ring = &self.shared.playback_rings[ch];
      let mask = ring.len() - 1;
      // after a pause or a stall the transfers continue later than the last one ended: the positions in
      // between count as written, they must be silence, not what the ring held a cycle ago
      let written = self.shared.playback_written[ch].load(Ordering::Acquire);
      let gap = start.saturating_sub(written).min(ring.len());
      for p in start - gap..start {
        ring[p & mask].store(0, Ordering::Relaxed);
      }
      for f in 0..frames {
        ring[(start + f) & mask].store(samples[f * channels + i], Ordering::Relaxed);
      }
      self.shared.playback_written[ch].store(start + frames, Ordering::Release);
    }
    // the transmitter reads a position as soon as it's due: audio written after that goes out as silence
    if let Some(now) = self.shared.clock.position() {
      let overdue = now as isize - start as isize;
      if overdue > 0 {
        let count = self.shared.late_playback.fetch_add(1, Ordering::Relaxed) + 1;
        if count.is_power_of_two() {
          warn!("playback arrived {overdue} frames after it was due: transmitted as silence ({count} times so far)");
        }
      }
    }
  }

  fn capture(&self, frame: u64, samples: &mut [i32]) {
    let channels = self.capture.len();
    if channels == 0 {
      return;
    }
    let frames = samples.len() / channels;
    let mut column = vec![0; frames];
    for (i, ch) in self.capture.clone().enumerate() {
      let readable = self.shared.capture_readable[ch].load(Ordering::Acquire);
      let silenced = copy_captured(&self.shared.capture_rings[ch], readable, frame as usize, &mut column);
      if silenced > 0 && silenced < frames {
        let count = self.shared.unreceived_capture.fetch_add(1, Ordering::Relaxed) + 1;
        if count.is_power_of_two() {
          warn!("capture channel not received in time ({silenced} frames), silence instead ({count} times so far)");
        }
      }
      for (f, s) in column.iter().enumerate() {
        samples[f * channels + i] = *s;
      }
    }
  }
}

/// Copies `dst.len()` frames from position `start` of `ring` (power of 2 long), as far as the receiver has made
/// them readable (`readable`): what's beyond is the audio of a ring cycle ago, silence instead. A `readable`
/// not within a ring of `start` (before a restart, or the channel isn't received) makes it all silence.
/// Returns the frames of silence (at the end of `dst`).
pub fn copy_captured(ring: &[AtomicSample], readable: usize, start: usize, dst: &mut [Sample]) -> usize {
  let ahead = readable.wrapping_sub(start) as isize;
  let valid = if ahead <= 0 || ahead as usize > ring.len() { 0 } else { (ahead as usize).min(dst.len()) };
  let mask = ring.len() - 1;
  for (i, d) in dst[..valid].iter_mut().enumerate() {
    *d = ring[(start + i) & mask].load(Ordering::Relaxed);
  }
  dst[valid..].fill(0);
  dst.len() - valid
}

fn signature(rings: &Rings) -> BuffersSignature {
  BuffersSignature { areas: rings.iter().map(|r| (r.as_ptr() as usize, r.len(), 1)).collect(), period_size: 48 }
}

/// The device's receiver and transmitter, and the rings the cards' streams use.
pub struct Engine {
  sample_rate: u32,
  cards: Vec<CardChannels>,
  capture: Option<Stream>,
  playback: Option<Stream>,
  capture_rings: Rings,
  capture_readable: Arc<Vec<AtomicUsize>>,
  playback_rings: Rings,
  playback_written: Arc<Vec<AtomicUsize>>,
  shared: Option<Arc<Shared>>,
}

impl Engine {
  /// Prepares the device for `cards`: `settings`' RX and TX channels are replaced by theirs.
  pub fn new(
    mut settings: Settings,
    cards: &[CardChannels],
    clock_source: Option<Arc<dyn ClockSource>>,
  ) -> Result<Self, DeviceError> {
    let (rx, tx) = (cards.iter().map(|c| c.capture).sum(), cards.iter().map(|c| c.playback).sum());
    settings.make_rx_channels(rx);
    settings.make_tx_channels(tx);
    let info = &settings.self_info;
    let latency = info
      .latency_samples()
      .max(settings.tx_latency_ns as usize * info.sample_rate as usize / 1_000_000_000);
    let frames = ring_frames(latency);
    let mut engine = Self {
      sample_rate: info.sample_rate,
      cards: cards.to_vec(),
      capture: None,
      playback: None,
      capture_rings: rings(rx, frames),
      capture_readable: Arc::new((0..rx).map(|_| AtomicUsize::new(0)).collect()),
      playback_rings: rings(tx, frames),
      playback_written: Arc::new((0..tx).map(|_| AtomicUsize::new(0)).collect()),
      shared: None,
    };
    let with_clock = |stream: Stream| match &clock_source {
      Some(source) => stream.with_clock_source(source.clone()),
      None => stream,
    };
    let options = || StreamOptions { track_position: false, on_transfer: None };
    if rx > 0 {
      let mut stream = with_clock(Stream::new(Direction::Capture, settings.clone()));
      let channels = engine
        .capture_rings
        .iter()
        .enumerate()
        .map(|(ch, ring)| unsafe {
          ExternalBufferParameters::new(
            ring.as_ptr(),
            frames,
            1,
            stream.buffers_valid(),
            Some(PositionReportDestination::new(engine.capture_readable.clone(), ch)),
          )
        })
        .collect();
      stream.prepare(signature(&engine.capture_rings), channels, options())?;
      engine.capture = Some(stream);
    }
    if tx > 0 {
      let mut stream = with_clock(Stream::new(Direction::Playback, settings));
      let channels = engine
        .playback_rings
        .iter()
        .enumerate()
        .map(|(ch, ring)| {
          let written = engine.playback_written.clone();
          unsafe { ExternalBufferParameters::new(ring.as_ptr(), frames, 1, stream.buffers_valid(), None) }
            // silence where Windows hasn't played
            .with_written_from(Arc::new(move |position| written[ch].load(Ordering::Acquire).saturating_sub(position)))
        })
        .collect();
      if let Err(e) = stream.prepare(signature(&engine.playback_rings), channels, options()) {
        engine.close();
        return Err(e);
      }
      engine.playback = Some(stream);
    }
    Ok(engine)
  }

  fn streams(&mut self) -> impl Iterator<Item = &mut Stream> {
    self.capture.iter_mut().chain(self.playback.iter_mut())
  }

  /// Starts the receiver and transmitter now, if the device has a clock. Returns the clock the cards' USB/IP
  /// server must complete transfers by.
  pub fn start(&mut self) -> Result<Arc<EngineClock>, DeviceError> {
    if let Some(shared) = &self.shared {
      return Ok(shared.clock.clone());
    }
    let rate = self.sample_rate as u64;
    let first = self.streams().next().ok_or(DeviceError::ThreadEnded)?;
    let clock_source = first.clock_source().ok_or(DeviceError::ThreadEnded)?;
    let start_time = first.media_now(rate).ok_or(DeviceError::NoClock(clock_source.diagnose_missing()))?;
    for ring in self.capture_rings.iter().chain(self.playback_rings.iter()) {
      ring.iter().for_each(|s| s.store(0, Ordering::Relaxed));
    }
    for position in self.capture_readable.iter().chain(self.playback_written.iter()) {
      position.store(0, Ordering::Release);
    }
    for stream in self.streams() {
      stream.start_at(start_time);
    }
    let clock = Arc::new(EngineClock::new(&clock_source, self.sample_rate, start_time));
    self.shared = Some(Arc::new(Shared {
      capture_rings: self.capture_rings.clone(),
      capture_readable: self.capture_readable.clone(),
      playback_rings: self.playback_rings.clone(),
      playback_written: self.playback_written.clone(),
      late_playback: AtomicU64::new(0),
      unreceived_capture: AtomicU64::new(0),
      clock: clock.clone(),
    }));
    info!("started {} cards", self.cards.len());
    Ok(clock)
  }

  /// The streams of each card (once started).
  pub fn card_streams(&self) -> Vec<CardStreams> {
    let Some(shared) = &self.shared else { return vec![] };
    let (mut rx, mut tx) = (0, 0);
    self
      .cards
      .iter()
      .map(|card| {
        let streams =
          CardStreams { shared: shared.clone(), playback: tx..tx + card.playback, capture: rx..rx + card.capture };
        (rx, tx) = (rx + card.capture, tx + card.playback);
        streams
      })
      .collect()
  }

  /// The device's settings (with the cards' channels).
  pub fn settings(&self) -> Option<&Settings> {
    self.capture.iter().chain(self.playback.iter()).next().map(|s| s.settings())
  }

  fn close(&mut self) {
    self.shared = None;
    for stream in self.streams() {
      stream.stop();
      stream.close();
    }
  }
}

impl Drop for Engine {
  fn drop(&mut self) {
    self.close();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ring(values: &[Sample]) -> Vec<AtomicSample> {
    values.iter().map(|&v| AtomicSample::new(v)).collect()
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

  fn card(channels: usize) -> CardStreams {
    let source: Arc<dyn ClockSource> = Arc::new(inferno_aoip::clock::FreeRunningClockSource::new());
    let shared = Arc::new(Shared {
      capture_rings: rings(0, 8192),
      capture_readable: Arc::new(vec![]),
      playback_rings: rings(channels, 8192),
      playback_written: Arc::new((0..channels).map(|_| AtomicUsize::new(0)).collect()),
      late_playback: AtomicU64::new(0),
      unreceived_capture: AtomicU64::new(0),
      clock: Arc::new(EngineClock::new(&source, 48000, 0)),
    });
    CardStreams { shared, playback: 0..channels, capture: 0..0 }
  }

  #[test]
  fn playback_after_a_gap_leaves_silence_in_between() {
    let card = card(1);
    let ring = &card.shared.playback_rings[0];
    ring.iter().for_each(|s| s.store(7, Ordering::Relaxed)); // a cycle ago
    card.playback(1000, &[1; 48]);
    let written = card.shared.playback_written[0].load(Ordering::Relaxed);
    assert_eq!(written, 1000 + PLAYBACK_LEAD + 48);
    // the next transfer 100 frames later than the last ended
    card.playback(1148, &[2; 48]);
    let at = |p: usize| ring[p & (ring.len() - 1)].load(Ordering::Relaxed);
    assert_eq!(at(written - 1), 1);
    assert!((written..1148 + PLAYBACK_LEAD).all(|p| at(p) == 0), "the gap is silence");
    assert_eq!(at(1148 + PLAYBACK_LEAD), 2);
    assert_eq!(card.shared.playback_written[0].load(Ordering::Relaxed), 1148 + PLAYBACK_LEAD + 48);
  }

  #[test]
  fn positions_of_another_run_are_silence() {
    let r = ring(&[1; 8]);
    let mut dst = [9; 4];
    assert_eq!(copy_captured(&r, 0, 100, &mut dst), 4);
    assert_eq!(copy_captured(&r, 1000, 100, &mut dst), 4, "readable more than a ring ahead");
  }
}
