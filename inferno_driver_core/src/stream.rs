//! A stream (PCM) of an audio API, served by the receiver (capture) or transmitter (playback) of a
//! [`SharedDevice`].
//!
//! DAWs and audio servers restart streams all the time: on transport stop/start and on every xrun. A
//! restart only pauses and re-anchors the stream; the receiver/transmitter, its flows and subscriptions
//! keep running, so Dante connections aren't broken. They're only restarted when the application's
//! buffers change.

use std::{
  sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, RwLock,
  },
  time::Duration,
};

use inferno_aoip::{
  clock::ClockSource,
  device_server::{
    Clock, ExternalBufferParameters, MediaClock, RealTimeClockReceiver, Sample, Settings, StreamAnchor,
    TransferNotifier,
  },
};
use log::{debug, error, warn};

use crate::{
  shared_device::{SharedDevice, StreamClock, StreamStart},
  DeviceError, Direction,
};

/// How long `Stream::prepare` waits for the clock.
/// (a PTPv1 follower publishes the clock after 1.5-4 s of Syncs)
const CLOCK_WAIT: Duration = Duration::from_secs(8);
const CLOCK_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Identifies what a running receiver/transmitter was started with: the memory it is bound to,
/// (address, length, stride) per channel, and the period size (wakeup interval).
/// It only changes when the application changes its buffers (e.g. ALSA hw_params).
#[derive(PartialEq, Debug, Clone)]
pub struct BuffersSignature {
  pub areas: Vec<(usize, usize, usize)>,
  pub period_size: usize,
}

/// How `Stream::prepare` serves the stream if it (re)starts the receiver/transmitter.
pub struct StreamOptions {
  /// whether the receiver/transmitter reports the position it has processed up to (see
  /// `Stream::flows_position`)
  pub track_position: bool,
  /// called when the application should be woken up to transfer audio
  pub on_transfer: Option<TransferNotifier>,
}

/// What `Stream::prepare` did.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum Prepared {
  /// started the receiver/transmitter, or switched it to new buffers
  Started,
  /// kept the running receiver/transmitter: same buffers, the stream is only re-anchored on start
  Kept,
}

pub struct Stream {
  direction: Direction,
  settings: Settings,
  /// used if this stream starts the device
  clock_source: Option<Arc<dyn ClockSource>>,
  /// whether this stream's settings were compared with those of the device it shares (once, on first prepare)
  shared_settings_checked: bool,
  /// whether the application's buffers may be accessed (by the receiver/transmitter)
  buffers_valid: Arc<RwLock<bool>>,
  /// shared with the receiver/transmitter serving this stream
  anchor: StreamAnchor,
  media_clock: MediaClock,
  clock: Option<StreamClock>,
  start_time: Option<Clock>,
  /// buffers the running receiver/transmitter writes to/reads from, if any
  active_buffers: Option<BuffersSignature>,
  current_timestamp: Arc<AtomicUsize>,
}

impl Stream {
  pub fn new(direction: Direction, settings: Settings) -> Self {
    let buffers_valid = Arc::new(RwLock::new(false));
    Self {
      direction,
      settings,
      clock_source: None,
      shared_settings_checked: false,
      anchor: StreamAnchor::new_paused_guarding(buffers_valid.clone()),
      buffers_valid,
      media_clock: MediaClock::new(false /* TODO */),
      clock: None,
      start_time: None,
      active_buffers: None,
      current_timestamp: Arc::new(AtomicUsize::new(usize::MAX)),
    }
  }

  /// Uses `clock_source` (instead of the platform's default) if this stream starts the device.
  pub fn with_clock_source(mut self, clock_source: Arc<dyn ClockSource>) -> Self {
    self.clock_source = Some(clock_source);
    self
  }

  pub fn direction(&self) -> Direction {
    self.direction
  }

  pub fn settings(&self) -> &Settings {
    &self.settings
  }

  /// Guards the application's buffers: give it to the `ExternalBufferParameters` passed to `prepare`.
  pub fn buffers_valid(&self) -> Arc<RwLock<bool>> {
    self.buffers_valid.clone()
  }

  /// Media clock timestamp (in samples) of the stream's position 0, once started.
  pub fn start_time(&self) -> Option<Clock> {
    self.start_time
  }

  /// Network latency the stream's audio has, in samples.
  pub fn network_latency_samples(&self) -> usize {
    self.anchor.network_latency_samples()
  }

  /// Position (frames since start) the receiver/transmitter has actually processed up to, if it's running
  /// and reports it (`StreamOptions::track_position`).
  pub fn flows_position(&self) -> Option<isize> {
    let current = self.current_timestamp.load(Ordering::SeqCst /*TODO: really needed?*/);
    match self.start_time {
      Some(start_time) if current != usize::MAX => Some(current.wrapping_sub(start_time) as isize),
      _ => None,
    }
  }

  /// Position (frames since start) according to the media clock, once started and the clock is available.
  pub fn clock_position(&mut self, sample_rate: u64) -> Option<isize> {
    self.update_clock();
    // not wrapping: the application's position may wrap at a boundary which isn't a power of 2
    let now = self.media_clock.now_in_timebase(sample_rate)?;
    Some((now as usize).wrapping_sub(self.start_time?) as isize)
  }

  /// Takes the latest clock update, returns whether the clock is available.
  fn update_clock(&mut self) -> bool {
    if let Some(clock) = &mut self.clock {
      clock.receiver.update();
      // also the initial overlay, which isn't an update
      if let Some(overlay) = clock.receiver.get() {
        self.media_clock.update_overlay(*overlay);
      }
    }
    self.media_clock.is_ready()
  }

  /// Prepares the stream to (re)start: pauses it, and makes sure a receiver/transmitter serves `channels`
  /// (described by `signature`). If one already serves the same buffers, it's kept running.
  ///
  /// Pausing marks the buffers invalid, waiting for any access in progress: a receiver thread which hasn't
  /// noticed the pause yet mustn't write to it, the application may clear it, or even free it. They become
  /// valid again once the receiver/transmitter has applied the start time set in `start`.
  pub fn prepare(
    &mut self,
    signature: BuffersSignature,
    channels: Vec<ExternalBufferParameters<Sample>>,
    options: StreamOptions,
  ) -> Result<Prepared, DeviceError> {
    self.start_time = None;
    self.anchor.pause();

    let device = SharedDevice::get_or_create(&self.settings, self.clock_source.clone());
    if !self.shared_settings_checked {
      self.shared_settings_checked = true;
      device.warn_if_settings_differ(&self.settings);
    }
    if device.is_running(self.direction) && self.active_buffers.as_ref() == Some(&signature) {
      // Restarting the stream (transport stop/start, xrun recovery): keep the receiver/transmitter,
      // its flows and subscriptions running, the stream will be re-anchored in `start`.
      debug!("re-preparing running stream");
      return Ok(Prepared::Kept);
    }
    self.start_receiver_or_transmitter(&device, channels, options)?;
    self.active_buffers = Some(signature);
    Ok(Prepared::Started)
  }

  /// (Re)starts the receiver or transmitter serving this stream, bound to the given buffers, and waits for
  /// the clock.
  fn start_receiver_or_transmitter(
    &mut self,
    device: &SharedDevice,
    channels: Vec<ExternalBufferParameters<Sample>>,
    options: StreamOptions,
  ) -> Result<(), DeviceError> {
    self.anchor = StreamAnchor::new_paused_guarding(self.buffers_valid.clone());
    self.active_buffers = None;
    self.current_timestamp.store(usize::MAX, Ordering::SeqCst);
    let start = StreamStart {
      channels,
      anchor: self.anchor.clone(),
      current_timestamp: if options.track_position {
        self.current_timestamp.clone()
      } else {
        Default::default()
      },
      on_transfer: options.on_transfer,
    };
    self.clock = Some(device.start(self.direction, start)?);

    let mut waited = Duration::ZERO;
    while !self.update_clock() {
      if waited >= CLOCK_WAIT {
        let diagnosis = self.clock.as_ref().unwrap().source.diagnose_missing();
        error!("no clock after {} s, can't start the stream: {diagnosis}", CLOCK_WAIT.as_secs());
        return Err(DeviceError::NoClock(diagnosis));
      }
      std::thread::sleep(CLOCK_POLL_INTERVAL);
      waited += CLOCK_POLL_INTERVAL;
    }
    Ok(())
  }

  /// Media clock time (in samples, wrapping), once the clock is available.
  pub fn media_now(&mut self, sample_rate: u64) -> Option<Clock> {
    self.update_clock();
    self.media_clock.wrapping_now_in_timebase(sample_rate)
  }

  /// Starts the prepared stream with position 0 at media clock time `start_time`, e.g. to start several
  /// streams in lockstep. Returns false if it's already started.
  pub fn start_at(&mut self, start_time: Clock) -> bool {
    if self.start_time.is_some() {
      return false;
    }
    self.start_time = Some(start_time);
    // the receiver/transmitter marks the buffer valid once it addresses it according to start_time
    self.anchor.start(start_time);
    true
  }

  /// The source of the device's clock, once prepared.
  pub fn clock_source(&self) -> Option<Arc<dyn ClockSource>> {
    self.clock.as_ref().map(|c| c.source.clone())
  }

  /// Starts the prepared stream now: its position 0 is the current media clock time.
  /// Returns the start time, or `None` if it can't start (no clock, or already started).
  pub fn start(&mut self, sample_rate: u64) -> Option<Clock> {
    if !self.update_clock() {
      warn!("no overlay for media_clock");
    }
    // start_time is used in:
    // * flows_rx and flows_tx (via anchor) for calculating ringbuffer positions
    // * the stream's position (e.g. ALSA's hw pointer)
    // all these calculations are wrapping so we can use wrapping clock
    let now = self.media_clock.wrapping_now_in_timebase(sample_rate);
    match (now, self.start_time) {
      (Some(now), None) => {
        self.start_time = Some(now);
        // the receiver/transmitter marks the buffer valid once it addresses it according to start_time
        self.anchor.start(now);
        Some(now)
      }
      _ => {
        warn!("can't set start_time: now: {now:?}, start_time: {:?}", self.start_time);
        None
      }
    }
  }

  /// Stops the stream. Only pauses: the receiver/transmitter, its flows and subscriptions keep running,
  /// so that the stream can be restarted without a break in Dante connections. While paused, the buffer
  /// is invalid: the receiver doesn't touch it and the transmitter sends silence.
  pub fn stop(&mut self) {
    self.anchor.pause();
  }

  /// Closes the stream: stops its receiver/transmitter, and the device if no other stream uses it.
  pub fn close(&mut self) {
    self.anchor.pause();
    let was_active = self.active_buffers.take().is_some();
    SharedDevice::release(&self.settings.self_info.factory_device_id, self.direction, was_active);
  }

  /// The device's real-time clock receiver, once prepared.
  pub fn clock_receiver(&mut self) -> Option<&mut RealTimeClockReceiver> {
    self.clock.as_mut().map(|c| &mut c.receiver)
  }
}
