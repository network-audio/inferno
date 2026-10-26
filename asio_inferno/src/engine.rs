//! The audio engine behind the ASIO driver, independent of ASIO (and of Windows, so it's testable anywhere).
//!
//! ASIO hosts work with double buffers of one period per channel: on each buffer switch, the host processes
//! one half (reads the inputs, writes the outputs) while the driver uses the other. Inferno's receiver and
//! transmitter work with ring buffers addressed by media clock time, which must hold the network latency,
//! far more than a period. So the engine keeps its own rings and copies to and from the host's halves:
//!
//! * switch `k` (half `k % 2`) happens when the media clock reaches the end of period `k`: the capture ring
//!   holds the period's received audio (the receiver writes ahead, by the network latency)
//! * the host's outputs of switch `k` go to period `k + 2` of the playback ring: the host has up to a
//!   period to process, and the transmitter reads a period as it's due
//!
//! Capture and playback are `inferno_driver_core` streams of one shared device, started at the same time.

use std::{
  sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    Arc, Mutex,
  },
  thread::JoinHandle,
  time::Duration,
};

use inferno_aoip::{
  clock::ClockSource,
  device_server::{AtomicSample, Clock, ExternalBufferParameters, MediaClock, Sample, Settings},
};
use inferno_driver_core::{BuffersSignature, DeviceError, Direction, Stream, StreamOptions};
use log::{error, info, warn};

/// What the engine calls on each buffer switch.
pub trait Host: Send + 'static {
  /// Process half `index` (0 or 1) of the double buffers. `position`: the half's first frame, counted from the
  /// start; `system_time_ns`: when the switch happened (see `system_time_ns`).
  fn buffer_switch(&mut self, index: usize, position: u64, system_time_ns: u64);
}

/// Double buffers of one channel, shared with the host: two halves of `period` samples.
struct HostBuffer(Box<[Sample]>);

/// The host's and the engine's thread use different halves at any time (the ASIO contract).
struct HostBuffers(Vec<std::cell::UnsafeCell<HostBuffer>>);
unsafe impl Sync for HostBuffers {}
unsafe impl Send for HostBuffers {}

impl HostBuffers {
  fn new(channels: usize, period: usize) -> Arc<Self> {
    Arc::new(Self(
      (0..channels)
        .map(|_| std::cell::UnsafeCell::new(HostBuffer(vec![0; 2 * period].into_boxed_slice())))
        .collect(),
    ))
  }
  fn ptr(&self, channel: usize, half: usize, period: usize) -> *mut Sample {
    unsafe { (*self.0[channel].get()).0.as_mut_ptr().add(half * period) }
  }
}

type Rings = Arc<Vec<Box<[AtomicSample]>>>;

fn rings(channels: usize, frames: usize) -> Rings {
  Arc::new((0..channels).map(|_| (0..frames).map(|_| AtomicSample::new(0)).collect()).collect())
}

/// Where the host is, for ASIO's getSamplePosition.
#[derive(Default)]
pub struct Position {
  /// first frame of the half of the last switch
  pub samples: AtomicU64,
  pub system_time_ns: AtomicU64,
  pub running: AtomicBool,
}

struct Running {
  stop: Arc<AtomicBool>,
  thread: JoinHandle<()>,
}

pub struct Engine {
  sample_rate: u32,
  period: usize,
  /// how late (frames) buffer switches may be caught up on without losing captured audio
  max_lateness: usize,
  capture: Option<Stream>,
  playback: Option<Stream>,
  capture_rings: Rings,
  playback_rings: Rings,
  /// frames since the start written to the playback rings
  playback_written: Arc<AtomicUsize>,
  inputs: Arc<HostBuffers>,
  outputs: Arc<HostBuffers>,
  position: Arc<Position>,
  running: Option<Running>,
}

/// Ring length (power of 2): the network latency plus a few periods.
fn ring_frames(period: usize, latency: usize) -> usize {
  (latency + 4 * period).next_power_of_two().max(8192)
}

/// Monotonic system time in nanoseconds for ASIO time stamps: Windows' `timeGetTime` (milliseconds), as the
/// ASIO spec asks.
pub fn system_time_ns() -> u64 {
  #[cfg(windows)]
  {
    // safety: no preconditions
    (unsafe { windows_sys::Win32::Media::timeGetTime() } as u64) * 1_000_000
  }
  #[cfg(not(windows))]
  {
    use std::{sync::OnceLock, time::Instant};
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
  }
}

impl Engine {
  /// Starts the device's receiver and transmitter (if it has RX and TX channels) for buffer switches every
  /// `period` frames. Blocks until the device has a clock.
  pub fn new(settings: Settings, period: usize) -> Result<Self, DeviceError> {
    Self::new_with_clock(settings, period, None)
  }

  /// Like `new`, with the device following `clock_source` if it's started (`None`: its settings' source).
  pub fn new_with_clock(
    settings: Settings,
    period: usize,
    clock_source: Option<Arc<dyn ClockSource>>,
  ) -> Result<Self, DeviceError> {
    let with_clock = |stream: Stream| match &clock_source {
      Some(source) => stream.with_clock_source(source.clone()),
      None => stream,
    };
    let info = &settings.self_info;
    let (rx, tx) = (info.rx_channels.len(), info.tx_channels.len());
    let latency = info
      .latency_samples()
      .max(settings.tx_latency_ns as usize * info.sample_rate as usize / 1_000_000_000);
    let frames = ring_frames(period, latency);
    let mut engine = Self {
      sample_rate: info.sample_rate,
      period,
      // the receiver writes up to the latency ahead: a late switch may still read the rest of the ring
      max_lateness: frames - latency - 2 * period,
      capture: None,
      playback: None,
      capture_rings: rings(rx, frames),
      playback_rings: rings(tx, frames),
      playback_written: Arc::new(AtomicUsize::new(0)),
      inputs: HostBuffers::new(rx, period),
      outputs: HostBuffers::new(tx, period),
      position: Arc::new(Position::default()),
      running: None,
    };
    let options = || StreamOptions { track_position: false, on_transfer: None };
    if rx > 0 {
      let mut stream = with_clock(Stream::new(Direction::Capture, settings.clone()));
      let channels = engine
        .capture_rings
        .iter()
        .map(|ring| unsafe {
          ExternalBufferParameters::new(ring.as_ptr(), frames, 1, stream.buffers_valid(), None)
        })
        .collect();
      stream.prepare(signature(&engine.capture_rings, period), channels, options())?;
      engine.capture = Some(stream);
    }
    if tx > 0 {
      let mut stream = with_clock(Stream::new(Direction::Playback, settings));
      let channels = engine
        .playback_rings
        .iter()
        .map(|ring| {
          let written = engine.playback_written.clone();
          unsafe { ExternalBufferParameters::new(ring.as_ptr(), frames, 1, stream.buffers_valid(), None) }
            // silence where the host hasn't written (not started, or overloaded)
            .with_written_from(Arc::new(move |position| {
              written.load(Ordering::Acquire).saturating_sub(position)
            }))
        })
        .collect();
      if let Err(e) = stream.prepare(signature(&engine.playback_rings, period), channels, options()) {
        engine.close();
        return Err(e);
      }
      engine.playback = Some(stream);
    }
    Ok(engine)
  }

  pub fn period(&self) -> usize {
    self.period
  }

  /// The host's buffer for half `half` (0 or 1) of input `channel`: `period` samples.
  pub fn input_buffer(&self, channel: usize, half: usize) -> Option<*mut Sample> {
    (channel < self.inputs.0.len()).then(|| self.inputs.ptr(channel, half, self.period))
  }

  /// The host's buffer for half `half` of output `channel`.
  pub fn output_buffer(&self, channel: usize, half: usize) -> Option<*mut Sample> {
    (channel < self.outputs.0.len()).then(|| self.outputs.ptr(channel, half, self.period))
  }

  pub fn position(&self) -> Arc<Position> {
    self.position.clone()
  }

  fn streams(&mut self) -> impl Iterator<Item = &mut Stream> {
    self.capture.iter_mut().chain(self.playback.iter_mut())
  }

  /// Starts calling `host` every period.
  pub fn start(&mut self, host: Box<dyn Host>) -> Result<(), DeviceError> {
    if self.running.is_some() {
      return Ok(());
    }
    let rate = self.sample_rate as u64;
    let first = self.streams().next().ok_or(DeviceError::ThreadEnded)?;
    let clock_source = first.clock_source().ok_or(DeviceError::ThreadEnded)?;
    let start_time =
      first.media_now(rate).ok_or(DeviceError::NoClock(clock_source.diagnose_missing()))?;
    // the host's outputs of the first switch are due 2 periods later: silence until then
    self.playback_written.store(0, Ordering::Release);
    for stream in self.streams() {
      stream.start_at(start_time);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let worker = Worker {
      sample_rate: self.sample_rate,
      period: self.period,
      max_lateness: self.max_lateness,
      start_time,
      clock_source,
      capture_rings: self.capture_rings.clone(),
      playback_rings: self.playback_rings.clone(),
      playback_written: self.playback_written.clone(),
      inputs: self.inputs.clone(),
      outputs: self.outputs.clone(),
      position: self.position.clone(),
      stop: stop.clone(),
    };
    let thread = std::thread::Builder::new()
      .name("Inferno ASIO".into())
      .spawn(move || worker.run(host))
      .map_err(|e| DeviceError::StartFailed(e.to_string()))?;
    self.position.running.store(true, Ordering::Release);
    self.running = Some(Running { stop, thread });
    info!("started: {} frames per period", self.period);
    Ok(())
  }

  /// Stops calling the host. The device keeps running (flows and subscriptions are kept), paused.
  pub fn stop(&mut self) {
    if let Some(running) = self.running.take() {
      running.stop.store(true, Ordering::Release);
      let _ = running.thread.join();
    }
    self.position.running.store(false, Ordering::Release);
    for stream in self.streams() {
      stream.stop();
    }
  }

  fn close(&mut self) {
    self.stop();
    for stream in self.streams() {
      stream.close();
    }
  }
}

impl Drop for Engine {
  fn drop(&mut self) {
    self.close();
  }
}

fn signature(rings: &Rings, period: usize) -> BuffersSignature {
  BuffersSignature {
    areas: rings.iter().map(|r| (r.as_ptr() as usize, r.len(), 1)).collect(),
    period_size: period,
  }
}

struct Worker {
  sample_rate: u32,
  period: usize,
  max_lateness: usize,
  start_time: Clock,
  clock_source: Arc<dyn ClockSource>,
  capture_rings: Rings,
  playback_rings: Rings,
  playback_written: Arc<AtomicUsize>,
  inputs: Arc<HostBuffers>,
  outputs: Arc<HostBuffers>,
  position: Arc<Position>,
  stop: Arc<AtomicBool>,
}

impl Worker {
  fn run(self, mut host: Box<dyn Host>) {
    let _priority = sleep::realtime_thread();
    let timer = sleep::Timer::new();
    let rate = self.sample_rate as u64;
    let n = self.period;
    let mut clock_rx = self.clock_source.subscribe();
    let mut clock = MediaClock::new(false);
    let mut k: usize = 0;
    let mut late_switches = 0u64;
    let mut lost_outputs = 0u64;
    while !self.stop.load(Ordering::Acquire) {
      if let Some(overlay) = *clock_rx.borrow_and_update() {
        clock.update_overlay(overlay);
      }
      let Some(now) = clock.wrapping_now_in_timebase(rate) else {
        timer.sleep(Duration::from_millis(1));
        continue;
      };
      let position = now.wrapping_sub(self.start_time) as isize;
      let due = ((k + 1) * n) as isize;
      if position < due {
        let remaining = Duration::from_nanos(((due - position) as u64) * 1_000_000_000 / rate);
        // wake up a little early, then poll: timers are only so precise
        timer.sleep(remaining.saturating_sub(Duration::from_micros(300)).max(Duration::from_micros(100)));
        continue;
      }
      // Late (this thread or the host stalled): catch up switch by switch, the capture ring still holds the
      // audio; outputs for the missed time are lost. Skip ahead only if the receiver overwrote it.
      if position > due + n as isize {
        late_switches += 1;
        let skip = position - due > self.max_lateness as isize;
        if late_switches.is_power_of_two() || skip {
          warn!(
            "buffer switch late by {} frames ({late_switches} times so far), {}",
            position - due,
            if skip { "skipping ahead: captured audio lost" } else { "catching up" }
          );
        }
        if skip {
          k = position as usize / n - 1;
        }
      }
      let half = k % 2;
      let mask_c = self.capture_rings.first().map(|r| r.len() - 1).unwrap_or(0);
      for (ch, ring) in self.capture_rings.iter().enumerate() {
        let dst = unsafe { std::slice::from_raw_parts_mut(self.inputs.ptr(ch, half, n), n) };
        for (i, d) in dst.iter_mut().enumerate() {
          *d = ring[(k * n + i) & mask_c].load(Ordering::Relaxed);
        }
      }
      self.position.samples.store((k * n) as u64, Ordering::Release);
      let system_time = system_time_ns();
      self.position.system_time_ns.store(system_time, Ordering::Release);
      host.buffer_switch(half, (k * n) as u64, system_time);
      let mask_p = self.playback_rings.first().map(|r| r.len() - 1).unwrap_or(0);
      let out_start = (k + 2) * n;
      for (ch, ring) in self.playback_rings.iter().enumerate() {
        let src = unsafe { std::slice::from_raw_parts(self.outputs.ptr(ch, half, n), n) };
        for (i, s) in src.iter().enumerate() {
          ring[(out_start + i) & mask_p].store(*s, Ordering::Relaxed);
        }
      }
      self.playback_written.store(out_start + n, Ordering::Release);
      // the transmitter reads a position as soon as it's due: outputs written after that went out as silence
      if let Some(now) = clock.wrapping_now_in_timebase(rate) {
        let overdue = now.wrapping_sub(self.start_time) as isize - out_start as isize;
        if overdue > 0 && !self.playback_rings.is_empty() {
          lost_outputs += 1;
          if lost_outputs.is_power_of_two() {
            warn!(
              "buffer switch finished {overdue} frames after its outputs were due: they went out as \
               silence ({lost_outputs} times so far; the host must return within a period)"
            );
          }
        }
      }
      k += 1;
    }
  }
}

/// Precise sleeping and real-time priority for the switch thread.
mod sleep {
  use std::time::Duration;

  #[cfg(windows)]
  pub struct Timer(windows_sys::Win32::Foundation::HANDLE);

  #[cfg(windows)]
  impl Timer {
    pub fn new() -> Self {
      use windows_sys::Win32::System::Threading::{
        CreateWaitableTimerExW, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
      };
      // safety: no preconditions; a null handle falls back to Sleep
      Self(unsafe {
        CreateWaitableTimerExW(
          std::ptr::null(),
          std::ptr::null(),
          CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
          TIMER_ALL_ACCESS,
        )
      })
    }
    pub fn sleep(&self, duration: Duration) {
      use windows_sys::Win32::System::Threading::{SetWaitableTimer, WaitForSingleObject, INFINITE};
      if self.0.is_null() {
        std::thread::sleep(duration);
        return;
      }
      // relative, in 100 ns units
      let due = -((duration.as_nanos() / 100).max(1) as i64);
      // safety: valid timer handle
      unsafe {
        if SetWaitableTimer(self.0, &due, 0, None, std::ptr::null(), 0) != 0 {
          WaitForSingleObject(self.0, INFINITE);
        } else {
          std::thread::sleep(duration);
        }
      }
    }
  }

  #[cfg(windows)]
  impl Drop for Timer {
    fn drop(&mut self) {
      if !self.0.is_null() {
        // safety: owned handle
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
      }
    }
  }

  #[cfg(not(windows))]
  pub struct Timer;

  #[cfg(not(windows))]
  impl Timer {
    pub fn new() -> Self {
      Self
    }
    pub fn sleep(&self, duration: Duration) {
      std::thread::sleep(duration);
    }
  }

  /// Real-time priority for the current thread while the guard lives: MMCSS "Pro Audio" on Windows.
  pub struct Priority(#[cfg(windows)] windows_sys::Win32::Foundation::HANDLE);

  pub fn realtime_thread() -> Priority {
    #[cfg(windows)]
    {
      use windows_sys::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
      let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
      let mut index = 0u32;
      // safety: valid null-terminated string and index pointer
      let handle = unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut index) };
      if handle.is_null() {
        log::warn!("can't register the ASIO thread with MMCSS (Pro Audio), expect dropouts");
        let _ = inferno_aoip::utils::set_current_thread_realtime(85);
      }
      Priority(handle)
    }
    #[cfg(not(windows))]
    {
      if let Err(e) = inferno_aoip::utils::set_current_thread_realtime(85) {
        log::warn!("can't set realtime priority of the ASIO thread ({e:?}), expect dropouts");
      }
      Priority()
    }
  }

  #[cfg(windows)]
  impl Drop for Priority {
    fn drop(&mut self) {
      if !self.0.is_null() {
        // safety: handle from AvSetMmThreadCharacteristicsW
        unsafe { windows_sys::Win32::System::Threading::AvRevertMmThreadCharacteristics(self.0) };
      }
    }
  }
}

/// Logs the error (once) and keeps the message for the host (ASIO getErrorMessage).
pub fn report(last_error: &Mutex<String>, message: impl Into<String>) {
  let message = message.into();
  error!("{message}");
  *last_error.lock().unwrap() = message;
}
