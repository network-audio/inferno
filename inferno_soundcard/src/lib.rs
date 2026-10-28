//! Virtual sound cards on the Dante network, for Windows and macOS.
//!
//! One Inferno device carries all cards: their playback channels are its TX channels, their recording channels
//! its RX channels ([`engine`]). How they reach the operating system's audio differs:
//!
//! * Windows: each card is a USB Audio 2.0 device, emulated in this process and served over USB/IP on
//!   `127.0.0.1` ([usbip-audio](https://github.com/network-audio/usbip-audio)); the signed usbip-win2 driver
//!   attaches it and Windows' own USB audio driver makes it a sound device ([`usbip`])
//! * macOS: each card is a Core Audio device of the Inferno HAL plug-in (`inferno_coreaudio`), which plays and
//!   records through memory shared with this process ([`coreaudio`])
//!
//! No kernel driver of our own on either.

pub mod app;
pub mod config;
#[cfg(target_os = "macos")]
pub mod coreaudio;
pub mod engine;
#[cfg(windows)]
pub mod tray;
#[cfg(not(target_os = "macos"))]
pub mod usbip;
pub mod web;
