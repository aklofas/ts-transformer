//! [`StreamEndReason`] — structured, first-writer-wins record of why an
//! RTSP-backed receive session ended.
//!
//! Every session-death site on the client side (the interleaved pump's
//! exit paths, the keepalive thread's failure paths) records its reason
//! into a shared `EndReasonSlot` the moment it observes the failure —
//! not reconstructed after the fact from whatever terminal error a
//! caller happens to see. [`crate::rtsp::client::session::RtspSession::into_recv_transport`] /
//! `into_h264_receiver` clone the slot from the owning `RtspClient` so
//! the resulting `RtpRecvTransport` / `H264Receiver` can answer
//! [`StreamEndReasonHandle::get`] long after the `RtspClient` itself
//! (and its background threads) are gone.

use std::sync::{Arc, OnceLock};

/// Why an RTSP-backed receive session ended.
///
/// Recorded once, first-writer-wins, at whichever site actually observed
/// the death — see `EndReasonSlot`. `None` from
/// [`StreamEndReasonHandle::get`] means the session either hasn't ended
/// yet or ended through a path that records no reason (e.g. a plain
/// `rtp://` transport that was never closed or cancelled).
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum StreamEndReason {
    /// The peer closed the connection in an orderly way (TCP EOF) with
    /// no protocol or transport error.
    CleanTeardown,
    /// The server no longer honors the session — a keepalive ping was
    /// answered `454 Session Not Found`.
    SessionExpired,
    /// The keepalive background thread failed to encode or send a ping
    /// (e.g. the control connection is no longer writable).
    KeepaliveFailed {
        /// Human-readable detail from the underlying failure.
        msg: String,
    },
    /// A hard I/O error on the underlying transport (a read failure
    /// other than clean EOF).
    TransportFailed {
        /// Human-readable detail from the underlying failure.
        msg: String,
    },
    /// The peer violated the wire protocol (oversized/malformed framing,
    /// a queue flood) and the session was failed rather than silently
    /// tolerated.
    ProtocolError {
        /// Human-readable detail from the underlying failure.
        msg: String,
    },
    /// The session ended because the caller explicitly cancelled or
    /// closed the transport — not because of any wire-level failure.
    Cancelled,
}

/// Shared, first-writer-wins cell for a [`StreamEndReason`].
///
/// Cloning shares the same underlying cell (`Arc<OnceLock<_>>`) — every
/// clone observes the same recorded value. [`Self::record`] is a no-op
/// once a value has already been set: the FIRST site to observe the
/// session's death owns the reason, and a later, less-specific signal
/// (e.g. the mpsc bridge going `Disconnected` after the producer already
/// recorded why it exited) must not clobber it.
#[derive(Debug, Clone, Default)]
pub(crate) struct EndReasonSlot(Arc<OnceLock<StreamEndReason>>);

impl EndReasonSlot {
    /// Record `r` iff no reason has been recorded yet. Silently ignored
    /// on a second (or later) call — first-writer-wins.
    pub(crate) fn record(&self, r: StreamEndReason) {
        let _ = self.0.set(r);
    }

    /// The recorded reason, if any.
    pub(crate) fn get(&self) -> Option<StreamEndReason> {
        self.0.get().cloned()
    }
}

/// Cloneable, cross-thread-safe handle onto an `EndReasonSlot`.
///
/// Returned by [`crate::RtpRecvTransport::end_reason_handle`] so a
/// caller can poll the end reason from a thread other than the one
/// driving `recv_bytes` (e.g. a watchdog) without holding a reference to
/// the transport itself.
#[derive(Debug, Clone)]
pub struct StreamEndReasonHandle(EndReasonSlot);

impl StreamEndReasonHandle {
    pub(crate) fn new(slot: EndReasonSlot) -> Self {
        Self(slot)
    }

    /// The recorded [`StreamEndReason`], or `None` if the session hasn't
    /// ended yet (or ended through a path that records no reason).
    pub fn get(&self) -> Option<StreamEndReason> {
        self.0.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_is_first_writer_wins() {
        let slot = EndReasonSlot::default();
        assert!(slot.get().is_none(), "fresh slot must start empty");
        slot.record(StreamEndReason::Cancelled);
        slot.record(StreamEndReason::CleanTeardown);
        assert!(
            matches!(slot.get(), Some(StreamEndReason::Cancelled)),
            "the SECOND record() call must not clobber the first"
        );
    }

    #[test]
    fn clone_shares_the_same_cell() {
        let a = EndReasonSlot::default();
        let b = a.clone();
        a.record(StreamEndReason::SessionExpired);
        assert!(
            matches!(b.get(), Some(StreamEndReason::SessionExpired)),
            "a clone must observe the same recorded value"
        );
    }

    /// A value recorded on one thread must be visible to a
    /// [`StreamEndReasonHandle`] clone held on another — this is the
    /// cross-thread contract `end_reason_handle()` exists for (a
    /// watchdog thread polling a receive session driven elsewhere).
    #[test]
    fn handle_observes_recording_across_threads() {
        let slot = EndReasonSlot::default();
        let handle = StreamEndReasonHandle::new(slot.clone());
        assert!(handle.get().is_none());

        let writer = std::thread::spawn(move || {
            slot.record(StreamEndReason::TransportFailed { msg: "boom".into() });
        });
        writer.join().unwrap();

        match handle.get() {
            Some(StreamEndReason::TransportFailed { msg }) => assert_eq!(msg, "boom"),
            other => panic!("expected TransportFailed, got {other:?}"),
        }
    }

    /// `StreamEndReasonHandle` must be safe to hand to a different
    /// thread than the one driving I/O — bindings (JVM/Python/C) will do
    /// exactly that.
    #[test]
    fn stream_end_reason_handle_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StreamEndReasonHandle>();
    }
}
