//! Memory shared by the Inferno sound card service and the macOS Core Audio plug-in.
//!
//! On macOS the sound cards are Core Audio devices of a HAL plug-in (an AudioServerPlugIn, loaded by
//! `coreaudiod`). The plug-in does no networking: the service runs the Inferno device, and the two share a
//! region of memory ([`Region`]) holding what the plug-in needs to play and record:
//!
//! * the cards: their names, channels, and which of the device's RX and TX channels they use
//! * the device's rings, addressed by media clock time (frames since the start), which Inferno's receiver
//!   and transmitter use directly, and how far each has been received or played ([`rings`])
//! * the media clock, as published by the service ([`SharedClock`]), so that the plug-in can tell Core Audio
//!   when each frame is due, and Core Audio runs at the network's rate
//!
//! On macOS the service hands the region to the plug-in over a Mach service ([`mach`]). Nothing here needs
//! macOS otherwise: the layout and the ring operations are tested anywhere, and the Windows sound cards'
//! engine uses the same ring operations.

mod clock;
mod io;
mod layout;
#[cfg(target_os = "macos")]
pub mod mach;
pub mod rings;

#[cfg(unix)]
pub use clock::underlying_now_ns;
pub use clock::{Overlay, SharedClock, ZeroTimeStamps};
pub use io::{f32_to_sample, sample_to_f32, CardIo};
pub use layout::{
  CardInfo, Header, Layout, Region, RegionCard, RegionParams, State, MAX_CARDS, NAME_LEN, UID_LEN,
};
