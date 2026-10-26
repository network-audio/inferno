use clap::Parser;
use log::{error, info};
use std::fs::File;
use std::io::Read;
use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::oneshot;

use inferno_aoip::device_server::{
    AtomicSample, DeviceServer, ExternalBufferParameters, MediaClock, RealTimeClockReceiver,
    Sample, Settings, StreamAnchor,
};

const ABOUT: &str = "Pipe2inferno
Copyright (C) 2023-2025 Teodor Wozniak

This program is free software: you can redistribute it and/or modify
it under the terms of the:

GNU General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version,
or
the GNU Affero General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU General Public License or the GNU Affero General Public License
for more details.

You should have received a copy of the GNU General Public License
and the GNU Affero General Public License along with this program.
If not, see <http://www.gnu.org/licenses/>.
";

/// Frames per channel of the transmit ring buffers (power of 2): about 2.7 s at 48 kHz.
const RING_FRAMES: usize = 1 << 17;
/// How far ahead of the transmitter the input is read.
const READ_AHEAD_FRAMES: usize = RING_FRAMES / 2;
const FEED_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Parser, Debug)]
#[command(author, version, about = ABOUT, long_about = None, arg_required_else_help = true)]
struct Args {
    #[arg(long, short)]
    channels_count: usize,
    /// interleaved signed 32-bit native-endian samples (like Inferno2pipe writes); `-` for stdin
    #[arg(long, short)]
    input: String,
}

/// Media clock timestamp (in samples) as soon as the clock is available.
async fn wait_for_clock(mut clock_rx: RealTimeClockReceiver, sample_rate: u32) -> usize {
    let mut media_clock = MediaClock::new(false);
    loop {
        clock_rx.update();
        if let Some(overlay) = clock_rx.get() {
            media_clock.update_overlay(*overlay);
            if let Some(now) = media_clock.wrapping_now_in_timebase(sample_rate as u64) {
                return now as usize;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Reads the input into the ring buffers, ahead of the transmitter.
struct Feeder {
    input: Box<dyn Read + Send>,
    rings: Arc<Vec<Vec<AtomicSample>>>,
    /// frames since the stream's start written to the rings
    written: Arc<AtomicUsize>,
    bytes: Vec<u8>,
}

impl Feeder {
    /// Reads up to `frames` frames. Returns false at the end of the input.
    fn feed(&mut self, frames: usize) -> bool {
        let channels = self.rings.len();
        let frame_bytes = channels * size_of::<Sample>();
        self.bytes.resize(frames * frame_bytes, 0);
        let mut filled = 0;
        while filled < self.bytes.len() {
            match self.input.read(&mut self.bytes[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    error!("error reading input: {e:?}");
                    break;
                }
            }
        }
        let start = self.written.load(Ordering::Acquire);
        let complete_frames = filled / frame_bytes;
        for (fi, frame) in self.bytes[..complete_frames * frame_bytes].chunks_exact(frame_bytes).enumerate() {
            let index = (start + fi) & (RING_FRAMES - 1);
            for (ring, sample) in self.rings.iter().zip(frame.chunks_exact(size_of::<Sample>())) {
                ring[index].store(Sample::from_ne_bytes(sample.try_into().unwrap()), Ordering::Relaxed);
            }
        }
        self.written.store(start + complete_frames, Ordering::Release);
        complete_frames == frames
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let logenv = env_logger::Env::default().default_filter_or("info");
    env_logger::init_from_env(logenv);

    let args = Args::parse();
    let ch_count = args.channels_count;

    let config = Settings::merged_config(&Default::default());
    let unknown = Settings::unknown_keys(&config, &[]);
    if !unknown.is_empty() {
        log::warn!(
            "ignoring unknown INFERNO_* settings (misspelt?): {}",
            unknown.join(", ")
        );
    }
    let mut settings = match Settings::try_new("Pipe2inferno", "Pipe2inferno", None, &config) {
        Ok(settings) => settings,
        Err(e) => {
            log::error!("{e}");
            std::process::exit(2);
        }
    };
    settings.make_rx_channels(0);
    settings.make_tx_channels(ch_count);
    let sample_rate = settings.self_info.sample_rate;
    let tx_latency_ns = settings.tx_latency_ns;

    let input: Box<dyn Read + Send> = if args.input == "-" {
        Box::new(std::io::stdin())
    } else {
        match File::open(&args.input) {
            Ok(file) => Box::new(file),
            Err(e) => {
                error!("can't open {}: {e}", args.input);
                std::process::exit(2);
            }
        }
    };

    let rings: Arc<Vec<Vec<AtomicSample>>> = Arc::new(
        (0..ch_count)
            .map(|_| (0..RING_FRAMES).map(|_| AtomicSample::new(0)).collect())
            .collect(),
    );
    let written = Arc::new(AtomicUsize::new(0));
    let mut feeder = Feeder { input, rings: rings.clone(), written: written.clone(), bytes: vec![] };
    let mut input_ended = !feeder.feed(READ_AHEAD_FRAMES);

    let mut server = DeviceServer::start(settings).await;
    let start_time = wait_for_clock(server.get_realtime_clock_receiver(), sample_rate).await;

    let valid = Arc::new(RwLock::new(true));
    let buffers = rings
        .iter()
        .map(|ring| {
            let written = written.clone();
            // safety: the rings live until the process ends (`rings` is kept below)
            unsafe { ExternalBufferParameters::new(ring.as_ptr(), RING_FRAMES, 1, valid.clone(), None) }
                // silence where the input hasn't been read (ended, or too slow)
                .with_written_from(Arc::new(move |position| {
                    written.load(Ordering::Acquire).saturating_sub(position)
                }))
        })
        .collect();
    let anchor = StreamAnchor::new_paused();
    // the transmitter's position: unused, the input is read along the media clock (the stream runs whether
    // anyone receives it or not, and the transmitter reports no position without flows)
    let tx_position = Arc::new(AtomicUsize::new(usize::MAX));
    server.transmit_from_external_buffer(buffers, anchor.clone(), tx_position, None).await;
    anchor.start(start_time);
    info!("transmitting {ch_count} channels");

    // a frame at position p is transmitted at media time start_time + p + latency
    let latency_frames = (tx_latency_ns as u64 * sample_rate as u64 / 1_000_000_000) as usize;
    let mut clock_rx = server.get_realtime_clock_receiver();
    let (done_tx, done_rx) = oneshot::channel();
    let feeder_thread = std::thread::spawn(move || {
        let mut media_clock = MediaClock::new(false);
        loop {
            if clock_rx.update() {
                if let Some(overlay) = clock_rx.get() {
                    media_clock.update_overlay(*overlay);
                }
            }
            let Some(now) = media_clock.wrapping_now_in_timebase(sample_rate as u64) else {
                std::thread::sleep(FEED_INTERVAL);
                continue;
            };
            // the stream's position now: what the transmitter reads later, after its latency
            let Ok(position) = usize::try_from((now as usize).wrapping_sub(start_time) as isize) else {
                // the clock stepped back before the start
                std::thread::sleep(FEED_INTERVAL);
                continue;
            };
            if input_ended {
                // until everything read has been transmitted
                if position >= written.load(Ordering::Acquire) + latency_frames {
                    break;
                }
            } else {
                // after a stall, catch up at once: the position may have run far ahead of the input
                let target = position + READ_AHEAD_FRAMES;
                while !input_ended && written.load(Ordering::Acquire) < target {
                    let frames = (target - written.load(Ordering::Acquire)).min(READ_AHEAD_FRAMES);
                    input_ended = !feeder.feed(frames);
                    if input_ended {
                        info!("end of input");
                    }
                }
            }
            std::thread::sleep(FEED_INTERVAL);
        }
        let _ = done_tx.send(());
    });

    tokio::select! {
        _ = done_rx => {
            info!("input transmitted");
        }
        _ = tokio::signal::ctrl_c() => {}
    }
    server.shutdown().await;
    drop(feeder_thread);
    drop(rings);
}
