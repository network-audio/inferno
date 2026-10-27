//! Runs the configured sound cards: the engine, the USB/IP server exporting a USB audio device per card, and
//! their attachment through usbip-win2. Rebuilt whenever the configuration changes.

use std::{
  path::PathBuf,
  sync::{mpsc, Arc, Mutex},
  time::{Duration, Instant},
};

use inferno_driver_core::DeviceError;
use log::{error, info, warn};
use serde::Serialize;
use usbip_audio::{uac2::Uac2Device, usbip_exe, Server};

use crate::{
  config::{card_uac2_config, Config},
  engine::Engine,
};

/// Waiting for the device's clock, how long before trying again (with a fresh device).
const CLOCK_TIMEOUT: Duration = Duration::from_secs(60);

pub enum Command {
  /// the configuration was saved: apply it
  Reload,
  Quit,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Status {
  /// starting, waiting for the clock, running, or the error
  pub state: String,
  pub running: bool,
  /// the device's name on the Dante network
  pub device_name: Option<String>,
  pub sample_rate: Option<u32>,
  pub cards: Vec<CardStatus>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CardStatus {
  pub name: String,
  pub playback_channels: u16,
  pub capture_channels: u16,
  /// the device's TX channels the card plays to (1-based, inclusive), if any
  pub tx_channels: Option<(usize, usize)>,
  /// the device's RX channels the card records from
  pub rx_channels: Option<(usize, usize)>,
  /// the usbip-win2 port it's attached to
  pub usbip_port: Option<u32>,
}

pub type SharedStatus = Arc<Mutex<Status>>;

/// The running cards; detached and stopped when dropped.
struct Running {
  // dropped in this order: detached, unexported, then the device stopped
  usbip: Option<PathBuf>,
  attached: Vec<u32>,
  _server: Server,
  _engine: Engine,
}

impl Drop for Running {
  fn drop(&mut self) {
    if let Some(usbip) = &self.usbip {
      for &port in &self.attached {
        if let Err(e) = usbip_exe::detach(usbip, port) {
          warn!("{e}");
        }
      }
    }
  }
}

fn set_state(status: &SharedStatus, state: impl Into<String>) {
  let mut status = status.lock().unwrap();
  status.state = state.into();
  status.running = false;
}

/// Starts the cards of `config`. Waits for the clock, unless a command arrives first (returned as the error).
fn start(
  config: &Config,
  status: &SharedStatus,
  commands: &mpsc::Receiver<Command>,
) -> Result<Running, Option<Command>> {
  let fail = |e: String| {
    error!("{e}");
    set_state(status, e);
    None
  };
  config.validate().map_err(fail)?;
  let settings = config.device_settings().map_err(fail)?;
  let device_name = settings.self_info.friendly_hostname.clone();
  let device_id: String =
    settings.self_info.factory_device_id.iter().map(|b| format!("{b:02x}")).collect();
  let sample_rate = settings.self_info.sample_rate;
  let mut engine =
    Engine::new(settings, &config.card_channels(), None).map_err(|e| fail(e.to_string()))?;

  set_state(status, "waiting for the clock");
  let waiting = Instant::now();
  let clock = loop {
    match engine.start() {
      Ok(clock) => break clock,
      Err(DeviceError::NoClock(why)) if waiting.elapsed() < CLOCK_TIMEOUT => {
        set_state(status, format!("waiting for the clock: {why}"));
        match commands.recv_timeout(Duration::from_millis(500)) {
          Ok(command) => return Err(Some(command)),
          Err(mpsc::RecvTimeoutError::Timeout) => {}
          Err(mpsc::RecvTimeoutError::Disconnected) => return Err(Some(Command::Quit)),
        }
      }
      Err(e) => return Err(fail(e.to_string())),
    }
  };

  let server = Server::start(0, clock).map_err(|e| fail(format!("USB/IP server: {e}")))?;
  let mut cards = vec![];
  let (mut rx, mut tx) = (0, 0);
  for (index, (card, streams)) in config.cards.iter().zip(engine.card_streams()).enumerate() {
    let device = Uac2Device::new(card_uac2_config(card, index, &device_id, sample_rate), streams)
      .map_err(|e| fail(format!("{}: {e}", card.name)))?;
    let busid = format!("1-{}", index + 1);
    server.add(&busid, Arc::new(device));
    let range = |start: usize, count: u16| (count > 0).then(|| (start + 1, start + count as usize));
    cards.push((
      busid,
      CardStatus {
        name: card.name.trim().into(),
        playback_channels: card.playback_channels,
        capture_channels: card.capture_channels,
        tx_channels: range(tx, card.playback_channels),
        rx_channels: range(rx, card.capture_channels),
        usbip_port: None,
      },
    ));
    (rx, tx) = (rx + card.capture_channels as usize, tx + card.playback_channels as usize);
  }

  let usbip = usbip_exe::find();
  let mut running = Running { usbip: usbip.clone(), attached: vec![], _server: server, _engine: engine };
  match &usbip {
    Some(usbip) => {
      for (busid, card) in &mut cards {
        match usbip_exe::attach(usbip, running._server.port(), busid) {
          Ok(port) => {
            info!("{} attached to usbip-win2 port {port}", card.name);
            card.usbip_port = Some(port);
            running.attached.push(port);
          }
          Err(e) => error!("{}: {e}", card.name),
        }
      }
    }
    None => warn!(
      "usbip-win2 (usbip.exe) not found: the cards are served on 127.0.0.1:{}, but not attached",
      running._server.port()
    ),
  }
  let attached = cards.iter().filter(|(_, c)| c.usbip_port.is_some()).count();
  let mut s = status.lock().unwrap();
  *s = Status {
    state: if attached == cards.len() {
      "running".into()
    } else {
      format!("running, {} of {} cards attached", attached, cards.len())
    },
    running: true,
    device_name: Some(device_name),
    sample_rate: Some(sample_rate),
    cards: cards.into_iter().map(|(_, c)| c).collect(),
  };
  info!("{}", s.state);
  Ok(running)
}

/// Runs the configured cards until `Command::Quit`.
pub fn supervise(status: SharedStatus, commands: mpsc::Receiver<Command>) {
  loop {
    set_state(&status, "starting");
    let config = Config::load();
    let started = match &config {
      Ok(config) => start(config, &status, &commands),
      Err(e) => {
        error!("{e}");
        set_state(&status, e.clone());
        Err(None)
      }
    };
    let command = match started {
      Ok(running) => {
        let command = commands.recv().unwrap_or(Command::Quit);
        drop(running);
        command
      }
      Err(Some(command)) => command,
      // failed: wait for a new configuration (or try again, e.g. after the clock appeared)
      Err(None) => match commands.recv_timeout(Duration::from_secs(10)) {
        Ok(command) => command,
        Err(mpsc::RecvTimeoutError::Timeout) => Command::Reload,
        Err(mpsc::RecvTimeoutError::Disconnected) => Command::Quit,
      },
    };
    if let Command::Quit = command {
      set_state(&status, "stopped");
      return;
    }
    info!("applying the configuration");
  }
}
