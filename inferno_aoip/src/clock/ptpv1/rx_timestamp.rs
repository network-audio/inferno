//! Receive timestamps of PTP messages: the kernel's where the platform has them (the time the packet arrived),
//! the time `recv` returned otherwise.

use std::net::UdpSocket;

use super::local_now;

/// A kernel timestamp more than this before the message was read is not used (e.g. the wall clock Linux stamps
/// with was stepped in between): the time `recv` returned is.
const MAX_KERNEL_AGE_NS: i64 = 1_000_000_000;

/// A socket whose received messages are timestamped on the local monotonic clock.
pub(super) struct TimestampingSocket {
  socket: UdpSocket,
  kernel: bool,
}

impl TimestampingSocket {
  /// Asks the kernel to timestamp the messages `socket` receives, where the platform can.
  pub fn new(socket: UdpSocket) -> Self {
    let kernel = kernel::enable(&socket);
    if !kernel {
      log::info!("PTP: no kernel receive timestamps, timestamping in software");
    }
    Self { socket, kernel }
  }

  /// Whether received messages get the kernel's timestamps.
  #[cfg(test)]
  pub fn kernel_timestamps(&self) -> bool {
    self.kernel
  }

  /// Receives a message: its length and the local time it arrived at.
  pub fn recv(&self, buf: &mut [u8]) -> std::io::Result<(usize, i64)> {
    if !self.kernel {
      let len = self.socket.recv(buf)?;
      return Ok((len, local_now()));
    }
    let (len, age) = kernel::recv(&self.socket, buf)?;
    let now = local_now();
    Ok((len, age.filter(|age| (0..MAX_KERNEL_AGE_NS).contains(age)).map_or(now, |age| now - age)))
  }
}

/// The kernel's receive timestamps, converted by their age: how long before now (read together with the
/// timestamp's own clock) the message arrived. So the clock the kernel stamps with needn't be the local one.
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod kernel {
  use std::net::UdpSocket;
  use std::os::fd::AsRawFd;

  #[cfg(target_os = "macos")]
  const OPTION: libc::c_int = libc::SO_TIMESTAMP_MONOTONIC;
  /// `SCM_TIMESTAMP_MONOTONIC` (sys/socket.h): a `uint64_t` of `mach_absolute_time`
  #[cfg(target_os = "macos")]
  const MESSAGE: libc::c_int = 0x04;
  #[cfg(target_os = "linux")]
  const OPTION: libc::c_int = libc::SO_TIMESTAMPNS;
  /// a `timespec` of `CLOCK_REALTIME`
  #[cfg(target_os = "linux")]
  const MESSAGE: libc::c_int = libc::SO_TIMESTAMPNS;

  pub fn enable(socket: &UdpSocket) -> bool {
    let on: libc::c_int = 1;
    // safety: a valid socket and option value
    unsafe {
      libc::setsockopt(
        socket.as_raw_fd(),
        libc::SOL_SOCKET,
        OPTION,
        &on as *const _ as *const libc::c_void,
        std::mem::size_of_val(&on) as libc::socklen_t,
      ) == 0
    }
  }

  /// Receives a message: its length and its age in nanoseconds, if it has a timestamp.
  pub fn recv(socket: &UdpSocket, buf: &mut [u8]) -> std::io::Result<(usize, Option<i64>)> {
    let mut control = [0u64; 16]; // aligned for cmsghdr
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
    // safety: zeroed msghdr is valid; the pointers outlive the call
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    message.msg_controllen = std::mem::size_of_val(&control) as _;
    // safety: as above
    let len = unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, 0) };
    if len < 0 {
      return Err(std::io::Error::last_os_error());
    }
    let mut age = None;
    // safety: the control buffer as the kernel filled it
    unsafe {
      let mut header = libc::CMSG_FIRSTHDR(&message);
      while !header.is_null() {
        if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == MESSAGE {
          age = Some(stamp_age(libc::CMSG_DATA(header)));
        }
        header = libc::CMSG_NXTHDR(&message, header);
      }
    }
    Ok((len as usize, age))
  }

  #[cfg(target_os = "macos")]
  #[allow(deprecated)] // libc's Mach bindings, as in util::os
  unsafe fn stamp_age(data: *const u8) -> i64 {
    let stamp = std::ptr::read_unaligned(data as *const u64);
    let now = libc::mach_absolute_time();
    let mut timebase = libc::mach_timebase_info { numer: 0, denom: 0 };
    libc::mach_timebase_info(&mut timebase);
    let elapsed = now.wrapping_sub(stamp) as i64;
    (elapsed as i128 * timebase.numer.max(1) as i128 / timebase.denom.max(1) as i128) as i64
  }

  #[cfg(target_os = "linux")]
  unsafe fn stamp_age(data: *const u8) -> i64 {
    let stamp = std::ptr::read_unaligned(data as *const libc::timespec);
    let mut now: libc::timespec = std::mem::zeroed();
    libc::clock_gettime(libc::CLOCK_REALTIME, &mut now);
    let ns = |t: libc::timespec| t.tv_sec as i64 * 1_000_000_000 + t.tv_nsec as i64;
    ns(now) - ns(stamp)
  }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod kernel {
  use std::net::UdpSocket;

  pub fn enable(_socket: &UdpSocket) -> bool {
    false
  }

  pub fn recv(socket: &UdpSocket, buf: &mut [u8]) -> std::io::Result<(usize, Option<i64>)> {
    socket.recv(buf).map(|len| (len, None))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::net::{Ipv4Addr, SocketAddrV4};
  use std::time::Duration;

  fn pair() -> (TimestampingSocket, UdpSocket) {
    let receiver = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    receiver.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let sender = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    sender.connect(receiver.local_addr().unwrap()).unwrap();
    (TimestampingSocket::new(receiver), sender)
  }

  /// A message read late keeps the time it arrived at: the receiving thread's scheduling doesn't move it.
  #[cfg(any(target_os = "macos", target_os = "linux"))]
  #[test]
  fn a_message_read_late_has_the_time_it_arrived() {
    let (receiver, sender) = pair();
    assert!(receiver.kernel_timestamps());
    for _ in 0..3 {
      let sent = local_now();
      sender.send(b"sync").unwrap();
      std::thread::sleep(Duration::from_millis(50));
      let mut buf = [0u8; 16];
      let (len, arrived) = receiver.recv(&mut buf).unwrap();
      assert_eq!(&buf[..len], b"sync");
      let after_send = arrived - sent;
      assert!((0..5_000_000).contains(&after_send), "arrived {after_send} ns after it was sent");
    }
  }

  /// Without kernel timestamps (or with them), a message read at once is timestamped about when it was sent.
  #[test]
  fn a_message_read_at_once_has_about_the_time_it_was_sent() {
    let (receiver, sender) = pair();
    let sent = local_now();
    sender.send(b"sync").unwrap();
    let mut buf = [0u8; 16];
    let (len, arrived) = receiver.recv(&mut buf).unwrap();
    assert_eq!(len, 4);
    let after_send = arrived - sent;
    assert!((0..1_000_000_000).contains(&after_send), "arrived {after_send} ns after it was sent");
  }
}
