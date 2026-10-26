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
  device_server::{DeviceServer, Sample, Settings},
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
}

impl Host for TestHost {
  fn buffer_switch(&mut self, index: usize, position: u64, _system_time_ns: u64) {
    let n = self.switches.fetch_add(1, Ordering::Relaxed);
    let last = self.last_position.swap(position, Ordering::Relaxed);
    if n > 0 && position <= last {
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
  };
  let started = Instant::now();
  engine.start(Box::new(host)).unwrap();

  // a receiver checking that each channel continues the signal, sample by sample
  let mut rx =
    DeviceServer::try_start_with_clock(settings("asio_engine_rx", 22, 9700, 2, 0), clock).await.unwrap();
  let matched = Arc::new(AtomicUsize::new(0));
  let mismatched = Arc::new(Mutex::new(None::<String>));
  let (m, mm) = (matched.clone(), mismatched.clone());
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
  println!(
    "{switched} switches in {elapsed:.2} s (expected {expected:.0}), {} samples matched",
    matched.load(Ordering::Relaxed)
  );
  assert!(positions_ok.load(Ordering::Relaxed), "switch positions must increase");
  assert!((switched as f64 - expected).abs() < expected * 0.05, "one switch per period");
  assert_eq!(*mismatched.lock().unwrap(), None);
  assert!(
    matched.load(Ordering::Relaxed) >= 2 * RATE as usize,
    "a second of both channels received exactly"
  );
}
