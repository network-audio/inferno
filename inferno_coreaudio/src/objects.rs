//! The plug-in's Core Audio objects and their properties, independent of Core Audio (and so testable anywhere).
//!
//! The plug-in object owns a device per card of the service's region, each with an input stream (the card's
//! recording channels) and an output stream (its playback channels), if it has those channels. Their object
//! ids are new whenever the cards are (a new region): the old ones are gone.

#![allow(non_upper_case_globals)] // Core Audio's names

use std::sync::{
  atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering},
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
}

/// A property's value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
  U32(u32),
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
  /// Devices for the cards of `region`, with ids from `first_id` (3 per card).
  pub fn new(region: Arc<Region>, first_id: AudioObjectID) -> Self {
    let devices = region
      .header()
      .cards()
      .iter()
      .enumerate()
      .map(|(card, info)| {
        let id = first_id + 3 * card as u32;
        Device {
          id,
          input: (info.capture_channels > 0).then_some(id + 1),
          output: (info.playback_channels > 0).then_some(id + 2),
          card,
          io: Io::default(),
        }
      })
      .collect();
    Self { region, devices }
  }

  /// The ids this set uses, from its first: the next set's first.
  pub fn next_id(&self) -> AudioObjectID {
    self.devices.last().map(|d| d.id + 3).unwrap_or(0)
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
        None
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
    let input = (scope != kAudioObjectPropertyScopeOutput).then_some(device.input).flatten();
    let output = (scope != kAudioObjectPropertyScopeInput).then_some(device.output).flatten();
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
  Ok(false)
}

/// The value of a property. `uid`: the qualifier of `kAudioPlugInPropertyTranslateUIDToDevice`.
pub fn property(
  set: Option<&DeviceSet>,
  id: AudioObjectID,
  address: &AudioObjectPropertyAddress,
  uid: Option<&str>,
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
        kAudioObjectPropertyOwnedObjects | kAudioDevicePropertyStreams => {
          Value::U32s(DeviceSet::streams(device, scope))
        }
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
        kAudioObjectPropertyControlList => Value::U32s(vec![]),
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

/// Sets a property: only to what it is (e.g. a client setting the format it read).
pub fn set_property(
  set: Option<&DeviceSet>,
  id: AudioObjectID,
  address: &AudioObjectPropertyAddress,
  value: &Value,
) -> Result<(), OSStatus> {
  let current = property(set, id, address, None)?;
  if &current == value {
    return Ok(());
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
    let get = |id, a: AudioObjectPropertyAddress| property(set, id, &a, None);
    assert_eq!(get(1, global(kAudioPlugInPropertyDeviceList)), Ok(Value::U32s(vec![10, 13])));
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
      get(13, global(kAudioDevicePropertyStreams)),
      Ok(Value::U32s(vec![15])),
      "no recording channels"
    );
    assert_eq!(get(14, global(kAudioObjectPropertyClass)), Err(kAudioHardwareBadObjectError));
    assert_eq!(get(11, global(kAudioStreamPropertyDirection)), Ok(Value::U32(1)), "input");
    assert_eq!(get(12, global(kAudioObjectPropertyOwner)), Ok(Value::U32(10)));
    let Ok(Value::Format(format)) = get(12, global(kAudioStreamPropertyPhysicalFormat)) else { panic!() };
    assert_eq!((format.mChannelsPerFrame, format.mBytesPerFrame, format.mSampleRate), (8, 32, 48000.0));
    let domain = |id| get(id, global(kAudioDevicePropertyClockDomain));
    assert_eq!(domain(10), domain(13), "one clock");
    assert_eq!(
      get(13, address(kAudioDevicePropertyDeviceCanBeDefaultDevice, kAudioObjectPropertyScopeInput)),
      Ok(Value::U32(0)),
      "not a default input without recording channels"
    );
    assert_eq!(
      get(10, address(kAudioDevicePropertyLatency, kAudioObjectPropertyScopeOutput)),
      Ok(Value::U32(720))
    );
    let translate = |uid| property(set, 1, &global(kAudioPlugInPropertyTranslateUIDToDevice), Some(uid));
    assert_eq!(translate("inferno-00ab-2"), Ok(Value::U32(13)));
    assert_eq!(translate("other"), Ok(Value::U32(kAudioObjectUnknown)));
  }

  #[test]
  fn without_the_service_there_are_no_devices() {
    assert_eq!(property(None, 1, &global(kAudioPlugInPropertyDeviceList), None), Ok(Value::U32s(vec![])));
    assert_eq!(
      property(None, 10, &global(kAudioObjectPropertyName), None),
      Err(kAudioHardwareBadObjectError)
    );
    assert!(!has_property(None, 10, &global(kAudioObjectPropertyName)));
  }

  #[test]
  fn properties_are_set_only_to_what_they_are() {
    let set = set();
    let set = Some(&set);
    let rate = global(kAudioDevicePropertyNominalSampleRate);
    assert_eq!(set_property(set, 10, &rate, &Value::F64(48000.0)), Ok(()));
    assert_eq!(
      set_property(set, 10, &rate, &Value::F64(44100.0)),
      Err(kAudioHardwareUnsupportedOperationError)
    );
    assert_eq!(is_settable(set, 10, &rate), Ok(false));
    assert_eq!(is_settable(set, 10, &global(fourcc(b"none"))), Err(kAudioHardwareUnknownPropertyError));
  }

  #[test]
  fn ids_of_the_next_set_follow() {
    assert_eq!(set().next_id(), 16);
  }
}
