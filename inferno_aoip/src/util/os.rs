use thread_priority::Error;

/// Makes the current thread real-time (FIFO). `priority_hint` is a priority in Linux's range (1..=99): it's
/// mapped to the range of this OS (e.g. 15..=47 on macOS).
#[cfg(target_family = "unix")]
pub fn set_current_thread_realtime(priority_hint: u8) -> Result<(), Error> {
  use thread_priority::unix::set_thread_priority_and_policy;
  use thread_priority::{
    RealtimeThreadSchedulePolicy, ThreadPriority, ThreadPriorityValue, ThreadSchedulePolicy,
  };

  // safety: no preconditions
  let (min, max) =
    unsafe { (libc::sched_get_priority_min(libc::SCHED_FIFO), libc::sched_get_priority_max(libc::SCHED_FIFO)) };
  let priority = if min >= 0 && max >= min { fifo_priority(priority_hint, min, max) } else { priority_hint };
  set_thread_priority_and_policy(
    thread_priority::thread_native_id(),
    ThreadPriority::Crossplatform(ThreadPriorityValue::try_from(priority).map_err(Error::Priority)?),
    ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::Fifo),
  )
}

/// Maps `hint` from Linux's FIFO priority range (1..=99) to `min..=max`.
#[cfg(target_family = "unix")]
fn fifo_priority(hint: u8, min: i32, max: i32) -> u8 {
  let hint = (hint.clamp(1, 99) - 1) as i32;
  (min + hint * (max - min) / 98) as u8
}

#[cfg(not(target_family = "unix"))]
pub fn set_current_thread_realtime(_priority_hint: u8) -> Result<(), Error> {
  thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max)
}

#[cfg(all(test, target_family = "unix"))]
mod tests {
  use super::fifo_priority;

  #[test]
  fn fifo_priority_is_unchanged_in_linux_range() {
    assert_eq!(fifo_priority(80, 1, 99), 80);
    assert_eq!(fifo_priority(1, 1, 99), 1);
    assert_eq!(fifo_priority(99, 1, 99), 99);
  }

  #[test]
  fn fifo_priority_is_mapped_to_macos_range() {
    assert_eq!(fifo_priority(1, 15, 47), 15);
    assert_eq!(fifo_priority(99, 15, 47), 47);
    let p = fifo_priority(80, 15, 47);
    assert!((15..=47).contains(&p) && p > fifo_priority(50, 15, 47), "{p}");
  }
}
