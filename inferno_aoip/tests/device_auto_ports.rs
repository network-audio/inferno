//! Integration test: a second device on a host, without ALT_PORT and PROCESS_ID, picks its own free ports
//! and a process ID (so its device ID is unique), the same ones again when restarted.
//! In its own test binary: it uses the standard Dante ports.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use inferno_aoip::device_server::{DeviceServer, Settings};

fn settings(name: &str) -> Settings {
  let mut config = BTreeMap::new();
  config.insert("NAME".to_string(), name.to_string());
  config.insert("CLOCK_PATH".to_string(), "/tmp/inferno-auto-ports-nonexistent".to_string());
  Settings::new(name, name, Some(Ipv4Addr::LOCALHOST), &config)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_device_picks_its_own_ports_and_process_id() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
  let first = DeviceServer::try_start(settings("auto_first")).await.expect("first device");
  assert_eq!(first.self_info.arc_port, 4440, "the first device keeps the standard ports");
  assert_eq!(first.self_info.process_id, 0, "and its process ID");

  let second = DeviceServer::try_start(settings("auto_second")).await.expect("second device must start");
  let info = second.self_info.clone();
  assert_ne!(info.arc_port, 4440, "second device on the standard ports");
  assert_eq!(
    [info.cmc_port, info.flows_control_port, info.info_request_port],
    [info.arc_port + 1, info.arc_port + 2, info.arc_port + 3]
  );
  assert_ne!(info.process_id, 0);
  assert_ne!(info.factory_device_id, first.self_info.factory_device_id, "device IDs must differ");

  // restarted, it gets the same identity (its saved state is found again)
  second.shutdown().await;
  let again = DeviceServer::try_start(settings("auto_second")).await.expect("restart");
  assert_eq!(
    (again.self_info.arc_port, again.self_info.factory_device_id),
    (info.arc_port, info.factory_device_id)
  );
  again.shutdown().await;
  first.shutdown().await;
}
