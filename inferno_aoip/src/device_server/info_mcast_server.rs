use super::channels_subscriber::ChannelsSubscriber;
use crate::byte_utils::*;
use crate::common::*;
use crate::media_clock::MediaClock;
use crate::net_utils::UdpSocketWrapper;
use crate::protocol::mcast::{make_packet, MulticastMessage};
use crate::{byte_utils::write_str_to_buffer, device_info::DeviceInfo};
use bytebuffer::ByteBuffer;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::RwLock;
use std::time::Instant;
use std::{
  net::{IpAddr, Ipv4Addr, SocketAddr},
  sync::Arc,
  time::Duration,
};
use tokio::sync::watch;
use tokio::time::interval;
use tokio::{
  select,
  sync::{broadcast::Receiver as BroadcastReceiver, mpsc},
  time::MissedTickBehavior,
};

const SEND_BUFFER_SIZE: usize = 1500;
const DST_PORT_HEARTBEAT: u16 = 8708;
const DST_PORT_DEVICE_INFO: u16 = 8702;

pub type PeaksCallback = Box<dyn FnMut() -> (Vec<u8>, Vec<u8>) + Send + Sync>;

struct Multicaster<'s> {
  self_info: &'s DeviceInfo,
  pub server: UdpSocketWrapper,
  seqnum: u16,
  vendor: [u8; 8],
  firmware_version_bytes: [u8; 4],
  product_version_bytes: [u8; 4],
  device_info_destination: SocketAddr,
  heartbeat_destination: SocketAddr,
  send_buffer: [u8; SEND_BUFFER_SIZE],
  clock: Arc<RwLock<MediaClock>>,
  channels_subscriber: Option<Arc<ChannelsSubscriber>>,
  get_peaks: PeaksCallback,
  had_clock: bool,
  /// latest sample of the network interface's counters, taken with every heartbeat
  interface_sample: Option<(Instant, InterfaceCounters)>,
  interface_statistics: Option<InterfaceStatistics>,
}

impl<'s> Multicaster<'s> {
  pub fn new(
    self_info: &'s DeviceInfo,
    server: UdpSocketWrapper,
    clock: Arc<RwLock<MediaClock>>,
    get_peaks: PeaksCallback,
  ) -> Multicaster {
    let patch_version = env!("CARGO_PKG_VERSION_PATCH").parse::<u16>().unwrap();
    let mut r = Multicaster {
      self_info,
      server,
      seqnum: 1,
      vendor: [32; 8],
      firmware_version_bytes: [4, 1, 6, 2],
      product_version_bytes: [
        env!("CARGO_PKG_VERSION_MAJOR").parse::<u8>().unwrap(),
        env!("CARGO_PKG_VERSION_MINOR").parse::<u8>().unwrap(),
        H(patch_version),
        L(patch_version),
      ],
      device_info_destination: SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(224, 0, 0, 231)),
        DST_PORT_DEVICE_INFO,
      ),
      heartbeat_destination: SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(224, 0, 0, 233)),
        DST_PORT_HEARTBEAT,
      ),
      send_buffer: [0; SEND_BUFFER_SIZE],
      clock,
      channels_subscriber: None,
      get_peaks,
      had_clock: false,
      interface_sample: None,
      interface_statistics: None,
    };
    write_str_to_buffer(&mut r.vendor, 0, 8, &self_info.vendor_string);
    return r;
  }

  pub fn should_work(&self) -> bool {
    return self.server.should_work();
  }

  async fn send(&mut self, dst: SocketAddr, start_code: u16, opcode: [u8; 8], content: &[u8]) {
    let pkt = make_packet(
      &mut self.send_buffer,
      start_code,
      self.seqnum,
      self.self_info.process_id,
      self.self_info.factory_device_id,
      self.vendor,
      opcode,
      content,
    );
    self.seqnum = self.seqnum.wrapping_add(1);
    self.server.send(&dst, pkt).await;
  }

  async fn send_board_info(&mut self) {
    let mut content = [0u8; 200];
    // Firmware version:
    content[0..4].copy_from_slice(&[4, 1, 0, 6]);
    content[0x23] = 2;
    // Hardware version:
    content[4..8].copy_from_slice(&[4, 1, 0, 3]);
    content[0x27] = 1;
    // Boot version:
    content[0x28..0x2c].copy_from_slice(&[1, 0, 0, 0]);

    // flags of supported features:
    // 0x14: AES67, Device Lock
    //       0x01 - ??? (was 1, od 0xd)
    //       0x04 - supports AES67
    //       0x08 - is lockable
    // 0x15: ??? (was 0x50)
    // 0x16:
    //       0x10 - has Manufacturer name
    //       0x40 - Network is configurable (supports static addressing)
    // 0x17: Identify device, Sample rate & encoding configuration, Reboot, Factory reset (was 0xdb)
    content[0x14] = 0;
    content[0x15] = 0;
    content[0x16] = 0x10;
    content[0x17] = 0;

    content[0xbb] = 0x1f; // if 0, device is flooded with info multicast requests around 1 per second
                          /* content[0xbf] = 5;
                          content[0xc3] = 3;
                          content[0xc7] = 3; */
    write_str_to_buffer(&mut content, 12, 8, &self.self_info.board_name);
    write_str_to_buffer(&mut content, 0x38, 16, &self.self_info.board_name);

    self.send(self.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0x60, 0, 0, 0, 0], &content).await;
  }

  async fn send_product_info(&mut self) {
    let mut content = [0; 336];
    write_str_to_buffer(&mut content, 0, 8, &self.self_info.manufacturer);
    write_str_to_buffer(&mut content, 8, 8, &self.self_info.board_name);
    write_str_to_buffer(&mut content, 0x2c, 16, &self.self_info.manufacturer);
    write_str_to_buffer(&mut content, 0xac, 16, &self.self_info.model_name);
    // product version:
    //content[0x12c..0x130].copy_from_slice(&self.product_version_bytes);

    // firmware version:
    content[0x1c..0x20].copy_from_slice(&self.product_version_bytes);

    // 0x18..0x1b - software version
    // 0x24..0x26 - software patch version, u32
    // 0x28..0x2b - firmware patch version, u32

    self.send(self.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0xc0, 0, 0, 0, 0], &content).await;
  }

  fn get_freq_offset_ppb(&self) -> Option<i32> {
    self
      .clock
      .read()
      .unwrap()
      .get_overlay()
      .as_ref()
      .map(|clkovl| {
        let freq_offset_f = (clkovl.freq_scale_including_hw() * 1_000_000_000f64).round();
        if i32::MIN as f64 <= freq_offset_f && freq_offset_f <= i32::MAX as f64 {
          Some(freq_offset_f as i32)
        } else {
          None
        }
      })
      .flatten()
  }

  async fn send_heartbeat(&mut self) {
    let ctr = self.seqnum;
    let mut bytes = ByteBuffer::new();
    bytes.set_endian(bytebuffer::Endian::BigEndian);

    let freq_offset_opt = self.get_freq_offset_ppb();

    if let Some(freq_offset) = freq_offset_opt {
      bytes.write_u16(16); // length of this part
      bytes.write_u16(0x8001); // type
      bytes.write_u16(4); // ???
      bytes.write_u16(4); // maybe content length???
      bytes.write_u16(ctr);
      bytes.write_u16(0);
      bytes.write_i32(freq_offset);
      trace!("freq offset {freq_offset}/1000 ppm");

      /* bytes.write_bytes(&[
        0x00, 0x24, 0x80, 0x00,
        0x00, 0x04, 0x00, 0x04, H(ctr), L(ctr), 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0x10,
        /* TX Bps, 4B: */ 0x00, 0x00, 0x01, 0xde, /* RX Bps, 4B: */ 0x00, 0x07, 0xdf, 0x17,
        /* TX errors, 4B: */ 0x00, 0x00, 0x00, 0x02, /* RX errors, 4B: */ 0x00, 0x00, 0x00, 0x07,
      ]); // network statistics */

      let (rx_peaks, tx_peaks) = (self.get_peaks)();
      write_peaks_record(&mut bytes, ctr, &tx_peaks, &rx_peaks);

      // rx latency:
      if let Some(chsub) = self.channels_subscriber.as_ref() {
        let flows_info = chsub.flows_info();
        let flows_info = flows_info.read().unwrap();
        let flows_count = flows_info.len() as u16;
        bytes.write_u16(24 + flows_count * 4);
        bytes.write_u16(0x8003);
        bytes.write_u16(4);
        bytes.write_u16(12 + flows_count * 4); // content length
        bytes.write_u16(ctr);
        bytes.write_u16(0);
        bytes.write_u16(flows_count); // number of flows
        bytes.write_u16(0);
        bytes.write_u16(24);
        bytes.write_u16(0);
        bytes.write_u32(self.self_info.sample_rate);

        for opt in flows_info.iter() {
          let latency =
            opt.as_ref().map(|fi| fi.actual_latency_samples.swap(0, Ordering::Relaxed)).unwrap_or(0);
          bytes.write_u32(latency.clamp(0, i32::MAX) as u32);
        }
      }

      /* bytes.write_bytes(&[
        0x00, 0x1c, 0x80, 0x04,
        0x00, 0x04, 0x00, 0x10,  H(ctr), L(ctr), 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
      ]); */
      /*
      0x00, 0x1c, 0x80, 0x04, 0x00, 0x04, 0x00, 0x10, 0x17, 0x0f, 0x00, 0x00,
      0x00, 0x02, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00, missed packets, 4B: 0x00, 0x03, 0x90, 0x1e, 0x00, 0x00, 0x00, 0x00
       */
      if !self.had_clock {
        debug!("clock appeared");
      }
    } else {
      debug!("no clock available");
    }
    self.had_clock = freq_offset_opt.is_some();

    let content = bytes.as_bytes();
    self.send(self.heartbeat_destination, 0xfffe, [0, 8, 0, 1, 0x10, 0, 0, 0], &content).await;

    // this is probably response to 0738008100000064
    /* self.send(
      self.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00],
      &[0x00, 0x18, 0x00, 0x04, 0x00, 0x00, 0xbb, 0x80, 0x00, 0x00, 0xbb, 0x80, 0x00, 0x02, 0x00, 0x00,
      // supported sample rates:
      0x00, 0x00, /* 44100: */ 0xac, 0x44, 0x00, 0x00, 0xbb, 0x80, 0x00, 0x01, 0x58, 0x88, 0x00, 0x01, 0x77, 0x00]
    ).await; */

    /* self.send(
      self.device_info_destination, 0xffff, [0x07, 0x2a, 0x10, 0x07, 0, 0, 0, 0],
      &[0, 0, 0, 0]
    ).await; */

    // this is probably response to 0738008300000064
    /* self.send(
      self.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0x82, 0x00, 0x00, 0x00, 0x00],
    &[
      0x00, 0x18, 0x00, 0x03, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00, 0x18, 0x00, 0x02, 0x00, 0x00,
      0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x20
    ]).await; */

    /* self.send(
    self.device_info_destination, 0xffff, [0x07, 0x2a, 0x10, 0x09, 0x00, 0x00, 0x00, 0x00],
    &[
      0x00, 0x00, 0x00, 0x04, 0x00, 0x02, 0x00, 0x08, 0x00, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
      /* clock source: */0x00, 0x1d, 0xc1, 0xff, 0xfe, 0x11, 0x11, 0x33,
      /* transmitting to us??? : */ 0x00, 0x1d, 0xc1, 0xff, 0xfe, 0x11, 0x66, 0x33,
    ]).await; */
  }

  async fn send_clock_stats(&mut self) {
    let freq_offset = self.get_freq_offset_ppb();
    let required_prefix = format!("clock-stats.{}0000", hex::encode(self.self_info.mac_address.octets()));
    let mut master_clock = None;
    if let Ok(readdir) = std::fs::read_dir("/tmp") {
      for entry in readdir {
        if let Ok(entry) = entry {
          if entry.file_name().to_string_lossy().starts_with(&required_prefix) {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
              let content = content.trim_ascii();
              if content.len() >= 12 {
                if let Ok(master_id) = hex::decode(&content[0..16]) {
                  master_clock = Some(master_id);
                  break;
                }
              }
            }
          }
        }
      }
    }
    let has_clock = self.clock.read().unwrap().get_overlay().is_some();
    let content = clock_status_content(
      &self.self_info.mac_address.octets(),
      has_clock,
      freq_offset,
      master_clock.as_deref(),
    );
    self
      .send(
        self.device_info_destination,
        0xffff,
        [0x07, 0x2a, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00],
        &content,
      )
      .await;
  }

  /// Updates the interface statistics, call about once a second
  fn sample_interface(&mut self) {
    let Some(name) = &self.self_info.interface_name else {
      return;
    };
    let Some(counters) =
      read_interface_counters(&Path::new("/sys/class/net").join(name).join("statistics"))
    else {
      self.interface_statistics = None;
      return;
    };
    let now = (Instant::now(), counters);
    if let Some(prev) = &self.interface_sample {
      self.interface_statistics = Some(interface_statistics(prev, &now));
    }
    self.interface_sample = Some(now);
  }

  async fn send_interface_statistics(&mut self) {
    let content = interface_statistics_content(self.interface_statistics, self.self_info.link_speed);
    self
      .send(
        self.device_info_destination,
        0xffff,
        [0x07, 0x2a, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00],
        &content,
      )
      .await;
  }

  async fn send_network_info(&mut self) {
    let mut bytes = ByteBuffer::new();
    bytes.set_endian(bytebuffer::Endian::BigEndian);
    bytes.write_bytes(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x00]);
    bytes.write_u16(self.self_info.link_speed);
    bytes.write_u16(1);
    bytes.write_bytes(&self.self_info.mac_address.octets());
    bytes.write_bytes(&self.self_info.ip_address.octets());
    bytes.write_bytes(&self.self_info.netmask.octets());
    bytes.write_bytes(&self.self_info.gateway.octets());
    bytes.write_bytes(&self.self_info.gateway.octets()); // DNS? doesn't really matter.
    bytes.write_bytes(&[
      0x00, 0x18, 0x00, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ]);

    self
      .send(
        self.device_info_destination,
        0xffff,
        [0x07, 0x2a, 0x00, 0x11, 0x00, 0x00, 0x00, 0x00],
        bytes.as_bytes(),
      )
      .await;
  }
}

/// Cumulative counters of a network interface, as reported by the OS.
#[derive(Clone, Copy, Debug)]
struct InterfaceCounters {
  tx_bytes: u64,
  rx_bytes: u64,
  tx_errors: u64,
  rx_errors: u64,
}

#[derive(Clone, Copy, Debug)]
struct InterfaceStatistics {
  tx_bytes_per_s: u32,
  rx_bytes_per_s: u32,
  /// cumulative
  tx_errors: u32,
  rx_errors: u32,
}

/// Reads counters from a Linux sysfs interface statistics directory (`/sys/class/net/<name>/statistics`).
fn read_interface_counters(dir: &Path) -> Option<InterfaceCounters> {
  let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok()?.trim().parse::<u64>().ok();
  Some(InterfaceCounters {
    tx_bytes: read("tx_bytes")?,
    rx_bytes: read("rx_bytes")?,
    tx_errors: read("tx_errors")?,
    rx_errors: read("rx_errors")?,
  })
}

/// Rates between two samples of the counters, and the latest cumulative error counts.
fn interface_statistics(
  prev: &(Instant, InterfaceCounters),
  now: &(Instant, InterfaceCounters),
) -> InterfaceStatistics {
  let secs = now.0.saturating_duration_since(prev.0).as_secs_f64();
  let rate = |a: u64, b: u64| {
    if secs > 0.0 {
      (b.saturating_sub(a) as f64 / secs).round().min(u32::MAX as f64) as u32
    } else {
      0
    }
  };
  let clamp = |v: u64| v.min(u32::MAX as u64) as u32;
  InterfaceStatistics {
    tx_bytes_per_s: rate(prev.1.tx_bytes, now.1.tx_bytes),
    rx_bytes_per_s: rate(prev.1.rx_bytes, now.1.rx_bytes),
    tx_errors: clamp(now.1.tx_errors),
    rx_errors: clamp(now.1.rx_errors),
  }
}

/// Content of the interface statistics (0x0040) reply: one interface group with one record. Utilization and
/// error counts are advertised only when the interface's counters are available.
fn interface_statistics_content(stats: Option<InterfaceStatistics>, link_speed_mbps: u16) -> Vec<u8> {
  let mut bytes = ByteBuffer::new();
  bytes.set_endian(bytebuffer::Endian::BigEndian);
  // offsets are relative to the opcode field, which precedes this content (8 bytes)
  bytes.write_u16(1); // groups count
  bytes.write_u16(0x24); // group offset
  bytes.write_u16(0x10); // header offset
  bytes.write_u16(0);
  // header, first 16 bytes as in real devices:
  bytes.write_bytes(&[0x00, 0x14, 0, 0, 0, 0, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 0, 0]);
  bytes.write_u32(if stats.is_some() {
    0x3 /* utilization, errors */
  } else {
    0
  }); // capabilities
      // group:
  bytes.write_u16(1); // records count
  bytes.write_u16(0x28); // record offset
                         // record:
  let stats = stats.unwrap_or(InterfaceStatistics {
    tx_bytes_per_s: 0,
    rx_bytes_per_s: 0,
    tx_errors: 0,
    rx_errors: 0,
  });
  bytes.write_u32(stats.tx_bytes_per_s);
  bytes.write_u32(stats.rx_bytes_per_s);
  bytes.write_u32(stats.tx_errors);
  bytes.write_u32(stats.rx_errors);
  bytes.write_u32(1); // ??? 1 in real devices with link up
  bytes.write_u32(link_speed_mbps.into());
  bytes.into_vec()
}

/// Status of a fixed, not configurable u32 setting (sample rate, encoding): current = requested = the only choice.
fn fixed_u32_status(value: u32) -> Vec<u8> {
  let mut bytes = ByteBuffer::new();
  bytes.set_endian(bytebuffer::Endian::BigEndian);
  bytes.write_u16(0x18); // offset of the choices, relative to the opcode field
  bytes.write_u16(1); // number of choices
  bytes.write_u32(value); // current
  bytes.write_u32(value); // requested
  bytes.write_u16(0); // update mode: fixed
  bytes.write_u16(0);
  bytes.write_u32(value);
  bytes.into_vec()
}

/// Status of the sample rate pull-up: none, fixed. Since record revision 0x070f a flags word precedes the choices.
fn no_pullup_status() -> Vec<u8> {
  let mut bytes = ByteBuffer::new();
  bytes.set_endian(bytebuffer::Endian::BigEndian);
  bytes.write_u16(0x20); // offset of the choices, relative to the opcode field
  bytes.write_u16(1); // number of choices
  bytes.write_u32(0); // current: no pull-up
  bytes.write_u32(0); // requested
  bytes.write_u16(0); // update mode: fixed
  bytes.write_u16(0);
  bytes.write_u32(0);
  bytes.write_u32(0); // flags
  bytes.write_u32(0); // the only choice: no pull-up
  bytes.into_vec()
}

/// Reply (opcode, content) to a settings status request, reporting what Inferno actually supports.
/// None for requests answered elsewhere or not at all.
fn settings_status_reply(
  request_opcode: u16,
  sample_rate: u32,
  bits_per_sample: u8,
) -> Option<(u16, Vec<u8>)> {
  Some(match request_opcode {
    // AES67: currently disabled, not configured
    0x1006 => (0x1007, vec![0, 0, 0, 0]),
    0x0081 => (0x0080, fixed_u32_status(sample_rate)),
    0x0083 => (0x0082, fixed_u32_status(bits_per_sample.into())),
    0x0085 => (0x0084, no_pullup_status()),
    // codec: no parameters (descriptor width 8, descriptors at 0x10)
    0x100a => (0x100b, vec![0, 0, 0, 0, 0, 8, 0, 0x10]),
    // device lock: unlocked, no lock holder identifiers (identifier width 8)
    0x1008 => (0x1009, vec![0, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0]),
    _ => return None,
  })
}

/// Whether a settings request asks to change the setting (non-zero mode/flags word after the opcode field).
fn is_settings_write(request: &[u8]) -> bool {
  request.get(32..36).is_some_and(|word| word != [0, 0, 0, 0])
}

/// Content of the clock status (0x0020) reply to a clock status query (0x0021).
///
/// Always answered, also without PTP daemon information: controllers wait for the reply. When the PTP daemon's
/// leader isn't known, the clock state is "undisciplined" (or "none" without any clock), the servo state "unknown"
/// and the device reports itself as master, like leaderless Dante devices do. The frequency offset field has no
/// "unknown" value, it's 0 when not available.
fn clock_status_content(
  mac: &[u8; 6],
  has_clock: bool,
  freq_offset: Option<i32>,
  master_clock: Option<&[u8]>,
) -> Vec<u8> {
  let mut own_id = [0u8; 8];
  own_id[..6].copy_from_slice(mac);
  let (clock_state, servo_state, master) = match (freq_offset, master_clock) {
    (Some(_), Some(master)) => (3u16 /* disciplined */, 3u16 /* synchronized */, master),
    _ => (
      if has_clock {
        2 /* undisciplined */
      } else {
        0 /* none */
      },
      4, /* unknown */
      &own_id[..],
    ),
  };
  let mut bytes = ByteBuffer::new();
  bytes.set_endian(bytebuffer::Endian::BigEndian);
  bytes.write_u16(clock_state);
  bytes.write_u16(servo_state);
  bytes.write_bytes(&[
    0x00, 0x00, /* clock source: internal */
    0x00, /* not preferred leader */
    0x9f, /* stratum, was 0xff */
  ]);
  bytes.write_i32(freq_offset.unwrap_or(0));
  bytes.write_bytes(mac);
  bytes.write_u16(0);
  bytes.write_bytes(master);
  bytes.write_bytes(master);
  bytes.write_bytes(&[0u8; 76]);
  bytes.into_vec()
}

/// Heartbeat record 0x8002 with metering peaks, padded to a multiple of 4 bytes. Nothing if there are no peaks.
fn write_peaks_record(bytes: &mut ByteBuffer, ctr: u16, tx_peaks: &[u8], rx_peaks: &[u8]) {
  let total_peaks = rx_peaks.len() + tx_peaks.len();
  if total_peaks == 0 {
    return;
  }
  let mut total_len = 24 + total_peaks;
  while (total_len % 4) != 0 {
    total_len += 1;
  }
  let end_pos = bytes.get_wpos() + total_len;
  // the padding is part of the record
  bytes.write_u16(total_len.try_into().unwrap());
  bytes.write_u16(0x8002);
  bytes.write_u16(4);
  bytes.write_u16((total_len - 12).try_into().unwrap()); // content length
  bytes.write_u16(ctr);
  bytes.write_u16(0);
  bytes.write_u16(tx_peaks.len().try_into().unwrap());
  bytes.write_u16(0);
  bytes.write_u16(rx_peaks.len().try_into().unwrap());
  bytes.write_u16(0);
  bytes.write_u16(24);
  bytes.write_u16(0);
  for &peak in tx_peaks.iter().chain(rx_peaks.iter()) {
    bytes.write_u8(peak);
  }
  while bytes.get_wpos() < end_pos {
    bytes.write_u8(0);
  }
}

pub async fn run_server(
  self_info: Arc<DeviceInfo>,
  mut rx: mpsc::Receiver<MulticastMessage>,
  clock: Arc<RwLock<MediaClock>>,
  mut channels_sub_rx: watch::Receiver<Option<Arc<ChannelsSubscriber>>>,
  get_peaks: PeaksCallback,
  shutdown: BroadcastReceiver<()>,
) {
  let server =
    UdpSocketWrapper::new(Some(self_info.ip_address), self_info.info_request_port, shutdown).await;
  let mut recv_buff = crate::net_utils::ReceiveBuffer::new();
  let mut mcaster = Multicaster::new(self_info.as_ref(), server, clock, get_peaks);
  mcaster.send_board_info().await;
  mcaster.send_product_info().await;
  let mut heartbeat_interval = interval(Duration::from_secs(1));
  heartbeat_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
  while mcaster.should_work() {
    select! {
      r = mcaster.server.recv(&mut recv_buff) => {
        let (_src, request_buf) = match r {
          Some(v) => v,
          None => continue
        };
        if request_buf.len() < 32 {
          error!("too short packet received: {}", hex::encode(request_buf));
          continue;
        }
        let opcode = &request_buf[24..32];
        match opcode {
          [0x07, _, 0, 0x61, 0, 0, 0, 0] => {
            mcaster.send_board_info().await;
          },
          [0x07, _, 0, 0xc1, 0, 0, 0, 0] => {
            mcaster.send_product_info().await;
          },
          [0x07, _ /* was 0x38 */, 0, 0x21, 0, 0, 0, _ /* was 0x64 */] => {
            mcaster.send_clock_stats().await;
          },
          [0x07, _, 0, 0x13, 0, 0, 0, _] => {
            mcaster.send_network_info().await;
          }
          [0x07, _, 0, 0x41, 0, 0, 0, 0] => {
            mcaster.send_interface_statistics().await;
          }
          [0x07, _, 0, 0x77, 0, 0, 0, _]=> {
            mcaster.send(
              mcaster.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0x78, 0, 0, 0, 0],
              &[0, 0, 0, 3, 0, 0, 0, 0]
            ).await;
          }
          [0x07, _, hi, lo, _, _, _, _] => {
            let request_opcode = u16::from_be_bytes([*hi, *lo]);
            let self_info = &mcaster.self_info;
            if let Some((reply_opcode, content)) =
              settings_status_reply(request_opcode, self_info.sample_rate, self_info.bits_per_sample)
            {
              if is_settings_write(request_buf) {
                // nothing here is configurable at runtime: answer with the unchanged status
                warn!("ignoring request to change setting {request_opcode:#06x}: not supported");
              }
              let [rh, rl] = reply_opcode.to_be_bytes();
              mcaster.send(mcaster.device_info_destination, 0xffff, [0x07, 0x2a, rh, rl, 0, 0, 0, 0], &content).await;
            } else {
              warn!("unknown request to multicast port: opcode: {}", hex::encode(opcode));
              warn!("raw udp payload: {}", hex::encode(request_buf));
            }
          }
          _ => {
            warn!("unknown request to multicast port: opcode: {}", hex::encode(opcode));
            warn!("raw udp payload: {}", hex::encode(request_buf));
          }
        };
      },
      m = rx.recv() => {
        // TODO we could also make seqnum atomic and simply share socket with anyone that wants it
        if let Some(msg) = m {
          mcaster.send(mcaster.device_info_destination, msg.start_code, msg.opcode, &msg.content).await;
        } else {
          break;
        }
      },
      _ = heartbeat_interval.tick() => {
        mcaster.sample_interface();
        mcaster.send_heartbeat().await;
      },
      _ = channels_sub_rx.changed() => {
        mcaster.channels_subscriber = channels_sub_rx.borrow_and_update().clone();
      }
      // TODO receive shutdown properly, currently Ctrl-C doesn't work if there is error binding to socket
    };
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const MAC: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];

  /// Fields of a clock status reply, as controllers read them (offsets relative to the content).
  struct ClockStatus {
    clock_state: u16,
    servo_state: u16,
    freq_offset: i32,
    device_uuid: Vec<u8>,
    master_uuid: Vec<u8>,
    grandmaster_uuid: Vec<u8>,
  }
  fn parse_clock_status(c: &[u8]) -> ClockStatus {
    assert!(c.len() >= 40, "too short for controllers: {} bytes", c.len());
    ClockStatus {
      clock_state: u16::from_be_bytes([c[0], c[1]]),
      servo_state: u16::from_be_bytes([c[2], c[3]]),
      freq_offset: i32::from_be_bytes(c[8..12].try_into().unwrap()),
      device_uuid: c[12..20].to_vec(),
      master_uuid: c[20..28].to_vec(),
      grandmaster_uuid: c[28..36].to_vec(),
    }
  }

  /// Real AVIO interface statistics reply (netaudio fixture `interface_statistics`): 100 Mb/s, rates and errors.
  const AVIO_INTERFACE_STATISTICS: &str = "ffff00589ed40000001dc1fffe50368b417564696e6174650738004000000000000100240010000000140000000000010000000000000000000000030001002800085fd80009926d00000000000000000000000100000064";

  #[test]
  fn interface_statistics_reply_layout_matches_real_device() {
    let capture = hex::decode(AVIO_INTERFACE_STATISTICS).unwrap();
    let stats = InterfaceStatistics {
      tx_bytes_per_s: 0x85fd8,
      rx_bytes_per_s: 0x9926d,
      tx_errors: 0,
      rx_errors: 0,
    };
    assert_eq!(hex::encode(interface_statistics_content(Some(stats), 100)), hex::encode(&capture[32..]));
  }

  #[test]
  fn interface_statistics_without_counters_supports_nothing() {
    let c = interface_statistics_content(None, 1000);
    assert_eq!(c.len(), 88 - 32);
    assert_eq!(&c[0x18..0x1c], [0, 0, 0, 0], "capability mask");
    assert_eq!(&c[0x20..0x30], [0u8; 16], "counters");
    assert_eq!(&c[0x34..0x38], 1000u32.to_be_bytes(), "link speed");
  }

  #[test]
  fn interface_rates_are_per_second() {
    let a = InterfaceCounters { tx_bytes: 1000, rx_bytes: 5000, tx_errors: 1, rx_errors: 2 };
    let b = InterfaceCounters { tx_bytes: 251000, rx_bytes: 505000, tx_errors: 1, rx_errors: 3 };
    let t0 = Instant::now();
    let stats = interface_statistics(&(t0, a), &(t0 + Duration::from_secs(2), b));
    assert_eq!((stats.tx_bytes_per_s, stats.rx_bytes_per_s), (125000, 250000));
    assert_eq!((stats.tx_errors, stats.rx_errors), (1, 3));
    // counters reset (e.g. interface re-created): no negative rates
    let stats = interface_statistics(&(t0, b), &(t0 + Duration::from_secs(1), a));
    assert_eq!((stats.tx_bytes_per_s, stats.rx_bytes_per_s), (0, 0));
  }

  #[test]
  fn interface_counters_are_read_from_sysfs() {
    let dir = std::env::temp_dir().join(format!("inferno-test-iface-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, value) in
      [("tx_bytes", "123\n"), ("rx_bytes", "456\n"), ("tx_errors", "7\n"), ("rx_errors", "8\n")]
    {
      std::fs::write(dir.join(name), value).unwrap();
    }
    let c = read_interface_counters(&dir).unwrap();
    assert_eq!((c.tx_bytes, c.rx_bytes, c.tx_errors, c.rx_errors), (123, 456, 7, 8));
    std::fs::remove_file(dir.join("rx_errors")).unwrap();
    assert!(read_interface_counters(&dir).is_none());
    std::fs::remove_dir_all(&dir).unwrap();
  }

  /// Settings status replies; expected bytes are the content after the opcode field of replies netaudio
  /// parses as: AES67 off, sample rate 48 kHz fixed, encoding 24-bit fixed, no pull-up (fixed), no codec
  /// parameters, unlocked without identifiers.
  #[test]
  fn settings_status_replies() {
    let expected: [(u16, u16, &str); 6] = [
      (0x1006, 0x1007, "00000000"),
      (0x0081, 0x0080, "001800010000bb800000bb80000000000000bb80"),
      (0x0083, 0x0082, "0018000100000018000000180000000000000018"),
      (0x0085, 0x0084, "00200001000000000000000000000000000000000000000000000000"),
      (0x100a, 0x100b, "0000000000080010"),
      (0x1008, 0x1009, "00000000000000080000000000000000"),
    ];
    for (request, reply, content) in expected {
      let (reply_opcode, bytes) =
        settings_status_reply(request, 48000, 24).expect("request should be answered");
      assert_eq!(reply_opcode, reply, "reply opcode for {request:#06x}");
      assert_eq!(hex::encode(bytes), content, "reply content for {request:#06x}");
    }
    let (_, rate) = settings_status_reply(0x0081, 96000, 24).unwrap();
    assert_eq!(&rate[4..8], 96000u32.to_be_bytes());
    assert!(settings_status_reply(0x0021, 48000, 24).is_none(), "clock status has its own reply");
  }

  #[test]
  fn settings_write_requests_are_recognized() {
    let mut query = vec![0u8; 40];
    query[24..32].copy_from_slice(&[0x07, 0x3a, 0x00, 0x81, 0, 0, 0, 0x64]);
    assert!(!is_settings_write(&query));
    let mut write = query.clone();
    write[24..26].copy_from_slice(&[0x07, 0x27]);
    write[32..36].copy_from_slice(&1u32.to_be_bytes());
    write[36..40].copy_from_slice(&96000u32.to_be_bytes());
    assert!(is_settings_write(&write));
    assert!(!is_settings_write(&query[..32]), "lock status query has no body");
  }

  #[test]
  fn clock_status_with_ptp_leader_reports_synchronized() {
    let leader = [0x00, 0x1d, 0xc1, 0x08, 0x12, 0x58, 0x00, 0x00];
    let c = clock_status_content(&MAC, true, Some(-1234), Some(&leader));
    let st = parse_clock_status(&c);
    assert_eq!((st.clock_state, st.servo_state), (3, 3));
    assert_eq!(st.freq_offset, -1234);
    assert_eq!(st.device_uuid, [&MAC[..], &[0, 0]].concat());
    assert_eq!((st.master_uuid, st.grandmaster_uuid), (leader.to_vec(), leader.to_vec()));
    assert_eq!(c.len(), 112);
  }

  /// Without the PTP daemon's leader information the query must still be answered (controllers time out
  /// otherwise), without claiming synchronization.
  #[test]
  fn clock_status_without_ptp_leader_is_answered_honestly() {
    let own = [&MAC[..], &[0, 0]].concat();
    for (has_clock, freq_offset, clock_state) in [(true, Some(56), 2), (true, None, 2), (false, None, 0)]
    {
      let st = parse_clock_status(&clock_status_content(&MAC, has_clock, freq_offset, None));
      assert_eq!(
        (st.clock_state, st.servo_state),
        (clock_state, 4),
        "clock {has_clock}, offset {freq_offset:?}"
      );
      assert_eq!(st.freq_offset, freq_offset.unwrap_or(0));
      assert_eq!((st.master_uuid, st.grandmaster_uuid), (own.clone(), own.clone()));
    }
    // a leader without a frequency offset isn't trusted either
    let st = parse_clock_status(&clock_status_content(&MAC, true, None, Some(&[1, 2, 3, 4, 5, 6, 0, 0])));
    assert_eq!((st.clock_state, st.servo_state), (2, 4));
  }

  /// Records' declared lengths must cover their padding: receivers (e.g. netaudio) walk records by their
  /// length and reject lengths that aren't multiples of 4.
  #[test]
  fn peaks_record_length_includes_padding() {
    for (tx, rx) in [(2usize, 0usize), (1, 2), (2, 2), (5, 3)] {
      let mut bytes = ByteBuffer::new();
      bytes.set_endian(bytebuffer::Endian::BigEndian);
      write_peaks_record(&mut bytes, 7, &vec![0x11; tx], &vec![0x22; rx]);
      let data = bytes.as_bytes();
      let declared_len = u16::from_be_bytes([data[0], data[1]]) as usize;
      let content_len = u16::from_be_bytes([data[6], data[7]]) as usize;
      assert_eq!(declared_len, data.len(), "{tx} TX + {rx} RX peaks: declared length vs bytes written");
      assert_eq!(declared_len % 4, 0, "{tx} TX + {rx} RX peaks");
      assert_eq!(content_len, declared_len - 12, "{tx} TX + {rx} RX peaks: content length");
      assert_eq!(&data[24..24 + tx + rx], [vec![0x11; tx], vec![0x22; rx]].concat().as_slice());
    }
  }
}
