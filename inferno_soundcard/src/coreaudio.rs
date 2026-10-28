//! The cards as Core Audio devices (macOS): the engine's rings are in Mach shared memory, which the Core Audio
//! plug-in (`inferno_coreaudio`, loaded by `coreaudiod`) gets from this service's Mach service, and plays and
//! records directly. This process publishes the media clock there, for the plug-in to tell Core Audio when
//! each frame is due, and a heartbeat, for the plug-in to tell when the service is gone.

use std::{
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
  },
  thread::JoinHandle,
  time::Duration,
};

use inferno_soundcard_shm::{
  mach::{self, SharedMemory},
  underlying_now_ns, Region, State,
};
use log::{error, info, warn};

use crate::{
  app::{CardStatus, SharedStatus},
  config::Config,
  engine::{publish_clock, Allocate, Engine, EngineClock},
};

/// The state while the plug-in hasn't asked for the cards yet.
const WAITING_FOR_CORE_AUDIO: &str = "running, waiting for Core Audio to load the Inferno plug-in";

/// The region the Mach service hands out.
static CURRENT: Mutex<Option<Arc<Region>>> = Mutex::new(None);
/// why the Mach service doesn't run, if it doesn't
static SERVICE_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// The rings are in new Mach shared memory.
pub fn allocate() -> Box<Allocate> {
  Box::new(|cards, params| {
    let size = Region::layout_for(cards, params.ring_frames).size;
    let memory = SharedMemory::create(size)?;
    let (ptr, len) = (memory.ptr(), memory.len());
    // safety: new zeroed memory, page aligned, owned by the region
    unsafe { Region::create(ptr, len, Box::new(memory), cards, params) }
  })
}

fn memory_of(region: &Region) -> &SharedMemory {
  region.owner().downcast_ref::<SharedMemory>().expect("the region is in Mach shared memory")
}

/// Answers the plug-in's requests for the cards' region, in this thread, forever. The service must have been
/// started by launchd, with its property list declaring the Mach service.
pub fn serve(status: SharedStatus) {
  let service = match mach::Service::check_in(mach::SERVICE_NAME) {
    Ok(service) => service,
    Err(e) => {
      error!("{e}");
      *SERVICE_ERROR.lock().unwrap() = Some(e);
      return;
    }
  };
  info!("Mach service {} ready for the Core Audio plug-in", mach::SERVICE_NAME);
  loop {
    let mut served = None;
    let result = service.serve_one(1000, || {
      let current = CURRENT.lock().unwrap();
      let region = current.as_ref()?;
      let memory = memory_of(region);
      served = Some(region.header().generation);
      Some((memory.entry(), memory.len() as u64))
    });
    match result {
      Ok(Some(uid)) => match served {
        Some(generation) => {
          info!("the Core Audio plug-in (user {uid}) got the cards (region {generation:x})");
          let mut status = status.lock().unwrap();
          if status.state == WAITING_FOR_CORE_AUDIO {
            status.state = "running".into();
          }
        }
        None => info!("the Core Audio plug-in (user {uid}) asked for the cards: not running yet"),
      },
      Ok(None) => {}
      Err(e) => warn!("Mach service: {e}"),
    }
  }
}

/// The cards, as long as this lives: the clock and heartbeat published.
pub struct Exported {
  stop: Arc<AtomicBool>,
  thread: Option<JoinHandle<()>>,
  region: Arc<Region>,
}

impl Drop for Exported {
  fn drop(&mut self) {
    *CURRENT.lock().unwrap() = None;
    self.region.header().set_state(State::Closed);
    self.stop.store(true, Ordering::Relaxed);
    if let Some(thread) = self.thread.take() {
      let _ = thread.join();
    }
  }
}

/// Whether a count went from `before` to `now` past a power of 2.
fn crossed(before: u64, now: u64) -> bool {
  now > before && (before == 0 || now.ilog2() > before.ilog2())
}

/// Warns about counts of late playback or unreceived capture reaching a power of 2.
fn warn_counts(region: &Region, last: &mut (u64, u64)) {
  let header = region.header();
  let late = header.late_playback.load(Ordering::Relaxed);
  let unreceived = header.unreceived_capture.load(Ordering::Relaxed);
  if crossed(last.0, late) {
    warn!(
      "playback arrived after it was due: transmitted as silence ({late} times so far). Raise the \
       playback margin if it goes on"
    );
  }
  if crossed(last.1, unreceived) {
    warn!("capture not received in time, silence instead ({unreceived} times so far)");
  }
  *last = (late, unreceived);
}

/// Hands the cards of the started `engine` to the plug-in. Returns them and the state to show.
pub fn export(
  engine: &Engine,
  clock: Arc<EngineClock>,
  _config: &Config,
  _device_id: &str,
  _cards: &mut [CardStatus],
  _status: &SharedStatus,
) -> Result<(Exported, String), String> {
  let region = engine.region().clone();
  let stop = Arc::new(AtomicBool::new(false));
  let thread = {
    let (region, stop) = (region.clone(), stop.clone());
    std::thread::Builder::new()
      .name("core audio clock".into())
      .spawn(move || {
        let mut ticks = 0u64;
        let mut counts = (0, 0);
        while !stop.load(Ordering::Relaxed) {
          publish_clock(&clock, &region);
          let now = underlying_now_ns(libc::CLOCK_MONOTONIC as i64);
          region.header().heartbeat_ns.store(now as u64, Ordering::Release);
          ticks += 1;
          if ticks % 100 == 0 {
            warn_counts(&region, &mut counts);
          }
          std::thread::sleep(Duration::from_millis(10));
        }
      })
      .map_err(|e| e.to_string())?
  };
  *CURRENT.lock().unwrap() = Some(region.clone());
  let state = match &*SERVICE_ERROR.lock().unwrap() {
    Some(e) => format!("running, but Core Audio can't get the cards: {e}"),
    None => WAITING_FOR_CORE_AUDIO.into(),
  };
  Ok((Exported { stop, thread: Some(thread), region }, state))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn counts_are_warned_about_at_powers_of_two() {
    assert!(crossed(0, 1));
    assert!(crossed(1, 2));
    assert!(!crossed(2, 3));
    assert!(crossed(3, 4));
    assert!(crossed(3, 9));
    assert!(!crossed(4, 4));
  }
}
