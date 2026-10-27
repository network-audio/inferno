//! A minimal WASAPI application for the end-to-end test: plays a file to a sound card and records one, in
//! exclusive mode, so that the samples pass bit-exact (24 bits in 32-bit integers, interleaved, 48 kHz).
//!
//! `wasapi_host --card NAME --channels 2 --play FILE --record FILE --seconds S [--until-file F]`: waits for
//! the card's endpoints (`Speakers (NAME)`, `Microphone (NAME)`), plays FILE from its start and records until
//! S seconds have passed or F exists.

#[cfg(not(windows))]
fn main() {
  eprintln!("wasapi_host needs Windows");
  std::process::exit(1);
}

#[cfg(windows)]
fn main() {
  if let Err(e) = host::run() {
    eprintln!("error: {e}");
    std::process::exit(1);
  }
}

#[cfg(windows)]
mod host {
  use std::{
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
  };

  use windows::{
    core::GUID,
    Win32::{
      Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
      Media::Audio::{
        eCapture, eRender, EDataFlow, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice,
        IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_BUFFER_TOO_LARGE,
        AUDCLNT_SHAREMODE_EXCLUSIVE, DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
        WAVEFORMATEXTENSIBLE_0,
      },
      System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ},
    },
  };

  const RATE: u32 = 48000;
  const PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);

  fn at(call: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |e| format!("{call}: {e}")
  }

  fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
  }

  fn endpoint(flow: EDataFlow, name: &str) -> Result<Option<IMMDevice>, String> {
    let enumerator: IMMDeviceEnumerator =
      unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(at("MMDeviceEnumerator"))?;
    let collection = unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) }
      .map_err(at("EnumAudioEndpoints"))?;
    for index in 0..unsafe { collection.GetCount() }.map_err(at("GetCount"))? {
      let device = unsafe { collection.Item(index) }.map_err(at("Item"))?;
      let store = unsafe { device.OpenPropertyStore(STGM_READ) }.map_err(at("OpenPropertyStore"))?;
      let friendly =
        unsafe { store.GetValue(&PKEY_Device_FriendlyName) }.map_err(at("GetValue"))?.to_string();
      if friendly == name {
        return Ok(Some(device));
      }
    }
    Ok(None)
  }

  fn open(device: &IMMDevice, channels: u16) -> Result<IAudioClient, String> {
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(at("Activate"))?;
    let format = WAVEFORMATEXTENSIBLE {
      Format: WAVEFORMATEX {
        wFormatTag: 0xfffe,
        nChannels: channels,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * 4 * channels as u32,
        nBlockAlign: 4 * channels,
        wBitsPerSample: 32,
        cbSize: 22,
      },
      Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 24 },
      dwChannelMask: if channels == 2 { 3 } else { 0 },
      SubFormat: PCM,
    };
    let mut period = 0i64;
    unsafe { client.GetDevicePeriod(Some(&mut period), None) }.map_err(at("GetDevicePeriod"))?;
    let format = &format as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX;
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, 0, 1_000_000, period, format, None) }
      .map_err(at("Initialize (exclusive)"))?;
    Ok(client)
  }

  pub fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let card = arg(&args, "--card").ok_or("--card")?;
    let channels: u16 =
      arg(&args, "--channels").unwrap_or("2".into()).parse().map_err(|_| "--channels")?;
    let source =
      std::fs::read(arg(&args, "--play").ok_or("--play")?).map_err(|e| format!("--play: {e}"))?;
    let mut recording = std::fs::File::create(arg(&args, "--record").ok_or("--record")?)
      .map_err(|e| format!("--record: {e}"))?;
    let seconds: u64 = arg(&args, "--seconds").unwrap_or("60".into()).parse().map_err(|_| "--seconds")?;
    let until_file = arg(&args, "--until-file").map(PathBuf::from);
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok().map_err(at("CoInitializeEx"))?;

    let (speakers, microphone) = (format!("Speakers ({card})"), format!("Microphone ({card})"));
    let waiting = Instant::now();
    let (render_device, capture_device) = loop {
      if let (Some(r), Some(c)) = (endpoint(eRender, &speakers)?, endpoint(eCapture, &microphone)?) {
        break (r, c);
      }
      if waiting.elapsed() > Duration::from_secs(120) {
        return Err(format!("endpoints {speakers:?} and {microphone:?} didn't appear"));
      }
      std::thread::sleep(Duration::from_millis(500));
    };
    println!("found {speakers} and {microphone} after {:.1} s", waiting.elapsed().as_secs_f64());
    let render = open(&render_device, channels)?;
    let capture = open(&capture_device, channels)?;
    let render_client: IAudioRenderClient =
      unsafe { render.GetService() }.map_err(at("GetService render"))?;
    let capture_client: IAudioCaptureClient =
      unsafe { capture.GetService() }.map_err(at("GetService capture"))?;
    let buffer = unsafe { render.GetBufferSize() }.map_err(at("GetBufferSize"))?;
    let mut period = 0i64;
    unsafe { render.GetDevicePeriod(Some(&mut period), None) }.map_err(at("GetDevicePeriod"))?;
    let period_frames = (period as u64 * RATE as u64 / 10_000_000).max(1) as u32;
    let frame_bytes = 4 * channels as usize;
    unsafe { capture.Start() }.map_err(at("Start capture"))?;
    unsafe { render.Start() }.map_err(at("Start render"))?;

    let started = Instant::now();
    let (mut played, mut recorded) = (0usize, 0usize);
    while started.elapsed() < Duration::from_secs(seconds)
      && !until_file.as_ref().is_some_and(|f| f.exists())
    {
      let free = (buffer - unsafe { render.GetCurrentPadding() }.map_err(at("GetCurrentPadding"))?)
        .min(period_frames);
      // in exclusive mode the space can still be short of the padding's: try again later
      let data = match unsafe { render_client.GetBuffer(free) } {
        _ if free == 0 => None,
        Ok(data) => Some(data),
        Err(e) if e.code() == AUDCLNT_E_BUFFER_TOO_LARGE => None,
        Err(e) => return Err(format!("GetBuffer render: {e}")),
      };
      if let Some(data) = data {
        let dst = unsafe { std::slice::from_raw_parts_mut(data, free as usize * frame_bytes) };
        let from = (played * frame_bytes).min(source.len());
        let available = (source.len() - from).min(dst.len());
        dst[..available].copy_from_slice(&source[from..from + available]);
        dst[available..].fill(0);
        unsafe { render_client.ReleaseBuffer(free, 0) }.map_err(at("ReleaseBuffer render"))?;
        played += free as usize;
      }
      loop {
        let mut data = std::ptr::null_mut();
        let (mut frames, mut flags) = (0u32, 0u32);
        unsafe { capture_client.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
          .map_err(at("GetBuffer capture"))?;
        if frames == 0 {
          break;
        }
        let bytes = frames as usize * frame_bytes;
        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
          recording.write_all(&vec![0; bytes]).map_err(|e| e.to_string())?;
        } else {
          recording
            .write_all(unsafe { std::slice::from_raw_parts(data, bytes) })
            .map_err(|e| e.to_string())?;
        }
        unsafe { capture_client.ReleaseBuffer(frames) }.map_err(at("ReleaseBuffer capture"))?;
        recorded += frames as usize;
      }
      std::thread::sleep(Duration::from_millis(2));
    }
    unsafe { render.Stop() }.map_err(at("Stop render"))?;
    unsafe { capture.Stop() }.map_err(at("Stop capture"))?;
    let elapsed = started.elapsed().as_secs_f64();
    println!(
      "played {played} frames, recorded {recorded} frames in {elapsed:.1} s ({:.0} and {:.0} frames/s)",
      played as f64 / elapsed,
      recorded as f64 / elapsed
    );
    Ok(())
  }
}
