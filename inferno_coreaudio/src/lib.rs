//! The Inferno sound cards' Core Audio HAL plug-in (an AudioServerPlugIn, loaded by `coreaudiod`): a Core
//! Audio device per sound card of the Inferno sound card service (`inferno_soundcard`), on the Dante network.
//!
//! The plug-in does no networking. It plays and records through the memory the service shares with it (see
//! `inferno_soundcard_shm`), and tells Core Audio when each frame is due by the media clock the service
//! publishes there, so Core Audio runs at the network's rate, without resampling. Built into
//! `Inferno.driver` (see `packaging/macos`), installed in `/Library/Audio/Plug-Ins/HAL`.
//!
//! Its objects and their properties ([`objects`]) are independent of Core Audio, and tested anywhere.

pub mod abi;
#[cfg(target_os = "macos")]
mod driver;
pub mod objects;
