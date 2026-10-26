//! Integration tests of the stream lifecycle drivers build on, on any platform: devices on 127.0.0.1
//! following a free-running clock given by the "driver".

use std::{
  collections::BTreeMap,
  net::{Ipv4Addr, UdpSocket},
  sync::Arc,
};

use inferno_aoip::{
  clock::{ClockSource, FreeRunningClockSource},
  device_server::{AtomicSample, ExternalBufferParameters, Sample, Settings},
};
use inferno_driver_core::{
  BuffersSignature, DeviceError, Direction, Prepared, SharedDevice, Stream, StreamOptions,
};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: usize = 2;
const BUFFER_FRAMES: usize = 4096;

fn init_log() {
  let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
}

fn settings(name: &str, process_id: u16, alt_port: u16) -> Settings {
  let mut config = BTreeMap::new();
  config.insert("NAME".to_string(), name.to_string());
  config.insert("PROCESS_ID".to_string(), process_id.to_string());
  config.insert("ALT_PORT".to_string(), alt_port.to_string());
  config.insert("BIND_IP".to_string(), "127.0.0.1".to_string());
  config.insert("SAMPLE_RATE".to_string(), SAMPLE_RATE.to_string());
  config.insert("RX_CHANNELS".to_string(), CHANNELS.to_string());
  config.insert("TX_CHANNELS".to_string(), CHANNELS.to_string());
  Settings::try_new(name, name, Some(Ipv4Addr::LOCALHOST), &config).unwrap()
}

/// An application's interleaved buffer, which lives as long as the test.
struct AppBuffer(&'static [AtomicSample]);

impl AppBuffer {
  fn new() -> Self {
    Self(Box::leak((0..BUFFER_FRAMES * CHANNELS).map(|_| AtomicSample::new(0)).collect()))
  }
  fn signature(&self) -> BuffersSignature {
    BuffersSignature {
      areas: (0..CHANNELS)
        .map(|ch| (self.0.as_ptr() as usize + ch * size_of::<Sample>(), BUFFER_FRAMES, CHANNELS))
        .collect(),
      period_size: 256,
    }
  }
  fn channels(&self, stream: &Stream) -> Vec<ExternalBufferParameters<Sample>> {
    (0..CHANNELS)
      .map(|ch| unsafe {
        ExternalBufferParameters::new(
          self.0.as_ptr().add(ch),
          self.0.len() - ch,
          CHANNELS,
          stream.buffers_valid(),
          None,
        )
      })
      .collect()
  }
  fn prepare(&self, stream: &mut Stream) -> Result<Prepared, DeviceError> {
    let channels = self.channels(stream);
    stream.prepare(self.signature(), channels, StreamOptions { track_position: true, on_transfer: None })
  }
}

#[test]
fn restarting_a_stream_keeps_its_receiver_and_the_device_lives_while_streams_use_it() {
  init_log();
  let clock: Arc<dyn ClockSource> = Arc::new(FreeRunningClockSource::new());
  let settings = settings("core-lifecycle", 61, 46100);
  let id = settings.self_info.factory_device_id;
  let mut capture = Stream::new(Direction::Capture, settings.clone()).with_clock_source(clock.clone());
  let mut playback = Stream::new(Direction::Playback, settings).with_clock_source(clock);
  let capture_buffer = AppBuffer::new();
  let playback_buffer = AppBuffer::new();

  assert!(!SharedDevice::is_open(&id));
  assert_eq!(capture_buffer.prepare(&mut capture), Ok(Prepared::Started));
  assert!(SharedDevice::is_open(&id));
  assert!(capture.start(SAMPLE_RATE as u64).is_some());
  assert!(capture.start_time().is_some());
  assert!(capture.clock_position(SAMPLE_RATE as u64).is_some_and(|p| p >= 0));

  // transport stop/start, xrun recovery: same buffers
  capture.stop();
  assert_eq!(capture_buffer.prepare(&mut capture), Ok(Prepared::Kept));
  assert_eq!(capture.start_time(), None, "prepare un-anchors the stream");
  assert!(capture.start(SAMPLE_RATE as u64).is_some());

  // a second stream shares the device
  assert_eq!(playback_buffer.prepare(&mut playback), Ok(Prepared::Started));
  assert!(playback.start(SAMPLE_RATE as u64).is_some());
  playback.close();
  assert!(SharedDevice::is_open(&id), "the capture stream still uses the device");

  // new buffers (e.g. hw_params changed): the receiver is switched to them
  let new_capture_buffer = AppBuffer::new();
  capture.stop();
  assert_eq!(new_capture_buffer.prepare(&mut capture), Ok(Prepared::Started));
  capture.close();
  assert!(!SharedDevice::is_open(&id), "the last stream closed the device");
}

#[test]
fn device_on_ports_in_use_fails_and_is_started_again_once_free() {
  init_log();
  let clock: Arc<dyn ClockSource> = Arc::new(FreeRunningClockSource::new());
  let settings = settings("core-portclash", 62, 46200);
  let id = settings.self_info.factory_device_id;
  let arc_port = settings.self_info.arc_port;
  let mut stream = Stream::new(Direction::Capture, settings).with_clock_source(clock);
  let buffer = AppBuffer::new();

  let blocker = UdpSocket::bind((Ipv4Addr::LOCALHOST, arc_port)).unwrap();
  let result = buffer.prepare(&mut stream);
  assert!(matches!(result, Err(DeviceError::StartFailed(_))), "{result:?}");
  assert!(!SharedDevice::is_open(&id), "the failed device is forgotten");

  drop(blocker);
  assert_eq!(buffer.prepare(&mut stream), Ok(Prepared::Started));
  stream.close();
  assert!(!SharedDevice::is_open(&id));
}
