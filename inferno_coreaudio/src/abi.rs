//! The parts of Core Audio's AudioServerPlugIn interface (`CoreAudio/AudioServerPlugIn.h`,
//! `AudioHardwareBase.h`) and of Core Foundation the plug-in uses.

#![allow(non_upper_case_globals, non_snake_case, non_camel_case_types, dead_code)]

use std::ffi::c_void;

pub type OSStatus = i32;
pub type AudioObjectID = u32;
pub type HRESULT = i32;
pub type ULONG = u32;
pub type Boolean = u8;
pub type pid_t = i32;

/// A four-character code.
pub const fn fourcc(code: &[u8; 4]) -> u32 {
  u32::from_be_bytes(*code)
}

// errors
pub const kAudioHardwareNoError: OSStatus = 0;
pub const kAudioHardwareNotRunningError: OSStatus = fourcc(b"stop") as i32;
pub const kAudioHardwareUnspecifiedError: OSStatus = fourcc(b"what") as i32;
pub const kAudioHardwareUnknownPropertyError: OSStatus = fourcc(b"who?") as i32;
pub const kAudioHardwareBadPropertySizeError: OSStatus = fourcc(b"!siz") as i32;
pub const kAudioHardwareIllegalOperationError: OSStatus = fourcc(b"nope") as i32;
pub const kAudioHardwareBadObjectError: OSStatus = fourcc(b"!obj") as i32;
pub const kAudioHardwareBadDeviceError: OSStatus = fourcc(b"!dev") as i32;
pub const kAudioHardwareBadStreamError: OSStatus = fourcc(b"!str") as i32;
pub const kAudioHardwareUnsupportedOperationError: OSStatus = fourcc(b"unop") as i32;
pub const kAudioDeviceUnsupportedFormatError: OSStatus = fourcc(b"!dat") as i32;
pub const S_OK: HRESULT = 0;
pub const E_NOINTERFACE: HRESULT = 0x8000_0004_u32 as i32;

// objects and classes
pub const kAudioObjectUnknown: AudioObjectID = 0;
pub const kAudioObjectPlugInObject: AudioObjectID = 1;
pub const kAudioObjectClassID: u32 = fourcc(b"aobj");
pub const kAudioPlugInClassID: u32 = fourcc(b"aplg");
pub const kAudioDeviceClassID: u32 = fourcc(b"adev");
pub const kAudioStreamClassID: u32 = fourcc(b"astr");
pub const kAudioControlClassID: u32 = fourcc(b"actl");
pub const kAudioLevelControlClassID: u32 = fourcc(b"levl");
pub const kAudioBooleanControlClassID: u32 = fourcc(b"togl");
pub const kAudioVolumeControlClassID: u32 = fourcc(b"vlme");
pub const kAudioMuteControlClassID: u32 = fourcc(b"mute");

// scopes, elements
pub const kAudioObjectPropertyScopeGlobal: u32 = fourcc(b"glob");
pub const kAudioObjectPropertyScopeInput: u32 = fourcc(b"inpt");
pub const kAudioObjectPropertyScopeOutput: u32 = fourcc(b"outp");
pub const kAudioObjectPropertyElementMain: u32 = 0;

// properties of all objects
pub const kAudioObjectPropertyBaseClass: u32 = fourcc(b"bcls");
pub const kAudioObjectPropertyClass: u32 = fourcc(b"clas");
pub const kAudioObjectPropertyOwner: u32 = fourcc(b"stdv");
pub const kAudioObjectPropertyName: u32 = fourcc(b"lnam");
pub const kAudioObjectPropertyManufacturer: u32 = fourcc(b"lmak");
pub const kAudioObjectPropertyOwnedObjects: u32 = fourcc(b"ownd");
pub const kAudioObjectPropertyControlList: u32 = fourcc(b"ctrl");

// the plug-in's
pub const kAudioPlugInPropertyDeviceList: u32 = fourcc(b"dev#");
pub const kAudioPlugInPropertyTranslateUIDToDevice: u32 = fourcc(b"uidd");
pub const kAudioPlugInPropertyResourceBundle: u32 = fourcc(b"rsrc");

// devices'
pub const kAudioDevicePropertyDeviceUID: u32 = fourcc(b"uid ");
pub const kAudioDevicePropertyModelUID: u32 = fourcc(b"muid");
pub const kAudioDevicePropertyTransportType: u32 = fourcc(b"tran");
pub const kAudioDevicePropertyRelatedDevices: u32 = fourcc(b"akin");
pub const kAudioDevicePropertyClockDomain: u32 = fourcc(b"clkd");
pub const kAudioDevicePropertyDeviceIsAlive: u32 = fourcc(b"livn");
pub const kAudioDevicePropertyDeviceIsRunning: u32 = fourcc(b"goin");
pub const kAudioDevicePropertyDeviceCanBeDefaultDevice: u32 = fourcc(b"dflt");
pub const kAudioDevicePropertyDeviceCanBeDefaultSystemDevice: u32 = fourcc(b"sflt");
pub const kAudioDevicePropertyLatency: u32 = fourcc(b"ltnc");
pub const kAudioDevicePropertyStreams: u32 = fourcc(b"stm#");
pub const kAudioDevicePropertySafetyOffset: u32 = fourcc(b"saft");
pub const kAudioDevicePropertyNominalSampleRate: u32 = fourcc(b"nsrt");
pub const kAudioDevicePropertyAvailableNominalSampleRates: u32 = fourcc(b"nsr#");
pub const kAudioDevicePropertyIsHidden: u32 = fourcc(b"hidn");
pub const kAudioDevicePropertyPreferredChannelsForStereo: u32 = fourcc(b"dch2");
pub const kAudioDevicePropertyZeroTimeStampPeriod: u32 = fourcc(b"ring");
pub const kAudioDeviceTransportTypeVirtual: u32 = fourcc(b"virt");

// streams'
pub const kAudioStreamPropertyIsActive: u32 = fourcc(b"sact");
pub const kAudioStreamPropertyDirection: u32 = fourcc(b"sdir");
pub const kAudioStreamPropertyTerminalType: u32 = fourcc(b"term");
pub const kAudioStreamPropertyStartingChannel: u32 = fourcc(b"schn");
pub const kAudioStreamPropertyLatency: u32 = kAudioDevicePropertyLatency;
pub const kAudioStreamPropertyVirtualFormat: u32 = fourcc(b"sfmt");
pub const kAudioStreamPropertyAvailableVirtualFormats: u32 = fourcc(b"sfma");
pub const kAudioStreamPropertyPhysicalFormat: u32 = fourcc(b"pft ");
pub const kAudioStreamPropertyAvailablePhysicalFormats: u32 = fourcc(b"pfta");
pub const kAudioStreamTerminalTypeLine: u32 = fourcc(b"line");

// controls
pub const kAudioControlPropertyScope: u32 = fourcc(b"cscp");
pub const kAudioControlPropertyElement: u32 = fourcc(b"celm");
pub const kAudioLevelControlPropertyScalarValue: u32 = fourcc(b"lcsv");
pub const kAudioLevelControlPropertyDecibelValue: u32 = fourcc(b"lcdv");
pub const kAudioLevelControlPropertyDecibelRange: u32 = fourcc(b"lcdr");
pub const kAudioLevelControlPropertyConvertScalarToDecibels: u32 = fourcc(b"lcsd");
pub const kAudioLevelControlPropertyConvertDecibelsToScalar: u32 = fourcc(b"lcds");
pub const kAudioBooleanControlPropertyValue: u32 = fourcc(b"bcvl");

// formats
pub const kAudioFormatLinearPCM: u32 = fourcc(b"lpcm");
pub const kAudioFormatFlagIsFloat: u32 = 1 << 0;
pub const kAudioFormatFlagIsPacked: u32 = 1 << 3;

// clients' (the end-to-end test's application)
pub const kAudioObjectSystemObject: AudioObjectID = 1;
pub const kAudioHardwarePropertyDevices: u32 = fourcc(b"dev#");
pub const kAudioDevicePropertyStreamConfiguration: u32 = fourcc(b"slay");
pub const kAudioDevicePropertyBufferFrameSize: u32 = fourcc(b"fsiz");
pub const kAudioDevicePropertyVolumeScalar: u32 = fourcc(b"volm");
pub const kAudioDevicePropertyMute: u32 = fourcc(b"mute");

// I/O operations
pub const kAudioServerPlugInIOOperationReadInput: u32 = fourcc(b"read");
pub const kAudioServerPlugInIOOperationWriteMix: u32 = fourcc(b"rite");

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioObjectPropertyAddress {
  pub mSelector: u32,
  pub mScope: u32,
  pub mElement: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioValueRange {
  pub mMinimum: f64,
  pub mMaximum: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioStreamBasicDescription {
  pub mSampleRate: f64,
  pub mFormatID: u32,
  pub mFormatFlags: u32,
  pub mBytesPerPacket: u32,
  pub mFramesPerPacket: u32,
  pub mBytesPerFrame: u32,
  pub mChannelsPerFrame: u32,
  pub mBitsPerChannel: u32,
  pub mReserved: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioStreamRangedDescription {
  pub mFormat: AudioStreamBasicDescription,
  pub mSampleRateRange: AudioValueRange,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SMPTETime {
  pub mSubframes: i16,
  pub mSubframeDivisor: i16,
  pub mCounter: u32,
  pub mType: u32,
  pub mFlags: u32,
  pub mHours: i16,
  pub mMinutes: i16,
  pub mSeconds: i16,
  pub mFrames: i16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AudioTimeStamp {
  pub mSampleTime: f64,
  pub mHostTime: u64,
  pub mRateScalar: f64,
  pub mWordClockTime: u64,
  pub mSMPTETime: SMPTETime,
  pub mFlags: u32,
  pub mReserved: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AudioServerPlugInIOCycleInfo {
  pub mIOCycleCounter: u64,
  pub mNominalIOBufferFrameSize: u32,
  pub mCurrentTime: AudioTimeStamp,
  pub mInputTime: AudioTimeStamp,
  pub mOutputTime: AudioTimeStamp,
  pub mMainHostTicksPerFrame: f64,
  pub mDeviceHostTicksPerFrame: f64,
}

/// `CFUUIDBytes`, as COM's `REFIID`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CFUUIDBytes(pub [u8; 16]);

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFAllocatorRef = *const c_void;
pub type CFUUIDRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFIndex = isize;
pub type CFNumberRef = *const c_void;
pub type CFPropertyListRef = *const c_void;
pub const kCFNumberFloat32Type: CFIndex = 5;
pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;

pub type DriverRef = *mut *const AudioServerPlugInDriverInterface;
pub type HostRef = *const AudioServerPlugInHostInterface;

#[repr(C)]
pub struct AudioServerPlugInHostInterface {
  pub PropertiesChanged: unsafe extern "C" fn(
    host: HostRef,
    object: AudioObjectID,
    count: u32,
    addresses: *const AudioObjectPropertyAddress,
  ) -> OSStatus,
  pub CopyFromStorage:
    unsafe extern "C" fn(host: HostRef, key: CFStringRef, out: *mut CFPropertyListRef) -> OSStatus,
  pub WriteToStorage:
    unsafe extern "C" fn(host: HostRef, key: CFStringRef, data: CFPropertyListRef) -> OSStatus,
  pub DeleteFromStorage: *const c_void,
  pub RequestDeviceConfigurationChange: *const c_void,
}

#[repr(C)]
pub struct AudioServerPlugInDriverInterface {
  pub _reserved: *const c_void,
  pub QueryInterface: unsafe extern "C" fn(*mut c_void, CFUUIDBytes, *mut *mut c_void) -> HRESULT,
  pub AddRef: unsafe extern "C" fn(*mut c_void) -> ULONG,
  pub Release: unsafe extern "C" fn(*mut c_void) -> ULONG,
  pub Initialize: unsafe extern "C" fn(DriverRef, HostRef) -> OSStatus,
  pub CreateDevice:
    unsafe extern "C" fn(DriverRef, CFDictionaryRef, *const c_void, *mut AudioObjectID) -> OSStatus,
  pub DestroyDevice: unsafe extern "C" fn(DriverRef, AudioObjectID) -> OSStatus,
  pub AddDeviceClient: unsafe extern "C" fn(DriverRef, AudioObjectID, *const c_void) -> OSStatus,
  pub RemoveDeviceClient: unsafe extern "C" fn(DriverRef, AudioObjectID, *const c_void) -> OSStatus,
  pub PerformDeviceConfigurationChange:
    unsafe extern "C" fn(DriverRef, AudioObjectID, u64, *mut c_void) -> OSStatus,
  pub AbortDeviceConfigurationChange:
    unsafe extern "C" fn(DriverRef, AudioObjectID, u64, *mut c_void) -> OSStatus,
  pub HasProperty:
    unsafe extern "C" fn(DriverRef, AudioObjectID, pid_t, *const AudioObjectPropertyAddress) -> Boolean,
  pub IsPropertySettable: unsafe extern "C" fn(
    DriverRef,
    AudioObjectID,
    pid_t,
    *const AudioObjectPropertyAddress,
    *mut Boolean,
  ) -> OSStatus,
  pub GetPropertyDataSize: unsafe extern "C" fn(
    DriverRef,
    AudioObjectID,
    pid_t,
    *const AudioObjectPropertyAddress,
    u32,
    *const c_void,
    *mut u32,
  ) -> OSStatus,
  pub GetPropertyData: unsafe extern "C" fn(
    DriverRef,
    AudioObjectID,
    pid_t,
    *const AudioObjectPropertyAddress,
    u32,
    *const c_void,
    u32,
    *mut u32,
    *mut c_void,
  ) -> OSStatus,
  pub SetPropertyData: unsafe extern "C" fn(
    DriverRef,
    AudioObjectID,
    pid_t,
    *const AudioObjectPropertyAddress,
    u32,
    *const c_void,
    u32,
    *const c_void,
  ) -> OSStatus,
  pub StartIO: unsafe extern "C" fn(DriverRef, AudioObjectID, u32) -> OSStatus,
  pub StopIO: unsafe extern "C" fn(DriverRef, AudioObjectID, u32) -> OSStatus,
  pub GetZeroTimeStamp:
    unsafe extern "C" fn(DriverRef, AudioObjectID, u32, *mut f64, *mut u64, *mut u64) -> OSStatus,
  pub WillDoIOOperation:
    unsafe extern "C" fn(DriverRef, AudioObjectID, u32, u32, *mut Boolean, *mut Boolean) -> OSStatus,
  pub BeginIOOperation: unsafe extern "C" fn(
    DriverRef,
    AudioObjectID,
    u32,
    u32,
    u32,
    *const AudioServerPlugInIOCycleInfo,
  ) -> OSStatus,
  pub DoIOOperation: unsafe extern "C" fn(
    DriverRef,
    AudioObjectID,
    AudioObjectID,
    u32,
    u32,
    u32,
    *const AudioServerPlugInIOCycleInfo,
    *mut c_void,
    *mut c_void,
  ) -> OSStatus,
  pub EndIOOperation: unsafe extern "C" fn(
    DriverRef,
    AudioObjectID,
    u32,
    u32,
    u32,
    *const AudioServerPlugInIOCycleInfo,
  ) -> OSStatus,
}

// safety: the interface is immutable (its reserved pointer is null)
unsafe impl Sync for AudioServerPlugInDriverInterface {}

/// `kAudioServerPlugInTypeUUID`: 443ABAB8-E7B3-491A-B985-BEB9187030DB
pub const PLUGIN_TYPE_UUID: [u8; 16] =
  [0x44, 0x3A, 0xBA, 0xB8, 0xE7, 0xB3, 0x49, 0x1A, 0xB9, 0x85, 0xBE, 0xB9, 0x18, 0x70, 0x30, 0xDB];
/// `kAudioServerPlugInDriverInterfaceUUID`: EEA5773D-CC43-49F1-8E00-8F96E7D23B17
pub const DRIVER_INTERFACE_UUID: [u8; 16] =
  [0xEE, 0xA5, 0x77, 0x3D, 0xCC, 0x43, 0x49, 0xF1, 0x8E, 0x00, 0x8F, 0x96, 0xE7, 0xD2, 0x3B, 0x17];
/// `IUnknownUUID`: 00000000-0000-0000-C000-000000000046
pub const IUNKNOWN_UUID: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46];

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
  pub fn CFStringCreateWithBytes(
    alloc: CFAllocatorRef,
    bytes: *const u8,
    length: CFIndex,
    encoding: u32,
    is_external: Boolean,
  ) -> CFStringRef;
  pub fn CFStringGetLength(s: CFStringRef) -> CFIndex;
  pub fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
  pub fn CFStringGetCString(s: CFStringRef, buffer: *mut u8, size: CFIndex, encoding: u32) -> Boolean;
  pub fn CFUUIDGetUUIDBytes(uuid: CFUUIDRef) -> CFUUIDBytes;
  pub fn CFRelease(cf: CFTypeRef);
  pub fn CFGetTypeID(cf: CFTypeRef) -> usize;
  pub fn CFNumberGetTypeID() -> usize;
  pub fn CFNumberCreate(alloc: CFAllocatorRef, kind: CFIndex, value: *const c_void) -> CFNumberRef;
  pub fn CFNumberGetValue(number: CFNumberRef, kind: CFIndex, value: *mut c_void) -> Boolean;
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn layouts_match_core_audio() {
    assert_eq!(size_of::<AudioStreamBasicDescription>(), 40);
    assert_eq!(size_of::<AudioStreamRangedDescription>(), 56);
    assert_eq!(size_of::<AudioTimeStamp>(), 64);
    assert_eq!(size_of::<AudioServerPlugInIOCycleInfo>(), 224);
    assert_eq!(std::mem::offset_of!(AudioServerPlugInIOCycleInfo, mInputTime), 80);
    assert_eq!(std::mem::offset_of!(AudioServerPlugInIOCycleInfo, mOutputTime), 144);
    assert_eq!(size_of::<AudioServerPlugInDriverInterface>(), 23 * size_of::<usize>());
    assert_eq!(fourcc(b"aobj"), 0x616f626a);
  }
}
