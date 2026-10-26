use log::{error, info};

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use tokio::{net::UdpSocket, select, sync::broadcast::Receiver};

pub const MTU: usize = 1500;
const PACKET_BUFFER_SIZE: usize = MTU;
pub const MAX_PAYLOAD_BYTES: usize = 1400; // ???

pub struct ReceiveBuffer {
  buff: [u8; PACKET_BUFFER_SIZE],
}

impl ReceiveBuffer {
  pub fn new() -> Self {
    Self { buff: [0u8; PACKET_BUFFER_SIZE] }
  }
}

pub struct UdpSocketWrapper {
  socket: Option<UdpSocket>,
  listen_addr: Ipv4Addr,
  listen_port: u16,
  shutdown: Receiver<()>,
  dowork: bool,
  recv_buff: [u8; PACKET_BUFFER_SIZE],
}

impl UdpSocketWrapper {
  /// Listens on `listen_addr` (default: all addresses) and `listen_port` (0: any free port).
  pub async fn bind(
    listen_addr: Option<Ipv4Addr>,
    listen_port: u16,
    shutdown: Receiver<()>,
  ) -> std::io::Result<UdpSocketWrapper> {
    let listen_addr = listen_addr.unwrap_or(Ipv4Addr::new(0, 0, 0, 0));
    let socket = UdpSocket::bind(SocketAddr::new(std::net::IpAddr::V4(listen_addr), listen_port)).await?;
    let listen_port = socket.local_addr()?.port();
    Ok(UdpSocketWrapper {
      listen_addr,
      socket: Some(socket),
      listen_port,
      shutdown,
      dowork: true,
      recv_buff: [0; PACKET_BUFFER_SIZE],
    })
  }

  pub fn should_work(&self) -> bool {
    self.dowork
  }

  pub fn port(&self) -> u16 {
    self.listen_port
  }

  pub async fn recv<'a>(&mut self, recv_buff: &'a mut ReceiveBuffer) -> Option<(SocketAddr, &'a [u8])> {
    let socket = match &self.socket {
      Some(s) => s,
      None => {
        return None;
      }
    };
    select! {
      r = socket.recv_from(&mut recv_buff.buff) => {
        match r {
          Ok((len_recv, src)) => {
            return Some((src, &recv_buff.buff[..len_recv]));
          },
          Err(e) => {
            error!("error receiving from socket: {e:?}");
            return None;
          }
        }
      },
      _ = self.shutdown.recv() => {
        self.dowork = false;
        return None;
      }
    };
  }

  pub async fn send(&self, dst: &SocketAddr, packet: &[u8]) {
    let socket = match &self.socket {
      Some(s) => s,
      None => {
        info!("shutting down, discarding message to send");
        return;
      }
    };
    if let Err(e) = socket.send_to(packet, dst).await {
      error!("send error (ignoring): {e:?}");
    }
  }
}

pub async fn create_tokio_udp_socket(self_ip: Ipv4Addr) -> tokio::io::Result<(UdpSocket, u16)> {
  let socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(self_ip), 0)).await?;
  let port = socket.local_addr()?.port();
  return Ok((socket, port));
}

pub fn create_mio_udp_socket(self_ip: Ipv4Addr) -> std::io::Result<(mio::net::UdpSocket, u16)> {
  let socket = mio::net::UdpSocket::bind(SocketAddr::new(IpAddr::V4(self_ip), 0))?;
  let port = socket.local_addr()?.port();
  return Ok((socket, port));
}

/// A non-blocking socket receiving multicast group `group` on the interface with address `interface_ip`.
/// Other sockets on this host (other devices, other software) can receive the same group and port too.
pub fn multicast_receiver(
  group: SocketAddrV4,
  interface_ip: Ipv4Addr,
) -> std::io::Result<std::net::UdpSocket> {
  use socket2::{Domain, Socket, Type};
  let socket = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
  // each receiver gets a copy of the group's packets
  socket.set_reuse_address(true)?;
  // bound to the group address: only this group's packets, not other groups' on the same port
  #[cfg(not(windows))]
  socket.bind(&group.into())?;
  // Windows can't bind to a multicast address: it receives other joined groups' packets on this port too
  #[cfg(windows)]
  socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, group.port()).into())?;
  socket.join_multicast_v4(group.ip(), &interface_ip)?;
  socket.set_nonblocking(true)?;
  Ok(socket.into())
}

#[cfg(test)]
mod multicast_tests {
  use super::*;
  use std::time::{Duration, Instant};

  /// Several devices (or other software) on one host receive the same multicast flow.
  #[test]
  fn two_receivers_of_one_multicast_group_on_one_host() {
    let group = SocketAddrV4::new(Ipv4Addr::new(239, 255, 77, 1), 4321);
    let lo = Ipv4Addr::LOCALHOST;
    let a = multicast_receiver(group, lo).expect("first receiver");
    let b = multicast_receiver(group, lo).expect("second receiver of the same group and port");

    let sender = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None).unwrap();
    sender.bind(&SocketAddrV4::new(lo, 0).into()).unwrap();
    sender.set_multicast_if_v4(&lo).unwrap();
    sender.set_multicast_loop_v4(true).unwrap();
    sender.send_to(b"media", &group.into()).unwrap();

    for (name, socket) in [("first", &a), ("second", &b)] {
      let mut buf = [0u8; 16];
      let deadline = Instant::now() + Duration::from_secs(2);
      loop {
        match socket.recv(&mut buf) {
          Ok(n) => {
            assert_eq!(&buf[..n], b"media");
            break;
          }
          Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
            std::thread::sleep(Duration::from_millis(10));
          }
          Err(e) => panic!("{name} receiver got nothing: {e}"),
        }
      }
    }
  }
}
