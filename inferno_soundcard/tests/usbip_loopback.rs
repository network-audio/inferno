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
  config::{card_uac2_config, CardConfig, Config},
  engine::Engine,
};
use inferno_soundcard_shm::RegionCard;
use usbip_audio::{uac2::Uac2Device, Server};

const CHANNELS: usize = 2;
/// Frames of the transmitted signal (power of 2), about 22 s.
const SIGNAL_FRAMES: usize = 1 << 20;
/// microframes per transfer, and transfers kept queued per direction (like Windows' USB audio driver)
const PACKETS: usize = 8;
const QUEUED: usize = 3;

fn settings(name: &str, process_id: u16, port: u16, rx: usize, tx: usize, rate: u32) -> Settings {
  let mut config = BTreeMap::new();
  for (k, v) in [
    ("NAME", name.to_string()),
    ("PROCESS_ID", process_id.to_string()),
    ("ALT_PORT", port.to_string()),
    ("BIND_IP", "127.0.0.1".into()),
    ("SAMPLE_RATE", rate.to_string()),
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
/// late), and resuming elsewhere after a stall: ahead (the transfers restart from the present: a dropout, as
/// on real USB), or up to the silence before it back (silence inserted, the audio delayed by it). Not allowed:
/// any other audio, e.g. a ring cycle ago.
#[derive(Default)]
struct Checker {
  next: [Option<u64>; CHANNELS],
  /// per channel, the silent samples just before
  silent_run: [u64; CHANNELS],
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
        if s == 0 {
          self.silent += 1;
          self.silent_run[ch] += 1;
          self.next[ch] = Some(p + 1);
          return;
        }
        let silent_run = std::mem::take(&mut self.silent_run[ch]);
        if s == signal(ch, p) {
          self.matched += 1;
        } else if let Some(at) = (p.saturating_sub(silent_run)..p + 48000).find(|&q| signal(ch, q) == s) {
          self.matched += 1;
          self.skips += 1;
          self.next[ch] = Some(at + 1);
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
// macOS CI runners stall Inferno's flows for tens of milliseconds at a time (see loopback_trx there): the
// sound cards are for Windows; Linux and Windows CI run this
#[cfg_attr(target_os = "macos", ignore = "timing-sensitive, macOS runners stall too much")]
async fn plays_and_records_through_usbip() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
  let state_home =
    std::env::temp_dir().join(format!("inferno_soundcard_test_state_{}", std::process::id()));
  std::env::set_var("XDG_STATE_HOME", &state_home);
  // 44.1 kHz: microframes carry 5 or 6 frames
  for (i, rate) in [48000, 44100].into_iter().enumerate() {
    println!("== {rate} Hz");
    plays_and_records_at(rate, i as u16).await;
  }
  let _ = std::fs::remove_dir_all(&state_home);
}

async fn plays_and_records_at(rate: u32, run: u16) {
  let clock: Arc<dyn ClockSource> = Arc::new(FreeRunningClockSource::new());
  let (pid, port) = (41 + run * 3, 10100 + run * 300);

  // transmitter of the signal, at its ring positions
  let tx_settings = settings(&format!("soundcard_test_tx_{rate}"), pid, port, 0, CHANNELS, rate);
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
      if let Some(now) = media_clock.wrapping_now_in_timebase(rate as u64) {
        break now;
      }
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
  };
  tx.transmit_from_external_buffer(buffers, StreamAnchor::new_started(start), Default::default(), None)
    .await;

  // the card's recording channels subscribed to it, as saved by a controller's routing
  let card_settings =
    settings(&format!("soundcard_test_card_{rate}"), pid + 1, port + 100, CHANNELS, CHANNELS, rate);
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
    let channels = [RegionCard {
      name: "Test card".into(),
      uid: "test-1".into(),
      playback_channels: CHANNELS,
      capture_channels: CHANNELS,
    }];
    let lead = Config::default().playback_lead(rate);
    let mut engine = Engine::new(card_settings, &channels, lead, Some(clock_for_engine)).unwrap();
    let card_clock = engine.start().unwrap();
    let server = Server::start(0, card_clock).unwrap();
    let card = CardConfig { name: "Test card".into(), playback_channels: 2, capture_channels: 2 };
    let streams = engine.card_streams().pop().unwrap();
    server
      .add("1-1", Arc::new(Uac2Device::new(card_uac2_config(&card, 0, "test", rate), streams).unwrap()));
    (engine, server)
  })
  .await
  .unwrap();

  // receiver of the card's playback
  let mut rx = DeviceServer::try_start_with_clock(
    settings(&format!("soundcard_test_rx_{rate}"), pid + 2, port + 200, CHANNELS, 0, rate),
    clock,
  )
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
    std::thread::spawn(move || windows_client(port, stop, captured, rate))
  };
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
  rx.shutdown().await;
  tx.shutdown().await;
  tokio::task::spawn_blocking(move || {
    drop(server);
    drop(engine)
  })
  .await
  .unwrap();

  let (played, captured) = (played.lock().unwrap(), captured.lock().unwrap());
  println!(
    "played: {} matched, {} silent, {} skips; recorded: {} matched, {} silent, {} skips",
    played.matched, played.silent, played.skips, captured.matched, captured.silent, captured.skips
  );
  assert_eq!(captured.mismatch, None, "recorded exactly what was transmitted, or silence");
  assert_eq!(played.mismatch, None, "transmitted exactly what was played, or silence");
  assert!(captured.matched >= CHANNELS * rate as usize * 2, "{rate} Hz: two seconds recorded exactly");
  assert!(played.matched >= CHANNELS * rate as usize * 2, "{rate} Hz: two seconds played exactly");
  // shared CI machines stall threads now and then: only that may make audio late (silence)
  assert!(played.silent <= played.matched / 10, "played audio mostly on time");
}
