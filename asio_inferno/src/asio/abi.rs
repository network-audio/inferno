//! The ASIO 2 driver ABI (x64 Windows): the IASIO COM interface and its types. Written from the published
//! interface, not taken from Steinberg's SDK. Structures use the SDK's 4-byte packing on Windows.

#![allow(non_camel_case_types, non_snake_case, dead_code)]

use std::ffi::{c_char, c_long, c_void};

pub type AsioBool = c_long;
pub type AsioError = c_long;
pub type AsioSampleRate = f64;
pub type AsioSampleType = c_long;

pub const ASIO_TRUE: AsioBool = 1;
pub const ASIO_FALSE: AsioBool = 0;

pub const ASE_OK: AsioError = 0;
pub const ASE_SUCCESS: AsioError = 0x3f4847a0;
pub const ASE_NOT_PRESENT: AsioError = -1000;
pub const ASE_HW_MALFUNCTION: AsioError = -999;
pub const ASE_INVALID_PARAMETER: AsioError = -998;
pub const ASE_INVALID_MODE: AsioError = -997;
pub const ASE_SP_NOT_ADVANCING: AsioError = -996;
pub const ASE_NO_CLOCK: AsioError = -995;
pub const ASE_NO_MEMORY: AsioError = -994;

pub const ASIO_ST_INT32_LSB: AsioSampleType = 18;

/// `future` selector: does the driver support `bufferSwitchTimeInfo`
pub const K_ASIO_CAN_TIME_INFO: c_long = 10;

/// `asioMessage` selectors
pub const K_ASIO_SELECTOR_SUPPORTED: c_long = 1;
pub const K_ASIO_ENGINE_VERSION: c_long = 2;
pub const K_ASIO_RESET_REQUEST: c_long = 3;
pub const K_ASIO_SUPPORTS_TIME_INFO: c_long = 7;

/// `AsioTimeInfo` flags
pub const K_SYSTEM_TIME_VALID: u32 = 1;
pub const K_SAMPLE_POSITION_VALID: u32 = 2;
pub const K_SAMPLE_RATE_VALID: u32 = 4;
pub const K_SPEED_VALID: u32 = 8;

/// 64-bit value as two 32-bit halves (`ASIOSamples`, `ASIOTimeStamp`)
#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
pub struct Asio64 {
  pub hi: u32,
  pub lo: u32,
}

impl From<u64> for Asio64 {
  fn from(v: u64) -> Self {
    Self { hi: (v >> 32) as u32, lo: v as u32 }
  }
}

#[repr(C, packed(4))]
pub struct AsioClockSource {
  pub index: c_long,
  pub associated_channel: c_long,
  pub associated_group: c_long,
  pub is_current_source: AsioBool,
  pub name: [c_char; 32],
}

#[repr(C, packed(4))]
pub struct AsioChannelInfo {
  pub channel: c_long,
  pub is_input: AsioBool,
  pub is_active: AsioBool,
  pub channel_group: c_long,
  pub sample_type: AsioSampleType,
  pub name: [c_char; 32],
}

#[repr(C, packed(4))]
pub struct AsioBufferInfo {
  pub is_input: AsioBool,
  pub channel_num: c_long,
  pub buffers: [*mut c_void; 2],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy, Default)]
pub struct AsioTimeInfo {
  pub speed: f64,
  pub system_time: Asio64,
  pub sample_position: Asio64,
  pub sample_rate: AsioSampleRate,
  pub flags: u32,
  pub reserved: [c_char; 12],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTimeCode {
  pub speed: f64,
  pub time_code_samples: Asio64,
  pub flags: u32,
  pub future: [c_char; 64],
}

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTime {
  pub reserved: [c_long; 4],
  pub time_info: AsioTimeInfo,
  pub time_code: AsioTimeCode,
}

impl Default for AsioTime {
  fn default() -> Self {
    Self {
      reserved: [0; 4],
      time_info: AsioTimeInfo::default(),
      time_code: AsioTimeCode {
        speed: 0.0,
        time_code_samples: Asio64::default(),
        flags: 0,
        future: [0; 64],
      },
    }
  }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AsioCallbacks {
  pub buffer_switch: extern "C" fn(double_buffer_index: c_long, direct_process: AsioBool),
  pub sample_rate_did_change: extern "C" fn(rate: AsioSampleRate),
  pub asio_message:
    extern "C" fn(selector: c_long, value: c_long, message: *mut c_void, opt: *mut f64) -> c_long,
  pub buffer_switch_time_info: extern "C" fn(
    params: *mut AsioTime,
    double_buffer_index: c_long,
    direct_process: AsioBool,
  ) -> *mut AsioTime,
}

pub type Guid = windows_sys::core::GUID;
pub type HResult = windows_sys::core::HRESULT;

/// IUnknown followed by IASIO, in vtable order.
#[repr(C)]
pub struct IAsioVtbl {
  pub query_interface:
    unsafe extern "system" fn(this: *mut c_void, riid: *const Guid, out: *mut *mut c_void) -> HResult,
  pub add_ref: unsafe extern "system" fn(this: *mut c_void) -> u32,
  pub release: unsafe extern "system" fn(this: *mut c_void) -> u32,
  pub init: unsafe extern "system" fn(this: *mut c_void, sys_handle: *mut c_void) -> AsioBool,
  pub get_driver_name: unsafe extern "system" fn(this: *mut c_void, name: *mut c_char),
  pub get_driver_version: unsafe extern "system" fn(this: *mut c_void) -> c_long,
  pub get_error_message: unsafe extern "system" fn(this: *mut c_void, message: *mut c_char),
  pub start: unsafe extern "system" fn(this: *mut c_void) -> AsioError,
  pub stop: unsafe extern "system" fn(this: *mut c_void) -> AsioError,
  pub get_channels:
    unsafe extern "system" fn(this: *mut c_void, inputs: *mut c_long, outputs: *mut c_long) -> AsioError,
  pub get_latencies:
    unsafe extern "system" fn(this: *mut c_void, input: *mut c_long, output: *mut c_long) -> AsioError,
  pub get_buffer_size: unsafe extern "system" fn(
    this: *mut c_void,
    min: *mut c_long,
    max: *mut c_long,
    preferred: *mut c_long,
    granularity: *mut c_long,
  ) -> AsioError,
  pub can_sample_rate: unsafe extern "system" fn(this: *mut c_void, rate: AsioSampleRate) -> AsioError,
  pub get_sample_rate:
    unsafe extern "system" fn(this: *mut c_void, rate: *mut AsioSampleRate) -> AsioError,
  pub set_sample_rate: unsafe extern "system" fn(this: *mut c_void, rate: AsioSampleRate) -> AsioError,
  pub get_clock_sources: unsafe extern "system" fn(
    this: *mut c_void,
    clocks: *mut AsioClockSource,
    count: *mut c_long,
  ) -> AsioError,
  pub set_clock_source: unsafe extern "system" fn(this: *mut c_void, reference: c_long) -> AsioError,
  pub get_sample_position: unsafe extern "system" fn(
    this: *mut c_void,
    position: *mut Asio64,
    time_stamp: *mut Asio64,
  ) -> AsioError,
  pub get_channel_info:
    unsafe extern "system" fn(this: *mut c_void, info: *mut AsioChannelInfo) -> AsioError,
  pub create_buffers: unsafe extern "system" fn(
    this: *mut c_void,
    infos: *mut AsioBufferInfo,
    channels: c_long,
    buffer_size: c_long,
    callbacks: *const AsioCallbacks,
  ) -> AsioError,
  pub dispose_buffers: unsafe extern "system" fn(this: *mut c_void) -> AsioError,
  pub control_panel: unsafe extern "system" fn(this: *mut c_void) -> AsioError,
  pub future:
    unsafe extern "system" fn(this: *mut c_void, selector: c_long, opt: *mut c_void) -> AsioError,
  pub output_ready: unsafe extern "system" fn(this: *mut c_void) -> AsioError,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn layouts_match_the_sdk() {
    assert_eq!(size_of::<AsioClockSource>(), 48);
    assert_eq!(size_of::<AsioChannelInfo>(), 52);
    assert_eq!(size_of::<AsioBufferInfo>(), 24);
    assert_eq!(size_of::<AsioTimeInfo>(), 48);
    assert_eq!(size_of::<AsioTimeCode>(), 84);
    assert_eq!(size_of::<AsioTime>(), 148);
    assert_eq!(size_of::<IAsioVtbl>(), 24 * size_of::<usize>());
  }
}
