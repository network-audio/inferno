//! Integration test: loopback TX→RX audio transmission without Docker.
//!
//! Two Inferno instances run on the same host using different ALT_PORT/PROCESS_ID:
//! - TX device transmits a deterministic ramp pattern on 2 channels
//! - RX device receives and verifies the exact payload
//!
//! The media clock comes from a fake usrvclock server (Unix), or from a free-running clock source shared by
//! both devices (any platform).

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use inferno_aoip::clock::FreeRunningClockSource;
use inferno_aoip::device_server::{
  AtomicSample, DeviceServer, ExternalBufferParameters, MediaClock, Sample, Settings, StreamAnchor,
  TransferNotifier,
};

const BUF_SIZE: usize = 65536;
const SAMPLE_RATE: u32 = 48000;

fn make_settings(
  name: &str,
  process_id: u16,
  alt_port: u16,
  rx_channels: usize,
  tx_channels: usize,
  clock_path: Option<&str>,
) -> Settings {
  let mut config = BTreeMap::new();
  config.insert("NAME".to_string(), name.to_string());
  config.insert("PROCESS_ID".to_string(), process_id.to_string());
  config.insert("ALT_PORT".to_string(), alt_port.to_string());
  config.insert("RX_CHANNELS".to_string(), rx_channels.to_string());
  config.insert("TX_CHANNELS".to_string(), tx_channels.to_string());
  config.insert("SAMPLE_RATE".to_string(), SAMPLE_RATE.to_string());
  if let Some(clock_path) = clock_path {
    config.insert("CLOCK_PATH".to_string(), clock_path.to_string());
  }
  config.insert("BIND_IP".to_string(), "127.0.0.1".to_string());

  let mut settings = Settings::new(name, name, Some(Ipv4Addr::new(127, 0, 0, 1)), &config);
  settings.make_rx_channels(rx_channels);
  settings.make_tx_channels(tx_channels);
  settings
}

#[cfg(unix)]
fn start_clock_server(path: &str, running: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
  let path = path.to_string();
  std::thread::spawn(move || {
    let mut server = usrvclock::Server::new(path.into()).unwrap();
    let overlay = usrvclock::ClockOverlay {
      clock_id: 1i64, // CLOCK_MONOTONIC
      last_sync: 0,
      shift: 0,
      freq_scale: 0.0,
    };
    while running.load(Ordering::Relaxed) {
      server.send(overlay);
      std::thread::sleep(Duration::from_millis(100));
    }
  })
}

async fn wait_for_clock(
  mut clock_rx: inferno_aoip::device_server::RealTimeClockReceiver,
  sample_rate: u32,
) -> usize {
  let mut media_clock = MediaClock::new(false);
  loop {
    clock_rx.update();
    if let Some(overlay) = clock_rx.get() {
      media_clock.update_overlay(*overlay);
      if let Some(now) = media_clock.wrapping_now_in_timebase(sample_rate as u64) {
        return now as usize;
      }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
}

/// Compares `b` with the ring buffer `a` starting at `a_start`, wrapping around its end.
#[inline(always)]
fn compare_samples(a: &[AtomicSample], a_start: usize, b: &[Sample]) -> bool {
  let mut mindiff = Sample::MAX;
  let mut maxdiff = Sample::MIN;
  for (i, b) in b.iter().enumerate() {
    let a = a[(a_start + i) % a.len()].load(atomic::Ordering::Relaxed);
    let diff = a.saturating_sub(*b);
    if diff.saturating_abs() > 384 {
      return false;
    }
    if diff > maxdiff {
      maxdiff = diff;
    }
    if diff < mindiff {
      mindiff = diff;
    }
  }
  log::debug!("diff: {mindiff}..{maxdiff}");
  true
}

/// Position of `b` in the ring buffer `a`.
fn find_samples_offset(a: &[AtomicSample], b: &[Sample]) -> Option<usize> {
  (0..a.len()).find(|&offset| compare_samples(a, offset, b))
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_loopback_trx() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();

  let clock_path = "/tmp/inferno-test-usrvclock";
  let _ = std::fs::remove_file(clock_path);

  let clock_running = Arc::new(AtomicBool::new(true));
  let clock_thread = start_clock_server(clock_path, clock_running.clone());
  tokio::time::sleep(Duration::from_millis(300)).await;

  let tx_settings = make_settings("test_tx", 1, 9000, 0, 2, Some(clock_path));
  let rx_settings = make_settings("test_rx", 2, 9100, 2, 0, Some(clock_path));

  let tx_server = DeviceServer::start(tx_settings).await;
  let rx_server = DeviceServer::start(rx_settings).await;
  loopback(tx_server, rx_server).await;

  clock_running.store(false, Ordering::Relaxed);
  let _ = clock_thread.join();
  let _ = std::fs::remove_file(clock_path);
}

/// Devices given a clock source by the application (instead of `CLOCK_PATH`) share its clock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_loopback_trx_with_given_clock_source() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();

  let clock = Arc::new(FreeRunningClockSource::new());
  let tx_settings = make_settings("test_tx_given_clock", 3, 9200, 0, 2, None);
  let rx_settings = make_settings("test_rx_given_clock", 4, 9300, 2, 0, None);
  let tx_server = DeviceServer::try_start_with_clock(tx_settings, clock.clone()).await.unwrap();
  let rx_server = DeviceServer::try_start_with_clock(rx_settings, clock).await.unwrap();
  loopback(tx_server, rx_server).await;
}

/// Transmits from `tx_server` to `rx_server` and verifies the received samples.
async fn loopback(mut tx_server: DeviceServer, mut rx_server: DeviceServer) {
  let tx_hostname = tx_server.self_info.friendly_hostname.clone();

  let tx_clock_rx = tx_server.get_realtime_clock_receiver();
  let rx_clock_rx = rx_server.get_realtime_clock_receiver();
  let tx_start_time = wait_for_clock(tx_clock_rx, SAMPLE_RATE).await;
  let _rx_start_time = wait_for_clock(rx_clock_rx, SAMPLE_RATE).await;

  let mut rng = SmallRng::from_rng(rand::thread_rng()).unwrap();

  let tx_buffers: Vec<Arc<Vec<AtomicSample>>> = (0..2)
    .map(move |_| Arc::new((0..BUF_SIZE).map(|_| AtomicSample::new(rng.gen::<Sample>())).collect()))
    .collect();

  for ch in 0..2 {
    let buf = &tx_buffers[ch];
    std::mem::forget(buf.clone()); // TODO: need to pin, too???
                                   // include some edge cases:
    buf[BUF_SIZE / 2].store(Sample::MIN, Ordering::Relaxed);
    buf[BUF_SIZE / 2 + 1].store(Sample::MAX, Ordering::Relaxed);
  }
  atomic::fence(Ordering::SeqCst);

  let tx_external_params: Vec<ExternalBufferParameters<Sample>> = (0..2)
    .map(|ch| {
      let buf = tx_buffers[ch].clone();
      unsafe {
        ExternalBufferParameters::new(
          buf.as_ptr(),
          buf.len(),
          1,
          Arc::new(RwLock::new(true)),
          None, // unconditional read
        )
      }
    })
    .collect();

  let tx_anchor = StreamAnchor::new_paused();
  let tx_current_timestamp: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(usize::MAX));

  tx_server
    .transmit_from_external_buffer(
      tx_external_params,
      tx_anchor.clone(),
      tx_current_timestamp.clone(),
      Some(TransferNotifier { callback: Box::new(|| {}), max_interval_samples: 480 }),
    )
    .await;

  let offset = Arc::new(AtomicUsize::new(0));
  let has_offset = Arc::new(AtomicBool::new(false));
  let total_samples_per_channel = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);

  let rx_callback = {
    let offset = offset.clone();
    let has_offset = has_offset.clone();
    let total_samples_per_channel = total_samples_per_channel.clone();
    let mut has_ch = [false; 2];

    move |samples_count: usize, channels: &Vec<Vec<Sample>>| {
      let mut start_pos_per_channel = [0; 2];
      for (chi, samples) in channels.iter().enumerate() {
        if !has_ch[chi] {
          for (i, sample) in samples[..samples_count].iter().enumerate() {
            if *sample != 0 {
              has_ch[chi] = true;
              start_pos_per_channel[chi] = i;
              break;
            }
          }
        }
      }
      if (!has_offset.load(atomic::Ordering::Relaxed)) && has_ch[0] {
        let found_offset =
          find_samples_offset(&tx_buffers[0], &channels[0][..samples_count][start_pos_per_channel[0]..]);
        if found_offset.is_none() {
          panic!("compared only non-zero samples but failed to find offset");
        }
        offset.store(
          (found_offset.unwrap() + BUF_SIZE - start_pos_per_channel[0]) % BUF_SIZE,
          atomic::Ordering::Relaxed,
        );
        has_offset.store(true, atomic::Ordering::Relaxed);
      }
      if has_offset.load(atomic::Ordering::Relaxed) {
        // the transmitter reads its buffer as a ring
        let offset_v = offset.load(atomic::Ordering::Relaxed);
        for (chi, samples) in channels.iter().enumerate() {
          if !has_ch[chi] {
            continue;
          }
          let start = start_pos_per_channel[chi];
          assert!(compare_samples(&tx_buffers[chi], offset_v + start, &samples[start..samples_count]));
          total_samples_per_channel[chi].fetch_add(samples_count - start, atomic::Ordering::Relaxed);
        }
        offset.store((offset_v + samples_count) % BUF_SIZE, atomic::Ordering::Relaxed);
      }
    }
  };

  rx_server.receive_with_callback(Box::new(rx_callback)).await;

  tx_anchor.start(tx_start_time);
  rx_server.subscribe(0, "TX 1", &tx_hostname).await;
  rx_server.subscribe(1, "TX 2", &tx_hostname).await;

  // wait for more than a whole buffer (so its wrap-around is verified too) instead of a fixed time:
  // startup (subscription, first packets) varies
  let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
  while total_samples_per_channel.iter().any(|t| t.load(Ordering::Relaxed) <= BUF_SIZE * 5 / 4)
    && tokio::time::Instant::now() < deadline
  {
    tokio::time::sleep(Duration::from_millis(50)).await;
  }

  rx_server.shutdown().await;
  tx_server.shutdown().await;

  atomic::fence(Ordering::SeqCst);

  log::info!(
    "offset: {:?}, total_samples_per_channel: {:?}",
    offset.load(Ordering::Relaxed),
    total_samples_per_channel
  );
  assert!(has_offset.load(atomic::Ordering::Relaxed));
  assert!(total_samples_per_channel[0].load(Ordering::Relaxed) > BUF_SIZE * 5 / 4);
  assert!(total_samples_per_channel[1].load(Ordering::Relaxed) > BUF_SIZE * 5 / 4);
}
