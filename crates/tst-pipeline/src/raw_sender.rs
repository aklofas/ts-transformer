//! `RawSender<T: Transport>` — one-shot byte-blind sender.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Each `send` call sends exactly one outbound message of the given
//! length. No buffering, no framing, no accumulation. Caller is
//! responsible for sizing each message to the transport's
//! `max_payload()` (typically 1316 bytes for SRT live mode).
//!
//! Wrap with [`crate::ManagedTransport`] for reconnection.

use alloc::boxed::Box;
use alloc::sync::Arc;
use tracing::info_span;
use tst_core::transport::{Transport, TransportError};

use crate::shell_error::ShellErrorKind;

/// Construction-time knobs for [`RawSender`].
///
/// Currently empty — no behavior knobs are needed today. Reserved as a
/// distinct type so future additions are non-breaking.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct RawSenderConfig {
    // Reserved for future use. Currently empty.
    _private: (),
}

/// Error returned by [`RawSender::send`].
///
/// # Categorization
///
/// Bindings categorize failures via [`Self::kind`] (one of 6
/// [`ShellErrorKind`] variants); power users inspect [`Self::source`]
/// for the typed inner error.
///
/// # Reachable kinds
///
/// `RawSender` can produce: `Backpressure`, `InputMalformed` (payload
/// exceeds max), `TransportBroken`, `Closed`. All other
/// [`ShellErrorKind`] variants are unreachable (no muxer, no framing,
/// no demux involved).
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
#[error("RawSender error ({kind:?}): {source}")]
pub struct RawSenderError {
    pub kind: ShellErrorKind,
    #[source]
    pub source: RawSenderErrorSource,
}

/// Typed source enum for [`RawSenderError`]. Single-variant today;
/// `#[non_exhaustive]` preserves future-proof shape symmetry with
/// `MuxSenderErrorSource` and other multi-source shells.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum RawSenderErrorSource {
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl From<TransportError> for RawSenderError {
    fn from(e: TransportError) -> Self {
        Self {
            kind: crate::shell_error::kind_from_transport(&e, crate::shell_error::Direction::Send),
            source: RawSenderErrorSource::Transport(e),
        }
    }
}

impl crate::shell_error::ShellError for RawSenderError {
    fn kind(&self) -> ShellErrorKind {
        self.kind
    }

    fn errno_code(&self) -> Option<i32> {
        match &self.source {
            RawSenderErrorSource::Transport(t) => crate::shell_error::errno_code_from_transport(t),
        }
    }
}

/// Stats for [`RawSender`]. Aggregate-only — there are no streams at
/// this layer.
#[must_use]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RawSendStats {
    /// Bytes that succeeded through the transport.
    pub bytes_sent: u64,
    /// Count of successful `send()` calls.
    pub packets_sent: u64,
}

/// One-shot byte-blind sender. See module docs for the no-buffering /
/// no-framing contract.
///
/// `RawSender` performs no pending retention — each `send()` call is a
/// single, independent attempt on its own input, so there is no
/// cross-call ambiguity, hence no `input_consumed` field (contrast
/// [`crate::Sender::send_ts`] and [`crate::MuxSender`], which retain
/// undelivered bytes across calls and so report per-error consumption).
/// Whether THIS call's bytes were delivered on error is governed by the
/// [`Transport::send_bytes`] contract, not by `RawSender`: only
/// `TransportError::Backpressure` guarantees they were not consumed — on
/// any other error the transport state is undefined and retrying the
/// same bytes is not guaranteed safe.
///
/// # Closing
///
/// `RawSender` supports three shutdown patterns:
///
/// 1. **Drop** — the [`Drop`] impl is currently a no-op for the transport
///    (the explicit `close()` is the canonical close path). The
///    underlying transport's own `Drop` runs after this struct's `Drop`,
///    which closes the libsrt socket; bounded by `SRTO_LINGER` (libsrt
///    default 30 s, configurable via `SocketBuilder::linger`).
/// 2. **Explicit close** — call [`Self::close`]. Closes the underlying
///    transport. Idempotent.
/// 3. **Cross-thread cancel** — call [`Self::cancel_handle`] to obtain a
///    `Send + Sync` [`tst_core::transport::TransportCancel`] handle,
///    then `cancel()` from any thread. Wakes a peer thread parked in
///    `send` within one libsrt I/O cycle (~3-10 ms).
///
/// ## Per-language idiom
///
/// | Language | Idiom |
/// |----------|-------|
/// | Rust | `let _ = sender;` (Drop) or `sender.cancel_handle().map(\|c\| c.cancel());` (cross-thread) |
/// | Java | Wrap as `AutoCloseable`; `try-with-resources` calls `close()` on exit |
/// | Kotlin | Wrap as `AutoCloseable`; `.use { }` calls `close()` on exit |
/// | Swift | `deinit` calls drop; `defer { handle.cancel() }` for explicit cross-thread |
/// | Python | Wrap as `__enter__`/`__exit__`; `with ... as sender:` calls `close()` on exit |
/// | C | `tst_raw_sender_close(sender)` (explicit; mirrors `Drop`) |
///
/// See [`docs/reference/srt-cancel-handle.md`](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/srt-cancel-handle.md) for the full cancel-handle pattern.
pub struct RawSender<T: Transport> {
    transport: T,
    _config: RawSenderConfig,
    stats: RawSendStats,
    /// Lifetime span — see [`crate::shell_error::ShellSpan`] for the
    /// unwind-safe rationale. Private; never exposed publicly.
    _span: crate::shell_error::ShellSpan,
}

impl<T: Transport> core::fmt::Debug for RawSender<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RawSender")
            .field("is_alive", &self.is_alive())
            .field("bytes_sent", &self.stats.bytes_sent)
            .field("transport_kind", &core::any::type_name::<T>())
            .finish()
    }
}

impl<T: Transport> RawSender<T> {
    pub fn new(transport: T, config: RawSenderConfig) -> Self {
        let span = info_span!(
            target: "tst_pipeline::raw_sender",
            "raw_sender",
            transport_kind = core::any::type_name::<T>(),
        );
        let _enter = span.enter();
        tracing::info!("RawSender opened");
        drop(_enter);
        Self {
            transport,
            _config: config,
            stats: RawSendStats::default(),
            _span: core::panic::AssertUnwindSafe(span),
        }
    }

    /// Send one outbound message. Validates `bytes.len() ≤ transport.max_payload()`
    /// before delegating; the transport may add its own validation on top.
    ///
    /// Use this when the caller has its own muxer; for muxer integration
    /// use [`crate::Sender`] (pre-muxed TS bytes) or [`crate::MuxSender`]
    /// (encoded video / KLV / audio / subtitle in, TS out).
    ///
    /// # C ABI
    ///
    /// `tst_raw_sender_send` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    ///
    /// Returns [`RawSenderError`] with `kind` one of:
    /// - [`ShellErrorKind::InputMalformed`] — `bytes.len()` exceeds
    ///   `transport.max_payload()`.
    /// - [`ShellErrorKind::Backpressure`] — transport's send buffer is full;
    ///   retry after backing off.
    /// - [`ShellErrorKind::TransportBroken`] — transport is dead; the handle
    ///   is unusable.
    /// - [`ShellErrorKind::Closed`] — caller already invoked `close()`.
    ///
    /// # Example
    /// ```
    /// use tst_pipeline::{RawSender, RawSenderConfig};
    /// use tst_core::transport::{Transport, TransportError};
    ///
    /// // In-memory sink; real callers plug in `tst_srt::SrtTransport`.
    /// struct Sink(Vec<u8>);
    /// impl Transport for Sink {
    ///     fn send_bytes(&mut self, b: &[u8]) -> Result<(), TransportError> {
    ///         self.0.extend_from_slice(b);
    ///         Ok(())
    ///     }
    ///     fn max_payload(&self) -> usize { 1316 }
    ///     fn close(&mut self) {}
    ///     fn is_alive(&self) -> bool { true }
    /// }
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut sender = RawSender::new(Sink(Vec::new()), RawSenderConfig::default());
    /// sender.send(&[0u8; 1316])?;
    /// assert_eq!(sender.stats().packets_sent, 1);
    /// # Ok(())
    /// # }
    /// ```
    pub fn send(&mut self, bytes: &[u8]) -> Result<(), RawSenderError> {
        let max = self.transport.max_payload();
        if bytes.len() > max {
            return Err(TransportError::TooLarge {
                len: bytes.len(),
                max,
            }
            .into());
        }
        self.transport.send_bytes(bytes)?;
        self.stats.bytes_sent += bytes.len() as u64;
        self.stats.packets_sent += 1;
        Ok(())
    }

    pub fn close(&mut self) {
        self.transport.close();
    }

    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.transport.is_alive()
    }

    /// Borrow the inner transport (e.g., for stats accessors specific to
    /// the transport type).
    ///
    /// # C ABI
    ///
    /// `tst_raw_sender_get_socket_stats` reaches through this accessor to
    /// call [`Transport::socket_stats`] on the inner transport. See
    /// `bindings/c/include/tstrans.h`.
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Snapshot of the underlying transport's cancel handle.
    ///
    /// # C ABI
    ///
    /// `tst_raw_sender_cancel` — see `bindings/c/include/tstrans.h`.
    pub fn cancel_handle(
        &self,
    ) -> Option<Arc<dyn tst_core::transport::TransportCancel + Send + Sync>> {
        self.transport.cancel_handle()
    }

    /// Snapshot stats counters.
    pub fn stats(&self) -> RawSendStats {
        self.stats
    }

    /// Zero all stats counters. Stats-only — does not affect transport,
    /// pending data, or any other state.
    pub fn reset_stats(&mut self) {
        self.stats = RawSendStats::default();
    }
}

/// Type alias for [`RawSender`] with a boxed [`Transport`] trait object.
///
/// See [`BoxedMuxSender`](crate::mux_sender::BoxedMuxSender) for rationale.
///
/// # Example
/// ```no_run
/// use tst_pipeline::raw_sender::BoxedRawSender;
/// use tst_pipeline::RawSender;
/// use tst_core::Transport;
///
/// fn open(transport: Box<dyn Transport>) -> BoxedRawSender {
///     RawSender::new(transport, Default::default())
/// }
/// ```
pub type BoxedRawSender = RawSender<Box<dyn crate::Transport>>;

impl<T: Transport> Drop for RawSender<T> {
    fn drop(&mut self) {
        let _enter = self._span.0.enter();
        tracing::info!("RawSender closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tst_core::transport::BrokenCause;
    use tst_core::transport::{Transport, TransportError};

    struct MemTransport {
        max: usize,
        alive: bool,
        accept: bool,
    }
    impl Transport for MemTransport {
        fn send_bytes(&mut self, _bytes: &[u8]) -> Result<(), TransportError> {
            if self.accept {
                Ok(())
            } else {
                Err(TransportError::Broken {
                    msg: "test".into(),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                })
            }
        }
        fn max_payload(&self) -> usize {
            self.max
        }
        fn close(&mut self) {
            self.alive = false;
        }
        fn is_alive(&self) -> bool {
            self.alive
        }
    }

    #[test]
    fn stats_starts_zero() {
        let s = RawSender::new(
            MemTransport {
                max: 1316,
                alive: true,
                accept: true,
            },
            RawSenderConfig::default(),
        );
        let st = s.stats();
        assert_eq!(st.bytes_sent, 0);
        assert_eq!(st.packets_sent, 0);
    }

    #[test]
    fn stats_increment_on_successful_send() {
        let mut s = RawSender::new(
            MemTransport {
                max: 1316,
                alive: true,
                accept: true,
            },
            RawSenderConfig::default(),
        );
        s.send(&[0u8; 100]).unwrap();
        s.send(&[0u8; 200]).unwrap();
        let st = s.stats();
        assert_eq!(st.bytes_sent, 300);
        assert_eq!(st.packets_sent, 2);
    }

    #[test]
    fn stats_unchanged_on_too_large() {
        let mut s = RawSender::new(
            MemTransport {
                max: 100,
                alive: true,
                accept: true,
            },
            RawSenderConfig::default(),
        );
        let _ = s.send(&[0u8; 200]); // exceeds max
        let st = s.stats();
        assert_eq!(st.bytes_sent, 0);
        assert_eq!(st.packets_sent, 0);
    }

    #[test]
    fn stats_unchanged_on_transport_error() {
        let mut s = RawSender::new(
            MemTransport {
                max: 1316,
                alive: true,
                accept: false,
            },
            RawSenderConfig::default(),
        );
        let _ = s.send(&[0u8; 100]);
        let st = s.stats();
        assert_eq!(st.bytes_sent, 0);
        assert_eq!(st.packets_sent, 0);
    }

    #[test]
    fn reset_zeros_counters() {
        let mut s = RawSender::new(
            MemTransport {
                max: 1316,
                alive: true,
                accept: true,
            },
            RawSenderConfig::default(),
        );
        s.send(&[0u8; 100]).unwrap();
        s.reset_stats();
        let st = s.stats();
        assert_eq!(st.bytes_sent, 0);
        assert_eq!(st.packets_sent, 0);
    }

    #[test]
    fn mem_transport_default_cancel_handle_is_none() {
        let t = MemTransport {
            max: 1316,
            alive: true,
            accept: true,
        };
        assert!(t.cancel_handle().is_none());
    }

    /// D5 follow-up: `ShellError::errno_code()` reaches through the
    /// typed source tree and returns the inner TransportError's
    /// `errno_code` field. Other ShellError types share this shape.
    #[test]
    fn shell_error_errno_code_reaches_through_source() {
        use crate::shell_error::ShellError;

        let err: RawSenderError = TransportError::Broken {
            msg: "test".into(),
            errno_code: Some(2),
            cause: BrokenCause::Unspecified,
        }
        .into();
        assert_eq!(err.errno_code(), Some(2));
        assert_eq!(err.kind(), ShellErrorKind::TransportBroken);

        // None propagates as None.
        let err_none: RawSenderError = TransportError::Backpressure {
            msg: "test".into(),
            errno_code: None,
        }
        .into();
        assert_eq!(err_none.errno_code(), None);

        // Non-carrying TransportError variants return None at the
        // shell layer too.
        let err_closed: RawSenderError = TransportError::Closed.into();
        assert_eq!(err_closed.errno_code(), None);
    }
}
