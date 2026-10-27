//! Integration test: the PTPv1 follower locks to a (simulated) leader's time and frequency, and leader
//! candidates choose a leader among themselves and hand over to a better one, over real multicast sockets on
//! 127.0.0.1. High ports: 319/320 need privileges on Unix.

use std::{
  net::Ipv4Addr,
  time::{Duration, Instant},
};

use inferno_aoip::clock::{
  ptpv1::{
    test_leader::{TestLeader, TestLeaderConfig},
    Ptpv1ClockSource, Ptpv1Config,
  },
  ClockOverlay, ClockSource,
};
use tokio::sync::watch;

fn configs(event_port: u16) -> (TestLeaderConfig, Ptpv1Config) {
  let lo = Ipv4Addr::LOCALHOST;
  let mut leader = TestLeaderConfig::new(lo);
  leader.event_port = event_port;
  leader.general_port = event_port + 1;
  let mut follower = Ptpv1Config::new(lo, [0x02, 0, 0, 0x11, 0x22, 0x33]);
  follower.event_port = event_port;
  follower.general_port = event_port + 1;
  (leader, follower)
}

#[test]
fn follower_locks_to_leader_time_and_frequency() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
  let (leader_config, follower_config) = configs(31900);
  let leader = TestLeader::start(leader_config).unwrap();
  let follower = Ptpv1ClockSource::start(follower_config).unwrap();
  let mut rx = follower.subscribe();

  let deadline = Instant::now() + Duration::from_secs(15);
  while rx.borrow_and_update().is_none() {
    assert!(Instant::now() < deadline, "no clock from the follower");
    std::thread::sleep(Duration::from_millis(100));
  }
  // published after 2 s of Syncs (8 per second); let the fit window fill a while longer
  std::thread::sleep(Duration::from_secs(8));
  let overlay = rx.borrow_and_update().unwrap();

  let mut worst = 0i64;
  for _ in 0..20 {
    let overlay = rx.borrow_and_update().unwrap();
    let error = overlay.now_ns() - leader.clock().now();
    worst = worst.max(error.abs());
    std::thread::sleep(Duration::from_millis(50));
  }
  println!("overlay {overlay:?}, worst error {worst} ns");
  // software timestamps on a shared CI host: tens of microseconds
  assert!(worst < 200_000, "follower off the leader's time by {worst} ns");
  let freq = rx.borrow().unwrap().freq_scale;
  assert!(
    (freq - 50e-6).abs() < 10e-6,
    "leader runs 50 ppm fast, follower measured {:.1} ppm",
    freq * 1e6
  );
}

#[test]
fn without_a_leader_the_diagnosis_says_so() {
  let (_, follower_config) = configs(31910);
  let follower = Ptpv1ClockSource::start(follower_config).unwrap();
  std::thread::sleep(Duration::from_millis(500));
  assert!(follower.subscribe().borrow().is_none());
  let diagnosis = follower.diagnose_missing();
  assert!(diagnosis.contains("no PTPv1 messages") && diagnosis.contains("Dante"), "{diagnosis}");
}

fn candidate(event_port: u16, uuid_last: u8) -> Ptpv1ClockSource {
  let mut config = Ptpv1Config::new(Ipv4Addr::LOCALHOST, [0x02, 0, 0, 0, 0, uuid_last]);
  config.event_port = event_port;
  config.general_port = event_port + 1;
  config.leader = true;
  config.leader_timeout = Duration::from_secs(1);
  Ptpv1ClockSource::start(config).unwrap()
}

fn wait_until(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
  let deadline = Instant::now() + timeout;
  while !condition() {
    assert!(Instant::now() < deadline, "timed out waiting until {what}");
    std::thread::sleep(Duration::from_millis(50));
  }
}

/// The time of `rx`'s overlay now, once it has one.
fn time(rx: &watch::Receiver<Option<ClockOverlay>>) -> i64 {
  rx.borrow().expect("no clock").now_ns()
}

/// Watches a clock for jumps: compares each overlay published with the previous one, at the same instant.
struct JumpWatch {
  rx: watch::Receiver<Option<ClockOverlay>>,
  previous: Option<ClockOverlay>,
  worst: i64,
}

impl JumpWatch {
  fn new(rx: watch::Receiver<Option<ClockOverlay>>) -> Self {
    Self { rx, previous: None, worst: 0 }
  }
  fn sample(&mut self) {
    let current = *self.rx.borrow_and_update();
    if let (Some(previous), Some(current)) = (self.previous, current) {
      let now = current.now_underlying_ns();
      let jump = current.underlying_to_overlay_ns(now) - previous.underlying_to_overlay_ns(now);
      self.worst = self.worst.max(jump.abs());
    }
    self.previous = current.or(self.previous);
  }
  fn sample_for(&mut self, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end {
      self.sample();
      std::thread::sleep(Duration::from_millis(20));
    }
  }
}

#[test]
fn candidates_settle_on_one_leader() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
  let a = candidate(31960, 9);
  let b = candidate(31960, 1);
  let (ra, rb) = (a.subscribe(), b.subscribe());
  // both may lead at first; the worse one steps down when it hears the other
  wait_until("one leader and both locked", Duration::from_secs(15), || {
    a.is_leading() != b.is_leading() && ra.borrow().is_some() && rb.borrow().is_some()
  });
  std::thread::sleep(Duration::from_secs(4));
  assert!(b.is_leading() && !a.is_leading(), "the lower identity leads");
  let mut worst = 0i64;
  for _ in 0..20 {
    worst = worst.max((time(&ra) - time(&rb)).abs());
    std::thread::sleep(Duration::from_millis(50));
  }
  assert!(worst < 200_000, "the follower is off the leader's time by {worst} ns");
}

#[test]
fn candidate_hands_over_to_a_better_leader_and_takes_back() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
  let (leader_config, _) = configs(31970);
  let device = candidate(31970, 5);
  let rx = device.subscribe();
  wait_until("the candidate leads, without a leader", Duration::from_secs(5), || {
    device.is_leading() && rx.borrow().is_some()
  });

  // a Dante device (better ranked) appears: follow it
  let leader = TestLeader::start(leader_config).unwrap();
  wait_until("the candidate follows the better leader", Duration::from_secs(5), || !device.is_leading());
  wait_until("locked to the leader", Duration::from_secs(15), || {
    (time(&rx) - leader.clock().now()).abs() < 200_000
  });
  std::thread::sleep(Duration::from_secs(3));

  // it disappears: lead again, carrying on with its time
  let mut jumps = JumpWatch::new(rx.clone());
  jumps.sample();
  let leader_clock = leader.clock();
  drop(leader);
  let silent = Instant::now();
  wait_until("the candidate leads again", Duration::from_secs(5), || {
    jumps.sample();
    device.is_leading()
  });
  assert!(silent.elapsed() > Duration::from_millis(900), "took over before the leader timed out");
  jumps.sample_for(Duration::from_millis(500));
  assert!(jumps.worst < 200_000, "the clock jumped by {} ns when taking over", jumps.worst);
  let drift = (time(&rx) - leader_clock.now()).abs();
  assert!(drift < 1_000_000, "{drift} ns off the time it carried on with");
}

#[test]
fn candidate_takes_over_from_a_worse_leader_without_a_jump() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
  let worse = candidate(31980, 9);
  wait_until("the first candidate leads", Duration::from_secs(5), || worse.is_leading());
  let mut follower_config = Ptpv1Config::new(Ipv4Addr::LOCALHOST, [0x02, 0, 0, 0, 0, 0x42]);
  follower_config.event_port = 31980;
  follower_config.general_port = 31981;
  let follower = Ptpv1ClockSource::start(follower_config).unwrap();
  let mut follower_jumps = JumpWatch::new(follower.subscribe());
  wait_until("the follower is locked", Duration::from_secs(15), || {
    follower_jumps.sample();
    follower_jumps.previous.is_some()
  });
  follower_jumps.sample_for(Duration::from_secs(3));
  let worst_before = follower_jumps.worst;

  // a better candidate locks to the leader first, then takes over with its time
  let mut worse_jumps = JumpWatch::new(worse.subscribe());
  worse_jumps.sample();
  let better = candidate(31980, 1);
  wait_until("the better candidate takes over", Duration::from_secs(15), || {
    worse_jumps.sample();
    follower_jumps.sample();
    better.is_leading() && !worse.is_leading()
  });
  // the worse one and the follower lock to the new leader
  for _ in 0..(6000 / 20) {
    worse_jumps.sample();
    follower_jumps.sample();
    std::thread::sleep(Duration::from_millis(20));
  }
  println!(
    "jumps: worse {} ns, follower {} ns (before {worst_before} ns)",
    worse_jumps.worst, follower_jumps.worst
  );
  assert!(worse_jumps.worst < 500_000, "the former leader's clock jumped by {} ns", worse_jumps.worst);
  assert!(follower_jumps.worst < 500_000, "the follower's clock jumped by {} ns", follower_jumps.worst);
  let (rb, rf) = (better.subscribe(), follower.subscribe());
  assert!((time(&rb) - time(&rf)).abs() < 200_000);
}
