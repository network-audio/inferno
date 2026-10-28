//! The region in Mach shared memory, handed from the service to the plug-in over a Mach service.
//!
//! The service is a launchd daemon with the Mach service [`SERVICE_NAME`] (its launchd property list declares
//! it, and the plug-in's `Info.plist` lets it look it up from Core Audio's sandbox). The plug-in sends a
//! request; the reply carries a memory entry of the current region (a send right), which it maps, or says
//! there's none yet (e.g. while waiting for the clock).
//!
//! Only Core Audio (`_coreaudiod`), root and the service's own user may get the region: it carries the cards'
//! audio.

use std::{ffi::CString, mem::size_of, ptr};

use mach2::{
  bootstrap::{bootstrap_check_in, bootstrap_look_up, bootstrap_port},
  kern_return::KERN_SUCCESS,
  mach_port::{mach_port_allocate, mach_port_deallocate, mach_port_insert_right, mach_port_mod_refs},
  message::*,
  port::{mach_port_t, MACH_PORT_NULL, MACH_PORT_RIGHT_RECEIVE},
  traps::mach_task_self,
  vm::{mach_make_memory_entry_64, mach_vm_deallocate, mach_vm_map},
  vm_inherit::VM_INHERIT_NONE,
  vm_prot::{VM_PROT_READ, VM_PROT_WRITE},
  vm_statistics::VM_FLAGS_ANYWHERE,
};

/// The service's Mach name, in its launchd property list and the plug-in's `Info.plist`.
pub const SERVICE_NAME: &str = "org.network-audio.inferno.soundcard";

const REQUEST_ID: mach_msg_id_t = 0x494e_0001;
const REPLY_REGION_ID: mach_msg_id_t = 0x494e_0002;
const REPLY_NONE_ID: mach_msg_id_t = 0x494e_0003;
const REPLY_DENIED_ID: mach_msg_id_t = 0x494e_0004;

/// `mach/memory_object_types.h`: create a named entry of new (zeroed) memory
const MAP_MEM_NAMED_CREATE: i32 = 0x020000;
/// `mach/message.h`: `MACH_RCV_TRAILER_TYPE(MACH_MSG_TRAILER_FORMAT_0)`, `MACH_RCV_TRAILER_ELEMENTS(AUDIT)`
const RCV_AUDIT_TRAILER: mach_msg_option_t = ((MACH_RCV_TRAILER_AUDIT & 0xf) << 24) as mach_msg_option_t;

fn kern_error(call: &str, code: i32) -> String {
  format!("{call}: Mach error {code:#x}")
}

/// Memory shared through a Mach memory entry, mapped in this process: unmapped when dropped.
pub struct SharedMemory {
  addr: u64,
  size: u64,
  entry: mach_port_t,
}

// safety: the mapping and the port name are usable from any thread
unsafe impl Send for SharedMemory {}
unsafe impl Sync for SharedMemory {}

impl SharedMemory {
  /// New zeroed memory of at least `size` bytes, to share.
  pub fn create(size: usize) -> Result<Self, String> {
    let mut entry_size = size as u64;
    let mut entry = MACH_PORT_NULL;
    // safety: valid out pointers
    let r = unsafe {
      mach_make_memory_entry_64(
        mach_task_self(),
        &mut entry_size,
        0,
        MAP_MEM_NAMED_CREATE | VM_PROT_READ | VM_PROT_WRITE,
        &mut entry,
        MACH_PORT_NULL,
      )
    };
    if r != KERN_SUCCESS {
      return Err(kern_error("mach_make_memory_entry_64", r));
    }
    Self::map(entry, entry_size)
  }

  /// Maps the memory of `entry` (a send right, which the result owns) of `size` bytes.
  pub fn map(entry: mach_port_t, size: u64) -> Result<Self, String> {
    let mut addr = 0;
    // safety: valid out pointer; the entry is checked by the kernel
    let r = unsafe {
      mach_vm_map(
        mach_task_self(),
        &mut addr,
        size,
        0,
        VM_FLAGS_ANYWHERE,
        entry,
        0,
        0,
        VM_PROT_READ | VM_PROT_WRITE,
        VM_PROT_READ | VM_PROT_WRITE,
        VM_INHERIT_NONE,
      )
    };
    if r != KERN_SUCCESS {
      // safety: our right
      unsafe { mach_port_deallocate(mach_task_self(), entry) };
      return Err(kern_error("mach_vm_map", r));
    }
    Ok(Self { addr, size, entry })
  }

  pub fn ptr(&self) -> *mut u8 {
    self.addr as *mut u8
  }

  pub fn len(&self) -> usize {
    self.size as usize
  }

  pub fn is_empty(&self) -> bool {
    self.size == 0
  }

  /// The memory entry, to send to another process.
  pub fn entry(&self) -> mach_port_t {
    self.entry
  }
}

impl Drop for SharedMemory {
  fn drop(&mut self) {
    // safety: our mapping and right
    unsafe {
      mach_vm_deallocate(mach_task_self(), self.addr, self.size);
      mach_port_deallocate(mach_task_self(), self.entry);
    }
  }
}

#[repr(C)]
struct Request {
  header: mach_msg_header_t,
}

#[repr(C, packed(4))]
struct RegionReply {
  header: mach_msg_header_t,
  body: mach_msg_body_t,
  entry: mach_msg_port_descriptor_t,
  size: u64,
}

#[repr(C)]
struct SimpleReply {
  header: mach_msg_header_t,
}

/// `mach/message.h`'s `mach_msg_audit_trailer_t`
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct AuditTrailer {
  trailer_type: u32,
  trailer_size: u32,
  seqno: u32,
  /// the sender's effective user and group
  sender: [u32; 2],
  audit: [u32; 8],
}

/// Room for any message here and its trailer.
#[repr(C, align(8))]
struct Buffer([u8; 256]);

impl Buffer {
  fn header(&mut self) -> *mut mach_msg_header_t {
    self.0.as_mut_ptr() as *mut mach_msg_header_t
  }
}

/// The receiving end of the Mach service.
pub struct Service {
  port: mach_port_t,
  /// users allowed to get the region, besides root and Core Audio's
  allowed_uid: u32,
}

/// The user id of Core Audio's daemon and driver services.
fn coreaudiod_uid() -> Option<u32> {
  let name = CString::new("_coreaudiod").unwrap();
  // safety: a valid C string; the result is static (only read here)
  let pw = unsafe { libc::getpwnam(name.as_ptr()) };
  (!pw.is_null()).then(|| unsafe { (*pw).pw_uid })
}

impl Service {
  /// The service launchd created for this daemon (from its property list).
  pub fn check_in(name: &str) -> Result<Self, String> {
    let name = CString::new(name).unwrap();
    let mut port = MACH_PORT_NULL;
    // safety: a valid C string and out pointer
    let r = unsafe { bootstrap_check_in(bootstrap_port, name.as_ptr(), &mut port) };
    if r != KERN_SUCCESS {
      return Err(format!(
        "{}: bootstrap_check_in failed ({r}): the service must be started by launchd, with its property \
         list",
        name.to_string_lossy()
      ));
    }
    Ok(Self { port, allowed_uid: unsafe { libc::geteuid() } })
  }

  /// A service of this process only, at `port()` (for tests).
  pub fn local() -> Result<Self, String> {
    let mut port = MACH_PORT_NULL;
    // safety: valid out pointer, then our receive right
    unsafe {
      let r = mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &mut port);
      if r != KERN_SUCCESS {
        return Err(kern_error("mach_port_allocate", r));
      }
      let r = mach_port_insert_right(mach_task_self(), port, port, MACH_MSG_TYPE_MAKE_SEND);
      if r != KERN_SUCCESS {
        return Err(kern_error("mach_port_insert_right", r));
      }
    }
    Ok(Self { port, allowed_uid: unsafe { libc::geteuid() } })
  }

  /// The port to send requests to (this process's name of it).
  pub fn port(&self) -> mach_port_t {
    self.port
  }

  /// Answers a request, if one arrives within `timeout_ms`, with the memory `current` returns (its entry and
  /// size), or that there's none. Returns the requester's user id, if a request came.
  pub fn serve_one(
    &self,
    timeout_ms: u32,
    current: impl FnOnce() -> Option<(mach_port_t, u64)>,
  ) -> Result<Option<u32>, String> {
    let mut buffer = Buffer([0; 256]);
    // safety: the buffer has room for the request and its trailer
    let r = unsafe {
      mach_msg(
        buffer.header(),
        MACH_RCV_MSG | MACH_RCV_TIMEOUT | RCV_AUDIT_TRAILER,
        0,
        size_of::<Buffer>() as u32,
        self.port,
        timeout_ms,
        MACH_PORT_NULL,
      )
    };
    if r == MACH_RCV_TIMED_OUT {
      return Ok(None);
    }
    if r != MACH_MSG_SUCCESS {
      return Err(kern_error("mach_msg (receive)", r));
    }
    // safety: received into the buffer
    let header = unsafe { ptr::read(buffer.header()) };
    let trailer_at = (header.msgh_size as usize).next_multiple_of(4);
    let uid = (trailer_at + size_of::<AuditTrailer>() <= buffer.0.len()).then(|| {
      // safety: within the buffer; the kernel appends the trailer requested
      unsafe { ptr::read_unaligned(buffer.0.as_ptr().add(trailer_at) as *const AuditTrailer) }.sender[0]
    });
    let reply_port = header.msgh_remote_port;
    if header.msgh_id != REQUEST_ID || reply_port == MACH_PORT_NULL {
      // safety: the received message, whose rights we don't use
      unsafe { mach_msg_destroy(buffer.header()) };
      return Ok(uid);
    }
    let allowed =
      uid.is_some_and(|uid| uid == 0 || uid == self.allowed_uid || Some(uid) == coreaudiod_uid());
    let region = if allowed { current() } else { None };
    let reply_header = |id, size: usize, bits| mach_msg_header_t {
      msgh_bits: bits,
      msgh_size: size as u32,
      msgh_remote_port: reply_port,
      msgh_local_port: MACH_PORT_NULL,
      msgh_voucher_port: MACH_PORT_NULL,
      msgh_id: id,
    };
    let r = match region {
      Some((entry, size)) => {
        let mut reply = RegionReply {
          header: reply_header(
            REPLY_REGION_ID,
            size_of::<RegionReply>(),
            MACH_MSGH_BITS_COMPLEX | MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0),
          ),
          body: mach_msg_body_t { msgh_descriptor_count: 1 },
          entry: mach_msg_port_descriptor_t::new(entry, MACH_MSG_TYPE_COPY_SEND),
          size,
        };
        // safety: a valid message
        unsafe { send(&mut reply.header, size_of::<RegionReply>()) }
      }
      None => {
        let id = if allowed { REPLY_NONE_ID } else { REPLY_DENIED_ID };
        let mut reply = SimpleReply {
          header: reply_header(
            id,
            size_of::<SimpleReply>(),
            MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0),
          ),
        };
        // safety: a valid message
        unsafe { send(&mut reply.header, size_of::<SimpleReply>()) }
      }
    };
    if r != MACH_MSG_SUCCESS {
      // the reply's send-once right is still ours
      unsafe { mach_port_deallocate(mach_task_self(), reply_port) };
      return Err(kern_error("mach_msg (reply)", r));
    }
    if !allowed {
      return Err(format!("refused the region to user {uid:?}"));
    }
    Ok(uid)
  }
}

impl Drop for Service {
  fn drop(&mut self) {
    // safety: our rights
    unsafe {
      mach_port_mod_refs(mach_task_self(), self.port, MACH_PORT_RIGHT_RECEIVE, -1);
      mach_port_deallocate(mach_task_self(), self.port);
    }
  }
}

unsafe fn send(header: *mut mach_msg_header_t, size: usize) -> mach_msg_return_t {
  mach_msg(
    header,
    MACH_SEND_MSG | MACH_SEND_TIMEOUT,
    size as u32,
    0,
    MACH_PORT_NULL,
    1000,
    MACH_PORT_NULL,
  )
}

/// The service's port (a send right, deallocated with `release`), if it's registered.
pub fn look_up(name: &str) -> Result<mach_port_t, String> {
  let c_name = CString::new(name).unwrap();
  let mut port = MACH_PORT_NULL;
  // safety: a valid C string and out pointer
  let r = unsafe { bootstrap_look_up(bootstrap_port, c_name.as_ptr(), &mut port) };
  if r != KERN_SUCCESS {
    return Err(format!("bootstrap_look_up({name}) failed ({r}): is the sound card service installed?"));
  }
  Ok(port)
}

/// Gives up a send right from `look_up`.
pub fn release(port: mach_port_t) {
  // safety: a right of ours
  unsafe { mach_port_deallocate(mach_task_self(), port) };
}

/// What the service answered.
pub enum Answer {
  Region(SharedMemory),
  /// it has no region (yet)
  None,
  /// it doesn't give this process the region
  Denied,
}

/// Asks the service at `service` (a send right) for its region, waiting up to `timeout_ms` for the answer.
pub fn request(service: mach_port_t, timeout_ms: u32) -> Result<Answer, String> {
  let mut reply_port = MACH_PORT_NULL;
  // safety: valid out pointer
  let r = unsafe { mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &mut reply_port) };
  if r != KERN_SUCCESS {
    return Err(kern_error("mach_port_allocate", r));
  }
  let result = (|| {
    let mut buffer = Buffer([0; 256]);
    // safety: the buffer holds a request
    unsafe {
      ptr::write(
        buffer.header(),
        mach_msg_header_t {
          msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MAKE_SEND_ONCE),
          msgh_size: size_of::<Request>() as u32,
          msgh_remote_port: service,
          msgh_local_port: reply_port,
          msgh_voucher_port: MACH_PORT_NULL,
          msgh_id: REQUEST_ID,
        },
      )
    };
    // safety: sends the request, receives the reply into the buffer
    let r = unsafe {
      mach_msg(
        buffer.header(),
        MACH_SEND_MSG | MACH_SEND_TIMEOUT | MACH_RCV_MSG | MACH_RCV_TIMEOUT,
        size_of::<Request>() as u32,
        size_of::<Buffer>() as u32,
        reply_port,
        timeout_ms,
        MACH_PORT_NULL,
      )
    };
    if r != MACH_MSG_SUCCESS {
      return Err(kern_error("mach_msg (request)", r));
    }
    // safety: received into the buffer
    let header = unsafe { ptr::read(buffer.header()) };
    match header.msgh_id {
      REPLY_REGION_ID if header.msgh_bits & MACH_MSGH_BITS_COMPLEX != 0 => {
        // safety: a region reply (by its id), received into the buffer
        let reply = unsafe { ptr::read_unaligned(buffer.0.as_ptr() as *const RegionReply) };
        let (entry, size) = (reply.entry.name, reply.size);
        SharedMemory::map(entry, size).map(Answer::Region)
      }
      REPLY_NONE_ID => Ok(Answer::None),
      REPLY_DENIED_ID => Ok(Answer::Denied),
      id => {
        // safety: the received message, whose rights we don't use
        unsafe { mach_msg_destroy(buffer.header()) };
        Err(format!("unexpected reply {id:#x}"))
      }
    }
  })();
  // safety: our receive right
  unsafe { mach_port_mod_refs(mach_task_self(), reply_port, MACH_PORT_RIGHT_RECEIVE, -1) };
  result
}

#[cfg(test)]
mod tests {
  use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
  };

  use super::*;

  #[test]
  fn the_region_is_shared_with_the_requester() {
    let memory = Arc::new(SharedMemory::create(64 * 1024).unwrap());
    let word = |m: &SharedMemory| unsafe { &*(m.ptr().add(4096) as *const AtomicU64) };
    word(&memory).store(0x1234, Ordering::Relaxed);
    let service = Service::local().unwrap();
    let port = service.port();
    let server = {
      let memory = memory.clone();
      std::thread::spawn(move || {
        let none = service.serve_one(5000, || None).unwrap();
        let region = service.serve_one(5000, || Some((memory.entry(), memory.len() as u64))).unwrap();
        (none, region)
      })
    };
    assert!(matches!(request(port, 5000).unwrap(), Answer::None));
    let Answer::Region(mapped) = request(port, 5000).unwrap() else { panic!("no region") };
    let own = unsafe { libc::geteuid() };
    assert_eq!(server.join().unwrap(), (Some(own), Some(own)), "the requester's user is known");
    assert_eq!(mapped.len(), memory.len());
    assert_ne!(mapped.ptr(), memory.ptr(), "mapped again");
    assert_eq!(word(&mapped).load(Ordering::Relaxed), 0x1234);
    word(&mapped).store(0x5678, Ordering::Relaxed);
    assert_eq!(word(&memory).load(Ordering::Relaxed), 0x5678, "shared, not copied");
  }

  #[test]
  fn no_answer_in_time() {
    let service = Service::local().unwrap();
    assert!(request(service.port(), 100).is_err());
  }
}
