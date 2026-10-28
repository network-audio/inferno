//! A minimal Core Audio application for the end-to-end test: plays a file to a sound card and records one, so
//! that the samples pass bit-exact (24 bits in 32-bit integers, interleaved, at the card's rate).
//!
//! `coreaudio_host --list`: lists the audio devices.
//!
//! `coreaudio_host --card NAME --channels 2 --play FILE --record FILE --seconds S [--until-file F] [--buffer 512]`:
//! waits for the device named NAME, plays FILE from its start and records until S seconds have passed or F
//! exists.

#[cfg(not(target_os = "macos"))]
fn main() {
  eprintln!("coreaudio_host needs macOS");
  std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() {
  if let Err(e) = host::run() {
    eprintln!("error: {e}");
    std::process::exit(1);
  }
}

#[cfg(target_os = "macos")]
#[allow(non_upper_case_globals, non_snake_case)]
mod host {
  use std::{
    ffi::c_void,
    io::Write,
    path::PathBuf,
    sync::{
      atomic::{AtomicU64, AtomicUsize, Ordering},
      Mutex,
    },
    time::{Duration, Instant},
  };

  use inferno_coreaudio::abi::*;
  use inferno_soundcard_shm::{f32_to_sample, sample_to_f32};

  const kAudioObjectSystemObject: AudioObjectID = 1;
  const kAudioHardwarePropertyDevices: u32 = fourcc(b"dev#");
  const kAudioDevicePropertyStreamConfiguration: u32 = fourcc(b"slay");
  const kAudioDevicePropertyBufferFrameSize: u32 = fourcc(b"fsiz");

  #[repr(C)]
  struct AudioBuffer {
    mNumberChannels: u32,
    mDataByteSize: u32,
    mData: *mut c_void,
  }

  #[repr(C)]
  struct AudioBufferList {
    mNumberBuffers: u32,
    mBuffers: [AudioBuffer; 1],
  }

  type IOProc = unsafe extern "C" fn(
    AudioObjectID,
    *const AudioTimeStamp,
    *const AudioBufferList,
    *const AudioTimeStamp,
    *mut AudioBufferList,
    *const AudioTimeStamp,
    *mut c_void,
  ) -> OSStatus;

  #[link(name = "CoreAudio", kind = "framework")]
  extern "C" {
    fn AudioObjectGetPropertyDataSize(
      object: AudioObjectID,
      address: *const AudioObjectPropertyAddress,
      qualifier_size: u32,
      qualifier: *const c_void,
      out_size: *mut u32,
    ) -> OSStatus;
    fn AudioObjectGetPropertyData(
      object: AudioObjectID,
      address: *const AudioObjectPropertyAddress,
      qualifier_size: u32,
      qualifier: *const c_void,
      io_size: *mut u32,
      out: *mut c_void,
    ) -> OSStatus;
    fn AudioObjectSetPropertyData(
      object: AudioObjectID,
      address: *const AudioObjectPropertyAddress,
      qualifier_size: u32,
      qualifier: *const c_void,
      size: u32,
      data: *const c_void,
    ) -> OSStatus;
    fn AudioDeviceCreateIOProcID(
      device: AudioObjectID,
      proc_: IOProc,
      client: *mut c_void,
      out: *mut *mut c_void,
    ) -> OSStatus;
    fn AudioDeviceDestroyIOProcID(device: AudioObjectID, id: *mut c_void) -> OSStatus;
    fn AudioDeviceStart(device: AudioObjectID, id: *mut c_void) -> OSStatus;
    fn AudioDeviceStop(device: AudioObjectID, id: *mut c_void) -> OSStatus;
  }

  fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
  }

  fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
      mSelector: selector,
      mScope: scope,
      mElement: kAudioObjectPropertyElementMain,
    }
  }

  fn check(call: &str, status: OSStatus) -> Result<(), String> {
    if status == 0 {
      Ok(())
    } else {
      let code = (status as u32).to_be_bytes();
      let printable = code.iter().all(|b| b.is_ascii_graphic() || *b == b' ');
      Err(if printable {
        format!("{call}: '{}'", String::from_utf8_lossy(&code))
      } else {
        format!("{call}: {status}")
      })
    }
  }

  fn bytes(object: AudioObjectID, selector: u32, scope: u32) -> Result<Vec<u8>, String> {
    let address = address(selector, scope);
    let mut size = 0;
    // safety: valid pointers
    check("AudioObjectGetPropertyDataSize", unsafe {
      AudioObjectGetPropertyDataSize(object, &address, 0, std::ptr::null(), &mut size)
    })?;
    let mut data = vec![0u8; size as usize];
    check("AudioObjectGetPropertyData", unsafe {
      AudioObjectGetPropertyData(
        object,
        &address,
        0,
        std::ptr::null(),
        &mut size,
        data.as_mut_ptr() as *mut c_void,
      )
    })?;
    data.truncate(size as usize);
    Ok(data)
  }

  fn string(object: AudioObjectID, selector: u32) -> Result<String, String> {
    let data = bytes(object, selector, kAudioObjectPropertyScopeGlobal)?;
    let s = unsafe { *(data.as_ptr() as *const CFStringRef) };
    let size =
      unsafe { CFStringGetMaximumSizeForEncoding(CFStringGetLength(s), kCFStringEncodingUTF8) } + 1;
    let mut buffer = vec![0u8; size as usize];
    unsafe { CFStringGetCString(s, buffer.as_mut_ptr(), size, kCFStringEncodingUTF8) };
    unsafe { CFRelease(s) };
    let end = buffer.iter().position(|&b| b == 0).unwrap_or(0);
    Ok(String::from_utf8_lossy(&buffer[..end]).into_owned())
  }

  fn devices() -> Result<Vec<AudioObjectID>, String> {
    let data =
      bytes(kAudioObjectSystemObject, kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal)?;
    Ok(data.chunks_exact(4).map(|c| u32::from_ne_bytes(c.try_into().unwrap())).collect())
  }

  /// Channels of the device in `scope` (input or output).
  fn channels_of(device: AudioObjectID, scope: u32) -> Result<u32, String> {
    let data = bytes(device, kAudioDevicePropertyStreamConfiguration, scope)?;
    let count = u32::from_ne_bytes(data[..4].try_into().unwrap()) as usize;
    // AudioBufferList: the count, then (on 64-bit) 4 bytes of padding and 16-byte buffers
    Ok((0..count).map(|i| u32::from_ne_bytes(data[8 + 16 * i..12 + 16 * i].try_into().unwrap())).sum())
  }

  fn list() -> Result<(), String> {
    for device in devices()? {
      let input = channels_of(device, kAudioObjectPropertyScopeInput).unwrap_or(0);
      let output = channels_of(device, kAudioObjectPropertyScopeOutput).unwrap_or(0);
      let rate = bytes(device, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal)
        .map(|d| f64::from_ne_bytes(d[..8].try_into().unwrap()))
        .unwrap_or(0.0);
      println!(
        "{device}: {:?} (uid {:?}), {input} in, {output} out, {rate} Hz",
        string(device, kAudioObjectPropertyName)?,
        string(device, kAudioDevicePropertyDeviceUID)?,
      );
    }
    Ok(())
  }

  /// What the I/O proc plays and records.
  struct State {
    channels: usize,
    source: Vec<i32>,
    played: AtomicUsize,
    recording: Mutex<Vec<i32>>,
    cycles: AtomicU64,
    first: Mutex<Option<String>>,
  }

  unsafe extern "C" fn io_proc(
    _device: AudioObjectID,
    _now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    input_time: *const AudioTimeStamp,
    output: *mut AudioBufferList,
    output_time: *const AudioTimeStamp,
    client: *mut c_void,
  ) -> OSStatus {
    let state = &*(client as *const State);
    let cycle = state.cycles.fetch_add(1, Ordering::Relaxed);
    if cycle == 0 {
      if let Ok(mut first) = state.first.try_lock() {
        *first = Some(format!(
          "first cycle: input at sample {}, output at sample {}",
          (*input_time).mSampleTime,
          (*output_time).mSampleTime
        ));
      }
    }
    if !input.is_null() && (*input).mNumberBuffers > 0 {
      let buffer = &(*input).mBuffers[0];
      let samples =
        std::slice::from_raw_parts(buffer.mData as *const f32, buffer.mDataByteSize as usize / 4);
      if let Ok(mut recording) = state.recording.try_lock() {
        // within the capacity reserved: no allocation here
        let room = recording.capacity() - recording.len();
        recording.extend(samples.iter().take(room).map(|&s| f32_to_sample(s)));
      }
    }
    if !output.is_null() && (*output).mNumberBuffers > 0 {
      let buffer = &mut (*output).mBuffers[0];
      let samples =
        std::slice::from_raw_parts_mut(buffer.mData as *mut f32, buffer.mDataByteSize as usize / 4);
      let from = state.played.load(Ordering::Relaxed) * state.channels;
      for (i, s) in samples.iter_mut().enumerate() {
        *s = state.source.get(from + i).map(|&s| sample_to_f32(s)).unwrap_or(0.0);
      }
      state.played.fetch_add(samples.len() / state.channels, Ordering::Relaxed);
    }
    0
  }

  pub fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
      return list();
    }
    let card = arg(&args, "--card").ok_or("--card")?;
    let channels: usize =
      arg(&args, "--channels").unwrap_or("2".into()).parse().map_err(|_| "--channels")?;
    let source =
      std::fs::read(arg(&args, "--play").ok_or("--play")?).map_err(|e| format!("--play: {e}"))?;
    let mut recording_file = std::fs::File::create(arg(&args, "--record").ok_or("--record")?)
      .map_err(|e| format!("--record: {e}"))?;
    let seconds: u64 = arg(&args, "--seconds").unwrap_or("60".into()).parse().map_err(|_| "--seconds")?;
    let until_file = arg(&args, "--until-file").map(PathBuf::from);
    let buffer_frames: u32 =
      arg(&args, "--buffer").unwrap_or("512".into()).parse().map_err(|_| "--buffer")?;

    let waiting = Instant::now();
    let device = loop {
      let found =
        devices()?.into_iter().find(|&d| string(d, kAudioObjectPropertyName).is_ok_and(|n| n == card));
      if let Some(device) = found {
        break device;
      }
      if waiting.elapsed() > Duration::from_secs(120) {
        list()?;
        return Err(format!("the device {card:?} didn't appear"));
      }
      std::thread::sleep(Duration::from_millis(500));
    };
    let (inputs, outputs) = (
      channels_of(device, kAudioObjectPropertyScopeInput)?,
      channels_of(device, kAudioObjectPropertyScopeOutput)?,
    );
    println!(
      "found {card:?} (device {device}, {inputs} in, {outputs} out) after {:.1} s",
      waiting.elapsed().as_secs_f64()
    );
    if inputs as usize != channels || outputs as usize != channels {
      return Err(format!("expected {channels} channels each way"));
    }
    let rate = bytes(device, kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal)
      .map(|d| f64::from_ne_bytes(d[..8].try_into().unwrap()))?;
    let size_address = address(kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal);
    // safety: valid pointers
    check("set the buffer size", unsafe {
      AudioObjectSetPropertyData(
        device,
        &size_address,
        0,
        std::ptr::null(),
        4,
        &buffer_frames as *const u32 as *const c_void,
      )
    })?;

    let capacity = (seconds as f64 * rate) as usize * channels + (1 << 16);
    let state = Box::new(State {
      channels,
      source: source.chunks_exact(4).map(|c| i32::from_ne_bytes(c.try_into().unwrap())).collect(),
      played: AtomicUsize::new(0),
      recording: Mutex::new(Vec::with_capacity(capacity)),
      cycles: AtomicU64::new(0),
      first: Mutex::new(None),
    });
    let mut proc_id = std::ptr::null_mut();
    let client = &*state as *const State as *mut c_void;
    // safety: the state outlives the I/O proc (destroyed below)
    check("AudioDeviceCreateIOProcID", unsafe {
      AudioDeviceCreateIOProcID(device, io_proc, client, &mut proc_id)
    })?;
    check("AudioDeviceStart", unsafe { AudioDeviceStart(device, proc_id) })?;
    let started = Instant::now();
    let mut reported = false;
    while started.elapsed() < Duration::from_secs(seconds)
      && !until_file.as_ref().is_some_and(|f| f.exists())
    {
      std::thread::sleep(Duration::from_millis(20));
      if !reported {
        if let Some(first) = state.first.lock().unwrap().take() {
          println!("{first}");
          reported = true;
        }
      }
    }
    check("AudioDeviceStop", unsafe { AudioDeviceStop(device, proc_id) })?;
    check("AudioDeviceDestroyIOProcID", unsafe { AudioDeviceDestroyIOProcID(device, proc_id) })?;
    let elapsed = started.elapsed().as_secs_f64();
    let recording = state.recording.lock().unwrap();
    let recorded = recording.len() / channels;
    let played = state.played.load(Ordering::Relaxed);
    let silent = recording.iter().filter(|&&s| s == 0).count();
    println!(
      "played {played} frames, recorded {recorded} frames ({silent} silent samples) in {elapsed:.1} s, \
       {} cycles ({:.0} and {:.0} frames/s)",
      state.cycles.load(Ordering::Relaxed),
      played as f64 / elapsed,
      recorded as f64 / elapsed
    );
    let bytes: Vec<u8> = recording.iter().flat_map(|s| s.to_ne_bytes()).collect();
    recording_file.write_all(&bytes).map_err(|e| e.to_string())?;
    Ok(())
  }
}
