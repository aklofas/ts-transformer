//! [`Owned<T, S>`] — the one handle state machine behind every binding
//! object (Arc 2 spec §3.2).
//!
//! A binding wraps a pipeline shell `T` in an `Owned` and reaches it only
//! through [`Owned::with_mut`] / [`Owned::with_ref`]; cross-thread cancel
//! and close go through [`Owned::cancel`] / [`Owned::close`], which never
//! wait on the slot a parked operation holds. Construction-constant
//! getters (local address, port, URL text) read the [`Owned::snapshot`]
//! captured at construction, never the slot (the PR #234 hang class).
//!
//! # Rules (each pinned by a unit test below)
//!
//! - **Lock inside**: `with_mut` is the only lock site; the binding holds no
//!   mutex of its own around it, and takes the lock with the GIL released /
//!   outside the JNI critical region.
//! - **Poison policy**: readers (`with_ref`, `take`, `is_closed`, `close`)
//!   RECOVER a poisoned mutex; the mutator `with_mut` REFUSES with
//!   [`HandleState::Poisoned`] (the RTSP-client policy of PR #146).
//! - **Panic policy**: a panic inside the closure is caught and reported as
//!   [`HandleState::Panicked`] for THAT call; the guard lives outside the
//!   boundary, so the mutex is NOT poisoned. A panicking MUTATOR drops `T`
//!   (later calls → [`HandleState::Closed`]); a panicking READER keeps it
//!   (spec §3.2 as amended at plan review).
//! - **Double close is quiet**: the second `close()` is `Ok(())`.
//! - **Snapshot getters never take the slot**; **`cancel()` never takes the
//!   slot** (the #189 lease class) — the cancel handle is an `Arc` read
//!   lock-free.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use tst_core::transport::TransportCancel;

use super::panic;
use crate::reconnect::{RecvEndReason, RecvEndReasonHandle};

/// Explicit, fallible shutdown of the value an [`Owned`] holds.
///
/// Implemented by the pipeline shells (and by transports the bindings box
/// directly) so [`Owned::close`] can run the real close *after* the slot
/// has been taken — outside the lock, so a parked operation on another
/// thread is never waited on while the lock is held.
pub trait Close {
    /// The shell's own close error (e.g. a failed final flush).
    type Error: core::fmt::Debug + core::fmt::Display;
    /// Close the value. Called at most once per value by [`Owned::close`].
    fn close(&mut self) -> Result<(), Self::Error>;
}

/// Why [`Owned::with_mut`] / [`Owned::with_ref`] could not run the closure.
///
/// The only three failure modes of the state machine (spec Arc 2 §5); each
/// has exactly one mapping per binding, implemented once in `binding::kind`
/// (WP-A2): `Closed` → the binding's `CLOSED` kind, `Poisoned` → its
/// internal-error kind, `Panicked` → its panic kind.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandleState {
    /// The slot is empty: [`Owned::close`] or [`Owned::take`] ran.
    Closed,
    /// The mutex is poisoned and the operation is a mutator, which the
    /// policy refuses. Readers keep recovering; `close()` still works.
    Poisoned,
    /// The closure panicked; `detail` is [`panic::payload_message`] of the
    /// payload. The mutex is NOT poisoned by this; whether the slot survives
    /// depends on the call: a mutator panic drops `T` (later calls report
    /// `Closed`), a reader panic keeps it.
    Panicked {
        /// Panic message (or `"non-string panic payload"`).
        detail: String,
    },
}

/// Why [`Owned::close`] did not complete cleanly.
#[derive(Debug)]
pub enum CloseFailure<E> {
    /// [`Close::close`] returned this error. The slot is already empty.
    Inner(E),
    /// [`Close::close`] panicked; the value was dropped after the unwind
    /// was caught. The slot is already empty.
    Panicked {
        /// Panic message (or `"non-string panic payload"`).
        detail: String,
    },
}

/// A binding's ownership of one pipeline value `T`, plus the two things a
/// cross-thread caller may touch without the slot: the cancel handle and
/// the construction-time snapshot `S`.
///
/// See the module docs for the rules. `S` defaults to `()` for shells
/// with no construction-constant getters.
pub struct Owned<T, S = ()> {
    inner: Mutex<Option<T>>,
    /// The latch + the value's transport-side handle, as ONE shareable
    /// `TransportCancel` — what [`Self::cancel_arc`] hands out, so a
    /// cancel fired through a handed-out handle is visible to
    /// [`Self::is_cancelled`] exactly like [`Self::cancel`].
    cancel: Arc<OwnedCancel>,
    end_reason: Option<RecvEndReasonHandle>,
    snapshot: S,
}

/// The cancel handle an [`Owned`] owns and shares: fires the transport's
/// handle, then latches. Private — reached only as `Arc<dyn TransportCancel>`
/// through [`Owned::cancel_arc`].
struct OwnedCancel {
    transport: Arc<dyn TransportCancel>,
    cancelled: AtomicBool,
}

impl TransportCancel for OwnedCancel {
    fn cancel(&self) {
        self.transport.cancel();
        self.cancelled.store(true, Ordering::SeqCst);
    }
    // WP-C1 adds here: `fn is_cancelled(&self) -> bool { self.cancelled.load(SeqCst) || self.transport.is_cancelled() }`
}

impl<T, S> Owned<T, S> {
    /// Wrap `inner`. `cancel` is the value's own cancel handle (every shell
    /// has one after WP-D; the managed wrappers' `ManagedCancel`, a
    /// transport's `cancel_handle()`); `snapshot` is whatever the binding's
    /// constant getters must answer without the slot.
    pub fn new(inner: T, cancel: Arc<dyn TransportCancel>, snapshot: S) -> Self {
        Self {
            inner: Mutex::new(Some(inner)),
            cancel: Arc::new(OwnedCancel {
                transport: cancel,
                cancelled: AtomicBool::new(false),
            }),
            end_reason: None,
            snapshot,
        }
    }

    /// The cancel handle as a shareable `Arc<dyn TransportCancel>` — for
    /// `tst_*_cancel_handle` / Python `cancel_handle()` / JVM
    /// `cancelHandle()`. A plain `Arc` clone: never touches the slot, so it
    /// can be obtained while another thread is parked in
    /// [`Self::with_mut`] (the #189 lease class). `cancel()` on the returned
    /// handle is [`Self::cancel`]: it fires the transport handle AND latches
    /// [`Self::is_cancelled`].
    pub fn cancel_arc(&self) -> Arc<dyn TransportCancel> {
        Arc::clone(&self.cancel) as Arc<dyn TransportCancel>
    }

    /// Attach the receiver's [`RecvEndReasonHandle`] (obtained BEFORE the
    /// receiver moved into `inner`), so [`Self::end_reason`] answers after
    /// the value is gone. Senders never call this.
    pub fn with_end_reason(mut self, h: RecvEndReasonHandle) -> Self {
        self.end_reason = Some(h);
        self
    }

    /// Fire the cancel handle and latch [`Self::is_cancelled`]. Never takes
    /// the slot: safe from any thread while another is parked inside
    /// [`Self::with_mut`] — that is the whole point.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// `true` once [`Self::cancel`] (or [`Self::close`], or `cancel()` on a
    /// handle from [`Self::cancel_arc`]) has run. Until WP-C1 lands
    /// `TransportCancel::is_cancelled`, this reads only the local latch;
    /// WP-C1 ORs in `self.cancel.transport.is_cancelled()`.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.cancelled.load(Ordering::SeqCst)
    }

    /// The construction-time snapshot. Lock-free by construction — a plain
    /// field read — so a getter routed here can never park behind a
    /// blocked `with_mut` (PR #234's five getters did exactly that).
    pub fn snapshot(&self) -> &S {
        &self.snapshot
    }

    /// The recorded end reason, or `None` when no handle was attached or
    /// the stream has not ended. Lock-free (the handle is a `OnceLock`).
    pub fn end_reason(&self) -> Option<RecvEndReason> {
        self.end_reason.as_ref().and_then(RecvEndReasonHandle::get)
    }

    /// Run `f` on `&mut T` — the ONE lock site. Bindings call this with the
    /// GIL released / outside the JNI critical region, and hold no mutex
    /// of their own around it.
    ///
    /// # Errors
    ///
    /// [`HandleState::Poisoned`] — the mutex is poisoned (a mutator refuses,
    /// spec §3.2); [`HandleState::Closed`] — the slot is empty;
    /// [`HandleState::Panicked`] — `f` panicked. In the last case the guard
    /// was held OUTSIDE the catch boundary, so the mutex is not poisoned —
    /// but a panic mid-mutation leaves `T` in an unknown state, so the slot
    /// is DROPPED: the panicking call reports `Panicked` and every later
    /// call reports `Closed` (the std-poisoning model; also what the C and
    /// JVM bindings did before Arc 2). Readers ([`Self::with_ref`]) keep
    /// the slot: a `&T` closure can only mutate through interior mutability,
    /// and every such interior (transport mutex, atomics) carries its own
    /// poison/latch rule — A1.9 records the per-site audit.
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut T) -> R) -> Result<R, HandleState> {
        let mut guard = self.inner.lock().map_err(|_| HandleState::Poisoned)?;
        if guard.is_none() {
            return Err(HandleState::Closed);
        }
        match panic::catch(|| f(guard.as_mut().expect("checked non-empty above"))) {
            Ok(r) => Ok(r),
            Err(detail) => {
                *guard = None; // drop T here, under the lock
                Err(HandleState::Panicked { detail })
            }
        }
    }

    /// Run `f` on `&T` (stats, `is_alive`, `repr`). Recovers a poisoned
    /// mutex; otherwise the same contract as [`Self::with_mut`].
    ///
    /// # Errors
    ///
    /// [`HandleState::Closed`], [`HandleState::Panicked`] — never `Poisoned`.
    pub fn with_ref<R>(&self, f: impl FnOnce(&T) -> R) -> Result<R, HandleState> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let t = guard.as_ref().ok_or(HandleState::Closed)?;
        panic::catch(|| f(t)).map_err(|detail| HandleState::Panicked { detail })
    }

    /// Take the value out (consuming ops: `into_inner`, `finish`). Recovers a
    /// poisoned mutex. `None` if already taken or closed.
    pub fn take(&self) -> Option<T> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// `true` when the slot is empty. NON-BLOCKING: a slot another thread
    /// currently holds (a parked receive) is by definition still open, so
    /// this answers `false` without waiting — the `is_alive()` / `repr()`
    /// probes the bindings run from a watchdog thread must never queue
    /// behind the parked call (the PR #234 class). Recovers a poisoned
    /// mutex.
    pub fn is_closed(&self) -> bool {
        match self.inner.try_lock() {
            Ok(guard) => guard.is_none(),
            Err(TryLockError::Poisoned(p)) => p.into_inner().is_none(),
            Err(TryLockError::WouldBlock) => false,
        }
    }

    /// Non-blocking [`Self::with_ref`]. `None` when another thread holds
    /// the slot — the caller decides what "busy" means (Python's
    /// `is_alive()` reports `True`, a `repr()` prints "busy"); otherwise
    /// exactly `Some(with_ref(f))`: `Err(Closed)` on an empty slot,
    /// `Err(Panicked)` if `f` panics, poison recovered.
    pub fn try_with_ref<R>(&self, f: impl FnOnce(&T) -> R) -> Option<Result<R, HandleState>> {
        let guard = match self.inner.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        let Some(t) = guard.as_ref() else {
            return Some(Err(HandleState::Closed));
        };
        Some(panic::catch(|| f(t)).map_err(|detail| HandleState::Panicked { detail }))
    }
}

impl<T: Close, S> Owned<T, S> {
    /// Cancel-first close, the Arc 1 contract enforced once:
    ///
    /// 1. [`Self::cancel`] — a thread parked inside [`Self::with_mut`]
    ///    returns (its operation fails with `ExplicitClose`) and releases
    ///    the slot;
    /// 2. [`Self::take`] — recovers a poisoned mutex, empties the slot;
    /// 3. [`Close::close`] on the taken value, OUTSIDE the lock, inside a
    ///    panic boundary.
    ///
    /// A second call finds the slot empty and returns `Ok(())` (the cancel
    /// handle is fired again — idempotent by the `TransportCancel`
    /// contract). Never panics, so a binding's `Drop` may call it.
    ///
    /// # Errors
    ///
    /// [`CloseFailure::Inner`] / [`CloseFailure::Panicked`] from step 3; in
    /// both cases the slot is already empty and a retry is the quiet
    /// double close.
    pub fn close(&self) -> Result<(), CloseFailure<T::Error>> {
        self.cancel();
        let Some(mut value) = self.take() else {
            return Ok(());
        };
        match panic::catch(|| value.close()) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(CloseFailure::Inner(e)),
            Err(detail) => Err(CloseFailure::Panicked { detail }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// Cancel handle that counts calls and flips a flag `Parked` polls.
    struct MockCancel {
        calls: AtomicU32,
        flag: Arc<AtomicBool>,
    }
    impl TransportCancel for MockCancel {
        fn cancel(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.flag.store(true, Ordering::SeqCst);
        }
    }

    /// The `T` under test: a counter plus the cancel flag it shares with
    /// `MockCancel`, and a `Close` impl that logs + counts.
    struct Mock {
        n: u32,
        flag: Arc<AtomicBool>,
        closes: Arc<AtomicU32>,
        log: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }
    #[derive(Debug)]
    struct MockCloseError;
    impl core::fmt::Display for MockCloseError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("mock close failed")
        }
    }
    impl Close for Mock {
        type Error = MockCloseError;
        fn close(&mut self) -> Result<(), MockCloseError> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            self.log.lock().unwrap().push("close");
            if self.n == u32::MAX {
                Err(MockCloseError)
            } else {
                Ok(())
            }
        }
    }

    /// A fresh `Owned<Mock, &'static str>` plus the shared observables.
    struct Fixture {
        owned: Arc<Owned<Mock, &'static str>>,
        cancel: Arc<MockCancel>,
        flag: Arc<AtomicBool>,
        closes: Arc<AtomicU32>,
        log: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }
    fn fixture() -> Fixture {
        let flag = Arc::new(AtomicBool::new(false));
        let closes = Arc::new(AtomicU32::new(0));
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cancel = Arc::new(MockCancel {
            calls: AtomicU32::new(0),
            flag: Arc::clone(&flag),
        });
        let mock = Mock {
            n: 0,
            flag: Arc::clone(&flag),
            closes: Arc::clone(&closes),
            log: Arc::clone(&log),
        };
        let cancel_dyn: Arc<dyn TransportCancel> = Arc::clone(&cancel) as Arc<dyn TransportCancel>;
        let owned = Arc::new(Owned::new(mock, cancel_dyn, "snap"));
        Fixture {
            owned,
            cancel,
            flag,
            closes,
            log,
        }
    }

    // ---- construction + lock-free half (Task A1.2) ----

    #[test]
    fn fresh_handle_is_open_uncancelled_and_carries_its_snapshot() {
        let f = fixture();
        assert!(!f.owned.is_cancelled());
        assert_eq!(*f.owned.snapshot(), "snap");
        assert_eq!(f.owned.end_reason(), None, "no end-reason handle attached");
    }

    #[test]
    fn cancel_fires_the_handle_and_latches_is_cancelled() {
        let f = fixture();
        f.owned.cancel();
        assert!(f.owned.is_cancelled());
        assert!(
            f.flag.load(Ordering::SeqCst),
            "the transport-side handle was fired"
        );
        assert_eq!(f.cancel.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancel_arc_is_the_same_handle_and_latches_is_cancelled() {
        let f = fixture();
        let handed_out: Arc<dyn TransportCancel> = f.owned.cancel_arc();
        assert!(!f.owned.is_cancelled());
        handed_out.cancel();
        assert!(
            f.owned.is_cancelled(),
            "a cancel through the handed-out handle is visible to the Owned"
        );
        assert!(
            f.flag.load(Ordering::SeqCst),
            "and it reached the transport-side handle"
        );
        assert_eq!(f.cancel.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn end_reason_reads_the_attached_handle() {
        let f = fixture();
        let h = RecvEndReasonHandle::default();
        // Rebuild with the handle attached (the fixture's Owned is behind an Arc).
        let flag = Arc::new(AtomicBool::new(false));
        let mock = Mock {
            n: 0,
            flag: Arc::clone(&flag),
            closes: Arc::clone(&f.closes),
            log: Arc::clone(&f.log),
        };
        let owned = Owned::new(
            mock,
            Arc::new(MockCancel {
                calls: AtomicU32::new(0),
                flag,
            }) as Arc<dyn TransportCancel>,
            (),
        )
        .with_end_reason(h.clone());
        assert_eq!(owned.end_reason(), None, "not yet ended");
        h.record(RecvEndReason::Cancelled);
        assert_eq!(owned.end_reason(), Some(RecvEndReason::Cancelled));
    }

    #[test]
    fn owned_is_send_and_sync_for_a_send_inner() {
        fn assert_send_sync<X: Send + Sync>() {}
        assert_send_sync::<Owned<Mock, &'static str>>();
    }

    // ---- slot access, poison + panic policy (Task A1.3) ----

    /// Poison the fixture's mutex the only way std allows: unwind while a
    /// guard is held, on another thread (the test's own `with_mut` cannot,
    /// by design — see `with_mut_panic_does_not_poison`).
    fn poison(f: &Fixture) {
        let o = Arc::clone(&f.owned);
        let r = std::thread::spawn(move || {
            let _guard = o.inner.lock().unwrap();
            panic!("poisoning on purpose");
        })
        .join();
        assert!(r.is_err(), "the poisoning thread must have panicked");
        assert!(f.owned.inner.is_poisoned());
    }

    #[test]
    fn with_mut_and_with_ref_reach_the_value() {
        let f = fixture();
        assert_eq!(
            f.owned.with_mut(|m| {
                m.n += 5;
                m.n
            }),
            Ok(5)
        );
        assert_eq!(f.owned.with_ref(|m| m.n), Ok(5));
    }

    #[test]
    fn take_empties_the_slot_and_everything_after_is_closed() {
        let f = fixture();
        let taken = f.owned.take().expect("first take yields the value");
        assert_eq!(taken.n, 0);
        assert!(f.owned.take().is_none(), "second take: slot already empty");
        assert!(f.owned.is_closed());
        assert_eq!(f.owned.with_mut(|m| m.n), Err(HandleState::Closed));
        assert_eq!(f.owned.with_ref(|m| m.n), Err(HandleState::Closed));
    }

    #[test]
    fn with_mut_panic_is_reported_once_and_drops_the_slot() {
        let f = fixture();
        let r = f.owned.with_mut(|_| -> u32 { panic!("mutator boom") });
        assert_eq!(
            r,
            Err(HandleState::Panicked {
                detail: String::from("mutator boom")
            })
        );
        assert!(
            !f.owned.inner.is_poisoned(),
            "the guard lives outside the catch boundary"
        );
        assert_eq!(
            f.owned.with_mut(|m| {
                m.n += 1;
                m.n
            }),
            Err(HandleState::Closed),
            "a mutator panic closes the slot"
        );
        assert!(f.owned.is_closed());
        assert_eq!(
            f.closes.load(Ordering::SeqCst),
            0,
            "dropped, not Close::close()d — the state is unknown"
        );
        assert!(f.owned.close().is_ok(), "close() after a panic is quiet");
    }

    #[test]
    fn with_ref_panic_is_reported_and_keeps_the_slot() {
        let f = fixture();
        let r = f.owned.with_ref(|_| -> u32 { panic!("reader boom {}", 2) });
        assert_eq!(
            r,
            Err(HandleState::Panicked {
                detail: String::from("reader boom 2")
            })
        );
        assert!(!f.owned.inner.is_poisoned());
        assert_eq!(f.owned.with_ref(|m| m.n), Ok(0));
    }

    #[test]
    fn poisoned_mutex_refuses_the_mutator_and_recovers_for_readers() {
        let f = fixture();
        poison(&f);
        assert_eq!(
            f.owned.with_mut(|m| m.n),
            Err(HandleState::Poisoned),
            "mutator: refuse"
        );
        assert_eq!(f.owned.with_ref(|m| m.n), Ok(0), "reader: recover");
        assert!(!f.owned.is_closed(), "is_closed: recover (slot still Some)");
        assert!(f.owned.take().is_some(), "take: recover");
        assert!(f.owned.is_closed());
    }

    #[test]
    fn try_with_ref_uncontended_paths() {
        let f = fixture();
        assert_eq!(f.owned.try_with_ref(|m| m.n), Some(Ok(0)));
        let r = f.owned.try_with_ref(|_| -> u32 { panic!("probe boom") });
        assert_eq!(
            r,
            Some(Err(HandleState::Panicked {
                detail: String::from("probe boom")
            }))
        );
        assert!(!f.owned.inner.is_poisoned());
        poison(&f);
        assert_eq!(
            f.owned.try_with_ref(|m| m.n),
            Some(Ok(0)),
            "poison recovered, like with_ref"
        );
        assert!(f.owned.take().is_some());
        assert_eq!(
            f.owned.try_with_ref(|m| m.n),
            Some(Err(HandleState::Closed))
        );
    }

    // ---- close (Task A1.4) ----

    #[test]
    fn close_cancels_first_then_closes_once_and_empties_the_slot() {
        let f = fixture();
        // Outcome pin: the cancel handle fired (flag), the latch is set,
        // Mock::close ran once, the slot is empty. The ORDER of cancel vs
        // close is pinned separately by `close_fires_cancel_before_close_close`.
        assert!(f.owned.close().is_ok());
        assert!(f.flag.load(Ordering::SeqCst), "cancel handle fired");
        assert!(f.owned.is_cancelled());
        assert_eq!(
            f.closes.load(Ordering::SeqCst),
            1,
            "Close::close ran exactly once"
        );
        assert!(f.owned.is_closed());
        assert_eq!(f.owned.with_mut(|m| m.n), Err(HandleState::Closed));
    }

    #[test]
    fn close_fires_cancel_before_close_close() {
        // Order pin: Mock::close pushes "close"; a cancel wrapper that logs
        // "cancel" sits in front of the mock handle.
        struct LoggingCancel(Arc<std::sync::Mutex<Vec<&'static str>>>, Arc<MockCancel>);
        impl TransportCancel for LoggingCancel {
            fn cancel(&self) {
                self.0.lock().unwrap().push("cancel");
                self.1.cancel();
            }
        }
        let f = fixture();
        let log = Arc::clone(&f.log);
        let mock = Mock {
            n: 0,
            flag: Arc::clone(&f.flag),
            closes: Arc::clone(&f.closes),
            log: Arc::clone(&log),
        };
        let owned = Owned::new(
            mock,
            Arc::new(LoggingCancel(Arc::clone(&log), Arc::clone(&f.cancel)))
                as Arc<dyn TransportCancel>,
            (),
        );
        assert!(owned.close().is_ok());
        assert_eq!(*log.lock().unwrap(), vec!["cancel", "close"]);
    }

    #[test]
    fn second_close_is_quiet_and_does_not_close_again() {
        let f = fixture();
        assert!(f.owned.close().is_ok());
        assert!(f.owned.close().is_ok(), "double close is Ok(())");
        assert_eq!(f.closes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn close_surfaces_the_inner_error_and_still_empties_the_slot() {
        let f = fixture();
        assert!(f.owned.with_mut(|m| m.n = u32::MAX).is_ok()); // arms Mock::close to fail
        match f.owned.close() {
            Err(CloseFailure::Inner(MockCloseError)) => {}
            other => panic!("expected CloseFailure::Inner, got {other:?}"),
        }
        assert!(
            f.owned.is_closed(),
            "the value was taken before its close ran"
        );
        assert!(
            f.owned.close().is_ok(),
            "and the retry is the quiet double close"
        );
    }

    #[test]
    fn close_reports_a_panicking_close_and_still_empties_the_slot() {
        struct Explodes;
        impl Close for Explodes {
            type Error = MockCloseError;
            fn close(&mut self) -> Result<(), MockCloseError> {
                panic!("close boom")
            }
        }
        let flag = Arc::new(AtomicBool::new(false));
        let owned = Owned::new(
            Explodes,
            Arc::new(MockCancel {
                calls: AtomicU32::new(0),
                flag,
            }) as Arc<dyn TransportCancel>,
            (),
        );
        match owned.close() {
            Err(CloseFailure::Panicked { detail }) => assert_eq!(detail, "close boom"),
            other => panic!("expected CloseFailure::Panicked, got {other:?}"),
        }
        assert!(owned.is_closed());
        assert!(!owned.inner.is_poisoned());
    }

    #[test]
    fn close_recovers_a_poisoned_mutex() {
        let f = fixture();
        poison(&f);
        assert!(
            f.owned.close().is_ok(),
            "Drop-never-panics: close always recovers"
        );
        assert_eq!(f.closes.load(Ordering::SeqCst), 1);
    }

    // ---- concurrency: nothing but with_mut/with_ref/take/is_closed/close
    // waits on the slot (Task A1.5) ----

    const PARK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
    const PROMPT: std::time::Duration = std::time::Duration::from_secs(2);

    /// Latch-and-poll with a bounded watchdog — never a wall-clock assert.
    fn wait_for(deadline: std::time::Duration, f: impl Fn() -> bool) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        f()
    }

    /// Park a thread inside `with_mut` until the shared cancel flag flips
    /// (the shape of a receive parked in libsrt that only the cancel handle
    /// can return). Returns the thread + an `entered` latch.
    fn park_in_with_mut(
        f: &Fixture,
    ) -> (
        std::thread::JoinHandle<Result<u32, HandleState>>,
        Arc<AtomicBool>,
    ) {
        let entered = Arc::new(AtomicBool::new(false));
        let o = Arc::clone(&f.owned);
        let e = Arc::clone(&entered);
        let h = std::thread::spawn(move || {
            o.with_mut(|m| {
                e.store(true, Ordering::SeqCst);
                let start = std::time::Instant::now();
                while !m.flag.load(Ordering::SeqCst) {
                    assert!(
                        start.elapsed() < PARK_DEADLINE,
                        "parked closure was never released — watchdog"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                m.n += 1;
                m.n
            })
        });
        assert!(
            wait_for(PARK_DEADLINE, || entered.load(Ordering::SeqCst)),
            "never entered with_mut"
        );
        (h, entered)
    }

    #[test]
    fn cancel_returns_promptly_while_another_thread_is_parked_in_with_mut() {
        let f = fixture();
        let (parked, _) = park_in_with_mut(&f);
        // cancel() from THIS thread must return without the slot.
        let canceller = {
            let o = Arc::clone(&f.owned);
            std::thread::spawn(move || o.cancel())
        };
        assert!(
            wait_for(PROMPT, || canceller.is_finished()),
            "cancel() waited on the parked slot"
        );
        canceller.join().unwrap();
        assert!(f.owned.is_cancelled());
        // The parked closure observes the fired handle and returns.
        assert_eq!(parked.join().unwrap(), Ok(1));
    }

    #[test]
    fn snapshot_and_end_reason_and_is_cancelled_never_take_the_slot() {
        let f = fixture();
        let (parked, _) = park_in_with_mut(&f);
        let reader = {
            let o = Arc::clone(&f.owned);
            std::thread::spawn(move || {
                let handle = o.cancel_arc(); // must not lease the slot (the #189 class)
                (*o.snapshot(), o.end_reason(), o.is_cancelled(), handle)
            })
        };
        assert!(
            wait_for(PROMPT, || reader.is_finished()),
            "a snapshot getter or cancel_arc() waited on the parked slot (the PR #234 / #189 classes)"
        );
        let (snap, reason, cancelled, handle) = reader.join().unwrap();
        assert_eq!((snap, reason, cancelled), ("snap", None, false));
        handle.cancel(); // release the parked thread THROUGH the handed-out handle
        assert_eq!(parked.join().unwrap(), Ok(1));
        assert!(f.owned.is_cancelled());
    }

    #[test]
    fn close_from_another_thread_returns_the_parked_call_first() {
        let f = fixture();
        let (parked, _) = park_in_with_mut(&f);
        let closer = {
            let o = Arc::clone(&f.owned);
            std::thread::spawn(move || o.close())
        };
        assert!(
            wait_for(PROMPT, || closer.is_finished()),
            "close() blocked behind the parked with_mut"
        );
        assert!(closer.join().unwrap().is_ok());
        assert_eq!(
            parked.join().unwrap(),
            Ok(1),
            "the parked call completed (released by cancel), then close took the slot"
        );
        assert_eq!(f.closes.load(Ordering::SeqCst), 1);
        assert!(f.owned.is_closed());
    }

    #[test]
    fn is_closed_and_try_with_ref_never_wait_on_a_parked_slot() {
        let f = fixture();
        let (parked, _) = park_in_with_mut(&f);
        let prober = {
            let o = Arc::clone(&f.owned);
            std::thread::spawn(move || (o.is_closed(), o.try_with_ref(|m| m.n)))
        };
        assert!(
            wait_for(PROMPT, || prober.is_finished()),
            "a non-blocking probe waited on the parked slot (the PR #234 class)"
        );
        assert_eq!(
            prober.join().unwrap(),
            (false, None),
            "busy slot = still open; the probe declines rather than waits"
        );
        f.owned.cancel(); // release the parked thread
        let _ = parked.join().unwrap();
    }

    #[test]
    fn owned_cancel_fires_the_transport_handle_before_latching() {
        // Order pin for the private `OwnedCancel`, deterministic — no polling:
        // the transport handle reads the `Owned`'s own latch from INSIDE its
        // `cancel()`. Observing `false` there proves the transport fired
        // first and the latch was set after. `Weak` keeps the handle's
        // back-reference from forming an `Arc` cycle.
        struct OrderCancel {
            owned: std::sync::OnceLock<std::sync::Weak<Owned<u8, ()>>>,
            latch_seen_from_transport: std::sync::Mutex<Option<bool>>,
        }
        impl TransportCancel for OrderCancel {
            fn cancel(&self) {
                let owned = self
                    .owned
                    .get()
                    .expect("wired immediately after construction")
                    .upgrade()
                    .expect("the Owned outlives this call");
                *self.latch_seen_from_transport.lock().unwrap() = Some(owned.is_cancelled());
            }
        }

        let c = Arc::new(OrderCancel {
            owned: std::sync::OnceLock::new(),
            latch_seen_from_transport: std::sync::Mutex::new(None),
        });
        let owned = Arc::new(Owned::new(
            0u8,
            Arc::clone(&c) as Arc<dyn TransportCancel>,
            (),
        ));
        c.owned
            .set(Arc::downgrade(&owned))
            .expect("set exactly once");

        owned.cancel();
        assert_eq!(
            *c.latch_seen_from_transport.lock().unwrap(),
            Some(false),
            "the transport handle fired BEFORE is_cancelled latched"
        );
        assert!(
            owned.is_cancelled(),
            "and the latch is set once cancel returns"
        );
    }

    #[test]
    fn owned_is_send_and_sync_for_a_send_but_not_sync_inner() {
        // The other half of the auto-trait claim: `Mutex<Option<T>>` is `Sync`
        // iff `T: Send`, so a `!Sync` inner still yields a `Send + Sync`
        // handle. `Cell<u32>` is `Send` but not `Sync`.
        fn assert_send_sync<X: Send + Sync>() {}
        assert_send_sync::<Owned<core::cell::Cell<u32>, ()>>();
    }

    #[test]
    fn close_runs_the_inner_close_outside_the_lock() {
        // `close()` takes the value OUT of the slot and only then runs
        // `Close::close`. So while the inner close is parked, the slot is
        // empty AND unlocked: a BLOCKING `with_ref` from another thread must
        // answer `Err(Closed)` immediately instead of queueing behind it.
        struct GatedClose {
            gate: Arc<AtomicBool>,
            in_close: Arc<AtomicBool>,
        }
        impl Close for GatedClose {
            type Error = MockCloseError;
            fn close(&mut self) -> Result<(), MockCloseError> {
                self.in_close.store(true, Ordering::SeqCst);
                let start = std::time::Instant::now();
                while !self.gate.load(Ordering::SeqCst) {
                    assert!(
                        start.elapsed() < PARK_DEADLINE,
                        "close gate was never opened — watchdog"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Ok(())
            }
        }

        let gate = Arc::new(AtomicBool::new(false));
        let in_close = Arc::new(AtomicBool::new(false));
        let flag = Arc::new(AtomicBool::new(false));
        let owned = Arc::new(Owned::new(
            GatedClose {
                gate: Arc::clone(&gate),
                in_close: Arc::clone(&in_close),
            },
            Arc::new(MockCancel {
                calls: AtomicU32::new(0),
                flag,
            }) as Arc<dyn TransportCancel>,
            (),
        ));

        let closer = {
            let o = Arc::clone(&owned);
            std::thread::spawn(move || o.close())
        };
        assert!(
            wait_for(PARK_DEADLINE, || in_close.load(Ordering::SeqCst)),
            "never entered Close::close"
        );

        let reader = {
            let o = Arc::clone(&owned);
            std::thread::spawn(move || o.with_ref(|_| ()))
        };
        assert!(
            wait_for(PROMPT, || reader.is_finished()),
            "with_ref queued behind Close::close — the inner close is running under the lock"
        );
        assert_eq!(
            reader.join().unwrap(),
            Err(HandleState::Closed),
            "the value was taken before its close ran"
        );

        gate.store(true, Ordering::SeqCst);
        assert!(closer.join().unwrap().is_ok());
    }
}
