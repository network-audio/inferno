//! The engine behind the ASIO driver, on any platform: a host writing a known signal on every buffer switch,
//! received by another Inferno device over 127.0.0.1. Both follow one free-running clock.

use std::{
  collections::BTreeMap,
  net::Ipv4Addr,
  sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    Arc, Mutex,
  },
  time::{Duration, Instant},
};

use inferno_aoip::{
  clock::{ClockSource, FreeRunningClockSource},
  device_server::{
    state_dir, AtomicSample, BuffersValid, DeviceServer, ExternalBufferParameters, MediaClock, Sample, Settings,
    StreamAnchor,
  },
};
use inferno_asio::engine::{Engine, Host};

const RATE: u32 = 48000;
const PERIOD: usize = 128;

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

/// The sample of channel `ch` at position `p`: 24-bit, unique over a long stretch.
fn signal(ch: usize, p: u64) -> Sample {
  ((((p as u32).wrapping_mul(2654435761) >> 8) ^ (ch as u32 * 0x5a5a5a)) << 8) as Sample
}

struct TestHost {
  outputs: Vec<[usize; 2]>,
  period: usize,
  switches: Arc<AtomicU64>,
  last_position: Arc<AtomicU64>,
  positions_ok: Arc<AtomicBool>,
  /// stall (like an overloaded host) once, at this switch
  stall_at: Option<u64>,
}

impl Host for TestHost {
  fn buffer_switch(&mut self, index: usize, position: u64, _system_time_ns: u64) {
    let n = self.switches.fetch_add(1, Ordering::Relaxed);
    let last = self.last_position.swap(position, Ordering::Relaxed);
    if self.stall_at == Some(n) {
      std::thread::sleep(Duration::from_millis(30));
    }
    // consecutive periods: late switches are caught up, not skipped
    if n > 0 && position != last + self.period as u64 {
      self.positions_ok.store(false, Ordering::Relaxed);
    }
    for (ch, halves) in self.outputs.iter().enumerate() {
      let out = unsafe { std::slice::from_raw_parts_mut(halves[index] as *mut Sample, self.period) };
      for (i, s) in out.iter_mut().enumerate() {
        *s = signal(ch, position + i as u64);
      }
    }
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_output_is_transmitted_on_time() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
  let clock: Arc<dyn ClockSource> = Arc::new(FreeRunningClockSource::new());

  let engine_settings = settings("asio_engine_test", 21, 9600, 2, 2);
  let engine_hostname = engine_settings.self_info.friendly_hostname.clone();
  let (mut engine, clock_for_engine) = (None, clock.clone());
  // the engine blocks (like an ASIO host's thread): not on the runtime's threads
  let built = tokio::task::spawn_blocking(move || {
    Engine::new_with_clock(engine_settings, PERIOD, Some(clock_for_engine))
  })
  .await
  .unwrap()
  .unwrap();
  engine.replace(built);
  let mut engine = engine.unwrap();

  let switches = Arc::new(AtomicU64::new(0));
  let positions_ok = Arc::new(AtomicBool::new(true));
  let host = TestHost {
    outputs: (0..2).map(|ch| [0, 1].map(|h| engine.output_buffer(ch, h).unwrap() as usize)).collect(),
    period: PERIOD,
    switches: switches.clone(),
    last_position: Arc::new(AtomicU64::new(0)),
    positions_ok: positions_ok.clone(),
    stall_at: None,
  };
  let started = Instant::now();
  engine.start(Box::new(host)).unwrap();

  // a receiver checking that each channel continues the signal, sample by sample
  let mut rx =
    DeviceServer::try_start_with_clock(settings("asio_engine_rx", 22, 9700, 2, 0), clock).await.unwrap();
  let matched = Arc::new(AtomicUsize::new(0));
  let silent = Arc::new(AtomicUsize::new(0));
  let mismatched = Arc::new(Mutex::new(None::<String>));
  let (m, sl, mm) = (matched.clone(), silent.clone(), mismatched.clone());
  let mut next: [Option<u64>; 2] = [None, None];
  rx.receive_with_callback(Box::new(move |count: usize, channels: &Vec<Vec<Sample>>| {
    for (ch, samples) in channels.iter().enumerate() {
      for &s in &samples[..count] {
        match next[ch] {
          None if s != 0 => {
            // find the position: brute force over the last minute of the signal
            next[ch] = (0..(RATE as u64 * 60)).find(|&p| signal(ch, p) == s).map(|p| p + 1);
          }
          None => {}
          Some(p) => {
            if s == signal(ch, p) {
              m.fetch_add(1, Ordering::Relaxed);
            } else if s == 0 {
              // a switch late beyond its period (the test's thread stalled): its outputs went out as silence
              sl.fetch_add(1, Ordering::Relaxed);
            } else {
              let mut mm = mm.lock().unwrap();
              eprintln!("MISMATCH channel {ch} at {p}: got {s:#x}, expected {:#x}", signal(ch, p));
              if mm.is_none() {
                *mm = Some(format!("channel {ch} at {p}: got {s:#x}, expected {:#x}", signal(ch, p)));
              }
            }
            next[ch] = Some(p + 1);
          }
        }
      }
    }
  }))
  .await;
  rx.subscribe(0, "TX 1", &engine_hostname).await;
  rx.subscribe(1, "TX 2", &engine_hostname).await;

  let deadline = Instant::now() + Duration::from_secs(10);
  while matched.load(Ordering::Relaxed) < 2 * RATE as usize && Instant::now() < deadline {
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
  let elapsed = started.elapsed().as_secs_f64();
  let switched = switches.load(Ordering::Relaxed);
  rx.shutdown().await;
  tokio::task::spawn_blocking(move || drop(engine)).await.unwrap();

  let expected = elapsed * RATE as f64 / PERIOD as f64;
  let (matched, silent) = (matched.load(Ordering::Relaxed), silent.load(Ordering::Relaxed));
  println!("{switched} switches in {elapsed:.2} s (expected {expected:.0}), {matched} samples matched, {silent} silent");
  assert!(positions_ok.load(Ordering::Relaxed), "switch positions must increase");
  assert!((switched as f64 - expected).abs() < expected * 0.05, "one switch per period");
  assert_eq!(*mismatched.lock().unwrap(), None, "outputs are received exactly, or as silence");
  assert!(matched >= 2 * RATE as usize, "a second of both channels received exactly");
  // shared CI machines stall threads now and then, by more than a period: only that may go out as silence
  assert!(silent <= matched / 10, "outputs mostly on time");
}

#[test]
fn a_stalled_host_gets_the_missed_switches() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
  let clock: Arc<dyn ClockSource> = Arc::new(FreeRunningClockSource::new());
  let mut engine =
    Engine::new_with_clock(settings("asio_engine_stall", 23, 9800, 2, 2), PERIOD, Some(clock)).unwrap();
  let switches = Arc::new(AtomicU64::new(0));
  let positions_ok = Arc::new(AtomicBool::new(true));
  let host = TestHost {
    outputs: (0..2).map(|ch| [0, 1].map(|h| engine.output_buffer(ch, h).unwrap() as usize)).collect(),
    period: PERIOD,
    switches: switches.clone(),
    last_position: Arc::new(AtomicU64::new(0)),
    positions_ok: positions_ok.clone(),
    // 30 ms: about 11 periods
    stall_at: Some(100),
  };
  let started = Instant::now();
  engine.start(Box::new(host)).unwrap();
  std::thread::sleep(Duration::from_secs(1));
  let elapsed = started.elapsed().as_secs_f64();
  let switched = switches.load(Ordering::Relaxed);
  drop(engine);
  let expected = elapsed * RATE as f64 / PERIOD as f64;
  println!("{switched} switches in {elapsed:.2} s (expected {expected:.0})");
  assert!(positions_ok.load(Ordering::Relaxed), "no period skipped");
  assert!((switched as f64 - expected).abs() < expected * 0.05, "the missed switches were caught up");
}

/// Records nothing: checks that each input continues the transmitted signal, or is silent.
struct CheckingHost {
  inputs: Vec<[usize; 2]>,
  period: usize,
  /// per input: the signal position of the next sample, once found
  next: Vec<Option<u64>>,
  matched: Arc<AtomicUsize>,
  silent: Arc<AtomicUsize>,
  mismatch: Arc<Mutex<Option<String>>>,
}

impl Host for CheckingHost {
  fn buffer_switch(&mut self, index: usize, _position: u64, _system_time_ns: u64) {
    for (ch, halves) in self.inputs.iter().enumerate() {
      let input = unsafe { std::slice::from_raw_parts(halves[index] as *const Sample, self.period) };
      for &s in input {
        match self.next[ch] {
          None if s != 0 => {
            self.next[ch] = (0..CAPTURE_SIGNAL_FRAMES as u64).find(|&p| signal(ch, p) == s).map(|p| p + 1);
          }
          None => {}
          Some(p) => {
            // the receiver's silence (not received) is fine; anything else is the signal of another position,
            // e.g. a ring cycle ago
            if s == signal(ch, p) {
              self.matched.fetch_add(1, Ordering::Relaxed);
            } else if s == 0 {
              self.silent.fetch_add(1, Ordering::Relaxed);
            } else {
              let mut mismatch = self.mismatch.lock().unwrap();
              if mismatch.is_none() {
                *mismatch = Some(format!("input {ch} at {p}: got {s:#x}, expected {:#x}", signal(ch, p)));
              }
            }
            self.next[ch] = Some(p + 1);
          }
        }
      }
    }
  }
}

/// Frames of the transmitted signal (power of 2), about 22 s.
const CAPTURE_SIGNAL_FRAMES: usize = 1 << 20;

/// The engine's capture: another device transmits a known signal to the engine's inputs (subscribed in their
/// saved state, as by a controller earlier). The host gets it sample by sample; once the transmitter is gone,
/// silence, never older audio from the engine's rings.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transmitted_audio_reaches_the_host_inputs() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
  let state_home = std::env::temp_dir().join(format!("inferno_asio_test_state_{}", std::process::id()));
  std::env::set_var("XDG_STATE_HOME", &state_home);
  let clock: Arc<dyn ClockSource> = Arc::new(FreeRunningClockSource::new());

  // transmitter: the signal at its ring positions
  let tx_settings = settings("asio_capture_tx", 31, 9900, 0, 2);
  let tx_hostname = tx_settings.self_info.friendly_hostname.clone();
  let mut tx = DeviceServer::try_start_with_clock(tx_settings, clock.clone()).await.unwrap();
  let rings: Arc<Vec<Vec<AtomicSample>>> = Arc::new(
    (0..2).map(|ch| (0..CAPTURE_SIGNAL_FRAMES).map(|p| AtomicSample::new(signal(ch, p as u64))).collect()).collect(),
  );
  let valid = BuffersValid::new(true);
  let buffers = rings
    .iter()
    .map(|ring| unsafe {
      ExternalBufferParameters::new(ring.as_ptr(), CAPTURE_SIGNAL_FRAMES, 1, valid.clone(), None)
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
  tx.transmit_from_external_buffer(buffers, StreamAnchor::new_started(start), Default::default(), None).await;

  // the engine's inputs subscribed to it, as saved by a controller's routing
  let engine_settings = settings("asio_capture_engine", 32, 10000, 2, 0);
  let dir = std::path::PathBuf::from(state_dir(&engine_settings.self_info));
  std::fs::create_dir_all(&dir).unwrap();
  let subscriptions: String = (1..=2)
    .map(|ch| {
      format!(
        "[[channels]]\nlocal_channel_id = {ch}\nlocal_channel_name = \"RX {ch}\"\ntx_channel_name = \"TX {ch}\"\n\
         tx_hostname = \"{tx_hostname}\"\n"
      )
    })
    .collect();
  std::fs::write(dir.join("rx_subscriptions.toml"), subscriptions).unwrap();
  let clock_for_engine = clock.clone();
  let mut engine = tokio::task::spawn_blocking(move || {
    Engine::new_with_clock(engine_settings, PERIOD, Some(clock_for_engine))
  })
  .await
  .unwrap()
  .unwrap();

  let (matched, silent) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
  let mismatch = Arc::new(Mutex::new(None));
  let host = CheckingHost {
    inputs: (0..2).map(|ch| [0, 1].map(|h| engine.input_buffer(ch, h).unwrap() as usize)).collect(),
    period: PERIOD,
    next: vec![None; 2],
    matched: matched.clone(),
    silent: silent.clone(),
    mismatch: mismatch.clone(),
  };
  engine.start(Box::new(host)).unwrap();

  let deadline = Instant::now() + Duration::from_secs(20);
  while matched.load(Ordering::Relaxed) < 2 * RATE as usize && Instant::now() < deadline {
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
  let matched_while_transmitting = matched.load(Ordering::Relaxed);
  // the transmitter disappears without unsubscribing: the inputs must fall silent
  tx.shutdown().await;
  tokio::time::sleep(Duration::from_millis(500)).await;
  let silent_after = silent.load(Ordering::Relaxed);
  tokio::task::spawn_blocking(move || drop(engine)).await.unwrap();
  let _ = std::fs::remove_dir_all(&state_home);

  println!("{matched_while_transmitting} samples matched, then {silent_after} silent");
  assert_eq!(*mismatch.lock().unwrap(), None);
  assert!(matched_while_transmitting >= 2 * RATE as usize, "a second of both inputs received exactly");
  assert!(silent_after >= RATE as usize / 2, "silence once the transmitter is gone");
}

