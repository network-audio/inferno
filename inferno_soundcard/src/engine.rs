//! The audio engine behind the virtual sound cards, independent of the operating system (and so testable
//! anywhere).
//!
//! One Inferno device carries all cards: its RX channels are the cards' capture (recording) channels, its TX
//! channels their playback channels, card after card. Inferno's receiver and transmitter work with ring
//! buffers addressed by media clock time (frames since the start). The rings are in a [`Region`]: this
//! process's memory on Windows, memory shared with the Core Audio plug-in on macOS. The cards' streams use
//! them by the media clock:
//!
//! * a capture transfer (Windows) or input cycle (Core Audio) is due once the media clock has passed the frames
//!   it covers: they're copied from the capture rings, as far as the receiver made them readable (silence where
//!   it didn't, rather than the audio of a ring cycle ago)
//! * playback arrives ahead of the frames it covers (by the transfers Windows queues, or Core Audio's I/O
//!   cycle): it's written to the playback rings a margin later than its frames
//!   (`Config::playback_margin_ms`), so that it's there when the transmitter reads them (as they're due)
//!
//! On Windows, the USB/IP server completes the cards' transfers by the media clock (`EngineClock`); on macOS
//! the plug-in tells Core Audio when the frames are due, from the clock the engine publishes in the region.

use std::{
  ops::Range,
  sync::{atomic::Ordering, Arc, Mutex},
  time::Instant,
};

use inferno_aoip::{
  clock::ClockSource,
  device_server::{
    AtomicSample, Clock, ClockOverlay, ExternalBufferParameters, MediaClock, PositionReportDestination,
    PositionTable, Settings,
  },
};
use inferno_driver_core::{BuffersSignature, DeviceError, Direction, Stream, StreamOptions};
use inferno_soundcard_shm::{
  rings::{copy_captured, write_playback},
  Overlay, Region, RegionCard, RegionParams, State,
};
use log::{info, warn};
use usbip_audio::uac2::Uac2Streams;

// the rings are 32-bit atomics, whichever type each side calls them
const _: () = assert!(size_of::<AtomicSample>() == size_of::<std::sync::atomic::AtomicI32>());

/// Ring length (power of 2): the network latency plus margin.
fn ring_frames(latency: usize) -> usize {
  (latency + 4096).next_power_of_two().max(8192)
}

/// Where the receiver reports how far it has made each capture ring readable: the region's table.
struct CaptureReadable(Arc<Region>);

impl PositionTable for CaptureReadable {
  fn position(&self, index: usize) -> &std::sync::atomic::AtomicUsize {
    &self.0.capture_readable()[index]
  }
}

/// What the streams of all cards share.
struct Shared {
  region: Arc<Region>,
  clock: Arc<EngineClock>,
}

/// The media clock, in nanoseconds since the start: the clock of the USB/IP server, and so of the cards.
pub struct EngineClock {
  sample_rate: u64,
  start_time: Clock,
  state: Mutex<ClockState>,
}

struct ClockState {
  receiver: tokio::sync::watch::Receiver<Option<ClockOverlay>>,
  clock: MediaClock,
  /// the last time read, to go on from while the clock is unavailable
  last: Option<(u64, Instant)>,
}

impl EngineClock {
  fn new(source: &Arc<dyn ClockSource>, sample_rate: u32, start_time: Clock) -> Self {
    let state = ClockState { receiver: source.subscribe(), clock: MediaClock::new(false), last: None };
    Self { sample_rate: sample_rate as u64, start_time, state: Mutex::new(state) }
  }

  /// Frames since the start, if the clock is available.
  pub fn position(&self) -> Option<usize> {
    let mut state = self.state.lock().unwrap();
    Self::update(&mut state);
    state.clock.wrapping_now_in_timebase(self.sample_rate).map(|now| now.wrapping_sub(self.start_time))
  }

  /// The clock's current overlay, if it's available.
  pub fn overlay(&self) -> Option<ClockOverlay> {
    let mut state = self.state.lock().unwrap();
    Self::update(&mut state);
    *state.clock.get_overlay()
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
    let position =
      state.clock.wrapping_now_in_timebase(self.sample_rate).map(|now| now.wrapping_sub(self.start_time));
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
    let region = &self.shared.region;
    let header = region.header();
    let start = frame as usize + header.playback_lead as usize;
    let frames = samples.len() / channels;
    let mut dropped = false;
    for (i, ch) in self.playback.clone().enumerate() {
      let written = &region.playback_written()[ch];
      dropped |=
        !write_playback(region.playback_ring(ch), written, start, frames, |f| samples[f * channels + i]);
    }
    if dropped {
      warn!("playback {frames} frames from position {start} far behind what's been played: dropped");
    }
    // the transmitter reads a position as soon as it's due: audio written after that goes out as silence
    if let Some(now) = self.shared.clock.position() {
      let overdue = now as isize - start as isize;
      if overdue > 0 {
        let count = header.late_playback.fetch_add(1, Ordering::Relaxed) + 1;
        if count.is_power_of_two() {
          warn!(
            "playback arrived {overdue} frames after it was due: transmitted as silence ({count} times \
             so far)"
          );
        }
      }
    }
  }

  fn capture(&self, frame: u64, samples: &mut [i32]) {
    let channels = self.capture.len();
    if channels == 0 {
      return;
    }
    let region = &self.shared.region;
    let frames = samples.len() / channels;
    let mut column = vec![0; frames];
    for (i, ch) in self.capture.clone().enumerate() {
      let readable = region.capture_readable()[ch].load(Ordering::Acquire);
      let silenced = copy_captured(region.capture_ring(ch), readable, frame as usize, &mut column);
      if silenced > 0 && silenced < frames {
        let count = region.header().unreceived_capture.fetch_add(1, Ordering::Relaxed) + 1;
        if count.is_power_of_two() {
          warn!(
            "capture channel not received in time ({silenced} frames), silence instead ({count} times \
             so far)"
          );
        }
      }
      for (f, s) in column.iter().enumerate() {
        samples[f * channels + i] = *s;
      }
    }
  }
}

fn signature<'a>(rings: impl Iterator<Item = &'a [std::sync::atomic::AtomicI32]>) -> BuffersSignature {
  BuffersSignature { areas: rings.map(|r| (r.as_ptr() as usize, r.len(), 1)).collect(), period_size: 48 }
}

/// Publishes `clock` in `region`, if it changed.
pub fn publish_clock(clock: &EngineClock, region: &Region) {
  let overlay = clock.overlay().map(|o| Overlay {
    clock_id: o.clock_id,
    last_sync: o.last_sync,
    shift: o.shift,
    freq_scale: o.freq_scale,
  });
  let shared = &region.header().clock;
  if shared.read() != overlay {
    shared.publish(overlay);
  }
}

/// Creates the region of the engine's rings (`Region::heap`, or shared memory).
pub type Allocate = dyn FnOnce(&[RegionCard], RegionParams) -> Result<Region, String>;

/// A value different for every region this computer creates.
fn generation() -> u64 {
  let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
  (now.as_nanos() as u64) ^ ((std::process::id() as u64) << 48)
}

/// The device's receiver and transmitter, and the rings the cards' streams use.
pub struct Engine {
  sample_rate: u32,
  cards: Vec<RegionCard>,
  capture: Option<Stream>,
  playback: Option<Stream>,
  region: Arc<Region>,
  shared: Option<Arc<Shared>>,
}

impl Engine {
  /// Prepares the device for `cards`, with its rings in this process's memory: `settings`' RX and TX channels
  /// are replaced by theirs. Playback audio is transmitted `playback_lead` frames later than its frames.
  pub fn new(
    settings: Settings,
    cards: &[RegionCard],
    playback_lead: usize,
    clock_source: Option<Arc<dyn ClockSource>>,
  ) -> Result<Self, DeviceError> {
    Self::with_memory(settings, cards, playback_lead, clock_source, Box::new(Region::heap))
  }

  /// As `new`, with the rings in the region `allocate` creates.
  pub fn with_memory(
    mut settings: Settings,
    cards: &[RegionCard],
    playback_lead: usize,
    clock_source: Option<Arc<dyn ClockSource>>,
    allocate: Box<Allocate>,
  ) -> Result<Self, DeviceError> {
    let (rx, tx) =
      (cards.iter().map(|c| c.capture_channels).sum(), cards.iter().map(|c| c.playback_channels).sum());
    settings.make_rx_channels(rx);
    settings.make_tx_channels(tx);
    let info = &settings.self_info;
    let ns_to_frames = |ns: u64| (ns as u128 * info.sample_rate as u128 / 1_000_000_000) as usize;
    let output_latency = ns_to_frames(settings.tx_latency_ns as u64);
    let latency = info.latency_samples().max(output_latency);
    let params = RegionParams {
      sample_rate: info.sample_rate,
      ring_frames: ring_frames(latency + playback_lead),
      generation: generation(),
      playback_lead,
      input_latency: info.latency_samples(),
      output_latency,
    };
    let region = Arc::new(allocate(cards, params).map_err(DeviceError::StartFailed)?);
    let frames = region.ring_frames();
    let mut engine = Self {
      sample_rate: info.sample_rate,
      cards: cards.to_vec(),
      capture: None,
      playback: None,
      region: region.clone(),
      shared: None,
    };
    let with_clock = |stream: Stream| match &clock_source {
      Some(source) => stream.with_clock_source(source.clone()),
      None => stream,
    };
    let options = || StreamOptions { track_position: false, on_transfer: None };
    if rx > 0 {
      let mut stream = with_clock(Stream::new(Direction::Capture, settings.clone()));
      let readable = Arc::new(CaptureReadable(region.clone()));
      let channels = (0..rx)
        .map(|ch| unsafe {
          ExternalBufferParameters::new(
            region.capture_ring(ch).as_ptr() as *const AtomicSample,
            frames,
            1,
            stream.buffers_valid(),
            Some(PositionReportDestination::new(readable.clone(), ch)),
          )
        })
        .collect();
      stream.prepare(signature((0..rx).map(|ch| region.capture_ring(ch))), channels, options())?;
      engine.capture = Some(stream);
    }
    if tx > 0 {
      let mut stream = with_clock(Stream::new(Direction::Playback, settings));
      let channels = (0..tx)
        .map(|ch| {
          let region = region.clone();
          let ring = region.playback_ring(ch).as_ptr() as *const AtomicSample;
          unsafe { ExternalBufferParameters::new(ring, frames, 1, stream.buffers_valid(), None) }
            // silence where the card hasn't played
            .with_written_from(Arc::new(move |position| {
              region.playback_written()[ch].load(Ordering::Acquire).saturating_sub(position)
            }))
        })
        .collect();
      let rings = (0..tx).map(|ch| region.playback_ring(ch));
      if let Err(e) = stream.prepare(signature(rings), channels, options()) {
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
  /// server must complete transfers by. The region is then `Running`, with the clock published.
  pub fn start(&mut self) -> Result<Arc<EngineClock>, DeviceError> {
    if let Some(shared) = &self.shared {
      return Ok(shared.clock.clone());
    }
    let rate = self.sample_rate as u64;
    let first = self.streams().next().ok_or(DeviceError::ThreadEnded)?;
    let clock_source = first.clock_source().ok_or(DeviceError::ThreadEnded)?;
    let start_time =
      first.media_now(rate).ok_or(DeviceError::NoClock(clock_source.diagnose_missing()))?;
    let region = self.region.clone();
    let rings = (0..region.header().capture_channels as usize).map(|ch| region.capture_ring(ch));
    for ring in
      rings.chain((0..region.header().playback_channels as usize).map(|ch| region.playback_ring(ch)))
    {
      ring.iter().for_each(|s| s.store(0, Ordering::Relaxed));
    }
    for position in region.capture_readable().iter().chain(region.playback_written()) {
      position.store(0, Ordering::Release);
    }
    region.header().start_time.store(start_time as u64, Ordering::Release);
    for stream in self.streams() {
      stream.start_at(start_time);
    }
    let clock = Arc::new(EngineClock::new(&clock_source, self.sample_rate, start_time));
    self.shared = Some(Arc::new(Shared { region: region.clone(), clock: clock.clone() }));
    self.publish_clock();
    region.header().set_state(State::Running);
    info!("started {} cards", self.cards.len());
    Ok(clock)
  }

  /// Publishes the media clock in the region (once started), for another process using it.
  pub fn publish_clock(&self) {
    if let Some(shared) = &self.shared {
      publish_clock(&shared.clock, &self.region);
    }
  }

  /// The region of the rings.
  pub fn region(&self) -> &Arc<Region> {
    &self.region
  }

  /// The streams of each card (once started).
  pub fn card_streams(&self) -> Vec<CardStreams> {
    let Some(shared) = &self.shared else { return vec![] };
    let (mut rx, mut tx) = (0, 0);
    self
      .cards
      .iter()
      .map(|card| {
        let streams = CardStreams {
          shared: shared.clone(),
          playback: tx..tx + card.playback_channels,
          capture: rx..rx + card.capture_channels,
        };
        (rx, tx) = (rx + card.capture_channels, tx + card.playback_channels);
        streams
      })
      .collect()
  }

  /// The device's settings (with the cards' channels).
  pub fn settings(&self) -> Option<&Settings> {
    self.capture.iter().chain(self.playback.iter()).next().map(|s| s.settings())
  }

  fn close(&mut self) {
    self.region.header().set_state(State::Closed);
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
  use std::sync::atomic::AtomicUsize;

  use super::*;

  const LEAD: usize = 240;

  fn card(channels: usize) -> CardStreams {
    let source: Arc<dyn ClockSource> = Arc::new(inferno_aoip::clock::FreeRunningClockSource::new());
    let cards = [RegionCard {
      name: "A".into(),
      uid: "a".into(),
      playback_channels: channels,
      capture_channels: 0,
    }];
    let params = RegionParams {
      sample_rate: 48000,
      ring_frames: 8192,
      generation: 1,
      playback_lead: LEAD,
      input_latency: 0,
      output_latency: 0,
    };
    let region = Arc::new(Region::heap(&cards, params).unwrap());
    let shared = Arc::new(Shared { region, clock: Arc::new(EngineClock::new(&source, 48000, 0)) });
    CardStreams { shared, playback: 0..channels, capture: 0..0 }
  }

  #[test]
  fn playback_after_a_gap_leaves_silence_in_between() {
    let card = card(1);
    let region = card.shared.region.clone();
    let ring = region.playback_ring(0);
    ring.iter().for_each(|s| s.store(7, Ordering::Relaxed)); // a cycle ago
    card.playback(1000, &[1; 48]);
    let written: &AtomicUsize = &region.playback_written()[0];
    let end = written.load(Ordering::Relaxed);
    assert_eq!(end, 1000 + LEAD + 48);
    // the next transfer 100 frames later than the last ended
    card.playback(1148, &[2; 48]);
    let at = |p: usize| ring[p & (ring.len() - 1)].load(Ordering::Relaxed);
    assert_eq!(at(end - 1), 1);
    assert!((end..1148 + LEAD).all(|p| at(p) == 0), "the gap is silence");
    assert_eq!(at(1148 + LEAD), 2);
    assert_eq!(written.load(Ordering::Relaxed), 1148 + LEAD + 48);
  }
}
