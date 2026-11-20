//! [`ParkingSlot`] — a session object whose calls may park, and the one
//! rule for closing it while one does.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! A binding object over a session such as an RTSP push holds it in a
//! mutex. Some calls hold that mutex while they wait on the network (a send
//! waiting for room, `PAUSE` draining the queue); others only read it for a
//! moment. The rules, which the bindings' classes inherit:
//!
//! - A close fires the session's cancel first only for a call that HOLDS
//!   the slot and may park ([`ParkingSlot::run_parking`]), so that call
//!   ends and the close does not wait on the network for it. A query
//!   holding the slot ([`ParkingSlot::run`]) is waited for without a
//!   cancel, so the close stays clean.
//! - A call only waiting for the slot is no reason to cancel: it gets the
//!   slot after the close began, runs nothing and is refused with
//!   [`SlotRefused::Closed`].
//! - A conversion ([`ParkingSlot::take_for_conversion`]) waits behind a
//!   query and refuses ([`SlotRefused::Busy`]) only behind a parking
//!   call, taking nothing.
//! - [`ParkingSlot::try_peek`] never waits (`is_session_alive`).
//! - A panic inside a call poisons the slot: every later call but a close
//!   is refused with [`SlotRefused::Poisoned`] (a peek reads
//!   [`Peek::Closed`]), and a close still takes the value so the session
//!   ends.
//!
//! A close or a conversion that finds the slot held by a query retries in
//! 1 ms rounds, re-reading the parking marker each round, so a parking call
//! that starts while it waits is still cancelled (or refuses the
//! conversion).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::time::Duration;

use tst_core::transport::TransportCancel;

/// How long a close or a conversion waits before it looks at a held slot
/// again.
const RETRY: Duration = Duration::from_millis(1);

/// Why a call on a [`ParkingSlot`] ran nothing.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotRefused {
    /// A close or a conversion began, or took the value, before the call
    /// got the slot.
    Closed,
    /// A call panicked while it held the slot; only a close runs now.
    Poisoned,
    /// A conversion found a parking call holding the slot; nothing was
    /// taken and the session is as it was.
    Busy,
}

/// What [`ParkingSlot::try_peek`] saw, without waiting.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peek<R> {
    /// The slot was free; the closure's answer.
    Value(R),
    /// Another call holds the slot and no close has begun.
    Held,
    /// Closed, converted, closing, or poisoned.
    Closed,
}

/// What a [`ParkingSlot`] tells its observer (a binding's test hook).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkingEvent {
    /// A parking call starts waiting for the slot.
    ParkingEntered,
    /// A parking call has stopped waiting for the slot (it has the slot
    /// and has not yet looked at whether it may run).
    ParkingLeft,
    /// A close found the slot held by a query and waits a round.
    CloseRetry,
    /// A conversion found the slot held by a query and waits a round.
    ConversionRetry,
    /// A query starts waiting for the slot.
    QueryEntered,
}

/// Lowers the parking marker when a parking call ends, however it ends.
/// Declared after the mutex guard, so it drops first: the marker is down
/// before the slot is free.
struct Holding<'a>(&'a AtomicBool);

impl Drop for Holding<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// A value in a mutex, a cancel outside it, and the close rule of the
/// module docs.
pub struct ParkingSlot<T> {
    /// `None` once a close or a conversion took the value.
    value: Mutex<Option<T>>,
    /// Fired by a close that finds a parking call holding the slot.
    cancel: Arc<dyn TransportCancel>,
    /// Set when a close begins and when a conversion claims the value: a
    /// call that gets the slot afterwards runs nothing.
    closed: AtomicBool,
    /// Set while a parking call holds the slot and runs: raised after it
    /// has the slot and has found `closed` clear, lowered before it lets
    /// go. Queries and calls only waiting never set it.
    parking_holder: AtomicBool,
    observer: Option<fn(ParkingEvent)>,
}

impl<T> ParkingSlot<T> {
    /// A slot holding `value`; `cancel` ends a parked call on it.
    pub fn new(value: T, cancel: Arc<dyn TransportCancel>) -> Self {
        Self {
            value: Mutex::new(Some(value)),
            cancel,
            closed: AtomicBool::new(false),
            parking_holder: AtomicBool::new(false),
            observer: None,
        }
    }

    /// Report the slot's waits to `observer`, a binding's test hook.
    #[must_use]
    pub fn with_observer(mut self, observer: fn(ParkingEvent)) -> Self {
        self.observer = Some(observer);
        self
    }

    fn notify(&self, event: ParkingEvent) {
        if let Some(observer) = self.observer {
            observer(event);
        }
    }

    /// Run a call that may park (a send, `PAUSE`) holding the slot, with
    /// the parking marker set so a close cancels it. Waits for the slot.
    ///
    /// # Errors
    /// [`SlotRefused::Closed`] when a close or a conversion began before the
    /// call got the slot (nothing ran); [`SlotRefused::Poisoned`] after a
    /// call panicked holding it.
    pub fn run_parking<R>(&self, f: impl FnOnce(&mut T) -> R) -> Result<R, SlotRefused> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(SlotRefused::Closed);
        }
        self.notify(ParkingEvent::ParkingEntered);
        let locked = self.value.lock();
        self.notify(ParkingEvent::ParkingLeft);
        // A close stores `closed` and then tries the slot; this call has
        // the slot and then reads `closed` (both `SeqCst`). A call that
        // reads `false` holds the slot, so the close sees it held and
        // re-reads the marker until it is set.
        if self.closed.load(Ordering::SeqCst) {
            return Err(SlotRefused::Closed);
        }
        let mut guard = locked.map_err(|_| SlotRefused::Poisoned)?;
        let value = guard.as_mut().ok_or(SlotRefused::Closed)?;
        self.parking_holder.store(true, Ordering::SeqCst);
        let _holding = Holding(&self.parking_holder);
        Ok(f(value))
    }

    /// Run a query (stats, a control call that cannot park) holding the
    /// slot. Waits for the slot; a close waits for it in turn and does not
    /// cancel it.
    ///
    /// # Errors
    /// As [`Self::run_parking`].
    pub fn run<R>(&self, f: impl FnOnce(&mut T) -> R) -> Result<R, SlotRefused> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(SlotRefused::Closed);
        }
        self.notify(ParkingEvent::QueryEntered);
        let locked = self.value.lock();
        if self.closed.load(Ordering::SeqCst) {
            return Err(SlotRefused::Closed);
        }
        let mut guard = locked.map_err(|_| SlotRefused::Poisoned)?;
        let value = guard.as_mut().ok_or(SlotRefused::Closed)?;
        Ok(f(value))
    }

    /// Read the value only if the slot is free; never waits. A held slot is
    /// [`Peek::Held`] until a close begins, then [`Peek::Closed`].
    pub fn try_peek<R>(&self, f: impl FnOnce(&T) -> R) -> Peek<R> {
        if self.closed.load(Ordering::SeqCst) {
            return Peek::Closed;
        }
        match self.value.try_lock() {
            Ok(guard) => guard.as_ref().map_or(Peek::Closed, |v| Peek::Value(f(v))),
            Err(TryLockError::WouldBlock) if !self.closed.load(Ordering::SeqCst) => Peek::Held,
            Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => Peek::Closed,
        }
    }

    /// Mark the slot closed and take the value for the close. A parking
    /// call holding the slot is cancelled first and waited for; a query
    /// holding it is waited for without a cancel; a call only waiting gets
    /// the slot afterwards and runs nothing. A poisoned slot still gives
    /// its value up. `None` when a close or a conversion already took it.
    pub fn take_for_close(&self) -> Option<T> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return None;
        }
        loop {
            match self.value.try_lock() {
                Ok(mut guard) => return guard.take(),
                Err(TryLockError::Poisoned(p)) => return p.into_inner().take(),
                Err(TryLockError::WouldBlock) => {
                    if self.parking_holder.load(Ordering::SeqCst) {
                        self.cancel.cancel();
                        return self
                            .value
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .take();
                    }
                    self.notify(ParkingEvent::CloseRetry);
                    std::thread::sleep(RETRY);
                }
            }
        }
    }

    /// Take the value for a conversion into another object. Waits behind a
    /// query; behind a parking call it takes nothing. Marks the slot
    /// closed only once it holds the slot, so a refused conversion leaves
    /// the session as it was and a close racing it does not cancel the
    /// value the new owner gets.
    ///
    /// # Errors
    /// [`SlotRefused::Busy`] behind a parking call; [`SlotRefused::Closed`]
    /// when a close began or the value is gone; [`SlotRefused::Poisoned`]
    /// after a call panicked holding the slot.
    pub fn take_for_conversion(&self) -> Result<T, SlotRefused> {
        let mut guard = loop {
            if self.closed.load(Ordering::SeqCst) {
                return Err(SlotRefused::Closed);
            }
            match self.value.try_lock() {
                Ok(guard) => break guard,
                Err(TryLockError::Poisoned(_)) => return Err(SlotRefused::Poisoned),
                Err(TryLockError::WouldBlock) => {
                    // A close that began holds nothing until the holder lets
                    // go: report it as the close it is.
                    if self.closed.load(Ordering::SeqCst) {
                        return Err(SlotRefused::Closed);
                    }
                    if self.parking_holder.load(Ordering::SeqCst) {
                        return Err(SlotRefused::Busy);
                    }
                    self.notify(ParkingEvent::ConversionRetry);
                    std::thread::sleep(RETRY);
                }
            }
        };
        if self.closed.swap(true, Ordering::SeqCst) {
            return Err(SlotRefused::Closed);
        }
        guard.take().ok_or(SlotRefused::Closed)
    }

    /// `true` once a close or a conversion began.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Condvar, mpsc};
    use std::thread;
    use std::time::Instant;

    const DEADLINE: Duration = Duration::from_secs(10);

    /// A cancel that counts its calls and wakes [`TestCancel::park`].
    #[derive(Default)]
    struct TestCancel {
        fired: Mutex<bool>,
        wake: Condvar,
        calls: AtomicUsize,
    }

    impl TestCancel {
        /// Stand in for a call parked on the network: return once the
        /// cancel fired, panic at the deadline.
        fn park(&self) {
            let start = Instant::now();
            let mut fired = self.fired.lock().unwrap();
            while !*fired {
                let left = DEADLINE
                    .checked_sub(start.elapsed())
                    .expect("the parked call was never cancelled");
                fired = self.wake.wait_timeout(fired, left).unwrap().0;
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl TransportCancel for TestCancel {
        fn cancel(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.fired.lock().unwrap() = true;
            self.wake.notify_all();
        }
        fn is_cancelled(&self) -> bool {
            *self.fired.lock().unwrap()
        }
    }

    /// A query that holds the slot until released.
    #[derive(Default)]
    struct Release {
        released: Mutex<bool>,
        wake: Condvar,
    }

    impl Release {
        fn wait(&self) {
            let start = Instant::now();
            let mut r = self.released.lock().unwrap();
            while !*r {
                let left = DEADLINE
                    .checked_sub(start.elapsed())
                    .expect("the holder was never released");
                r = self.wake.wait_timeout(r, left).unwrap().0;
            }
        }
        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.wake.notify_all();
        }
    }

    fn wait_until(what: &str, cond: impl Fn() -> bool) {
        let start = Instant::now();
        while !cond() {
            assert!(start.elapsed() < DEADLINE, "never: {what}");
            thread::yield_now();
        }
    }

    type Slot = Arc<ParkingSlot<u32>>;

    fn slot() -> (Slot, Arc<TestCancel>) {
        let cancel = Arc::new(TestCancel::default());
        let slot = Arc::new(ParkingSlot::new(
            7,
            cancel.clone() as Arc<dyn TransportCancel>,
        ));
        (slot, cancel)
    }

    fn slot_with(observer: fn(ParkingEvent)) -> (Slot, Arc<TestCancel>) {
        let cancel = Arc::new(TestCancel::default());
        let slot =
            ParkingSlot::new(7, cancel.clone() as Arc<dyn TransportCancel>).with_observer(observer);
        (Arc::new(slot), cancel)
    }

    /// A thread parked in `run_parking` until the cancel fires; returns
    /// once it holds the slot.
    fn park_holder(
        slot: &Slot,
        cancel: &Arc<TestCancel>,
    ) -> thread::JoinHandle<Result<u32, SlotRefused>> {
        let (held_tx, held_rx) = mpsc::channel();
        let (s, c) = (slot.clone(), cancel.clone());
        let h = thread::spawn(move || {
            s.run_parking(|v| {
                held_tx.send(()).unwrap();
                c.park();
                *v
            })
        });
        held_rx
            .recv_timeout(DEADLINE)
            .expect("the parking call got the slot");
        h
    }

    /// A thread holding the slot through `run` until `release`.
    fn query_holder(
        slot: &Slot,
        release: &Arc<Release>,
    ) -> thread::JoinHandle<Result<u32, SlotRefused>> {
        let (held_tx, held_rx) = mpsc::channel();
        let (s, r) = (slot.clone(), release.clone());
        let h = thread::spawn(move || {
            s.run(|v| {
                held_tx.send(()).unwrap();
                r.wait();
                *v
            })
        });
        held_rx
            .recv_timeout(DEADLINE)
            .expect("the query got the slot");
        h
    }

    #[test]
    fn close_cancels_a_parked_holder_exactly_once() {
        let (slot, cancel) = slot();
        let parked = park_holder(&slot, &cancel);
        assert_eq!(cancel.calls(), 0);
        assert_eq!(slot.take_for_close(), Some(7));
        assert_eq!(cancel.calls(), 1);
        assert_eq!(
            parked.join().unwrap(),
            Ok(7),
            "the parked call ran to its end"
        );
        assert!(slot.is_closed());
        assert_eq!(slot.take_for_close(), None, "a second close takes nothing");
        assert_eq!(cancel.calls(), 1);
    }

    static Q_RETRIES: AtomicUsize = AtomicUsize::new(0);
    fn q_observer(e: ParkingEvent) {
        if e == ParkingEvent::CloseRetry {
            Q_RETRIES.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn close_does_not_cancel_a_query_and_waits_for_it() {
        let (slot, cancel) = slot_with(q_observer);
        let release = Arc::new(Release::default());
        let query = query_holder(&slot, &release);
        let s = slot.clone();
        let closer = thread::spawn(move || s.take_for_close());
        wait_until("the close waits for the query", || {
            Q_RETRIES.load(Ordering::SeqCst) > 0
        });
        assert!(
            !closer.is_finished(),
            "the close waits while the query holds"
        );
        assert_eq!(cancel.calls(), 0, "a query is not cancelled");
        release.release();
        assert_eq!(query.join().unwrap(), Ok(7), "the query ran to its end");
        assert_eq!(closer.join().unwrap(), Some(7));
        assert_eq!(cancel.calls(), 0);
    }

    static W_ENTERED: AtomicUsize = AtomicUsize::new(0);
    static W_LEFT: AtomicUsize = AtomicUsize::new(0);
    static W_RETRIES: AtomicUsize = AtomicUsize::new(0);
    static W_QUERIES: AtomicUsize = AtomicUsize::new(0);
    fn w_observer(e: ParkingEvent) {
        match e {
            ParkingEvent::ParkingEntered => W_ENTERED.fetch_add(1, Ordering::SeqCst),
            ParkingEvent::ParkingLeft => W_LEFT.fetch_add(1, Ordering::SeqCst),
            ParkingEvent::CloseRetry => W_RETRIES.fetch_add(1, Ordering::SeqCst),
            ParkingEvent::QueryEntered => W_QUERIES.fetch_add(1, Ordering::SeqCst),
            ParkingEvent::ConversionRetry => 0,
        };
    }

    #[test]
    fn a_caller_waiting_for_the_slot_runs_nothing_after_close_and_gets_closed() {
        let (slot, cancel) = slot_with(w_observer);
        let parked = park_holder(&slot, &cancel);
        // The holder entered and left the wait; the waiter enters it and
        // stays there behind the holder.
        let ran = Arc::new(AtomicBool::new(false));
        let (s, r) = (slot.clone(), ran.clone());
        let waiter = thread::spawn(move || s.run_parking(|_| r.store(true, Ordering::SeqCst)));
        wait_until("the second call waits for the slot", || {
            W_ENTERED.load(Ordering::SeqCst) == 2 && W_LEFT.load(Ordering::SeqCst) == 1
        });
        let queries = W_QUERIES.load(Ordering::SeqCst);
        let (s, r) = (slot.clone(), ran.clone());
        let query_waiter = thread::spawn(move || s.run(|_| r.store(true, Ordering::SeqCst)));
        wait_until("the query waits for the slot", || {
            W_QUERIES.load(Ordering::SeqCst) == queries + 1
        });

        assert_eq!(slot.take_for_close(), Some(7));
        assert_eq!(cancel.calls(), 1, "only the holder is cancelled");
        assert_eq!(parked.join().unwrap(), Ok(7));
        assert_eq!(waiter.join().unwrap(), Err(SlotRefused::Closed));
        assert_eq!(query_waiter.join().unwrap(), Err(SlotRefused::Closed));
        assert!(
            !ran.load(Ordering::SeqCst),
            "nothing ran after the close began"
        );
        assert_eq!(slot.run_parking(|_| ()), Err(SlotRefused::Closed));
        assert_eq!(slot.run(|_| ()), Err(SlotRefused::Closed));

        // Behind a query the close retries in rounds, so a waiting call
        // usually gets the slot before the close does: it still runs nothing.
        let (slot, cancel) = slot_with(w_observer);
        let release = Arc::new(Release::default());
        let query = query_holder(&slot, &release);
        let entered = W_ENTERED.load(Ordering::SeqCst);
        let queries = W_QUERIES.load(Ordering::SeqCst);
        let (s, r) = (slot.clone(), ran.clone());
        let waiter = thread::spawn(move || s.run_parking(|_| r.store(true, Ordering::SeqCst)));
        let (s, r) = (slot.clone(), ran.clone());
        let query_waiter = thread::spawn(move || s.run(|_| r.store(true, Ordering::SeqCst)));
        wait_until("both calls wait behind the query", || {
            W_ENTERED.load(Ordering::SeqCst) == entered + 1
                && W_QUERIES.load(Ordering::SeqCst) == queries + 1
        });
        let s = slot.clone();
        let closer = thread::spawn(move || s.take_for_close());
        wait_until("the close waits behind the query", || {
            W_RETRIES.load(Ordering::SeqCst) > 0
        });
        release.release();
        assert_eq!(query.join().unwrap(), Ok(7));
        assert_eq!(waiter.join().unwrap(), Err(SlotRefused::Closed));
        assert_eq!(query_waiter.join().unwrap(), Err(SlotRefused::Closed));
        assert_eq!(closer.join().unwrap(), Some(7));
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(cancel.calls(), 0);
    }

    static C_RETRIES: AtomicUsize = AtomicUsize::new(0);
    fn c_observer(e: ParkingEvent) {
        if e == ParkingEvent::ConversionRetry {
            C_RETRIES.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn conversion_refuses_behind_a_parking_call_and_waits_behind_a_query() {
        let (slot, cancel) = slot_with(c_observer);
        let parked = park_holder(&slot, &cancel);
        assert_eq!(slot.take_for_conversion(), Err(SlotRefused::Busy));
        assert!(!slot.is_closed(), "a refused conversion leaves the session");
        assert_eq!(cancel.calls(), 0, "a conversion cancels nothing");
        // End the parked call by its own cancel; the session stays usable.
        cancel.cancel();
        assert_eq!(parked.join().unwrap(), Ok(7));
        assert_eq!(slot.run(|v| *v), Ok(7));

        let release = Arc::new(Release::default());
        let query = query_holder(&slot, &release);
        let s = slot.clone();
        let converter = thread::spawn(move || s.take_for_conversion());
        wait_until("the conversion waits for the query", || {
            C_RETRIES.load(Ordering::SeqCst) > 0
        });
        assert!(!converter.is_finished());
        release.release();
        assert_eq!(query.join().unwrap(), Ok(7));
        assert_eq!(converter.join().unwrap(), Ok(7));
        assert!(slot.is_closed());
        assert_eq!(slot.run(|_| ()), Err(SlotRefused::Closed));
        assert_eq!(slot.take_for_close(), None, "the conversion took the value");
        assert_eq!(slot.take_for_conversion(), Err(SlotRefused::Closed));
    }

    static P_RETRIES: AtomicUsize = AtomicUsize::new(0);
    fn p_observer(e: ParkingEvent) {
        if e == ParkingEvent::CloseRetry {
            P_RETRIES.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// `try_peek` on another thread, answered within the deadline.
    fn peek_now(slot: &Slot) -> Peek<u32> {
        let (tx, rx) = mpsc::channel();
        let s = slot.clone();
        thread::spawn(move || {
            let _ = tx.send(s.try_peek(|v| *v));
        });
        rx.recv_timeout(DEADLINE).expect("try_peek waited")
    }

    #[test]
    fn try_peek_never_waits() {
        let (slot, cancel) = slot_with(p_observer);
        assert_eq!(slot.try_peek(|v| *v + 1), Peek::Value(8));

        let parked = park_holder(&slot, &cancel);
        assert_eq!(peek_now(&slot), Peek::Held);
        cancel.cancel();
        parked.join().unwrap().unwrap();

        let release = Arc::new(Release::default());
        let query = query_holder(&slot, &release);
        assert_eq!(peek_now(&slot), Peek::Held);
        let s = slot.clone();
        let closer = thread::spawn(move || s.take_for_close());
        wait_until("the close waits for the query", || {
            P_RETRIES.load(Ordering::SeqCst) > 0
        });
        assert_eq!(peek_now(&slot), Peek::Closed, "a close has begun");
        release.release();
        query.join().unwrap().unwrap();
        assert_eq!(closer.join().unwrap(), Some(7));
        assert_eq!(slot.try_peek(|v| *v), Peek::Closed);
    }

    #[test]
    fn a_poisoned_slot_refuses_every_call_but_close() {
        let (slot, cancel) = slot();
        let s = slot.clone();
        let panicked = thread::spawn(move || {
            let _ = s.run_parking(|_| panic!("a call panics holding the slot"));
        })
        .join();
        assert!(panicked.is_err());
        assert_eq!(slot.run_parking(|_| ()), Err(SlotRefused::Poisoned));
        assert_eq!(slot.run(|_| ()), Err(SlotRefused::Poisoned));
        assert_eq!(slot.take_for_conversion(), Err(SlotRefused::Poisoned));
        assert_eq!(slot.try_peek(|v| *v), Peek::Closed);
        assert!(!slot.is_closed(), "the refusals took nothing");

        assert_eq!(
            slot.take_for_close(),
            Some(7),
            "a close still takes the value"
        );
        assert_eq!(cancel.calls(), 0, "the panicked call's marker is down");
        assert_eq!(slot.run(|_| ()), Err(SlotRefused::Closed));
    }

    static O_EVENTS: Mutex<Vec<ParkingEvent>> = Mutex::new(Vec::new());
    fn o_observer(e: ParkingEvent) {
        O_EVENTS.lock().unwrap().push(e);
    }
    fn o_count(e: ParkingEvent) -> usize {
        O_EVENTS.lock().unwrap().iter().filter(|x| **x == e).count()
    }

    #[test]
    fn the_observer_sees_each_retry() {
        let (slot, _cancel) = slot_with(o_observer);
        assert_eq!(slot.run_parking(|v| *v), Ok(7));
        assert_eq!(o_count(ParkingEvent::ParkingEntered), 1);
        assert_eq!(o_count(ParkingEvent::ParkingLeft), 1);
        assert_eq!(slot.run(|v| *v), Ok(7));
        assert_eq!(o_count(ParkingEvent::QueryEntered), 1);
        assert_eq!(
            O_EVENTS.lock().unwrap().len(),
            3,
            "no retry without a holder"
        );

        let release = Arc::new(Release::default());
        let query = query_holder(&slot, &release);
        let s = slot.clone();
        let converter = thread::spawn(move || s.take_for_conversion());
        wait_until("several conversion rounds", || {
            o_count(ParkingEvent::ConversionRetry) >= 3
        });
        release.release();
        query.join().unwrap().unwrap();
        assert_eq!(converter.join().unwrap(), Ok(7));
        let rounds = o_count(ParkingEvent::ConversionRetry);
        assert_eq!(o_count(ParkingEvent::CloseRetry), 0);

        let (slot, _cancel) = slot_with(o_observer);
        let release = Arc::new(Release::default());
        let query = query_holder(&slot, &release);
        let s = slot.clone();
        let closer = thread::spawn(move || s.take_for_close());
        wait_until("several close rounds", || {
            o_count(ParkingEvent::CloseRetry) >= 3
        });
        release.release();
        query.join().unwrap().unwrap();
        assert_eq!(closer.join().unwrap(), Some(7));
        assert_eq!(
            o_count(ParkingEvent::ConversionRetry),
            rounds,
            "a close reports close rounds only"
        );
    }
}
