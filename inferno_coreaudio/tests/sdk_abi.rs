//! The plug-in's Core Audio definitions (`inferno_coreaudio::abi`) against the macOS SDK's headers: a C program
//! compiled with them prints the constants, sizes and offsets, which must be the same.

#![cfg(target_os = "macos")]
#![allow(non_upper_case_globals)]

use std::{collections::BTreeMap, mem::offset_of, process::Command};

use inferno_coreaudio::abi::*;

macro_rules! constants {
  ($($name:ident),* $(,)?) => {
    vec![$((stringify!($name), $name as i64 as u64)),*]
  };
}

/// What the C program prints (C expressions), with the Rust value of each.
fn expected() -> Vec<(&'static str, u64)> {
  let mut values = constants![
    kAudioHardwareNoError,
    kAudioHardwareNotRunningError,
    kAudioHardwareUnspecifiedError,
    kAudioHardwareUnknownPropertyError,
    kAudioHardwareBadPropertySizeError,
    kAudioHardwareIllegalOperationError,
    kAudioHardwareBadObjectError,
    kAudioHardwareBadDeviceError,
    kAudioHardwareBadStreamError,
    kAudioHardwareUnsupportedOperationError,
    kAudioDeviceUnsupportedFormatError,
    kAudioObjectUnknown,
    kAudioObjectPlugInObject,
    kAudioObjectClassID,
    kAudioPlugInClassID,
    kAudioDeviceClassID,
    kAudioStreamClassID,
    kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput,
    kAudioObjectPropertyScopeOutput,
    kAudioObjectPropertyElementMain,
    kAudioObjectPropertyBaseClass,
    kAudioObjectPropertyClass,
    kAudioObjectPropertyOwner,
    kAudioObjectPropertyName,
    kAudioObjectPropertyManufacturer,
    kAudioObjectPropertyOwnedObjects,
    kAudioObjectPropertyControlList,
    kAudioPlugInPropertyDeviceList,
    kAudioPlugInPropertyTranslateUIDToDevice,
    kAudioPlugInPropertyResourceBundle,
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
    kAudioDevicePropertySafetyOffset,
    kAudioDevicePropertyNominalSampleRate,
    kAudioDevicePropertyAvailableNominalSampleRates,
    kAudioDevicePropertyIsHidden,
    kAudioDevicePropertyPreferredChannelsForStereo,
    kAudioDevicePropertyZeroTimeStampPeriod,
    kAudioDeviceTransportTypeVirtual,
    kAudioStreamPropertyIsActive,
    kAudioStreamPropertyDirection,
    kAudioStreamPropertyTerminalType,
    kAudioStreamPropertyStartingChannel,
    kAudioStreamPropertyLatency,
    kAudioStreamPropertyVirtualFormat,
    kAudioStreamPropertyAvailableVirtualFormats,
    kAudioStreamPropertyPhysicalFormat,
    kAudioStreamPropertyAvailablePhysicalFormats,
    kAudioStreamTerminalTypeLine,
    kAudioFormatLinearPCM,
    kAudioFormatFlagIsFloat,
    kAudioFormatFlagIsPacked,
    kAudioObjectSystemObject,
    kAudioHardwarePropertyDevices,
    kAudioDevicePropertyStreamConfiguration,
    kAudioDevicePropertyBufferFrameSize,
    kAudioServerPlugInIOOperationReadInput,
    kAudioServerPlugInIOOperationWriteMix,
    kCFStringEncodingUTF8,
  ];
  let layouts = [
    ("sizeof(AudioObjectPropertyAddress)", size_of::<AudioObjectPropertyAddress>()),
    ("sizeof(AudioValueRange)", size_of::<AudioValueRange>()),
    ("sizeof(AudioStreamBasicDescription)", size_of::<AudioStreamBasicDescription>()),
    ("sizeof(AudioStreamRangedDescription)", size_of::<AudioStreamRangedDescription>()),
    ("sizeof(AudioTimeStamp)", size_of::<AudioTimeStamp>()),
    ("offsetof(AudioTimeStamp, mSMPTETime)", offset_of!(AudioTimeStamp, mSMPTETime)),
    ("offsetof(AudioTimeStamp, mFlags)", offset_of!(AudioTimeStamp, mFlags)),
    ("sizeof(AudioServerPlugInIOCycleInfo)", size_of::<AudioServerPlugInIOCycleInfo>()),
    (
      "offsetof(AudioServerPlugInIOCycleInfo, mCurrentTime)",
      offset_of!(AudioServerPlugInIOCycleInfo, mCurrentTime),
    ),
    (
      "offsetof(AudioServerPlugInIOCycleInfo, mInputTime)",
      offset_of!(AudioServerPlugInIOCycleInfo, mInputTime),
    ),
    (
      "offsetof(AudioServerPlugInIOCycleInfo, mOutputTime)",
      offset_of!(AudioServerPlugInIOCycleInfo, mOutputTime),
    ),
    ("sizeof(AudioServerPlugInHostInterface)", size_of::<AudioServerPlugInHostInterface>()),
    ("sizeof(AudioServerPlugInDriverInterface)", size_of::<AudioServerPlugInDriverInterface>()),
    (
      "offsetof(AudioServerPlugInDriverInterface, Initialize)",
      offset_of!(AudioServerPlugInDriverInterface, Initialize),
    ),
    (
      "offsetof(AudioServerPlugInDriverInterface, HasProperty)",
      offset_of!(AudioServerPlugInDriverInterface, HasProperty),
    ),
    (
      "offsetof(AudioServerPlugInDriverInterface, GetZeroTimeStamp)",
      offset_of!(AudioServerPlugInDriverInterface, GetZeroTimeStamp),
    ),
    (
      "offsetof(AudioServerPlugInDriverInterface, DoIOOperation)",
      offset_of!(AudioServerPlugInDriverInterface, DoIOOperation),
    ),
    (
      "offsetof(AudioServerPlugInDriverInterface, EndIOOperation)",
      offset_of!(AudioServerPlugInDriverInterface, EndIOOperation),
    ),
  ];
  values.extend(layouts.map(|(name, value)| (name, value as u64)));
  // the first 8 bytes of the UUIDs
  for (name, uuid) in [
    ("kAudioServerPlugInTypeUUID", PLUGIN_TYPE_UUID),
    ("kAudioServerPlugInDriverInterfaceUUID", DRIVER_INTERFACE_UUID),
    ("IUnknownUUID", IUNKNOWN_UUID),
  ] {
    values.push((name, u64::from_be_bytes(uuid[..8].try_into().unwrap())));
  }
  values
}

/// A C expression printing `name`'s value as an unsigned 64-bit number.
fn c_value(name: &str) -> String {
  if name.ends_with("UUID") {
    let bytes: Vec<String> =
      (0..8).map(|i| format!("((unsigned long long)b.byte{i} << {})", 56 - 8 * i)).collect();
    format!("({{ CFUUIDBytes b = CFUUIDGetUUIDBytes({name}); {}; }})", bytes.join(" | "))
  } else {
    format!("(unsigned long long)(long long)({name})")
  }
}

#[test]
fn definitions_match_the_sdk() {
  let expected = expected();
  let mut program = String::from(
    "#include <CoreAudio/AudioServerPlugIn.h>\n#include <CoreAudio/AudioHardware.h>\n#include \
     <stdio.h>\n#include <stddef.h>\nint main(void) {\n",
  );
  for (name, _) in &expected {
    program += &format!("  printf(\"%s=%llu\\n\", \"{name}\", {});\n", c_value(name));
  }
  program += "  return 0;\n}\n";
  let dir = std::env::temp_dir().join(format!("inferno_coreaudio_abi_{}", std::process::id()));
  std::fs::create_dir_all(&dir).unwrap();
  let (source, binary) = (dir.join("abi.c"), dir.join("abi"));
  std::fs::write(&source, program).unwrap();
  let status = Command::new("cc")
    .arg("-o")
    .arg(&binary)
    .arg(&source)
    .args(["-framework", "CoreAudio", "-framework", "CoreFoundation"])
    .status()
    .expect("a C compiler (the Xcode command line tools)");
  assert!(status.success(), "the C program didn't compile");
  let output = String::from_utf8(Command::new(&binary).output().unwrap().stdout).unwrap();
  let _ = std::fs::remove_dir_all(&dir);
  let sdk: BTreeMap<&str, u64> = output
    .lines()
    .filter_map(|line| line.rsplit_once('=').map(|(name, value)| (name, value.parse().unwrap())))
    .collect();
  let wrong: Vec<String> = expected
    .iter()
    .filter(|(name, value)| sdk.get(name) != Some(value))
    .map(|(name, value)| format!("{name}: {value:#x} here, {:#x?} in the SDK", sdk.get(name)))
    .collect();
  assert!(wrong.is_empty(), "differing from the SDK:\n{}", wrong.join("\n"));
}
