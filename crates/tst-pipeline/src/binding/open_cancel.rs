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
//! - What a cancelled slot wakes is the opener's business: a listener-mode
//!   first accept at any time; a caller-mode dial is not interrupted (the
//!   dial runs on, and the outcome rule below decides what the host sees).
//! - **One outcome rule for every open: an open reports `CLOSED` whenever
//!   the object was cancelled by the time the open finished, whatever the
//!   open itself did.** The open ends by calling [`OpenSlot::finish`],
//!   which unregisters its slot and reads the latch in one step under the
//!   object's lock. `true`: the binding closes whatever the open returned
//!   (a transport or shell that had just opened is dropped; a failed
//!   open's own error is replaced) and reports `ExplicitClose` / `CLOSED`.
//!   `false`: the open is returned whole, and the object never touches its
//!   slot again. This is the one place where a cancel beats a success, and
//!   nothing is lost by it: the open has handed nothing out yet.
//! - So the object is NOT the opened shell's cancel handle. After an open
//!   returned, `cancel()` does nothing to its shell, a managed receiver
//!   included (it keeps the slot for its re-accepts and re-dials, but the
//!   object no longer holds it, so its reconnects go on); the shell's own
//!   `cancel_handle()` is its cancel from then on.
//! - An opener that takes no slot (every sender, every non-SRT scheme)
//!   follows the same steps: the binding takes a slot all the same, checks
//!   it before the open and calls `finish()` after it.
//!
//! A binding's open, for every kind:
//!
//! ```ignore
//! let slot = open_cancel.slot();
//! if slot.is_cancelled() {
//!     return Err(closed()); // before the URL is even parsed
//! }
//! let r = open(url, &slot); // `&slot` only where the opener takes one
//! if slot.finish() {
//!     drop(r);
//!     return Err(closed());
//! }
//! r
//! ```
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

    /// Register a fresh slot for one open and return it in a guard. Pass
    /// the slot to the opener (`&*guard` is the `&Arc<CancelSlot>` the
    /// openers take) and end the open with [`OpenSlot::finish`]. A guard
    /// that is only dropped (a panic, an early return) unregisters too.
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
    /// Later calls do nothing more. The latch is set and the registered
    /// slots are taken in one critical section; the slots are cancelled
    /// after the object's lock is released (an SRT slot's cancel closes
    /// the listening socket and can block). If one slot's cancel panics,
    /// the rest are still cancelled while the panic unwinds.
    ///
    /// A second `cancel()` that runs while the first is still cancelling
    /// slots returns at once: it may return before every open has been
    /// reached. Do not free anything an open uses because `cancel()`
    /// returned; an open's own result (through [`OpenSlot::finish`]) is
    /// the answer.
    pub fn cancel(&self) {
        let taken = {
            let mut s = self.lock();
            s.cancelled = true;
            core::mem::take(&mut s.open)
        };
        let mut rest = FireOnUnwind(taken.into_iter());
        for slot in rest.0.by_ref() {
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

/// Cancels the slots a panicking slot cancel left behind, as the panic
/// unwinds out of [`OpenCancel::cancel`]; empty on the normal path.
struct FireOnUnwind(std::vec::IntoIter<Arc<CancelSlot>>);

impl Drop for FireOnUnwind {
    fn drop(&mut self) {
        for slot in self.0.by_ref() {
            slot.cancel();
        }
    }
}

/// One open's slot, registered with its [`OpenCancel`] until
/// [`finish`](Self::finish) (or a drop). Dereferences to the
/// `Arc<CancelSlot>` the openers take.
pub struct OpenSlot {
    owner: OpenCancel,
    slot: Arc<CancelSlot>,
}

impl OpenSlot {
    /// End the open: unregister the slot and say whether the object was
    /// cancelled by now. `true`: report `ExplicitClose` / `CLOSED` and drop
    /// whatever the open returned. `false`: return the open whole.
    ///
    /// Both happen under the object's lock, and `cancel()` sets the latch
    /// and takes the registered slots under the same lock in one step. So
    /// `false` means no `cancel()` had taken the slot (it would have set
    /// the latch first) and none ever will (the slot is no longer
    /// registered): the object never touches this slot again, however a
    /// concurrent `cancel()` is scheduled.
    #[must_use = "true means the open must be closed and reported as CLOSED"]
    pub fn finish(self) -> bool {
        self.unregister()
    }

    /// Remove the slot from the registry (if still there) and read the
    /// latch, in one critical section.
    fn unregister(&self) -> bool {
        let mut s = self.owner.lock();
        if let Some(i) = s.open.iter().position(|o| Arc::ptr_eq(o, &self.slot)) {
            s.open.swap_remove(i);
        }
        s.cancelled
    }
}

impl core::ops::Deref for OpenSlot {
    type Target = Arc<CancelSlot>;

    fn deref(&self) -> &Arc<CancelSlot> {
        &self.slot
    }
}

impl Drop for OpenSlot {
    fn drop(&mut self) {
        // After `finish` this finds nothing to remove.
        let _ = self.unregister();
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

    /// An open that finished before the cancel: `finish()` is false, and the
    /// object no longer reaches the slot, though an opener (a managed
    /// receiver) may still hold it.
    #[test]
    fn a_cancel_after_the_open_finished_leaves_its_slot_alone() {
        let oc = OpenCancel::new();
        let guard = oc.slot();
        let kept: Arc<CancelSlot> = Arc::clone(&guard);
        assert!(!guard.finish());
        oc.cancel();
        assert!(oc.is_cancelled());
        assert!(!kept.is_cancelled());
    }

    /// An open that finishes after the cancel (it had opened, or failed,
    /// or was refused): `finish()` is true.
    #[test]
    fn finish_after_the_cancel_is_true() {
        let oc = OpenCancel::new();
        let in_progress = oc.slot();
        oc.cancel();
        let refused = oc.slot();
        assert!(in_progress.finish());
        assert!(refused.finish());
    }

    /// The target of the first registered slot blocks inside `cancel()`'s
    /// fire loop until released: `cancel()` has set the latch and taken
    /// every slot, and has not fired the second one yet.
    struct Hold {
        firing: Mutex<mpsc::Sender<()>>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl TransportCancel for Hold {
        fn cancel(&self) {
            let _ = self.firing.lock().unwrap().send(());
            let _ = self.release.lock().unwrap().recv_timeout(DEADLINE);
        }
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    /// Register a slot whose target holds `cancel()`'s fire loop until
    /// released: slots registered after it are fired after it.
    fn hold_slot(oc: &OpenCancel) -> (OpenSlot, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let held = oc.slot();
        let (firing_tx, firing_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        held.install(Arc::new(Hold {
            firing: Mutex::new(firing_tx),
            release: Mutex::new(release_rx),
        }));
        (held, firing_rx, release_tx)
    }

    /// `finish()` false means no `cancel()` ever touches the slot, however
    /// the cancel is scheduled.
    #[test]
    fn finish_false_means_no_cancel_ever_touches_the_slot() {
        // The open finishes while a cancel has set the latch and taken the
        // slots, and has not fired this one yet: true, never a whole open
        // whose slot is latched afterwards.
        let oc = OpenCancel::new();
        let (held, firing, release) = hold_slot(&oc);
        let open = oc.slot();
        let kept = Arc::clone(&open);
        let canceller = {
            let oc = oc.clone();
            thread::spawn(move || oc.cancel())
        };
        firing
            .recv_timeout(DEADLINE)
            .expect("cancel() reached the held slot");
        let finished = open.finish();
        release.send(()).unwrap();
        canceller.join().unwrap();
        drop(held);
        assert!(
            finished || !kept.is_cancelled(),
            "finish() said the open was whole, then the cancel latched its slot"
        );
        assert!(finished, "the latch was set before the open finished");

        // The open finishes before the cancel: false, and the cancel that
        // follows never reaches its slot.
        let oc = OpenCancel::new();
        let (held, firing, release) = hold_slot(&oc);
        let open = oc.slot();
        let kept = Arc::clone(&open);
        assert!(!open.finish());
        let canceller = {
            let oc = oc.clone();
            thread::spawn(move || oc.cancel())
        };
        firing
            .recv_timeout(DEADLINE)
            .expect("cancel() reached the held slot");
        release.send(()).unwrap();
        canceller.join().unwrap();
        drop(held);
        assert!(!kept.is_cancelled());
    }

    /// A slot whose cancel panics does not leave the slots taken after it
    /// unfired.
    #[test]
    fn a_panicking_slot_cancel_still_fires_the_rest() {
        struct Panics;
        impl TransportCancel for Panics {
            fn cancel(&self) {
                panic!("a target that panics");
            }
            fn is_cancelled(&self) -> bool {
                false
            }
        }
        let oc = OpenCancel::new();
        let first = oc.slot();
        first.install(Arc::new(Panics));
        let second = oc.slot();
        let (tx, rx) = mpsc::channel();
        second.install(Arc::new(Wake(Mutex::new(tx))));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| oc.cancel()));
        assert!(r.is_err(), "the panic propagates");
        assert!(rx.try_recv().is_ok(), "the second slot was fired");
        assert!(second.is_cancelled());
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
