//! Internal lock abstraction so the sender shells compile both with the
//! standard library and `#![no_std]`+`alloc`.
//!
//! - Under `std`, [`ShellMutex`] is a type alias for [`std::sync::Mutex`] —
//!   poison semantics and behavior are exactly std's (zero divergence on the
//!   gating platforms).
//! - Under `no_std`, it is a thin newtype over [`spin::Mutex`] — no
//!   poisoning, no priority inheritance, no interrupt masking. The shells'
//!   documented contract (one sender per task) keeps the lock uncontended
//!   in correct programs: it exists to satisfy `Sync`, not to arbitrate
//!   cross-task sharing. Sharing one sender across preemptive tasks anyway
//!   can livelock — a higher-priority task spins forever against a
//!   preempted lock-holder (classic priority inversion). That is a
//!   contract violation, not a supported mode. `lock()` returns
//!   `Result<_, Poisoned<_>>` (always `Ok`) and `try_lock()` returns
//!   `Result<_, TryLockError<_>>`, so the call sites — which use
//!   `if let Ok(..)` / `match` / `.map_err(|_| ..)?` /
//!   `.unwrap_or_else(|e| e.into_inner())` — compile unchanged against
//!   both backends.
//! - `binding::owned::Owned` takes the same lock, so the one-handle-per-task
//!   contract below also binds every binding handle on a `no_std` target.

#[cfg(feature = "std")]
pub(crate) type ShellMutex<T> = std::sync::Mutex<T>;
#[cfg(feature = "std")]
#[allow(unused_imports)] // the binding layer's poison-recover sites
pub(crate) use std::sync::{PoisonError as Poisoned, TryLockError};

// `spin` is referenced only by the no_std backend below, and Cargo cannot
// express "dependency only when a feature is OFF" — so on std builds it looks
// unused to cargo's (nightly) `unused_dependencies` lint. The conventional
// `use … as _` marks it as deliberately present.
#[cfg(feature = "std")]
use spin as _;

#[cfg(not(feature = "std"))]
#[allow(unused_imports)] // `Poisoned` is named only through `lock()`'s signature
pub(crate) use no_std_impl::{Poisoned, ShellMutex, TryLockError};

#[cfg(not(feature = "std"))]
mod no_std_impl {
    use spin::{Mutex, MutexGuard};

    /// Spin-backed mutex whose `lock()` / `try_lock()` mirror the shape of
    /// `std::sync::Mutex` (minus poisoning, which cannot occur without
    /// unwinding), so the shells and `binding::owned` compile unchanged on
    /// both planes.
    pub(crate) struct ShellMutex<T>(Mutex<T>);

    /// Stand-in for `std::sync::PoisonError`: never constructed (a spin lock
    /// cannot be poisoned); exists so a poison-recover site
    /// (`.unwrap_or_else(|e| e.into_inner())`) compiles on both planes.
    pub(crate) struct Poisoned<G>(G);

    impl<G> core::fmt::Debug for Poisoned<G> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("Poisoned(..)")
        }
    }

    impl<G> Poisoned<G> {
        pub(crate) fn into_inner(self) -> G {
            self.0
        }
    }

    /// Stand-in for `std::sync::TryLockError`.
    pub(crate) enum TryLockError<G> {
        /// Never produced (see [`Poisoned`]).
        #[allow(dead_code)]
        Poisoned(Poisoned<G>),
        WouldBlock,
    }

    impl<T> ShellMutex<T> {
        pub(crate) fn new(value: T) -> Self {
            ShellMutex(Mutex::new(value))
        }

        /// Always `Ok`: a spinlock cannot be poisoned.
        pub(crate) fn lock(&self) -> Result<MutexGuard<'_, T>, Poisoned<MutexGuard<'_, T>>> {
            Ok(self.0.lock())
        }

        /// `Err(WouldBlock)` while another holder has the lock.
        pub(crate) fn try_lock(
            &self,
        ) -> Result<MutexGuard<'_, T>, TryLockError<MutexGuard<'_, T>>> {
            self.0.try_lock().ok_or(TryLockError::WouldBlock)
        }
    }
}
