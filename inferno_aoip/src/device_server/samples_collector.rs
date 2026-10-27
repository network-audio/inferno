use std::{pin::Pin, sync::Arc, time::Duration};

use crate::{
  common::*,
  device_info::DeviceInfo,
  media_clock::{ClockOverlay, MediaClock},
  ring_buffer::{ProxyToBuffer, ProxyToSamplesBuffer, RBOutput},
  util::real_time_box_channel::{self, RealTimeBoxReceiver, RealTimeBoxSender},
};
use futures::{Future, FutureExt};

use itertools::Itertools;
use tokio::{sync::mpsc, time::interval};

const READ_INTERVAL: Duration = Duration::from_millis(50);
const BUFFER_SIZE: usize = 65536;
const SANE_CLOCK_DIFF: usize = 192000;
const MAX_LAG_SAMPLES: Clock = 9600;
/// How long to wait for channels to receive before going on without them.
const CLOCK_MARGIN_MS: Clock = 200;

pub type SamplesCallback = Box<dyn FnMut(usize, &Vec<Vec<Sample>>) + Send + 'static>;

struct Channel<P: ProxyToSamplesBuffer> {
  id: usize,
  source: RBOutput<Sample, P>,
  prev_holes_count: usize,
  latency_samples: Clock,
  was_connected: bool,
}

impl<P: ProxyToSamplesBuffer> Channel<P> {
  fn report_lost_samples(&self, timestamp: Clock, num_samples: usize, reason: &str) {
    error!("Lost {num_samples} samples at timestamp {timestamp} in channel id {} ({reason})", self.id);
  }
  fn read_samples_from_ringbuffer(&mut self, start_timestamp: Clock, buffer: &mut [Sample]) -> bool {
    let mut good = true;
    // report holes:
    let holes_count = self.source.holes_count();
    if holes_count != self.prev_holes_count {
      debug!("holes {} -> {}", self.prev_holes_count, holes_count);
      self.report_lost_samples(start_timestamp, buffer.len(), "reorder buffer timeout");
      self.prev_holes_count = holes_count;
      good = false;
    }

    // read samples:
    let r = self.source.read_at(start_timestamp as usize, buffer);
    if r.useful_start_index != 0 {
      if self.was_connected {
        self.report_lost_samples(
          start_timestamp,
          r.useful_start_index,
          "buffer underrun or overwritten in the meantime",
        );
        good = false;
      }
      // clear whatever junk data was contained at the beginning of buffer
      for sample in &mut buffer[0..r.useful_start_index] {
        *sample = 0;
      }
    }
    if r.useful_start_index < r.useful_end_index {
      self.was_connected = true;
    }
    if r.useful_end_index != buffer.len() {
      if self.was_connected {
        self.report_lost_samples(
          start_timestamp.wrapping_add(r.useful_end_index.try_into().unwrap()),
          buffer.len() - r.useful_end_index,
          "buffer underrun",
        );
        good = false;
      }
      for sample in &mut buffer[r.useful_end_index..] {
        *sample = 0;
      }
    }
    if !good {
      warn!(
        "wanted {start_timestamp}..{} but has ..{}",
        start_timestamp + buffer.len(),
        self.source.readable_until()
      );
    }
    good
  }
}

pub struct RealTimeSamplesReceiver<P: ProxyToSamplesBuffer> {
  channels: Vec<RealTimeBoxReceiver<Option<Channel<P>>>>,
  clock: MediaClock,
  clock_recv: RealTimeBoxReceiver<Option<ClockOverlay>>,
}

// MAYBE TODO move timestamp checks to separate module

impl<P: ProxyToSamplesBuffer> RealTimeSamplesReceiver<P> {
  fn get_min_max_end_timestamps(&mut self) -> Option<(Clock, Clock)> {
    get_min_max_end_timestamps(self.channels.iter_mut().map(|chrecv| {
      chrecv.update();
      chrecv.get()
    }))
  }
  pub fn get_available_num_samples(&mut self, start_timestamp: Clock) -> usize {
    self
      .get_min_max_end_timestamps()
      .map(|(end_ts, _)| {
        let diff = wrapped_diff(end_ts, start_timestamp);
        if diff > 0 {
          diff as Clock
        } else {
          0
        }
      })
      .unwrap_or(0)
      .try_into()
      .unwrap()
  }
  pub fn get_samples(
    &mut self,
    start_timestamp: Clock,
    channel_index: usize,
    buffer: &mut [Sample],
  ) -> bool {
    let chrecv = &mut self.channels[channel_index];
    chrecv.update();
    if let Some(ch) = chrecv.get_mut() {
      let start_timestamp =
        start_timestamp.wrapping_sub(ch.latency_samples).wrapping_sub(buffer.len() as Clock);
      ch.read_samples_from_ringbuffer(start_timestamp, buffer)
    } else {
      buffer.fill(0);
      true
    }
  }
  pub fn clock(&mut self) -> &MediaClock {
    if self.clock_recv.update() {
      if let Some(ovl) = self.clock_recv.get() {
        self.clock.update_overlay(*ovl);
      }
    }
    &self.clock
  }
}

enum Command<P: ProxyToSamplesBuffer> {
  NoOp,
  Shutdown,
  ConnectChannel { channel_index: usize, source: RBOutput<Sample, P>, latency_samples: usize },
  DisconnectChannel { channel_index: usize },
}

struct ToRealTime<P: ProxyToSamplesBuffer> {
  commands_receiver: mpsc::Receiver<Command<P>>,
  senders: Vec<RealTimeBoxSender<Option<Channel<P>>>>,
}

impl<P: ProxyToSamplesBuffer> ToRealTime<P> {
  async fn run(&mut self) {
    loop {
      let command_opt = self.commands_receiver.recv().await;
      if !self.handle_command(command_opt).await {
        break;
      }
      for sender in &self.senders {
        sender.collect_garbage();
      }
    }
  }
  async fn handle_command(&mut self, command_opt: Option<Command<P>>) -> bool {
    let command = command_opt.unwrap_or(Command::Shutdown);
    match command {
      Command::ConnectChannel { channel_index, source, latency_samples } => {
        debug!("connecting channel index={channel_index}");
        self.senders[channel_index].send(Box::new(Some(Channel {
          id: channel_index + 1,
          source,
          prev_holes_count: 0,
          latency_samples: latency_samples.try_into().unwrap(),
          was_connected: false,
        })));
      }
      Command::DisconnectChannel { channel_index } => {
        debug!("disconnecting channel index={channel_index}");
        self.senders[channel_index].send(Box::new(None));
      }
      Command::Shutdown => {
        return false;
      }
      Command::NoOp => {}
    };
    return true;
  }
}

struct PeriodicSamplesCollector<P: ProxyToSamplesBuffer> {
  commands_receiver: mpsc::Receiver<Command<P>>,
  channels: Vec<Option<Channel<P>>>,
  callback: SamplesCallback,
  clock: MediaClock,
  /// `None`: only as far as the channels have received
  clock_recv: Option<RealTimeBoxReceiver<Option<ClockOverlay>>>,
  sample_rate: u32,
  latency_samples: Clock,
}

fn get_min_max_end_timestamps<'a, P: ProxyToSamplesBuffer + 'a>(
  channels: impl IntoIterator<Item = &'a Option<Channel<P>>>,
) -> Option<(Clock, Clock)> {
  let clocks = channels
    .into_iter()
    .filter_map(|opt| opt.as_ref())
    .map(|ch| ch.source.readable_until())
    .collect_vec();
  Some((
    clocks.iter().min_by(|&&a, &&b| wrapped_diff(a, b).cmp(&0))?.to_owned().try_into().unwrap(),
    clocks.iter().max_by(|&&a, &&b| wrapped_diff(a, b).cmp(&0))?.to_owned().try_into().unwrap(),
  ))
}

impl<P: ProxyToSamplesBuffer> PeriodicSamplesCollector<P> {
  /// Where to read up to, from `position` (where the last read ended, if any): as far as the connected channels
  /// have received. If following the clock, channels which have received nothing past `position` for
  /// `CLOCK_MARGIN_MS` (after the receive latency: the largest of the device's and the connected flows') don't
  /// hold it back, and without any channel holding it back, the clock does: time goes on, in silence, while
  /// nothing is received. A channel still receiving holds it back even when late (e.g. the packets queued
  /// while its flow started, written in a burst), so that it's read whole.
  fn readable_until(&mut self, position: Option<Clock>) -> Option<Clock> {
    let Some(clock) = self.clock_position() else {
      return self.received_until(|_| true);
    };
    let Some(position) = position else {
      // the first read: from the clock, or what's been received since
      return match self.received_until(|_| true) {
        Some(received) if wrapped_diff(received, clock) > 0 => Some(received),
        _ => Some(clock),
      };
    };
    let overdue = |until: Clock| wrapped_diff(until, position) <= 0 && wrapped_diff(until, clock) < 0;
    match self.received_until(|until| !overdue(until)) {
      Some(received) => Some(received),
      None if wrapped_diff(clock, position) > 0 => Some(clock),
      None => None,
    }
  }
  /// As far as the channels whose received position `holds` have received.
  fn received_until(&self, holds: impl Fn(Clock) -> bool) -> Option<Clock> {
    let ends =
      self.channels.iter().flatten().map(|ch| ch.source.readable_until()).filter(|&end| holds(end)).collect_vec();
    let min_end_ts = *ends.iter().min_by(|&&a, &&b| wrapped_diff(a, b).cmp(&0))?;
    let max_end_ts = *ends.iter().max_by(|&&a, &&b| wrapped_diff(a, b).cmp(&0))?;
    // lag prevention logic to prevent abruptly disconnected transmitters from causing buffer overrun
    // in other audio channels
    let lag = max_end_ts.wrapping_sub(min_end_ts);
    Some(if lag > MAX_LAG_SAMPLES { max_end_ts.wrapping_sub(MAX_LAG_SAMPLES) } else { min_end_ts })
  }
  /// The media clock minus the receive latency and `CLOCK_MARGIN_MS`, if following the clock and it's available.
  fn clock_position(&mut self) -> Option<Clock> {
    let clock_recv = self.clock_recv.as_mut()?;
    // also the receiver's initial overlay, before any update
    if clock_recv.update() || !self.clock.is_ready() {
      if let Some(ovl) = clock_recv.get() {
        self.clock.update_overlay(*ovl);
      }
    }
    let now = self.clock.wrapping_now_in_timebase(self.sample_rate as u64)?;
    let latency =
      self.channels.iter().flatten().map(|ch| ch.latency_samples).fold(self.latency_samples, Clock::max);
    Some(now.wrapping_sub(latency + self.sample_rate as Clock * CLOCK_MARGIN_MS / 1000))
  }
  async fn run(&mut self) {
    let mut clock = None;
    let mut read_data_interval = interval(READ_INTERVAL);
    read_data_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut channels_buffers = (0..self.channels.len()).map(|_| vec![0; BUFFER_SIZE]).collect_vec();
    loop {
      tokio::select! {
        _ = read_data_interval.tick() => {
          if let Some(readable_until) = self.readable_until(clock) {
            let new_clock = {
              if let Some(mut start_timestamp) = clock {
                // we already have clock, use it as a start timestamp

                // e.g. the clock took over from the channels, or a flow with a higher latency connected: wait until
                // it's past where we are
                if wrapped_diff(readable_until, start_timestamp) <= 0 {
                  continue;
                }
                let mut readable_samples_count: usize = readable_until.wrapping_sub(start_timestamp);
                if readable_samples_count > SANE_CLOCK_DIFF {
                  error!("insane clock diff {readable_samples_count}, using {SANE_CLOCK_DIFF} last samples");
                  readable_samples_count = SANE_CLOCK_DIFF;
                  start_timestamp = readable_until.wrapping_sub(readable_samples_count.try_into().unwrap());
                }
                if readable_samples_count > BUFFER_SIZE.try_into().unwrap() {
                  readable_samples_count = BUFFER_SIZE.try_into().unwrap();
                }
                for chi in 0..self.channels.len() {
                  let buffer = &mut channels_buffers[chi][0..readable_samples_count];
                  match &mut self.channels[chi] {
                    // received nothing of it (yet, or the transmitter is gone): silence
                    Some(ch) if wrapped_diff(ch.source.readable_until(), start_timestamp) <= 0 => {
                      ch.was_connected = false;
                      buffer.fill(0);
                    }
                    // what the channel hasn't received in time is silence
                    Some(ch) => {
                      ch.read_samples_from_ringbuffer(start_timestamp, buffer);
                    }
                    None => buffer.fill(0),
                  }
                }
                (self.callback)(readable_samples_count, &channels_buffers);
                start_timestamp.wrapping_add(readable_samples_count.try_into().unwrap())
              } else {
                // we don't have clock yet, bootstrap it using currently available timestamp
                readable_until
              }
            };
            for chi in 0..self.channels.len() {
              if let Some(ch) = &mut self.channels[chi] {
                ch.source.read_done(new_clock as usize); // TODO force buffer sizes to be power of 2
              }
            }
            clock = Some(new_clock);
          }
        }
        command_opt = self.commands_receiver.recv() => {
          if !self.handle_command(command_opt).await {
            break;
          }
        }
      }
    }
  }
  async fn handle_command(&mut self, command_opt: Option<Command<P>>) -> bool {
    let command = command_opt.unwrap_or(Command::Shutdown);
    match command {
      Command::ConnectChannel { channel_index, source, latency_samples } => {
        debug!("connecting channel index={channel_index}");
        self.channels[channel_index] = Some(Channel {
          id: channel_index + 1,
          source,
          prev_holes_count: 0,
          latency_samples: latency_samples.try_into().unwrap(),
          was_connected: false,
        });
      }
      Command::DisconnectChannel { channel_index } => {
        debug!("disconnecting channel index={channel_index}");
        self.channels[channel_index] = None;
      }
      Command::Shutdown => {
        return false;
      }
      Command::NoOp => {}
    };
    return true;
  }
}

pub struct SamplesCollector<P: ProxyToSamplesBuffer> {
  commands_sender: mpsc::Sender<Command<P>>,
}

impl<P: ProxyToSamplesBuffer + Sync + Send + 'static> SamplesCollector<P> {
  pub fn new_with_callback(
    self_info: Arc<DeviceInfo>,
    callback: SamplesCallback,
    clock_recv: Option<RealTimeBoxReceiver<Option<ClockOverlay>>>,
  ) -> (Self, Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {
    let (tx, rx) = mpsc::channel(100);
    let mut internal = PeriodicSamplesCollector {
      commands_receiver: rx,
      channels: (0..self_info.rx_channels.len()).map(|_| None).collect(),
      callback,
      clock: MediaClock::new(false /* TODO */),
      clock_recv,
      sample_rate: self_info.sample_rate,
      latency_samples: self_info.latency_samples(),
    };
    return (Self { commands_sender: tx }, async move { internal.run().await }.boxed());
  }

  pub fn new_realtime(
    self_info: Arc<DeviceInfo>,
    clock_recv: RealTimeBoxReceiver<Option<ClockOverlay>>,
  ) -> (Self, Pin<Box<dyn Future<Output = ()> + Send + 'static>>, RealTimeSamplesReceiver<P>) {
    let (tx, rx) = mpsc::channel(100);
    let (senders, receivers) =
      (0..self_info.rx_channels.len()).map(|chi| real_time_box_channel::channel(Box::new(None))).unzip();

    let mut internal = ToRealTime { commands_receiver: rx, senders };

    (
      Self { commands_sender: tx },
      async move { internal.run().await }.boxed(),
      RealTimeSamplesReceiver {
        channels: receivers,
        clock: MediaClock::new(false /* TODO */),
        clock_recv,
      },
    )
  }

  /* pub fn new_external(self_info: Arc<DeviceInfo>, external_channels: impl IntoIterator<Item = ExternalBufferParameters<Sample>>) -> (Self, Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {

  } */

  pub async fn connect_channel(
    &self,
    channel_index: usize,
    source: RBOutput<Sample, P>,
    latency_samples: usize,
  ) {
    self
      .commands_sender
      .send(Command::ConnectChannel { channel_index, source, latency_samples })
      .await
      .log_and_forget();
  }
  pub async fn disconnect_channel(&self, channel_index: usize) {
    self.commands_sender.send(Command::DisconnectChannel { channel_index }).await.log_and_forget();
  }
  pub async fn shutdown(&self) {
    self.commands_sender.send(Command::Shutdown).await.log_and_forget();
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Mutex;

  use atomic::Atomic;

  use super::*;
  use crate::{
    device_info::{tests::dummy_device_info, Channel as DeviceChannel},
    ring_buffer::{self, OwnedBuffer},
  };

  const RATE: u32 = 48000;

  fn collector(
    channels: usize,
  ) -> (SamplesCollector<OwnedBuffer<Atomic<Sample>>>, Arc<Mutex<Vec<Vec<Sample>>>>, MediaClock) {
    let mut info = dummy_device_info(0, RATE);
    info.rx_channels = (0..channels)
      .map(|i| DeviceChannel { factory_name: format!("{i}"), friendly_name: Default::default() })
      .collect();
    let recorded = Arc::new(Mutex::new(vec![vec![]; channels]));
    let rec = recorded.clone();
    let callback = move |count: usize, chs: &Vec<Vec<Sample>>| {
      let mut rec = rec.lock().unwrap();
      for (r, ch) in rec.iter_mut().zip(chs) {
        r.extend_from_slice(&ch[..count]);
      }
    };
    let (_, clock_recv) = real_time_box_channel::channel(Box::new(Some(ClockOverlay::monotonic())));
    let (col, fut) =
      SamplesCollector::new_with_callback(Arc::new(info), Box::new(callback), Some(clock_recv));
    tokio::spawn(fut);
    let mut clock = MediaClock::new(false);
    clock.update_overlay(ClockOverlay::monotonic());
    (col, recorded, clock)
  }

  /// A clocked collector with channels received until `ends`, relative to its clock position (returned).
  fn collector_receiving_until(ends: &[isize]) -> (PeriodicSamplesCollector<OwnedBuffer<Atomic<Sample>>>, Clock) {
    let (_, clock_recv) = real_time_box_channel::channel(Box::new(Some(ClockOverlay::monotonic())));
    let (_, rx) = mpsc::channel(1);
    let mut collector = PeriodicSamplesCollector {
      commands_receiver: rx,
      channels: vec![],
      callback: Box::new(|_, _| {}),
      clock: MediaClock::new(false),
      clock_recv: Some(clock_recv),
      sample_rate: RATE,
      latency_samples: 0,
    };
    let clock = collector.clock_position().unwrap();
    for (id, &end) in ends.iter().enumerate() {
      let start = clock.wrapping_add_signed(end - 1000);
      let (mut sink, source) = ring_buffer::new_owned::<Sample>(1 << 16, start, 0);
      sink.write_from_at(start, std::iter::repeat(1).take(1000));
      let channel = Channel { id, source, prev_holes_count: 0, latency_samples: 0, was_connected: true };
      collector.channels.push(Some(channel));
    }
    (collector, clock)
  }

  #[test]
  fn clocked_reading_waits_for_channels_still_receiving() {
    // positions relative to the clock's, far apart: it moves on while the test runs
    let read = |ends: &[isize], position: isize| {
      let (mut collector, clock) = collector_receiving_until(ends);
      let until = collector.readable_until(Some(clock.wrapping_add_signed(position)));
      (until.map(|u| wrapped_diff(u, clock)), clock)
    };
    // a flow's start, written in a burst: late, but received past the position: wait for it
    assert_eq!(read(&[-5_000, 2_000], -8_000).0, Some(-5_000));
    // a channel with nothing new, for longer than the margin, doesn't hold it back
    assert_eq!(read(&[-40_000, 5_000], -40_000).0, Some(5_000));
    // nothing new, not yet overdue (the position is ahead of the clock): wait
    assert_eq!(read(&[20_000], 20_000).0, Some(20_000));
    // nothing received for longer than the margin: the clock
    let until = read(&[-40_000], -40_000).0.unwrap();
    assert!((0..4800).contains(&until), "{until}");
    // no channels: the clock
    let until = read(&[], -40_000).0.unwrap();
    assert!((0..4800).contains(&until), "{until}");
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn silence_while_nothing_is_received() {
    let (col, recorded, _) = collector(2);
    let started = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let expected = started.elapsed().as_secs_f64() * RATE as f64;
    col.shutdown().await;
    let rec = recorded.lock().unwrap();
    for ch in rec.iter() {
      assert!(
        (ch.len() as f64 - expected).abs() < 0.15 * RATE as f64,
        "{} samples, expected {expected}",
        ch.len()
      );
      assert!(ch.iter().all(|&s| s == 0));
    }
  }

  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn gaps_in_reception_are_silence() {
    let (col, recorded, clock) = collector(2);
    tokio::time::sleep(Duration::from_millis(120)).await;
    let start = clock.wrapping_now_in_timebase(RATE as u64).unwrap();
    let (mut sink, source) = ring_buffer::new_owned::<Sample>(524288, start, 0);
    col.connect_channel(0, source, 0).await;
    // 0.1 s of audio, then the transmitter stops
    sink.write_from_at(start, std::iter::repeat(7).take(4800));
    let started = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(600)).await;
    col.shutdown().await;
    let rec = recorded.lock().unwrap();
    let audio = &rec[0];
    let first = audio.iter().position(|&s| s != 0).expect("no audio recorded");
    assert!(audio[first..first + 4800].iter().all(|&s| s == 7));
    assert!(audio[first + 4800..].iter().all(|&s| s == 0));
    // time went on after the transmitter stopped
    let after = (audio.len() - first - 4800) as f64 / RATE as f64;
    assert!(after > started.elapsed().as_secs_f64() - 0.1 - 0.2 - 0.15, "only {after} s after the audio");
    assert!(rec[1].iter().all(|&s| s == 0));
    assert_eq!(rec[0].len(), rec[1].len());
  }
}
