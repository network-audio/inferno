//! A sound card on any platform, through the USB/IP protocol: a client in place of Windows (usbip-win2 and
//! its USB audio driver) keeps playback and capture transfers queued, as Windows does. Another Inferno device
//! transmits a known signal to the card's recording channels; the card's playback channels are received by
//! a third. All follow one free-running clock.

mod common;

use std::{
  collections::HashMap,
  io::{Read, Write},
  net::{Ipv4Addr, TcpStream},
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
  },
  time::{Duration, Instant},
};

use common::{assert_loopback, own_state_home, signal, Checker, Loopback, CHANNELS};
use inferno_soundcard::config::{card_uac2_config, CardConfig};
use usbip_audio::{uac2::Uac2Device, Server};

/// microframes per transfer, and transfers kept queued per direction (like Windows' USB audio driver)
const PACKETS: usize = 8;
const QUEUED: usize = 3;

fn be32(bytes: &[u8]) -> u32 {
  u32::from_be_bytes(bytes[..4].try_into().unwrap())
}

/// An isochronous CMD_SUBMIT of packets of `lengths`.
fn submit(seq: u32, direction: u32, ep: u32, payload: &[u8], lengths: &[usize]) -> Vec<u8> {
  let mut out = vec![];
  let length = lengths.iter().sum::<usize>() as u32;
  for value in [1, seq, 0x0001_0001, direction, ep, 0, length, 0, lengths.len() as u32, 1] {
    out.extend_from_slice(&u32::to_be_bytes(value));
  }
  out.extend_from_slice(&[0; 8]);
  out.extend_from_slice(payload);
  let mut offset = 0;
  for &len in lengths {
    for value in [offset as u32, len as u32, 0, 0] {
      out.extend_from_slice(&value.to_be_bytes());
    }
    offset += len;
  }
  out
}

/// Frames Windows plays in microframe `m`: whole frames, `rate / 8000` on average, in a pattern of its own
/// (a few microframes out of phase with the card's clock).
fn played_frames(m: u64, rate: u32) -> usize {
  const PHASE: u64 = 3;
  (((m + 1 + PHASE) * rate as u64 / 8000) - ((m + PHASE) * rate as u64 / 8000)) as usize
}

/// Plays the signal to the card and checks what it records, until `stop`.
fn windows_client(port: u16, stop: Arc<AtomicBool>, captured: Arc<Mutex<Checker>>, rate: u32) {
  let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
  stream.set_nodelay(true).unwrap();
  stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
  let mut import = vec![1, 0x11, 0x80, 3, 0, 0, 0, 0];
  import.extend_from_slice(b"1-1");
  import.resize(8 + 32, 0);
  stream.write_all(&import).unwrap();
  let mut reply = [0; 8 + 312];
  stream.read_exact(&mut reply).unwrap();
  assert_eq!(be32(&reply[4..8]), 0, "imported");

  // capture packets: room for the most frames a microframe carries
  let capture_lengths = [rate.div_ceil(8000) as usize * CHANNELS * 4; PACKETS];
  let (mut played, mut microframe) = (0u64, 0u64);
  let mut play = |seq: u32| {
    let (mut payload, mut lengths) = (vec![], vec![]);
    for _ in 0..PACKETS {
      let frames = played_frames(microframe, rate);
      microframe += 1;
      lengths.push(frames * CHANNELS * 4);
      for _ in 0..frames {
        for ch in 0..CHANNELS {
          payload.extend_from_slice(&signal(ch, played).to_le_bytes());
        }
        played += 1;
      }
    }
    submit(seq, 0, 1, &payload, &lengths)
  };
  let mut seq = 0;
  let mut kinds = HashMap::new();
  for _ in 0..QUEUED {
    for direction in [0, 1] {
      seq += 1;
      kinds.insert(seq, direction);
      let message = if direction == 0 { play(seq) } else { submit(seq, 1, 2, &[], &capture_lengths) };
      stream.write_all(&message).unwrap();
    }
  }
  while !stop.load(Ordering::Relaxed) {
    let mut header = [0; 48];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(be32(&header[0..4]), 3, "RET_SUBMIT");
    assert_eq!(be32(&header[20..24]), 0, "status");
    let direction = kinds.remove(&be32(&header[4..8])).unwrap();
    let mut data = vec![0; if direction == 1 { be32(&header[24..28]) as usize } else { 0 }];
    stream.read_exact(&mut data).unwrap();
    let mut descriptors = vec![0; be32(&header[32..36]) as usize * 16];
    stream.read_exact(&mut descriptors).unwrap();
    if direction == 1 {
      let mut checker = captured.lock().unwrap();
      for (i, sample) in data.chunks(4).enumerate() {
        checker.check(i % CHANNELS, i32::from_le_bytes(sample.try_into().unwrap()));
      }
    }
    seq += 1;
    kinds.insert(seq, direction);
    let message = if direction == 0 { play(seq) } else { submit(seq, 1, 2, &[], &capture_lengths) };
    stream.write_all(&message).unwrap();
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// macOS CI runners stall Inferno's flows for tens of milliseconds at a time (see loopback_trx there):
// Linux and Windows CI run this
#[cfg_attr(target_os = "macos", ignore = "timing-sensitive, macOS runners stall too much")]
async fn plays_and_records_through_usbip() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
  let _state = own_state_home();
  // 44.1 kHz: microframes carry 5 or 6 frames
  for (i, rate) in [48000, 44100].into_iter().enumerate() {
    println!("== {rate} Hz");
    plays_and_records_at(rate, i as u16).await;
  }
}

async fn plays_and_records_at(rate: u32, run: u16) {
  let devices = Loopback::start("soundcard_test", rate, run).await;
  let server = Server::start(0, devices.clock.clone()).unwrap();
  let card = CardConfig { name: "Test card".into(), playback_channels: 2, capture_channels: 2 };
  let streams = devices.engine.card_streams().pop().unwrap();
  server
    .add("1-1", Arc::new(Uac2Device::new(card_uac2_config(&card, 0, "test", rate), streams).unwrap()));

  let stop = Arc::new(AtomicBool::new(false));
  let captured = Arc::new(Mutex::new(Checker::default()));
  let client = {
    let (port, stop, captured) = (server.port(), stop.clone(), captured.clone());
    std::thread::spawn(move || windows_client(port, stop, captured, rate))
  };
  let played = devices.played.clone();
  let enough = || {
    played.lock().unwrap().matched >= CHANNELS * rate as usize * 2
      && captured.lock().unwrap().matched >= CHANNELS * rate as usize * 2
  };
  let deadline = Instant::now() + Duration::from_secs(30);
  while !enough() && Instant::now() < deadline {
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
  stop.store(true, Ordering::Relaxed);
  client.join().unwrap();
  devices.finish(server).await;
  assert_loopback(rate, &played.lock().unwrap(), &captured.lock().unwrap());
}
