extern crate alsa_sys_all;
extern crate libc;
use alsa_sys_all::*;

use inferno_aoip::device_server::{
    AtomicSample, Clock, ExternalBufferParameters, PositionReportDestination, Sample, Settings,
    TransferNotifier,
};
use inferno_driver_core::{BuffersSignature, DeviceError, Direction, Stream, StreamOptions};
use lazy_static::lazy_static;
use libc::{c_char, c_int, c_void, eventfd, EFD_CLOEXEC, EFD_NONBLOCK, EPIPE, POLLIN, POLLOUT};
use log::{debug, error, trace, warn};
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::mem::zeroed;
use std::num::Wrapping;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const PLUGIN_NAME: [u8; 23] = *b"Inferno virtual device\0";

/// Keys of the PCM's ALSA config (upper case) that are not `Settings` but plugin options or ALSA's own.
const PLUGIN_KEYS: &[&str] = &[
    "TYPE",
    "HINT",
    "COMMENT",
    "DISABLE_POLLFD",
    "USE_FLOWS_CLOCK",
];

lazy_static! {
    static ref global_initialized: Mutex<bool> = false.into();
}

fn direction_of(stream: snd_pcm_stream_t) -> Option<Direction> {
    match stream {
        SND_PCM_STREAM_CAPTURE => Some(Direction::Capture),
        SND_PCM_STREAM_PLAYBACK => Some(Direction::Playback),
        _ => None,
    }
}

/// The error code to return to alsa-lib (already logged).
fn error_code(e: &DeviceError) -> c_int {
    match e {
        DeviceError::StartFailed(_) => -libc::EBUSY,
        DeviceError::ThreadEnded => -libc::EPIPE,
        DeviceError::NoClock(_) => -libc::ETIMEDOUT,
    }
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

#[repr(C)]
struct MyIOPlug {
    io: snd_pcm_ioplug_t,
    callbacks: snd_pcm_ioplug_callback_t,
    /// the device, its receiver/transmitter and the lifecycle of this PCM
    stream: Stream,
    ref_time: Instant,
    use_flows_clock: bool,
    /// capture: per channel, how far the running receiver has written (frames since start_time)
    capture_written: Arc<Vec<AtomicUsize>>,
    stream_info: Arc<StreamInfo>,
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

    // Position the transmitter/receiver has actually processed up to, if it's running.
    // It is important to use actual input/output clock here because sample precision is required.
    // Otherwise app may overwrite not-yet-sent samples or read not-yet-received ones.
    let flows_ptr = this.stream.flows_position().map(|p| p as snd_pcm_sframes_t);

    // TODO may be non-monotonic in edge cases (switching between clocks)
    // TODO rethink int sizes here
    let mut ptr: snd_pcm_sframes_t =
        if let (true, Some(flows_ptr)) = (this.use_flows_clock, flows_ptr) {
            flows_ptr
        } else {
            // ... but fall back to the media clock when not transmitting/receiving right now
            // TODO: optimize when boundary is power-of-2 (the position needn't be computed from the long clock)
            match this.stream.clock_position((*io).rate as u64) {
                Some(position) => position as snd_pcm_sframes_t,
                None => {
                    // the stream is prepared but not started yet (e.g. the application fills the buffer first)
                    trace!("plugin_pointer before the stream started, returning 0");
                    0
                }
            }
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

/// Refuses capture buffers which can't hold the network latency plus a period: the receiver writes one latency
/// ahead of the hardware pointer, so it would overwrite samples before the application reads them.
unsafe extern "C" fn plugin_hw_params(
    io: *mut snd_pcm_ioplug_t,
    _params: *mut snd_pcm_hw_params_t,
) -> c_int {
    let this = get_private(io);
    if (*io).stream == SND_PCM_STREAM_CAPTURE {
        let latency = this.stream.settings().self_info.latency_samples();
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

    let signature = BuffersSignature {
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
        period_size: (*io).period_size as usize,
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
                this.stream.buffers_valid(),
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

    let options = StreamOptions {
        // playback always needs the transmitter's read position, see plugin_pointer
        track_position: this.use_flows_clock || (*io).stream == SND_PCM_STREAM_PLAYBACK,
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
    // pauses the stream; keeps a receiver/transmitter already serving these buffers (PCM restart)
    if let Err(e) = this.stream.prepare(signature, channels_buffers, options) {
        return error_code(&e);
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

unsafe extern "C" fn plugin_start(io: *mut snd_pcm_ioplug_t) -> c_int {
    let appl_ptr = (*io).appl_ptr as snd_pcm_sframes_t;
    debug!("plugin_start called with appl_ptr: {appl_ptr}");
    let this = get_private(io);
    // the receiver/transmitter marks the buffer valid once it addresses it according to the start time
    this.stream.start((*io).rate as u64);
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

    // Only pauses: the receiver/transmitter, its flows and subscriptions keep running,
    // so that the PCM can be restarted without a break in Dante connections.
    get_private(io).stream.stop();

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
        buffered as snd_pcm_sframes_t + this.stream.network_latency_samples() as snd_pcm_sframes_t;
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
    // stops the receiver/transmitter, and the device when no other PCM uses it
    this.stream.close();
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
            // runs inside the application (e.g. PipeWire): only what the user should see, unless RUST_LOG
            let logenv = env_logger::Env::default().default_filter_or("info");
            let _ = env_logger::builder()
                .parse_env(logenv)
                .format_timestamp_micros()
                .try_init();
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

    let config = Settings::merged_config(&config);
    let unknown = Settings::unknown_keys(&config, PLUGIN_KEYS);
    if !unknown.is_empty() {
        warn!(
            "ignoring unknown settings (misspelt?): {}",
            unknown.join(", ")
        );
    }
    let app_name = get_app_name().unwrap_or(format!("process {}", std::process::id().to_string()));
    let settings = match Settings::try_new(&app_name, &app_name, None, &config) {
        Ok(settings) => settings,
        Err(e) => {
            error!("{e}");
            return -libc::EINVAL;
        }
    };
    let Some(direction) = direction_of(stream) else {
        error!("no stream specified, cannot continue");
        return -libc::EINVAL;
    };
    let disable_pollfd = config
        .get("DISABLE_POLLFD")
        .is_some_and(|v| v.trim() == "1");
    let use_flows_clock = config
        .get("USE_FLOWS_CLOCK")
        .is_some_and(|v| v.trim() == "1");

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

    let myio = Box::into_raw(Box::new(MyIOPlug {
        io: zeroed(),
        callbacks,
        stream: Stream::new(direction, settings),
        ref_time: Instant::now(),
        use_flows_clock,
        capture_written: Arc::new(vec![]),
        stream_info: StreamInfo::new(std::ptr::null()), // set below, when io's address is known
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

    let self_info = &(*myio).stream.settings().self_info;

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
