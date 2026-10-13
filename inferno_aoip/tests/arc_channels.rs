//! Integration test: ARC channel queries answered by a running device.
//!
//! Controllers (e.g. netaudio) read the channel counts first and then page through the channels,
//! and reject devices whose pages don't match the counts.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, UdpSocket};
use std::time::Duration;

use inferno_aoip::device_server::{DeviceServer, Settings};

const ARC_START_CODE: u16 = 0x27ff;
const GET_RECEIVE_CHANNELS: u16 = 0x3000;
const HEADER_LENGTH: usize = 10;

fn make_settings(
  name: &str,
  process_id: u16,
  alt_port: u16,
  rx_channels: usize,
  tx_channels: usize,
) -> Settings {
  let mut config = BTreeMap::new();
  config.insert("NAME".to_string(), name.to_string());
  config.insert("PROCESS_ID".to_string(), process_id.to_string());
  config.insert("ALT_PORT".to_string(), alt_port.to_string());
  config.insert("RX_CHANNELS".to_string(), rx_channels.to_string());
  config.insert("TX_CHANNELS".to_string(), tx_channels.to_string());
  config.insert("SAMPLE_RATE".to_string(), "48000".to_string());
  config.insert("BIND_IP".to_string(), "127.0.0.1".to_string());

  let mut settings = Settings::new(name, name, Some(Ipv4Addr::new(127, 0, 0, 1)), &config);
  settings.make_rx_channels(rx_channels);
  settings.make_tx_channels(tx_channels);
  settings
}

/// Sends an ARC paginated request for the page starting at channel 1, returns the response content.
fn arc_request_first_page(arc_port: u16, opcode: u16) -> Vec<u8> {
  let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
  socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
  let content = [0u8, 0, 0, 1];
  let mut packet = Vec::new();
  for word in [ARC_START_CODE, (HEADER_LENGTH + content.len()) as u16, 1, opcode, 0] {
    packet.extend_from_slice(&word.to_be_bytes());
  }
  packet.extend_from_slice(&content);
  socket.send_to(&packet, ("127.0.0.1", arc_port)).unwrap();

  let mut buf = [0u8; 1500];
  let len = socket.recv(&mut buf).expect("no ARC response");
  assert!(len >= HEADER_LENGTH + 2, "response too short: {:02x?}", &buf[..len]);
  buf[HEADER_LENGTH..len].to_vec()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receive_channels_listed_without_running_receiver() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();

  // like a playback-only ALSA client (aplay): RX channels configured, but no receiver is started
  let settings = make_settings("arc_test_rx_idle", 3, 9200, 2, 2);
  let arc_port = settings.self_info.arc_port;
  let server = DeviceServer::start(settings).await;
  tokio::time::sleep(Duration::from_millis(300)).await;

  let content =
    tokio::task::spawn_blocking(move || arc_request_first_page(arc_port, GET_RECEIVE_CHANNELS))
      .await
      .unwrap();
  let items_in_page = content[1];
  assert_eq!(
    items_in_page, 2,
    "receive channels page should list both RX channels, content: {content:02x?}"
  );

  server.shutdown().await;
}
