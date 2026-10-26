//! Minimal ASIO host for testing the driver without a DAW: loads the driver DLL the way hosts do (class
//! factory, IASIO), plays a file to its outputs and records its inputs (interleaved signed 32-bit samples,
//! like Inferno2pipe and Pipe2inferno), until a file appears or a time limit.
//!
//! Usage: asio_host <driver.dll> [--play FILE] [--record FILE] [--buffer FRAMES] [--seconds N] [--until-file F]

#[cfg(not(windows))]
fn main() {
  eprintln!("ASIO hosts run on Windows");
  std::process::exit(2);
}

#[cfg(windows)]
fn main() {
  std::process::exit(host::run());
}

#[cfg(windows)]
mod host {
  use std::{
    ffi::{c_char, c_long, c_void, CStr},
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
  };

  use inferno_asio::asio::{abi::*, com::CLSID_INFERNO_ASIO};
  use windows_sys::{
    core::GUID,
    Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW},
  };

  const IID_ICLASSFACTORY: GUID =
    GUID { data1: 1, data2: 0, data3: 0, data4: [0xc0, 0, 0, 0, 0, 0, 0, 0x46] };

  #[repr(C)]
  struct ClassFactoryVtbl {
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    create_instance:
      unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, *mut *mut c_void) -> HResult,
    lock_server: usize,
  }

  struct Buffer {
    input: bool,
    channel: usize,
    halves: [*mut i32; 2],
  }

  struct State {
    buffers: Vec<Buffer>,
    period: usize,
    inputs: usize,
    outputs: usize,
    play: Vec<i32>,
    play_frame: usize,
    record: Vec<i32>,
    switches: u64,
    last_position: Option<u64>,
    position_errors: u64,
    driver: *mut c_void,
    vtbl: *const IAsioVtbl,
  }
  unsafe impl Send for State {}

  static STATE: Mutex<Option<State>> = Mutex::new(None);

  fn process(index: usize) {
    let mut guard = STATE.lock().unwrap();
    let Some(s) = guard.as_mut() else { return };
    let n = s.period;
    s.switches += 1;
    let mut position = Asio64::default();
    let mut time = Asio64::default();
    if unsafe { ((*s.vtbl).get_sample_position)(s.driver, &mut position, &mut time) } == ASE_OK {
      let p = ((position.hi as u64) << 32) | position.lo as u64;
      if s.last_position.is_some_and(|last| p != last + n as u64) {
        s.position_errors += 1;
      }
      s.last_position = Some(p);
    }
    let record_start = s.record.len();
    s.record.resize(record_start + n * s.inputs, 0);
    for b in &s.buffers {
      let half = unsafe { std::slice::from_raw_parts_mut(b.halves[index], n) };
      if b.input {
        for (i, v) in half.iter().enumerate() {
          s.record[record_start + i * s.inputs + b.channel] = *v;
        }
      } else if s.play.is_empty() {
        half.fill(0);
      } else {
        let frames = s.play.len() / s.outputs;
        for (i, v) in half.iter_mut().enumerate() {
          *v = s.play[((s.play_frame + i) % frames) * s.outputs + b.channel];
        }
      }
    }
    s.play_frame += n;
  }

  extern "C" fn buffer_switch(index: c_long, _direct: AsioBool) {
    process(index as usize);
  }
  extern "C" fn buffer_switch_time_info(
    params: *mut AsioTime,
    index: c_long,
    _direct: AsioBool,
  ) -> *mut AsioTime {
    process(index as usize);
    params
  }
  extern "C" fn sample_rate_did_change(_rate: AsioSampleRate) {}
  extern "C" fn asio_message(
    selector: c_long,
    value: c_long,
    _message: *mut c_void,
    _opt: *mut f64,
  ) -> c_long {
    match (selector, value) {
      (K_ASIO_SELECTOR_SUPPORTED, K_ASIO_SUPPORTS_TIME_INFO | K_ASIO_ENGINE_VERSION) => 1,
      (K_ASIO_ENGINE_VERSION, _) => 2,
      (K_ASIO_SUPPORTS_TIME_INFO, _) => 1,
      _ => 0,
    }
  }
  static CALLBACKS: AsioCallbacks =
    AsioCallbacks { buffer_switch, sample_rate_did_change, asio_message, buffer_switch_time_info };

  fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
  }

  unsafe fn error_message(vtbl: &IAsioVtbl, driver: *mut c_void) -> String {
    let mut buf = [0 as c_char; 124];
    (vtbl.get_error_message)(driver, buf.as_mut_ptr());
    CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
  }

  fn read_samples(path: &PathBuf) -> Vec<i32> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("can't read {}: {e}", path.display()));
    bytes.chunks_exact(4).map(|c| i32::from_ne_bytes(c.try_into().unwrap())).collect()
  }

  pub fn run() -> i32 {
    let mut args = std::env::args().skip(1);
    let Some(dll) = args.next() else {
      eprintln!(
        "usage: asio_host <driver.dll> [--play FILE] [--record FILE] [--buffer FRAMES] [--seconds N] \
         [--until-file F]"
      );
      return 2;
    };
    let (mut play, mut record, mut buffer, mut seconds, mut until) = (None, None, 256i32, 30u64, None);
    while let Some(a) = args.next() {
      let v = args.next().unwrap_or_default();
      match a.as_str() {
        "--play" => play = Some(PathBuf::from(v)),
        "--record" => record = Some(PathBuf::from(v)),
        "--buffer" => buffer = v.parse().unwrap(),
        "--seconds" => seconds = v.parse().unwrap(),
        "--until-file" => until = Some(PathBuf::from(v)),
        _ => {
          eprintln!("unknown argument {a}");
          return 2;
        }
      }
    }

    unsafe {
      let module = LoadLibraryW(wide(&dll).as_ptr());
      if module.is_null() {
        eprintln!("can't load {dll}");
        return 1;
      }
      let Some(get_class_object) = GetProcAddress(module, b"DllGetClassObject\0".as_ptr()) else {
        eprintln!("{dll} has no DllGetClassObject");
        return 1;
      };
      let get_class_object: unsafe extern "system" fn(
        *const GUID,
        *const GUID,
        *mut *mut c_void,
      ) -> HResult = std::mem::transmute(get_class_object);
      let mut factory = std::ptr::null_mut();
      let r = get_class_object(&CLSID_INFERNO_ASIO, &IID_ICLASSFACTORY, &mut factory);
      if r != 0 {
        eprintln!("DllGetClassObject failed: {r:#x}");
        return 1;
      }
      let factory_vtbl = &**(factory as *const *const ClassFactoryVtbl);
      let mut driver = std::ptr::null_mut();
      // hosts ask for the driver's CLSID as the interface
      let r =
        (factory_vtbl.create_instance)(factory, std::ptr::null_mut(), &CLSID_INFERNO_ASIO, &mut driver);
      (factory_vtbl.release)(factory);
      if r != 0 {
        eprintln!("CreateInstance failed: {r:#x}");
        return 1;
      }
      let vtbl = &**(driver as *const *const IAsioVtbl);

      if (vtbl.init)(driver, std::ptr::null_mut()) != ASIO_TRUE {
        eprintln!("init failed: {}", error_message(vtbl, driver));
        return 1;
      }
      let mut name = [0 as c_char; 32];
      (vtbl.get_driver_name)(driver, name.as_mut_ptr());
      let (mut inputs, mut outputs) = (0, 0);
      (vtbl.get_channels)(driver, &mut inputs, &mut outputs);
      let (mut min, mut max, mut preferred, mut granularity) = (0, 0, 0, 0);
      (vtbl.get_buffer_size)(driver, &mut min, &mut max, &mut preferred, &mut granularity);
      let mut rate = 0.0;
      (vtbl.get_sample_rate)(driver, &mut rate);
      println!(
        "driver {:?} version {}: {inputs} inputs, {outputs} outputs, {rate} Hz, buffer {min}..{max} \
         (preferred {preferred}, granularity {granularity})",
        CStr::from_ptr(name.as_ptr()),
        (vtbl.get_driver_version)(driver)
      );
      if (vtbl.can_sample_rate)(driver, rate) != ASE_OK
        || (vtbl.can_sample_rate)(driver, rate * 2.0) == ASE_OK
      {
        eprintln!("canSampleRate doesn't report the network's rate only");
        return 1;
      }

      let mut infos: Vec<AsioBufferInfo> = (0..inputs)
        .map(|c| AsioBufferInfo {
          is_input: ASIO_TRUE,
          channel_num: c,
          buffers: [std::ptr::null_mut(); 2],
        })
        .chain((0..outputs).map(|c| AsioBufferInfo {
          is_input: ASIO_FALSE,
          channel_num: c,
          buffers: [std::ptr::null_mut(); 2],
        }))
        .collect();
      let started = Instant::now();
      let r =
        (vtbl.create_buffers)(driver, infos.as_mut_ptr(), infos.len() as c_long, buffer, &CALLBACKS);
      if r != ASE_OK {
        eprintln!("createBuffers failed ({r}): {}", error_message(vtbl, driver));
        return 1;
      }
      println!("createBuffers: {:.1} s (device start, clock)", started.elapsed().as_secs_f64());
      let (mut input_latency, mut output_latency) = (0, 0);
      (vtbl.get_latencies)(driver, &mut input_latency, &mut output_latency);
      println!("latencies: input {input_latency}, output {output_latency} frames");
      for i in 0..inputs.max(outputs) {
        for input in [ASIO_TRUE, ASIO_FALSE] {
          let mut info = AsioChannelInfo {
            channel: i,
            is_input: input,
            is_active: 0,
            channel_group: 0,
            sample_type: 0,
            name: [0; 32],
          };
          if (vtbl.get_channel_info)(driver, &mut info) == ASE_OK && info.sample_type != ASIO_ST_INT32_LSB
          {
            eprintln!("unexpected sample type {}", { info.sample_type });
            return 1;
          }
        }
      }

      *STATE.lock().unwrap() = Some(State {
        buffers: infos
          .iter()
          .map(|b| Buffer {
            input: b.is_input != 0,
            channel: b.channel_num as usize,
            halves: [b.buffers[0] as *mut i32, b.buffers[1] as *mut i32],
          })
          .collect(),
        period: buffer as usize,
        inputs: inputs as usize,
        outputs: outputs as usize,
        play: play.as_ref().map(read_samples).unwrap_or_default(),
        play_frame: 0,
        record: vec![],
        switches: 0,
        last_position: None,
        position_errors: 0,
        driver,
        vtbl,
      });

      let r = (vtbl.start)(driver);
      if r != ASE_OK {
        eprintln!("start failed ({r}): {}", error_message(vtbl, driver));
        return 1;
      }
      let started = Instant::now();
      while started.elapsed() < Duration::from_secs(seconds)
        && !until.as_ref().is_some_and(|f| f.exists())
      {
        std::thread::sleep(Duration::from_millis(100));
      }
      let elapsed = started.elapsed().as_secs_f64();
      (vtbl.stop)(driver);
      (vtbl.dispose_buffers)(driver);
      let state = STATE.lock().unwrap().take().unwrap();
      (vtbl.release)(driver);

      let expected = elapsed * rate / buffer as f64;
      println!(
        "{} buffer switches in {elapsed:.1} s (expected {expected:.0}), {} sample position errors",
        state.switches, state.position_errors
      );
      if let Some(path) = record {
        let bytes: Vec<u8> = state.record.iter().flat_map(|v| v.to_ne_bytes()).collect();
        std::fs::write(&path, bytes).unwrap();
      }
      if (state.switches as f64) < expected * 0.95 || state.position_errors > 0 {
        eprintln!("buffer switches missing or out of order");
        return 1;
      }
      0
    }
  }
}
