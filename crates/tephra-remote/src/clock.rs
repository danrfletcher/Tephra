//! The clock the adapters read `now` from. The adapters never sleep: a
//! rate limit is a `retry_at` instant, and the caller decides when to come
//! back. Tests drive time with [`FakeClock`] instead of waiting.

use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// A source of "now".
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

/// The wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A clock that moves only when told to.
pub struct FakeClock {
    now: Mutex<SystemTime>,
}

impl FakeClock {
    /// A clock standing at `start`.
    pub fn new(start: SystemTime) -> Self {
        FakeClock {
            now: Mutex::new(start),
        }
    }

    /// A clock standing at a fixed, round instant (2026-11-20 00:00:00 UTC),
    /// so a test's expected `retry_at` values read as plain sums.
    pub fn at_epoch_2026() -> Self {
        FakeClock::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_795_132_800))
    }

    pub fn advance(&self, by: Duration) {
        let mut now = self.now.lock().unwrap_or_else(|e| e.into_inner());
        *now += by;
    }

    pub fn set(&self, to: SystemTime) {
        *self.now.lock().unwrap_or_else(|e| e.into_inner()) = to;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> SystemTime {
        *self.now.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl fmt::Debug for FakeClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeClock")
            .field("now", &self.now())
            .finish()
    }
}
