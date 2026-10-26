//! Integration test: the PTPv1 follower locks to a (simulated) leader's time and frequency, over real
//! multicast sockets on 127.0.0.1. High ports: 319/320 need privileges on Unix.

use std::{
  net::Ipv4Addr,
  time::{Duration, Instant},
};

use inferno_aoip::clock::{
  ptpv1::{
    test_leader::{TestLeader, TestLeaderConfig},
    Ptpv1ClockSource, Ptpv1Config,
  },
  ClockSource,
};

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
