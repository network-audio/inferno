//! The ASIO driver object: IASIO methods on top of the engine.

use std::{
  ffi::{c_char, c_long, c_void},
  sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex, RwLock,
  },
};

use inferno_aoip::device_server::Settings;
use inferno_driver_core::DeviceError;
use log::{debug, info};
use windows_sys::Win32::Foundation::{E_NOINTERFACE, E_POINTER, S_OK};

use super::{
  abi::*,
  com::{object_created, object_destroyed, CLSID_INFERNO_ASIO, IID_IUNKNOWN},
};
use crate::{
  config,
  engine::{report, Engine, Host, Position},
};

const DRIVER_NAME: &str = "Inferno";
const DRIVER_VERSION: c_long = 1;
const MIN_BUFFER: c_long = 32;
const MAX_BUFFER: c_long = 4096;
const PREFERRED_BUFFER: c_long = 256;

#[derive(Default)]
struct State {
  settings: Option<Settings>,
  engine: Option<Engine>,
  callbacks: Option<AsioCallbacks>,
  active_inputs: Vec<bool>,
  active_outputs: Vec<bool>,
}

impl State {
  fn clear_active(&mut self) {
    self.active_inputs.iter_mut().chain(self.active_outputs.iter_mut()).for_each(|a| *a = false);
  }
}

#[repr(C)]
pub struct AsioDriver {
  vtbl: &'static IAsioVtbl,
  refs: AtomicU32,
  state: Mutex<State>,
  /// outside `state`: the host may ask for it from within a buffer switch
  position: RwLock<Option<Arc<Position>>>,
  last_error: Mutex<String>,
}

/// Calls the host on each buffer switch.
struct AsioHost {
  callbacks: AsioCallbacks,
  time_info: bool,
  sample_rate: f64,
  time: AsioTime,
}

impl Host for AsioHost {
  fn buffer_switch(&mut self, index: usize, position: u64, system_time_ns: u64) {
    if self.time_info {
      let info = &mut self.time.time_info;
      info.speed = 1.0;
      info.sample_position = position.into();
      info.system_time = system_time_ns.into();
      info.sample_rate = self.sample_rate;
      info.flags = K_SYSTEM_TIME_VALID | K_SAMPLE_POSITION_VALID | K_SAMPLE_RATE_VALID | K_SPEED_VALID;
      (self.callbacks.buffer_switch_time_info)(&mut self.time, index as c_long, ASIO_TRUE);
    } else {
      (self.callbacks.buffer_switch)(index as c_long, ASIO_TRUE);
    }
  }
}

fn copy_str(dst: *mut c_char, capacity: usize, s: &str) {
  if dst.is_null() {
    return;
  }
  let bytes = s.as_bytes();
  let len = bytes.len().min(capacity - 1);
  // safety: the host provides `capacity` bytes (ASIO: 32 for names, 124 for messages)
  unsafe {
    std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, dst, len);
    *dst.add(len) = 0;
  }
}

fn error_code(e: &DeviceError) -> AsioError {
  match e {
    DeviceError::NoClock(_) => ASE_NO_CLOCK,
    DeviceError::StartFailed(_) | DeviceError::ThreadEnded => ASE_HW_MALFUNCTION,
  }
}

unsafe fn driver<'a>(this: *mut c_void) -> &'a AsioDriver {
  &*(this as *const AsioDriver)
}

impl AsioDriver {
  pub fn create() -> *mut AsioDriver {
    object_created();
    Box::into_raw(Box::new(AsioDriver {
      vtbl: &VTBL,
      refs: AtomicU32::new(1),
      state: Mutex::new(State::default()),
      position: RwLock::new(None),
      last_error: Mutex::new(String::new()),
    }))
  }

  fn fail(&self, message: impl Into<String>) {
    report(&self.last_error, message);
  }

  /// Takes the engine out of the state, so that it can be stopped without holding the lock (the host may call
  /// the driver from within a buffer switch, which stopping waits for).
  fn with_engine_unlocked<R>(&self, f: impl FnOnce(&mut Option<Engine>) -> R) -> R {
    let mut engine = self.state.lock().unwrap().engine.take();
    let r = f(&mut engine);
    if engine.is_some() {
      self.state.lock().unwrap().engine = engine;
    }
    r
  }
}

pub unsafe extern "system" fn query_interface(
  this: *mut c_void,
  riid: *const Guid,
  out: *mut *mut c_void,
) -> HResult {
  if out.is_null() || riid.is_null() {
    return E_POINTER;
  }
  let riid = &*riid;
  // ASIO hosts ask for the driver's CLSID as the interface ID
  if guid_eq(riid, &IID_IUNKNOWN) || guid_eq(riid, &CLSID_INFERNO_ASIO) {
    add_ref(this);
    *out = this;
    S_OK
  } else {
    *out = std::ptr::null_mut();
    E_NOINTERFACE
  }
}

pub fn guid_eq(a: &Guid, b: &Guid) -> bool {
  a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

pub unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
  driver(this).refs.fetch_add(1, Ordering::AcqRel) + 1
}

pub unsafe extern "system" fn release(this: *mut c_void) -> u32 {
  let refs = driver(this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
  if refs == 0 {
    drop(Box::from_raw(this as *mut AsioDriver));
    object_destroyed();
  }
  refs
}

unsafe extern "system" fn init(this: *mut c_void, _sys_handle: *mut c_void) -> AsioBool {
  config::init_logging();
  let d = driver(this);
  match config::load_settings() {
    Ok(settings) => {
      info!(
        "init: {} RX, {} TX channels at {} Hz",
        settings.self_info.rx_channels.len(),
        settings.self_info.tx_channels.len(),
        settings.self_info.sample_rate
      );
      let mut state = d.state.lock().unwrap();
      state.active_inputs = vec![false; settings.self_info.rx_channels.len()];
      state.active_outputs = vec![false; settings.self_info.tx_channels.len()];
      state.settings = Some(settings);
      ASIO_TRUE
    }
    Err(e) => {
      d.fail(e);
      ASIO_FALSE
    }
  }
}

unsafe extern "system" fn get_driver_name(_this: *mut c_void, name: *mut c_char) {
  copy_str(name, 32, DRIVER_NAME);
}

unsafe extern "system" fn get_driver_version(_this: *mut c_void) -> c_long {
  DRIVER_VERSION
}

unsafe extern "system" fn get_error_message(this: *mut c_void, message: *mut c_char) {
  copy_str(message, 124, &driver(this).last_error.lock().unwrap());
}

unsafe extern "system" fn start(this: *mut c_void) -> AsioError {
  let d = driver(this);
  let mut state = d.state.lock().unwrap();
  let (Some(callbacks), Some(settings)) = (state.callbacks, state.settings.as_ref()) else {
    return ASE_INVALID_MODE;
  };
  let time_info = (callbacks.asio_message)(
    K_ASIO_SELECTOR_SUPPORTED,
    K_ASIO_SUPPORTS_TIME_INFO,
    std::ptr::null_mut(),
    std::ptr::null_mut(),
  ) == 1
    && (callbacks.asio_message)(K_ASIO_SUPPORTS_TIME_INFO, 0, std::ptr::null_mut(), std::ptr::null_mut())
      == 1;
  let host = AsioHost {
    callbacks,
    time_info,
    sample_rate: settings.self_info.sample_rate as f64,
    time: AsioTime::default(),
  };
  let Some(engine) = state.engine.as_mut() else {
    return ASE_INVALID_MODE;
  };
  match engine.start(Box::new(host)) {
    Ok(()) => {
      debug!("start (time info: {time_info})");
      ASE_OK
    }
    Err(e) => {
      drop(state);
      d.fail(format!("can't start: {e}"));
      error_code(&e)
    }
  }
}

unsafe extern "system" fn stop(this: *mut c_void) -> AsioError {
  driver(this).with_engine_unlocked(|engine| {
    if let Some(engine) = engine {
      engine.stop();
    }
  });
  ASE_OK
}

unsafe extern "system" fn get_channels(
  this: *mut c_void,
  inputs: *mut c_long,
  outputs: *mut c_long,
) -> AsioError {
  let state = driver(this).state.lock().unwrap();
  let Some(settings) = &state.settings else {
    return ASE_NOT_PRESENT;
  };
  if !inputs.is_null() {
    *inputs = settings.self_info.rx_channels.len() as c_long;
  }
  if !outputs.is_null() {
    *outputs = settings.self_info.tx_channels.len() as c_long;
  }
  ASE_OK
}

unsafe extern "system" fn get_latencies(
  this: *mut c_void,
  input: *mut c_long,
  output: *mut c_long,
) -> AsioError {
  let state = driver(this).state.lock().unwrap();
  let Some(settings) = &state.settings else {
    return ASE_NOT_PRESENT;
  };
  let period = state.engine.as_ref().map(|e| e.period() as c_long).unwrap_or(PREFERRED_BUFFER);
  let info = &settings.self_info;
  let tx_latency = (settings.tx_latency_ns as u64 * info.sample_rate as u64 / 1_000_000_000) as c_long;
  // inputs: a period, plus the network latency; outputs: written 2 periods ahead of being due
  if !input.is_null() {
    *input = period + info.latency_samples() as c_long;
  }
  if !output.is_null() {
    *output = 2 * period + tx_latency;
  }
  ASE_OK
}

unsafe extern "system" fn get_buffer_size(
  _this: *mut c_void,
  min: *mut c_long,
  max: *mut c_long,
  preferred: *mut c_long,
  granularity: *mut c_long,
) -> AsioError {
  for (p, v) in [(min, MIN_BUFFER), (max, MAX_BUFFER), (preferred, PREFERRED_BUFFER), (granularity, -1)] {
    if !p.is_null() {
      *p = v; // granularity -1: powers of 2
    }
  }
  ASE_OK
}

fn sample_rate(this: *mut c_void) -> Option<f64> {
  unsafe { driver(this) }.state.lock().unwrap().settings.as_ref().map(|s| s.self_info.sample_rate as f64)
}

unsafe extern "system" fn can_sample_rate(this: *mut c_void, rate: AsioSampleRate) -> AsioError {
  match sample_rate(this) {
    Some(r) if r == rate => ASE_OK,
    Some(_) => ASE_NO_CLOCK,
    None => ASE_NOT_PRESENT,
  }
}

unsafe extern "system" fn get_sample_rate(this: *mut c_void, rate: *mut AsioSampleRate) -> AsioError {
  match (sample_rate(this), rate.is_null()) {
    (Some(r), false) => {
      *rate = r;
      ASE_OK
    }
    (None, _) => ASE_NOT_PRESENT,
    (_, true) => ASE_INVALID_PARAMETER,
  }
}

unsafe extern "system" fn set_sample_rate(this: *mut c_void, rate: AsioSampleRate) -> AsioError {
  match sample_rate(this) {
    // 0: the host asks for external sync, which the network clock is
    Some(r) if r == rate || rate == 0.0 => ASE_OK,
    Some(r) => {
      driver(this)
        .fail(format!("the Dante network runs at {r} Hz (SAMPLE_RATE): can't switch to {rate} Hz"));
      ASE_NO_CLOCK
    }
    None => ASE_NOT_PRESENT,
  }
}

unsafe extern "system" fn get_clock_sources(
  _this: *mut c_void,
  clocks: *mut AsioClockSource,
  count: *mut c_long,
) -> AsioError {
  if clocks.is_null() || count.is_null() || *count < 1 {
    return ASE_INVALID_PARAMETER;
  }
  let clock = &mut *clocks;
  clock.index = 0;
  clock.associated_channel = -1;
  clock.associated_group = -1;
  clock.is_current_source = ASIO_TRUE;
  copy_str(clock.name.as_mut_ptr(), 32, "Dante network (PTP)");
  *count = 1;
  ASE_OK
}

unsafe extern "system" fn set_clock_source(_this: *mut c_void, reference: c_long) -> AsioError {
  if reference == 0 {
    ASE_OK
  } else {
    ASE_INVALID_PARAMETER
  }
}

unsafe extern "system" fn get_sample_position(
  this: *mut c_void,
  position: *mut Asio64,
  time_stamp: *mut Asio64,
) -> AsioError {
  let guard = driver(this).position.read().unwrap();
  let Some(p) = guard.as_ref().filter(|p| p.running.load(Ordering::Acquire)) else {
    return ASE_SP_NOT_ADVANCING;
  };
  if !position.is_null() {
    *position = p.samples.load(Ordering::Acquire).into();
  }
  if !time_stamp.is_null() {
    *time_stamp = p.system_time_ns.load(Ordering::Acquire).into();
  }
  ASE_OK
}

unsafe extern "system" fn get_channel_info(this: *mut c_void, info: *mut AsioChannelInfo) -> AsioError {
  if info.is_null() {
    return ASE_INVALID_PARAMETER;
  }
  let state = driver(this).state.lock().unwrap();
  let Some(settings) = &state.settings else {
    return ASE_NOT_PRESENT;
  };
  let info = &mut *info;
  let (channels, active) = if info.is_input != 0 {
    (&settings.self_info.rx_channels, &state.active_inputs)
  } else {
    (&settings.self_info.tx_channels, &state.active_outputs)
  };
  let index = info.channel;
  let Some(channel) = usize::try_from(index).ok().and_then(|i| channels.get(i)) else {
    return ASE_INVALID_PARAMETER;
  };
  info.is_active = active[index as usize] as AsioBool;
  info.channel_group = 0;
  info.sample_type = ASIO_ST_INT32_LSB;
  copy_str(info.name.as_mut_ptr(), 32, &channel.friendly_name.read().unwrap());
  ASE_OK
}

unsafe extern "system" fn create_buffers(
  this: *mut c_void,
  infos: *mut AsioBufferInfo,
  channels: c_long,
  buffer_size: c_long,
  callbacks: *const AsioCallbacks,
) -> AsioError {
  let d = driver(this);
  if infos.is_null() || callbacks.is_null() || channels <= 0 {
    return ASE_INVALID_PARAMETER;
  }
  if !(16..=8192).contains(&buffer_size) {
    d.fail(format!("unsupported buffer size {buffer_size}"));
    return ASE_INVALID_MODE;
  }
  let mut state = d.state.lock().unwrap();
  if state.engine.is_some() {
    return ASE_INVALID_MODE;
  }
  let Some(settings) = state.settings.clone() else {
    return ASE_NOT_PRESENT;
  };
  info!("create_buffers: {channels} channels, {buffer_size} frames");
  let engine = match Engine::new(settings, buffer_size as usize) {
    Ok(engine) => engine,
    Err(e) => {
      drop(state);
      d.fail(format!("can't start the Dante device: {e}"));
      return error_code(&e);
    }
  };
  let infos = std::slice::from_raw_parts_mut(infos, channels as usize);
  state.clear_active();
  for info in infos.iter_mut() {
    let channel = info.channel_num as usize;
    let input = info.is_input != 0;
    let buffers = [0, 1].map(|half| {
      if input {
        engine.input_buffer(channel, half)
      } else {
        engine.output_buffer(channel, half)
      }
    });
    let [Some(b0), Some(b1)] = buffers else {
      drop(state);
      d.fail(format!("no {} channel {channel}", if input { "input" } else { "output" }));
      return ASE_INVALID_PARAMETER;
    };
    info.buffers = [b0 as *mut c_void, b1 as *mut c_void];
    if input {
      state.active_inputs[channel] = true;
    } else {
      state.active_outputs[channel] = true;
    }
  }
  *d.position.write().unwrap() = Some(engine.position());
  state.callbacks = Some(*callbacks);
  state.engine = Some(engine);
  ASE_OK
}

unsafe extern "system" fn dispose_buffers(this: *mut c_void) -> AsioError {
  let d = driver(this);
  // dropping the engine stops it (without the lock) and closes its streams
  d.with_engine_unlocked(|engine| drop(engine.take()));
  let mut state = d.state.lock().unwrap();
  state.callbacks = None;
  state.clear_active();
  *d.position.write().unwrap() = None;
  ASE_OK
}

unsafe extern "system" fn control_panel(this: *mut c_void) -> AsioError {
  match config::open_config_file() {
    Ok(()) => ASE_OK,
    Err(e) => {
      driver(this).fail(format!("can't open the settings file: {e}"));
      ASE_NOT_PRESENT
    }
  }
}

unsafe extern "system" fn future(_this: *mut c_void, selector: c_long, _opt: *mut c_void) -> AsioError {
  match selector {
    K_ASIO_CAN_TIME_INFO => ASE_SUCCESS,
    _ => ASE_NOT_PRESENT,
  }
}

unsafe extern "system" fn output_ready(_this: *mut c_void) -> AsioError {
  ASE_NOT_PRESENT
}

static VTBL: IAsioVtbl = IAsioVtbl {
  query_interface,
  add_ref,
  release,
  init,
  get_driver_name,
  get_driver_version,
  get_error_message,
  start,
  stop,
  get_channels,
  get_latencies,
  get_buffer_size,
  can_sample_rate,
  get_sample_rate,
  set_sample_rate,
  get_clock_sources,
  set_clock_source,
  get_sample_position,
  get_channel_info,
  create_buffers,
  dispose_buffers,
  control_panel,
  future,
  output_ready,
};
