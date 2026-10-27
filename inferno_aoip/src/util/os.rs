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

/// Registers the current thread with MMCSS as "Pro Audio" (for the rest of its life), like the audio threads of
/// hosts and drivers: plain thread priorities rank below them. `priority_hint` of 80 and more (the threads that
/// move audio): critical within the task, high otherwise. Falls back to time-critical thread priority.
#[cfg(windows)]
pub fn set_current_thread_realtime(priority_hint: u8) -> Result<(), Error> {
  use windows_sys::Win32::System::Threading::{
    AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority, GetCurrentThread, SetThreadPriority,
    AVRT_PRIORITY_CRITICAL, AVRT_PRIORITY_HIGH, THREAD_PRIORITY_TIME_CRITICAL,
  };
  let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
  let mut index = 0u32;
  // safety: valid null-terminated string and index pointer; the handle is kept until the thread ends
  let handle = unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut index) };
  if !handle.is_null() {
    let priority = if priority_hint >= 80 { AVRT_PRIORITY_CRITICAL } else { AVRT_PRIORITY_HIGH };
    // safety: valid MMCSS handle
    unsafe { AvSetMmThreadPriority(handle, priority) };
    return Ok(());
  }
  // safety: pseudo-handle of the current thread
  if unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) } != 0 {
    Ok(())
  } else {
    Err(Error::OS(std::io::Error::last_os_error().raw_os_error().unwrap_or(0)))
  }
}

#[cfg(not(any(target_family = "unix", windows)))]
pub fn set_current_thread_realtime(_priority_hint: u8) -> Result<(), Error> {
  thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max)
}

/// macOS: makes the current thread a time-constraint (real-time) thread, like Core Audio's I/O threads: every
/// `period` it needs up to `computation` of CPU, done within `constraint` of the period's start. Its timers are
/// then not coalesced either. A thread exceeding its computation repeatedly is demoted by the system, so it's
/// for threads doing a bounded amount of work per period. Returns the Mach error code on failure.
#[cfg(target_os = "macos")]
pub fn set_current_thread_time_constraint(
  period: std::time::Duration,
  computation: std::time::Duration,
  constraint: std::time::Duration,
) -> Result<(), i32> {
  let mut timebase = libc::mach_timebase_info { numer: 0, denom: 0 };
  // safety: valid out pointer
  if unsafe { libc::mach_timebase_info(&mut timebase) } != 0 || timebase.numer == 0 {
    return Err(-1);
  }
  // nanoseconds to Mach absolute time units
  let abs = |d: std::time::Duration| {
    (d.as_nanos() * timebase.denom as u128 / timebase.numer as u128).min(u32::MAX as u128) as u32
  };
  let mut policy = libc::thread_time_constraint_policy {
    period: abs(period),
    computation: abs(computation),
    constraint: abs(constraint),
    preemptible: 1,
  };
  // safety: the policy struct matches the flavor and count; the current thread's port
  let r = unsafe {
    libc::thread_policy_set(
      libc::mach_thread_self(),
      libc::THREAD_TIME_CONSTRAINT_POLICY as _,
      &mut policy as *mut _ as libc::thread_policy_t,
      libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT,
    )
  };
  if r == 0 {
    Ok(())
  } else {
    Err(r)
  }
}

/// Fine timer resolution (1 ms, process-wide) while it lives. On Windows, sleeps and timeouts of the audio
/// threads are otherwise rounded up to the default 15.6 ms tick. Nothing to do elsewhere.
pub struct TimerResolution(());

impl TimerResolution {
  pub fn fine() -> Self {
    #[cfg(windows)]
    // safety: no preconditions; undone in drop
    unsafe {
      windows_sys::Win32::Media::timeBeginPeriod(1);
    }
    Self(())
  }
}

impl Drop for TimerResolution {
  fn drop(&mut self) {
    #[cfg(windows)]
    // safety: matches timeBeginPeriod in fine()
    unsafe {
      windows_sys::Win32::Media::timeEndPeriod(1);
    }
  }
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
