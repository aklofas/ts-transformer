//! [`OpenCancel`] — a host's cancel for opens that have no handle yet.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! A listener-mode open blocks in its first accept before any shell (and
//! so any `cancel_handle()`) exists. The openers already take a
//! [`CancelSlot`] for that accept, but one slot holds one target: two
//! opens sharing it would overwrite each other's target, and the first to
//! return would clear the other's. So the object a binding exposes hands
//! every open its own fresh slot and cancels all of them at once.
//!
//! The rules, which the bindings' classes inherit:
//!
//! - One object may be given to any number of opens, one after another or
//!   at the same time.
//! - [`OpenCancel::cancel`] is sticky. It cancels the slot of every open
//!   in progress, and an open started after it gets a slot that is
//!   already cancelled, so an opener that takes the slot refuses it with
//!   `TransportError::ExplicitClose` (binding kind `CLOSED`) before it
//!   binds or dials.
//! - What a cancelled slot reaches is the opener's business: a
//!   listener-mode first accept at any time; a caller-mode open before
//!   it dials. A cancel that arrives during a dial does not interrupt it:
//!   if the dial then succeeds the open returns a live shell (end it
//!   through its own cancel handle; on a managed receiver the next re-dial
//!   is refused), and if it fails the open returns `ExplicitClose`.
//! - An opener that takes no slot (every sender, every non-SRT scheme) is
//!   reached only through the binding, under the same rule: it checks
//!   [`OpenCancel::is_cancelled`] before the open and refuses with
//!   `ExplicitClose`, and it reports an open that failed while the object
//!   was cancelled as `ExplicitClose` too.
//! - The object is NOT the opened shell's cancel handle. An open's slot is
//!   registered only while its [`OpenSlot`] lives; drop that guard as soon
//!   as the open returns. A later `cancel()` then does nothing to the
//!   shell, a managed receiver included: the managed receiver keeps the
//!   slot for its re-accepts and re-dials, but the object no longer holds
//!   it, so its reconnects go on. The shell's own `cancel_handle()` is its
//!   cancel from then on.
//! - A cancel that lands after the open has finished but before its guard
//!   is dropped does not undo the open (a success wins). The slot stays
//!   cancelled, though, so a managed receiver opened that way ends with
//!   `ExplicitClose` at its next reconnect instead of reconnecting.
//!
//! `std`-only, like [`CancelSlot`].

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tst_core::cancel::CancelSlot;

/// A sticky cancel for opens in progress, shared by any number of them.
///
/// Cheap to clone: every clone is the same object (one latch, one set of
/// registered slots), so a host can hand one clone to each thread that
/// opens and keep one to cancel with. `Send + Sync`; every method may be
/// called from any thread at any time, any number of times.
#[derive(Clone, Default)]
pub struct OpenCancel {
    shared: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    cancelled: bool,
    /// The slots of the opens in progress. Emptied by `cancel()`, and never
    /// filled again once the latch is set.
    open: Vec<Arc<CancelSlot>>,
}

impl OpenCancel {
    /// A new object, not cancelled, with no open registered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a fresh slot for one open and return it in a guard that
    /// unregisters it on drop. Pass the slot to the opener
    /// (`&*guard` is the `&Arc<CancelSlot>` the openers take) and drop the
    /// guard when the open returns.
    ///
    /// Once the object is cancelled the slot comes back already cancelled
    /// and is not registered, so the opener refuses the open. The latch is
    /// read under the same lock `cancel()` sets it under, so an open that
    /// registers while `cancel()` runs is either in the set `cancel()`
    /// fires or sees the latch: it is never left parked.
    #[must_use]
    pub fn slot(&self) -> OpenSlot {
        let slot = Arc::new(CancelSlot::new());
        let cancelled = {
            let mut s = self.lock();
            if !s.cancelled {
                s.open.push(Arc::clone(&slot));
            }
            s.cancelled
        };
        if cancelled {
            // Nothing is installed in a fresh slot: this only latches it.
            slot.cancel();
        }
        OpenSlot {
            owner: self.clone(),
            slot,
        }
    }

    /// Latch the object and cancel the slot of every open in progress.
    /// Later calls do nothing more. The slots are cancelled after the
    /// object's lock is released: an SRT slot's cancel closes the
    /// listening socket and can block.
    pub fn cancel(&self) {
        let open = {
            let mut s = self.lock();
            s.cancelled = true;
            core::mem::take(&mut s.open)
        };
        for slot in open {
            slot.cancel();
        }
    }

    /// `true` once [`cancel`](Self::cancel) has run.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    // Recover on poison: the state is a flag and a list, never left
    // half-updated by a panic.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl core::fmt::Debug for OpenCancel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = self.lock();
        f.debug_struct("OpenCancel")
            .field("cancelled", &s.cancelled)
            .field("opens_in_progress", &s.open.len())
            .finish()
    }
}

/// One open's slot, registered with its [`OpenCancel`] until this guard
/// is dropped. Dereferences to the `Arc<CancelSlot>` the openers take.
pub struct OpenSlot {
    owner: OpenCancel,
    slot: Arc<CancelSlot>,
}

impl core::ops::Deref for OpenSlot {
    type Target = Arc<CancelSlot>;

    fn deref(&self) -> &Arc<CancelSlot> {
        &self.slot
    }
}

impl Drop for OpenSlot {
    fn drop(&mut self) {
        let mut s = self.owner.lock();
        if let Some(i) = s.open.iter().position(|o| Arc::ptr_eq(o, &self.slot)) {
            s.open.swap_remove(i);
        }
    }
}

impl core::fmt::Debug for OpenSlot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OpenSlot")
            .field("cancelled", &self.slot.is_cancelled())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    use tst_core::transport::TransportCancel;

    /// Far longer than any healthy wake takes; a test fails, not hangs.
    const DEADLINE: Duration = Duration::from_secs(10);

    /// The target a fake open installs: its cancel wakes the open.
    struct Wake(Mutex<mpsc::Sender<()>>);

    impl TransportCancel for Wake {
        fn cancel(&self) {
            let _ = self.0.lock().unwrap().send(());
        }
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    #[derive(Debug, PartialEq)]
    enum Outcome {
        /// Refused before it parked (the slot was already cancelled).
        Refused,
        /// Parked, then woken by the slot's cancel.
        Cancelled,
        /// Still parked at the deadline: nothing reached it.
        Parked,
    }

    /// A stand-in for `accept_one_cancellable`: refuse a cancelled slot,
    /// install a wake target, signal `parked`, wait for the wake.
    fn fake_open(slot: &CancelSlot, parked: Option<mpsc::Sender<()>>) -> Outcome {
        if slot.is_cancelled() {
            return Outcome::Refused;
        }
        let (tx, rx) = mpsc::channel();
        slot.install(Arc::new(Wake(Mutex::new(tx))));
        if let Some(p) = parked {
            let _ = p.send(());
        }
        let woke = rx.recv_timeout(DEADLINE).is_ok();
        slot.clear();
        if woke {
            Outcome::Cancelled
        } else {
            Outcome::Parked
        }
    }

    fn registered(oc: &OpenCancel) -> usize {
        oc.lock().open.len()
    }

    #[test]
    fn is_send_sync() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<OpenCancel>();
        assert_send_sync::<OpenSlot>();
    }

    #[test]
    fn one_cancel_ends_every_open_in_progress() {
        let oc = OpenCancel::new();
        let (parked_tx, parked_rx) = mpsc::channel();
        let opens: Vec<_> = (0..2)
            .map(|_| {
                let oc = oc.clone();
                let parked = parked_tx.clone();
                thread::spawn(move || {
                    let slot = oc.slot();
                    fake_open(&slot, Some(parked))
                })
            })
            .collect();
        for _ in 0..2 {
            parked_rx
                .recv_timeout(DEADLINE)
                .expect("both opens park before the cancel");
        }
        assert_eq!(registered(&oc), 2);
        oc.cancel();
        for open in opens {
            assert_eq!(open.join().unwrap(), Outcome::Cancelled);
        }
        assert!(oc.is_cancelled());
    }

    #[test]
    fn a_slot_taken_after_the_cancel_is_already_cancelled() {
        let oc = OpenCancel::new();
        assert!(!oc.is_cancelled());
        oc.cancel();
        let slot = oc.slot();
        assert!(slot.is_cancelled());
        assert_eq!(fake_open(&slot, None), Outcome::Refused);
        assert_eq!(registered(&oc), 0, "a refused open is never registered");
    }

    /// An open that registers while `cancel()` runs on another thread
    /// always ends: refused, or parked and woken. Even rounds cancel as
    /// soon as the opener is released; odd rounds once it holds its slot.
    #[test]
    fn an_open_racing_the_cancel_is_never_left_parked() {
        for round in 0..200 {
            let oc = OpenCancel::new();
            let (go_tx, go_rx) = mpsc::channel::<()>();
            let (registered_tx, registered_rx) = mpsc::channel::<()>();
            let opener = {
                let oc = oc.clone();
                thread::spawn(move || {
                    go_rx.recv().unwrap();
                    let slot = oc.slot();
                    registered_tx.send(()).unwrap();
                    fake_open(&slot, None)
                })
            };
            go_tx.send(()).unwrap();
            if round % 2 == 1 {
                registered_rx.recv_timeout(DEADLINE).unwrap();
            }
            oc.cancel();
            let outcome = opener.join().unwrap();
            assert_ne!(outcome, Outcome::Parked, "round {round}");
        }
    }

    #[test]
    fn finished_opens_leave_no_slot_behind() {
        let oc = OpenCancel::new();
        for _ in 0..1_000 {
            let slot = oc.slot();
            assert!(!slot.is_cancelled());
            drop(slot);
        }
        assert_eq!(registered(&oc), 0);

        let held = oc.slot();
        assert_eq!(registered(&oc), 1);
        oc.cancel();
        assert_eq!(registered(&oc), 0, "cancel() releases what it fired");
        drop(held);
        for _ in 0..1_000 {
            drop(oc.slot());
        }
        assert_eq!(registered(&oc), 0);
    }

    /// After the guard is dropped the object no longer reaches the slot,
    /// though an opener (a managed receiver) may still hold it.
    #[test]
    fn a_cancel_after_the_open_returned_leaves_its_slot_alone() {
        let oc = OpenCancel::new();
        let guard = oc.slot();
        let kept: Arc<CancelSlot> = Arc::clone(&guard);
        drop(guard);
        oc.cancel();
        assert!(oc.is_cancelled());
        assert!(!kept.is_cancelled());
    }

    /// The slots are cancelled with the object's lock released: a target
    /// that calls back into the object does not deadlock.
    #[test]
    fn cancel_fires_the_slots_without_holding_the_lock() {
        struct Reenter(OpenCancel, mpsc::Sender<bool>);
        impl TransportCancel for Reenter {
            fn cancel(&self) {
                let _ = self.1.send(self.0.is_cancelled());
                drop(self.0.slot());
            }
            fn is_cancelled(&self) -> bool {
                false
            }
        }
        let oc = OpenCancel::new();
        // Not dropped on a failure: its drop would wait on the lock a
        // stuck canceller holds, and the test would hang, not fail.
        let guard = core::mem::ManuallyDrop::new(oc.slot());
        let (tx, rx) = mpsc::channel();
        guard.install(Arc::new(Reenter(oc.clone(), tx)));
        let canceller = {
            let oc = oc.clone();
            thread::spawn(move || oc.cancel())
        };
        assert!(
            rx.recv_timeout(DEADLINE).expect("the target ran"),
            "the latch is set before the targets fire"
        );
        canceller.join().unwrap();
        drop(core::mem::ManuallyDrop::into_inner(guard));
    }

    #[test]
    fn cancel_is_idempotent_from_many_threads() {
        let oc = OpenCancel::new();
        let guard = oc.slot();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let oc = oc.clone();
                thread::spawn(move || {
                    oc.cancel();
                    oc.cancel();
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(oc.is_cancelled());
        assert!(guard.is_cancelled());
    }
}
