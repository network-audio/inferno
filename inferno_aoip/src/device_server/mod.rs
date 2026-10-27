use crate::mdns_client::{MdnsClient, PointerToMulticast};
use crate::clock::{start_clock_source, ClockSource};
use crate::media_clock::{async_clock_receiver_to_realtime, make_shared_media_clock};
use crate::net_utils::UdpSocketWrapper;
use crate::ring_buffer::{
  self, ExternalBuffer, OwnedBuffer, ProxyToBuffer, ProxyToSamplesBuffer, RBOutput,
};
use crate::state_storage::StateStorage;
use atomic::Atomic;
use flows_tx::FlowsTransmitter;
use futures::{Future, FutureExt};
use itertools::Itertools;
use mdns_server::DeviceMDNSResponder;
use std::net::SocketAddrV4;
use tokio::task::JoinHandle;
use tx_multicasts::TransmitMulticasts;

use std::collections::BTreeMap;
use std::io::Write;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, RwLock};

use std::time::{Duration, Instant};
use tokio::sync::{broadcast as broadcast_queue, mpsc, watch, Mutex};

use crate::common::*;
use crate::device_info::DeviceInfo;

pub(crate) mod arc_server;
pub(crate) mod cmc_server;
pub(crate) mod flows_control_server;
pub(crate) mod info_mcast_server;
pub(crate) mod mdns_server;

pub(crate) mod channels_subscriber;
pub(crate) mod flows_rx;
pub(crate) mod flows_tx;
mod peaks;
pub(crate) mod samples_collector;
pub(crate) mod samples_utils;
pub(crate) mod saved_settings;
pub(crate) mod settings;
mod stream_anchor;
pub(crate) mod tx_multicasts;

pub use crate::common::{Clock, ClockDiff, Sample};
#[cfg(unix)]
pub use crate::clock::{clock_path_or_default, diagnose_missing_clock};
pub use crate::clock::{ClockOverlay, FreeRunningClockSource};
pub use crate::media_clock::{MediaClock, RealTimeClockReceiver};
pub use crate::ring_buffer::{
  new_owned, BuffersValid, ExternalBufferParameters, PositionReportDestination, RBInput, WrittenFrom,
};
pub use crate::state_storage::state_dir;
pub use settings::{Settings, SettingsError};
pub use stream_anchor::StreamAnchor;
pub type AtomicSample = atomic::Atomic<Sample>;

use channels_subscriber::{ChannelsBuffering, ChannelsSubscriber, ExternalBuffering, OwnedBuffering};
use peaks::peaks_of_buffers;
use samples_collector::{RealTimeSamplesReceiver, SamplesCallback, SamplesCollector};

const PEAKS_BUFFER_LEN: usize = 24000;
const PEAKS_ITER_SLEEP: Duration = Duration::from_millis(100);

pub struct TransferNotifier {
  pub callback: Box<dyn Fn() + Send + Sync>,
  pub max_interval_samples: Clock,
  /// media time slot of the last notification, see `notify_at`
  last_slot: std::sync::atomic::AtomicUsize,
}

impl TransferNotifier {
  /// `callback` is called (from a real-time thread) as the stream's position advances, at least every
  /// `max_interval_samples` (e.g. a period) and more often.
  pub fn new(callback: Box<dyn Fn() + Send + Sync>, max_interval_samples: Clock) -> Self {
    Self { callback, max_interval_samples, last_slot: usize::MAX.into() }
  }
  /// Calls the callback when media time `now` entered another quarter of `max_interval_samples`: a reader
  /// waiting for a period is woken within a quarter period (and a packet) of it being ready, not on every
  /// packet. Unconditionally without the time.
  pub(crate) fn notify_at(&self, now: Option<Clock>) {
    if let Some(now) = now {
      let slot = now / (self.max_interval_samples / 4).max(1);
      if self.last_slot.swap(slot, std::sync::atomic::Ordering::Relaxed) == slot {
        return;
      }
    }
    (self.callback)();
  }
}

/// Listens on the device's 4 ports: ARC, CMC, flows control, device info.
async fn listen_on_device_ports(
  info: &DeviceInfo,
  shutdown: &broadcast_queue::Sender<()>,
) -> Result<[UdpSocketWrapper; 4], StartError> {
  let ip = info.ip_address;
  let mut sockets = Vec::with_capacity(4);
  for (service, port) in [
    ("ARC (routing control)", info.arc_port),
    ("CMC (control and monitoring)", info.cmc_port),
    ("flows control", info.flows_control_port),
    ("device info", info.info_request_port),
  ] {
    let socket = UdpSocketWrapper::bind(Some(ip), port, shutdown.subscribe())
      .await
      .map_err(|source| StartError { service, addr: SocketAddrV4::new(ip, port), source })?;
    sockets.push(socket);
  }
  Ok(sockets.try_into().unwrap_or_else(|_| unreachable!()))
}

/// The device couldn't listen on one of its network ports.
#[derive(Debug)]
pub struct StartError {
  pub service: &'static str,
  pub addr: SocketAddrV4,
  pub source: std::io::Error,
}

impl std::fmt::Display for StartError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "can't listen on UDP {} for {}: {}.", self.addr, self.service, self.source)?;
    match self.source.kind() {
      std::io::ErrorKind::AddrInUse => write!(
        f,
        " Is another Inferno device (or Dante software) using it? Each device on a host needs its own ports: \
         give them ALT_PORT values at least 10 apart, or leave ALT_PORT unset to have free ports picked."
      ),
      std::io::ErrorKind::AddrNotAvailable => {
        write!(f, " Is {} an address of this host (BIND_IP), and is its interface up?", self.addr.ip())
      }
      _ => Ok(()),
    }
  }
}

impl std::error::Error for StartError {
  fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
    Some(&self.source)
  }
}

pub struct DeviceServer {
  pub self_info: Arc<DeviceInfo>,
  ref_instant: Instant,
  state_storage: Arc<StateStorage>,
  clock_source: Arc<dyn ClockSource>,
  /// whether the device created `clock_source` (and stops it on shutdown)
  owns_clock_source: bool,
  shared_media_clock: Arc<RwLock<MediaClock>>,
  mdns_client: Arc<MdnsClient>,
  mdns_server: Arc<DeviceMDNSResponder>,
  mcast_tx: mpsc::Sender<crate::protocol::mcast::MulticastMessage>,
  tx_latency_ns: u32,
  tx_source_bit_depth: u8,
  channels_sub_tx: watch::Sender<Option<Arc<ChannelsSubscriber>>>,
  channels_subscriber: Option<Arc<ChannelsSubscriber>>,
  flows_tx: Arc<Mutex<Option<FlowsTransmitter>>>,
  tx_multicasts: Arc<Mutex<Option<TransmitMulticasts>>>,
  tx_multicasts_by_channel: Arc<RwLock<BTreeMap<usize, PointerToMulticast>>>,
  rx_peaks_supplier: Arc<RwLock<Box<dyn Fn() -> Vec<u8> + Send + Sync>>>,
  tx_peaks_supplier: Arc<RwLock<Box<dyn Fn() -> Vec<u8> + Send + Sync>>>,
  shutdown_todo: Pin<Box<dyn Future<Output = ()> + Send>>,
  tx_shutdown_todo: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
  rx_shutdown_todo: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
  /// receiver into external buffers, if running: its flows receiver and buffers, to switch them
  external_rx: Option<ExternalReceiver>,
  /// the audio threads' sleeps and timeouts are short
  _timer_resolution: crate::util::os::TimerResolution,
}

struct ExternalReceiver {
  flows_rx: Arc<flows_rx::FlowsReceiver<ExternalBuffer<Atomic<Sample>>>>,
  ring_buffers: Arc<RwLock<channels_subscriber::ExternalRingBuffers>>,
  hole_fix_wait: usize,
}

impl DeviceServer {
  /// Like `try_start`, but panics if the device can't start.
  pub async fn start(settings: settings::Settings) -> Self {
    Self::try_start(settings).await.unwrap_or_else(|e| panic!("{e}"))
  }

  /// Starts the device with the clock source of its settings (`CLOCK_SOURCE`, see `clock::ClockSourceKind`).
  /// Fails if it can't listen on its network ports (e.g. used by another device).
  /// If the standard ports are in use (by another device on this host), and the settings let Inferno choose
  /// (see `Settings::auto_identity`), it picks free ports, and a process ID and name to be a separate device.
  pub async fn try_start(settings: settings::Settings) -> Result<Self, StartError> {
    Self::try_start_inner(settings, None).await
  }

  /// Like `try_start`, but following the media clock from `clock_source` (which `settings.clock_path`
  /// doesn't affect then). The source isn't stopped on shutdown: it may be shared by several devices.
  pub async fn try_start_with_clock(
    settings: settings::Settings,
    clock_source: Arc<dyn ClockSource>,
  ) -> Result<Self, StartError> {
    Self::try_start_inner(settings, Some(clock_source)).await
  }

  async fn try_start_inner(
    settings: settings::Settings,
    clock_source: Option<Arc<dyn ClockSource>>,
  ) -> Result<Self, StartError> {
    let mut info = settings.self_info;
    let ref_instant = Instant::now();

    // before anything else is started, so that nothing needs to be undone on failure
    let (shutdown_send, _) = broadcast_queue::channel(16);
    let [arc_socket, cmc_socket, flows_control_socket, info_socket] =
      match listen_on_device_ports(&info, &shutdown_send).await {
        Ok(sockets) => sockets,
        Err(e) if e.source.kind() == std::io::ErrorKind::AddrInUse && settings.auto_identity.ports => {
          let standard = (info.arc_port, info.process_id);
          debug!("standard ports in use: {e}");
          let first = settings::first_auto_block(&info.friendly_hostname);
          let mut found = None;
          for i in 0..settings::AUTO_PORTS_BLOCKS {
            let mut candidate = info.clone();
            settings.auto_identity.apply(&mut candidate, (first + i) % settings::AUTO_PORTS_BLOCKS);
            if let Ok(sockets) = listen_on_device_ports(&candidate, &shutdown_send).await {
              found = Some((candidate, sockets));
              break;
            }
          }
          let Some((candidate, sockets)) = found else {
            return Err(e);
          };
          info = candidate;
          info!(
          "UDP {} is in use (another device on this host?): this device uses ports {}-{}, PROCESS_ID {} \
             (instead of {}) and name {:?}. Set ALT_PORT (and PROCESS_ID, NAME) to choose them.",
          e.addr,
          info.arc_port,
          info.info_request_port,
          info.process_id,
          standard.1,
          info.friendly_hostname
        );
          sockets
        }
        Err(e) => return Err(e),
      };
    let self_info = Arc::new(info);

    let state_storage = Arc::new(StateStorage::new(&self_info));
    let tx_multicasts_by_channel: Arc<RwLock<BTreeMap<usize, PointerToMulticast>>> = Default::default();
    let mdns_handle = Arc::new(mdns_server::DeviceMDNSResponder::start(
      self_info.clone(),
      tx_multicasts_by_channel.clone(),
    ));

    let mdns_client = Arc::new(crate::mdns_client::MdnsClient::new(self_info.clone()));
    let (mcast_tx, mcast_rx) = mpsc::channel(100);

    let owns_clock_source = clock_source.is_none();
    let clock_source = match clock_source {
      Some(source) => source,
      None => start_clock_source(
        settings.clock_source,
        settings.clock_path.clone(),
        settings.clock_leader,
        self_info.ip_address,
        self_info.mac_address.octets(),
      )
      .map_err(|source| StartError {
        service: "PTP clock",
        addr: SocketAddrV4::new(self_info.ip_address, crate::clock::ptpv1::EVENT_PORT),
        source,
      })?,
    };
    info!("clock: {}", clock_source.description());
    let shared_media_clock = make_shared_media_clock(&clock_source, settings.use_safe_clock).await;

    let mut tasks = vec![];

    let (channels_sub_tx, channels_sub_rx) = watch::channel(None);
    //let tx_peaks = Arc::new((0..self_info.tx_channels.len()).map(|_|AtomicSample::new(0)).collect_vec());
    //let rx_peaks = Arc::new((0..self_info.rx_channels.len()).map(|_|AtomicSample::new(0)).collect_vec());

    let rx_peaks_supplier: Arc<RwLock<Box<dyn Fn() -> Vec<u8> + Send + Sync>>> =
      Arc::new(RwLock::new(Box::new(|| -> Vec<u8> { vec![] })));
    let tx_peaks_supplier: Arc<RwLock<Box<dyn Fn() -> Vec<u8> + Send + Sync>>> =
      Arc::new(RwLock::new(Box::new(|| -> Vec<u8> { vec![] })));
    let txps = tx_peaks_supplier.clone();
    let rxps = rx_peaks_supplier.clone();
    let flows_tx: Arc<Mutex<Option<FlowsTransmitter>>> = Default::default();
    let tx_multicasts: Arc<Mutex<Option<TransmitMulticasts>>> = Default::default();
    tasks.append(&mut vec![
      tokio::spawn(arc_server::run_server(
        self_info.clone(),
        state_storage.clone(),
        mdns_handle.clone(),
        mcast_tx.clone(),
        channels_sub_rx.clone(),
        flows_tx.clone(),
        tx_multicasts.clone(),
        arc_socket,
      )),
      tokio::spawn(cmc_server::run_server(self_info.clone(), cmc_socket)),
      tokio::spawn(flows_control_server::run_server(
        self_info.clone(),
        flows_tx.clone(),
        flows_control_socket,
      )),
      tokio::spawn(info_mcast_server::run_server(
        self_info.clone(),
        mcast_rx,
        shared_media_clock.clone(),
        channels_sub_rx,
        Box::new(move || ((rxps.read().unwrap())(), (txps.read().unwrap())())),
        info_socket,
      )),
    ]);

    info!("all common tasks spawned");

    let mdns_server = mdns_handle.clone();
    let shutdown_todo = async move {
      shutdown_send.send(()).unwrap();
      for task in tasks {
        task.await.unwrap();
      }
      mdns_handle.shutdown_and_join();
    }
    .boxed();

    Ok(Self {
      self_info,
      ref_instant,
      state_storage,
      clock_source,
      owns_clock_source,
      shared_media_clock,
      mdns_client,
      mdns_server,
      mcast_tx,
      tx_latency_ns: settings.tx_latency_ns,
      tx_source_bit_depth: settings.tx_source_bit_depth,
      channels_sub_tx,
      channels_subscriber: None,
      flows_tx,
      tx_multicasts,
      tx_multicasts_by_channel,
      rx_peaks_supplier,
      tx_peaks_supplier,
      //tasks,
      //tx_inputs,
      shutdown_todo,
      rx_shutdown_todo: None,
      external_rx: None,
      tx_shutdown_todo: None,
      _timer_resolution: crate::util::os::TimerResolution::fine(),
    })
  }

  /// Receives into `samples_callback`, as far as all the subscribed channels have received: samples come only
  /// while a flow is alive.
  pub async fn receive_with_callback(&mut self, samples_callback: SamplesCallback) {
    self.receive_with_callback_impl(samples_callback, None).await
  }
  /// Like [`receive_with_callback`](Self::receive_with_callback), but time goes on along the media clock: silence
  /// while nothing is received (nothing subscribed, a transmitter gone), after waiting 200 ms for late audio.
  pub async fn receive_with_callback_clocked(&mut self, samples_callback: SamplesCallback) {
    let clock = self.get_realtime_clock_receiver();
    self.receive_with_callback_impl(samples_callback, Some(clock)).await
  }
  async fn receive_with_callback_impl(
    &mut self,
    samples_callback: SamplesCallback,
    clock_recv: Option<RealTimeClockReceiver>,
  ) {
    let (col, col_fut) = SamplesCollector::<OwnedBuffer<Atomic<Sample>>>::new_with_callback(
      self.self_info.clone(),
      Box::new(samples_callback),
      clock_recv,
    );
    let tasks = vec![tokio::spawn(col_fut)];
    let buffering = OwnedBuffering::new(524288 /*TODO*/, 4800 /*TODO*/, Arc::new(col));
    self.receive(tasks, StreamAnchor::new_started(0), buffering, Default::default(), None).await;
  }
  pub async fn receive_realtime(&mut self) -> RealTimeSamplesReceiver<OwnedBuffer<Atomic<Sample>>> {
    let (col, col_fut, rt_recv) =
      SamplesCollector::new_realtime(self.self_info.clone(), self.get_realtime_clock_receiver());
    let tasks = vec![tokio::spawn(col_fut)];
    let buffering = OwnedBuffering::new(524288 /*TODO*/, 4800 /*TODO*/, Arc::new(col));
    self.receive(tasks, StreamAnchor::new_started(0), buffering, Default::default(), None).await;

    rt_recv
  }
  /// Receives into buffers owned by the caller (e.g. ALSA mmap areas), addressed relative to `anchor`.
  /// The stream may be paused and re-anchored any number of times without restarting the receiver.
  /// If already receiving into external buffers, switches to the new ones, keeping flows and subscriptions.
  pub async fn receive_to_external_buffer(
    &mut self,
    rx_channels_buffers: Vec<ExternalBufferParameters<Sample>>,
    anchor: StreamAnchor,
    current_timestamp: Arc<AtomicUsize>,
    on_transfer: Option<TransferNotifier>,
  ) {
    if let Some(ext) = &self.external_rx {
      let new_rbs =
        rx_channels_buffers.iter().map(|par| ring_buffer::shared_from_external(par, 0)).collect_vec();
      let replacements = {
        let mut rbs = ext.ring_buffers.write().unwrap();
        let old = std::mem::replace(&mut *rbs, new_rbs.clone());
        old.into_iter().zip(new_rbs.iter().cloned()).collect_vec()
      };
      *self.rx_peaks_supplier.write().unwrap() = Box::new(move || peaks_of_buffers(&new_rbs));
      ext
        .flows_rx
        .switch_buffers(replacements, ext.hole_fix_wait, anchor, current_timestamp, on_transfer)
        .await;
      return;
    }
    let buffering = ExternalBuffering::new(rx_channels_buffers, 4800 /*TODO*/);
    let ring_buffers = buffering.ring_buffers.clone();
    let hole_fix_wait = buffering.hole_fix_wait;
    let rbs = ring_buffers.clone();
    *self.rx_peaks_supplier.write().unwrap() = Box::new(move || peaks_of_buffers(&rbs.read().unwrap()));
    let flows_rx = self.receive(vec![], anchor, buffering, current_timestamp, on_transfer).await;
    self.external_rx = Some(ExternalReceiver { flows_rx, ring_buffers, hole_fix_wait });
  }
  async fn receive<
    P: ProxyToSamplesBuffer + Send + Sync + 'static,
    B: ChannelsBuffering<P> + Send + Sync + 'static,
  >(
    &mut self,
    mut tasks: Vec<JoinHandle<()>>,
    anchor: StreamAnchor,
    channels_buffering: B,
    current_timestamp: Arc<AtomicUsize>,
    on_transfer: Option<TransferNotifier>,
  ) -> Arc<flows_rx::FlowsReceiver<P>> {
    let (flows_rx_handle, flows_rx_thread) = flows_rx::FlowsReceiver::start(
      self.self_info.clone(),
      self.get_realtime_clock_receiver(),
      self.ref_instant,
      anchor,
      current_timestamp,
      on_transfer,
    );
    let flows_rx_handle = Arc::new(flows_rx_handle);
    let flows_rx_result = flows_rx_handle.clone();
    let (channels_sub_handle, channels_sub_worker) = ChannelsSubscriber::new(
      self.self_info.clone(),
      self.shared_media_clock.clone(),
      flows_rx_handle.clone(),
      self.mdns_client.clone(),
      self.mcast_tx.clone(),
      channels_buffering,
      self.state_storage.clone(),
      self.ref_instant,
    );
    let channels_sub_handle = Arc::new(channels_sub_handle);
    let _ = self.channels_sub_tx.send(Some(channels_sub_handle.clone()));
    self.channels_subscriber = Some(channels_sub_handle.clone());

    tasks.push(tokio::spawn(channels_sub_worker));

    let shutdown_todo = async move {
      flows_rx_handle.shutdown().await;
      channels_sub_handle.shutdown().await;
      flows_rx_thread.join().unwrap();
      for task in tasks {
        task.await.unwrap();
      }
      info!("receiver stopped");
    }
    .boxed();
    self.rx_shutdown_todo = Some(shutdown_todo);
    flows_rx_result
  }
  pub async fn stop_receiver(&mut self) {
    self.external_rx = None;
    let _ = self.channels_sub_tx.send(None);
    self.rx_shutdown_todo.take().unwrap().await;
  }

  /// Transmits from buffers owned by the caller (e.g. ALSA mmap areas), addressed relative to `anchor`.
  /// The stream may be paused (silence is sent) and re-anchored any number of times
  /// without restarting the transmitter.
  /// Transmits from the given buffers. If already transmitting, switches to them without interrupting flows.
  pub async fn transmit_from_external_buffer(
    &mut self,
    tx_channels_buffers: Vec<ExternalBufferParameters<Sample>>,
    anchor: StreamAnchor,
    current_timestamp: Arc<AtomicUsize>,
    on_transfer: Option<TransferNotifier>,
  ) {
    let rb_outputs: Vec<_> =
      tx_channels_buffers.iter().map(|par| ring_buffer::wrap_external_source(par, 0)).collect();
    let rbs = rb_outputs.iter().map(|rbo| rbo.shared().clone()).collect_vec();
    *self.tx_peaks_supplier.write().unwrap() = Box::new(move || peaks_of_buffers(&rbs));
    if self.tx_shutdown_todo.is_some() {
      // already transmitting: switch sources, keeping the flows (and receivers' subscriptions)
      if let Some(flows_tx) = self.flows_tx.lock().await.as_ref() {
        flows_tx.set_sources(rb_outputs, anchor, current_timestamp, on_transfer).await;
      }
    } else {
      self.transmit(anchor, rb_outputs, current_timestamp, on_transfer).await;
    }
  }
  async fn transmit<P: ProxyToSamplesBuffer + Send + Sync + 'static>(
    &mut self,
    anchor: StreamAnchor,
    rb_outputs: Vec<RBOutput<Sample, P>>,
    current_timestamp: Arc<AtomicUsize>,
    on_transfer: Option<TransferNotifier>,
  ) {
    let clock_rx = self.clock_source.subscribe();

    let (flows_tx_handle, flows_tx_thread) = flows_tx::FlowsTransmitter::start(
      self.self_info.clone(),
      self.tx_latency_ns.try_into().unwrap(),
      self.tx_source_bit_depth,
      self.get_realtime_clock_receiver(),
      rb_outputs.clone(),
      anchor,
      current_timestamp.clone(),
      on_transfer,
    );
    *self.flows_tx.lock().await = Some(flows_tx_handle);
    let txm = TransmitMulticasts::new(
      self.tx_multicasts_by_channel.clone(),
      self.state_storage.clone(),
      self.self_info.clone(),
      self.flows_tx.clone(),
      self.mdns_server.clone(),
      self.mdns_client.clone(),
    );
    txm.load_state().await;
    *self.tx_multicasts.lock().await = Some(txm);
    for (index, _) in self.self_info.tx_channels.iter().enumerate() {
      self.mdns_server.remove_tx_channel(index);
      self.mdns_server.add_tx_channel(index);
    }
    //let peaks_work = Arc::new(AtomicBool::new(true));
    //let peaks_work1 = peaks_work.clone();
    //let peaks = self.tx_peaks.clone();
    //let media_clock = self.shared_media_clock.clone();
    //let sample_rate = self.self_info.sample_rate;
    // TODO
    /* let peaks_thread = std::thread::Builder::new().name("peaks of TX".to_owned()).spawn(move || {
      let mut buf = vec![0 as Sample; PEAKS_BUFFER_LEN];
      while peaks_work.load(Ordering::Relaxed) {
        sleep(PEAKS_ITER_SLEEP);
        let read_from = 0; // TODO doesn't work because unconditional_read()==true in ALSA plugin
        for (chi, rbout) in rb_outputs.iter().enumerate() {
          let readable_until = rbout.readable_until();
          let to_read = readable_until.wrapping_sub(read_from).min(PEAKS_BUFFER_LEN);
          let start_timestamp = readable_until.wrapping_sub(to_read);
          rbout.read_at(start_timestamp, &mut buf);
          let samples = &buf[0..to_read];
          let peak = samples.iter().map(|n|n.saturating_abs()).max().unwrap_or(0);
          peaks[chi].fetch_max(peak, Ordering::Relaxed);
        }
      }
    }).unwrap(); */
    let flows_tx = self.flows_tx.clone();
    let tx_multicasts = self.tx_multicasts.clone();
    self.tx_shutdown_todo = Some(
      async move {
        //peaks_work1.store(false, Ordering::Relaxed);
        if let Some(txm) = tx_multicasts.lock().await.take() {
          txm.shutdown().await;
        }
        if let Some(flows_tx) = flows_tx.lock().await.take() {
          flows_tx.shutdown().await;
        }
        flows_tx_thread.join().unwrap();
        //peaks_thread.join().unwrap();
        info!("transmitter stopped");
      }
      .boxed(),
    );
  }
  pub async fn stop_transmitter(&mut self) {
    self.tx_shutdown_todo.take().unwrap().await;
  }

  pub async fn subscribe(&self, local_channel_index: usize, tx_channel_name: &str, tx_hostname: &str) {
    if let Some(sub) = &self.channels_subscriber {
      sub.subscribe(local_channel_index, tx_channel_name, tx_hostname).await;
    } else {
      warn!("cannot subscribe: receiver not started");
    }
  }

  pub async fn unsubscribe(&self, local_channel_index: usize) {
    if let Some(sub) = &self.channels_subscriber {
      sub.unsubscribe(local_channel_index).await;
    } else {
      warn!("cannot unsubscribe: receiver not started");
    }
  }

  /// The source of this device's media clock.
  pub fn clock_source(&self) -> Arc<dyn ClockSource> {
    self.clock_source.clone()
  }

  pub fn get_realtime_clock_receiver(&self) -> RealTimeClockReceiver {
    async_clock_receiver_to_realtime(
      self.clock_source.subscribe(),
      self.shared_media_clock.read().unwrap().get_overlay().clone(),
    )
  }

  pub async fn shutdown(self) {
    info!("shutting down");
    if let Some(todo) = self.rx_shutdown_todo {
      todo.await;
    }
    if let Some(todo) = self.tx_shutdown_todo {
      todo.await;
    }
    self.shutdown_todo.await;
    if self.owns_clock_source {
      self.clock_source.shutdown().await;
    }
    info!("shutdown ok");
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicUsize, Ordering};

  #[test]
  fn transfer_notifications_once_per_quarter_interval() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let notifier = TransferNotifier::new(Box::new(move || _ = c.fetch_add(1, Ordering::Relaxed)), 1024);
    // a packet (32 samples) at a time over 4 intervals
    for now in (10_000..10_000 + 4 * 1024).step_by(32) {
      notifier.notify_at(Some(now));
    }
    assert_eq!(calls.load(Ordering::Relaxed), 16);
    notifier.notify_at(None);
    assert_eq!(calls.load(Ordering::Relaxed), 17, "without the time: always");
  }
}
