//! A simulated PTPv1 leader, for testing the follower without Dante hardware. Not a usable PTP leader:
//! it doesn't take part in choosing a leader, and its timestamps are taken in software.
//!
//! Its clock is the local monotonic clock with an offset and a frequency error, so tests can check that a
//! follower locks to both. It sends Sync (with the assist flag) and Follow_Up, and answers Delay_Req.

use std::{
  net::{Ipv4Addr, SocketAddrV4, UdpSocket},
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
  time::Duration,
};

use super::{
  blocking_receiver, local_now,
  messages::{self, Body, Grandmaster, PortId, PtpNs},
  EVENT_PORT, GENERAL_PORT, PTP_MULTICAST,
};

#[derive(Debug, Clone)]
pub struct TestLeaderConfig {
  pub interface: Ipv4Addr,
  pub group: Ipv4Addr,
  pub event_port: u16,
  pub general_port: u16,
  /// the leader's time minus the local monotonic clock, at the start
  pub offset_ns: i64,
  /// how much faster the leader's clock runs than the local monotonic clock
  pub freq_ppm: f64,
  pub sync_interval: Duration,
  pub uuid: [u8; 6],
}

impl TestLeaderConfig {
  pub fn new(interface: Ipv4Addr) -> Self {
    Self {
      interface,
      group: PTP_MULTICAST,
      event_port: EVENT_PORT,
      general_port: GENERAL_PORT,
      // a plausible PTP time (2026), far from the local clock
      offset_ns: 1_780_000_000_000_000_000,
      freq_ppm: 50.0,
      sync_interval: Duration::from_millis(125),
      uuid: [0x02, 0x1d, 0xc1, 0x7e, 0x57, 0x01],
    }
  }
}

/// The leader's clock: the local monotonic clock, shifted and running at a different rate.
#[derive(Debug, Clone, Copy)]
pub struct LeaderClock {
  start_local: i64,
  offset_ns: i64,
  freq: f64,
}

impl LeaderClock {
  pub fn at(&self, local: i64) -> PtpNs {
    let elapsed = local - self.start_local;
    local + self.offset_ns + (elapsed as f64 * self.freq) as i64
  }
  pub fn now(&self) -> PtpNs {
    self.at(local_now())
  }
}

pub struct TestLeader {
  clock: LeaderClock,
  stop: Arc<AtomicBool>,
}

impl TestLeader {
  pub fn start(config: TestLeaderConfig) -> std::io::Result<Self> {
    let clock =
      LeaderClock { start_local: local_now(), offset_ns: config.offset_ns, freq: config.freq_ppm * 1e-6 };
    let me = PortId { uuid: config.uuid, port: 1 };
    let grandmaster = Grandmaster {
      uuid: config.uuid,
      port: 1,
      stratum: 4,
      identifier: *b"DFLT",
      variance: -4000,
      preferred: false,
    };
    let send_socket = {
      use socket2::{Domain, Socket, Type};
      let socket = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
      socket.bind(&SocketAddrV4::new(config.interface, 0).into())?;
      socket.set_multicast_if_v4(&config.interface)?;
      socket.set_multicast_loop_v4(true)?;
      Arc::new(UdpSocket::from(socket))
    };
    let delay_req_socket = blocking_receiver(config.group, config.event_port, config.interface)?;
    let stop = Arc::new(AtomicBool::new(false));
    let event = SocketAddrV4::new(config.group, config.event_port);
    let general = SocketAddrV4::new(config.group, config.general_port);

    let (socket, thread_stop) = (send_socket.clone(), stop.clone());
    std::thread::Builder::new().name("PTP test leader sync".into()).spawn(move || {
      let _ = crate::utils::set_current_thread_realtime(70);
      let mut sequence_id: u16 = 0;
      while !thread_stop.load(Ordering::Relaxed) {
        sequence_id = sequence_id.wrapping_add(1);
        let approximate = clock.now();
        let sent = clock.now();
        let _ =
          socket.send_to(&messages::sync(me, sequence_id, approximate, true, &grandmaster, -3), event);
        let _ = socket.send_to(&messages::follow_up(me, sequence_id, sequence_id, sent), general);
        std::thread::sleep(config.sync_interval);
      }
    })?;

    let thread_stop = stop.clone();
    std::thread::Builder::new().name("PTP test leader delay".into()).spawn(move || {
      let mut buf = [0u8; 512];
      let mut sequence_id: u16 = 0;
      while !thread_stop.load(Ordering::Relaxed) {
        let Ok(len) = delay_req_socket.recv(&mut buf) else {
          continue;
        };
        let received = clock.now();
        let Some(message) = messages::parse(&buf[..len]) else {
          continue;
        };
        if message.body != Body::DelayReq {
          continue;
        }
        sequence_id = sequence_id.wrapping_add(1);
        let response = messages::delay_resp(
          me,
          sequence_id,
          received,
          message.header.source,
          message.header.sequence_id,
        );
        let _ = send_socket.send_to(&response, general);
      }
    })?;
    Ok(Self { clock, stop })
  }

  pub fn clock(&self) -> LeaderClock {
    self.clock
  }
}

impl Drop for TestLeader {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Relaxed);
  }
}
