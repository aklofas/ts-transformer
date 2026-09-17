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
    pub fn enqueue(&mut self, msg: Vec<u8>) -> Result<(), GapBufferError> {
        if self.queue.len() >= self.capacity {
            match self.overflow {
                OverflowPolicy::DropOldest => {
                    let victim = usize::from(self.front_is_in_flight());
                    if let Some((_, dropped)) = self.queue.remove(victim) {
                        self.bytes_dropped += dropped.len() as u64;
                        self.messages_dropped += 1;
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

    pub fn pop_front(&mut self) -> Option<Vec<u8>> {
        self.queue.pop_front().map(|(_, msg)| msg)
    }

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
    /// Panics if `seq` is not the in-flight front. That would mean the
    /// buffer desynchronised while the lock was released — the one
    /// condition the in-flight mark exists to prevent.
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
}
