//! Settings of the ASIO driver: the same keys as the other Inferno integrations (see the README's
//! Configuration), from a `KEY=VALUE` file in the user's application data, overridden by `INFERNO_*`
//! environment variables. Logs go to a file, as a driver has no console.

use std::{collections::BTreeMap, io::Write, path::PathBuf, sync::Once};

use inferno_aoip::device_server::Settings;

// not reformatted: rustfmt's string wrapping would change the text
#[rustfmt::skip]
const TEMPLATE: &str = "\
# Inferno ASIO driver settings: KEY=VALUE, one per line. Restart the audio application after changing them.
# All settings: https://github.com/stakach/inferno#configuration
#
# Name of the device on the Dante network (default: the application's name)
#NAME=My DAW
# Channels
#RX_CHANNELS=2
#TX_CHANNELS=2
# Network interface (address) to use
#BIND_IP=192.168.1.10
#SAMPLE_RATE=48000
#RX_LATENCY_NS=10000000
#TX_LATENCY_NS=10000000
";

/// `%APPDATA%\\Inferno\\inferno_asio.conf` (or `INFERNO_ASIO_CONFIG`).
pub fn config_path() -> PathBuf {
  if let Some(path) = std::env::var_os("INFERNO_ASIO_CONFIG") {
    return path.into();
  }
  let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
  base.join("Inferno").join("inferno_asio.conf")
}

/// Parses `KEY=VALUE` lines; empty lines and `#` comments are skipped.
pub fn parse_config(text: &str) -> BTreeMap<String, String> {
  text
    .lines()
    .map(str::trim)
    .filter(|l| !l.is_empty() && !l.starts_with('#'))
    .filter_map(|l| l.split_once('='))
    .map(|(k, v)| (k.trim().to_uppercase(), v.trim().to_string()))
    .collect()
}

fn app_name() -> String {
  std::env::current_exe()
    .ok()
    .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
    .unwrap_or_else(|| format!("process {}", std::process::id()))
}

/// The settings for the device, or what's wrong with them.
pub fn load_settings() -> Result<Settings, String> {
  let path = config_path();
  let file = match std::fs::read_to_string(&path) {
    Ok(text) => parse_config(&text),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
    Err(e) => return Err(format!("can't read {}: {e}", path.display())),
  };
  let config = Settings::merged_config(&file);
  let unknown = Settings::unknown_keys(&config, &[]);
  if !unknown.is_empty() {
    log::warn!(
      "ignoring unknown settings (misspelt?) in {} or the environment: {}",
      path.display(),
      unknown.join(", ")
    );
  }
  let name = app_name();
  Settings::try_new(&name, &name, None, &config)
    .map_err(|e| format!("{e} (in {} or INFERNO_* variables)", path.display()))
}

/// Opens the settings file in an editor, creating it from a template first.
pub fn open_config_file() -> std::io::Result<()> {
  let path = config_path();
  if !path.exists() {
    if let Some(dir) = path.parent() {
      std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, TEMPLATE)?;
  }
  std::process::Command::new("notepad.exe").arg(&path).spawn().map(|_| ())
}

/// Logs to `inferno_asio.log` in the temporary directory (level: `RUST_LOG`, default info).
pub fn init_logging() {
  static ONCE: Once = Once::new();
  ONCE.call_once(|| {
    let path = std::env::temp_dir().join("inferno_asio.log");
    let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
      return;
    };
    let env = env_logger::Env::default().default_filter_or("info");
    let _ = env_logger::Builder::from_env(env)
      .target(env_logger::Target::Pipe(Box::new(file)))
      .format(|buf, record| {
        writeln!(
          buf,
          "[{} {} {} {}] {}",
          buf.timestamp_micros(),
          std::process::id(),
          record.level(),
          record.target(),
          record.args()
        )
      })
      .try_init();
  });
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_key_value_lines() {
    let c = parse_config("# comment\n\nname = Studio A\nRX_CHANNELS=8\nbroken line\n");
    assert_eq!(c.get("NAME").map(String::as_str), Some("Studio A"));
    assert_eq!(c.get("RX_CHANNELS").map(String::as_str), Some("8"));
    assert_eq!(c.len(), 2);
  }

  #[test]
  fn template_is_all_comments() {
    assert!(parse_config(TEMPLATE).is_empty());
    assert!(TEMPLATE.lines().all(|l| l.starts_with('#')), "{TEMPLATE}");
    assert!(TEMPLATE.contains("#configuration\n#\n# Name"), "{TEMPLATE}");
  }
}
