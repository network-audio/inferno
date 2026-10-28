//! The loopback tests' devices: a transmitter of a known signal to the card's recording channels, the card
//! (the engine, started), and a receiver of its playback channels, checking what it receives. All follow one
//! free-running clock.

#![allow(dead_code)]

use std::{
  collections::BTreeMap,
  net::Ipv4Addr,
  sync::{Arc, Mutex},
  time::Duration,
};

use inferno_aoip::{
  clock::{ClockSource, FreeRunningClockSource},
  device_server::{
    state_dir, AtomicSample, BuffersValid, DeviceServer, ExternalBufferParameters, MediaClock, Sample,
    Settings, StreamAnchor,
  },
};
use inferno_soundcard::{
  config::Config,
  engine::{Engine, EngineClock},
};
use inferno_soundcard_shm::RegionCard;

pub const CHANNELS: usize = 2;
/// Frames of the transmitted signal (power of 2), about 22 s.
pub const SIGNAL_FRAMES: usize = 1 << 20;

pub fn settings(name: &str, process_id: u16, port: u16, rx: usize, tx: usize, rate: u32) -> Settings {
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
pub fn signal(ch: usize, p: u64) -> Sample {
  let p = p as u32 & (SIGNAL_FRAMES as u32 - 1);
  (((p.wrapping_mul(2654435761) >> 8) ^ (ch as u32 * 0x5a5a5a)) << 8) as Sample
}

/// Checks that each channel continues the signal sample by sample. Allowed: silence (not received, or
/// late), and resuming elsewhere after a stall: ahead (the transfers restart from the present: a dropout, as
/// on real USB), or up to the silence before it back (silence inserted, the audio delayed by it). Not allowed:
/// any other audio, e.g. a ring cycle ago.
#[derive(Default)]
pub struct Checker {
  next: [Option<u64>; CHANNELS],
  /// per channel, the silent samples just before
  silent_run: [u64; CHANNELS],
  pub matched: usize,
  pub silent: usize,
  pub skips: usize,
  pub mismatch: Option<String>,
}

impl Checker {
  pub fn check(&mut self, ch: usize, s: Sample) {
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

/// The transmitter, the started card and the receiver.
pub struct Loopback {
  tx: DeviceServer,
  /// the transmitter's (valid until it's shut down)
  tx_rings: Arc<Vec<Vec<AtomicSample>>>,
  rx: DeviceServer,
  pub engine: Engine,
  /// the card's clock
  pub clock: Arc<EngineClock>,
  /// what the receiver got of the card's playback
  pub played: Arc<Mutex<Checker>>,
  pub rate: u32,
}

impl Loopback {
  /// Devices named after `name` and `rate`, on ports of their own for run `run`.
  pub async fn start(name: &str, rate: u32, run: u16) -> Self {
    let clock: Arc<dyn ClockSource> = Arc::new(FreeRunningClockSource::new());
    let (pid, port) = (41 + run * 3, 10100 + run * 300);

    // transmitter of the signal, at its ring positions
    let tx_settings = settings(&format!("{name}_tx_{rate}"), pid, port, 0, CHANNELS, rate);
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
      settings(&format!("{name}_card_{rate}"), pid + 1, port + 100, CHANNELS, CHANNELS, rate);
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
    let (engine, card_clock) = tokio::task::spawn_blocking(move || {
      let cards = [RegionCard {
        name: "Test card".into(),
        uid: "test-1".into(),
        playback_channels: CHANNELS,
        capture_channels: CHANNELS,
      }];
      let lead = Config::default().playback_lead(rate);
      let mut engine = Engine::new(card_settings, &cards, lead, Some(clock_for_engine)).unwrap();
      let card_clock = engine.start().unwrap();
      (engine, card_clock)
    })
    .await
    .unwrap();

    // receiver of the card's playback
    let mut rx = DeviceServer::try_start_with_clock(
      settings(&format!("{name}_rx_{rate}"), pid + 2, port + 200, CHANNELS, 0, rate),
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
    Self { tx, tx_rings: rings, rx, engine, clock: card_clock, played, rate }
  }

  /// Shuts the devices down; `before_engine` is dropped (in a blocking thread) before the engine.
  pub async fn finish<T: Send + 'static>(self, before_engine: T) {
    let Self { tx, tx_rings, rx, engine, .. } = self;
    rx.shutdown().await;
    tx.shutdown().await;
    drop(tx_rings);
    tokio::task::spawn_blocking(move || {
      drop(before_engine);
      drop(engine)
    })
    .await
    .unwrap();
  }
}

/// Uses a state directory of its own for the test (the card's saved subscriptions), removed by the guard.
pub fn own_state_home() -> impl Drop {
  struct Remove(std::path::PathBuf);
  impl Drop for Remove {
    fn drop(&mut self) {
      let _ = std::fs::remove_dir_all(&self.0);
    }
  }
  let state_home =
    std::env::temp_dir().join(format!("inferno_soundcard_test_state_{}", std::process::id()));
  std::env::set_var("XDG_STATE_HOME", &state_home);
  Remove(state_home)
}

/// Asserts both directions passed the signal exactly (or silence), for two seconds at least.
pub fn assert_loopback(rate: u32, played: &Checker, captured: &Checker) {
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
