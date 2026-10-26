//! Platform-neutral core of Inferno virtual soundcard drivers.
//!
//! Audio API integrations (the ALSA plugin, and future CoreAudio or Windows drivers) translate their
//! callbacks into a [`Stream`]'s lifecycle: `prepare`, `start`, `stop`, `close`, plus position queries.
//! This crate handles the rest:
//! * all streams of a process with the same device ID share one Dante device ([`SharedDevice`]), started on
//!   first use and shut down when its last stream closes
//! * the device transmits silence while only capturing, so receivers don't see a broken stream
//! * restarting a stream (transport stop/start, xrun recovery) keeps its flows and subscriptions
//! * waiting for the media clock, and saying what's wrong when it doesn't come

mod shared_device;
mod stream;

pub use shared_device::{SharedDevice, StreamClock, StreamStart};
pub use stream::{BuffersSignature, Prepared, Stream, StreamOptions};

/// Direction of a stream, from the application's point of view.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum Direction {
  /// the application records audio received from the network
  Capture,
  /// the application plays audio transmitted to the network
  Playback,
}

/// Why a stream can't be served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceError {
  /// The device failed to start (e.g. its ports are in use). It's forgotten, so a later attempt starts it again.
  StartFailed(String),
  /// BUG: the device thread has ended.
  ThreadEnded,
  /// No media clock arrived in time: the diagnosis says what to check.
  NoClock(String),
}

impl std::fmt::Display for DeviceError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::StartFailed(reason) => write!(f, "the device failed to start: {reason}"),
      Self::ThreadEnded => write!(f, "BUG: the Inferno thread has ended"),
      Self::NoClock(diagnosis) => write!(f, "no clock: {diagnosis}"),
    }
  }
}

impl std::error::Error for DeviceError {}
