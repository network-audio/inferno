//! Integration tests for DAW-style PCM usage of the Inferno ALSA plugin.
//!
//! DAWs and JACK restart PCMs all the time: `snd_pcm_drop` + `snd_pcm_prepare` + `snd_pcm_start`
//! on transport stop/start and on every xrun. These tests load the built plugin `.so` through an
//! in-memory ALSA configuration and run it against an in-process Inferno device (library API),
//! all over 127.0.0.1 with a fake usrvclock server providing the media clock.
//!
//! Linux-only (ALSA).

use alsa_sys_all::*;
use inferno_aoip::device_server::{
    AtomicSample, DeviceServer, ExternalBufferParameters, Sample, Settings, StreamAnchor,
};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: usize = 2;
const LATENCY_NS: u32 = 10_000_000;
const LATENCY_SAMPLES: i64 = (LATENCY_NS as i64) * (SAMPLE_RATE as i64) / 1_000_000_000;
/// Maximum acceptable silence caused by a PCM restart. A full receiver/transmitter rebuild
/// (re-resolving and re-subscribing flows) takes seconds; re-anchoring should take ~latency.
const MAX_RESTART_GAP: Duration = Duration::from_millis(200);
/// How often the library receiver used in playback tests delivers audio.
const LIBRARY_RX_READ_INTERVAL: Duration = Duration::from_millis(50);
const ROUTING_TIMEOUT: Duration = Duration::from_secs(30);
/// Constant non-zero pattern the test app writes to the plugin's playback stream; `+ epoch << 16`
/// is added after each restart so the receiver can tell post-restart audio from stale audio.
const TX_PATTERN: Sample = 0x0123_4500;
/// The library transmitter sends a ramp encoding the media timestamp and an epoch bit in each
/// sample: `sample = (timestamp mod 2^20) << 9 | epoch << 8`, which survives 24-bit transport.
/// The test flips the epoch while the PCM is stopped, so capture can tell post-restart audio
/// apart from stale buffer contents or uninitialised memory.
const RAMP_BITS: u32 = 20;
const RAMP_LEN: usize = 1 << RAMP_BITS;
/// Captured audio older than this (latency + read slack) is considered stale.
const MAX_FRESH_AGE_SAMPLES: usize = SAMPLE_RATE as usize / 10;
/// Tolerance for frames elapsing between consecutive avail/delay calls.
const DELAY_TOLERANCE: i64 = SAMPLE_RATE as i64 / 1000;

/// Each test uses its own ports, process ids, device ids and clock socket so they can run in parallel.
struct TestIds {
    base_port: u16,
    base_pid: u16,
    tag: &'static str,
    /// RX and TX network latency of all devices in the test
    latency_ns: u32,
}

impl TestIds {
    fn clock_path(&self) -> String {
        format!("/tmp/inferno-daw-test-{}-usrvclock", self.tag)
    }
}

/// Common XDG_STATE_HOME for every device in this test process (state is keyed by device id).
fn state_home() -> PathBuf {
    std::env::temp_dir().join(format!("inferno-daw-test-{}", std::process::id()))
}

fn plugin_path() -> PathBuf {
    // tests run from target/<profile>/deps/, the plugin is built into target/<profile>/
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let path = dir.join("libasound_module_pcm_inferno.so");
    assert!(path.exists(), "plugin not built at {path:?}");
    path
}

fn init_test_env() {
    set_state_home();
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();
}

struct ClockServer {
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    path: String,
}

impl ClockServer {
    fn start(path: String) -> Self {
        let _ = std::fs::remove_file(&path);
        let running = Arc::new(AtomicBool::new(true));
        let r = running.clone();
        let p = path.clone();
        let thread = std::thread::spawn(move || {
            let mut server = usrvclock::Server::new(p.into()).unwrap();
            let overlay = usrvclock::ClockOverlay {
                clock_id: libc::CLOCK_MONOTONIC as i64,
                last_sync: 0,
                shift: 0,
                freq_scale: 0.0,
            };
            while r.load(Ordering::Relaxed) {
                server.send(overlay);
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        std::thread::sleep(Duration::from_millis(200));
        Self {
            running,
            thread: Some(thread),
            path,
        }
    }
}

impl Drop for ClockServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn library_settings(name: &str, ids: &TestIds, rx: usize, tx: usize) -> Settings {
    let mut config = BTreeMap::new();
    config.insert("NAME".to_string(), name.to_string());
    config.insert("PROCESS_ID".to_string(), ids.base_pid.to_string());
    config.insert("ALT_PORT".to_string(), ids.base_port.to_string());
    config.insert("SAMPLE_RATE".to_string(), SAMPLE_RATE.to_string());
    config.insert("CLOCK_PATH".to_string(), ids.clock_path());
    config.insert("BIND_IP".to_string(), "127.0.0.1".to_string());
    config.insert("RX_LATENCY_NS".to_string(), ids.latency_ns.to_string());
    config.insert("TX_LATENCY_NS".to_string(), ids.latency_ns.to_string());
    // no dither: the capture tests rely on bit-exact 24-bit transport of the ramp
    config.insert("TX_SOURCE_BIT_DEPTH".to_string(), "24".to_string());
    let mut settings = Settings::new(name, name, Some(Ipv4Addr::LOCALHOST), &config);
    settings.make_rx_channels(rx);
    settings.make_tx_channels(tx);
    settings
}

fn plugin_device_id(ids: &TestIds) -> String {
    format!("00000000{:08x}", ids.base_pid as u32 + 100)
}

/// ALSA config with the plugin registered from the build directory and one `inferno` PCM.
fn plugin_config(ids: &TestIds, name: &str) -> String {
    format!(
        r#"
pcm_type.inferno {{ lib "{lib}" }}
pcm.dut {{
  type inferno
  NAME "{name}"
  DEVICE_ID "{devid}"
  PROCESS_ID "{pid}"
  ALT_PORT "{port}"
  BIND_IP "127.0.0.1"
  SAMPLE_RATE "{rate}"
  CLOCK_PATH "{clock}"
  RX_CHANNELS "{ch}"
  TX_CHANNELS "{ch}"
  RX_LATENCY_NS "{lat}"
  TX_LATENCY_NS "{lat}"
  TX_SOURCE_BIT_DEPTH "24"
}}
"#,
        lib = plugin_path().display(),
        devid = plugin_device_id(ids),
        pid = ids.base_pid + 100,
        port = ids.base_port + 100,
        rate = SAMPLE_RATE,
        clock = ids.clock_path(),
        ch = CHANNELS,
        lat = ids.latency_ns,
    )
}

/// Minimal safe-ish wrapper over an ALSA PCM handle, non-blocking.
struct Pcm {
    pcm: *mut snd_pcm_t,
    config: *mut snd_config_t,
    period_size: usize,
    buffer_size: usize,
}

impl Pcm {
    fn open(config_text: &str, stream: snd_pcm_stream_t, buffer_us: u32) -> Self {
        unsafe {
            let mut config = std::ptr::null_mut();
            assert_eq!(snd_config_top(&mut config), 0);
            let mut input = std::ptr::null_mut();
            let text = CString::new(config_text).unwrap();
            assert_eq!(
                snd_input_buffer_open(&mut input, text.as_ptr(), config_text.len() as isize),
                0
            );
            assert_eq!(
                snd_config_load(config, input),
                0,
                "failed to parse ALSA config"
            );
            snd_input_close(input);

            let mut pcm = std::ptr::null_mut();
            let name = CString::new("dut").unwrap();
            let r = snd_pcm_open_lconf(
                &mut pcm,
                name.as_ptr(),
                stream,
                SND_PCM_NONBLOCK as i32,
                config,
            );
            assert_eq!(r, 0, "snd_pcm_open_lconf failed: {r}");
            let r = snd_pcm_set_params(
                pcm,
                SND_PCM_FORMAT_S32,
                SND_PCM_ACCESS_RW_INTERLEAVED,
                CHANNELS as u32,
                SAMPLE_RATE,
                0,
                buffer_us,
            );
            assert_eq!(r, 0, "snd_pcm_set_params failed: {r}");
            let mut buffer_size = 0;
            let mut period_size = 0;
            assert_eq!(
                snd_pcm_get_params(pcm, &mut buffer_size, &mut period_size),
                0
            );
            log::info!("PCM opened: buffer {buffer_size}, period {period_size}");
            Self {
                pcm,
                config,
                period_size: period_size as usize,
                buffer_size: buffer_size as usize,
            }
        }
    }

    /// Opens with an explicit period and buffer size (in frames), like DAWs do.
    /// Returns the error code if the configuration is refused.
    fn open_with(
        config_text: &str,
        stream: snd_pcm_stream_t,
        period_frames: usize,
        buffer_frames: usize,
    ) -> Result<Self, i32> {
        unsafe {
            let mut config = std::ptr::null_mut();
            assert_eq!(snd_config_top(&mut config), 0);
            let mut input = std::ptr::null_mut();
            let text = CString::new(config_text).unwrap();
            assert_eq!(
                snd_input_buffer_open(&mut input, text.as_ptr(), config_text.len() as isize),
                0
            );
            assert_eq!(
                snd_config_load(config, input),
                0,
                "failed to parse ALSA config"
            );
            snd_input_close(input);
            let mut pcm = std::ptr::null_mut();
            let name = CString::new("dut").unwrap();
            let r = snd_pcm_open_lconf(
                &mut pcm,
                name.as_ptr(),
                stream,
                SND_PCM_NONBLOCK as i32,
                config,
            );
            assert_eq!(r, 0, "snd_pcm_open_lconf failed: {r}");
            let mut pcm_struct = Self {
                pcm,
                config,
                period_size: 0,
                buffer_size: 0,
            };
            pcm_struct.set_hw_params(period_frames, buffer_frames)?;
            Ok(pcm_struct)
        }
    }

    /// (Re)configures period and buffer size (in frames); the PCM must not be running.
    fn set_hw_params(&mut self, period_frames: usize, buffer_frames: usize) -> Result<(), i32> {
        let pcm = self.pcm;
        unsafe {
            let mut hw = std::ptr::null_mut();
            assert_eq!(snd_pcm_hw_params_malloc(&mut hw), 0);
            let mut period = period_frames as snd_pcm_uframes_t;
            let mut buffer = buffer_frames as snd_pcm_uframes_t;
            let mut dir = 0;
            let mut r = snd_pcm_hw_params_any(pcm, hw);
            if r >= 0 {
                r = snd_pcm_hw_params_set_access(pcm, hw, SND_PCM_ACCESS_RW_INTERLEAVED);
            }
            if r >= 0 {
                r = snd_pcm_hw_params_set_format(pcm, hw, SND_PCM_FORMAT_S32);
            }
            if r >= 0 {
                r = snd_pcm_hw_params_set_channels(pcm, hw, CHANNELS as u32);
            }
            if r >= 0 {
                r = snd_pcm_hw_params_set_rate(pcm, hw, SAMPLE_RATE, 0);
            }
            if r >= 0 {
                r = snd_pcm_hw_params_set_period_size_near(pcm, hw, &mut period, &mut dir);
            }
            if r >= 0 {
                r = snd_pcm_hw_params_set_buffer_size_near(pcm, hw, &mut buffer);
            }
            if r >= 0 {
                r = snd_pcm_hw_params(pcm, hw);
            }
            snd_pcm_hw_params_free(hw);
            if r < 0 {
                return Err(r);
            }
            assert_eq!(
                (period as usize, buffer as usize),
                (period_frames, buffer_frames),
                "PCM didn't take the requested sizes"
            );
            self.period_size = period as usize;
            self.buffer_size = buffer as usize;
            Ok(())
        }
    }

    fn prepare(&self) {
        let r = unsafe { snd_pcm_prepare(self.pcm) };
        assert_eq!(r, 0, "snd_pcm_prepare failed: {r}");
    }
    fn start(&self) {
        let r = unsafe { snd_pcm_start(self.pcm) };
        assert_eq!(r, 0, "snd_pcm_start failed: {r}");
    }
    fn drop_stream(&self) {
        let r = unsafe { snd_pcm_drop(self.pcm) };
        assert_eq!(r, 0, "snd_pcm_drop failed: {r}");
    }
    fn delay(&self) -> i64 {
        let mut d: snd_pcm_sframes_t = 0;
        let r = unsafe { snd_pcm_delay(self.pcm, &mut d) };
        assert_eq!(r, 0, "snd_pcm_delay failed: {r}");
        d as i64
    }
    /// Start unless already started implicitly by the start threshold.
    fn start_if_prepared(&self) {
        if unsafe { snd_pcm_state(self.pcm) } == SND_PCM_STATE_PREPARED {
            self.start();
        }
    }
    fn avail(&self) -> i64 {
        unsafe { snd_pcm_avail(self.pcm) as i64 }
    }

    /// Returns frames read, 0 if nothing available, or negative errno (e.g. -EPIPE on xrun).
    fn read(&self, buf: &mut [Sample]) -> i64 {
        unsafe {
            snd_pcm_readi(
                self.pcm,
                buf.as_mut_ptr() as *mut _,
                (buf.len() / CHANNELS) as snd_pcm_uframes_t,
            ) as i64
        }
        .max_with_eagain()
    }
    fn write(&self, buf: &[Sample]) -> i64 {
        unsafe {
            snd_pcm_writei(
                self.pcm,
                buf.as_ptr() as *const _,
                (buf.len() / CHANNELS) as snd_pcm_uframes_t,
            ) as i64
        }
        .max_with_eagain()
    }
}

trait EagainIsZero {
    fn max_with_eagain(self) -> i64;
}
impl EagainIsZero for i64 {
    fn max_with_eagain(self) -> i64 {
        if self == -(libc::EAGAIN as i64) {
            0
        } else {
            self
        }
    }
}

impl Drop for Pcm {
    fn drop(&mut self) {
        unsafe {
            snd_pcm_close(self.pcm);
            snd_config_delete(self.config);
        }
    }
}

/// Current media time in samples; the fake clock server publishes CLOCK_MONOTONIC unmodified.
fn media_now_samples() -> usize {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let ns = ts.tv_sec as u128 * 1_000_000_000 + ts.tv_nsec as u128;
    (ns * SAMPLE_RATE as u128 / 1_000_000_000) as usize
}

fn ramp_sample(ts: usize, epoch: usize) -> Sample {
    (((ts & (RAMP_LEN - 1)) << 9) | ((epoch & 1) << 8)) as Sample
}

/// Is this a recent sample of the library transmitter's ramp, sent in the given epoch?
fn is_fresh_ramp_sample(sample: Sample, epoch: usize) -> bool {
    if sample == 0 || (sample & 0xff) != 0 || ((sample as usize >> 8) & 1) != (epoch & 1) {
        return false;
    }
    let ts = (sample as u32 >> 9) as usize;
    let age = media_now_samples().wrapping_sub(ts) & (RAMP_LEN - 1);
    age < MAX_FRESH_AGE_SAMPLES
}

/// Reads until a fresh frame of the transmitted ramp from the given epoch arrives.
/// Returns time waited, or None on timeout.
/// Xruns are recovered with prepare+start (as JACK and DAWs do).
fn capture_until_audio(pcm: &Pcm, epoch: usize, timeout: Duration) -> Option<Duration> {
    let started = Instant::now();
    let mut buf = vec![0 as Sample; pcm.period_size * CHANNELS];
    while started.elapsed() < timeout {
        let r = pcm.read(&mut buf);
        if r == -(libc::EPIPE as i64) {
            log::warn!("capture xrun while waiting for audio, recovering");
            pcm.prepare();
            pcm.start();
            continue;
        }
        assert!(r >= 0, "snd_pcm_readi failed: {r}");
        if buf[..r as usize * CHANNELS]
            .iter()
            .any(|&s| is_fresh_ramp_sample(s, epoch))
        {
            return Some(started.elapsed());
        }
        if r == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    None
}

/// Library-side TX device transmitting the timestamp ramp on all channels.
fn start_library_transmitter(
    rt: &tokio::runtime::Runtime,
    ids: &TestIds,
) -> (DeviceServer, String, Vec<&'static [AtomicSample]>) {
    rt.block_on(async {
        let mut server = DeviceServer::start(library_settings(
            &format!("lib-tx-{}", ids.tag),
            ids,
            0,
            CHANNELS,
        ))
        .await;
        let hostname = server.self_info.friendly_hostname.clone();
        let ramps: Vec<&'static [AtomicSample]> = (0..CHANNELS)
            .map(|_| {
                &*Box::leak(
                    (0..RAMP_LEN)
                        .map(|i| AtomicSample::new(ramp_sample(i, 0)))
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                )
            })
            .collect();
        let buffers: Vec<ExternalBufferParameters<Sample>> = ramps
            .iter()
            .map(|buf| unsafe {
                ExternalBufferParameters::new(
                    buf.as_ptr(),
                    buf.len(),
                    1,
                    Arc::new(RwLock::new(true)),
                    None,
                )
            })
            .collect();
        // buffer index = media timestamp
        let anchor = StreamAnchor::new_started(0);
        server
            .transmit_from_external_buffer(
                buffers,
                anchor,
                Arc::new(AtomicUsize::new(usize::MAX)),
                None,
            )
            .await;
        (server, hostname, ramps)
    })
}

/// Pre-seeds the plugin device's saved RX subscriptions so it subscribes to the library TX on start.
fn seed_plugin_subscriptions(ids: &TestIds, tx_hostname: &str) {
    let dir = state_home()
        .join("inferno_aoip")
        .join(plugin_device_id(ids));
    std::fs::create_dir_all(&dir).unwrap();
    let mut content = String::new();
    for ch in 1..=CHANNELS {
        content += &format!(
      "[[channels]]\nlocal_channel_id = {ch}\nlocal_channel_name = \"{ch:02}\"\ntx_channel_name = \"TX {ch}\"\ntx_hostname = \"{tx_hostname}\"\n\n"
    );
    }
    std::fs::write(dir.join("rx_subscriptions.toml"), content).unwrap();
}

/// Points state storage of all devices in this process at a fresh temporary directory.
fn set_state_home() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| std::env::set_var("XDG_STATE_HOME", state_home()));
}

struct CaptureFixture {
    rt: tokio::runtime::Runtime,
    _clock: ClockServer,
    tx_server: Option<DeviceServer>,
    pcm: Option<Pcm>,
    ramps: Vec<&'static [AtomicSample]>,
    epoch: usize,
}

impl CaptureFixture {
    fn new(ids: &TestIds) -> Self {
        Self::with_pcm(ids, |config| {
            Pcm::open(config, SND_PCM_STREAM_CAPTURE, 85_333)
        })
    }
    /// `open_pcm` opens the capture PCM from the plugin's ALSA config
    fn with_pcm(ids: &TestIds, open_pcm: impl FnOnce(&str) -> Pcm) -> Self {
        init_test_env();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(4)
            .build()
            .unwrap();
        let clock = ClockServer::start(ids.clock_path());
        let (tx_server, tx_hostname, ramps) = start_library_transmitter(&rt, ids);
        seed_plugin_subscriptions(ids, &tx_hostname);
        let pcm = open_pcm(&plugin_config(ids, &format!("plug-rx-{}", ids.tag)));
        pcm.prepare();
        pcm.start();
        let waited = capture_until_audio(&pcm, 0, ROUTING_TIMEOUT)
            .expect("routing never established: no audio captured");
        log::info!("initial routing established after {waited:?}");
        Self {
            rt,
            _clock: clock,
            tx_server: Some(tx_server),
            pcm: Some(pcm),
            ramps,
            epoch: 0,
        }
    }
    fn pcm(&self) -> &Pcm {
        self.pcm.as_ref().unwrap()
    }
    /// Changes what the library transmitter sends from now on. Call while the PCM is stopped.
    fn next_epoch(&mut self) {
        self.epoch += 1;
        for ramp in &self.ramps {
            for (i, s) in ramp.iter().enumerate() {
                s.store(ramp_sample(i, self.epoch), Ordering::Relaxed);
            }
        }
    }
    /// prepare + start after a stop or xrun; returns how long until post-restart audio arrives
    fn restart_and_wait_for_audio(&mut self) -> Option<Duration> {
        self.next_epoch();
        let pcm = self.pcm.as_ref().unwrap();
        let t0 = Instant::now();
        pcm.prepare();
        pcm.start();
        capture_until_audio(pcm, self.epoch, ROUTING_TIMEOUT).map(|_| t0.elapsed())
    }
}

impl Drop for CaptureFixture {
    fn drop(&mut self) {
        drop(self.pcm.take());
        if let Some(s) = self.tx_server.take() {
            self.rt.block_on(s.shutdown());
        }
    }
}

/// T1: transport stop/start (drop → prepare → start) must not tear down Dante subscriptions.
#[test]
fn capture_restart_resumes_quickly() {
    let ids = TestIds {
        base_port: 21000,
        base_pid: 21,
        tag: "caprestart",
        latency_ns: LATENCY_NS,
    };
    let mut fx = CaptureFixture::new(&ids);

    for round in 0..3 {
        fx.pcm().drop_stream();
        let gap = fx.restart_and_wait_for_audio();
        log::info!("capture restart round {round}: audio resumed after {gap:?}");
        let gap = gap.expect("audio never resumed after restart");
        assert!(
            gap < MAX_RESTART_GAP,
            "audio resumed after {gap:?}, expected < {MAX_RESTART_GAP:?}"
        );
    }
}

/// T2: xrun recovery (prepare → start after -EPIPE) must not tear down Dante subscriptions.
#[test]
fn capture_xrun_recovers_quickly() {
    let ids = TestIds {
        base_port: 22000,
        base_pid: 22,
        tag: "capxrun",
        latency_ns: LATENCY_NS,
    };
    let mut fx = CaptureFixture::new(&ids);

    for round in 0..3 {
        let pcm = fx.pcm();
        let mut buf = vec![0 as Sample; pcm.period_size * CHANNELS];
        // stall longer than the whole buffer → overrun
        std::thread::sleep(Duration::from_micros(
            (pcm.buffer_size as u64 * 3 * 1_000_000) / SAMPLE_RATE as u64,
        ));
        let mut r = 0;
        for _ in 0..(pcm.buffer_size / pcm.period_size + 2) {
            r = pcm.read(&mut buf);
            if r < 0 {
                break;
            }
        }
        assert_eq!(r, -(libc::EPIPE as i64), "expected an xrun after stalling");
        let gap = fx.restart_and_wait_for_audio();
        log::info!("xrun round {round}: audio resumed after {gap:?}");
        let gap = gap.expect("audio never resumed after xrun");
        assert!(
            gap < MAX_RESTART_GAP,
            "audio resumed after {gap:?}, expected < {MAX_RESTART_GAP:?}"
        );
    }
}

/// T4 (capture): snd_pcm_delay must include the network receive latency so DAWs can compensate.
#[test]
fn capture_delay_includes_network_latency() {
    let ids = TestIds {
        base_port: 23000,
        base_pid: 23,
        tag: "capdelay",
        latency_ns: LATENCY_NS,
    };
    let fx = CaptureFixture::new(&ids);
    let pcm = fx.pcm();
    std::thread::sleep(Duration::from_millis(50));
    let avail = pcm.avail();
    let delay = pcm.delay();
    log::info!("capture: avail {avail}, delay {delay}, latency {LATENCY_SAMPLES}");
    assert!(avail >= 0);
    // allow for frames captured between the two calls
    assert!(
    delay >= avail + LATENCY_SAMPLES - DELAY_TOLERANCE,
    "capture delay {delay} doesn't include network latency ({LATENCY_SAMPLES} samples, avail {avail})"
  );
}

/// Library-side RX device subscribed to the plugin's TX channels, recording when audio arrives.
struct PlaybackFixture {
    rt: tokio::runtime::Runtime,
    _clock: ClockServer,
    rx_server: Option<DeviceServer>,
    pcm: Option<Pcm>,
    /// value changes in the received sample stream: (sample index, new value, time delivered)
    arrivals: Arc<Mutex<Vec<(usize, Sample, Instant)>>>,
    buf: Vec<Sample>,
    epoch: usize,
}

impl PlaybackFixture {
    fn new(ids: &TestIds) -> Self {
        init_test_env();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(4)
            .build()
            .unwrap();
        let clock = ClockServer::start(ids.clock_path());
        let plugin_name = format!("plug-tx-{}", ids.tag);
        let pcm = Pcm::open(
            &plugin_config(ids, &plugin_name),
            SND_PCM_STREAM_PLAYBACK,
            85_333,
        );
        let buf = vec![TX_PATTERN; pcm.period_size * CHANNELS];

        let arrivals: Arc<Mutex<Vec<(usize, Sample, Instant)>>> = Default::default();
        let mut rx_server = rt.block_on(DeviceServer::start(library_settings(
            &format!("lib-rx-{}", ids.tag),
            ids,
            CHANNELS,
            0,
        )));
        let a = arrivals.clone();
        let mut last_value: Sample = 0;
        let mut received = 0usize;
        rt.block_on(rx_server.receive_with_callback(Box::new(
            move |count: usize, channels: &Vec<Vec<Sample>>| {
                let now = Instant::now();
                for (i, &s) in channels[0][..count].iter().enumerate() {
                    if s != last_value {
                        a.lock().unwrap().push((received + i, s, now));
                        last_value = s;
                    }
                }
                received += count;
            },
        )));

        let mut fx = Self {
            rt,
            _clock: clock,
            rx_server: Some(rx_server),
            pcm: Some(pcm),
            arrivals,
            buf,
            epoch: 0,
        };
        fx.restart();
        // the plugin's device must be up (it is started by prepare) before subscribing
        for ch in 0..CHANNELS {
            let rx = fx.rx_server.as_ref().unwrap();
            fx.rt
                .block_on(rx.subscribe(ch, &format!("TX {}", ch + 1), &plugin_name));
        }
        let deadline = Instant::now() + ROUTING_TIMEOUT;
        while fx.arrivals.lock().unwrap().is_empty() {
            assert!(
                Instant::now() < deadline,
                "routing never established: no audio received"
            );
            fx.pump(Duration::from_millis(10));
        }
        log::info!("playback routing established");
        fx
    }

    fn pcm(&self) -> &Pcm {
        self.pcm.as_ref().unwrap()
    }

    /// prepare, prefill whole buffer, start (what JACK/DAWs do for playback)
    fn restart(&mut self) {
        let pcm = self.pcm.as_ref().unwrap();
        pcm.prepare();
        while pcm.write(&self.buf) > 0 {}
        pcm.start_if_prepared();
    }

    /// keep the playback buffer filled for `duration`
    fn pump(&mut self, duration: Duration) {
        let end = Instant::now() + duration;
        let pcm = self.pcm.as_ref().unwrap();
        while Instant::now() < end {
            let r = pcm.write(&self.buf);
            if r == -(libc::EPIPE as i64) {
                log::warn!("playback xrun while pumping, recovering");
                pcm.prepare();
                while pcm.write(&self.buf) > 0 {}
                pcm.start_if_prepared();
                continue;
            }
            assert!(r >= 0, "snd_pcm_writei failed: {r}");
            if r == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Changes what the app plays from now on. Call while the PCM is stopped.
    fn next_epoch(&mut self) {
        self.epoch += 1;
        self.buf.fill(Self::pattern(self.epoch));
    }
    fn pattern(epoch: usize) -> Sample {
        TX_PATTERN + ((epoch as Sample) << 16)
    }
    /// For audio of the current epoch: (silence before it in the received stream, delivery time since `since`).
    /// Both are needed: the receiver inserts silence for missing packets of a live flow, but its time
    /// stops when no flow is alive, and delivery time is only as precise as the read interval.
    fn current_epoch_arrival(&self, since: Instant) -> Option<(Duration, Duration)> {
        let pattern = Self::pattern(self.epoch);
        let arrivals = self.arrivals.lock().unwrap();
        let new_index = arrivals.iter().position(|&(_, s, _)| s == pattern)?;
        let (new_pos, _, delivered) = arrivals[new_index];
        let silence_samples = match arrivals[..new_index].last() {
            Some(&(silence_start, 0, _)) => new_pos - silence_start,
            _ => 0,
        };
        let silence =
            Duration::from_nanos(silence_samples as u64 * 1_000_000_000 / SAMPLE_RATE as u64);
        Some((silence, delivered.saturating_duration_since(since)))
    }
}

impl Drop for PlaybackFixture {
    fn drop(&mut self) {
        drop(self.pcm.take());
        if let Some(s) = self.rx_server.take() {
            self.rt.block_on(s.shutdown());
        }
    }
}

/// T3: playback transport stop/start must not tear down the Dante transmitter and its flows.
#[test]
fn playback_restart_resumes_quickly() {
    let ids = TestIds {
        base_port: 24000,
        base_pid: 24,
        tag: "pbrestart",
        latency_ns: LATENCY_NS,
    };
    let mut fx = PlaybackFixture::new(&ids);
    fx.pump(Duration::from_millis(500));

    for round in 0..3 {
        let t0 = Instant::now();
        fx.pcm().drop_stream();
        fx.next_epoch();
        fx.restart();
        fx.pump(Duration::from_millis(1500));
        let arrival = fx.current_epoch_arrival(t0);
        log::info!("playback restart round {round}: (silence, delivered after) = {arrival:?}");
        let (silence, delivered) = arrival.expect("receiver never got audio played after restart");
        assert!(
            silence < MAX_RESTART_GAP,
            "receiver got {silence:?} of silence, expected < {MAX_RESTART_GAP:?}"
        );
        let max_delivery = MAX_RESTART_GAP + LIBRARY_RX_READ_INTERVAL;
        assert!(
            delivered < max_delivery,
            "audio delivered after {delivered:?}, expected < {max_delivery:?}"
        );
    }
}

/// T4 (playback): snd_pcm_delay must include the network latency demanded from receivers.
#[test]
fn playback_delay_includes_network_latency() {
    let ids = TestIds {
        base_port: 25000,
        base_pid: 25,
        tag: "pbdelay",
        latency_ns: LATENCY_NS,
    };
    let mut fx = PlaybackFixture::new(&ids);
    fx.pump(Duration::from_millis(100));
    let pcm = fx.pcm();
    let hw_avail = pcm.buffer_size as i64 - pcm.avail();
    let delay = pcm.delay();
    log::info!("playback: buffered {hw_avail}, delay {delay}, latency {LATENCY_SAMPLES}");
    assert!(
    delay >= hw_avail + LATENCY_SAMPLES - DELAY_TOLERANCE,
    "playback delay {delay} doesn't include network latency ({LATENCY_SAMPLES} samples, buffered {hw_avail})"
  );
}

/// Value of frame `n` (from 1) of the counter played in T5: never zero, +1 in the 24 transmitted bits.
fn counter_sample(n: usize) -> Sample {
    (((n - 1) % 0x7f_ffff + 1) << 8) as Sample
}

/// T5: an application keeping the playback buffer full (like aplay) must not overwrite samples
/// the transmitter hasn't sent yet: the receiver must get the played stream bit-exact.
#[test]
fn playback_full_buffer_is_bit_exact() {
    let ids = TestIds {
        base_port: 26000,
        base_pid: 26,
        tag: "pbexact",
        // headroom for scheduling stalls of the test VM, the overwrite race doesn't depend on latency
        latency_ns: 50_000_000,
    };
    let mut fx = PlaybackFixture::new(&ids);
    let first_received = fx.arrivals.lock().unwrap().len();

    // keep the buffer full: write whatever fits, as soon as it fits
    let pcm = fx.pcm.as_ref().unwrap();
    let mut chunk = vec![0 as Sample; pcm.period_size * CHANNELS];
    let mut next_frame = 1usize;
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        for (i, frame) in chunk.chunks_mut(CHANNELS).enumerate() {
            frame.fill(counter_sample(next_frame + i));
        }
        let r = pcm.write(&chunk);
        assert!(r >= 0, "snd_pcm_writei failed: {r}");
        next_frame += r as usize;
        if r == 0 {
            std::thread::sleep(Duration::from_micros(200));
        }
    }
    fx.pump(Duration::from_millis(300)); // flush the counter out of the buffer
    let played = next_frame - 1;
    log::info!("played {played} counter frames");

    // a copy: asserting while holding the lock would poison it for the receiver thread
    let arrivals = fx.arrivals.lock().unwrap().clone();
    let ramp = &arrivals[first_received..];
    let start = ramp
        .iter()
        .position(|&(_, s, _)| s == counter_sample(1))
        .expect("receiver never got the counter");
    let mut errors = vec![];
    let mut expected = 2usize;
    for w in ramp[start..].windows(2) {
        let ((pos0, _, _), (pos1, s1, _)) = (w[0], w[1]);
        if expected > played {
            break; // the counter ended
        }
        if pos1 != pos0 + 1 || s1 != counter_sample(expected) {
            errors.push((pos1, expected, s1 >> 8));
        }
        expected += pos1 - pos0;
    }
    assert!(
        expected > played,
        "only {expected} of {played} counter frames received"
    );
    assert!(
        errors.is_empty(),
        "{} discontinuities in {expected} received frames, first (position, expected frame, got value): {:?}",
        errors.len(),
        &errors[..errors.len().min(10)]
    );
}

/// T6: when the application stops writing (stalled or crashed, so nobody calls into the PCM to detect the
/// underrun), receivers must get silence after the last written frame, not the stale buffer contents again.
#[test]
fn playback_underrun_sends_silence() {
    let ids = TestIds {
        base_port: 27000,
        base_pid: 27,
        tag: "pbunderrun",
        latency_ns: LATENCY_NS,
    };
    let fx = PlaybackFixture::new(&ids);
    let first_received = fx.arrivals.lock().unwrap().len();

    let pcm = fx.pcm.as_ref().unwrap();
    let mut chunk = vec![0 as Sample; pcm.period_size * CHANNELS];
    let mut next_frame = 1usize;
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        for (i, frame) in chunk.chunks_mut(CHANNELS).enumerate() {
            frame.fill(counter_sample(next_frame + i));
        }
        let r = pcm.write(&chunk);
        assert!(r >= 0, "snd_pcm_writei failed: {r}");
        next_frame += r as usize;
        if r == 0 {
            std::thread::sleep(Duration::from_micros(200));
        }
    }
    let last = counter_sample(next_frame - 1);
    // the application stalls: no more calls into the PCM
    std::thread::sleep(Duration::from_secs(1));

    // a copy: asserting while holding the lock would poison it for the receiver thread
    let arrivals = fx.arrivals.lock().unwrap().clone();
    let after_last: Vec<Sample> = arrivals[first_received..]
        .iter()
        .skip_while(|&&(_, s, _)| s != last)
        .skip(1)
        .map(|&(_, s, _)| s >> 8)
        .collect();
    assert!(
        arrivals[first_received..]
            .iter()
            .any(|&(_, s, _)| s == last),
        "the last written frame was never received"
    );
    assert_eq!(
        after_last,
        vec![0],
        "after the last written frame the receiver must get only silence, got values (counter frames): {:?}",
        &after_last[..after_last.len().min(10)]
    );
}

/// Capture latency used by T7/T8, like Ardour on Docker Desktop: 40 ms = 1920 samples.
const LONG_LATENCY_NS: u32 = 40_000_000;

/// T7: a capture buffer that can't hold the network latency plus a period is refused: the receiver writes
/// one latency ahead of the hardware pointer, so it would overwrite samples the application hasn't read.
#[test]
fn capture_buffer_too_small_for_latency_is_refused() {
    let ids = TestIds {
        base_port: 28000,
        base_pid: 28,
        tag: "capsmall",
        latency_ns: LONG_LATENCY_NS,
    };
    init_test_env();
    let _clock = ClockServer::start(ids.clock_path());
    let config = plugin_config(&ids, "plug-rx-capsmall");
    // 1920 + 1024 > 2048 (Ardour's default 1024x2)
    let r = Pcm::open_with(&config, SND_PCM_STREAM_CAPTURE, 1024, 2048).err();
    assert!(
        r.is_some(),
        "1024x2 capture buffer accepted with 40 ms latency"
    );
    assert!(
        Pcm::open_with(&config, SND_PCM_STREAM_CAPTURE, 1024, 4096).is_ok(),
        "1024x4 capture buffer refused with 40 ms latency"
    );
}

/// T8: an application reading capture late (more than buffer - latency unread) must get an xrun,
/// never audio overwritten by the receiver.
#[test]
fn capture_reading_late_reports_xrun_instead_of_overwritten_audio() {
    let ids = TestIds {
        base_port: 29000,
        base_pid: 29,
        tag: "caplate",
        latency_ns: LONG_LATENCY_NS,
    };
    let fx = CaptureFixture::with_pcm(&ids, |config| {
        Pcm::open_with(config, SND_PCM_STREAM_CAPTURE, 1024, 4096).expect("1024x4 refused")
    });
    let pcm = fx.pcm();
    let mut buf = vec![0 as Sample; pcm.period_size * CHANNELS];
    let mut last_ts: Option<usize> = None;
    let mut discontinuities = vec![];
    let mut xruns = 0;
    for round in 0..20 {
        // every 4th period, read late: ~2900 frames pending > 4096 - 1920
        if round % 4 == 3 {
            std::thread::sleep(Duration::from_millis(60));
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        let r = loop {
            let r = pcm.read(&mut buf);
            if r != 0 || Instant::now() > deadline {
                break r;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        if r == -(libc::EPIPE as i64) {
            xruns += 1;
            last_ts = None;
            pcm.prepare();
            pcm.start();
            continue;
        }
        assert!(r > 0, "snd_pcm_readi failed or timed out: {r}");
        for frame in buf[..r as usize * CHANNELS].chunks(CHANNELS) {
            let s = frame[0];
            if !is_fresh_ramp_sample(s, 0) {
                last_ts = None; // silence after (re)start
                continue;
            }
            let ts = (s as u32 >> 9) as usize;
            if let Some(prev) = last_ts {
                if ts != (prev + 1) & (RAMP_LEN - 1) {
                    discontinuities.push((round, prev, ts));
                }
            }
            last_ts = Some(ts);
        }
    }
    log::info!(
        "late capture: {xruns} xruns, {} discontinuities",
        discontinuities.len()
    );
    assert!(
        discontinuities.is_empty(),
        "captured audio is discontinuous (overwritten by the receiver?), first (round, prev ts, ts): {:?}",
        &discontinuities[..discontinuities.len().min(5)]
    );
    assert!(xruns > 0, "late reads must be reported as xruns");
}

/// Non-silent captured samples that aren't fresh ramp samples of `epoch` (stale or misplaced audio).
fn stale_samples(buf: &[Sample], epoch: usize) -> usize {
    buf.iter()
        .filter(|&&s| s != 0 && !is_fresh_ramp_sample(s, epoch))
        .count()
}

/// T9: when the transmitter stops (crash, network loss), the capture must get silence, not the receive
/// buffer's old contents for the missing packets.
#[test]
fn capture_after_transmitter_stops_is_silent() {
    let ids = TestIds {
        base_port: 30000,
        base_pid: 30,
        tag: "captxstop",
        latency_ns: LONG_LATENCY_NS,
    };
    // like Ardour at 40 ms latency
    let mut fx = CaptureFixture::with_pcm(&ids, |config| {
        Pcm::open_with(config, SND_PCM_STREAM_CAPTURE, 1024, 4096).expect("1024x4 refused")
    });
    let mut buf = vec![0 as Sample; fx.pcm().period_size * CHANNELS];
    // let the buffer fill with audio
    let until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < until {
        let r = fx.pcm().read(&mut buf);
        assert!(r >= 0, "snd_pcm_readi failed: {r}");
        std::thread::sleep(Duration::from_millis(2));
    }
    let tx = fx.tx_server.take().unwrap();
    fx.rt.block_on(tx.shutdown());
    let stopped = Instant::now();
    let mut stale = 0;
    let mut audio_after = 0;
    while stopped.elapsed() < Duration::from_millis(1500) {
        let r = fx.pcm().read(&mut buf);
        if r == -(libc::EPIPE as i64) {
            fx.pcm().prepare();
            fx.pcm().start();
            continue;
        }
        assert!(r >= 0, "snd_pcm_readi failed: {r}");
        let data = &buf[..r as usize * CHANNELS];
        // audio still in flight is fine during the latency; afterwards only silence
        if stopped.elapsed() > Duration::from_millis(200) {
            audio_after += data.iter().filter(|&&s| s != 0).count();
        }
        stale += stale_samples(data, 0);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        stale, 0,
        "{stale} stale samples captured after the transmitter stopped"
    );
    assert_eq!(
        audio_after, 0,
        "{audio_after} non-silent samples long after the transmitter stopped"
    );
}

/// T10: right after restarts (as in xrun recovery), captured audio must be fresh audio of the new run,
/// never misplaced samples written with the previous run's addressing.
#[test]
fn capture_restart_has_no_misplaced_samples() {
    let ids = TestIds {
        base_port: 31000,
        base_pid: 31,
        tag: "capmisplace",
        latency_ns: LONG_LATENCY_NS,
    };
    // like Ardour at 40 ms latency
    let mut fx = CaptureFixture::with_pcm(&ids, |config| {
        Pcm::open_with(config, SND_PCM_STREAM_CAPTURE, 1024, 4096).expect("1024x4 refused")
    });
    let mut buf = vec![0 as Sample; fx.pcm().period_size * CHANNELS];
    let mut stale = 0;
    for _round in 0..40 {
        fx.pcm().drop_stream();
        fx.next_epoch();
        fx.pcm().prepare();
        fx.pcm().start();
        let until = Instant::now() + Duration::from_millis(60);
        while Instant::now() < until {
            let r = fx.pcm().read(&mut buf);
            if r == -(libc::EPIPE as i64) {
                break;
            }
            assert!(r >= 0, "snd_pcm_readi failed: {r}");
            stale += stale_samples(&buf[..r as usize * CHANNELS], fx.epoch);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    assert_eq!(
        stale, 0,
        "{stale} stale or misplaced samples captured right after restarts"
    );
}

/// T11: a valid configuration read on time (each period up to about half a period late, like Ardour does)
/// must not get xruns: only real overwrites of unread samples may be reported.
#[test]
fn capture_read_slightly_late_has_no_false_xruns() {
    let ids = TestIds {
        base_port: 32000,
        base_pid: 32,
        tag: "capnofalse",
        latency_ns: LATENCY_NS,
    };
    // 10 ms (480 frames) + 1024 <= 2048
    let fx = CaptureFixture::with_pcm(&ids, |config| {
        Pcm::open_with(config, SND_PCM_STREAM_CAPTURE, 1024, 2048).expect("1024x2 refused")
    });
    let pcm = fx.pcm();
    let mut buf = vec![0 as Sample; pcm.period_size * CHANNELS];
    let (mut xruns, mut discontinuities, mut frames) = (0, 0, 0usize);
    let mut last_ts: Option<usize> = None;
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        // wait for a period, then process it a bit late: 2..6 ms, within the 1024 - 480 frames (11 ms) left
        while pcm.avail() < pcm.period_size as i64 {
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(2 + (frames / 1024 % 5) as u64));
        let r = pcm.read(&mut buf);
        if r == -(libc::EPIPE as i64) {
            xruns += 1;
            last_ts = None;
            pcm.prepare();
            pcm.start();
            continue;
        }
        assert!(r >= 0, "snd_pcm_readi failed: {r}");
        frames += r as usize;
        for frame in buf[..r as usize * CHANNELS].chunks(CHANNELS) {
            if !is_fresh_ramp_sample(frame[0], 0) {
                last_ts = None;
                continue;
            }
            let ts = (frame[0] as u32 >> 9) as usize;
            if last_ts.is_some_and(|prev| ts != (prev + 1) & (RAMP_LEN - 1)) {
                discontinuities += 1;
            }
            last_ts = Some(ts);
        }
    }
    log::info!(
        "slightly late capture: {frames} frames, {xruns} xruns, {discontinuities} discontinuities"
    );
    assert_eq!(discontinuities, 0, "captured audio is discontinuous");
    assert_eq!(xruns, 0, "false xruns: nothing unread was overwritten");
}

/// T12: xrun recovery like Ardour's (stall until underrun, drop, prepare, prefill silence, start): after the
/// old run's audio has ended, none of it may be transmitted again (e.g. read from the ring with stale addressing).
#[test]
fn playback_xrun_recovery_sends_no_old_audio() {
    let ids = TestIds {
        base_port: 33000,
        base_pid: 33,
        tag: "pbrecover",
        latency_ns: LONG_LATENCY_NS,
    };
    let mut fx = PlaybackFixture::new(&ids);
    let first_received = fx.arrivals.lock().unwrap().len();
    for round in 0..3 {
        // the old run: a counter, keeping the buffer full
        let pcm = fx.pcm.as_ref().unwrap();
        let mut chunk = vec![0 as Sample; pcm.period_size * CHANNELS];
        let mut next_frame = 1usize;
        let end = Instant::now() + Duration::from_millis(700);
        while Instant::now() < end {
            for (i, frame) in chunk.chunks_mut(CHANNELS).enumerate() {
                frame.fill(counter_sample(next_frame + i));
            }
            let r = pcm.write(&chunk);
            assert!(r >= 0, "snd_pcm_writei failed: {r}");
            next_frame += r as usize;
            if r == 0 {
                std::thread::sleep(Duration::from_micros(500));
            }
        }
        // stall: no calls into the PCM until the buffer has run out
        std::thread::sleep(Duration::from_millis(300));
        let pcm = fx.pcm.as_ref().unwrap();
        pcm.drop_stream();
        pcm.prepare();
        let silence = vec![0 as Sample; pcm.period_size * CHANNELS];
        for _ in 0..3 {
            pcm.write(&silence);
        }
        pcm.start_if_prepared();
        fx.next_epoch();
        fx.pump(Duration::from_millis(700));
        log::info!("round {round} done");
    }
    let values: Vec<Sample> = fx.arrivals.lock().unwrap()[first_received..]
        .iter()
        .map(|&(_, s, _)| s)
        .collect();
    // counter samples are multiples of 256 below the patterns' range
    let is_counter = |s: Sample| s != 0 && s & 0xff == 0 && (s >> 8) < 0x10000;
    let mut stale = 0;
    let mut in_counter_run = false;
    let mut silent_after_counter = false;
    for &s in &values {
        if is_counter(s) {
            if silent_after_counter && !in_counter_run {
                stale += 1;
            }
            in_counter_run = true;
        } else if s == 0 {
            if in_counter_run {
                silent_after_counter = true;
            }
            in_counter_run = false;
        } else {
            // a new epoch's pattern: the next counter run is legitimate
            in_counter_run = false;
            silent_after_counter = false;
        }
    }
    assert_eq!(
        stale, 0,
        "{stale} fragments of old audio transmitted after xrun recoveries"
    );
}

/// T13: an application only capturing still provides the device's transmit channels: receivers subscribed to them
/// get a live (silent) flow instead of a broken stream.
#[test]
fn capture_only_device_transmits_silence() {
    let ids = TestIds {
        base_port: 34000,
        base_pid: 34,
        tag: "caponlytx",
        latency_ns: LATENCY_NS,
    };
    init_test_env();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .build()
        .unwrap();
    let _clock = ClockServer::start(ids.clock_path());
    let plugin_name = format!("plug-caponly-{}", ids.tag);
    let pcm = Pcm::open(
        &plugin_config(&ids, &plugin_name),
        SND_PCM_STREAM_CAPTURE,
        85_333,
    );
    pcm.prepare();
    pcm.start();

    let mut rx_server = rt.block_on(DeviceServer::start(library_settings(
        &format!("lib-rx-{}", ids.tag),
        &ids,
        CHANNELS,
        0,
    )));
    // the receiver delivers samples only while a flow is alive
    let received = Arc::new(AtomicUsize::new(0));
    let nonzero = Arc::new(AtomicUsize::new(0));
    let (r, n) = (received.clone(), nonzero.clone());
    rt.block_on(rx_server.receive_with_callback(Box::new(
        move |count: usize, channels: &Vec<Vec<Sample>>| {
            r.fetch_add(count, Ordering::Relaxed);
            n.fetch_add(
                channels[0][..count].iter().filter(|&&s| s != 0).count(),
                Ordering::Relaxed,
            );
        },
    )));
    for ch in 0..CHANNELS {
        rt.block_on(rx_server.subscribe(ch, &format!("TX {}", ch + 1), &plugin_name));
    }
    let mut buf = vec![0 as Sample; pcm.period_size * CHANNELS];
    let deadline = Instant::now() + ROUTING_TIMEOUT;
    while received.load(Ordering::Relaxed) < SAMPLE_RATE as usize && Instant::now() < deadline {
        // keep capturing, like a recording application
        let r = pcm.read(&mut buf);
        assert!(
            r >= 0 || r == -(libc::EPIPE as i64),
            "snd_pcm_readi failed: {r}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let received = received.load(Ordering::Relaxed);
    log::info!("capture-only device: receiver got {received} samples");
    assert!(
        received >= SAMPLE_RATE as usize,
        "receiver got only {received} samples from a capture-only device's transmit channels"
    );
    assert_eq!(
        nonzero.load(Ordering::Relaxed),
        0,
        "transmitted audio must be silence"
    );
    drop(pcm);
    rt.block_on(rx_server.shutdown());
}

/// T14: when a capturing application also opens playback (and closes it again), the transmitter switches between
/// silence and the application's audio without tearing down flows: receivers keep receiving throughout.
#[test]
fn transmitter_switches_between_silence_and_playback_without_dropping_flows() {
    let ids = TestIds {
        base_port: 35000,
        base_pid: 35,
        tag: "txswitch",
        latency_ns: LATENCY_NS,
    };
    init_test_env();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .build()
        .unwrap();
    let _clock = ClockServer::start(ids.clock_path());
    let plugin_name = format!("plug-txswitch-{}", ids.tag);
    let config = plugin_config(&ids, &plugin_name);
    let capture = Pcm::open(&config, SND_PCM_STREAM_CAPTURE, 85_333);
    capture.prepare();
    capture.start();

    let mut rx_server = rt.block_on(DeviceServer::start(library_settings(
        &format!("lib-rx-{}", ids.tag),
        &ids,
        CHANNELS,
        0,
    )));
    let received = Arc::new(AtomicUsize::new(0));
    let first_pattern: Arc<Mutex<Option<Instant>>> = Default::default();
    let (r, fp) = (received.clone(), first_pattern.clone());
    rt.block_on(rx_server.receive_with_callback(Box::new(
        move |count: usize, channels: &Vec<Vec<Sample>>| {
            r.fetch_add(count, Ordering::Relaxed);
            if channels[0][..count].contains(&TX_PATTERN) {
                fp.lock().unwrap().get_or_insert(Instant::now());
            }
        },
    )));
    for ch in 0..CHANNELS {
        rt.block_on(rx_server.subscribe(ch, &format!("TX {}", ch + 1), &plugin_name));
    }
    let mut cbuf = vec![0 as Sample; capture.period_size * CHANNELS];
    let mut capture_for = |d: Duration, playback: Option<(&Pcm, &[Sample])>| {
        let end = Instant::now() + d;
        while Instant::now() < end {
            let r = capture.read(&mut cbuf);
            assert!(
                r >= 0 || r == -(libc::EPIPE as i64),
                "snd_pcm_readi failed: {r}"
            );
            if let Some((pcm, buf)) = playback {
                while pcm.write(buf) > 0 {}
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    // silence flowing from the capture-only device
    let deadline = Instant::now() + ROUTING_TIMEOUT;
    while received.load(Ordering::Relaxed) < SAMPLE_RATE as usize / 2 && Instant::now() < deadline {
        capture_for(Duration::from_millis(20), None);
    }
    assert!(
        received.load(Ordering::Relaxed) >= SAMPLE_RATE as usize / 2,
        "no flow from the capture-only device"
    );

    // samples received during `d` must keep up with real time (the receiver only counts while a flow is alive)
    let assert_flowing = |what: &str, before: usize, d: Duration| {
        let got = received.load(Ordering::Relaxed) - before;
        let expected = (d.as_secs_f64() * SAMPLE_RATE as f64) as usize;
        assert!(
            got > expected * 8 / 10,
            "{what}: received {got} samples in {d:?}, expected ~{expected}"
        );
    };

    // the application starts playing too
    let playback = Pcm::open(&config, SND_PCM_STREAM_PLAYBACK, 85_333);
    let pbuf = vec![TX_PATTERN; playback.period_size * CHANNELS];
    let started = Instant::now();
    let before = received.load(Ordering::Relaxed);
    playback.prepare();
    while playback.write(&pbuf) > 0 {}
    playback.start_if_prepared();
    capture_for(Duration::from_millis(1500), Some((&playback, &pbuf)));
    assert_flowing("while switching to playback", before, started.elapsed());
    let audio_after = first_pattern.lock().unwrap().map(|t| t - started);
    log::info!("playback audio received after {audio_after:?}");
    let audio_after = audio_after.expect("receiver never got the played audio");
    assert!(
        audio_after < Duration::from_millis(500),
        "played audio arrived after {audio_after:?}"
    );

    // the application stops playing but keeps capturing
    drop(playback);
    let stopped = Instant::now();
    let before = received.load(Ordering::Relaxed);
    capture_for(Duration::from_millis(1500), None);
    assert_flowing("after closing playback", before, stopped.elapsed());

    drop(capture);
    rt.block_on(rx_server.shutdown());
}

/// T15: changing the capture buffer configuration (e.g. a DAW changing its buffer size) must not tear down the
/// receiver and its subscriptions: fresh audio resumes within about the latency, not after re-subscribing.
#[test]
fn capture_reconfiguration_keeps_subscriptions() {
    let ids = TestIds {
        base_port: 36000,
        base_pid: 36,
        tag: "capreconf",
        latency_ns: LATENCY_NS,
    };
    let mut fx = CaptureFixture::with_pcm(&ids, |config| {
        Pcm::open_with(config, SND_PCM_STREAM_CAPTURE, 1024, 4096).expect("1024x4 refused")
    });
    for (round, (period, buffer)) in [(512, 2048), (1024, 4096), (256, 2048)]
        .into_iter()
        .enumerate()
    {
        fx.pcm().drop_stream();
        fx.next_epoch();
        fx.pcm
            .as_mut()
            .unwrap()
            .set_hw_params(period, buffer)
            .expect("reconfiguration refused");
        let t0 = Instant::now();
        fx.pcm().prepare();
        fx.pcm().start();
        let gap = capture_until_audio(fx.pcm(), fx.epoch, ROUTING_TIMEOUT).map(|_| t0.elapsed());
        log::info!(
            "reconfiguration {round} to {period}x{}: audio resumed after {gap:?}",
            buffer / period
        );
        let gap = gap.expect("audio never resumed after reconfiguration");
        assert!(
            gap < MAX_RESTART_GAP,
            "audio resumed after {gap:?}, expected < {MAX_RESTART_GAP:?}"
        );
    }
}
