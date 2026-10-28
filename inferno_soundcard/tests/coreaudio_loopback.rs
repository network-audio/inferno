//! A sound card on any platform, as the Core Audio plug-in uses it: a client in place of Core Audio runs I/O
//! cycles by the card's clock, reading input and writing the mix through the region (`CardIo`, as the
//! plug-in does), in 32-bit floats. Another Inferno device transmits a known signal to the card's recording
//! channels; the card's playback channels are received by a third. All follow one free-running clock.

mod common;

use std::{
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
  },
  time::{Duration, Instant},
};

use common::{assert_loopback, own_state_home, signal, Checker, Loopback, CHANNELS};
use inferno_soundcard::engine::EngineClock;
use inferno_soundcard_shm::{f32_to_sample, sample_to_f32, CardIo, Region};

/// Frames per I/O cycle, and Core Audio's safety offset (as the plug-in reports it).
const IO_FRAMES: usize = 512;
const SAFETY: usize = 32;

/// Runs I/O cycles as Core Audio does: woken when the clock reaches `wake`, it reads the input frames that
/// have passed (by the safety offset) and writes the mix of frames ahead (by the safety offset), until `stop`.
fn core_audio(
  region: Arc<Region>,
  clock: Arc<EngineClock>,
  stop: Arc<AtomicBool>,
  captured: Arc<Mutex<Checker>>,
) {
  let card = CardIo::new(&region, 0).unwrap();
  let origin = loop {
    if let Some(now) = clock.position() {
      break now;
    }
    std::thread::sleep(Duration::from_millis(1));
  };
  let mut input = vec![0.0f32; IO_FRAMES * CHANNELS];
  let mut mix = vec![0.0f32; IO_FRAMES * CHANNELS];
  let mut cycle = 0;
  while !stop.load(Ordering::Relaxed) {
    let sample_time = cycle * IO_FRAMES;
    let wake = origin + sample_time + IO_FRAMES + SAFETY;
    while clock.position().is_none_or(|now| (now as isize - wake as isize) < 0) {
      std::thread::sleep(Duration::from_micros(500));
    }
    card.read_input(origin + sample_time, IO_FRAMES, &mut input);
    {
      let mut checker = captured.lock().unwrap();
      for (i, &s) in input.iter().enumerate() {
        checker.check(i % CHANNELS, f32_to_sample(s));
      }
    }
    // the mix plays the signal from its start
    for (i, s) in mix.iter_mut().enumerate() {
      *s = sample_to_f32(signal(i % CHANNELS, (sample_time + i / CHANNELS) as u64));
    }
    let output_at = origin + sample_time + IO_FRAMES + 2 * SAFETY;
    card.write_mix(output_at, IO_FRAMES, &mix, clock.position());
    cycle += 1;
  }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// macOS CI runners stall Inferno's flows for tens of milliseconds at a time (see loopback_trx there): the
// macOS end-to-end test plays and records through Core Audio itself; Linux and Windows CI run this
#[cfg_attr(target_os = "macos", ignore = "timing-sensitive, macOS runners stall too much")]
async fn plays_and_records_through_the_region() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
  let _state = own_state_home();
  for (i, rate) in [48000, 44100].into_iter().enumerate() {
    println!("== {rate} Hz");
    plays_and_records_at(rate, 10 + i as u16).await;
  }
}

async fn plays_and_records_at(rate: u32, run: u16) {
  let devices = Loopback::start("coreaudio_test", rate, run).await;
  let stop = Arc::new(AtomicBool::new(false));
  let captured = Arc::new(Mutex::new(Checker::default()));
  let client = {
    let (region, clock) = (devices.engine.region().clone(), devices.clock.clone());
    let (stop, captured) = (stop.clone(), captured.clone());
    std::thread::spawn(move || core_audio(region, clock, stop, captured))
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
  devices.finish(()).await;
  assert_loopback(rate, &played.lock().unwrap(), &captured.lock().unwrap());
}
