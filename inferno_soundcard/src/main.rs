//! The sound card service: runs the cards of the configuration, the settings page and (on Windows) the tray
//! icon. `--no-tray`: without the icon, until stopped. On macOS it's a launchd daemon, without an icon, and
//! serves the cards to the Core Audio plug-in.

use std::sync::{mpsc, Arc, Mutex};

use inferno_soundcard::{
  app::{self, Status},
  config::{config_path, Config, DEFAULT_UI_PORT},
  web,
};
use log::{error, info};

fn main() {
  env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
  let tray = !std::env::args().any(|a| a == "--no-tray");
  info!("configuration: {}", config_path().display());

  let status = Arc::new(Mutex::new(Status::default()));
  let (commands, receiver) = mpsc::channel();
  let supervisor = {
    let status = status.clone();
    std::thread::Builder::new()
      .name("supervisor".into())
      .spawn(move || app::supervise(status, receiver))
      .unwrap()
  };

  #[cfg(target_os = "macos")]
  {
    let status = status.clone();
    std::thread::Builder::new()
      .name("mach service".into())
      .spawn(move || inferno_soundcard::coreaudio::serve(status))
      .unwrap();
  }

  let port = Config::load().map(|c| c.ui_port).unwrap_or(DEFAULT_UI_PORT);
  match web::bind(port) {
    Ok(listener) => {
      info!("settings page: http://127.0.0.1:{port}/");
      let (status, commands) = (status.clone(), commands.clone());
      std::thread::Builder::new()
        .name("settings page".into())
        .spawn(move || web::serve(listener, status, commands))
        .unwrap();
    }
    // e.g. another instance is running
    Err(e) => error!("settings page on port {port}: {e}"),
  }

  #[cfg(windows)]
  if tray {
    inferno_soundcard::tray::run(&format!("http://127.0.0.1:{port}/"));
    let _ = commands.send(app::Command::Quit);
  }
  #[cfg(not(windows))]
  let _ = tray;
  // without the tray: until the process is stopped (the supervisor never returns by itself)
  let _ = supervisor.join();
  drop(commands);
}
