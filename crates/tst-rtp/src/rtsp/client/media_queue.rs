//! The queue of whole `$` interleaved frames a publishing caller hands to
//! the client's thread. The caller waits here when it is full; the thread
//! only ever takes the lock for a moment, so it is never held up by a
//! caller.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use tst_core::cancel::{CANCEL_POLL_INTERVAL, FlagCancel};

/// Frames the queue holds before a send waits.
pub(crate) const MEDIA_QUEUE_BOUND: usize = 1024;

/// Unsent bytes on the control stream above which the thread moves no
/// media onto it, so a request submitted later waits behind at most this
/// much and one frame.
pub(crate) const MEDIA_WATERMARK: u64 = 64 * 1024;

/// Why a wait on the queue ended without what it waited for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaWaitEnd {
    /// The client's cancel is set.
    Cancelled,
    /// The thread closed the queue: the session or the stream has ended,
    /// and the client's end-reason slot names the cause when there is one.
    Ended,
}

#[derive(Default)]
struct State {
    frames: VecDeque<Bytes>,
    closed: bool,
    /// The stream's unsent bytes as the thread last published them, plus
    /// the frames it has taken since: a taken frame is never out of both
    /// counts.
    unsent: u64,
    #[cfg(test)]
    high_water: usize,
    /// Callers inside a wait.
    #[cfg(test)]
    parked: usize,
    /// The most unsent bytes the thread has published (read only by tests
    /// that run on Linux).
    #[cfg(all(test, target_os = "linux"))]
    unsent_high_water: u64,
}

#[derive(Default)]
pub(crate) struct MediaQueue {
    state: Mutex<State>,
    /// Signals space, the stream running empty, and the close.
    changed: Condvar,
    dropped_at_close: AtomicU64,
}

// By hand: the derived form would print every queued frame.
impl std::fmt::Debug for MediaQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = self.lock();
        f.debug_struct("MediaQueue")
            .field("frames", &st.frames.len())
            .field("closed", &st.closed)
            .field("unsent", &st.unsent)
            .field("dropped_at_close", &self.dropped_at_close())
            .finish()
    }
}

impl MediaQueue {
    /// No holder of the lock can leave the state half-changed, so a
    /// poisoned lock is taken as it is.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One slice of a caller's wait: until a signal, or the time after
    /// which the cancel is looked at again.
    fn park<'a>(
        &self,
        #[cfg_attr(not(test), allow(unused_mut))] mut st: MutexGuard<'a, State>,
    ) -> MutexGuard<'a, State> {
        // Counted under the lock on both sides of the wait, and a waiter
        // keeps the lock from one slice to the next: whoever reads the
        // count sees a caller for as long as it waits.
        #[cfg(test)]
        {
            st.parked += 1;
        }
        #[cfg_attr(not(test), allow(unused_mut))]
        let mut st = self
            .changed
            .wait_timeout(st, CANCEL_POLL_INTERVAL)
            .map(|(st, _)| st)
            .unwrap_or_else(|e| e.into_inner().0);
        #[cfg(test)]
        {
            st.parked -= 1;
        }
        st
    }

    /// Queue `frame` behind the frames queued before, waiting while the
    /// queue is full. Each round checks, in this order, the cancel, the
    /// close, and space.
    pub(crate) fn push(&self, frame: Bytes, cancel: &FlagCancel) -> Result<(), MediaWaitEnd> {
        let mut st = self.lock();
        loop {
            if cancel.is_cancelled() {
                return Err(MediaWaitEnd::Cancelled);
            }
            if st.closed {
                return Err(MediaWaitEnd::Ended);
            }
            if st.frames.len() < MEDIA_QUEUE_BOUND {
                st.frames.push_back(frame);
                #[cfg(test)]
                {
                    st.high_water = st.high_water.max(st.frames.len());
                }
                return Ok(());
            }
            st = self.park(st);
        }
    }

    /// Wait until no frame is queued and the stream has no unsent byte.
    /// Each round checks, in this order, the cancel, the close, and that.
    pub(crate) fn wait_drained(&self, cancel: &FlagCancel) -> Result<(), MediaWaitEnd> {
        let mut st = self.lock();
        loop {
            if cancel.is_cancelled() {
                return Err(MediaWaitEnd::Cancelled);
            }
            if st.closed {
                return Err(MediaWaitEnd::Ended);
            }
            if st.frames.is_empty() && st.unsent == 0 {
                return Ok(());
            }
            st = self.park(st);
        }
    }

    /// The thread takes the oldest frame to write it. The frame counts as
    /// unsent from here on, so a caller waiting for the drain never sees
    /// the queue empty and the stream idle with this frame still unwritten.
    pub(crate) fn pop(&self) -> Option<Bytes> {
        let mut st = self.lock();
        let was_full = st.frames.len() >= MEDIA_QUEUE_BOUND;
        let frame = st.frames.pop_front()?;
        st.unsent = st.unsent.saturating_add(frame.len() as u64);
        drop(st);
        // A send waits only on a full queue.
        if was_full {
            self.changed.notify_all();
        }
        Some(frame)
    }

    /// The thread reports the stream's unsent bytes after a write.
    pub(crate) fn publish_unsent(&self, unsent: u64) {
        let mut st = self.lock();
        #[cfg(all(test, target_os = "linux"))]
        {
            st.unsent_high_water = st.unsent_high_water.max(unsent);
        }
        if st.unsent == unsent {
            return;
        }
        st.unsent = unsent;
        drop(st);
        if unsent == 0 {
            self.changed.notify_all();
        }
    }

    /// No frame will be written any more: the queued ones are dropped and
    /// counted, and every wait ends with `Ended`. Only the first call acts.
    pub(crate) fn close(&self) {
        let mut st = self.lock();
        if st.closed {
            return;
        }
        st.closed = true;
        let dropped = std::mem::take(&mut st.frames);
        // Before the lock is released: whoever sees the close sees the
        // count.
        self.dropped_at_close
            .fetch_add(dropped.len() as u64, Ordering::Release);
        drop(st);
        self.changed.notify_all();
        // Freed here, outside the lock: a full queue of large frames takes
        // a while, and a sender that sees the close does not wait for it.
        drop(dropped);
    }

    /// Frames that were still queued when the queue closed.
    pub(crate) fn dropped_at_close(&self) -> u64 {
        self.dropped_at_close.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().frames.len()
    }

    /// How many callers are inside a wait of [`Self::push`] or
    /// [`Self::wait_drained`] now.
    #[cfg(test)]
    pub(crate) fn parked(&self) -> usize {
        self.lock().parked
    }

    /// The most frames the queue has held.
    #[cfg(test)]
    pub(crate) fn high_water(&self) -> usize {
        self.lock().high_water
    }

    /// The most unsent bytes the stream has held after a write.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn unsent_high_water(&self) -> u64 {
        self.lock().unsent_high_water
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use tst_test_helpers::timing::scaled;

    fn frame(n: usize) -> Bytes {
        Bytes::from(vec![0xAB; n])
    }

    /// Runs `wait` on a thread that says when it starts; the result comes
    /// with the time the wait returned.
    fn waiter<T: Send + 'static>(
        wait: impl FnOnce() -> T + Send + 'static,
    ) -> (mpsc::Receiver<()>, mpsc::Receiver<(T, Instant)>) {
        let (started_tx, started) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = started_tx.send(());
            let r = wait();
            let _ = done_tx.send((r, Instant::now()));
        });
        (started, done)
    }

    fn within<T>(rx: &mpsc::Receiver<T>) -> T {
        rx.recv_timeout(scaled(Duration::from_secs(5)))
            .expect("within the deadline")
    }

    /// Pacing, not a sync: by the end of it a wait that began is parked,
    /// and one that should have parked and did not has returned.
    fn let_it_park() {
        std::thread::sleep(Duration::from_millis(20));
    }

    /// A queue at its bound.
    fn full() -> Arc<MediaQueue> {
        let q = Arc::new(MediaQueue::default());
        for _ in 0..MEDIA_QUEUE_BOUND {
            q.push(frame(1), &FlagCancel::new()).unwrap();
        }
        q
    }

    /// `trigger` ends a parked `wait` with `end`, within `bound`.
    /// Scheduling can hold one return back past the bound, not five in a
    /// row: the first round inside it passes.
    fn ends_within<T: PartialEq + std::fmt::Debug + Send + 'static>(
        bound: Duration,
        end: T,
        arrange: impl Fn() -> Arc<MediaQueue>,
        wait: impl Fn(&MediaQueue, &FlagCancel) -> T + Send + Copy + 'static,
        trigger: impl Fn(&MediaQueue, &FlagCancel),
    ) -> bool {
        (0..5).any(|_| {
            let q = arrange();
            let cancel = FlagCancel::new();
            let (started, done) = waiter({
                let (q, cancel) = (q.clone(), cancel.clone());
                move || wait(&q, &cancel)
            });
            within(&started);
            let_it_park();
            let triggered = Instant::now();
            trigger(&q, &cancel);
            let (ended, returned) = within(&done);
            assert_eq!(ended, end);
            returned.saturating_duration_since(triggered) < bound
        })
    }

    /// `trigger` ends a parked `wait` at once, not at the end of the
    /// wait's 100 ms slice.
    fn wakes_at_once<T: PartialEq + std::fmt::Debug + Send + 'static>(
        end: T,
        arrange: impl Fn() -> Arc<MediaQueue>,
        wait: impl Fn(&MediaQueue, &FlagCancel) -> T + Send + Copy + 'static,
        trigger: impl Fn(&MediaQueue),
    ) -> bool {
        ends_within(CANCEL_POLL_INTERVAL * 2 / 5, end, arrange, wait, |q, _| {
            trigger(q)
        })
    }

    /// The cancel signals nothing: a parked wait sees it when its slice
    /// ends, so one slice after the cancel at the latest. The bound is the
    /// slice and room for the wake.
    fn a_cancel_ends_it_within_one_slice(
        arrange: impl Fn() -> Arc<MediaQueue>,
        wait: impl Fn(&MediaQueue, &FlagCancel) -> Result<(), MediaWaitEnd> + Send + Copy + 'static,
    ) -> bool {
        ends_within(
            CANCEL_POLL_INTERVAL + Duration::from_millis(50),
            Err(MediaWaitEnd::Cancelled),
            arrange,
            wait,
            |_, cancel| cancel.cancel(),
        )
    }

    /// A queue the stream has not finished writing for.
    fn undrained() -> Arc<MediaQueue> {
        let q = Arc::new(MediaQueue::default());
        q.publish_unsent(40);
        q
    }

    #[test]
    fn frames_leave_in_the_order_they_were_queued() {
        let q = MediaQueue::default();
        let cancel = FlagCancel::new();
        for n in 1..=3 {
            q.push(frame(n), &cancel).unwrap();
        }
        let lens: Vec<usize> = std::iter::from_fn(|| q.pop()).map(|f| f.len()).collect();
        assert_eq!(lens, [1, 2, 3]);
    }

    #[test]
    fn a_push_checks_the_cancel_then_the_close_then_space() {
        let q = MediaQueue::default();
        let cancel = FlagCancel::new();
        q.close();
        assert_eq!(q.push(frame(1), &cancel), Err(MediaWaitEnd::Ended));
        cancel.cancel();
        assert_eq!(q.push(frame(1), &cancel), Err(MediaWaitEnd::Cancelled));
        // With space too, the cancel is what a push reports.
        let open = MediaQueue::default();
        assert_eq!(open.push(frame(1), &cancel), Err(MediaWaitEnd::Cancelled));
        assert_eq!(open.len(), 0);
    }

    #[test]
    fn a_drain_checks_the_cancel_then_the_close_then_emptiness() {
        let q = MediaQueue::default();
        let cancel = FlagCancel::new();
        assert_eq!(q.wait_drained(&cancel), Ok(()));
        q.close();
        assert_eq!(q.wait_drained(&cancel), Err(MediaWaitEnd::Ended));
        cancel.cancel();
        assert_eq!(q.wait_drained(&cancel), Err(MediaWaitEnd::Cancelled));
        assert_eq!(
            MediaQueue::default().wait_drained(&cancel),
            Err(MediaWaitEnd::Cancelled)
        );
    }

    #[test]
    fn a_full_queue_parks_a_push_until_a_frame_leaves() {
        let q = Arc::new(MediaQueue::default());
        let cancel = FlagCancel::new();
        for _ in 0..MEDIA_QUEUE_BOUND {
            q.push(frame(1), &cancel).unwrap();
        }
        let (started, done) = waiter({
            let (q, cancel) = (q.clone(), cancel.clone());
            move || q.push(frame(2), &cancel)
        });
        within(&started);
        let_it_park();
        // The frame is queued only once one has left: a push that did not
        // wait would have made the queue longer than its bound.
        assert!(q.pop().is_some());
        assert_eq!(within(&done).0, Ok(()));
        assert_eq!(q.len(), MEDIA_QUEUE_BOUND);
        assert_eq!(q.high_water(), MEDIA_QUEUE_BOUND);
    }

    /// A frame the thread has taken is unsent until the thread says the
    /// stream is empty: the drain does not end on the empty queue alone.
    #[test]
    fn a_drain_waits_for_the_stream_as_well_as_the_queue() {
        let q = Arc::new(MediaQueue::default());
        let cancel = FlagCancel::new();
        q.push(frame(100), &cancel).unwrap();
        assert!(q.pop().is_some());
        // Still unwritten after a write that took only part of it.
        q.publish_unsent(40);
        let (started, done) = waiter({
            let (q, cancel) = (q.clone(), cancel.clone());
            move || q.wait_drained(&cancel)
        });
        within(&started);
        let_it_park();
        let released = Instant::now();
        q.publish_unsent(0);
        let (r, at) = within(&done);
        assert_eq!(r, Ok(()));
        assert!(
            at >= released,
            "the drain ended before the stream was empty"
        );
    }

    /// The same wait with nothing to end it but the cancel: `Ok` would mean
    /// it took the empty queue for the drain.
    #[test]
    fn a_drain_with_unsent_bytes_ends_only_on_the_cancel() {
        let q = Arc::new(MediaQueue::default());
        let cancel = FlagCancel::new();
        q.push(frame(100), &cancel).unwrap();
        assert!(q.pop().is_some());
        let (started, done) = waiter({
            let (q, cancel) = (q.clone(), cancel.clone());
            move || q.wait_drained(&cancel)
        });
        within(&started);
        let_it_park();
        cancel.cancel();
        assert_eq!(within(&done).0, Err(MediaWaitEnd::Cancelled));
    }

    #[test]
    fn a_frame_leaving_wakes_a_parked_push_at_once() {
        assert!(wakes_at_once(
            Ok(()),
            full,
            |q, cancel| q.push(frame(2), cancel),
            |q| assert!(q.pop().is_some()),
        ));
    }

    #[test]
    fn a_cancel_ends_a_parked_push_within_one_slice() {
        assert!(a_cancel_ends_it_within_one_slice(full, |q, cancel| {
            q.push(frame(2), cancel)
        }));
    }

    #[test]
    fn a_cancel_ends_a_parked_drain_within_one_slice() {
        assert!(a_cancel_ends_it_within_one_slice(undrained, |q, cancel| {
            q.wait_drained(cancel)
        }));
    }

    #[test]
    fn the_stream_running_empty_wakes_a_parked_drain_at_once() {
        assert!(wakes_at_once(
            Ok(()),
            undrained,
            |q, cancel| q.wait_drained(cancel),
            |q| q.publish_unsent(0),
        ));
    }

    #[test]
    fn a_close_wakes_a_parked_push_at_once() {
        assert!(wakes_at_once(
            Err(MediaWaitEnd::Ended),
            full,
            |q, cancel| q.push(frame(2), cancel),
            |q| q.close(),
        ));
    }

    #[test]
    fn a_close_drops_and_counts_the_queued_frames_and_ends_a_parked_push() {
        let q = Arc::new(MediaQueue::default());
        let cancel = FlagCancel::new();
        for _ in 0..MEDIA_QUEUE_BOUND {
            q.push(frame(1), &cancel).unwrap();
        }
        let (started, done) = waiter({
            let (q, cancel) = (q.clone(), cancel.clone());
            move || q.push(frame(2), &cancel)
        });
        within(&started);
        q.close();
        assert_eq!(within(&done).0, Err(MediaWaitEnd::Ended));
        assert_eq!(q.dropped_at_close(), MEDIA_QUEUE_BOUND as u64);
        assert_eq!(q.len(), 0);
        // A second close counts nothing more.
        q.close();
        assert_eq!(q.dropped_at_close(), MEDIA_QUEUE_BOUND as u64);
    }
}
