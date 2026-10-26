//! Clock from a PTP daemon (Statime) via the usrvclock protocol, or directly from a PTP hardware clock device.

use std::{
  path::{Path, PathBuf},
  sync::Mutex,
};

use futures::future::BoxFuture;
use tokio::sync::watch;

use super::{ClockOverlay, ClockSource};
use crate::common::*;

impl From<usrvclock::ClockOverlay> for ClockOverlay {
  fn from(o: usrvclock::ClockOverlay) -> Self {
    Self { clock_id: o.clock_id, last_sync: o.last_sync, shift: o.shift, freq_scale: o.freq_scale }
  }
}

/// The clock source of `CLOCK_PATH`: a usrvclock socket of the PTP daemon, or a PTP hardware clock device.
pub fn clock_path_or_default(path: &Option<PathBuf>) -> PathBuf {
  path.clone().unwrap_or(usrvclock::DEFAULT_SERVER_SOCKET_PATH.into())
}

/// Why no clock arrives from `path`, and what to check.
pub fn diagnose_missing_clock(path: &Path) -> String {
  use std::os::unix::fs::FileTypeExt;
  let shown = path.display();
  match std::fs::metadata(path) {
    Err(_) => format!(
      "nothing at {shown}: is the PTP daemon (Statime) running? Its usrvclock socket must be at \
       CLOCK_PATH (default {})",
      usrvclock::DEFAULT_SERVER_SOCKET_PATH
    ),
    Ok(m) if m.file_type().is_socket() => format!(
      "{shown} exists but sends no clock updates: is the PTP daemon still running, and does it have a \
       clock leader to follow? (If the application runs in a sandbox or with a private /tmp, it can't \
       reach the daemon.)"
    ),
    Ok(m) if m.file_type().is_char_device() => {
      format!("{shown} is a PTP hardware clock but gives no time: can this user read it?")
    }
    Ok(_) => {
      format!("{shown} is neither a usrvclock socket nor a PTP hardware clock device: check CLOCK_PATH")
    }
  }
}

/// Clock from `CLOCK_PATH` (see [`clock_path_or_default`]).
pub struct UsrvClockSource {
  path: PathBuf,
  client: Mutex<Option<usrvclock::AsyncClient>>,
  receiver: watch::Receiver<Option<ClockOverlay>>,
}

impl UsrvClockSource {
  /// Starts receiving the clock. Must be called inside a tokio runtime.
  pub fn start(path: Option<PathBuf>) -> Self {
    let path = clock_path_or_default(&path);
    let client =
      usrvclock::AsyncClient::start(path.clone(), Box::new(|e| warn!("clock receive error: {e:?}")));
    let mut upstream = client.subscribe();
    let (sender, receiver) = watch::channel(upstream.borrow_and_update().map(ClockOverlay::from));
    tokio::spawn(async move {
      while upstream.changed().await.is_ok() {
        let overlay = upstream.borrow_and_update().map(ClockOverlay::from);
        if sender.send(overlay).is_err() {
          break;
        }
      }
    });
    Self { path, client: Mutex::new(Some(client)), receiver }
  }

  pub fn path(&self) -> &Path {
    &self.path
  }
}

impl ClockSource for UsrvClockSource {
  fn subscribe(&self) -> watch::Receiver<Option<ClockOverlay>> {
    self.receiver.clone()
  }
  fn description(&self) -> String {
    self.path.display().to_string()
  }
  fn diagnose_missing(&self) -> String {
    diagnose_missing_clock(&self.path)
  }
  fn shutdown(&self) -> BoxFuture<'static, ()> {
    let client = self.client.lock().unwrap().take();
    Box::pin(async move {
      if let Some(client) = client {
        client.stop().await.unwrap();
      }
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn missing_clock_diagnosis_says_what_is_at_the_path() {
    let dir = std::env::temp_dir().join(format!("inferno-clock-diag-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let missing = dir.join("missing");
    let d = diagnose_missing_clock(&missing);
    assert!(d.contains(&missing.display().to_string()) && d.contains("Statime"), "{d}");

    let socket_path = dir.join("socket");
    let _ = std::fs::remove_file(&socket_path);
    let _socket = std::os::unix::net::UnixDatagram::bind(&socket_path).unwrap();
    let d = diagnose_missing_clock(&socket_path);
    assert!(d.contains("sends no clock updates"), "{d}");

    let file = dir.join("file");
    std::fs::write(&file, b"").unwrap();
    let d = diagnose_missing_clock(&file);
    assert!(d.contains("check CLOCK_PATH"), "{d}");
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn converts_usrvclock_overlay() {
    let o = usrvclock::ClockOverlay { clock_id: 3, last_sync: 4, shift: 5, freq_scale: 0.5 };
    assert_eq!(
      ClockOverlay::from(o),
      ClockOverlay { clock_id: 3, last_sync: 4, shift: 5, freq_scale: 0.5 }
    );
  }
}
