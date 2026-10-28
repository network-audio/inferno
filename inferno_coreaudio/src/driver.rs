//! The AudioServerPlugIn driver: the interface `coreaudiod` calls, the connection to the sound card service,
//! and the cards' clock and I/O.
//!
//! A thread of the plug-in looks up the service's Mach service and maps the region it hands out, then
//! publishes a device per card. It watches the region: when the service closes it (new settings, or it
//! quits) or stops updating its heartbeat (it crashed), the devices go, until there's a region again.

#![allow(non_upper_case_globals)] // Core Audio's names

use std::{
  ffi::{c_void, CString},
  panic::{catch_unwind, AssertUnwindSafe},
  sync::{
    atomic::{AtomicPtr, AtomicU32, Ordering},
    Arc, OnceLock, RwLock,
  },
  time::Duration,
};

use inferno_soundcard_shm::{
  mach::{self, Answer},
  underlying_now_ns, CardIo, Region, State, ZeroTimeStamps,
};

use crate::{abi::*, objects::*};

/// The service is gone when its heartbeat is older than this.
const HEARTBEAT_TIMEOUT_NS: i64 = 3_000_000_000;
/// Zero time stamps more than this off where the last one's rate predicts mean the clock jumped.
const JUMP_NS: f64 = 1_000_000.0;

static INTERFACE: AudioServerPlugInDriverInterface = AudioServerPlugInDriverInterface {
  _reserved: std::ptr::null(),
  QueryInterface: query_interface,
  AddRef: add_ref,
  Release: release,
  Initialize: initialize,
  CreateDevice: create_device,
  DestroyDevice: destroy_device,
  AddDeviceClient: add_device_client,
  RemoveDeviceClient: remove_device_client,
  PerformDeviceConfigurationChange: perform_device_configuration_change,
  AbortDeviceConfigurationChange: abort_device_configuration_change,
  HasProperty: has_property_fn,
  IsPropertySettable: is_property_settable,
  GetPropertyDataSize: get_property_data_size,
  GetPropertyData: get_property_data,
  SetPropertyData: set_property_data,
  StartIO: start_io,
  StopIO: stop_io,
  GetZeroTimeStamp: get_zero_time_stamp,
  WillDoIOOperation: will_do_io_operation,
  BeginIOOperation: begin_io_operation,
  DoIOOperation: do_io_operation,
  EndIOOperation: end_io_operation,
};

/// The driver reference (a pointer to the pointer to the interface, COM style).
struct DriverHandle(#[allow(dead_code)] *const AudioServerPlugInDriverInterface);
// safety: points to an immutable static
unsafe impl Sync for DriverHandle {}
static DRIVER: DriverHandle = DriverHandle(&INTERFACE);

fn driver_ref() -> *mut c_void {
  &DRIVER as *const DriverHandle as *mut c_void
}

static HOST: AtomicPtr<AudioServerPlugInHostInterface> = AtomicPtr::new(std::ptr::null_mut());
static REFERENCES: AtomicU32 = AtomicU32::new(1);
static DEVICES: RwLock<Option<Arc<DeviceSet>>> = RwLock::new(None);
/// the first object id of the next device set (the plug-in is 1)
static NEXT_ID: AtomicU32 = AtomicU32::new(2);

fn current() -> Option<Arc<DeviceSet>> {
  DEVICES.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Logs to the system log (`log show --predicate 'eventMessage CONTAINS "Inferno"'`).
fn log(error: bool, message: &str) {
  let Ok(message) = CString::new(format!("Inferno: {message}")) else { return };
  let priority = if error { libc::LOG_ERR } else { libc::LOG_NOTICE };
  // safety: a format with one string, and the string
  unsafe { libc::syslog(priority, c"%s".as_ptr(), message.as_ptr()) };
}

/// Runs `f`, or returns `fallback` if it panics (unwinding into Core Audio is undefined behaviour).
fn guard<T>(fallback: T, f: impl FnOnce() -> T) -> T {
  catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
    log(true, "BUG: panicked");
    fallback
  })
}

/// The plug-in's entry point, named in its `Info.plist`.
///
/// # Safety
/// Called by Core Foundation's plug-in loader with a valid type UUID.
#[no_mangle]
pub unsafe extern "C" fn InfernoDriverFactory(
  _allocator: CFAllocatorRef,
  type_uuid: CFUUIDRef,
) -> *mut c_void {
  if type_uuid.is_null() || CFUUIDGetUUIDBytes(type_uuid).0 != PLUGIN_TYPE_UUID {
    return std::ptr::null_mut();
  }
  driver_ref()
}

unsafe extern "C" fn query_interface(
  _driver: *mut c_void,
  uuid: CFUUIDBytes,
  out: *mut *mut c_void,
) -> HRESULT {
  if out.is_null() {
    return E_NOINTERFACE;
  }
  if uuid.0 == IUNKNOWN_UUID || uuid.0 == DRIVER_INTERFACE_UUID {
    REFERENCES.fetch_add(1, Ordering::Relaxed);
    *out = driver_ref();
    S_OK
  } else {
    *out = std::ptr::null_mut();
    E_NOINTERFACE
  }
}

unsafe extern "C" fn add_ref(_driver: *mut c_void) -> ULONG {
  REFERENCES.fetch_add(1, Ordering::Relaxed) + 1
}

/// The driver is static: never freed.
unsafe extern "C" fn release(_driver: *mut c_void) -> ULONG {
  REFERENCES
    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |r| Some(r.saturating_sub(1)))
    .unwrap_or(1)
    - 1
}

unsafe extern "C" fn initialize(_driver: DriverRef, host: HostRef) -> OSStatus {
  guard(kAudioHardwareUnspecifiedError, || {
    HOST.store(host as *mut _, Ordering::Release);
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
      log(false, concat!("plug-in ", env!("CARGO_PKG_VERSION"), " loaded"));
      let _ = std::thread::Builder::new().name("inferno connection".into()).spawn(connection);
    });
    kAudioHardwareNoError
  })
}

unsafe extern "C" fn create_device(
  _driver: DriverRef,
  _description: CFDictionaryRef,
  _client: *const c_void,
  _out: *mut AudioObjectID,
) -> OSStatus {
  kAudioHardwareUnsupportedOperationError
}

unsafe extern "C" fn destroy_device(_driver: DriverRef, _device: AudioObjectID) -> OSStatus {
  kAudioHardwareUnsupportedOperationError
}

unsafe extern "C" fn add_device_client(
  _driver: DriverRef,
  _device: AudioObjectID,
  _client: *const c_void,
) -> OSStatus {
  kAudioHardwareNoError
}

unsafe extern "C" fn remove_device_client(
  _driver: DriverRef,
  _device: AudioObjectID,
  _client: *const c_void,
) -> OSStatus {
  kAudioHardwareNoError
}

unsafe extern "C" fn perform_device_configuration_change(
  _driver: DriverRef,
  _device: AudioObjectID,
  _action: u64,
  _info: *mut c_void,
) -> OSStatus {
  kAudioHardwareNoError
}

unsafe extern "C" fn abort_device_configuration_change(
  _driver: DriverRef,
  _device: AudioObjectID,
  _action: u64,
  _info: *mut c_void,
) -> OSStatus {
  kAudioHardwareNoError
}

unsafe extern "C" fn has_property_fn(
  _driver: DriverRef,
  object: AudioObjectID,
  _pid: pid_t,
  address: *const AudioObjectPropertyAddress,
) -> Boolean {
  guard(0, || {
    if address.is_null() {
      return 0;
    }
    has_property(current().as_deref(), object, &*address) as Boolean
  })
}

unsafe extern "C" fn is_property_settable(
  _driver: DriverRef,
  object: AudioObjectID,
  _pid: pid_t,
  address: *const AudioObjectPropertyAddress,
  out: *mut Boolean,
) -> OSStatus {
  guard(kAudioHardwareUnspecifiedError, || {
    if address.is_null() || out.is_null() {
      return kAudioHardwareIllegalOperationError;
    }
    match is_settable(current().as_deref(), object, &*address) {
      Ok(settable) => {
        *out = settable as Boolean;
        kAudioHardwareNoError
      }
      Err(e) => e,
    }
  })
}

fn cf_string(s: &str) -> CFStringRef {
  // safety: valid UTF-8 bytes
  unsafe {
    CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as CFIndex, kCFStringEncodingUTF8, 0)
  }
}

/// # Safety
/// `s` must be a CFString or null.
unsafe fn string_of(s: CFStringRef) -> Option<String> {
  if s.is_null() {
    return None;
  }
  let size = CFStringGetMaximumSizeForEncoding(CFStringGetLength(s), kCFStringEncodingUTF8) + 1;
  let mut buffer = vec![0u8; size as usize];
  if CFStringGetCString(s, buffer.as_mut_ptr(), size, kCFStringEncodingUTF8) == 0 {
    return None;
  }
  let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
  buffer.truncate(end);
  String::from_utf8(buffer).ok()
}

/// The value of a property, with the UID of `kAudioPlugInPropertyTranslateUIDToDevice` from its qualifier.
unsafe fn value_of(
  object: AudioObjectID,
  address: &AudioObjectPropertyAddress,
  qualifier_size: u32,
  qualifier: *const c_void,
) -> Result<Value, OSStatus> {
  let uid = if address.mSelector == kAudioPlugInPropertyTranslateUIDToDevice {
    if qualifier.is_null() || (qualifier_size as usize) < size_of::<CFStringRef>() {
      return Err(kAudioHardwareBadPropertySizeError);
    }
    string_of(*(qualifier as *const CFStringRef))
  } else {
    None
  };
  property(current().as_deref(), object, address, uid.as_deref())
}

/// Bytes of a value: of each element of a list, and of all of it.
fn sizes(value: &Value) -> (usize, usize) {
  match value {
    Value::U32(_) => (4, 4),
    Value::F64(_) => (8, 8),
    Value::String(_) => (size_of::<CFStringRef>(), size_of::<CFStringRef>()),
    Value::U32s(v) => (4, 4 * v.len()),
    Value::Format(_) => {
      (size_of::<AudioStreamBasicDescription>(), size_of::<AudioStreamBasicDescription>())
    }
    Value::Formats(v) => {
      (size_of::<AudioStreamRangedDescription>(), size_of::<AudioStreamRangedDescription>() * v.len())
    }
    Value::Ranges(v) => (size_of::<AudioValueRange>(), size_of::<AudioValueRange>() * v.len()),
  }
}

unsafe extern "C" fn get_property_data_size(
  _driver: DriverRef,
  object: AudioObjectID,
  _pid: pid_t,
  address: *const AudioObjectPropertyAddress,
  qualifier_size: u32,
  qualifier: *const c_void,
  out_size: *mut u32,
) -> OSStatus {
  guard(kAudioHardwareUnspecifiedError, || {
    if address.is_null() || out_size.is_null() {
      return kAudioHardwareIllegalOperationError;
    }
    match value_of(object, &*address, qualifier_size, qualifier) {
      Ok(value) => {
        *out_size = sizes(&value).1 as u32;
        kAudioHardwareNoError
      }
      Err(e) => e,
    }
  })
}

/// Writes as many elements of `items` as fit in `size` bytes at `out`.
unsafe fn write_items<T: Copy>(items: &[T], size: u32, out: *mut c_void) -> u32 {
  let count = items.len().min(size as usize / size_of::<T>());
  std::ptr::copy_nonoverlapping(items.as_ptr(), out as *mut T, count);
  (count * size_of::<T>()) as u32
}

unsafe extern "C" fn get_property_data(
  _driver: DriverRef,
  object: AudioObjectID,
  _pid: pid_t,
  address: *const AudioObjectPropertyAddress,
  qualifier_size: u32,
  qualifier: *const c_void,
  size: u32,
  out_size: *mut u32,
  out: *mut c_void,
) -> OSStatus {
  guard(kAudioHardwareUnspecifiedError, || {
    if address.is_null() || out_size.is_null() || out.is_null() {
      return kAudioHardwareIllegalOperationError;
    }
    let value = match value_of(object, &*address, qualifier_size, qualifier) {
      Ok(value) => value,
      Err(e) => return e,
    };
    let (element, _) = sizes(&value);
    let is_list = matches!(value, Value::U32s(_) | Value::Formats(_) | Value::Ranges(_));
    if !is_list && (size as usize) < element {
      return kAudioHardwareBadPropertySizeError;
    }
    *out_size = match &value {
      Value::U32(v) => write_items(&[*v], size, out),
      Value::F64(v) => write_items(&[*v], size, out),
      // the caller releases it
      Value::String(s) => write_items(&[cf_string(s)], size, out),
      Value::U32s(v) => write_items(v, size, out),
      Value::Format(v) => write_items(&[*v], size, out),
      Value::Formats(v) => write_items(v, size, out),
      Value::Ranges(v) => write_items(v, size, out),
    };
    kAudioHardwareNoError
  })
}

unsafe extern "C" fn set_property_data(
  _driver: DriverRef,
  object: AudioObjectID,
  _pid: pid_t,
  address: *const AudioObjectPropertyAddress,
  _qualifier_size: u32,
  _qualifier: *const c_void,
  size: u32,
  data: *const c_void,
) -> OSStatus {
  guard(kAudioHardwareUnspecifiedError, || {
    if address.is_null() || data.is_null() {
      return kAudioHardwareIllegalOperationError;
    }
    let address = &*address;
    let value = match address.mSelector {
      kAudioDevicePropertyNominalSampleRate if size as usize >= size_of::<f64>() => {
        Value::F64(*(data as *const f64))
      }
      kAudioStreamPropertyVirtualFormat | kAudioStreamPropertyPhysicalFormat
        if size as usize >= size_of::<AudioStreamBasicDescription>() =>
      {
        Value::Format(*(data as *const AudioStreamBasicDescription))
      }
      _ => {
        return match is_settable(current().as_deref(), object, address) {
          Ok(_) => kAudioHardwareUnsupportedOperationError,
          Err(e) => e,
        }
      }
    };
    match set_property(current().as_deref(), object, address, &value) {
      Ok(()) => kAudioHardwareNoError,
      Err(e) => e,
    }
  })
}

/// Host time (Mach absolute time) units per nanosecond.
fn ticks_per_ns() -> f64 {
  static TIMEBASE: OnceLock<f64> = OnceLock::new();
  *TIMEBASE.get_or_init(|| {
    let mut info = mach2::mach_time::mach_timebase_info { numer: 0, denom: 0 };
    // safety: valid out pointer
    unsafe { mach2::mach_time::mach_timebase_info(&mut info) };
    if info.numer == 0 {
      1.0
    } else {
      info.denom as f64 / info.numer as f64
    }
  })
}

unsafe extern "C" fn start_io(_driver: DriverRef, device: AudioObjectID, _client: u32) -> OSStatus {
  guard(kAudioHardwareUnspecifiedError, || {
    let Some(set) = current() else { return kAudioHardwareBadObjectError };
    let Some(device) = set.device(device) else { return kAudioHardwareBadObjectError };
    let io = &device.io;
    if io.clients.fetch_add(1, Ordering::AcqRel) == 0 {
      let Some(now) = set.region.position_now() else {
        io.clients.fetch_sub(1, Ordering::AcqRel);
        return kAudioHardwareNotRunningError;
      };
      io.origin.store(now, Ordering::Release);
      io.last_sample_time.store(u64::MAX, Ordering::Relaxed);
      io.seed.fetch_add(1, Ordering::Relaxed);
    }
    kAudioHardwareNoError
  })
}

unsafe extern "C" fn stop_io(_driver: DriverRef, device: AudioObjectID, _client: u32) -> OSStatus {
  guard(kAudioHardwareUnspecifiedError, || {
    let Some(set) = current() else { return kAudioHardwareNoError };
    if let Some(device) = set.device(device) {
      let _ = device.io.clients.fetch_update(Ordering::AcqRel, Ordering::Acquire, |c| c.checked_sub(1));
    }
    kAudioHardwareNoError
  })
}

/// Core Audio's clock of the device: the media clock, every `ZERO_TIME_STAMP_PERIOD` frames of it.
unsafe extern "C" fn get_zero_time_stamp(
  _driver: DriverRef,
  device: AudioObjectID,
  _client: u32,
  out_sample_time: *mut f64,
  out_host_time: *mut u64,
  out_seed: *mut u64,
) -> OSStatus {
  guard(kAudioHardwareUnspecifiedError, || {
    let Some(set) = current() else { return kAudioHardwareBadObjectError };
    let Some(device) = set.device(device) else { return kAudioHardwareBadObjectError };
    let io = &device.io;
    let header = set.region.header();
    let last = (io.last_sample_time.load(Ordering::Relaxed), io.last_host_time.load(Ordering::Relaxed));
    let stamp = header.clock.read().map(|overlay| {
      let stamps = ZeroTimeStamps {
        rate: header.sample_rate,
        start_time: header.start_time.load(Ordering::Acquire) as usize,
        origin: io.origin.load(Ordering::Acquire),
        period: ZERO_TIME_STAMP_PERIOD as usize,
      };
      let underlying = underlying_now_ns(overlay.clock_id);
      let host_now = mach2::mach_time::mach_absolute_time();
      let (sample_time, until_ns) = stamps.latest(&overlay, underlying);
      let host = (host_now as f64 + until_ns as f64 * ticks_per_ns()).max(0.0) as u64;
      (sample_time, host)
    });
    // the clock is unavailable: the last one
    let (sample_time, host) = match stamp {
      Some(stamp) => stamp,
      None if last.0 != u64::MAX => last,
      None => return kAudioHardwareNotRunningError,
    };
    if last.0 != u64::MAX && (sample_time, host) != last {
      let frames = sample_time as f64 - last.0 as f64;
      let predicted = last.1 as f64 + frames * 1e9 / header.sample_rate as f64 * ticks_per_ns();
      if frames < 0.0 || (host as f64 - predicted).abs() > JUMP_NS * ticks_per_ns() {
        io.seed.fetch_add(1, Ordering::Relaxed);
      }
    }
    io.last_sample_time.store(sample_time, Ordering::Relaxed);
    io.last_host_time.store(host, Ordering::Relaxed);
    *out_sample_time = sample_time as f64;
    *out_host_time = host;
    *out_seed = io.seed.load(Ordering::Relaxed).max(1);
    kAudioHardwareNoError
  })
}

unsafe extern "C" fn will_do_io_operation(
  _driver: DriverRef,
  device: AudioObjectID,
  _client: u32,
  operation: u32,
  out_will_do: *mut Boolean,
  out_in_place: *mut Boolean,
) -> OSStatus {
  guard(kAudioHardwareUnspecifiedError, || {
    let Some(set) = current() else { return kAudioHardwareBadObjectError };
    let Some(device) = set.device(device) else { return kAudioHardwareBadObjectError };
    if !out_will_do.is_null() {
      *out_will_do = will_do(device, operation) as Boolean;
    }
    if !out_in_place.is_null() {
      *out_in_place = 1;
    }
    kAudioHardwareNoError
  })
}

unsafe extern "C" fn begin_io_operation(
  _driver: DriverRef,
  _device: AudioObjectID,
  _client: u32,
  _operation: u32,
  _frames: u32,
  _cycle: *const AudioServerPlugInIOCycleInfo,
) -> OSStatus {
  kAudioHardwareNoError
}

unsafe extern "C" fn end_io_operation(
  _driver: DriverRef,
  _device: AudioObjectID,
  _client: u32,
  _operation: u32,
  _frames: u32,
  _cycle: *const AudioServerPlugInIOCycleInfo,
) -> OSStatus {
  kAudioHardwareNoError
}

unsafe extern "C" fn do_io_operation(
  _driver: DriverRef,
  device: AudioObjectID,
  _stream: AudioObjectID,
  _client: u32,
  operation: u32,
  frames: u32,
  cycle: *const AudioServerPlugInIOCycleInfo,
  main: *mut c_void,
  _secondary: *mut c_void,
) -> OSStatus {
  guard(kAudioHardwareUnspecifiedError, || {
    let Some(set) = current() else { return kAudioHardwareBadObjectError };
    let Some(device) = set.device(device) else { return kAudioHardwareBadObjectError };
    if cycle.is_null() || main.is_null() {
      return kAudioHardwareIllegalOperationError;
    }
    let Some(card) = CardIo::new(&set.region, device.card) else { return kAudioHardwareBadObjectError };
    let origin = device.io.origin.load(Ordering::Acquire);
    let frames = frames as usize;
    let at = |time: &AudioTimeStamp| origin.wrapping_add(time.mSampleTime.round().max(0.0) as usize);
    match operation {
      kAudioServerPlugInIOOperationReadInput => {
        let channels = card.info().capture_channels as usize;
        let buffer = std::slice::from_raw_parts_mut(main as *mut f32, frames * channels);
        let position = at(&(*cycle).mInputTime);
        let first = card.info().first_capture as usize;
        let readable = set.region.capture_readable()[first].load(Ordering::Relaxed);
        device.io.timing.input(readable as i64 - (position + frames) as i64);
        card.read_input(position, frames, buffer);
      }
      kAudioServerPlugInIOOperationWriteMix => {
        let channels = card.info().playback_channels as usize;
        let buffer = std::slice::from_raw_parts(main as *const f32, frames * channels);
        let position = at(&(*cycle).mOutputTime);
        let now = set.region.position_now();
        if let Some(now) = now {
          let lead = set.region.header().playback_lead as usize;
          device.io.timing.output((position + lead) as i64 - now as i64);
        }
        card.write_mix(position, frames, buffer, now);
      }
      _ => {}
    }
    kAudioHardwareNoError
  })
}

/// Tells Core Audio that properties of `object` changed.
fn changed(object: AudioObjectID, selectors: &[u32]) {
  let host = HOST.load(Ordering::Acquire);
  if host.is_null() {
    return;
  }
  let addresses: Vec<_> = selectors
    .iter()
    .map(|&s| AudioObjectPropertyAddress {
      mSelector: s,
      mScope: kAudioObjectPropertyScopeGlobal,
      mElement: kAudioObjectPropertyElementMain,
    })
    .collect();
  // safety: the host Core Audio gave, valid while the plug-in is loaded
  unsafe { ((*host).PropertiesChanged)(host, object, addresses.len() as u32, addresses.as_ptr()) };
}

fn set_devices(set: Option<DeviceSet>) {
  let old =
    std::mem::replace(&mut *DEVICES.write().unwrap_or_else(|e| e.into_inner()), set.map(Arc::new));
  if let Some(old) = &old {
    for device in &old.devices {
      changed(device.id, &[kAudioDevicePropertyDeviceIsAlive]);
    }
  }
  changed(kAudioObjectPlugInObject, &[kAudioPlugInPropertyDeviceList, kAudioObjectPropertyOwnedObjects]);
}

/// Why the region can't be used (any more), if it can't.
fn unusable(region: &Region) -> Option<&'static str> {
  let header = region.header();
  if header.state() != Some(State::Running) {
    return Some("the service closed the cards");
  }
  let heartbeat = header.heartbeat_ns.load(Ordering::Acquire) as i64;
  if underlying_now_ns(libc::CLOCK_MONOTONIC as i64) - heartbeat > HEARTBEAT_TIMEOUT_NS {
    return Some("the service stopped responding");
  }
  None
}

/// Gets the service's region, if it has one ready.
fn connect() -> Result<Option<Region>, String> {
  let port = mach::look_up(mach::SERVICE_NAME)?;
  let answer = mach::request(port, 2000);
  mach::release(port);
  match answer? {
    Answer::Region(memory) => {
      let (ptr, len) = (memory.ptr(), memory.len());
      // safety: mapped for as long as the region owns it
      let region = unsafe { Region::open(ptr, len, Box::new(memory)) }?;
      match unusable(&region) {
        Some(why) => Err(why.into()),
        None => Ok(Some(region)),
      }
    }
    Answer::None => Ok(None),
    Answer::Denied => Err("the service refused the cards to Core Audio".into()),
  }
}

/// Keeps the devices in line with the service's region, forever.
fn connection() {
  // what was last logged, not to log it again and again
  let mut last = String::new();
  // the I/O cycles' timing is logged every 5 s
  let mut logged = 0u64;
  let mut report = |message: String, error: bool| {
    if last != message {
      log(error, &message);
      last = message;
    }
  };
  loop {
    match current() {
      None => match connect() {
        Ok(Some(region)) => {
          let set = DeviceSet::new(Arc::new(region), NEXT_ID.load(Ordering::Relaxed));
          NEXT_ID.store(set.next_id(), Ordering::Relaxed);
          let names: Vec<_> = set.region.header().cards().iter().map(|c| c.name().to_string()).collect();
          report(format!("the sound cards: {}", names.join(", ")), false);
          set_devices(Some(set));
        }
        Ok(None) => {
          report("the sound card service has no cards yet".into(), false);
          std::thread::sleep(Duration::from_secs(1));
        }
        Err(e) => {
          report(e, true);
          std::thread::sleep(Duration::from_secs(1));
        }
      },
      Some(set) => {
        if let Some(why) = unusable(&set.region) {
          report(format!("{why}: the sound cards are gone until it's back"), false);
          drop(set);
          set_devices(None);
        } else {
          std::thread::sleep(Duration::from_millis(100));
          logged += 1;
          if logged % 50 == 0 {
            let cards = set.region.header().cards();
            for device in &set.devices {
              if let Some(timing) = device.io.timing.take() {
                log(false, &format!("{}: {timing}", cards[device.card].name()));
              }
            }
          }
        }
      }
    }
  }
}
