//! The sound cards and the device's settings: a JSON file, edited through the settings page.

use std::{collections::BTreeMap, path::PathBuf};

use inferno_aoip::device_server::Settings;
use serde::{Deserialize, Serialize};
use usbip_audio::uac2::Uac2Config;

use crate::engine::CardChannels;

/// Windows' shared-mode audio engine opens at most this many channels of a device; more are only usable by
/// applications opening it in exclusive mode.
pub const SHARED_MODE_CHANNELS: u16 = 32;
/// At 48 kHz, as many as fit into a high-speed USB isochronous endpoint.
pub const MAX_CHANNELS: u16 = 128;
pub const DEFAULT_UI_PORT: u16 = 47800;
pub const DEFAULT_PLAYBACK_MARGIN_MS: u32 = 10;
pub const MAX_PLAYBACK_MARGIN_MS: u32 = 100;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
  /// Name of the device on the Dante network (default: Inferno's, from the computer's name)
  pub name: Option<String>,
  /// Further Inferno settings (`KEY: VALUE`, see the README's Configuration), e.g. `BIND_IP`
  pub settings: BTreeMap<String, String>,
  /// The sound cards, each a Windows sound device with playback (transmitted) and capture (received)
  /// channels: the device's TX and RX channels, card after card
  pub cards: Vec<CardConfig>,
  /// Port of the settings page on 127.0.0.1
  pub ui_port: u16,
  /// How much later than Windows plays it audio is transmitted: margin for Windows' audio being late (e.g. a
  /// stalled audio engine). Audio later than that is transmitted as silence.
  pub playback_margin_ms: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CardConfig {
  pub name: String,
  /// Channels Windows plays to the card: transmitted to the Dante network
  pub playback_channels: u16,
  /// Channels Windows records from the card: received from the Dante network
  pub capture_channels: u16,
}

impl Default for CardConfig {
  fn default() -> Self {
    Self { name: "Inferno".into(), playback_channels: 2, capture_channels: 2 }
  }
}

impl Default for Config {
  fn default() -> Self {
    Self {
      name: None,
      settings: BTreeMap::new(),
      cards: vec![CardConfig::default()],
      ui_port: DEFAULT_UI_PORT,
      playback_margin_ms: DEFAULT_PLAYBACK_MARGIN_MS,
    }
  }
}

/// `INFERNO_SOUNDCARD_CONFIG`, or `%APPDATA%\Inferno\soundcard.json`.
pub fn config_path() -> PathBuf {
  if let Some(path) = std::env::var_os("INFERNO_SOUNDCARD_CONFIG") {
    return path.into();
  }
  let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
  base.join("Inferno").join("soundcard.json")
}

impl Config {
  /// The saved configuration, the default if there's none.
  pub fn load() -> Result<Self, String> {
    let path = config_path();
    match std::fs::read_to_string(&path) {
      Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
      Err(e) => Err(format!("can't read {}: {e}", path.display())),
    }
  }

  pub fn save(&self) -> Result<(), String> {
    let path = config_path();
    if let Some(dir) = path.parent() {
      std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(self).unwrap();
    std::fs::write(&path, text).map_err(|e| format!("can't write {}: {e}", path.display()))
  }

  /// The Inferno device's settings (its channels are the cards').
  pub fn device_settings(&self) -> Result<Settings, String> {
    let mut config: BTreeMap<String, String> =
      self.settings.iter().map(|(k, v)| (k.to_uppercase(), v.clone())).collect();
    for key in ["RX_CHANNELS", "TX_CHANNELS"] {
      if config.remove(key).is_some() {
        log::warn!("ignoring {key}: the channels are the sound cards'");
      }
    }
    if let Some(name) = self.name.as_deref().filter(|n| !n.trim().is_empty()) {
      config.insert("NAME".into(), name.trim().into());
    }
    let unknown = Settings::unknown_keys(&config, &["SOUNDCARD_CONFIG"]);
    if !unknown.is_empty() {
      return Err(format!("unknown settings: {}", unknown.join(", ")));
    }
    Settings::try_new("Inferno Sound Card", "soundcard", None, &config).map_err(|e| e.to_string())
  }

  pub fn card_channels(&self) -> Vec<CardChannels> {
    self
      .cards
      .iter()
      .map(|c| CardChannels {
        playback: c.playback_channels as usize,
        capture: c.capture_channels as usize,
      })
      .collect()
  }

  /// What's wrong with the configuration, if anything.
  /// The playback margin in frames at `sample_rate`.
  pub fn playback_lead(&self, sample_rate: u32) -> usize {
    self.playback_margin_ms as usize * sample_rate as usize / 1000
  }

  pub fn validate(&self) -> Result<(), String> {
    if self.cards.is_empty() {
      return Err("add a sound card".into());
    }
    if !(1..=MAX_PLAYBACK_MARGIN_MS).contains(&self.playback_margin_ms) {
      return Err(format!("the playback margin must be 1 to {MAX_PLAYBACK_MARGIN_MS} ms"));
    }
    let sample_rate = self.device_settings()?.self_info.sample_rate;
    let mut names = std::collections::BTreeSet::new();
    for card in &self.cards {
      let name = card.name.trim();
      if name.is_empty() {
        return Err("every sound card needs a name".into());
      }
      if !names.insert(name.to_lowercase()) {
        return Err(format!("two sound cards are named {name:?}"));
      }
      if card.playback_channels > MAX_CHANNELS || card.capture_channels > MAX_CHANNELS {
        return Err(format!("{name}: at most {MAX_CHANNELS} channels"));
      }
      card_uac2_config(card, 0, "", sample_rate).validate().map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
  }
}

/// pid.codes test ids, until the project has its own
pub const VENDOR_ID: u16 = 0x1209;
pub const PRODUCT_ID: u16 = 0x0001;

/// The USB audio device of card `index` of the device `device_id` (which keeps Windows' settings of the card
/// apart from other devices' cards).
pub fn card_uac2_config(
  card: &CardConfig,
  index: usize,
  device_id: &str,
  sample_rate: u32,
) -> Uac2Config {
  Uac2Config {
    vendor_id: VENDOR_ID,
    product_id: PRODUCT_ID,
    bcd_device: 0x0100,
    manufacturer: "Inferno".into(),
    product: card.name.trim().into(),
    serial: format!("{device_id}-{}", index + 1),
    sample_rate,
    playback_channels: card.playback_channels,
    capture_channels: card.capture_channels,
    playback_name: None,
    capture_name: None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn default_is_one_stereo_card() {
    let config = Config::default();
    assert_eq!(config.card_channels(), vec![CardChannels { playback: 2, capture: 2 }]);
    config.validate().unwrap();
  }

  #[test]
  fn playback_margin_is_limited() {
    for (ms, ok) in
      [(0, false), (1, true), (MAX_PLAYBACK_MARGIN_MS, true), (MAX_PLAYBACK_MARGIN_MS + 1, false)]
    {
      let config = Config { playback_margin_ms: ms, ..Config::default() };
      assert_eq!(config.validate().is_ok(), ok, "{ms} ms");
    }
  }

  #[test]
  fn reads_partial_json() {
    let config: Config =
      serde_json::from_str(r#"{"cards": [{"name": "Stage", "capture_channels": 16}]}"#).unwrap();
    assert_eq!(config.cards[0].playback_channels, 2);
    assert_eq!(config.cards[0].capture_channels, 16);
    assert_eq!(config.ui_port, DEFAULT_UI_PORT);
    assert_eq!(config.playback_lead(48000), 480);
    assert!(serde_json::from_str::<Config>(r#"{"cardz": []}"#).is_err(), "misspelt keys are errors");
  }

  #[test]
  fn invalid_configurations() {
    let card = |name: &str, playback, capture| CardConfig {
      name: name.into(),
      playback_channels: playback,
      capture_channels: capture,
    };
    let with = |cards| Config { cards, ..Config::default() };
    assert!(with(vec![]).validate().is_err());
    assert!(with(vec![card(" ", 2, 2)]).validate().is_err());
    assert!(with(vec![card("A", 2, 2), card("a", 2, 2)]).validate().is_err(), "duplicate names");
    assert!(with(vec![card("A", 0, 0)]).validate().is_err(), "no channels");
    assert!(with(vec![card("A", 200, 0)]).validate().is_err());
    assert!(with(vec![card("A", 64, 0), card("B", 0, 8)]).validate().is_ok());
    let bad_setting =
      Config { settings: [("SAMPLE_RATE".into(), "44100".into())].into(), ..Config::default() };
    assert!(bad_setting.validate().is_err(), "USB audio 2.0 needs a multiple of 8 kHz");
    let unknown =
      Config { settings: [("BIND_IPP".into(), "1.2.3.4".into())].into(), ..Config::default() };
    assert!(unknown.validate().unwrap_err().contains("BIND_IPP"));
  }
}
