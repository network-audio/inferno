//! A device shared by all streams of one process with the same device ID.
//!
//! Audio APIs open capture and playback separately, and an application may open several streams on one
//! device. They all share one [`DeviceServer`], running in its own thread with its own tokio runtime, so
//! that the stream callbacks (which aren't async, and may be called from real-time threads) only send it
//! commands.

use std::{
  collections::BTreeMap,
  sync::{atomic::AtomicUsize, Arc, Mutex, RwLock},
  thread::JoinHandle,
};

use futures_util::FutureExt;
use inferno_aoip::{
  clock::ClockSource,
  device_info::DeviceId,
  device_server::{
    AtomicSample, BuffersValid, DeviceServer, ExternalBufferParameters, RealTimeClockReceiver, Sample,
    Settings, StreamAnchor, TransferNotifier,
  },
  utils::run_future_in_new_thread,
};
use lazy_static::lazy_static;
use log::{debug, error, warn};
use tokio::sync::{mpsc, oneshot};

use crate::{DeviceError, Direction};

/// What a receiver/transmitter serving a stream is started with.
pub struct StreamStart {
  pub channels: Vec<ExternalBufferParameters<Sample>>,
  pub anchor: StreamAnchor,
  /// position the receiver/transmitter has processed up to, `usize::MAX` until it runs
  pub current_timestamp: Arc<AtomicUsize>,
  pub on_transfer: Option<TransferNotifier>,
}

/// The device's clock, as the stream needs it.
pub struct StreamClock {
  pub receiver: RealTimeClockReceiver,
  pub source: Arc<dyn ClockSource>,
}

struct StartArgs {
  start: StreamStart,
  clock_tx: oneshot::Sender<StreamClock>,
}

enum Command {
  StartReceiver(StartArgs),
  StartTransmitter(StartArgs),
  StopReceiver,
  StopTransmitter,
  Shutdown,
}

struct Instance {
  commands_sender: mpsc::Sender<Command>,
  _thread: JoinHandle<()>,
  capturing: bool,
  playing: bool,
  /// set if the device server failed to start (its thread then ends)
  start_error: Arc<Mutex<Option<String>>>,
  /// settings the device was started with, see `device_settings`
  settings: Vec<(&'static str, String)>,
}

impl Instance {
  fn running(&mut self, direction: Direction) -> &mut bool {
    match direction {
      Direction::Capture => &mut self.capturing,
      Direction::Playback => &mut self.playing,
    }
  }
}

lazy_static! {
  static ref INSTANCES: RwLock<BTreeMap<DeviceId, Arc<Mutex<Instance>>>> = RwLock::new(BTreeMap::new());
}

/// The settings of a device that the streams sharing it (same device ID in one process) should agree on.
fn device_settings(settings: &Settings) -> Vec<(&'static str, String)> {
  let info = &settings.self_info;
  vec![
    ("name", info.friendly_hostname.clone()),
    ("IP address", info.ip_address.to_string()),
    ("RX channels", info.rx_channels.len().to_string()),
    ("TX channels", info.tx_channels.len().to_string()),
    ("sample rate", info.sample_rate.to_string()),
    ("RX latency", format!("{} ns", info.latency_ns)),
    ("TX latency", format!("{} ns", settings.tx_latency_ns)),
    ("TX source bit depth", settings.tx_source_bit_depth.to_string()),
    ("ports", format!("{}..", info.arc_port)),
    ("clock", format!("{:?}", settings.clock_path)),
  ]
}

fn device_id_hex(id: &DeviceId) -> String {
  id.iter().map(|b| format!("{b:02x}")).collect()
}

/// The transmitter of a device (device server thread).
#[derive(PartialEq)]
enum Transmitter {
  None,
  /// sends silence
  Silent,
  /// serves the playback stream
  Stream,
}

static SILENCE: AtomicSample = AtomicSample::new(0);

/// Starts transmitting silence, or switches the running transmitter to it (keeping its flows).
async fn transmit_silence(device_server: &mut DeviceServer, channels: usize) -> Transmitter {
  // never valid and never started: the transmitter sends silence without touching the buffer
  let valid = BuffersValid::new(false);
  let anchor = StreamAnchor::new_paused_guarding(valid.clone());
  let buffers = (0..channels)
    .map(|_| unsafe { ExternalBufferParameters::new(&SILENCE, 1, 1, valid.clone(), None) })
    .collect();
  device_server.transmit_from_external_buffer(buffers, anchor, Default::default(), None).await;
  debug!("transmitting silence");
  Transmitter::Silent
}

fn stream_clock(device_server: &DeviceServer) -> StreamClock {
  StreamClock {
    receiver: device_server.get_realtime_clock_receiver(),
    source: device_server.clock_source(),
  }
}

/// Runs the device server, serving commands of the streams, until shut down.
async fn run_device(
  settings: Settings,
  clock_source: Option<Arc<dyn ClockSource>>,
  mut commands_rx: mpsc::Receiver<Command>,
  start_error: Arc<Mutex<Option<String>>>,
) {
  let tx_channels = settings.self_info.tx_channels.len();
  let started = match clock_source {
    Some(clock_source) => DeviceServer::try_start_with_clock(settings, clock_source).await,
    None => DeviceServer::try_start(settings).await,
  };
  let mut device_server = match started {
    Ok(device_server) => device_server,
    Err(e) => {
      error!("{e}");
      // reported to the stream; dropping commands_rx fails its requests
      *start_error.lock().unwrap() = Some(e.to_string());
      return;
    }
  };
  // While the device exists, it transmits: from the playback stream, or silence (e.g. for
  // capture-only applications), so that receivers don't see a broken stream.
  let mut tx = Transmitter::None;
  let mut receiving = false;
  while let Some(command) = commands_rx.recv().await {
    match command {
      Command::StartReceiver(StartArgs { start, clock_tx }) => {
        let _ = clock_tx.send(stream_clock(&device_server));
        device_server
          .receive_to_external_buffer(
            start.channels,
            start.anchor,
            start.current_timestamp,
            start.on_transfer,
          )
          .await;
        receiving = true;
        debug!("started receiver");
        if tx == Transmitter::None && tx_channels > 0 {
          tx = transmit_silence(&mut device_server, tx_channels).await;
        }
      }
      Command::StartTransmitter(StartArgs { start, clock_tx }) => {
        let _ = clock_tx.send(stream_clock(&device_server));
        // switches a running (silent) transmitter, keeping its flows
        tx = Transmitter::Stream;
        device_server
          .transmit_from_external_buffer(
            start.channels,
            start.anchor,
            start.current_timestamp,
            start.on_transfer,
          )
          .await;
        debug!("started transmitter");
      }
      Command::StopReceiver => {
        device_server.stop_receiver().await;
        receiving = false;
        debug!("stopped receiver");
        if tx == Transmitter::Silent {
          device_server.stop_transmitter().await;
          tx = Transmitter::None;
          debug!("stopped transmitting silence");
        }
      }
      Command::StopTransmitter => {
        if receiving && tx_channels > 0 {
          // keep the flows for the still capturing application
          tx = transmit_silence(&mut device_server, tx_channels).await;
        } else {
          device_server.stop_transmitter().await;
          tx = Transmitter::None;
          debug!("stopped transmitter");
        }
      }
      Command::Shutdown => break,
    }
  }
  device_server.shutdown().await;
}

/// Handle to the device shared by the streams of this process with the same device ID.
#[derive(Clone)]
pub struct SharedDevice {
  id: DeviceId,
  instance: Arc<Mutex<Instance>>,
}

impl SharedDevice {
  /// The device with `settings`' device ID, started (in its own thread) if not open yet.
  /// `clock_source` is used if the device is started: `None` for the platform's default.
  pub fn get_or_create(settings: &Settings, clock_source: Option<Arc<dyn ClockSource>>) -> Self {
    let id = settings.self_info.factory_device_id;
    let mut instances = INSTANCES.write().unwrap();
    let instance = instances
      .entry(id)
      .or_insert_with(|| {
        // TODO make tx & rx channels based on the streams' channel counts
        // this requires a complicated refactor to allow adding channels to the Dante network dynamically at any time, not just on DeviceServer start
        // because we don't know beforehand whether the app will be capture&playback, playback-only or capture-only
        // so we don't know whether we should wait for the second prepare call to gather all channels counts
        let (commands_sender, commands_rx) = mpsc::channel(16);
        let start_error: Arc<Mutex<Option<String>>> = Default::default();
        let thread_settings = settings.clone();
        let thread_start_error = start_error.clone();
        let thread = run_future_in_new_thread("Inferno main", move || {
          run_device(thread_settings, clock_source, commands_rx, thread_start_error).boxed_local()
        });
        Arc::new(Mutex::new(Instance {
          commands_sender,
          _thread: thread,
          capturing: false,
          playing: false,
          start_error,
          settings: device_settings(settings),
        }))
      })
      .clone();
    Self { id, instance }
  }

  /// Whether a device with this ID is open in this process.
  pub fn is_open(id: &DeviceId) -> bool {
    INSTANCES.read().unwrap().contains_key(id)
  }

  /// Warns if a stream's settings differ from those of the device it shares (they don't apply).
  pub fn warn_if_settings_differ(&self, settings: &Settings) {
    let instance = self.instance.lock().unwrap();
    let differences: Vec<String> = instance
      .settings
      .iter()
      .zip(device_settings(settings))
      .filter(|((_, device), (_, this))| device != this)
      .map(|((key, device), (_, this))| format!("{key} {device} (this stream: {this})"))
      .collect();
    if !differences.is_empty() {
      warn!(
        "this stream shares device {} with a stream already open in this process, whose settings apply: \
         {}. To make it a separate device, give it its own PROCESS_ID (or DEVICE_ID), NAME and ALT_PORT.",
        device_id_hex(&self.id),
        differences.join(", ")
      );
    }
  }

  /// Whether the receiver (capture) or transmitter (playback) is serving a stream.
  pub fn is_running(&self, direction: Direction) -> bool {
    *self.instance.lock().unwrap().running(direction)
  }

  /// (Re)starts the receiver (capture) or transmitter (playback) serving a stream. A running one is switched
  /// to the new buffers, keeping its flows and subscriptions. Returns the device's clock.
  pub fn start(&self, direction: Direction, start: StreamStart) -> Result<StreamClock, DeviceError> {
    let (clock_tx, clock_rx) = oneshot::channel();
    let args = StartArgs { start, clock_tx };
    let command = match direction {
      Direction::Capture => Command::StartReceiver(args),
      Direction::Playback => Command::StartTransmitter(args),
    };
    let sent = {
      let mut instance = self.instance.lock().unwrap();
      let sent = instance.commands_sender.blocking_send(command).is_ok();
      *instance.running(direction) = sent;
      sent
    };
    if !sent {
      return Err(self.thread_failed());
    }
    clock_rx.blocking_recv().map_err(|_| self.thread_failed())
  }

  /// The device thread didn't take a request: it failed to start (already logged why), or has ended.
  /// The failed device is forgotten, so that a later attempt (e.g. after freeing its ports) starts it again.
  fn thread_failed(&self) -> DeviceError {
    let start_error = self.instance.lock().unwrap().start_error.lock().unwrap().clone();
    let Some(reason) = start_error else {
      error!("BUG: the Inferno thread has ended");
      return DeviceError::ThreadEnded;
    };
    // lock order: INSTANCES, then an instance
    let mut instances = INSTANCES.write().unwrap();
    if instances.get(&self.id).is_some_and(|entry| Arc::ptr_eq(entry, &self.instance)) {
      instances.remove(&self.id);
    }
    DeviceError::StartFailed(reason)
  }

  /// A stream of `direction` on device `id` is closed: stops its receiver/transmitter if `was_active`,
  /// and shuts the device down when no stream uses it anymore.
  pub fn release(id: &DeviceId, direction: Direction, was_active: bool) {
    let mut instances = INSTANCES.write().unwrap();
    let Some(instance) = instances.get(id).cloned() else {
      return;
    };
    let mut instance = instance.lock().unwrap();
    // TODO blocking_send inside mutex, risk of deadlock?
    if was_active && *instance.running(direction) {
      *instance.running(direction) = false;
      let command = match direction {
        Direction::Capture => Command::StopReceiver,
        Direction::Playback => Command::StopTransmitter,
      };
      if instance.commands_sender.blocking_send(command).is_err() {
        error!("BUG: error sending to Inferno thread");
      }
    }
    if !instance.capturing && !instance.playing {
      if let Err(e) = instance.commands_sender.blocking_send(Command::Shutdown) {
        error!("BUG: send shutdown via channel failed: {e:?}");
      }
      instances.remove(id);
    }
  }
}
