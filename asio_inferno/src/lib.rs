//! Inferno ASIO driver: a Dante virtual soundcard for Windows audio applications (DAWs).
//!
//! `inferno_asio.dll` is an ASIO driver (a COM in-process server): register it with
//! `regsvr32 inferno_asio.dll` as administrator, and it's listed as "Inferno" by ASIO hosts. Each application
//! using it is a Dante device of its own, named after the application unless configured (see `config`).
//!
//! The ASIO interface is implemented from its published ABI, without Steinberg's SDK. x64 only.

pub mod config;
pub mod engine;

#[cfg(windows)]
pub mod asio;
