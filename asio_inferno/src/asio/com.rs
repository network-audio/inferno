//! COM in-process server: the class factory, DLL exports and registration (`regsvr32 inferno_asio.dll`, as
//! administrator). ASIO hosts find drivers under HKLM\SOFTWARE\ASIO and create them with CoCreateInstance.

use std::{
  ffi::c_void,
  sync::atomic::{AtomicIsize, AtomicPtr, Ordering},
};

use windows_sys::{
  core::{GUID, HRESULT},
  Win32::{
    Foundation::{
      CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, ERROR_SUCCESS, E_NOINTERFACE, E_POINTER,
      HINSTANCE, S_FALSE, S_OK,
    },
    System::{
      LibraryLoader::GetModuleFileNameW,
      Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CLASSES_ROOT,
        HKEY_LOCAL_MACHINE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
      },
      SystemServices::DLL_PROCESS_ATTACH,
    },
  },
};

use super::driver::{guid_eq, AsioDriver};

/// {B834D377-1E6D-456E-8A73-4D8790B0C196}
pub const CLSID_INFERNO_ASIO: GUID = GUID {
  data1: 0xb834d377,
  data2: 0x1e6d,
  data3: 0x456e,
  data4: [0x8a, 0x73, 0x4d, 0x87, 0x90, 0xb0, 0xc1, 0x96],
};
const CLSID_STRING: &str = "{B834D377-1E6D-456E-8A73-4D8790B0C196}";
pub const IID_IUNKNOWN: GUID =
  GUID { data1: 0, data2: 0, data3: 0, data4: [0xc0, 0, 0, 0, 0, 0, 0, 0x46] };
const IID_ICLASSFACTORY: GUID =
  GUID { data1: 1, data2: 0, data3: 0, data4: [0xc0, 0, 0, 0, 0, 0, 0, 0x46] };

/// Name hosts list the driver by (HKLM\SOFTWARE\ASIO\<name>).
const ASIO_KEY: &str = "SOFTWARE\\ASIO\\Inferno";
const DESCRIPTION: &str = "Inferno (Dante)";

static MODULE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
/// live driver objects and LockServer locks: the DLL can't be unloaded while there are any
static IN_USE: AtomicIsize = AtomicIsize::new(0);

pub fn object_created() {
  IN_USE.fetch_add(1, Ordering::AcqRel);
}

pub fn object_destroyed() {
  IN_USE.fetch_sub(1, Ordering::AcqRel);
}

#[no_mangle]
pub extern "system" fn DllMain(module: HINSTANCE, reason: u32, _reserved: *mut c_void) -> i32 {
  if reason == DLL_PROCESS_ATTACH {
    MODULE.store(module, Ordering::Release);
  }
  1
}

#[repr(C)]
struct ClassFactoryVtbl {
  query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
  add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
  release: unsafe extern "system" fn(*mut c_void) -> u32,
  create_instance:
    unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
  lock_server: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT,
}

#[repr(C)]
struct ClassFactory {
  vtbl: &'static ClassFactoryVtbl,
}

unsafe extern "system" fn factory_query_interface(
  this: *mut c_void,
  riid: *const GUID,
  out: *mut *mut c_void,
) -> HRESULT {
  if out.is_null() || riid.is_null() {
    return E_POINTER;
  }
  if guid_eq(&*riid, &IID_IUNKNOWN) || guid_eq(&*riid, &IID_ICLASSFACTORY) {
    *out = this;
    S_OK
  } else {
    *out = std::ptr::null_mut();
    E_NOINTERFACE
  }
}

// the factory is static: no reference counting
unsafe extern "system" fn factory_add_ref(_this: *mut c_void) -> u32 {
  2
}

unsafe extern "system" fn factory_release(_this: *mut c_void) -> u32 {
  1
}

unsafe extern "system" fn factory_create_instance(
  _this: *mut c_void,
  outer: *mut c_void,
  riid: *const GUID,
  out: *mut *mut c_void,
) -> HRESULT {
  if out.is_null() {
    return E_POINTER;
  }
  *out = std::ptr::null_mut();
  if !outer.is_null() {
    return CLASS_E_NOAGGREGATION;
  }
  let driver = AsioDriver::create() as *mut c_void;
  let r = super::driver::query_interface(driver, riid, out);
  super::driver::release(driver); // the reference of `create`; `out` holds its own
  r
}

unsafe extern "system" fn factory_lock_server(_this: *mut c_void, lock: i32) -> HRESULT {
  IN_USE.fetch_add(if lock != 0 { 1 } else { -1 }, Ordering::AcqRel);
  S_OK
}

static FACTORY_VTBL: ClassFactoryVtbl = ClassFactoryVtbl {
  query_interface: factory_query_interface,
  add_ref: factory_add_ref,
  release: factory_release,
  create_instance: factory_create_instance,
  lock_server: factory_lock_server,
};

struct SyncFactory(ClassFactory);
unsafe impl Sync for SyncFactory {}
static FACTORY: SyncFactory = SyncFactory(ClassFactory { vtbl: &FACTORY_VTBL });

#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
  clsid: *const GUID,
  riid: *const GUID,
  out: *mut *mut c_void,
) -> HRESULT {
  if clsid.is_null() || out.is_null() {
    return E_POINTER;
  }
  if !guid_eq(&*clsid, &CLSID_INFERNO_ASIO) {
    *out = std::ptr::null_mut();
    return CLASS_E_CLASSNOTAVAILABLE;
  }
  factory_query_interface(&FACTORY.0 as *const ClassFactory as *mut c_void, riid, out)
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
  if IN_USE.load(Ordering::Acquire) == 0 {
    S_OK
  } else {
    S_FALSE
  }
}

fn wide(s: &str) -> Vec<u16> {
  s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Creates `root\path` and sets its values (`None`: the default value).
fn set_key(root: HKEY, path: &str, values: &[(Option<&str>, &str)]) -> Result<(), u32> {
  let mut key: HKEY = std::ptr::null_mut();
  // safety: valid strings and out pointer
  let r = unsafe {
    RegCreateKeyExW(
      root,
      wide(path).as_ptr(),
      0,
      std::ptr::null(),
      REG_OPTION_NON_VOLATILE,
      KEY_WRITE,
      std::ptr::null(),
      &mut key,
      std::ptr::null_mut(),
    )
  };
  if r != ERROR_SUCCESS {
    return Err(r);
  }
  let mut result = Ok(());
  for (name, value) in values {
    let name = name.map(wide);
    let data = wide(value);
    // safety: valid key, name and data (REG_SZ including the terminating null)
    let r = unsafe {
      RegSetValueExW(
        key,
        name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
        0,
        REG_SZ,
        data.as_ptr() as *const u8,
        (data.len() * 2) as u32,
      )
    };
    if r != ERROR_SUCCESS {
      result = Err(r);
    }
  }
  // safety: key opened above
  unsafe { RegCloseKey(key) };
  result
}

fn module_path() -> String {
  let mut buf = vec![0u16; 1024];
  // safety: valid buffer of that length
  let len =
    unsafe { GetModuleFileNameW(MODULE.load(Ordering::Acquire), buf.as_mut_ptr(), buf.len() as u32) };
  String::from_utf16_lossy(&buf[..len as usize])
}

const SELFREG_E_CLASS: HRESULT = 0x80040201u32 as HRESULT;

#[no_mangle]
pub extern "system" fn DllRegisterServer() -> HRESULT {
  let clsid_key = format!("CLSID\\{CLSID_STRING}");
  let path = module_path();
  let result = set_key(HKEY_CLASSES_ROOT, &clsid_key, &[(None, "Inferno ASIO driver")])
    .and_then(|_| {
      set_key(
        HKEY_CLASSES_ROOT,
        &format!("{clsid_key}\\InprocServer32"),
        &[(None, &path), (Some("ThreadingModel"), "Apartment")],
      )
    })
    .and_then(|_| {
      set_key(
        HKEY_LOCAL_MACHINE,
        ASIO_KEY,
        &[(Some("CLSID"), CLSID_STRING), (Some("Description"), DESCRIPTION)],
      )
    });
  match result {
    Ok(()) => S_OK,
    // e.g. ERROR_ACCESS_DENIED: regsvr32 must run as administrator
    Err(_) => SELFREG_E_CLASS,
  }
}

#[no_mangle]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
  // safety: valid strings
  unsafe {
    RegDeleteTreeW(HKEY_LOCAL_MACHINE, wide(ASIO_KEY).as_ptr());
    RegDeleteTreeW(HKEY_CLASSES_ROOT, wide(&format!("CLSID\\{CLSID_STRING}")).as_ptr());
  }
  S_OK
}
