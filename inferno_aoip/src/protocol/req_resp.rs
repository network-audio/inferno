use crate::common::*;
use binary_layout::prelude::*;
use binary_serde::recursive_array::RecursiveArray as _;
use binary_serde::BinarySerde;
use std::net::SocketAddr;
use std::{borrow::BorrowMut, io::Cursor};

use crate::net_utils::{ReceiveBuffer, UdpSocketWrapper, MTU};

pub const HEADER_LENGTH: usize = 10;
const SEND_BUFFER_SIZE: usize = MTU;
pub const CODE_OK: u16 = 1;

define_layout!(req_resp_packet, BigEndian, {
  start_code: u16,
  total_length: u16,
  seqnum: u16,
  opcode1: u16,
  opcode2: u16,
  content: [u8]
});

#[derive(Clone)]
struct RemoteInfo {
  addr: SocketAddr,
  start_code: u16,
  seqnum: u16,
  opcode1: u16,
}

pub struct Connection {
  server: UdpSocketWrapper,
  send_buff: [u8; SEND_BUFFER_SIZE],
  remote: Option<RemoteInfo>,
}

pub fn make_packet<'a>(
  buf: &'a mut [u8],
  start_code: u16,
  seqnum: u16,
  opcode1: u16,
  opcode2: u16,
  content: &[u8],
) -> &'a [u8] {
  let total_len = content.len() + HEADER_LENGTH;
  assert!(total_len < (1 << 16)); // TODO MAY PANIC
  let buffer = &mut buf[..total_len]; // TODO MAY PANIC check length before slicing
  let mut view = req_resp_packet::View::new(buffer);
  view.start_code_mut().write(start_code);
  view.total_length_mut().write(total_len as u16);
  view.seqnum_mut().write(seqnum);
  view.opcode1_mut().write(opcode1);
  view.opcode2_mut().write(opcode2);
  view.content_mut().copy_from_slice(&content);
  return view.into_storage();
}

impl Connection {
  pub fn new(server: UdpSocketWrapper) -> Connection {
    Connection { server, send_buff: [0; SEND_BUFFER_SIZE], remote: None }
  }

  pub fn should_work(&self) -> bool {
    return self.server.should_work();
  }

  pub async fn recv<'a>(
    &mut self,
    recv_buff: &'a mut ReceiveBuffer,
  ) -> Option<req_resp_packet::View<&'a [u8]>> {
    let (src, request_buf) = match self.server.borrow_mut().recv(recv_buff).await {
      Some(v) => v,
      None => {
        return None;
      }
    };
    if request_buf.len() < HEADER_LENGTH {
      error!("received too short packet: {}", hex::encode(request_buf));
      return None;
    }
    let view = req_resp_packet::View::new(request_buf);
    self.remote = Some(RemoteInfo {
      addr: src,
      start_code: view.start_code().read(),
      seqnum: view.seqnum().read(),
      opcode1: view.opcode1().read(),
    });
    return Some(view);
  }

  pub async fn send(
    &mut self,
    dst: SocketAddr,
    start_code: u16,
    seqnum: u16,
    opcode1: u16,
    opcode2: u16,
    content: &[u8],
  ) {
    if content.len() > self.send_buff.len() - HEADER_LENGTH {
      error!("dropping oversized response to opcode {opcode1:#06x}: {} bytes", content.len());
      return;
    }
    let pkt = make_packet(&mut self.send_buff, start_code, seqnum, opcode1, opcode2, content);
    self.server.send(&dst, pkt).await;
  }
  pub async fn respond(&mut self, payload: &[u8]) {
    self.respond_with_code(1, payload).await;
  }
  pub async fn respond_with_code(&mut self, opcode2: u16, content: &[u8]) {
    let rem = self.remote.as_ref().unwrap();
    self.send(rem.addr, rem.start_code, rem.seqnum, rem.opcode1, opcode2, content).await;
  }
  pub async fn respond_with_struct(&mut self, code: u16, payload: impl BinarySerde) {
    self
      .respond_with_code(
        code,
        payload.binary_serialize_to_array(binary_serde::Endianness::Big).as_slice(),
      )
      .await;
  }
}

#[cfg(test)]
mod response_size_tests {
  use super::*;
  #[tokio::test]
  async fn drops_oversized_response_and_sends_next_valid_packet() {
    let (_shutdown, rx) = tokio::sync::broadcast::channel(1);
    let server = UdpSocketWrapper::bind(Some(std::net::Ipv4Addr::LOCALHOST), 0, rx).await.unwrap();
    let receiver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dst = receiver.local_addr().unwrap();
    let mut conn = Connection::new(server);
    conn.send(dst, 0x27ff, 1, 1, 1, &vec![0; SEND_BUFFER_SIZE - HEADER_LENGTH + 1]).await;
    conn.send(dst, 0x27ff, 2, 1, 1, &vec![0; SEND_BUFFER_SIZE - HEADER_LENGTH]).await;
    let mut buf = [0; SEND_BUFFER_SIZE + 1];
    let len = tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv(&mut buf)).await.unwrap().unwrap();
    assert_eq!(len, SEND_BUFFER_SIZE);
    assert_eq!(req_resp_packet::View::new(&buf[..len]).seqnum().read(), 2);
  }
}
