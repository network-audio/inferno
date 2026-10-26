//! Integration tests: receive channels end up silent when their flow goes away.
//!
//! When the last channel of a flow is unsubscribed, or the transmitter disappears, the flow and its socket are
//! removed. The channels' buffers must then be filled with silence, not keep the last audio.

#![cfg(unix)] // fake usrvclock server

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use inferno_aoip::device_server::{
  AtomicSample, DeviceServer, ExternalBufferParameters, MediaClock, Sample, Settings, StreamAnchor,
};

const SAMPLE_RATE: u32 = 48000;
const RX_BUF_SIZE: usize = 4096;
const TX_BUF_SIZE: usize = 65536;
const CHANNELS: usize = 2;

fn make_settings(
  name: &str,
  process_id: u16,
  alt_port: u16,
  rx: usize,
  tx: usize,
  clock_path: &str,
) -> Settings {
  let mut config = BTreeMap::new();
  config.insert("NAME".to_string(), name.to_string());
  config.insert("PROCESS_ID".to_string(), process_id.to_string());
  config.insert("ALT_PORT".to_string(), alt_port.to_string());
  config.insert("RX_CHANNELS".to_string(), rx.to_string());
  config.insert("TX_CHANNELS".to_string(), tx.to_string());
  config.insert("SAMPLE_RATE".to_string(), SAMPLE_RATE.to_string());
  config.insert("CLOCK_PATH".to_string(), clock_path.to_string());
  config.insert("BIND_IP".to_string(), "127.0.0.1".to_string());
  let mut settings = Settings::new(name, name, Some(Ipv4Addr::new(127, 0, 0, 1)), &config);
  settings.make_rx_channels(rx);
  settings.make_tx_channels(tx);
  settings
}

fn start_clock_server(path: &str, running: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
  let path = path.to_string();
  std::thread::spawn(move || {
    let mut server = usrvclock::Server::new(path.into()).unwrap();
    let overlay = usrvclock::ClockOverlay { clock_id: 1i64, last_sync: 0, shift: 0, freq_scale: 0.0 };
    while running.load(Ordering::Relaxed) {
      server.send(overlay);
      std::thread::sleep(Duration::from_millis(100));
    }
  })
}

async fn wait_for_clock(mut clock_rx: inferno_aoip::device_server::RealTimeClockReceiver) -> usize {
  let mut media_clock = MediaClock::new(false);
  loop {
    clock_rx.update();
    if let Some(overlay) = clock_rx.get() {
      media_clock.update_overlay(*overlay);
      if let Some(now) = media_clock.wrapping_now_in_timebase(SAMPLE_RATE as u64) {
        return now as usize;
      }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
}

fn external_buffers(
  bufs: &[Arc<Vec<AtomicSample>>],
  valid: &Arc<RwLock<bool>>,
) -> Vec<ExternalBufferParameters<Sample>> {
  bufs
    .iter()
    .map(|buf| unsafe { ExternalBufferParameters::new(buf.as_ptr(), buf.len(), 1, valid.clone(), None) })
    .collect()
}

fn nonzero(buf: &[AtomicSample]) -> usize {
  buf.iter().filter(|s| s.load(Ordering::Relaxed) != 0).count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsubscribed_channels_are_silent() {
  channels_end_silent(false, 9300).await;
}

/// The transmitter disappears: its channels get orphaned and the flow removed, while still subscribed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn orphaned_channels_are_silent() {
  channels_end_silent(true, 9500).await;
}

async fn channels_end_silent(transmitter_disappears: bool, base_port: u16) {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
  let clock_path = &format!("/tmp/inferno-test-unsubscribe-{base_port}-usrvclock");
  let _ = std::fs::remove_file(clock_path);
  let clock_running = Arc::new(AtomicBool::new(true));
  let clock_thread = start_clock_server(clock_path, clock_running.clone());
  tokio::time::sleep(Duration::from_millis(300)).await;

  let tx_name = format!("unsub_tx{base_port}");
  let rx_name = format!("unsub_rx{base_port}");
  let mut tx_server =
    DeviceServer::start(make_settings(&tx_name, 1, base_port, 0, CHANNELS, clock_path)).await;
  let mut rx_server =
    DeviceServer::start(make_settings(&rx_name, 2, base_port + 100, CHANNELS, 0, clock_path)).await;
  let tx_hostname = tx_server.self_info.friendly_hostname.clone();
  let tx_start = wait_for_clock(tx_server.get_realtime_clock_receiver()).await;
  let rx_start = wait_for_clock(rx_server.get_realtime_clock_receiver()).await;

  // transmit constant non-zero audio
  let tx_bufs: Vec<Arc<Vec<AtomicSample>>> = (0..CHANNELS)
    .map(|_| Arc::new((0..TX_BUF_SIZE).map(|_| AtomicSample::new(0x1234_5600)).collect()))
    .collect();
  let tx_valid = Arc::new(RwLock::new(true));
  let tx_anchor = StreamAnchor::new_paused();
  tx_server
    .transmit_from_external_buffer(
      external_buffers(&tx_bufs, &tx_valid),
      tx_anchor.clone(),
      Default::default(),
      None,
    )
    .await;
  tx_anchor.start(tx_start);

  let rx_bufs: Vec<Arc<Vec<AtomicSample>>> =
    (0..CHANNELS).map(|_| Arc::new((0..RX_BUF_SIZE).map(|_| AtomicSample::new(0)).collect())).collect();
  let rx_valid = Arc::new(RwLock::new(false));
  let rx_anchor = StreamAnchor::new_paused_guarding(rx_valid.clone());
  rx_server
    .receive_to_external_buffer(
      external_buffers(&rx_bufs, &rx_valid),
      rx_anchor.clone(),
      Arc::new(AtomicUsize::new(usize::MAX)),
      None,
    )
    .await;
  rx_anchor.start(rx_start);
  for ch in 0..CHANNELS {
    rx_server.subscribe(ch, &format!("TX {}", ch + 1), &tx_hostname).await;
  }
  let mut waited = Duration::ZERO;
  while rx_bufs.iter().any(|b| nonzero(b) < RX_BUF_SIZE / 2) {
    assert!(waited < Duration::from_secs(30), "no audio received");
    tokio::time::sleep(Duration::from_millis(50)).await;
    waited += Duration::from_millis(50);
  }

  let tx_server = if transmitter_disappears {
    tx_server.shutdown().await;
    // until the receiver has given up on the flow
    tokio::time::sleep(Duration::from_secs(8)).await;
    None
  } else {
    for ch in 0..CHANNELS {
      rx_server.unsubscribe(ch).await;
    }
    // many buffer cycles later, the buffers must hold silence
    tokio::time::sleep(Duration::from_millis(1000)).await;
    Some(tx_server)
  };
  let left: Vec<usize> = rx_bufs.iter().map(|b| nonzero(b)).collect();
  log::info!("non-silent samples left after unsubscribing: {left:?}");

  rx_server.shutdown().await;
  if let Some(tx_server) = tx_server {
    tx_server.shutdown().await;
  }
  clock_running.store(false, Ordering::Relaxed);
  let _ = clock_thread.join();
  let _ = std::fs::remove_file(clock_path);
  assert!(left.iter().all(|&n| n == 0), "unsubscribed channels kept audio: {left:?} non-silent samples");
}
