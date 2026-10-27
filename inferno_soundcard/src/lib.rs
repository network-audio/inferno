//! Windows virtual sound cards on the Dante network.
//!
//! Each card is a USB Audio 2.0 device, emulated in this process and served over USB/IP on `127.0.0.1`
//! ([usbip-audio](https://github.com/network-audio/usbip-audio)); the signed usbip-win2 driver attaches it and
//! Windows' own USB audio driver makes it a sound device. Its playback channels are the Inferno device's TX
//! channels, its recording channels the RX channels. No kernel driver of our own.

pub mod app;
pub mod config;
pub mod engine;
#[cfg(windows)]
pub mod tray;
pub mod web;
