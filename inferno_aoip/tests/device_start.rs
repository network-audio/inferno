//! Integration test: a device that can't get its network ports fails to start with a clear error, instead of
//! running without them (invisible to controllers) or crashing.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use inferno_aoip::device_server::{DeviceServer, Settings};

fn settings(name: &str, process_id: u16, alt_port: u16) -> Settings {
  let mut config = BTreeMap::new();
  config.insert("NAME".to_string(), name.to_string());
  config.insert("PROCESS_ID".to_string(), process_id.to_string());
  config.insert("ALT_PORT".to_string(), alt_port.to_string());
  config.insert("CLOCK_PATH".to_string(), format!("/tmp/inferno-device-start-{alt_port}-nonexistent"));
  Settings::new(name, name, Some(Ipv4Addr::LOCALHOST), &config)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn port_in_use_fails_start_with_an_error_naming_it() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
  let first =
    DeviceServer::try_start(settings("start_a", 1, 9700)).await.expect("first device must start");
  for clash in [9700, 9702] {
    let error = match DeviceServer::try_start(settings("start_b", 2, clash)).await {
      Ok(_) => panic!("second device started on ports in use (ALT_PORT {clash})"),
      Err(e) => e.to_string(),
    };
    assert!(error.contains("ALT_PORT"), "error doesn't say how to fix it: {error}");
    assert!(error.contains("127.0.0.1:97"), "error doesn't name the address: {error}");
  }
  // the ports are free again once the failed devices are gone, and the first device is unaffected
  first.shutdown().await;
  let again = DeviceServer::try_start(settings("start_c", 3, 9700)).await.expect("ports not released");
  again.shutdown().await;
}
