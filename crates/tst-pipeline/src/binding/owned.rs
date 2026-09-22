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
}
