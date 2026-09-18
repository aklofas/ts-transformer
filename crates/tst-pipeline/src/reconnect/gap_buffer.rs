//! Fixed-capacity ring buffer of outbound messages, used by
//! `ManagedTransport` to hold messages that couldn't be sent during a
//! transport outage.
//!
//! Overflow policy is configurable: `DropOldest` (the default) discards
//! the front of the queue to make room; `Reject` returns an error
//! signaling the caller to back off.
//!
//! Drop policy is uniform across all sender types — drop oldest message.
//! The previously-considered drop-oldest-GOP policy is deferred; it would
//! require IDR-boundary metadata in the mux path and byte scanning in
//! the ts/raw paths.
//!
//! # In-flight marking
//!
//! The background drain worker must not hold the buffer's lock across the
//! inner transport's `send_bytes` — that one call is unbounded against a
//! peer that stops draining, and holding the lock there stalls both the
//! producer and the stats observer. Instead the worker *marks* the front
//! entry in flight ([`GapBuffer::begin_send`]), releases the lock, sends,
//! and then re-takes the lock to either [`GapBuffer::finish_send`] (pop
//! it) or [`GapBuffer::abort_send`] (leave it at the front for the
//! retry). Each entry carries a monotonic sequence number so those two
//! calls name exactly the message the worker sent, and `DropOldest`
//! eviction skips the in-flight entry so it is still at the front when
//! the send returns.

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverflowPolicy {
    /// Drop the oldest queued message to make room (default).
    #[default]
    DropOldest,
    /// Refuse to enqueue; return an error to the caller.
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GapBufferError {
    Full,
}

pub struct GapBuffer {
    capacity: usize,
    overflow: OverflowPolicy,
    /// `(seq, message)`. The sequence number exists so the drain worker
    /// can name the entry it handed to the transport after releasing the
    /// lock — see the module docs.
    queue: VecDeque<(u64, Vec<u8>)>,
    /// Sequence number for the next enqueued message.
    next_seq: u64,
    /// Sequence number of the entry the drain worker is currently
    /// sending, if any. Always the front entry, and never an eviction
    /// victim.
    in_flight: Option<u64>,
    /// Bytes dropped (oldest-first) due to overflow; for stats.
    pub bytes_dropped: u64,
    /// Messages dropped due to overflow.
    pub messages_dropped: u64,
}

impl GapBuffer {
    pub fn new(capacity: usize, overflow: OverflowPolicy) -> Self {
        Self {
            capacity,
            overflow,
            queue: VecDeque::with_capacity(capacity),
            next_seq: 0,
            in_flight: None,
            bytes_dropped: 0,
            messages_dropped: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Enqueue a message. Returns Ok if added; `Err(Full)` if the
    /// overflow policy is `Reject` and the buffer is full.
    ///
    /// Under `DropOldest` the victim is the oldest entry that is NOT in
    /// flight: the in-flight message has already been handed to the
    /// transport, and the worker must still find it at the front when its
    /// send returns. When that in-flight entry is the *only* entry the
    /// message is pushed anyway (transient `len == capacity + 1`); the
    /// next `finish_send` brings the buffer back within capacity.
    ///
    /// A zero capacity buffers nothing: with no evictable entry the
    /// incoming message is itself the oldest, so it is dropped and
    /// counted, and `Ok` means *accepted then dropped* as it does for any
    /// other `DropOldest` eviction. Under `Reject` a zero capacity
    /// returns `Err(Full)` for every message.
    pub fn enqueue(&mut self, msg: Vec<u8>) -> Result<(), GapBufferError> {
        if self.queue.len() >= self.capacity {
            match self.overflow {
                OverflowPolicy::DropOldest => {
                    let victim = usize::from(self.front_is_in_flight());
                    if let Some((_, dropped)) = self.queue.remove(victim) {
                        self.bytes_dropped += dropped.len() as u64;
                        self.messages_dropped += 1;
                    } else if !self.front_is_in_flight() {
                        // Nothing to evict and nothing pinned: the queue is
                        // empty, which inside this branch means `capacity`
                        // is 0. The incoming message is itself the oldest,
                        // so drop-oldest drops it. (With a pinned front
                        // there IS something evictable in principle — it is
                        // just not a legal victim — and the message rides
                        // the documented transient overshoot instead.)
                        self.bytes_dropped += msg.len() as u64;
                        self.messages_dropped += 1;
                        return Ok(());
                    }
                }
                OverflowPolicy::Reject => return Err(GapBufferError::Full),
            }
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.queue.push_back((seq, msg));
        Ok(())
    }

    /// Pop the front message.
    ///
    /// # Panics
    ///
    /// Debug builds assert that no send is in flight: the drain worker
    /// settles its own entry with `finish_send` (crate-internal), and
    /// popping out from under a live in-flight mark would leave that mark
    /// pointing at a message the buffer no longer holds. (The inline
    /// `Blocking` drain and any outside-crate caller run with no worker,
    /// so the mark is `None` for them.)
    pub fn pop_front(&mut self) -> Option<Vec<u8>> {
        debug_assert!(
            self.in_flight.is_none(),
            "BUG: pop_front while a send is in flight — settle it with finish_send/abort_send"
        );
        self.queue.pop_front().map(|(_, msg)| msg)
    }

    /// Peek at the front message. Read-only, so it is safe alongside an
    /// in-flight send — note it then returns the very message the drain
    /// worker is writing, which has NOT been delivered yet.
    pub fn front(&self) -> Option<&Vec<u8>> {
        self.queue.front().map(|(_, msg)| msg)
    }

    /// Clone the front message and mark it in flight, returning its
    /// sequence number alongside the bytes. `None` when the buffer is
    /// empty.
    ///
    /// The caller may then release the buffer's lock for the duration of
    /// the send and settle the entry afterwards with [`Self::finish_send`]
    /// or [`Self::abort_send`].
    ///
    /// # Panics
    ///
    /// Debug builds assert that no send is already in flight — the drain
    /// worker is the single caller and is single-threaded, so a second
    /// `begin_send` would mean a lost settle call.
    pub(crate) fn begin_send(&mut self) -> Option<(u64, Vec<u8>)> {
        debug_assert!(
            self.in_flight.is_none(),
            "BUG: begin_send with a send already in flight"
        );
        let (seq, msg) = self.queue.front()?;
        let (seq, msg) = (*seq, msg.clone());
        self.in_flight = Some(seq);
        Some((seq, msg))
    }

    /// Settle an in-flight send that succeeded (or whose message must be
    /// discarded): pop and return it, clearing the in-flight mark.
    ///
    /// # Panics
    ///
    /// Panics in **all** builds if `seq` is not the front entry: that
    /// would mean the buffer desynchronised while the lock was released,
    /// the one condition the in-flight mark exists to prevent, and
    /// popping anyway would discard an unsent message. Debug builds
    /// additionally assert that `seq` is the entry `begin_send` marked,
    /// which only a caller that skipped `begin_send` could violate.
    pub(crate) fn finish_send(&mut self, seq: u64) -> Option<Vec<u8>> {
        debug_assert_eq!(
            self.in_flight,
            Some(seq),
            "BUG: finish_send for a message that is not in flight"
        );
        assert_eq!(
            self.queue.front().map(|(s, _)| *s),
            Some(seq),
            "BUG: gap buffer desynchronised — the in-flight message is no longer at the front"
        );
        self.in_flight = None;
        self.pop_front()
    }

    /// Settle an in-flight send that did not deliver: clear the mark and
    /// leave the message at the front for the retry.
    ///
    /// # Panics
    ///
    /// Debug builds assert that `seq` is the in-flight entry.
    pub(crate) fn abort_send(&mut self, seq: u64) {
        debug_assert_eq!(
            self.in_flight,
            Some(seq),
            "BUG: abort_send for a message that is not in flight"
        );
        self.in_flight = None;
    }

    fn front_is_in_flight(&self) -> bool {
        match (self.in_flight, self.queue.front()) {
            (Some(marked), Some((front, _))) => marked == *front,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_oldest_policy_evicts_front() {
        let mut buf = GapBuffer::new(2, OverflowPolicy::DropOldest);
        buf.enqueue(vec![1]).unwrap();
        buf.enqueue(vec![2]).unwrap();
        buf.enqueue(vec![3]).unwrap();
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.pop_front().unwrap(), vec![2]);
        assert_eq!(buf.pop_front().unwrap(), vec![3]);
        assert_eq!(buf.messages_dropped, 1);
        assert_eq!(buf.bytes_dropped, 1);
    }

    #[test]
    fn reject_policy_returns_error_when_full() {
        let mut buf = GapBuffer::new(1, OverflowPolicy::Reject);
        buf.enqueue(vec![1]).unwrap();
        let result = buf.enqueue(vec![2]);
        assert_eq!(result, Err(GapBufferError::Full));
        assert_eq!(buf.messages_dropped, 0);
    }

    #[test]
    fn fifo_order_preserved() {
        let mut buf = GapBuffer::new(10, OverflowPolicy::DropOldest);
        for i in 0..5 {
            buf.enqueue(vec![i]).unwrap();
        }
        for i in 0..5 {
            assert_eq!(buf.pop_front().unwrap(), vec![i]);
        }
        assert!(buf.is_empty());
    }

    /// The drain worker settles its own entry via `finish_send` (which
    /// clears the mark before popping). A bare `pop_front` while a send
    /// is in flight would leave the mark pointing at a message the buffer
    /// no longer holds — caught loudly in debug rather than silently
    /// desyncing.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "BUG: pop_front while a send is in flight")]
    fn pop_front_while_a_send_is_in_flight_panics_in_debug() {
        let mut buf = GapBuffer::new(2, OverflowPolicy::DropOldest);
        buf.enqueue(vec![1]).unwrap();
        let _ = buf.begin_send().expect("non-empty");
        let _ = buf.pop_front();
    }

    #[test]
    fn begin_send_on_empty_buffer_returns_none() {
        let mut buf = GapBuffer::new(2, OverflowPolicy::DropOldest);
        assert!(buf.begin_send().is_none());
    }

    #[test]
    fn drop_oldest_eviction_skips_the_in_flight_front() {
        let mut buf = GapBuffer::new(2, OverflowPolicy::DropOldest);
        buf.enqueue(vec![1]).unwrap();
        buf.enqueue(vec![2]).unwrap();
        let (seq, msg) = buf.begin_send().expect("non-empty");
        assert_eq!(msg, vec![1]);
        // Full: the victim is [2], not the in-flight [1].
        buf.enqueue(vec![3]).unwrap();
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.messages_dropped, 1);
        assert_eq!(buf.bytes_dropped, 1);
        assert_eq!(
            buf.finish_send(seq).unwrap(),
            vec![1],
            "finish_send pops exactly the in-flight message"
        );
        assert_eq!(buf.pop_front().unwrap(), vec![3]);
        assert!(buf.is_empty());
    }

    #[test]
    fn abort_send_leaves_the_message_at_the_front() {
        let mut buf = GapBuffer::new(4, OverflowPolicy::DropOldest);
        buf.enqueue(vec![1]).unwrap();
        buf.enqueue(vec![2]).unwrap();
        let (seq, msg) = buf.begin_send().expect("non-empty");
        assert_eq!(msg, vec![1]);
        buf.abort_send(seq);
        assert_eq!(buf.len(), 2, "aborting settles the mark, not the queue");
        // The retry re-reads the same front message.
        let (seq2, msg2) = buf.begin_send().expect("non-empty");
        assert_eq!((seq2, msg2), (seq, vec![1]));
        assert_eq!(buf.finish_send(seq2).unwrap(), vec![1]);
        assert_eq!(buf.pop_front().unwrap(), vec![2]);
    }

    #[test]
    fn reject_policy_still_refuses_with_an_in_flight_front() {
        let mut buf = GapBuffer::new(1, OverflowPolicy::Reject);
        buf.enqueue(vec![1]).unwrap();
        let (seq, _) = buf.begin_send().expect("non-empty");
        assert_eq!(buf.enqueue(vec![2]), Err(GapBufferError::Full));
        assert_eq!(buf.messages_dropped, 0);
        assert_eq!(buf.finish_send(seq).unwrap(), vec![1]);
    }

    #[test]
    fn sole_in_flight_entry_pushes_past_capacity_then_drains_back() {
        let mut buf = GapBuffer::new(1, OverflowPolicy::DropOldest);
        buf.enqueue(vec![1]).unwrap();
        let (seq, _) = buf.begin_send().expect("non-empty");
        // At capacity with only the in-flight entry: push anyway rather
        // than evict the message the worker is already sending.
        buf.enqueue(vec![2]).unwrap();
        assert_eq!(buf.len(), 2, "transient capacity + 1");
        assert_eq!(buf.messages_dropped, 0);
        // A third message evicts the follower, never the in-flight front.
        buf.enqueue(vec![3]).unwrap();
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.messages_dropped, 1);
        assert_eq!(buf.finish_send(seq).unwrap(), vec![1]);
        assert_eq!(buf.len(), 1, "back within capacity");
        assert_eq!(buf.pop_front().unwrap(), vec![3]);
    }

    #[test]
    fn zero_capacity_buffers_nothing_and_counts_every_drop() {
        let mut buf = GapBuffer::new(0, OverflowPolicy::DropOldest);
        buf.enqueue(vec![1, 2, 3]).unwrap();
        assert_eq!(buf.len(), 0, "a zero-capacity buffer holds nothing");
        assert_eq!(buf.messages_dropped, 1);
        assert_eq!(buf.bytes_dropped, 3);
        // Nothing is ever queued, so no entry can be marked in flight and
        // the in-flight overshoot cannot widen a zero capacity either.
        assert!(buf.begin_send().is_none());
        buf.enqueue(vec![4, 5]).unwrap();
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.messages_dropped, 2);
        assert_eq!(buf.bytes_dropped, 5);

        let mut reject = GapBuffer::new(0, OverflowPolicy::Reject);
        assert_eq!(reject.enqueue(vec![1]), Err(GapBufferError::Full));
        assert_eq!(reject.len(), 0);
    }
}
