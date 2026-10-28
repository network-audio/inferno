//! The cards as USB audio devices, served over USB/IP on `127.0.0.1` and attached by usbip-win2 (Windows).
//! Elsewhere they're served, not attached (for tests).

use std::{path::PathBuf, sync::Arc};

use inferno_soundcard_shm::Region;
use log::{error, info, warn};
use usbip_audio::{uac2::Uac2Device, usbip_exe, Server};

use crate::{
  app::{CardStatus, SharedStatus},
  config::{card_uac2_config, Config},
  engine::{Allocate, Engine, EngineClock},
};

/// The rings are in this process's memory.
pub fn allocate() -> Box<Allocate> {
  Box::new(Region::heap)
}

/// The served cards: detached, then unexported, when dropped.
pub struct Exported {
  usbip: Option<PathBuf>,
  attached: Vec<u32>,
  _server: Server,
}

impl Drop for Exported {
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

/// Serves the cards of the started `engine`, and attaches them. Returns them and the state to show.
pub fn export(
  engine: &Engine,
  clock: Arc<EngineClock>,
  config: &Config,
  device_id: &str,
  cards: &mut [CardStatus],
  _status: &SharedStatus,
) -> Result<(Exported, String), String> {
  let sample_rate = engine.region().header().sample_rate;
  let server = Server::start(0, clock).map_err(|e| format!("USB/IP server: {e}"))?;
  let mut busids = vec![];
  for (index, (card, streams)) in config.cards.iter().zip(engine.card_streams()).enumerate() {
    let device = Uac2Device::new(card_uac2_config(card, index, device_id, sample_rate), streams)
      .map_err(|e| format!("{}: {e}", card.name))?;
    let busid = format!("1-{}", index + 1);
    server.add(&busid, Arc::new(device));
    busids.push(busid);
  }

  let usbip = usbip_exe::find();
  let mut exported = Exported { usbip: usbip.clone(), attached: vec![], _server: server };
  match &usbip {
    Some(usbip) => {
      for (busid, card) in busids.iter().zip(cards.iter_mut()) {
        match usbip_exe::attach(usbip, exported._server.port(), busid) {
          Ok(port) => {
            info!("{} attached to usbip-win2 port {port}", card.name);
            card.usbip_port = Some(port);
            exported.attached.push(port);
          }
          Err(e) => error!("{}: {e}", card.name),
        }
      }
    }
    None => warn!(
      "usbip-win2 (usbip.exe) not found: the cards are served on 127.0.0.1:{}, but not attached",
      exported._server.port()
    ),
  }
  let attached = cards.iter().filter(|c| c.usbip_port.is_some()).count();
  let state = if attached == cards.len() {
    "running".into()
  } else {
    format!("running, {} of {} cards attached", attached, cards.len())
  };
  Ok((exported, state))
}
