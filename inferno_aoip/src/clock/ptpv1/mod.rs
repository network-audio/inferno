//! PTPv1 (IEEE 1588-2002) follower: the clock of Dante networks, followed without a PTP daemon.
//!
//! Listens for the leader's Sync and Follow_Up messages, measures the path delay with Delay_Req, and
//! publishes the leader's time as a [`ClockOverlay`] over the local monotonic clock. Timestamps are taken in
//! software, as packets are received and sent, so precision depends on scheduling (tens of microseconds),
//! well within the network latency Dante receivers buffer.
//!
//! It never acts as a leader: a network needs a Dante device (or other PTPv1 leader) for clock.

use std::{
  net::{Ipv4Addr, SocketAddrV4, UdpSocket},
  sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc, Mutex,
  },
  time::{Duration, Instant},
};

use futures::future::BoxFuture;
use tokio::sync::watch;

use super::{ClockOverlay, ClockSource, MONOTONIC_CLOCK_ID};
use crate::net_utils::multicast_receiver;

pub mod messages;
mod servo;
#[doc(hidden)]
pub mod test_leader;

use messages::{Body, Grandmaster, Message, PortId};
use servo::Servo;

pub const PTP_MULTICAST: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);
pub const EVENT_PORT: u16 = 319;
pub const GENERAL_PORT: u16 = 320;

const DELAY_REQ_INTERVAL: Duration = Duration::from_secs(1);
/// A leader silent this long is replaced by another one heard.
const LEADER_TIMEOUT: Duration = Duration::from_secs(5);
const RECV_TIMEOUT: Duration = Duration::from_millis(200);

/// Where the follower listens, and who it is.
#[derive(Debug, Clone)]
pub struct Ptpv1Config {
  /// address of the network interface to follow the clock on
  pub interface: Ipv4Addr,
  /// this clock's identity in Delay_Req messages, usually the interface's MAC address
  pub uuid: [u8; 6],
  pub group: Ipv4Addr,
  pub event_port: u16,
  pub general_port: u16,
}

impl Ptpv1Config {
  pub fn new(interface: Ipv4Addr, uuid: [u8; 6]) -> Self {
    Self { interface, uuid, group: PTP_MULTICAST, event_port: EVENT_PORT, general_port: GENERAL_PORT }
  }
}

/// Local monotonic clock, the same the overlays are over.
fn local_now() -> i64 {
  ClockOverlay::monotonic().now_underlying_ns()
}

/// A message, and the local time it was received at.
struct Received {
  message: Message,
  local: i64,
}

#[derive(Default)]
struct Stats {
  messages: AtomicU64,
  syncs_from_leader: AtomicU64,
}

/// Follows the PTPv1 leader of the network on an interface.
pub struct Ptpv1ClockSource {
  config: Ptpv1Config,
  receiver: watch::Receiver<Option<ClockOverlay>>,
  stop: Arc<AtomicBool>,
  stats: Arc<Stats>,
  leader: Arc<Mutex<Option<Grandmaster>>>,
}

fn receive_thread(
  socket: UdpSocket,
  tx: mpsc::Sender<Received>,
  stop: Arc<AtomicBool>,
  stats: Arc<Stats>,
) {
  let _ = crate::utils::set_current_thread_realtime(70);
  let mut buf = [0u8; 512];
  while !stop.load(Ordering::Relaxed) {
    match socket.recv(&mut buf) {
      Ok(len) => {
        let local = local_now();
        if let Some(message) = messages::parse(&buf[..len]) {
          stats.messages.fetch_add(1, Ordering::Relaxed);
          if tx.send(Received { message, local }).is_err() {
            break;
          }
        }
      }
      Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
      Err(e) => {
        log::warn!("PTP receive error: {e}");
        std::thread::sleep(RECV_TIMEOUT);
      }
    }
  }
}

fn blocking_receiver(group: Ipv4Addr, port: u16, interface: Ipv4Addr) -> std::io::Result<UdpSocket> {
  let socket = multicast_receiver(SocketAddrV4::new(group, port), interface)?;
  socket.set_nonblocking(false)?;
  socket.set_read_timeout(Some(RECV_TIMEOUT))?;
  Ok(socket)
}

struct Follower {
  config: Ptpv1Config,
  me: PortId,
  send_socket: UdpSocket,
  sender: watch::Sender<Option<ClockOverlay>>,
  stats: Arc<Stats>,
  leader_shared: Arc<Mutex<Option<Grandmaster>>>,
  leader: Option<(PortId, Grandmaster)>,
  leader_heard: Instant,
  servo: Servo,
  /// Sync waiting for its Follow_Up: (sequence ID, local receive time)
  pending_sync: Option<(u16, i64)>,
  delay_req_sequence: u16,
  /// Delay_Req waiting for its response: (sequence ID, local send time)
  pending_delay_req: Option<(u16, i64)>,
  next_delay_req: Instant,
}

impl Follower {
  fn set_leader(&mut self, source: PortId, grandmaster: Grandmaster) {
    log::info!(
      "following PTP leader {} (port {})",
      grandmaster.uuid.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":"),
      source.port
    );
    self.leader = Some((source, grandmaster));
    *self.leader_shared.lock().unwrap() = Some(grandmaster);
    self.servo.reset();
    self.pending_sync = None;
    self.pending_delay_req = None;
  }

  fn handle(&mut self, received: Received) {
    let Received { message: Message { header, body }, local } = received;
    if header.source == self.me {
      return; // own Delay_Req, looped back
    }
    match body {
      Body::Sync { origin, grandmaster } => {
        let switch = match self.leader {
          None => true,
          Some((source, _)) if source == header.source => false,
          Some((_, current)) => {
            grandmaster.rank() < current.rank() || self.leader_heard.elapsed() > LEADER_TIMEOUT
          }
        };
        if switch {
          self.set_leader(header.source, grandmaster);
        }
        if self.leader.map(|(s, _)| s) != Some(header.source) {
          return;
        }
        self.leader_heard = Instant::now();
        self.stats.syncs_from_leader.fetch_add(1, Ordering::Relaxed);
        if header.assist {
          self.pending_sync = Some((header.sequence_id, local));
        } else {
          self.servo.add_sync(local, origin);
          self.publish();
        }
      }
      Body::FollowUp { associated_sequence_id, precise_origin } => {
        if self.leader.map(|(s, _)| s) != Some(header.source) {
          return;
        }
        if let Some((sequence_id, local)) = self.pending_sync {
          if sequence_id == associated_sequence_id {
            self.pending_sync = None;
            self.servo.add_sync(local, precise_origin);
            self.publish();
          }
        }
      }
      Body::DelayResp { receipt, requester, requester_sequence_id } => {
        if requester != self.me || self.leader.map(|(s, _)| s) != Some(header.source) {
          return;
        }
        if let Some((sequence_id, sent)) = self.pending_delay_req {
          if sequence_id == requester_sequence_id {
            self.pending_delay_req = None;
            self.servo.add_delay(sent, receipt);
          }
        }
      }
      Body::DelayReq => {}
    }
  }

  fn publish(&self) {
    if let Some(overlay) = self.servo.overlay() {
      let previous = self.sender.send_replace(Some(overlay));
      if let Some(previous) = previous {
        let now = local_now();
        let jump = overlay.underlying_to_overlay_ns(now) - previous.underlying_to_overlay_ns(now);
        if jump.abs() > 1_000_000 {
          log::warn!("PTP clock jumped by {jump} ns");
        }
      }
    }
  }

  fn send_delay_req(&mut self) {
    let Some((_, grandmaster)) = self.leader else {
      return;
    };
    if self.servo.fit().is_none() {
      return;
    }
    self.delay_req_sequence = self.delay_req_sequence.wrapping_add(1);
    let packet = messages::delay_req(self.me, self.delay_req_sequence, &grandmaster);
    let destination = SocketAddrV4::new(self.config.group, self.config.event_port);
    let sent = local_now();
    match self.send_socket.send_to(&packet, destination) {
      Ok(_) => self.pending_delay_req = Some((self.delay_req_sequence, sent)),
      Err(e) => log::warn!("can't send PTP Delay_Req: {e}"),
    }
  }

  fn run(mut self, rx: mpsc::Receiver<Received>, stop: Arc<AtomicBool>) {
    let _ = crate::utils::set_current_thread_realtime(70);
    while !stop.load(Ordering::Relaxed) {
      let timeout = self.next_delay_req.saturating_duration_since(Instant::now()).min(RECV_TIMEOUT);
      match rx.recv_timeout(timeout) {
        Ok(received) => self.handle(received),
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        Err(mpsc::RecvTimeoutError::Disconnected) => break,
      }
      if Instant::now() >= self.next_delay_req {
        self.send_delay_req();
        self.next_delay_req = Instant::now() + DELAY_REQ_INTERVAL;
      }
    }
  }
}

impl Ptpv1ClockSource {
  /// Starts following the clock on `config.interface`.
  pub fn start(config: Ptpv1Config) -> std::io::Result<Self> {
    let event = blocking_receiver(config.group, config.event_port, config.interface)?;
    let general = blocking_receiver(config.group, config.general_port, config.interface)?;
    let send_socket = {
      use socket2::{Domain, Socket, Type};
      let socket = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
      socket.bind(&SocketAddrV4::new(config.interface, 0).into())?;
      socket.set_multicast_if_v4(&config.interface)?;
      // other followers and leaders on this host hear it too
      socket.set_multicast_loop_v4(true)?;
      UdpSocket::from(socket)
    };
    // distinguishes followers on one host, which share the interface's MAC address
    let me = PortId { uuid: config.uuid, port: 0x8000 | (rand::random::<u16>() & 0x7fff) };

    let (sender, receiver) = watch::channel(None);
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Stats::default());
    let leader = Arc::new(Mutex::new(None));
    let (tx, rx) = mpsc::channel();
    // the threads end within RECV_TIMEOUT once stopped
    for (name, socket) in [("PTP event", event), ("PTP general", general)] {
      let (tx, stop, stats) = (tx.clone(), stop.clone(), stats.clone());
      std::thread::Builder::new()
        .name(name.into())
        .spawn(move || receive_thread(socket, tx, stop, stats))?;
    }
    drop(tx);
    let follower = Follower {
      config: config.clone(),
      me,
      send_socket,
      sender,
      stats: stats.clone(),
      leader_shared: leader.clone(),
      leader: None,
      leader_heard: Instant::now(),
      servo: Servo::new(),
      pending_sync: None,
      delay_req_sequence: 0,
      pending_delay_req: None,
      next_delay_req: Instant::now() + DELAY_REQ_INTERVAL,
    };
    let follower_stop = stop.clone();
    std::thread::Builder::new()
      .name("PTP follower".into())
      .spawn(move || follower.run(rx, follower_stop))?;
    debug_assert_eq!(MONOTONIC_CLOCK_ID, ClockOverlay::monotonic().clock_id);
    Ok(Self { config, receiver, stop, stats, leader })
  }

  fn stop(&self) {
    self.stop.store(true, Ordering::Relaxed);
  }
}

impl Drop for Ptpv1ClockSource {
  fn drop(&mut self) {
    self.stop();
  }
}

impl ClockSource for Ptpv1ClockSource {
  fn subscribe(&self) -> watch::Receiver<Option<ClockOverlay>> {
    self.receiver.clone()
  }
  fn description(&self) -> String {
    format!(
      "PTPv1 on {} ({}, UDP {}/{})",
      self.config.interface, self.config.group, self.config.event_port, self.config.general_port
    )
  }
  fn diagnose_missing(&self) -> String {
    let messages = self.stats.messages.load(Ordering::Relaxed);
    let syncs = self.stats.syncs_from_leader.load(Ordering::Relaxed);
    if messages == 0 {
      format!(
        "no PTPv1 messages received on {} ({}, UDP {}/{}): is a Dante device (clock leader) on this \
         network, is BIND_IP on it, and does the firewall let PTP in?",
        self.config.interface, self.config.group, self.config.event_port, self.config.general_port
      )
    } else if self.leader.lock().unwrap().is_none() {
      format!("{messages} PTPv1 messages received, but no clock leader sending Sync")
    } else {
      format!("following a PTPv1 leader, {syncs} Sync messages so far: not enough to synchronise yet")
    }
  }
  fn shutdown(&self) -> BoxFuture<'static, ()> {
    self.stop();
    Box::pin(async {})
  }
}
