//! The time source of the server's media thread.
//!
//! Every deadline the media thread keeps (ICE timers, retransmissions, the
//! session reapers) is read from a [`Clock`], never from
//! [`Instant::now`] directly, so a test can drive the reapers by advancing
//! a manual clock instead of waiting.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// A monotonic time source.
pub trait Clock: Send + Sync + 'static {
    /// The current instant.
    fn now(&self) -> Instant;
}

/// The system's monotonic clock, [`Instant::now`]. The server's default.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A clock that moves only when [`advance`](ManualClock::advance) is
/// called. A test seam; not a supported API.
#[doc(hidden)]
#[derive(Debug)]
pub struct ManualClock {
    base: Instant,
    offset: Mutex<Duration>,
}

impl ManualClock {
    /// A clock standing at the instant it was created.
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            offset: Mutex::new(Duration::ZERO),
        }
    }

    /// Moves the clock forward by `d`.
    pub fn advance(&self, d: Duration) {
        let mut offset = self.offset.lock().unwrap_or_else(PoisonError::into_inner);
        *offset += d;
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.base + *self.offset.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
