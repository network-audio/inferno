//! A sound card on any platform, through the USB/IP protocol: a client in place of Windows (usbip-win2 and
//! its USB audio driver) keeps playback and capture transfers queued, as Windows does. Another Inferno device
//! transmits a known signal to the card's recording channels; the card's playback channels are received by
//! a third. All follow one free-running clock.

use std::{
  collections::{BTreeMap, HashMap},
  io::{Read, Write},
  net::{Ipv4Addr, TcpStream},
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
  },
  time::{Duration, Instant},
};

use inferno_aoip::{
  clock::{ClockSource, FreeRunningClockSource},
  device_server::{
    state_dir, AtomicSample, BuffersValid, DeviceServer, ExternalBufferParameters, MediaClock, Sample,
    Settings, StreamAnchor,
  },
};
use inferno_soundcard::{
  config::{card_uac2_config, CardConfig},
  engine::{CardChannels, Engine},
};
use usbip_audio::{uac2::Uac2Device, Server};

const RATE: u32 = 48000;
const CHANNELS: usize = 2;
/// Frames of the transmitted signal (power of 2), about 22 s.
const SIGNAL_FRAMES: usize = 1 << 20;
/// microframes per transfer, and transfers kept queued per direction (like Windows' USB audio driver)
const PACKETS: usize = 8;
const QUEUED: usize = 3;

fn settings(name: &str, process_id: u16, port: u16, rx: usize, tx: usize) -> Settings {
  let mut config = BTreeMap::new();
  for (k, v) in [
    ("NAME", name.to_string()),
    ("PROCESS_ID", process_id.to_string()),
    ("ALT_PORT", port.to_string()),
    ("BIND_IP", "127.0.0.1".into()),
    ("SAMPLE_RATE", RATE.to_string()),
    ("RX_CHANNELS", rx.to_string()),
    ("TX_CHANNELS", tx.to_string()),
    ("TX_SOURCE_BIT_DEPTH", "24".into()),
  ] {
    config.insert(k.to_string(), v);
  }
  Settings::try_new(name, name, Some(Ipv4Addr::LOCALHOST), &config).unwrap()
}

/// The sample of channel `ch` at position `p`: 24-bit, unique within `SIGNAL_FRAMES` (the transmitter's ring,
/// which it repeats).
fn signal(ch: usize, p: u64) -> Sample {
  let p = p as u32 & (SIGNAL_FRAMES as u32 - 1);
  (((p.wrapping_mul(2654435761) >> 8) ^ (ch as u32 * 0x5a5a5a)) << 8) as Sample
}

/// Checks that each channel continues the signal sample by sample. Allowed: silence (not received, or
/// late), and skipping ahead (after a stall, the transfers restart from the present: a dropout, as on real
/// USB), but no older audio (e.g. a ring cycle ago).
#[derive(Default)]
struct Checker {
  next: [Option<u64>; CHANNELS],
  matched: usize,
  silent: usize,
  skips: usize,
  mismatch: Option<String>,
}

impl Checker {
  fn check(&mut self, ch: usize, s: Sample) {
    match self.next[ch] {
      None if s != 0 => {
        self.next[ch] = (0..SIGNAL_FRAMES as u64).find(|&p| signal(ch, p) == s).map(|p| p + 1)
      }
      None => {}
      Some(p) => {
        if s == signal(ch, p) {
          self.matched += 1;
        } else if s == 0 {
          self.silent += 1;
        } else if let Some(later) = (p + 1..p + RATE as u64).find(|&q| signal(ch, q) == s) {
          self.skips += 1;
          self.next[ch] = Some(later + 1);
          return;
        } else if self.mismatch.is_none() {
          self.mismatch = Some(format!("channel {ch} at {p}: got {s:#x}, expected {:#x}", signal(ch, p)));
        }
        self.next[ch] = Some(p + 1);
      }
    }
  }
}

fn be32(bytes: &[u8]) -> u32 {
  u32::from_be_bytes(bytes[..4].try_into().unwrap())
}

fn submit(seq: u32, direction: u32, ep: u32, payload: &[u8], packet_len: usize) -> Vec<u8> {
  let mut out = vec![];
  let length = (PACKETS * packet_len) as u32;
  for value in [1, seq, 0x0001_0001, direction, ep, 0, length, 0, PACKETS as u32, 1] {
    out.extend_from_slice(&u32::to_be_bytes(value));
  }
  out.extend_from_slice(&[0; 8]);
  out.extend_from_slice(payload);
  for i in 0..PACKETS {
    for value in [(i * packet_len) as u32, packet_len as u32, 0, 0] {
      out.extend_from_slice(&value.to_be_bytes());
    }
  }
  out
}

/// Plays the signal to the card and checks what it records, until `stop`.
fn windows_client(port: u16, stop: Arc<AtomicBool>, captured: Arc<Mutex<Checker>>) {
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

  let packet_len = 6 * CHANNELS * 4;
  let mut played = 0u64;
  let mut play = |seq: u32| {
    let mut payload = vec![];
    for _ in 0..PACKETS * 6 {
      for ch in 0..CHANNELS {
        payload.extend_from_slice(&signal(ch, played).to_le_bytes());
      }
      played += 1;
    }
    submit(seq, 0, 1, &payload, packet_len)
  };
  let mut seq = 0;
  let mut kinds = HashMap::new();
  for _ in 0..QUEUED {
    for direction in [0, 1] {
      seq += 1;
      kinds.insert(seq, direction);
      let message = if direction == 0 { play(seq) } else { submit(seq, 1, 2, &[], packet_len) };
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
    let message = if direction == 0 { play(seq) } else { submit(seq, 1, 2, &[], packet_len) };
    stream.write_all(&message).unwrap();
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plays_and_records_through_usbip() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
  let state_home =
    std::env::temp_dir().join(format!("inferno_soundcard_test_state_{}", std::process::id()));
  std::env::set_var("XDG_STATE_HOME", &state_home);
  let clock: Arc<dyn ClockSource> = Arc::new(FreeRunningClockSource::new());

  // transmitter of the signal, at its ring positions
  let tx_settings = settings("soundcard_test_tx", 41, 10100, 0, CHANNELS);
  let tx_hostname = tx_settings.self_info.friendly_hostname.clone();
  let mut tx = DeviceServer::try_start_with_clock(tx_settings, clock.clone()).await.unwrap();
  let rings: Arc<Vec<Vec<AtomicSample>>> = Arc::new(
    (0..CHANNELS)
      .map(|ch| (0..SIGNAL_FRAMES).map(|p| AtomicSample::new(signal(ch, p as u64))).collect())
      .collect(),
  );
  let valid = BuffersValid::new(true);
  let buffers = rings
    .iter()
    .map(|ring| unsafe {
      ExternalBufferParameters::new(ring.as_ptr(), SIGNAL_FRAMES, 1, valid.clone(), None)
    })
    .collect();
  let mut clock_rx = tx.get_realtime_clock_receiver();
  let start = loop {
    clock_rx.update();
    let mut media_clock = MediaClock::new(false);
    if let Some(overlay) = clock_rx.get() {
      media_clock.update_overlay(*overlay);
      if let Some(now) = media_clock.wrapping_now_in_timebase(RATE as u64) {
        break now;
      }
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
  };
  tx.transmit_from_external_buffer(buffers, StreamAnchor::new_started(start), Default::default(), None)
    .await;

  // the card's recording channels subscribed to it, as saved by a controller's routing
  let card_settings = settings("soundcard_test_card", 42, 10200, CHANNELS, CHANNELS);
  let card_hostname = card_settings.self_info.friendly_hostname.clone();
  let dir = std::path::PathBuf::from(state_dir(&card_settings.self_info));
  std::fs::create_dir_all(&dir).unwrap();
  let subscriptions: String = (1..=CHANNELS)
    .map(|ch| {
      format!(
        "[[channels]]\nlocal_channel_id = {ch}\nlocal_channel_name = \"RX {ch}\"\ntx_channel_name = \
         \"TX {ch}\"\ntx_hostname = \"{tx_hostname}\"\n"
      )
    })
    .collect();
  std::fs::write(dir.join("rx_subscriptions.toml"), subscriptions).unwrap();
  let clock_for_engine = clock.clone();
  let (engine, server) = tokio::task::spawn_blocking(move || {
    let channels = [CardChannels { playback: CHANNELS, capture: CHANNELS }];
    let mut engine = Engine::new(card_settings, &channels, Some(clock_for_engine)).unwrap();
    let card_clock = engine.start().unwrap();
    let server = Server::start(0, card_clock).unwrap();
    let card = CardConfig { name: "Test card".into(), playback_channels: 2, capture_channels: 2 };
    let streams = engine.card_streams().pop().unwrap();
    server
      .add("1-1", Arc::new(Uac2Device::new(card_uac2_config(&card, 0, "test", RATE), streams).unwrap()));
    (engine, server)
  })
  .await
  .unwrap();

  // receiver of the card's playback
  let mut rx =
    DeviceServer::try_start_with_clock(settings("soundcard_test_rx", 43, 10300, CHANNELS, 0), clock)
      .await
      .unwrap();
  let played = Arc::new(Mutex::new(Checker::default()));
  let checker = played.clone();
  rx.receive_with_callback(Box::new(move |count: usize, channels: &Vec<Vec<Sample>>| {
    let mut checker = checker.lock().unwrap();
    for (ch, samples) in channels.iter().enumerate() {
      for &s in &samples[..count] {
        checker.check(ch, s);
      }
    }
  }))
  .await;
  for ch in 1..=CHANNELS {
    rx.subscribe(ch - 1, &format!("TX {ch}"), &card_hostname).await;
  }

  let stop = Arc::new(AtomicBool::new(false));
  let captured = Arc::new(Mutex::new(Checker::default()));
  let client = {
    let (port, stop, captured) = (server.port(), stop.clone(), captured.clone());
    std::thread::spawn(move || windows_client(port, stop, captured))
  };
  let enough = || {
    played.lock().unwrap().matched >= CHANNELS * RATE as usize * 2
      && captured.lock().unwrap().matched >= CHANNELS * RATE as usize * 2
  };
  let deadline = Instant::now() + Duration::from_secs(30);
  while !enough() && Instant::now() < deadline {
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
  stop.store(true, Ordering::Relaxed);
  client.join().unwrap();
  rx.shutdown().await;
  tx.shutdown().await;
  tokio::task::spawn_blocking(move || {
    drop(server);
    drop(engine)
  })
  .await
  .unwrap();
  let _ = std::fs::remove_dir_all(&state_home);

  let (played, captured) = (played.lock().unwrap(), captured.lock().unwrap());
  println!(
    "played: {} matched, {} silent, {} skips; recorded: {} matched, {} silent, {} skips",
    played.matched, played.silent, played.skips, captured.matched, captured.silent, captured.skips
  );
  assert_eq!(captured.mismatch, None, "recorded exactly what was transmitted, or silence");
  assert_eq!(played.mismatch, None, "transmitted exactly what was played, or silence");
  assert!(captured.matched >= CHANNELS * RATE as usize * 2, "two seconds recorded exactly");
  assert!(played.matched >= CHANNELS * RATE as usize * 2, "two seconds played exactly");
  // shared CI machines stall threads now and then: only that may make audio late (silence)
  assert!(played.silent <= played.matched / 10, "played audio mostly on time");
  assert!(played.skips + captured.skips <= 10, "a few stalls at most");
}
