//! A stopwatch for the budget tests that counts the CPU time the calling
//! thread has used, not wall-clock time.
//!
//! A budget test asks "does this work cost too much?". On a loaded or
//! shared machine a wall-clock stopwatch also counts the time the thread
//! sat descheduled behind other processes, so the same code passes
//! alone and fails at load 30. Thread CPU time does not grow when the
//! thread is waiting for a core, so the same budget holds on a quiet
//! and a busy machine. Only for work that runs on the calling thread
//! from start to finish; a test that is about waiting (a sleep, a
//! timeout) must keep wall-clock time.
//!
//! Shared by `tephra-parser`, `tephra-graph` and `tephra-cache` tests,
//! which include this one file with `#[path]`.

#![allow(dead_code)]

use std::time::Duration;

/// CPU time used so far by the calling thread.
#[cfg(unix)]
fn thread_cpu_now() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec for the call.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    assert_eq!(rc, 0, "clock_gettime(CLOCK_THREAD_CPUTIME_ID) failed");
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// No per-thread CPU clock here: fall back to a monotonic clock since
/// process start, so the tests still run (and are load-sensitive).
#[cfg(not(unix))]
fn thread_cpu_now() -> Duration {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

/// Started with [`CpuTimer::start`]; read with [`CpuTimer::elapsed`].
pub struct CpuTimer(Duration);

impl CpuTimer {
    pub fn start() -> Self {
        CpuTimer(thread_cpu_now())
    }

    /// CPU time this thread has used since [`CpuTimer::start`].
    pub fn elapsed(&self) -> Duration {
        thread_cpu_now().saturating_sub(self.0)
    }
}
