use std::{
  collections::BTreeMap,
  env,
  net::{IpAddr, Ipv4Addr},
  path::PathBuf,
  sync::{Arc, RwLock},
};

use netdev::mac::MacAddr;

use crate::device_info::{Channel, DeviceInfo};
use crate::protocol::flows_control::PORT as FLOWS_CONTROL_PORT;
use crate::protocol::mcast::INFO_REQUEST_PORT;
use crate::protocol::proto_arc::PORT as ARC_PORT;
use crate::protocol::proto_cmc::PORT as CMC_PORT;

/// Settings keys (in upper case) understood by `Settings`, as config keys or `INFERNO_` environment variables.
pub const KEYS: &[&str] = &[
  "NAME",
  "DEVICE_ID",
  "PROCESS_ID",
  "BIND_IP",
  "ALT_PORT",
  "SAMPLE_RATE",
  "RX_CHANNELS",
  "TX_CHANNELS",
  "RX_LATENCY_NS",
  "TX_LATENCY_NS",
  "TX_SOURCE_BIT_DEPTH",
  "CLOCK_PATH",
  "USE_SAFE_CLOCK",
];

/// An invalid setting: which one, its value and what is accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError {
  pub key: &'static str,
  pub value: Option<String>,
  pub problem: String,
}

impl std::fmt::Display for SettingsError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match &self.value {
      Some(value) => write!(f, "invalid setting {}={value:?}: {}", self.key, self.problem),
      None => write!(f, "setting {}: {}", self.key, self.problem),
    }
  }
}

impl std::error::Error for SettingsError {}

fn invalid(key: &'static str, value: &str, problem: impl Into<String>) -> SettingsError {
  SettingsError { key, value: Some(value.to_owned()), problem: problem.into() }
}

/// Parses setting `key` if present.
fn parse<T: std::str::FromStr>(
  settings: &BTreeMap<String, String>,
  key: &'static str,
  expected: &str,
) -> Result<Option<T>, SettingsError> {
  settings.get(key).map(|v| v.trim().parse().map_err(|_| invalid(key, v, expected))).transpose()
}

/// The longest prefix of `s` of at most `max_bytes` bytes that ends on a character boundary.
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
  let mut end = s.len().min(max_bytes);
  while !s.is_char_boundary(end) {
    end -= 1;
  }
  &s[..end]
}

fn bind_ip(
  settings: &BTreeMap<String, String>,
  interfaces: &[netdev::Interface],
) -> Result<Ipv4Addr, SettingsError> {
  let Some(value) = settings.get("BIND_IP") else {
    return match local_ip_address::local_ip() {
      Ok(IpAddr::V4(a)) => Ok(a),
      Ok(other) => Err(SettingsError {
        key: "BIND_IP",
        value: None,
        problem: format!("the default route uses {other}, which is not IPv4; set BIND_IP"),
      }),
      Err(e) => Err(SettingsError {
        key: "BIND_IP",
        value: None,
        problem: format!("cannot determine the local IP address ({e}); set BIND_IP"),
      }),
    };
  };
  if let Ok(ip) = value.parse() {
    return Ok(ip);
  }
  match interfaces.iter().find(|iface| &iface.name == value) {
    Some(iface) => iface
      .ipv4
      .first()
      .map(|net| net.addr())
      .ok_or_else(|| invalid("BIND_IP", value, "this network interface has no IPv4 address")),
    None => {
      let names = interfaces.iter().map(|iface| iface.name.as_str()).collect::<Vec<_>>().join(", ");
      Err(invalid(
        "BIND_IP",
        value,
        format!("not an IPv4 address or network interface (interfaces: {names})"),
      ))
    }
  }
}

fn create_self_info(
  app_name: &str,
  short_app_name: &str,
  my_ip: Option<Ipv4Addr>,
  settings: &BTreeMap<String, String>,
) -> Result<DeviceInfo, SettingsError> {
  let interfaces = netdev::get_interfaces();
  let my_ipv4 = match my_ip {
    Some(ip) => ip,
    None => bind_ip(settings, &interfaces)?,
  };

  let process_id: u16 = parse(settings, "PROCESS_ID", "must be an integer from 0 to 65535")?.unwrap_or(0);

  let mut devid = [0u8; 8];
  match settings.get("DEVICE_ID") {
    Some(idstr) => hex::decode_to_slice(idstr.trim(), &mut devid)
      .map_err(|_| invalid("DEVICE_ID", idstr, "must be 16 hexadecimal digits"))?,
    None => {
      devid[2..6].copy_from_slice(&my_ipv4.octets());
      devid[6..8].copy_from_slice(&process_id.to_be_bytes());
    }
  }

  // TODO make hostname and sample rate configurable from DC
  let friendly_hostname = match settings.get("NAME") {
    Some(name) => truncate_utf8(name, 31).to_owned(),
    None => format!("{} {}", truncate_utf8(app_name, 22), hex::encode(my_ipv4.octets())),
  };
  let short_app_name = truncate_utf8(short_app_name, 14);

  let sample_rate: u32 =
    parse(settings, "SAMPLE_RATE", "must be a sample rate in Hz, e.g. 48000")?.unwrap_or(48000);
  if !(8000..=384000).contains(&sample_rate) {
    return Err(invalid("SAMPLE_RATE", &sample_rate.to_string(), "must be from 8000 to 384000 Hz"));
  }

  let mut netmask = Ipv4Addr::new(0, 0, 0, 0);
  let mut gateway = Ipv4Addr::new(0, 0, 0, 0);
  let mut mac_address = MacAddr::zero();
  let mut interface_name = None;
  let mut speed = 0;
  for iface in interfaces {
    let mut our_iface = false;
    for network in iface.ipv4 {
      if network.addr() == my_ipv4 {
        netmask = network.netmask();
        our_iface = true;
        break;
      }
    }
    if our_iface {
      interface_name = Some(iface.name.clone());
      speed =
        [iface.transmit_speed.unwrap_or(0), iface.receive_speed.unwrap_or(0)].iter().max().unwrap_or(&0)
          / 1_000_000;
      if let Some(gws) = iface.gateway {
        for gw in gws.ipv4 {
          if (gw.to_bits() & netmask.to_bits()) == (my_ipv4.to_bits() & netmask.to_bits()) {
            gateway = gw;
            break;
          }
        }
      }
      if let Some(mac) = iface.mac_addr {
        mac_address = mac;
      }
      break;
    }
  }

  let latency_ns: usize =
    parse(settings, "RX_LATENCY_NS", "must be a latency in nanoseconds, e.g. 10000000")?
      .unwrap_or(10_000_000);

  let mut result = DeviceInfo {
    ip_address: my_ipv4,
    netmask,
    gateway,
    mac_address,
    interface_name,
    link_speed: speed.clamp(0, 10000).try_into().unwrap(),

    board_name: "Inferno-AoIP".to_owned(),
    manufacturer: "Inferno-AoIP".to_owned(),
    model_name: app_name.to_owned(),
    factory_device_id: devid,
    process_id,
    vendor_string: "Audinate Dante-compatible".to_owned(),
    factory_hostname: format!("{short_app_name}-{}", hex::encode(devid)),
    friendly_hostname,
    model_number: "_000000000000000b".to_owned(),
    rx_channels: vec![],
    tx_channels: vec![],
    bits_per_sample: 24, // TODO make it configurable
    pcm_type: 0xe,
    latency_ns,
    sample_rate,

    arc_port: ARC_PORT,
    cmc_port: CMC_PORT,
    flows_control_port: FLOWS_CONTROL_PORT,
    info_request_port: INFO_REQUEST_PORT,
  };

  // the device listens on 4 consecutive ports starting at ALT_PORT
  if let Some(altport) = parse::<u16>(settings, "ALT_PORT", "must be a port number from 1 to 65532")? {
    if altport == 0 || altport > u16::MAX - 3 {
      return Err(invalid("ALT_PORT", &altport.to_string(), "must be a port number from 1 to 65532"));
    }
    result.arc_port = altport;
    result.cmc_port = altport + 1;
    result.flows_control_port = altport + 2;
    result.info_request_port = altport + 3;
  }

  Ok(result)
}

#[derive(Clone)] // TODO: this shouldn't need to be clonable, fix the ALSA plugin
pub struct Settings {
  pub self_info: DeviceInfo,
  pub tx_latency_ns: u32,
  pub clock_path: Option<PathBuf>,
  pub use_safe_clock: bool,
  pub tx_source_bit_depth: u8,
}

impl Settings {
  /// Like `try_new`, but panics on invalid settings.
  pub fn new(
    app_name: &str,
    short_app_name: &str,
    my_ip: Option<Ipv4Addr>,
    config: &BTreeMap<String, String>,
  ) -> Self {
    Self::try_new(app_name, short_app_name, my_ip, config).unwrap_or_else(|e| panic!("{e}"))
  }

  /// Settings from `config` (keys in any case), completed by `INFERNO_*` environment variables.
  pub fn try_new(
    app_name: &str,
    short_app_name: &str,
    my_ip: Option<Ipv4Addr>,
    config: &BTreeMap<String, String>,
  ) -> Result<Self, SettingsError> {
    let config = Self::merged_config(config);
    let self_info = create_self_info(app_name, short_app_name, my_ip, &config)?;

    let use_safe_clock: bool =
      parse(&config, "USE_SAFE_CLOCK", "must be true or false")?.unwrap_or(false);
    if use_safe_clock {
      return Err(invalid("USE_SAFE_CLOCK", "true", "the safe clock is not supported yet"));
    }
    let bit_depth_error = "must be one of: 16, 24, 32";
    let tx_source_bit_depth: u8 = parse(&config, "TX_SOURCE_BIT_DEPTH", bit_depth_error)?.unwrap_or(32);
    if !matches!(tx_source_bit_depth, 16 | 24 | 32) {
      return Err(invalid("TX_SOURCE_BIT_DEPTH", &tx_source_bit_depth.to_string(), bit_depth_error));
    }

    let mut result = Self {
      self_info,
      tx_latency_ns: parse(&config, "TX_LATENCY_NS", "must be a latency in nanoseconds, e.g. 10000000")?
        .unwrap_or(10_000_000),
      clock_path: config.get("CLOCK_PATH").map(PathBuf::from),
      use_safe_clock,
      tx_source_bit_depth,
    };

    // the following should be harmless, as the application still has the chance to overwrite it
    let channels_error = "must be a number of channels from 0 to 65535";
    let rx_count: u16 = parse(&config, "RX_CHANNELS", channels_error)?.unwrap_or(2);
    result.make_rx_channels(rx_count.into());
    let tx_count: u16 = parse(&config, "TX_CHANNELS", channels_error)?.unwrap_or(2);
    result.make_tx_channels(tx_count.into());

    Ok(result)
  }

  /// `config` with keys in upper case, completed by `INFERNO_*` environment variables (config wins).
  /// Empty values count as unset, e.g. ALSA device arguments defaulting to "" when not given.
  pub fn merged_config(config: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut config: BTreeMap<String, String> = config
      .iter()
      .filter(|(_, v)| !v.trim().is_empty())
      .map(|(k, v)| (k.to_ascii_uppercase(), v.clone()))
      .collect();
    env::vars().for_each(|(env_key, env_value)| {
      if let Some(key) = env_key.strip_prefix("INFERNO_") {
        if !env_value.trim().is_empty() {
          config.entry(key.to_ascii_uppercase()).or_insert(env_value);
        }
      }
    });
    config
  }

  /// Keys of a merged config that are neither settings (`KEYS`) nor in `other_keys`, e.g. misspelt ones.
  pub fn unknown_keys<'a>(config: &'a BTreeMap<String, String>, other_keys: &[&str]) -> Vec<&'a str> {
    config.keys().map(String::as_str).filter(|k| !KEYS.contains(k) && !other_keys.contains(k)).collect()
  }
  pub fn make_rx_channels(&mut self, count: usize) {
    self.self_info.rx_channels = (1..=count)
      .map(|id| Channel {
        factory_name: format!("{id:02}"),
        friendly_name: Arc::new(RwLock::new(format!("RX {id}"))),
      })
      .collect();
  }
  pub fn make_tx_channels(&mut self, count: usize) {
    self.self_info.tx_channels = (1..=count)
      .map(|id| Channel {
        factory_name: format!("{id:02}"),
        friendly_name: Arc::new(RwLock::new(format!("TX {id}"))),
      })
      .collect();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn settings(pairs: &[(&str, &str)]) -> Result<Settings, SettingsError> {
    let mut config: BTreeMap<String, String> =
      pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    config.entry("BIND_IP".into()).or_insert("127.0.0.1".into());
    Settings::try_new("app", "app", None, &config)
  }
  fn error_key(pairs: &[(&str, &str)]) -> &'static str {
    settings(pairs).err().expect("accepted invalid settings").key
  }

  #[test]
  fn defaults_are_valid() {
    let s = settings(&[]).unwrap();
    assert_eq!(s.self_info.sample_rate, 48000);
    assert_eq!((s.self_info.rx_channels.len(), s.self_info.tx_channels.len()), (2, 2));
  }

  #[test]
  fn invalid_values_name_their_setting() {
    assert_eq!(error_key(&[("BIND_IP", "no-such-interface0")]), "BIND_IP");
    assert_eq!(error_key(&[("PROCESS_ID", "70000")]), "PROCESS_ID");
    assert_eq!(error_key(&[("DEVICE_ID", "xyz")]), "DEVICE_ID");
    assert_eq!(error_key(&[("SAMPLE_RATE", "48k")]), "SAMPLE_RATE");
    assert_eq!(error_key(&[("SAMPLE_RATE", "0")]), "SAMPLE_RATE");
    assert_eq!(error_key(&[("RX_LATENCY_NS", "-1")]), "RX_LATENCY_NS");
    assert_eq!(error_key(&[("TX_LATENCY_NS", "10ms")]), "TX_LATENCY_NS");
    assert_eq!(error_key(&[("RX_CHANNELS", "two")]), "RX_CHANNELS");
    assert_eq!(error_key(&[("TX_SOURCE_BIT_DEPTH", "20")]), "TX_SOURCE_BIT_DEPTH");
    assert_eq!(error_key(&[("USE_SAFE_CLOCK", "maybe")]), "USE_SAFE_CLOCK");
  }

  /// e.g. ALSA arguments defaulting to "" when not given
  #[test]
  fn empty_values_are_unset() {
    let s = settings(&[("NAME", " "), ("SAMPLE_RATE", ""), ("ALT_PORT", ""), ("DEVICE_ID", "")]).unwrap();
    assert_eq!(s.self_info.sample_rate, 48000);
    assert!(s.self_info.friendly_hostname.starts_with("app "), "{}", s.self_info.friendly_hostname);
    let merged = Settings::merged_config(&[("RX_CHANNELS".to_string(), "".to_string())].into());
    assert!(!merged.contains_key("RX_CHANNELS"), "{merged:?}");
  }

  #[test]
  fn alt_port_leaves_room_for_its_four_ports() {
    assert_eq!(error_key(&[("ALT_PORT", "65535")]), "ALT_PORT");
    assert_eq!(error_key(&[("ALT_PORT", "0")]), "ALT_PORT");
    let s = settings(&[("ALT_PORT", "65532")]).unwrap();
    assert_eq!(s.self_info.info_request_port, 65535);
  }

  #[test]
  fn unsupported_safe_clock_is_an_error_not_a_crash() {
    assert_eq!(error_key(&[("USE_SAFE_CLOCK", "true")]), "USE_SAFE_CLOCK");
    assert!(settings(&[("USE_SAFE_CLOCK", "false")]).is_ok());
  }

  #[test]
  fn long_names_are_cut_on_character_boundaries() {
    // 30 ASCII bytes, then a 2-byte character across the 31-byte limit
    let name = format!("{}é", "a".repeat(30));
    let s = settings(&[("NAME", &name)]).unwrap();
    assert_eq!(s.self_info.friendly_hostname, "a".repeat(30));
    let app = "ü".repeat(20);
    let s = Settings::try_new(&app, &app, Some(Ipv4Addr::LOCALHOST), &BTreeMap::new()).unwrap();
    assert!(s.self_info.friendly_hostname.starts_with(&"ü".repeat(11)));
  }

  #[test]
  fn error_message_names_key_value_and_accepted_values() {
    let e = settings(&[("TX_SOURCE_BIT_DEPTH", "20")]).err().unwrap();
    assert_eq!(e.to_string(), r#"invalid setting TX_SOURCE_BIT_DEPTH="20": must be one of: 16, 24, 32"#);
  }

  #[test]
  fn keys_are_case_insensitive_and_unknown_ones_are_reported() {
    let config: BTreeMap<String, String> =
      [("sample_rate", "44100"), ("SAMPLERATE", "44100"), ("type", "inferno")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let merged = Settings::merged_config(&config);
    assert_eq!(merged.get("SAMPLE_RATE").map(String::as_str), Some("44100"));
    let unknown = Settings::unknown_keys(&merged, &["TYPE"]);
    assert!(unknown.contains(&"SAMPLERATE"), "{unknown:?}");
    assert!(!unknown.contains(&"SAMPLE_RATE") && !unknown.contains(&"TYPE"), "{unknown:?}");
  }
}
