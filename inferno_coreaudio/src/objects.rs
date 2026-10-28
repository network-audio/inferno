//! The plug-in's Core Audio objects and their properties, independent of Core Audio (and so testable anywhere).
//!
//! The plug-in object owns a device per card of the service's region, each with an input stream (the card's
//! recording channels) and an output stream (its playback channels), if it has those channels, and a volume
//! and a mute control for each (System Settings' sliders, the volume keys). Their object ids are new whenever
//! the cards are (a new region): the old ones are gone.

#![allow(non_upper_case_globals)] // Core Audio's names

use std::sync::{
  atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering},
  Arc,
};

use inferno_soundcard_shm::Region;

use crate::abi::*;

/// Frames between Core Audio's zero time stamps: more than any I/O buffer.
pub const ZERO_TIME_STAMP_PERIOD: u32 = 16384;
/// Frames Core Audio keeps between the present and its I/O (each way): for the receiver writing its frames
/// late, and Core Audio waking up late.
pub const SAFETY_OFFSET: u32 = 32;
pub const MANUFACTURER: &str = "Inferno";
pub const MODEL_UID: &str = "org.network-audio.inferno.soundcard";

/// Object ids per card: the device, its two streams and its four controls (see `DeviceSet::new`).
const IDS_PER_CARD: u32 = 7;
/// The volume controls' range: the slider's bottom is this, then silence at 0.
pub const MIN_DB: f32 = -64.0;

/// Decibels of a volume slider's position (0 to 1): the gain is its square, a taper like other devices'.
pub fn scalar_to_db(scalar: f32) -> f32 {
  if scalar <= 0.0 {
    MIN_DB
  } else {
    (40.0 * scalar.min(1.0).log10()).max(MIN_DB)
  }
}

/// The slider position of `db` decibels (`MIN_DB` and below: 0).
pub fn db_to_scalar(db: f32) -> f32 {
  if db <= MIN_DB {
    0.0
  } else {
    10f32.powf(db.min(0.0) / 40.0)
  }
}

/// A direction's volume and mute, as Core Audio's controls set them.
pub struct Level {
  /// f32 bits of the slider's position, 0 to 1
  scalar: AtomicU32,
  muted: AtomicBool,
}

impl Default for Level {
  fn default() -> Self {
    Self { scalar: AtomicU32::new(1f32.to_bits()), muted: AtomicBool::new(false) }
  }
}

impl Level {
  pub fn scalar(&self) -> f32 {
    f32::from_bits(self.scalar.load(Ordering::Relaxed))
  }

  pub fn set_scalar(&self, scalar: f32) {
    let scalar = if scalar.is_nan() { 1.0 } else { scalar.clamp(0.0, 1.0) };
    self.scalar.store(scalar.to_bits(), Ordering::Relaxed);
  }

  pub fn muted(&self) -> bool {
    self.muted.load(Ordering::Relaxed)
  }

  pub fn set_muted(&self, muted: bool) {
    self.muted.store(muted, Ordering::Relaxed);
  }

  /// The gain applied to the audio: exactly 1 at full volume (the audio passes unchanged), 0 when muted or at
  /// the bottom.
  pub fn gain(&self) -> f32 {
    let scalar = self.scalar();
    if self.muted() || scalar <= 0.0 {
      0.0
    } else if scalar >= 1.0 {
      1.0
    } else {
      scalar * scalar
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlKind {
  Volume,
  Mute,
}

/// A card's volume and mute controls, saved under these keys in Core Audio's storage for the plug-in.
pub fn storage_key(card_uid: &str, direction: Direction, kind: ControlKind) -> String {
  let direction = match direction {
    Direction::Input => "input",
    Direction::Output => "output",
  };
  let kind = match kind {
    ControlKind::Volume => "volume",
    ControlKind::Mute => "mute",
  };
  format!("{}.{direction}.{kind}", device_uid(card_uid))
}

/// Device UIDs: unique on the computer.
pub fn device_uid(card_uid: &str) -> String {
  format!("inferno-{card_uid}")
}

/// A card's device, while Core Audio uses it.
#[derive(Default)]
pub struct Io {
  /// clients that have started I/O
  pub clients: AtomicU32,
  /// the rings' position of sample time 0
  pub origin: AtomicUsize,
  /// changed when the time stamps jump (the clock stepped)
  pub seed: AtomicU64,
  pub last_sample_time: AtomicU64,
  pub last_host_time: AtomicU64,
  /// how the I/O cycles fit the media clock, logged now and then
  pub timing: Timing,
}

/// The least and most frames, since last taken, that the playback written was ahead of when the transmitter
/// reads it, and that the receiver had received beyond the input read (negative: late).
pub struct Timing {
  pub cycles: AtomicU64,
  pub output_min: AtomicI64,
  pub output_max: AtomicI64,
  pub input_min: AtomicI64,
  pub input_max: AtomicI64,
}

impl Default for Timing {
  fn default() -> Self {
    Self {
      cycles: AtomicU64::new(0),
      output_min: AtomicI64::new(i64::MAX),
      output_max: AtomicI64::new(i64::MIN),
      input_min: AtomicI64::new(i64::MAX),
      input_max: AtomicI64::new(i64::MIN),
    }
  }
}

impl Timing {
  pub fn output(&self, ahead: i64) {
    self.cycles.fetch_add(1, Ordering::Relaxed);
    self.output_min.fetch_min(ahead, Ordering::Relaxed);
    self.output_max.fetch_max(ahead, Ordering::Relaxed);
  }

  pub fn input(&self, received: i64) {
    self.input_min.fetch_min(received, Ordering::Relaxed);
    self.input_max.fetch_max(received, Ordering::Relaxed);
  }

  /// What was recorded since last taken, if anything.
  pub fn take(&self) -> Option<String> {
    let cycles = self.cycles.swap(0, Ordering::Relaxed);
    let range = |min: &AtomicI64, max: &AtomicI64| {
      let (min, max) = (min.swap(i64::MAX, Ordering::Relaxed), max.swap(i64::MIN, Ordering::Relaxed));
      if min == i64::MAX {
        "-".to_string()
      } else {
        format!("{min}..{max}")
      }
    };
    let output = range(&self.output_min, &self.output_max);
    let input = range(&self.input_min, &self.input_max);
    (cycles > 0)
      .then(|| format!("{cycles} cycles, playback ahead {output} frames, input received ahead {input}"))
  }
}

pub struct Device {
  pub id: AudioObjectID,
  /// recording channels' stream
  pub input: Option<AudioObjectID>,
  /// playback channels' stream
  pub output: Option<AudioObjectID>,
  /// the card's index in the region
  pub card: usize,
  pub io: Io,
  /// the volume and mute controls, of the directions it has
  pub controls: Vec<(AudioObjectID, Direction, ControlKind)>,
  pub input_level: Level,
  pub output_level: Level,
}

impl Device {
  pub fn level(&self, direction: Direction) -> &Level {
    match direction {
      Direction::Input => &self.input_level,
      Direction::Output => &self.output_level,
    }
  }

  fn controls_in(&self, scope: u32) -> impl Iterator<Item = u32> + '_ {
    self.controls.iter().filter(move |(_, d, _)| in_scope(*d, scope)).map(|(id, _, _)| *id)
  }
}

/// Whether `direction` is in `scope` (the global scope has both).
fn in_scope(direction: Direction, scope: u32) -> bool {
  match direction {
    Direction::Input => scope != kAudioObjectPropertyScopeOutput,
    Direction::Output => scope != kAudioObjectPropertyScopeInput,
  }
}

/// The devices of a region.
pub struct DeviceSet {
  pub region: Arc<Region>,
  pub devices: Vec<Device>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
  Input,
  Output,
}

pub enum Object<'a> {
  PlugIn,
  Device(&'a Device),
  Stream(&'a Device, Direction),
  Control(&'a Device, Direction, ControlKind),
}

/// What a property query comes with, besides its address.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input<'a> {
  None,
  /// `kAudioPlugInPropertyTranslateUIDToDevice`'s qualifier
  Uid(&'a str),
  /// the value to convert (`kAudioLevelControlPropertyConvert...`)
  Value(f32),
}

/// A property's value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
  U32(u32),
  F32(f32),
  F64(f64),
  /// a CFString
  String(String),
  /// object ids, or other UInt32s
  U32s(Vec<u32>),
  Format(AudioStreamBasicDescription),
  Formats(Vec<AudioStreamRangedDescription>),
  Ranges(Vec<AudioValueRange>),
}

impl DeviceSet {
  /// Devices for the cards of `region`, with ids from `first_id` (`IDS_PER_CARD` per card): the device, its
  /// input and output streams, then the input's volume and mute, and the output's.
  pub fn new(region: Arc<Region>, first_id: AudioObjectID) -> Self {
    let devices = region
      .header()
      .cards()
      .iter()
      .enumerate()
      .map(|(card, info)| {
        let id = first_id + IDS_PER_CARD * card as u32;
        let (has_input, has_output) = (info.capture_channels > 0, info.playback_channels > 0);
        let mut controls = vec![];
        if has_input {
          controls.extend([
            (id + 3, Direction::Input, ControlKind::Volume),
            (id + 4, Direction::Input, ControlKind::Mute),
          ]);
        }
        if has_output {
          controls.extend([
            (id + 5, Direction::Output, ControlKind::Volume),
            (id + 6, Direction::Output, ControlKind::Mute),
          ]);
        }
        Device {
          id,
          input: has_input.then_some(id + 1),
          output: has_output.then_some(id + 2),
          card,
          io: Io::default(),
          controls,
          input_level: Level::default(),
          output_level: Level::default(),
        }
      })
      .collect();
    Self { region, devices }
  }

  /// The ids this set uses, from its first: the next set's first.
  pub fn next_id(&self) -> AudioObjectID {
    self.devices.last().map(|d| d.id + IDS_PER_CARD).unwrap_or(0)
  }

  pub fn device(&self, id: AudioObjectID) -> Option<&Device> {
    self.devices.iter().find(|d| d.id == id)
  }

  pub fn object(&self, id: AudioObjectID) -> Option<Object<'_>> {
    if id == kAudioObjectPlugInObject {
      return Some(Object::PlugIn);
    }
    self.devices.iter().find_map(|d| {
      if d.id == id {
        Some(Object::Device(d))
      } else if d.input == Some(id) {
        Some(Object::Stream(d, Direction::Input))
      } else if d.output == Some(id) {
        Some(Object::Stream(d, Direction::Output))
      } else {
        d.controls
          .iter()
          .find(|(c, _, _)| *c == id)
          .map(|&(_, direction, kind)| Object::Control(d, direction, kind))
      }
    })
  }

  pub fn device_ids(&self) -> Vec<u32> {
    self.devices.iter().map(|d| d.id).collect()
  }

  /// Channels of `device`'s stream in `direction`.
  pub fn channels(&self, device: &Device, direction: Direction) -> u32 {
    let card = &self.region.header().cards()[device.card];
    match direction {
      Direction::Input => card.capture_channels,
      Direction::Output => card.playback_channels,
    }
  }

  pub fn sample_rate(&self) -> f64 {
    self.region.header().sample_rate as f64
  }

  /// The one format of `device`'s stream in `direction`: interleaved 32-bit floats at the network's rate.
  pub fn format(&self, device: &Device, direction: Direction) -> AudioStreamBasicDescription {
    let channels = self.channels(device, direction);
    AudioStreamBasicDescription {
      mSampleRate: self.sample_rate(),
      mFormatID: kAudioFormatLinearPCM,
      mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
      mBytesPerPacket: 4 * channels,
      mFramesPerPacket: 1,
      mBytesPerFrame: 4 * channels,
      mChannelsPerFrame: channels,
      mBitsPerChannel: 32,
      mReserved: 0,
    }
  }

  /// All cards of a region follow one clock (Core Audio needn't resample between them, e.g. in an aggregate
  /// device).
  fn clock_domain(&self) -> u32 {
    let generation = self.region.header().generation;
    ((generation ^ (generation >> 32)) as u32) | 1
  }

  fn streams(device: &Device, scope: u32) -> Vec<u32> {
    let input = in_scope(Direction::Input, scope).then_some(device.input).flatten();
    let output = in_scope(Direction::Output, scope).then_some(device.output).flatten();
    input.into_iter().chain(output).collect()
  }
}

/// The properties of each class (whatever their scope).
fn has(object: &Object, selector: u32) -> bool {
  const COMMON: [u32; 4] = [
    kAudioObjectPropertyBaseClass,
    kAudioObjectPropertyClass,
    kAudioObjectPropertyOwner,
    kAudioObjectPropertyOwnedObjects,
  ];
  if COMMON.contains(&selector) {
    return true;
  }
  match object {
    Object::PlugIn => [
      kAudioObjectPropertyManufacturer,
      kAudioPlugInPropertyDeviceList,
      kAudioPlugInPropertyTranslateUIDToDevice,
      kAudioPlugInPropertyResourceBundle,
    ]
    .contains(&selector),
    Object::Device(_) => [
      kAudioObjectPropertyName,
      kAudioObjectPropertyManufacturer,
      kAudioDevicePropertyDeviceUID,
      kAudioDevicePropertyModelUID,
      kAudioDevicePropertyTransportType,
      kAudioDevicePropertyRelatedDevices,
      kAudioDevicePropertyClockDomain,
      kAudioDevicePropertyDeviceIsAlive,
      kAudioDevicePropertyDeviceIsRunning,
      kAudioDevicePropertyDeviceCanBeDefaultDevice,
      kAudioDevicePropertyDeviceCanBeDefaultSystemDevice,
      kAudioDevicePropertyLatency,
      kAudioDevicePropertyStreams,
      kAudioObjectPropertyControlList,
      kAudioDevicePropertySafetyOffset,
      kAudioDevicePropertyNominalSampleRate,
      kAudioDevicePropertyAvailableNominalSampleRates,
      kAudioDevicePropertyIsHidden,
      kAudioDevicePropertyPreferredChannelsForStereo,
      kAudioDevicePropertyZeroTimeStampPeriod,
    ]
    .contains(&selector),
    Object::Control(_, _, ControlKind::Volume) => [
      kAudioControlPropertyScope,
      kAudioControlPropertyElement,
      kAudioLevelControlPropertyScalarValue,
      kAudioLevelControlPropertyDecibelValue,
      kAudioLevelControlPropertyDecibelRange,
      kAudioLevelControlPropertyConvertScalarToDecibels,
      kAudioLevelControlPropertyConvertDecibelsToScalar,
    ]
    .contains(&selector),
    Object::Control(_, _, ControlKind::Mute) => {
      [kAudioControlPropertyScope, kAudioControlPropertyElement, kAudioBooleanControlPropertyValue]
        .contains(&selector)
    }
    Object::Stream(..) => [
      kAudioStreamPropertyIsActive,
      kAudioStreamPropertyDirection,
      kAudioStreamPropertyTerminalType,
      kAudioStreamPropertyStartingChannel,
      kAudioStreamPropertyLatency,
      kAudioStreamPropertyVirtualFormat,
      kAudioStreamPropertyAvailableVirtualFormats,
      kAudioStreamPropertyPhysicalFormat,
      kAudioStreamPropertyAvailablePhysicalFormats,
    ]
    .contains(&selector),
  }
}

/// The object `id` of `set` (or the plug-in, without devices).
fn object_of(set: Option<&DeviceSet>, id: AudioObjectID) -> Option<Object<'_>> {
  match set {
    _ if id == kAudioObjectPlugInObject => Some(Object::PlugIn),
    Some(set) => set.object(id),
    None => None,
  }
}

pub fn has_property(
  set: Option<&DeviceSet>,
  id: AudioObjectID,
  address: &AudioObjectPropertyAddress,
) -> bool {
  object_of(set, id).is_some_and(|object| has(&object, address.mSelector))
}

/// Whether the property can be set: none can (the network sets the rate, the formats are fixed).
pub fn is_settable(
  set: Option<&DeviceSet>,
  id: AudioObjectID,
  address: &AudioObjectPropertyAddress,
) -> Result<bool, OSStatus> {
  let object = object_of(set, id).ok_or(kAudioHardwareBadObjectError)?;
  if !has(&object, address.mSelector) {
    return Err(kAudioHardwareUnknownPropertyError);
  }
  Ok(matches!(object, Object::Control(..)) && SETTABLE.contains(&address.mSelector))
}

/// The properties that can be set to another value: the controls'.
const SETTABLE: [u32; 3] = [
  kAudioLevelControlPropertyScalarValue,
  kAudioLevelControlPropertyDecibelValue,
  kAudioBooleanControlPropertyValue,
];

/// The value of a property.
pub fn property(
  set: Option<&DeviceSet>,
  id: AudioObjectID,
  address: &AudioObjectPropertyAddress,
  input: Input,
) -> Result<Value, OSStatus> {
  let object = object_of(set, id).ok_or(kAudioHardwareBadObjectError)?;
  let selector = address.mSelector;
  let scope = address.mScope;
  let unknown = Err(kAudioHardwareUnknownPropertyError);
  if !has(&object, selector) {
    return unknown;
  }
  let device_ids = || set.map(|s| s.device_ids()).unwrap_or_default();
  Ok(match object {
    Object::PlugIn => match selector {
      kAudioObjectPropertyBaseClass => Value::U32(kAudioObjectClassID),
      kAudioObjectPropertyClass => Value::U32(kAudioPlugInClassID),
      kAudioObjectPropertyOwner => Value::U32(kAudioObjectUnknown),
      kAudioObjectPropertyManufacturer => Value::String(MANUFACTURER.into()),
      kAudioObjectPropertyOwnedObjects | kAudioPlugInPropertyDeviceList => Value::U32s(device_ids()),
      kAudioPlugInPropertyTranslateUIDToDevice => {
        let uid = match input {
          Input::Uid(uid) => Some(uid),
          _ => None,
        };
        let found = set.zip(uid).and_then(|(set, uid)| {
          let cards = set.region.header().cards();
          set.devices.iter().find(|d| device_uid(cards[d.card].uid()) == uid).map(|d| d.id)
        });
        Value::U32(found.unwrap_or(kAudioObjectUnknown))
      }
      kAudioPlugInPropertyResourceBundle => Value::String(String::new()),
      _ => return unknown,
    },
    Object::Device(device) => {
      // a device exists only with its set
      let set = set.unwrap();
      let header = set.region.header();
      let card = &header.cards()[device.card];
      let has_input = device.input.is_some();
      let has_output = device.output.is_some();
      let in_scope = match scope {
        kAudioObjectPropertyScopeInput => has_input,
        kAudioObjectPropertyScopeOutput => has_output,
        _ => true,
      };
      match selector {
        kAudioObjectPropertyBaseClass => Value::U32(kAudioObjectClassID),
        kAudioObjectPropertyClass => Value::U32(kAudioDeviceClassID),
        kAudioObjectPropertyOwner => Value::U32(kAudioObjectPlugInObject),
        kAudioObjectPropertyName => Value::String(card.name().into()),
        kAudioObjectPropertyManufacturer => Value::String(MANUFACTURER.into()),
        kAudioObjectPropertyOwnedObjects => Value::U32s(
          DeviceSet::streams(device, scope).into_iter().chain(device.controls_in(scope)).collect(),
        ),
        kAudioDevicePropertyStreams => Value::U32s(DeviceSet::streams(device, scope)),
        kAudioDevicePropertyDeviceUID => Value::String(device_uid(card.uid())),
        kAudioDevicePropertyModelUID => Value::String(MODEL_UID.into()),
        kAudioDevicePropertyTransportType => Value::U32(kAudioDeviceTransportTypeVirtual),
        kAudioDevicePropertyRelatedDevices => Value::U32s(vec![device.id]),
        kAudioDevicePropertyClockDomain => Value::U32(set.clock_domain()),
        kAudioDevicePropertyDeviceIsAlive => Value::U32(1),
        kAudioDevicePropertyDeviceIsRunning => {
          Value::U32((device.io.clients.load(Ordering::Relaxed) > 0) as u32)
        }
        kAudioDevicePropertyDeviceCanBeDefaultDevice
        | kAudioDevicePropertyDeviceCanBeDefaultSystemDevice => Value::U32(in_scope as u32),
        kAudioDevicePropertyLatency => Value::U32(match scope {
          kAudioObjectPropertyScopeInput => header.input_latency,
          kAudioObjectPropertyScopeOutput => header.output_latency + header.playback_lead,
          _ => 0,
        }),
        kAudioObjectPropertyControlList => Value::U32s(device.controls_in(scope).collect()),
        kAudioDevicePropertySafetyOffset => Value::U32(SAFETY_OFFSET),
        kAudioDevicePropertyNominalSampleRate => Value::F64(set.sample_rate()),
        kAudioDevicePropertyAvailableNominalSampleRates => Value::Ranges(vec![AudioValueRange {
          mMinimum: set.sample_rate(),
          mMaximum: set.sample_rate(),
        }]),
        kAudioDevicePropertyIsHidden => Value::U32(0),
        kAudioDevicePropertyPreferredChannelsForStereo => {
          let channels = match scope {
            kAudioObjectPropertyScopeInput => card.capture_channels,
            _ => card.playback_channels,
          };
          Value::U32s(if channels >= 2 { vec![1, 2] } else { vec![1, 1] })
        }
        kAudioDevicePropertyZeroTimeStampPeriod => Value::U32(ZERO_TIME_STAMP_PERIOD),
        _ => return unknown,
      }
    }
    Object::Control(device, direction, kind) => {
      let level = device.level(direction);
      let value_in = match input {
        Input::Value(value) => value,
        _ => 0.0,
      };
      match (kind, selector) {
        (ControlKind::Volume, kAudioObjectPropertyBaseClass) => Value::U32(kAudioLevelControlClassID),
        (ControlKind::Volume, kAudioObjectPropertyClass) => Value::U32(kAudioVolumeControlClassID),
        (ControlKind::Mute, kAudioObjectPropertyBaseClass) => Value::U32(kAudioBooleanControlClassID),
        (ControlKind::Mute, kAudioObjectPropertyClass) => Value::U32(kAudioMuteControlClassID),
        (_, kAudioObjectPropertyOwner) => Value::U32(device.id),
        (_, kAudioObjectPropertyOwnedObjects) => Value::U32s(vec![]),
        (_, kAudioControlPropertyScope) => Value::U32(match direction {
          Direction::Input => kAudioObjectPropertyScopeInput,
          Direction::Output => kAudioObjectPropertyScopeOutput,
        }),
        (_, kAudioControlPropertyElement) => Value::U32(kAudioObjectPropertyElementMain),
        (_, kAudioLevelControlPropertyScalarValue) => Value::F32(level.scalar()),
        (_, kAudioLevelControlPropertyDecibelValue) => Value::F32(scalar_to_db(level.scalar())),
        (_, kAudioLevelControlPropertyDecibelRange) => {
          Value::Ranges(vec![AudioValueRange { mMinimum: MIN_DB as f64, mMaximum: 0.0 }])
        }
        (_, kAudioLevelControlPropertyConvertScalarToDecibels) => Value::F32(scalar_to_db(value_in)),
        (_, kAudioLevelControlPropertyConvertDecibelsToScalar) => Value::F32(db_to_scalar(value_in)),
        (_, kAudioBooleanControlPropertyValue) => Value::U32(level.muted() as u32),
        _ => return unknown,
      }
    }
    Object::Stream(device, direction) => {
      let set = set.unwrap();
      let format = set.format(device, direction);
      let ranged = AudioStreamRangedDescription {
        mFormat: format,
        mSampleRateRange: AudioValueRange { mMinimum: format.mSampleRate, mMaximum: format.mSampleRate },
      };
      match selector {
        kAudioObjectPropertyBaseClass => Value::U32(kAudioObjectClassID),
        kAudioObjectPropertyClass => Value::U32(kAudioStreamClassID),
        kAudioObjectPropertyOwner => Value::U32(device.id),
        kAudioObjectPropertyOwnedObjects => Value::U32s(vec![]),
        kAudioStreamPropertyIsActive => Value::U32(1),
        kAudioStreamPropertyDirection => Value::U32((direction == Direction::Input) as u32),
        kAudioStreamPropertyTerminalType => Value::U32(kAudioStreamTerminalTypeLine),
        kAudioStreamPropertyStartingChannel => Value::U32(1),
        kAudioStreamPropertyLatency => Value::U32(0),
        kAudioStreamPropertyVirtualFormat | kAudioStreamPropertyPhysicalFormat => Value::Format(format),
        kAudioStreamPropertyAvailableVirtualFormats | kAudioStreamPropertyAvailablePhysicalFormats => {
          Value::Formats(vec![ranged])
        }
        _ => return unknown,
      }
    }
  })
}

/// Sets a property: a control's value, otherwise only to what it is (e.g. a client setting the format it read).
/// Returns the properties of the object that changed.
pub fn set_property(
  set: Option<&DeviceSet>,
  id: AudioObjectID,
  address: &AudioObjectPropertyAddress,
  value: &Value,
) -> Result<Vec<u32>, OSStatus> {
  if let Some(object @ Object::Control(device, direction, _)) = object_of(set, id) {
    if !has(&object, address.mSelector) {
      return Err(kAudioHardwareUnknownPropertyError);
    }
    let level = device.level(direction);
    return match (address.mSelector, value) {
      (kAudioLevelControlPropertyScalarValue, &Value::F32(scalar)) => {
        level.set_scalar(scalar);
        Ok(vec![kAudioLevelControlPropertyScalarValue, kAudioLevelControlPropertyDecibelValue])
      }
      (kAudioLevelControlPropertyDecibelValue, &Value::F32(db)) => {
        level.set_scalar(db_to_scalar(db));
        Ok(vec![kAudioLevelControlPropertyScalarValue, kAudioLevelControlPropertyDecibelValue])
      }
      (kAudioBooleanControlPropertyValue, &Value::U32(muted)) => {
        level.set_muted(muted != 0);
        Ok(vec![kAudioBooleanControlPropertyValue])
      }
      _ => Err(kAudioHardwareUnsupportedOperationError),
    };
  }
  let current = property(set, id, address, Input::None)?;
  if &current == value {
    return Ok(vec![]);
  }
  Err(match address.mSelector {
    kAudioStreamPropertyVirtualFormat | kAudioStreamPropertyPhysicalFormat => {
      kAudioDeviceUnsupportedFormatError
    }
    _ => kAudioHardwareUnsupportedOperationError,
  })
}

/// Whether `device` does the I/O operation `operation` (reading input, writing the mix).
pub fn will_do(device: &Device, operation: u32) -> bool {
  match operation {
    kAudioServerPlugInIOOperationReadInput => device.input.is_some(),
    kAudioServerPlugInIOOperationWriteMix => device.output.is_some(),
    _ => false,
  }
}

#[cfg(test)]
mod tests {
  use inferno_soundcard_shm::{RegionCard, RegionParams};

  use super::*;

  fn set() -> DeviceSet {
    let cards = [
      RegionCard {
        name: "Stage".into(),
        uid: "00ab-1".into(),
        playback_channels: 8,
        capture_channels: 2,
      },
      RegionCard {
        name: "Monitor".into(),
        uid: "00ab-2".into(),
        playback_channels: 2,
        capture_channels: 0,
      },
    ];
    let params = RegionParams {
      sample_rate: 48000,
      ring_frames: 8192,
      generation: 5,
      playback_lead: 480,
      input_latency: 480,
      output_latency: 240,
    };
    DeviceSet::new(Arc::new(Region::heap(&cards, params).unwrap()), 10)
  }

  fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
      mSelector: selector,
      mScope: scope,
      mElement: kAudioObjectPropertyElementMain,
    }
  }

  fn global(selector: u32) -> AudioObjectPropertyAddress {
    address(selector, kAudioObjectPropertyScopeGlobal)
  }

  #[test]
  fn a_device_per_card_with_its_streams() {
    let set = set();
    let set = Some(&set);
    let get = |id, a: AudioObjectPropertyAddress| property(set, id, &a, Input::None);
    assert_eq!(get(1, global(kAudioPlugInPropertyDeviceList)), Ok(Value::U32s(vec![10, 17])));
    assert_eq!(get(10, global(kAudioObjectPropertyName)), Ok(Value::String("Stage".into())));
    assert_eq!(
      get(10, global(kAudioDevicePropertyDeviceUID)),
      Ok(Value::String("inferno-00ab-1".into()))
    );
    assert_eq!(get(10, global(kAudioDevicePropertyStreams)), Ok(Value::U32s(vec![11, 12])));
    assert_eq!(
      get(10, address(kAudioDevicePropertyStreams, kAudioObjectPropertyScopeInput)),
      Ok(Value::U32s(vec![11]))
    );
    assert_eq!(
      get(17, global(kAudioDevicePropertyStreams)),
      Ok(Value::U32s(vec![19])),
      "no recording channels"
    );
    assert_eq!(get(18, global(kAudioObjectPropertyClass)), Err(kAudioHardwareBadObjectError));
    assert_eq!(get(11, global(kAudioStreamPropertyDirection)), Ok(Value::U32(1)), "input");
    assert_eq!(get(12, global(kAudioObjectPropertyOwner)), Ok(Value::U32(10)));
    let Ok(Value::Format(format)) = get(12, global(kAudioStreamPropertyPhysicalFormat)) else { panic!() };
    assert_eq!((format.mChannelsPerFrame, format.mBytesPerFrame, format.mSampleRate), (8, 32, 48000.0));
    let domain = |id| get(id, global(kAudioDevicePropertyClockDomain));
    assert_eq!(domain(10), domain(17), "one clock");
    assert_eq!(
      get(17, address(kAudioDevicePropertyDeviceCanBeDefaultDevice, kAudioObjectPropertyScopeInput)),
      Ok(Value::U32(0)),
      "not a default input without recording channels"
    );
    assert_eq!(
      get(10, address(kAudioDevicePropertyLatency, kAudioObjectPropertyScopeOutput)),
      Ok(Value::U32(720))
    );
    let translate =
      |uid| property(set, 1, &global(kAudioPlugInPropertyTranslateUIDToDevice), Input::Uid(uid));
    assert_eq!(translate("inferno-00ab-2"), Ok(Value::U32(17)));
    assert_eq!(translate("other"), Ok(Value::U32(kAudioObjectUnknown)));
  }

  #[test]
  fn without_the_service_there_are_no_devices() {
    assert_eq!(
      property(None, 1, &global(kAudioPlugInPropertyDeviceList), Input::None),
      Ok(Value::U32s(vec![]))
    );
    assert_eq!(
      property(None, 10, &global(kAudioObjectPropertyName), Input::None),
      Err(kAudioHardwareBadObjectError)
    );
    assert!(!has_property(None, 10, &global(kAudioObjectPropertyName)));
  }

  #[test]
  fn properties_are_set_only_to_what_they_are() {
    let set = set();
    let set = Some(&set);
    let rate = global(kAudioDevicePropertyNominalSampleRate);
    assert_eq!(set_property(set, 10, &rate, &Value::F64(48000.0)), Ok(vec![]));
    assert_eq!(
      set_property(set, 10, &rate, &Value::F64(44100.0)),
      Err(kAudioHardwareUnsupportedOperationError)
    );
    assert_eq!(is_settable(set, 10, &rate), Ok(false));
    assert_eq!(is_settable(set, 10, &global(fourcc(b"none"))), Err(kAudioHardwareUnknownPropertyError));
  }

  #[test]
  fn ids_of_the_next_set_follow() {
    assert_eq!(set().next_id(), 24);
  }

  #[test]
  fn volume_and_mute_controls_of_each_direction() {
    let set = set();
    let set_ref = Some(&set);
    let get = |id, a: AudioObjectPropertyAddress| property(set_ref, id, &a, Input::None);
    // Stage: input volume 13, input mute 14, output volume 15, output mute 16; Monitor: output only
    assert_eq!(get(10, global(kAudioObjectPropertyControlList)), Ok(Value::U32s(vec![13, 14, 15, 16])));
    assert_eq!(
      get(10, address(kAudioObjectPropertyOwnedObjects, kAudioObjectPropertyScopeOutput)),
      Ok(Value::U32s(vec![12, 15, 16]))
    );
    assert_eq!(get(17, global(kAudioObjectPropertyControlList)), Ok(Value::U32s(vec![22, 23])));
    assert_eq!(get(15, global(kAudioObjectPropertyClass)), Ok(Value::U32(kAudioVolumeControlClassID)));
    assert_eq!(get(15, global(kAudioObjectPropertyBaseClass)), Ok(Value::U32(kAudioLevelControlClassID)));
    assert_eq!(get(16, global(kAudioObjectPropertyClass)), Ok(Value::U32(kAudioMuteControlClassID)));
    assert_eq!(
      get(13, global(kAudioControlPropertyScope)),
      Ok(Value::U32(kAudioObjectPropertyScopeInput))
    );
    assert_eq!(
      get(15, global(kAudioLevelControlPropertyScalarValue)),
      Ok(Value::F32(1.0)),
      "full volume"
    );
    assert_eq!(get(15, global(kAudioLevelControlPropertyDecibelValue)), Ok(Value::F32(0.0)));
    assert_eq!(set.devices[0].output_level.gain(), 1.0, "audio unchanged");
    assert_eq!(is_settable(set_ref, 15, &global(kAudioLevelControlPropertyScalarValue)), Ok(true));

    let scalar = global(kAudioLevelControlPropertyScalarValue);
    assert_eq!(
      set_property(set_ref, 15, &scalar, &Value::F32(0.5)),
      Ok(vec![kAudioLevelControlPropertyScalarValue, kAudioLevelControlPropertyDecibelValue])
    );
    assert_eq!(set.devices[0].output_level.gain(), 0.25, "the square of the slider");
    let Ok(Value::F32(db)) = get(15, global(kAudioLevelControlPropertyDecibelValue)) else { panic!() };
    assert!((db - -12.04).abs() < 0.01, "{db} dB");
    assert_eq!(set.devices[0].input_level.gain(), 1.0, "the input's own");
    set_property(set_ref, 15, &global(kAudioLevelControlPropertyDecibelValue), &Value::F32(-6.0))
      .unwrap();
    assert!((set.devices[0].output_level.gain() - 0.501).abs() < 0.001, "-6 dB");
    set_property(set_ref, 15, &scalar, &Value::F32(7.0)).unwrap();
    assert_eq!(set.devices[0].output_level.scalar(), 1.0, "clipped to full");

    set_property(set_ref, 16, &global(kAudioBooleanControlPropertyValue), &Value::U32(1)).unwrap();
    assert_eq!(get(16, global(kAudioBooleanControlPropertyValue)), Ok(Value::U32(1)));
    assert_eq!(set.devices[0].output_level.gain(), 0.0, "muted");
    assert_eq!(
      set_property(set_ref, 16, &scalar, &Value::F32(0.5)),
      Err(kAudioHardwareUnknownPropertyError)
    );

    let convert = |selector, value| property(set_ref, 15, &global(selector), Input::Value(value));
    assert_eq!(convert(kAudioLevelControlPropertyConvertScalarToDecibels, 0.0), Ok(Value::F32(MIN_DB)));
    assert_eq!(convert(kAudioLevelControlPropertyConvertDecibelsToScalar, MIN_DB), Ok(Value::F32(0.0)));
    assert_eq!(convert(kAudioLevelControlPropertyConvertDecibelsToScalar, 0.0), Ok(Value::F32(1.0)));
  }

  #[test]
  fn decibels_and_slider_positions_convert_both_ways() {
    // the slider's bottom, MIN_DB, is at about 0.025: silence below
    assert_eq!(db_to_scalar(scalar_to_db(0.01)), 0.0);
    for scalar in [0.03f32, 0.1, 0.3, 0.5, 0.9, 1.0] {
      let back = db_to_scalar(scalar_to_db(scalar));
      assert!((back - scalar).abs() < 1e-4, "{scalar}: {back}");
    }
    assert_eq!(
      storage_key("00ab-1", Direction::Output, ControlKind::Volume),
      "inferno-00ab-1.output.volume"
    );
  }
}
