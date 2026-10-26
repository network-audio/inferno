//! Simulated PTPv1 leader for tests without Dante hardware (see `clock::ptpv1::test_leader`): sends Sync and
//! Follow_Up on the standard PTP ports and answers Delay_Req, until killed. Not a usable PTP leader.
//!
//! Usage: ptpv1_test_leader <interface address>

use std::net::Ipv4Addr;

use inferno_aoip::clock::ptpv1::test_leader::{TestLeader, TestLeaderConfig};

fn main() {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
  let Some(interface) = std::env::args().nth(1).and_then(|a| a.parse::<Ipv4Addr>().ok()) else {
    eprintln!("usage: ptpv1_test_leader <interface address>");
    std::process::exit(2);
  };
  let _leader = TestLeader::start(TestLeaderConfig::new(interface)).unwrap_or_else(|e| {
    eprintln!("can't start the test leader on {interface}: {e}");
    std::process::exit(1);
  });
  log::info!("simulated PTPv1 leader on {interface}");
  loop {
    std::thread::park();
  }
}
