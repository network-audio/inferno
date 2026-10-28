//! The shared region: a header with the cards and the clock, the rings' positions, then the rings.

use std::{
  any::Any,
  sync::atomic::{AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use crate::SharedClock;

pub const MAGIC: u64 = u64::from_le_bytes(*b"INFRNOSC");
/// changed whenever the layout changes: the service and the plug-in must be of the same version
pub const VERSION: u32 = 1;
pub const MAX_CARDS: usize = 32;
pub const NAME_LEN: usize = 128;
pub const UID_LEN: usize = 64;
const ALIGN: usize = 64;

/// The region's state, set by the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum State {
  /// the receiver and transmitter run: the cards can be used
  Running = 1,
  /// the service has stopped using the region (it quit, or restarts the cards with a new configuration)
  Closed = 2,
}

/// A card, as the service describes it to the plug-in.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CardInfo {
  name: [u8; NAME_LEN],
  uid: [u8; UID_LEN],
  pub playback_channels: u32,
  pub capture_channels: u32,
  /// the first of the rings' playback (TX) channels the card plays to
  pub first_playback: u32,
  /// the first of the rings' capture (RX) channels the card records from
  pub first_capture: u32,
}

fn str_of(bytes: &[u8]) -> &str {
  let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
  // written from a `&str`, cut at a character boundary
  std::str::from_utf8(&bytes[..end]).unwrap_or("")
}

fn copy_str<const N: usize>(s: &str) -> [u8; N] {
  let mut out = [0; N];
  let mut end = s.len().min(N - 1);
  while !s.is_char_boundary(end) {
    end -= 1;
  }
  out[..end].copy_from_slice(&s.as_bytes()[..end]);
  out
}

impl CardInfo {
  pub fn name(&self) -> &str {
    str_of(&self.name)
  }
  /// unique, and the same whenever the service starts the card again (Core Audio remembers settings by it)
  pub fn uid(&self) -> &str {
    str_of(&self.uid)
  }
}

#[repr(C)]
pub struct Header {
  magic: u64,
  version: u32,
  pub sample_rate: u32,
  total_size: u64,
  /// different for every region the service creates
  pub generation: u64,
  /// frames of each ring (a power of 2)
  pub ring_frames: u32,
  card_count: u32,
  pub capture_channels: u32,
  pub playback_channels: u32,
  /// how much later than its frames playback audio is transmitted (frames)
  pub playback_lead: u32,
  /// the network's receive latency (frames): what the cards' recording lags behind the transmitters
  pub input_latency: u32,
  /// the network's transmit latency (frames)
  pub output_latency: u32,
  state: AtomicU32,
  /// the media clock frame the rings' position 0 is (set when the receiver and transmitter start)
  pub start_time: AtomicU64,
  /// the service's monotonic clock (`CLOCK_MONOTONIC`, ns), updated while it runs
  pub heartbeat_ns: AtomicU64,
  pub clock: SharedClock,
  /// playback written after the transmitter had read it (transmitted as silence)
  pub late_playback: AtomicU64,
  /// capture read before it was received (silence instead)
  pub unreceived_capture: AtomicU64,
  cards: [CardInfo; MAX_CARDS],
}

impl Header {
  pub fn state(&self) -> Option<State> {
    match self.state.load(Ordering::Acquire) {
      1 => Some(State::Running),
      2 => Some(State::Closed),
      _ => None,
    }
  }

  pub fn set_state(&self, state: State) {
    self.state.store(state as u32, Ordering::Release);
  }

  pub fn cards(&self) -> &[CardInfo] {
    &self.cards[..self.card_count as usize]
  }
}

/// Where everything is, from the channel counts and the ring length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
  capture_channels: usize,
  playback_channels: usize,
  ring_frames: usize,
  capture_readable: usize,
  playback_written: usize,
  capture_rings: usize,
  playback_rings: usize,
  /// bytes of the region
  pub size: usize,
}

fn align(offset: usize, to: usize) -> usize {
  offset.div_ceil(to) * to
}

impl Layout {
  pub fn new(capture_channels: usize, playback_channels: usize, ring_frames: usize) -> Self {
    let capture_readable = align(size_of::<Header>(), ALIGN);
    let playback_written = align(capture_readable + capture_channels * size_of::<AtomicUsize>(), ALIGN);
    let capture_rings = align(playback_written + playback_channels * size_of::<AtomicUsize>(), 4096);
    let ring = ring_frames * size_of::<AtomicI32>();
    let playback_rings = capture_rings + capture_channels * ring;
    let size = align(playback_rings + playback_channels * ring, 4096);
    Self {
      capture_channels,
      playback_channels,
      ring_frames,
      capture_readable,
      playback_written,
      capture_rings,
      playback_rings,
      size,
    }
  }
}

/// A card of a region to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionCard {
  pub name: String,
  pub uid: String,
  pub playback_channels: usize,
  pub capture_channels: usize,
}

/// What a region to create holds, besides its cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionParams {
  pub sample_rate: u32,
  pub ring_frames: usize,
  pub generation: u64,
  pub playback_lead: usize,
  pub input_latency: usize,
  pub output_latency: usize,
}

/// The shared memory, mapped in this process.
pub struct Region {
  ptr: *mut u8,
  len: usize,
  layout: Layout,
  owner: Box<dyn Any + Send + Sync>,
}

// safety: everything in the region shared between threads is atomic, the rest is written before it's shared
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Region {
  /// The layout of a region for `cards`.
  pub fn layout_for(cards: &[RegionCard], ring_frames: usize) -> Layout {
    Layout::new(
      cards.iter().map(|c| c.capture_channels).sum(),
      cards.iter().map(|c| c.playback_channels).sum(),
      ring_frames,
    )
  }

  /// Creates a region for `cards` in zeroed memory of `layout_for(cards).size` bytes at `ptr`, kept mapped by
  /// `owner` (unmapped when it's dropped). Its state is still unset: `Running` once the rings are in use.
  ///
  /// # Safety
  /// `ptr` must be valid, zeroed, page aligned memory of at least that size, not used otherwise.
  pub unsafe fn create(
    ptr: *mut u8,
    len: usize,
    owner: Box<dyn Any + Send + Sync>,
    cards: &[RegionCard],
    params: RegionParams,
  ) -> Result<Self, String> {
    if cards.len() > MAX_CARDS {
      return Err(format!("at most {MAX_CARDS} sound cards"));
    }
    assert!(params.ring_frames.is_power_of_two());
    let layout = Self::layout_for(cards, params.ring_frames);
    assert!(len >= layout.size);
    // safety: the caller's
    let header = unsafe { &mut *(ptr as *mut Header) };
    header.magic = MAGIC;
    header.version = VERSION;
    header.sample_rate = params.sample_rate;
    header.total_size = layout.size as u64;
    header.generation = params.generation;
    header.ring_frames = params.ring_frames as u32;
    header.card_count = cards.len() as u32;
    header.capture_channels = layout.capture_channels as u32;
    header.playback_channels = layout.playback_channels as u32;
    header.playback_lead = params.playback_lead as u32;
    header.input_latency = params.input_latency as u32;
    header.output_latency = params.output_latency as u32;
    let (mut capture, mut playback) = (0, 0);
    for (info, card) in header.cards.iter_mut().zip(cards) {
      *info = CardInfo {
        name: copy_str(&card.name),
        uid: copy_str(&card.uid),
        playback_channels: card.playback_channels as u32,
        capture_channels: card.capture_channels as u32,
        first_playback: playback as u32,
        first_capture: capture as u32,
      };
      (capture, playback) = (capture + card.capture_channels, playback + card.playback_channels);
    }
    Ok(Self { ptr, len, layout, owner })
  }

  /// A region of `len` bytes at `ptr`, created by another process (or `create`), if it's valid.
  ///
  /// # Safety
  /// `ptr` must be valid memory of `len` bytes, page aligned, for as long as `owner` lives.
  pub unsafe fn open(
    ptr: *mut u8,
    len: usize,
    owner: Box<dyn Any + Send + Sync>,
  ) -> Result<Self, String> {
    if len < size_of::<Header>() {
      return Err("the region is too small".into());
    }
    // safety: the caller's
    let header = unsafe { &*(ptr as *const Header) };
    if header.magic != MAGIC {
      return Err("not an Inferno sound card region".into());
    }
    if header.version != VERSION {
      return Err(format!(
        "the sound card service is of another version (layout {}, expected {VERSION}): reinstall Inferno",
        header.version
      ));
    }
    let ring_frames = header.ring_frames as usize;
    if !ring_frames.is_power_of_two() || header.card_count as usize > MAX_CARDS || header.sample_rate == 0
    {
      return Err("invalid region header".into());
    }
    let layout =
      Layout::new(header.capture_channels as usize, header.playback_channels as usize, ring_frames);
    if layout.size > len || layout.size as u64 != header.total_size {
      return Err("the region doesn't fit its layout".into());
    }
    for card in header.cards() {
      if card.first_capture as u64 + card.capture_channels as u64 > header.capture_channels as u64
        || card.first_playback as u64 + card.playback_channels as u64 > header.playback_channels as u64
      {
        return Err(format!("{}: channels outside the rings", card.name()));
      }
    }
    Ok(Self { ptr, len, layout, owner })
  }

  /// A zeroed region in this process's memory, for `create` (tests, and the Windows sound cards).
  pub fn heap(cards: &[RegionCard], params: RegionParams) -> Result<Self, String> {
    let size = Self::layout_for(cards, params.ring_frames).size;
    let mut memory = vec![Page([0; 4096]); size / 4096].into_boxed_slice();
    let ptr = memory.as_mut_ptr() as *mut u8;
    // safety: zeroed, page aligned, owned by the region
    unsafe { Self::create(ptr, size, Box::new(HeapMemory(memory)), cards, params) }
  }

  pub fn header(&self) -> &Header {
    // safety: checked by `create` or `open`
    unsafe { &*(self.ptr as *const Header) }
  }

  pub fn len(&self) -> usize {
    self.len
  }

  /// What keeps the memory mapped, as given to `create` or `open`.
  pub fn owner(&self) -> &(dyn Any + Send + Sync) {
    &*self.owner
  }

  pub fn is_empty(&self) -> bool {
    self.len == 0
  }

  pub fn ptr(&self) -> *mut u8 {
    self.ptr
  }

  fn at<T>(&self, offset: usize, count: usize) -> &[T] {
    debug_assert!(offset + count * size_of::<T>() <= self.layout.size);
    // safety: within the region (by its layout), aligned, and only atomics are shared
    unsafe { std::slice::from_raw_parts(self.ptr.add(offset) as *const T, count) }
  }

  pub fn ring_frames(&self) -> usize {
    self.layout.ring_frames
  }

  /// The ring of capture (RX) channel `ch`.
  pub fn capture_ring(&self, ch: usize) -> &[AtomicI32] {
    assert!(ch < self.layout.capture_channels);
    let frames = self.layout.ring_frames;
    self.at(self.layout.capture_rings + ch * frames * size_of::<AtomicI32>(), frames)
  }

  /// The ring of playback (TX) channel `ch`.
  pub fn playback_ring(&self, ch: usize) -> &[AtomicI32] {
    assert!(ch < self.layout.playback_channels);
    let frames = self.layout.ring_frames;
    self.at(self.layout.playback_rings + ch * frames * size_of::<AtomicI32>(), frames)
  }

  /// Up to where (position since the start) the receiver has made each capture ring readable.
  pub fn capture_readable(&self) -> &[AtomicUsize] {
    self.at(self.layout.capture_readable, self.layout.capture_channels)
  }

  /// Up to where (position since the start) each playback ring has been written (silence beyond).
  pub fn playback_written(&self) -> &[AtomicUsize] {
    self.at(self.layout.playback_written, self.layout.playback_channels)
  }

  /// Position since the start now, if the clock is available.
  #[cfg(unix)]
  pub fn position_now(&self) -> Option<usize> {
    let header = self.header();
    let overlay = header.clock.read()?;
    let now = crate::underlying_now_ns(overlay.clock_id);
    let start_time = header.start_time.load(Ordering::Acquire) as usize;
    Some(overlay.frames(now, header.sample_rate).wrapping_sub(start_time))
  }
}

#[derive(Clone, Copy)]
#[repr(C, align(4096))]
struct Page([u8; 4096]);

struct HeapMemory(#[allow(dead_code)] Box<[Page]>);

#[cfg(test)]
mod tests {
  use super::*;

  fn cards() -> Vec<RegionCard> {
    vec![
      RegionCard {
        name: "Stage".into(),
        uid: "inferno-1".into(),
        playback_channels: 2,
        capture_channels: 8,
      },
      RegionCard {
        name: "Desk ü".into(),
        uid: "inferno-2".into(),
        playback_channels: 4,
        capture_channels: 0,
      },
    ]
  }

  const PARAMS: RegionParams = RegionParams {
    sample_rate: 48000,
    ring_frames: 8192,
    generation: 7,
    playback_lead: 480,
    input_latency: 480,
    output_latency: 480,
  };

  #[test]
  fn opened_as_created() {
    let region = Region::heap(&cards(), PARAMS).unwrap();
    region.header().start_time.store(12345, Ordering::Relaxed);
    region.header().set_state(State::Running);
    region.playback_ring(5)[8191].store(-1, Ordering::Relaxed);
    region.capture_readable()[7].store(99, Ordering::Relaxed);
    // safety: the heap region outlives the view
    let opened = unsafe { Region::open(region.ptr(), region.len(), Box::new(())) }.unwrap();
    let header = opened.header();
    let start_time = header.start_time.load(Ordering::Relaxed);
    assert_eq!((header.sample_rate, header.generation, start_time), (48000, 7, 12345));
    assert_eq!(header.state(), Some(State::Running));
    let names: Vec<_> = header.cards().iter().map(|c| (c.name(), c.uid())).collect();
    assert_eq!(names, [("Stage", "inferno-1"), ("Desk ü", "inferno-2")]);
    let desk = &header.cards()[1];
    assert_eq!((desk.first_playback, desk.playback_channels, desk.first_capture), (2, 4, 8));
    assert_eq!(opened.playback_ring(5)[8191].load(Ordering::Relaxed), -1);
    assert_eq!(opened.capture_readable()[7].load(Ordering::Relaxed), 99);
    let end = opened.playback_ring(5).as_ptr_range().end as usize;
    assert!(end <= region.ptr() as usize + region.len(), "the last ring is within the region");
  }

  #[test]
  fn regions_of_another_version_are_refused() {
    let region = Region::heap(&cards(), PARAMS).unwrap();
    // safety: the heap region outlives the views
    unsafe {
      (*(region.ptr() as *mut Header)).version = VERSION + 1;
      let refused = Region::open(region.ptr(), region.len(), Box::new(())).err().unwrap();
      assert!(refused.contains("version"), "{refused}");
      assert!(Region::open(region.ptr(), 100, Box::new(())).is_err());
    }
  }

  #[test]
  fn long_names_are_cut_at_a_character() {
    let long: [u8; 7] = copy_str("abcdeü");
    assert_eq!(str_of(&long), "abcde");
  }
}
