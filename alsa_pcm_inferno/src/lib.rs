extern crate alsa_sys_all;
extern crate libc;
use alsa_sys_all::*;

use futures_util::FutureExt;
use inferno_aoip::device_info::DeviceId;
use inferno_aoip::device_server::{
    AtomicSample, Clock, DeviceServer, ExternalBufferParameters, MediaClock,
    PositionReportDestination, RealTimeClockReceiver, Sample, Settings, StreamAnchor,
    TransferNotifier,
};
use inferno_aoip::utils::run_future_in_new_thread;
use itertools::Itertools;
use lazy_static::lazy_static;
use libc::{c_char, c_int, c_void, eventfd, EFD_CLOEXEC, EFD_NONBLOCK, EPIPE, POLLIN, POLLOUT};
use log::{debug, error, warn};
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::mem::zeroed;
use std::num::Wrapping;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

struct StartArgs {
    channels: Vec<ExternalBufferParameters<Sample>>,
    anchor: StreamAnchor,
    clock_rx_tx: oneshot::Sender<RealTimeClockReceiver>,
    current_timestamp: Arc<AtomicUsize>,
    on_transfer: Option<TransferNotifier>,
}

enum Command {
    StartReceiver(StartArgs),
    StartTransmitter(StartArgs),
    StopReceiver,
    StopTransmitter,
    Shutdown,
}

struct InfernoInstance {
    commands_sender: mpsc::Sender<Command>,
    thread: JoinHandle<()>,
    capturing: bool,
    playing: bool,
}

const PLUGIN_NAME: [u8; 23] = *b"Inferno virtual device\0";

lazy_static! {
    static ref global_instances: RwLock<BTreeMap<DeviceId, Arc<Mutex<InfernoInstance>>>> =
        RwLock::new(BTreeMap::new());
    static ref global_initialized: Mutex<bool> = false.into();
}

/// The transmitter of an Inferno instance (device server thread).
#[derive(PartialEq)]
enum Transmitter {
    None,
    /// sends silence
    Silent,
    /// serves the playback PCM
    Pcm,
}

static SILENCE: AtomicSample = AtomicSample::new(0);

/// Starts transmitting silence, or switches the running transmitter to it (keeping its flows).
async fn transmit_silence(device_server: &mut DeviceServer, channels: usize) -> Transmitter {
    // never valid and never started: the transmitter sends silence without touching the buffer
    let valid = Arc::new(RwLock::new(false));
    let anchor = StreamAnchor::new_paused_guarding(valid.clone());
    let buffers = (0..channels)
        .map(|_| unsafe { ExternalBufferParameters::new(&SILENCE, 1, 1, valid.clone(), None) })
        .collect();
    device_server
        .transmit_from_external_buffer(buffers, anchor, Default::default(), None)
        .await;
    debug!("transmitting silence");
    Transmitter::Silent
}

fn get_or_create_instance(settings: &Settings) -> Arc<Mutex<InfernoInstance>> {
    let mut instances_locked = global_instances.write().unwrap();
    let entry = instances_locked.entry(settings.self_info.factory_device_id);
    entry
        .or_insert_with(|| {
            //self_info.sample_rate = sample_rate;
            // TODO make tx & rx channels based on (*io).channels
            // this requires a complicated refactor to allow adding channels to the Dante network dynamically at any time, not just on DeviceServer start
            // because we don't know beforehand whether the app will be capture&playback, playback-only or capture-only
            // so we don't know whether we should wait for the second prepare call to gather all channels counts

            //assert_eq!(self_info.sample_rate, sample_rate);

            let (commands_sender, mut commands_rx) = mpsc::channel(16);

            let settings = settings.clone();
            let thread = run_future_in_new_thread("Inferno main", move || {
                async move {
                    // TODO DeviceServer::start may fail internally (e.g. if other process using Inferno is already listening on network ports)
                    // retrying infinitely will spam the log. (Audacity behaves this way if Inferno is already running)
                    let tx_channels = settings.self_info.tx_channels.len();
                    let mut device_server = DeviceServer::start(settings).await;
                    // While the device exists, it transmits: from the playback PCM, or silence (e.g. for
                    // capture-only applications), so that receivers don't see a broken stream.
                    let mut tx = Transmitter::None;
                    let mut receiving = false;
                    loop {
                        match commands_rx.recv().await {
                            Some(command) => match command {
                                Command::StartReceiver(StartArgs {
                                    channels,
                                    anchor,
                                    clock_rx_tx,
                                    current_timestamp,
                                    on_transfer,
                                }) => {
                                    let _ = clock_rx_tx
                                        .send(device_server.get_realtime_clock_receiver());
                                    device_server
                                        .receive_to_external_buffer(
                                            channels,
                                            anchor,
                                            current_timestamp,
                                            on_transfer,
                                        )
                                        .await;
                                    receiving = true;
                                    debug!("started receiver");
                                    if tx == Transmitter::None && tx_channels > 0 {
                                        tx =
                                            transmit_silence(&mut device_server, tx_channels).await;
                                    }
                                }
                                Command::StartTransmitter(StartArgs {
                                    channels,
                                    anchor,
                                    clock_rx_tx,
                                    current_timestamp,
                                    on_transfer,
                                }) => {
                                    let _ = clock_rx_tx
                                        .send(device_server.get_realtime_clock_receiver());
                                    // switches a running (silent) transmitter, keeping its flows
                                    tx = Transmitter::Pcm;
                                    device_server
                                        .transmit_from_external_buffer(
                                            channels,
                                            anchor,
                                            current_timestamp,
                                            on_transfer,
                                        )
                                        .await;
                                    debug!("started transmitter");
                                }
                                Command::StopReceiver => {
                                    device_server.stop_receiver().await;
                                    receiving = false;
                                    debug!("stopped receiver");
                                    if tx == Transmitter::Silent {
                                        device_server.stop_transmitter().await;
                                        tx = Transmitter::None;
                                        debug!("stopped transmitting silence");
                                    }
                                }
                                Command::StopTransmitter => {
                                    if receiving && tx_channels > 0 {
                                        // keep the flows for the still capturing application
                                        tx =
                                            transmit_silence(&mut device_server, tx_channels).await;
                                    } else {
                                        device_server.stop_transmitter().await;
                                        tx = Transmitter::None;
                                        debug!("stopped transmitter");
                                    }
                                }
                                Command::Shutdown => {
                                    break;
                                }
                            },
                            None => {
                                break;
                            }
                        }
                    }
                    device_server.shutdown().await;
                }
                .boxed_local()
            });

            Arc::new(Mutex::new(InfernoInstance {
                commands_sender,
                thread,
                capturing: false.into(),
                playing: false.into(),
            }))
        })
        .clone()
}

/// Position bookkeeping, shared with the transmitter thread (see `written_from`).
struct StreamInfo {
    /// ALSA's hw and application pointers wrap around at boundary
    boundary: AtomicUsize,
    /// added to our position (frames since start_time) to get ALSA's hw pointer
    boundary_add: AtomicIsize,
    /// `MyIOPlug::io.appl_ptr`, kept up to date by alsa-lib: ioplug's `pcm->appl.ptr` points to it
    appl_ptr: *const snd_pcm_uframes_t,
}

// safety: appl_ptr is only dereferenced while the PCM's buffers are valid (ExternalBuffer checks that),
// the PCM, and so `io`, lives longer
unsafe impl Send for StreamInfo {}
unsafe impl Sync for StreamInfo {}

impl StreamInfo {
    fn new(appl_ptr: *const snd_pcm_uframes_t) -> Arc<Self> {
        Arc::new(Self {
            boundary: AtomicUsize::new(0),
            boundary_add: AtomicIsize::new(0),
            appl_ptr,
        })
    }
    /// Number of frames starting at `position` (frames since start_time) the application has written.
    fn written_from(&self, position: usize) -> usize {
        let boundary = self.boundary.load(Ordering::Relaxed) as isize;
        // alsa-lib writes it non-atomically, but an aligned word is never torn
        let appl_ptr =
            unsafe { AtomicUsize::from_ptr(self.appl_ptr as *mut usize) }.load(Ordering::Relaxed);
        let hw_position = (position as isize)
            .wrapping_add(self.boundary_add.load(Ordering::Relaxed))
            .rem_euclid(boundary);
        // modulo boundary, so that either pointer may wrap around first
        let written = (appl_ptr as isize - hw_position).rem_euclid(boundary);
        if written > boundary / 2 {
            0 // the application is behind
        } else {
            written as usize
        }
    }
}

/// Identifies what a running receiver/transmitter was started with: the memory it is bound to,
/// (address, length, stride) per channel, and the period size (wakeup interval).
/// It only changes when the application changes hw_params.
#[derive(PartialEq)]
struct BuffersSignature {
    areas: Vec<(usize, usize, usize)>,
    period_size: snd_pcm_uframes_t,
}

#[repr(C)]
struct MyIOPlug {
    io: snd_pcm_ioplug_t,
    callbacks: snd_pcm_ioplug_callback_t,
    settings: Settings,
    //config: BTreeMap<String, String>,
    ref_time: Instant,
    use_flows_clock: bool,
    /// capture: per channel, how far the running receiver has written (frames since start_time)
    capture_written: Arc<Vec<AtomicUsize>>,
    stream_info: Arc<StreamInfo>,
    buffers_valid: Arc<RwLock<bool>>,
    media_clock: MediaClock,
    clock_receiver: Option<RealTimeClockReceiver>,
    start_time: Option<Clock>,
    /// shared with the receiver/transmitter serving this PCM
    anchor: StreamAnchor,
    /// buffers the running receiver/transmitter writes to/reads from, if any
    active_buffers: Option<BuffersSignature>,
    current_timestamp: Arc<AtomicUsize>,
    on_transfer_eventfd: libc::c_int,
    on_transfer_enabled: bool,
    last_transfer_buffer_offset: snd_pcm_uframes_t,
    transfer_offset_add: Wrapping<Clock>,
    // TODO refactor multiple Options to single Option<struct>
}

unsafe fn get_private<'a>(io: *mut snd_pcm_ioplug_t) -> &'a mut MyIOPlug {
    &mut *((*io).private_data as *mut MyIOPlug)
}

unsafe extern "C" fn plugin_pointer(io: *mut snd_pcm_ioplug_t) -> snd_pcm_sframes_t {
    let this = get_private(io);
    let cur = this
        .current_timestamp
        .load(Ordering::SeqCst /*TODO: really needed?*/);

    // Position the transmitter/receiver has actually processed up to, if it's running.
    // It is important to use actual input/output clock here because sample precision is required.
    // Otherwise app may overwrite not-yet-sent samples or read not-yet-received ones.
    let flows_ptr = match this.start_time {
        Some(start_time) if cur != usize::MAX => {
            Some(cur.wrapping_sub(start_time) as snd_pcm_sframes_t)
        }
        _ => None,
    };

    // TODO may be non-monotonic in edge cases (switching between clocks)
    // TODO rethink int sizes here
    let mut ptr: snd_pcm_sframes_t = if let (true, Some(flows_ptr)) =
        (this.use_flows_clock, flows_ptr)
    {
        flows_ptr
    } else {
        //println!("using own clock");
        // ... but fall back to system clock when not transmitting/receiving right now
        if let Some(clock_receiver) = &mut this.clock_receiver {
            if clock_receiver.update() {
                if let Some(overlay) = clock_receiver.get() {
                    this.media_clock.update_overlay(*overlay);
                }
            }
        }
        // here we must use long now_in_timebase (not wrapping) because boundary may be not power-of-2
        // TODO: optimize by setting plugin_pointer callback depending on whether boundary is power-of-2
        let now_samples_opt = this.media_clock.now_in_timebase((*io).rate as u64);
        if now_samples_opt.is_some() && this.start_time.is_none() {
            /*warn!("warning: setting start_time in plugin_pointer, not plugin_start");
            this.start_time = Some(now_samples_opt.unwrap() as usize);
            if let Some(start_time_tx) = this.start_time_tx.take() {
                if let Err(e) = start_time_tx.send(now_samples_opt.unwrap()) {
                    error!("failed to send start timestamp: {e}. tx/rx will not work.");
                }
            } else {
                error!("failed to send start timestamp: start_time_tx already used, BUG");
            }*/
            warn!("looks like app is calling plugin_pointer before clock is valid, returning 0");
        }
        if now_samples_opt.is_some() && this.start_time.is_some() {
            (now_samples_opt.unwrap() as usize).wrapping_sub(this.start_time.unwrap())
                as snd_pcm_sframes_t
        } else {
            //log::debug!("clock not ready... {} {}", now_samples_opt.is_some(), this.start_time.is_some());
            0
        }
        //now_samples_opt.map(|now_samples| now_samples.wrapping_sub(this.start_time.unwrap())).unwrap_or(0) as i64
    };
    if let (SND_PCM_STREAM_PLAYBACK, Some(flows_ptr)) = ((*io).stream, flows_ptr) {
        // The transmitter reads a packet once its first sample is due, so when its thread wakes up
        // late, the media clock is already past samples it hasn't read. Never report those as
        // consumed, or an application keeping its buffer full overwrites them.
        if flows_ptr.wrapping_sub(ptr) < 0 {
            ptr = flows_ptr;
        }
    }

    let stream_info = &*this.stream_info;
    let boundary = stream_info.boundary.load(Ordering::Relaxed) as snd_pcm_sframes_t;
    let max_diff = boundary >> 2;
    let boundary_add = stream_info.boundary_add.load(Ordering::Relaxed) as snd_pcm_sframes_t;
    let appl_ptr = ((*io).appl_ptr as snd_pcm_sframes_t).wrapping_add(boundary_add);
    let mut diff = ptr.wrapping_sub(appl_ptr);
    // handle situation when our "hardware" pointer wraps around boundary but the application pointer not yet (or vice versa):
    if diff.saturating_abs() > max_diff {
        let d = diff.wrapping_add(boundary);
        if d.saturating_abs() <= max_diff {
            diff = d;
        } else {
            let d = diff.wrapping_sub(boundary);
            if d.saturating_abs() <= max_diff {
                diff = d;
            } else {
                error!("very large hw-appl ptr diff: {diff} = {ptr} - {appl_ptr}, boundary_add {boundary_add} ({boundary}). reporting xrun because something is clearly wrong");
                return (-EPIPE).try_into().unwrap(); // report xrun
            }
        }
    }

    let (dir, buffered) = match (*io).stream {
        SND_PCM_STREAM_CAPTURE => ("capture", diff),
        SND_PCM_STREAM_PLAYBACK => ("playback", (0 as snd_pcm_sframes_t).wrapping_sub(diff)),
        _ => ("???", 0),
    };

    if (*io).stream == SND_PCM_STREAM_CAPTURE && (*io).state == SND_PCM_STATE_RUNNING {
        // The receiver writes ahead of our pointer (by up to the network latency): if the application
        // doesn't read in time, samples it hasn't read yet get overwritten.
        let buffer_size = (*io).buffer_size as snd_pcm_sframes_t;
        let appl = ptr.wrapping_sub(buffered) as usize;
        let written_ahead = this
            .capture_written
            .iter()
            .map(|w| w.load(Ordering::Acquire).wrapping_sub(appl) as snd_pcm_sframes_t)
            // ignore channels without a writer and positions from before a restart
            .filter(|&w| w >= buffered && w <= buffered + 2 * buffer_size)
            .max()
            .unwrap_or(0);
        if buffered > buffer_size || written_ahead > buffer_size {
            warn!(
                "XRUN: capture overrun, buffered {buffered}, written {written_ahead} > buffer size {buffer_size}"
            );
            return (-EPIPE).try_into().unwrap();
        }
    }

    if buffered < 0
        && (*io).stream == SND_PCM_STREAM_PLAYBACK
        && (*io).state == SND_PCM_STATE_DRAINING
    {
        // Everything written has been played: the drain is complete, not an xrun. Reporting exactly the end
        // of the written data lets ioplug stop the stream.
        ptr = ptr.wrapping_add(buffered);
    } else if buffered < 0
        && ((*io).state == SND_PCM_STATE_RUNNING || (*io).state == SND_PCM_STATE_DRAINING)
    {
        if let Ok(hw_ptr) = ptr.try_into() {
            let ioplug_avail = snd_pcm_ioplug_avail(io, hw_ptr, (*io).appl_ptr);
            let ioplug_hw_avail = snd_pcm_ioplug_hw_avail(io, hw_ptr, (*io).appl_ptr);
            warn!("XRUN: buffered for {dir}: {buffered} samples, avail {ioplug_avail}, hw_avail {ioplug_hw_avail}, hw_ptr {hw_ptr}, appl_ptr {}", (*io).appl_ptr);
        } else {
            warn!("severe clock discontinuity");
            return (-EPIPE).try_into().unwrap(); // report xrun
        }

        // workaround for pipewire: grace period during startup:
        if ptr > 100000 || boundary_add != 0 {
            return (-EPIPE).try_into().unwrap(); // report xrun
                                                 // TODO check for xruns in ExternalRingBuffer because this function may be called too seldom
        }
    }

    ptr = ptr.wrapping_add(boundary_add);
    if ptr >= boundary {
        stream_info
            .boundary_add
            .fetch_sub(boundary as isize, Ordering::Relaxed);
        ptr -= boundary;
    }
    //log::debug!("pointer: {ptr}");

    ptr
}

fn get_app_name() -> Option<String> {
    Some(
        std::env::current_exe()
            .ok()?
            .file_name()?
            .to_string_lossy()
            .to_string(),
    )
}

fn is_running(common: &InfernoInstance, stream: snd_pcm_stream_t) -> bool {
    match stream {
        SND_PCM_STREAM_CAPTURE => common.capturing,
        SND_PCM_STREAM_PLAYBACK => common.playing,
        _ => false,
    }
}

/// Stops the receiver or transmitter serving `stream`, if running. Returns false on failure.
fn stop_stream(common: &mut InfernoInstance, stream: snd_pcm_stream_t) -> bool {
    // TODO blocking_send inside mutex, risk of deadlock?
    let (running, command) = match stream {
        SND_PCM_STREAM_CAPTURE => (&mut common.capturing, Command::StopReceiver),
        SND_PCM_STREAM_PLAYBACK => (&mut common.playing, Command::StopTransmitter),
        _ => return false,
    };
    if !*running {
        return true;
    }
    *running = false;
    common.commands_sender.blocking_send(command).is_ok()
}

/// Refuses capture buffers which can't hold the network latency plus a period: the receiver writes one latency
/// ahead of the hardware pointer, so it would overwrite samples before the application reads them.
unsafe extern "C" fn plugin_hw_params(
    io: *mut snd_pcm_ioplug_t,
    _params: *mut snd_pcm_hw_params_t,
) -> c_int {
    let this = get_private(io);
    if (*io).stream == SND_PCM_STREAM_CAPTURE {
        let latency = this.settings.self_info.latency_samples();
        let (buffer, period) = ((*io).buffer_size as usize, (*io).period_size as usize);
        if buffer < latency + period {
            error!(
                "capture buffer of {buffer} frames is too small for RX latency of {latency} frames + period of \
                 {period} frames: use a larger buffer or more periods, or decrease RX_LATENCY_NS"
            );
            return -libc::EINVAL;
        }
    }
    0
}

unsafe extern "C" fn plugin_prepare(io: *mut snd_pcm_ioplug_t) -> c_int {
    debug!("plugin_prepare called");

    let this = get_private(io);

    let channels_areas = snd_pcm_ioplug_mmap_areas(io);
    if channels_areas.is_null() {
        error!("snd_pcm_ioplug_mmap_areas returned null, unable to get audio memory addresses");
        return -libc::EINVAL;
    }

    let bits_per_sample = (8 * size_of::<Sample>()) as u32;
    let channels_areas = std::slice::from_raw_parts(channels_areas, (*io).channels as usize);
    if channels_areas.len() > 0 {
        let area = &channels_areas[0];
        debug!("got buffer size {} samples * {} channels, first channel: address {:x} with first {}b, step {}b", (*io).buffer_size, channels_areas.len(), area.addr as usize, area.first, area.step);
    }
    for area in channels_areas {
        if (area.first % 8) != 0 || (area.step % 8) != 0 {
            error!("sample size is not measured in whole bytes, unsupported");
            return -libc::EINVAL;
        }
        if (area.first % bits_per_sample) != 0 || (area.step % bits_per_sample) != 0 {
            error!("samples not aligned, unsupported");
            return -libc::EINVAL;
        }
    }
    debug!("period size: {}", (*io).period_size);

    let buffers_signature = BuffersSignature {
        areas: channels_areas
            .iter()
            .map(|area| {
                (
                    area.addr as usize + (area.first / 8) as usize,
                    (*io).buffer_size as usize,
                    area.step as usize,
                )
            })
            .collect(),
        period_size: (*io).period_size,
    };

    // a running receiver keeps reporting to the table it was started with
    let capture_written = if this.capture_written.len() == channels_areas.len() {
        this.capture_written.clone()
    } else {
        Arc::new(channels_areas.iter().map(|_| AtomicUsize::new(0)).collect())
    };
    let channels_buffers = channels_areas
        .iter()
        .enumerate()
        .map(|(ch_index, area)| {
            let params = ExternalBufferParameters::<Sample>::new(
                area.addr.byte_offset((area.first / 8) as isize) as *const AtomicSample,
                ((*io).buffer_size as usize) * channels_areas.len()
                    - ((area.first / bits_per_sample) as usize),
                (area.step / bits_per_sample) as usize,
                this.buffers_valid.clone(),
                None,
            );
            if (*io).stream == SND_PCM_STREAM_PLAYBACK {
                // send silence, not stale buffer contents, when the application doesn't keep up or stops
                let stream_info = this.stream_info.clone();
                params
                    .with_written_from(Arc::new(move |position| stream_info.written_from(position)))
            } else {
                params.with_written_pos_report(PositionReportDestination::new(
                    capture_written.clone(),
                    ch_index,
                ))
            }
        })
        .collect();

    let mut swparams = std::ptr::null_mut::<snd_pcm_sw_params_t>();
    let r = snd_pcm_sw_params_malloc(&mut swparams);
    if r != 0 {
        error!("snd_pcm_sw_params_malloc failed");
        return r;
    }
    let r = snd_pcm_sw_params_current((*io).pcm, swparams);
    let boundary: snd_pcm_uframes_t = if r == 0 {
        let mut value = 0;
        snd_pcm_sw_params_get_boundary(swparams, &mut value);
        value
    } else {
        error!("snd_pcm_sw_params_current failed");
        return r;
    };
    snd_pcm_sw_params_free(swparams);
    assert!(boundary != 0);
    debug!("boundary: {boundary}");
    this.stream_info
        .boundary
        .store(boundary as usize, Ordering::Relaxed);
    this.stream_info.boundary_add.store(0, Ordering::Relaxed);
    this.start_time = None;
    // Pausing marks the buffer invalid, waiting for any access in progress: a receiver thread which hasn't
    // noticed the pause yet mustn't write to it, we're going to clear it, and the application may even
    // free it (hw_params is allowed in the PREPARED state). It becomes valid again once the
    // receiver/transmitter has applied the start time set in plugin_start.
    this.anchor.pause();

    let inferno_instance = get_or_create_instance(&this.settings);
    let running = is_running(&inferno_instance.lock().unwrap(), (*io).stream);
    if running && this.active_buffers.as_ref() == Some(&buffers_signature) {
        // Restarting the PCM (transport stop/start, xrun recovery): keep the receiver/transmitter,
        // its flows and subscriptions running, the stream will be re-anchored in plugin_start.
        debug!("re-preparing running stream");
    } else {
        let r = start_stream(io, this, &inferno_instance, channels_buffers);
        if r != 0 {
            return r;
        }
        this.active_buffers = Some(buffers_signature);
    }
    // the paused receiver doesn't write; positions of the next run start at 0
    for w in capture_written.iter() {
        w.store(0, Ordering::Release);
    }
    this.capture_written = capture_written;

    if (*io).stream == SND_PCM_STREAM_CAPTURE {
        // don't let the application read leftovers from previous runs or uninitialised memory
        snd_pcm_areas_silence(
            channels_areas.as_ptr(),
            0,
            (*io).channels,
            (*io).buffer_size,
            (*io).format,
        );
    }

    0
}

/// (Re)starts the receiver or transmitter serving this PCM, bound to the given buffers.
unsafe fn start_stream(
    io: *mut snd_pcm_ioplug_t,
    this: &mut MyIOPlug,
    inferno_instance: &Mutex<InfernoInstance>,
    channels_buffers: Vec<ExternalBufferParameters<Sample>>,
) -> c_int {
    let (clock_rx_tx, clock_rx_rx) = oneshot::channel::<RealTimeClockReceiver>();
    this.anchor = StreamAnchor::new_paused_guarding(this.buffers_valid.clone());
    this.active_buffers = None;
    {
        let mut common = inferno_instance.lock().unwrap();
        this.current_timestamp.store(usize::MAX, Ordering::SeqCst);
        let args = StartArgs {
            channels: channels_buffers,
            anchor: this.anchor.clone(),
            clock_rx_tx,
            // playback always needs the transmitter's read position, see plugin_pointer
            current_timestamp: if this.use_flows_clock || (*io).stream == SND_PCM_STREAM_PLAYBACK {
                this.current_timestamp.clone()
            } else {
                Default::default()
            },
            on_transfer: if this.on_transfer_enabled {
                let efd = this.on_transfer_eventfd;
                Some(TransferNotifier {
                    callback: Box::new(move || {
                        libc::write(efd, [1u64].as_ptr() as *const c_void, 8);
                    }),
                    max_interval_samples: (*io).period_size.try_into().unwrap(),
                })
            } else {
                None
            },
        };
        // A running receiver/transmitter is switched to the new buffers, keeping its flows and
        // subscriptions (see the device server thread).
        let mut err = false;
        match (*io).stream {
            SND_PCM_STREAM_CAPTURE => {
                err |= common
                    .commands_sender
                    .blocking_send(Command::StartReceiver(args))
                    .is_err();
                common.capturing = !err;
            }
            SND_PCM_STREAM_PLAYBACK => {
                err |= common
                    .commands_sender
                    .blocking_send(Command::StartTransmitter(args))
                    .is_err();
                common.playing = !err;
            }
            _ => {
                error!("unknown stream direction");
                return -libc::EINVAL;
            }
        }
        if err {
            error!("BUG: error sending to Inferno thread");
            return -libc::EPIPE;
        }
    }

    this.clock_receiver = match clock_rx_rx.blocking_recv() {
        Ok(clk) => Some(clk),
        Err(_) => {
            error!("no clock available (couldn't receive clock receiver)");
            return -libc::EINVAL;
        }
    };
    if let Some(clock_receiver) = &mut this.clock_receiver {
        let mut ctr = 0;
        loop {
            clock_receiver.update();
            if let Some(overlay) = clock_receiver.get() {
                this.media_clock.update_overlay(*overlay);
            }
            if this.media_clock.is_ready() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
            ctr += 1;
            if ctr >= 20 {
                error!("no clock available (timeout waiting for overlay update)");
                return -libc::ETIMEDOUT;
            }
        }
    }

    0
}

unsafe extern "C" fn plugin_start(io: *mut snd_pcm_ioplug_t) -> c_int {
    let appl_ptr = (*io).appl_ptr as snd_pcm_sframes_t;
    debug!("plugin_start called with appl_ptr: {appl_ptr}");
    let this = get_private(io);

    if let Some(clock_receiver) = &mut this.clock_receiver {
        clock_receiver.update();
        debug!("clock_receiver updated");
        if let Some(overlay) = clock_receiver.get() {
            this.media_clock.update_overlay(*overlay);
            debug!("media_clock overlay updated");
        } else {
            warn!("no overlay for media_clock");
        }
    }
    // start_time is used in:
    // * flows_rx and flows_tx (via anchor) for calculating ringbuffer positions
    // * plugin_pointer for calculating pointer
    // all these calculations are wrapping so we can use wrapping clock
    let now_samples_opt = this.media_clock.wrapping_now_in_timebase((*io).rate as u64);
    if now_samples_opt.is_some() && this.start_time.is_none() {
        let start_time = now_samples_opt.unwrap() as usize;
        this.start_time = Some(start_time);
        // the receiver/transmitter marks the buffer valid once it addresses it according to start_time
        this.anchor.start(start_time);
    } else {
        warn!(
            "can't set start_time in plugin_start: now_samples_opt: {:?}, start_time: {:?}",
            now_samples_opt, this.start_time
        );
    }
    if (*io).stream == SND_PCM_STREAM_PLAYBACK && this.on_transfer_enabled {
        libc::write(
            this.on_transfer_eventfd,
            [1u64].as_ptr() as *const c_void,
            8,
        );
    }
    0
}

unsafe extern "C" fn plugin_stop(io: *mut snd_pcm_ioplug_t) -> c_int {
    debug!("plugin_stop called");

    let this = get_private(io);
    // Only pause: the receiver/transmitter, its flows and subscriptions keep running,
    // so that the PCM can be restarted without a break in Dante connections.
    // While paused, the buffer is invalid: the receiver doesn't touch it and the transmitter sends silence.
    this.anchor.pause();

    0
}

/// Like ioplug's default (frames in the buffer), plus the network latency,
/// so that applications can compensate for it.
unsafe extern "C" fn plugin_delay(
    io: *mut snd_pcm_ioplug_t,
    delayp: *mut snd_pcm_sframes_t,
) -> c_int {
    let hw_ptr = plugin_pointer(io);
    if hw_ptr < 0 {
        return hw_ptr as c_int;
    }
    let buffered = match (*io).stream {
        SND_PCM_STREAM_CAPTURE => snd_pcm_ioplug_avail(io, hw_ptr as _, (*io).appl_ptr),
        _ => snd_pcm_ioplug_hw_avail(io, hw_ptr as _, (*io).appl_ptr),
    };
    let this = get_private(io);
    *delayp =
        buffered as snd_pcm_sframes_t + this.anchor.network_latency_samples() as snd_pcm_sframes_t;
    0
}

unsafe extern "C" fn plugin_demangle_revents(
    io: *mut snd_pcm_ioplug_t,
    pfd: *mut libc::pollfd,
    nfds: ::std::os::raw::c_uint,
    revents: *mut ::std::os::raw::c_ushort,
) -> ::std::os::raw::c_int {
    if pfd.is_null() || nfds < 1 || revents.is_null() {
        return -libc::EINVAL;
    }
    let got_events = (*pfd).revents;
    if (*pfd).fd == (*io).poll_fd {
        let mut out_events = got_events & !(POLLIN | POLLOUT);
        if (got_events & POLLIN) != 0 {
            out_events |= match (*io).stream {
                SND_PCM_STREAM_PLAYBACK => POLLOUT,
                SND_PCM_STREAM_CAPTURE => POLLIN,
                _ => return -libc::EINVAL,
            };
            let mut blackhole: [u64; 1] = [0];
            libc::read((*pfd).fd, blackhole.as_mut_ptr() as *mut c_void, 8);
        }
        *revents = out_events as _;
    } else {
        *revents = got_events as _;
    }
    0
}

unsafe extern "C" fn plugin_transfer(
    io: *mut snd_pcm_ioplug_t,
    areas: *const snd_pcm_channel_area_t,
    offset: snd_pcm_uframes_t,
    size: snd_pcm_uframes_t,
) -> snd_pcm_sframes_t {
    let this = get_private(io);
    let mut blackhole: [u64; 1] = [0];
    libc::read(
        this.on_transfer_eventfd,
        blackhole.as_mut_ptr() as *mut c_void,
        8,
    );
    size as snd_pcm_sframes_t
}

unsafe extern "C" fn plugin_close(io: *mut snd_pcm_ioplug_t) -> c_int {
    debug!("plugin_close called");
    let this = get_private(io);
    this.anchor.pause();
    {
        let mut instances_locked = global_instances.write().unwrap();
        // TODO blocking_send inside mutex, risk of deadlock?
        if let Some(common_mutex) = instances_locked
            .get(&this.settings.self_info.factory_device_id)
            .map(|a| a.clone())
        {
            let mut common = common_mutex.lock().unwrap();
            if this.active_buffers.take().is_some() && !stop_stream(&mut common, (*io).stream) {
                log::error!("BUG: error sending to Inferno thread");
            }
            if (!common.capturing) && (!common.playing) {
                if let Err(e) = common.commands_sender.blocking_send(Command::Shutdown) {
                    log::error!("BUG: send shutdown via channel failed: {e:?}");
                }
                instances_locked.remove(&this.settings.self_info.factory_device_id);
            }
        }
    }
    libc::close(this.on_transfer_eventfd); // TODO: shouldn't this be in a different place?

    0
}

unsafe extern "C" fn plugin_define(
    pcmp: *mut *mut snd_pcm_t,
    name: *const c_char,
    root: *const snd_config_t,
    conf: *const snd_config_t,
    stream: snd_pcm_stream_t,
    mode: c_int,
) -> c_int {
    {
        let mut locked_flag = global_initialized.lock().unwrap();
        if !*locked_flag {
            let logenv = env_logger::Env::default().default_filter_or("debug");
            env_logger::builder()
                .parse_env(logenv)
                .format_timestamp_micros()
                .init();
            *locked_flag = true;
        }
    }

    let mut config = BTreeMap::<String, String>::new();
    let mut pos = snd_config_iterator_first(conf);
    while pos != snd_config_iterator_end(conf) {
        let entry = snd_config_iterator_entry(pos);
        let mut key_container: [*const c_char; 1] = [core::ptr::null()];
        let r1 = snd_config_get_id(entry, key_container.as_mut_ptr());
        let mut value_container: [*mut c_char; 1] = [core::ptr::null_mut()];
        let r2 = snd_config_get_ascii(entry, value_container.as_mut_ptr());
        if r1 == 0 && r2 == 0 && (!key_container[0].is_null()) && (!value_container[0].is_null()) {
            let key = CStr::from_ptr(key_container[0]).to_str();
            let value = CStr::from_ptr(value_container[0]).to_str();
            if key.is_ok() && value.is_ok() {
                config.insert(key.unwrap().to_owned(), value.unwrap().to_owned());
            }
        }
        pos = snd_config_iterator_next(pos);
    }

    let efd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);

    let callbacks = snd_pcm_ioplug_callback_t {
        prepare: Some(plugin_prepare),
        start: Some(plugin_start),
        stop: Some(plugin_stop),
        pointer: Some(plugin_pointer),
        close: Some(plugin_close),
        poll_revents: Some(plugin_demangle_revents),
        delay: Some(plugin_delay),
        hw_params: Some(plugin_hw_params),
        //transfer: Some(plugin_transfer),
        ..zeroed()
    };

    let app_name = get_app_name().unwrap_or(format!("process {}", std::process::id().to_string()));
    let settings = Settings::new(&app_name, &app_name, None, &config);

    let disable_pollfd = config
        .get("DISABLE_POLLFD")
        .map(|v| v == "1")
        .unwrap_or(false);

    let use_flows_clock = config
        .get("USE_FLOWS_CLOCK")
        .map(|v| v == "1")
        .unwrap_or(false);

    let buffers_valid = Arc::new(RwLock::new(false));
    let myio = Box::into_raw(Box::new(MyIOPlug {
        io: zeroed(),
        callbacks,
        settings,
        ref_time: Instant::now(),
        use_flows_clock,
        capture_written: Arc::new(vec![]),
        stream_info: StreamInfo::new(std::ptr::null()), // set below, when io's address is known
        anchor: StreamAnchor::new_paused_guarding(buffers_valid.clone()),
        buffers_valid,
        media_clock: MediaClock::new(false /* TODO */),
        clock_receiver: None,
        start_time: None,
        active_buffers: None,
        current_timestamp: Arc::new(AtomicUsize::new(usize::MAX)),
        on_transfer_eventfd: efd,
        on_transfer_enabled: !disable_pollfd,
        last_transfer_buffer_offset: 0,
        transfer_offset_add: Wrapping(0),
    }));

    (*myio).stream_info = StreamInfo::new(&(*myio).io.appl_ptr);

    let io = &mut (*myio).io;
    io.version = (1 << 16) | (0 << 8) | 2;
    io.name = PLUGIN_NAME.as_ptr() as *const _;
    io.callback = &(*myio).callbacks;
    io.flags = SND_PCM_IOPLUG_FLAG_BOUNDARY_WA;
    io.mmap_rw = 1;

    // despite ALSA PCM plugin documentation saying that poll_fd is optional,
    // SoX actually requires it, misbehaving if not notified about transfers
    if !disable_pollfd {
        io.poll_events = POLLIN as u32;
        io.poll_fd = efd;
    }

    io.private_data = myio as *mut _;

    let self_info = &(*myio).settings.self_info;

    let r = snd_pcm_ioplug_create(io, name, stream, mode);
    if r < 0 {
        error!("snd_pcm_ioplug_create returned {r}");
        return r;
    }

    let r = snd_pcm_ioplug_set_param_list(
        io,
        SND_PCM_IOPLUG_HW_FORMAT as i32,
        1,
        [SND_PCM_FORMAT_S32 as u32].as_ptr(),
    );
    if r < 0 {
        error!("snd_pcm_ioplug_set_param_list SND_PCM_IOPLUG_HW_FORMAT returned {r}");
        return r;
    }

    let r = snd_pcm_ioplug_set_param_list(
        io,
        SND_PCM_IOPLUG_HW_ACCESS as i32,
        2,
        [
            SND_PCM_ACCESS_MMAP_INTERLEAVED as u32,
            SND_PCM_ACCESS_RW_INTERLEAVED as u32,
        ]
        .as_ptr(),
    ); // FIXME investigate why planar doesn't work
    if r < 0 {
        error!("snd_pcm_ioplug_set_param_list SND_PCM_IOPLUG_HW_ACCESS returned {r}");
        return r;
    }

    let r = snd_pcm_ioplug_set_param_list(
        io,
        SND_PCM_IOPLUG_HW_RATE as i32,
        1,
        [self_info.sample_rate].as_ptr(),
    );
    if r < 0 {
        error!("snd_pcm_ioplug_set_param_list SND_PCM_IOPLUG_HW_RATE returned {r}");
        return r;
    }

    let num_channels = match (*io).stream {
        SND_PCM_STREAM_CAPTURE => self_info.rx_channels.len(),
        SND_PCM_STREAM_PLAYBACK => self_info.tx_channels.len(),
        _ => {
            error!("no stream specified, cannot continue");
            return -libc::EINVAL;
        }
    } as u32;

    let r = snd_pcm_ioplug_set_param_list(
        io,
        SND_PCM_IOPLUG_HW_CHANNELS as i32,
        1,
        [num_channels].as_ptr(),
    );
    if r < 0 {
        error!("snd_pcm_ioplug_set_param_list SND_PCM_IOPLUG_HW_CHANNELS returned {r}");
        return r;
    }

    let powers_of_2 = |first, max| {
        core::iter::successors(Some(first), move |n| {
            let r = n * 2;
            if r > max {
                None
            } else {
                Some(r)
            }
        })
    };

    let min_periods = 2;
    let min_samples = 16;
    let min_samples_whole_buffer = 1024; // must be power of 2 and > (max receive latency + ALSA period)
    let max_samples = 65536;
    let bytes_per_sample = size_of::<Sample>() as u32;

    let periods_bytes: Vec<std::os::raw::c_uint> =
        powers_of_2(min_samples, max_samples / min_periods)
            .map(|n| (num_channels * bytes_per_sample * n) as std::os::raw::c_uint)
            .collect();
    debug!("HW_PERIOD_BYTES: {periods_bytes:?}");
    let r = snd_pcm_ioplug_set_param_list(
        io,
        SND_PCM_IOPLUG_HW_PERIOD_BYTES as i32,
        periods_bytes.len() as std::os::raw::c_uint,
        periods_bytes.as_ptr(),
    );
    if r < 0 {
        error!("snd_pcm_ioplug_set_param_minmax SND_PCM_IOPLUG_HW_PERIOD_BYTES returned {r}");
        return r;
    }

    let buffer_sizes: Vec<std::os::raw::c_uint> =
        powers_of_2(min_samples_whole_buffer, max_samples)
            .map(|n| (num_channels * bytes_per_sample * n) as std::os::raw::c_uint)
            .collect();
    debug!("HW_BUFFER_BYTES: {buffer_sizes:?}");
    let r = snd_pcm_ioplug_set_param_list(
        io,
        SND_PCM_IOPLUG_HW_BUFFER_BYTES as i32,
        buffer_sizes.len() as std::os::raw::c_uint,
        buffer_sizes.as_ptr(),
    );
    if r < 0 {
        error!("snd_pcm_ioplug_set_param_minmax SND_PCM_IOPLUG_HW_BUFFER_BYTES returned {r}");
        return r;
    }

    let periods_nums: Vec<std::os::raw::c_uint> =
        powers_of_2(min_periods, buffer_sizes.last().unwrap() / min_samples).collect();
    debug!("HW_PERIODS: {periods_nums:?}");
    let r = snd_pcm_ioplug_set_param_list(
        io,
        SND_PCM_IOPLUG_HW_PERIODS as i32,
        periods_nums.len() as std::os::raw::c_uint,
        periods_nums.as_ptr(),
    );
    if r < 0 {
        error!("snd_pcm_ioplug_set_param_minmax SND_PCM_IOPLUG_HW_PERIODS returned {r}");
        return r;
    }

    *pcmp = (*myio).io.pcm;

    debug!("plugin_define end");
    0
}

#[no_mangle]
pub extern "C" fn _snd_pcm_inferno_open(
    pcmp: *mut *mut snd_pcm_t,
    name: *const c_char,
    root: *const snd_config_t,
    conf: *const snd_config_t,
    stream: snd_pcm_stream_t,
    mode: c_int,
) -> c_int {
    unsafe { plugin_define(pcmp, name, root, conf, stream, mode) }
}

#[no_mangle]
pub extern "C" fn __snd_pcm_inferno_open_dlsym_pcm_001() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_info(appl_ptr: &snd_pcm_uframes_t, boundary: usize) -> Arc<StreamInfo> {
        let info = StreamInfo::new(appl_ptr);
        info.boundary.store(boundary, Ordering::Relaxed);
        info
    }

    #[test]
    fn written_from_counts_frames_up_to_the_application_pointer() {
        let appl: snd_pcm_uframes_t = 3072;
        let info = stream_info(&appl, 1 << 30);
        assert_eq!(info.written_from(0), 3072);
        assert_eq!(info.written_from(3000), 72);
        assert_eq!(info.written_from(3072), 0);
        assert_eq!(info.written_from(4000), 0, "beyond the application pointer");
    }

    #[test]
    fn written_from_across_the_boundary() {
        // the application pointer wrapped around the boundary, our hw position not yet (boundary_add = 0)
        let boundary = 1 << 20;
        let appl: snd_pcm_uframes_t = 100;
        let info = stream_info(&appl, boundary);
        assert_eq!(info.written_from(boundary - 50), 150);
    }
}
