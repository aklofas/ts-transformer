//! `RawReceiver<R>` — return one owned byte vec per recv, no TS framing.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! This is the simplest receive shell: one `recv_one` call blocks until a
//! single SRT message arrives, then returns the bytes verbatim. There is no
//! MPEG-TS sync recovery or stream demuxing — that's `Receiver`'s job.
//!
//! Use `RawReceiver` when:
//! - The sender uses `RawSender` (raw byte blobs, no TS wrapping).
//! - You want to handle framing yourself.
//! - You're writing a test that needs a bare receive loop.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use tracing::info_span;
use tst_core::transport::RecvTransport;
use tst_core::transport::TransportError;

use crate::shell_error::ShellErrorKind;

/// Aggregate receive stats for [`RawReceiver`].
#[must_use]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RawRecvStats {
    /// Total bytes received from the transport.
    pub bytes_received: u64,
    /// Count of successful `recv_one()` calls.
    pub packets_received: u64,
}

/// Receive shell that emits one raw byte vec per transport message.
///
/// `R` is any [`RecvTransport`] — typically `SrtTransport` for live
/// connections, or a test mock for unit tests.
///
/// # Closing
///
/// `RawReceiver` supports three shutdown patterns:
///
/// 1. **Drop** — the [`Drop`] impl emits a tracing event and lets the
///    underlying transport's `Drop` close the libsrt socket. Synchronous;
///    bounded by `SRTO_LINGER` (libsrt default off, sender preset 5 s, configurable via
///    `SocketBuilder::linger`).
/// 2. **Explicit close** — call [`Self::close`]. Closes the underlying
///    transport; subsequent `recv_one` calls return
///    [`TransportError::Closed`]. Idempotent.
/// 3. **Cross-thread cancel** — call [`Self::cancel_handle`] to obtain a
///    `Send + Sync` [`tst_core::transport::TransportCancel`] handle,
///    then `cancel()` from any thread. Wakes a peer thread parked in
///    `recv_one` within one libsrt I/O cycle (~3-10 ms).
///
/// # C ABI
///
/// `tst_raw_receiver_close` (plain) — see `bindings/c/include/tstrans.h`.
///
/// ## Per-language idiom
///
/// | Language | Idiom |
/// |----------|-------|
/// | Rust | `let _ = rx;` (Drop) or `rx.cancel_handle().map(\|c\| c.cancel());` (cross-thread) |
/// | Java | Wrap as `AutoCloseable`; `try-with-resources` calls `close()` on exit |
/// | Kotlin | Wrap as `AutoCloseable`; `.use { }` calls `close()` on exit |
/// | Swift | `deinit` calls drop; `defer { handle.cancel() }` for explicit cross-thread |
/// | Python | Wrap as `__enter__`/`__exit__`; `with ... as rx:` calls `close()` on exit |
/// | C | `tst_raw_receiver_close(rx)` (explicit; mirrors `Drop`); `tst_raw_receiver_cancel(handle)` from any thread |
///
/// See [`docs/reference/srt-cancel-handle.md`](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/srt-cancel-handle.md) for the full cancel-handle pattern.
pub struct RawReceiver<R: RecvTransport> {
    transport: R,
    /// Reusable scratch buffer sized to `transport.max_payload()` on
    /// construction. Avoids a per-call allocation for the recv itself;
    /// `recv_one` still allocates a `Vec` for the returned slice.
    buf: Vec<u8>,
    stats: RawRecvStats,
    /// Lifetime span — see [`crate::shell_error::ShellSpan`] for the
    /// unwind-safe rationale. Private; never exposed publicly.
    _span: crate::shell_error::ShellSpan,
}

impl<R: RecvTransport> core::fmt::Debug for RawReceiver<R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RawReceiver")
            .field("is_alive", &self.is_alive())
            .field("buf_capacity", &self.buf.capacity())
            .field("bytes_received", &self.stats.bytes_received)
            .field("transport_kind", &core::any::type_name::<R>())
            .finish()
    }
}

/// Construction parameters for [`RawReceiver`].
///
/// Currently empty; reserved for future knobs that can be added
/// non-breakingly thanks to the `#[non_exhaustive]` annotation.
/// Construct via `Default::default()` and assign overrides as more
/// fields land.
///
/// Symmetric with [`crate::RawSenderConfig`] on the send side; the
/// symmetry is documented in `docs/reference/conventions.md`.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Default, Clone)]
pub struct RawReceiverConfig {}

/// Error returned by [`RawReceiver::recv_one`].
///
/// # Categorization
///
/// Bindings categorize failures via [`Self::kind`] (one of 6
/// [`ShellErrorKind`] variants); power users inspect [`Self::source`]
/// for the typed inner error.
///
/// # Reachable kinds
///
/// `RawReceiver` can produce: `Backpressure`, `TransportBroken`, `Closed`,
/// `EndOfStream`. `Backpressure` is produced when the underlying transport
/// returns `TransportError::Backpressure` on a recv timeout.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
#[error("RawReceiver error ({kind:?}): {source}")]
pub struct RawReceiverError {
    pub kind: ShellErrorKind,
    #[source]
    pub source: RawReceiverErrorSource,
}

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum RawReceiverErrorSource {
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl From<TransportError> for RawReceiverError {
    fn from(e: TransportError) -> Self {
        Self {
            kind: crate::shell_error::kind_from_transport(&e, crate::shell_error::Direction::Recv),
            source: RawReceiverErrorSource::Transport(e),
        }
    }
}

impl crate::shell_error::ShellError for RawReceiverError {
    fn kind(&self) -> ShellErrorKind {
        self.kind
    }

    fn errno_code(&self) -> Option<i32> {
        match &self.source {
            RawReceiverErrorSource::Transport(t) => {
                crate::shell_error::errno_code_from_transport(t)
            }
        }
    }
}

impl<R: RecvTransport> RawReceiver<R> {
    /// Wrap a transport with the supplied config. Allocates an
    /// internal buffer sized to `transport.max_payload()`.
    ///
    /// `RawReceiverConfig` is currently empty; construct via
    /// [`RawReceiverConfig::default()`].
    pub fn new(transport: R, _config: RawReceiverConfig) -> Self {
        let span = info_span!(
            target: "tst_pipeline::raw_receiver",
            "raw_receiver",
            transport_kind = core::any::type_name::<R>(),
        );
        let _enter = span.enter();
        tracing::info!("RawReceiver opened");
        drop(_enter);
        let cap = crate::clamp_recv_capacity(transport.max_payload());
        Self {
            transport,
            buf: vec![0u8; cap],
            stats: RawRecvStats::default(),
            _span: core::panic::AssertUnwindSafe(span),
        }
    }

    /// Block until one message arrives. Returns a copy of the received bytes.
    ///
    /// Use this when the peer uses [`crate::RawSender`] (raw byte blobs,
    /// no TS wrapping); for pre-muxed TS recovery use [`crate::Receiver`]
    /// or [`crate::DemuxReceiver`].
    ///
    /// # C ABI
    ///
    /// `tst_raw_receiver_recv` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    ///
    /// Returns [`RawReceiverError`] with `kind` one of:
    /// - [`ShellErrorKind::TransportBroken`] — transport socket is broken.
    /// - [`ShellErrorKind::Closed`] — caller invoked `close()` (or for
    ///   `ManagedRecvTransport`, the cancel signal fired).
    /// - [`ShellErrorKind::EndOfStream`] — peer closed the connection.
    ///
    /// # Example
    /// ```
    /// use std::collections::VecDeque;
    /// use tst_pipeline::RawReceiver;
    /// use tst_core::transport::{RecvTransport, TransportError};
    ///
    /// // In-memory source; real callers plug in `tst_srt::SrtTransport`.
    /// struct Source(VecDeque<Vec<u8>>);
    /// impl RecvTransport for Source {
    ///     fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
    ///         match self.0.pop_front() {
    ///             Some(v) => {
    ///                 let n = v.len().min(buf.len());
    ///                 buf[..n].copy_from_slice(&v[..n]);
    ///                 Ok(n)
    ///             }
    ///             None => Err(TransportError::Closed),
    ///         }
    ///     }
    ///     fn max_payload(&self) -> usize { 1316 }
    ///     fn is_alive(&self) -> bool { !self.0.is_empty() }
    /// }
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use tst_pipeline::RawReceiverConfig;
    /// let q = VecDeque::from(vec![b"hello".to_vec(), b"world".to_vec()]);
    /// let mut rx = RawReceiver::new(Source(q), RawReceiverConfig::default());
    /// assert_eq!(rx.recv_one()?, b"hello");
    /// assert_eq!(rx.recv_one()?, b"world");
    /// # Ok(())
    /// # }
    /// ```
    pub fn recv_one(&mut self) -> Result<Vec<u8>, RawReceiverError> {
        let n = self.transport.recv_bytes(&mut self.buf)?;
        self.stats.bytes_received += n as u64;
        self.stats.packets_received += 1;
        Ok(self.buf[..n].to_vec())
    }

    /// Return a snapshot of aggregate receive stats.
    ///
    /// # C ABI
    ///
    /// `tst_raw_receiver_get_stats` — see `bindings/c/include/tstrans.h`.
    pub fn stats(&self) -> RawRecvStats {
        self.stats
    }

    /// Zero all counters. Does not affect the underlying transport.
    ///
    /// # C ABI
    ///
    /// `tst_raw_receiver_reset_stats` — see `bindings/c/include/tstrans.h`.
    pub fn reset_stats(&mut self) {
        self.stats = RawRecvStats::default();
    }

    /// Wire-level transport stats (RTT, packet loss, bandwidth, queue
    /// depths) sourced from the underlying
    /// [`RecvTransport::socket_stats`] implementation. Returns `None`
    /// when the transport doesn't expose comparable telemetry (test
    /// mocks) or when a managed wrapper has no live inner socket.
    ///
    /// # C ABI
    ///
    /// `tst_raw_receiver_get_socket_stats` — see
    /// `bindings/c/include/tstrans.h`.
    pub fn socket_stats(&self) -> Option<tst_core::transport::SocketStats> {
        self.transport.socket_stats()
    }

    /// Advisory liveness check. Delegates to the underlying transport.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.transport.is_alive()
    }

    /// Close the underlying transport. Idempotent. After close, `recv_one`
    /// returns `TransportError::Closed`. Mirrors `RawSender::close`.
    ///
    /// # C ABI
    ///
    /// `tst_raw_receiver_close` — see `bindings/c/include/tstrans.h`.
    pub fn close(&mut self) {
        self.transport.close();
    }

    /// Snapshot of the underlying recv-transport's cancel handle.
    ///
    /// # C ABI
    ///
    /// `tst_raw_receiver_cancel` — see `bindings/c/include/tstrans.h`.
    pub fn cancel_handle(
        &self,
    ) -> Option<Arc<dyn tst_core::transport::TransportCancel + Send + Sync>> {
        self.transport.cancel_handle()
    }
}

impl<R: RecvTransport> Drop for RawReceiver<R> {
    fn drop(&mut self) {
        let _enter = self._span.0.enter();
        tracing::info!("RawReceiver closed");
    }
}

/// Type alias for [`RawReceiver`] with a boxed [`RecvTransport`] trait object.
///
/// See [`BoxedMuxSender`](crate::mux_sender::BoxedMuxSender) for rationale.
///
/// # Example
/// ```no_run
/// use tst_pipeline::raw_receiver::BoxedRawReceiver;
/// use tst_pipeline::{RawReceiver, RawReceiverConfig};
/// use tst_core::RecvTransport;
///
/// fn open(transport: Box<dyn RecvTransport>) -> BoxedRawReceiver {
///     RawReceiver::new(transport, RawReceiverConfig::default())
/// }
/// ```
pub type BoxedRawReceiver = RawReceiver<Box<dyn crate::RecvTransport>>;

#[cfg(test)]
mod tests {
    use super::*;
    use tst_core::transport::RecvTransport;
    use tst_core::transport::TransportError;

    struct MemRecv {
        queue: std::collections::VecDeque<Vec<u8>>,
        alive: bool,
    }
    impl RecvTransport for MemRecv {
        fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            match self.queue.pop_front() {
                Some(v) => {
                    let n = v.len().min(buf.len());
                    buf[..n].copy_from_slice(&v[..n]);
                    Ok(n)
                }
                None => Err(TransportError::Closed),
            }
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn is_alive(&self) -> bool {
            self.alive
        }
    }

    /// A `RecvTransport` reporting an absurd `max_payload()` (e.g. a
    /// hostile/buggy transport, or a URL `pkt_size` that overflowed before
    /// the URL parsers were bounds-checked). `RawReceiver::new` must clamp
    /// the eager pre-allocation rather than attempt a usize::MAX-ish `vec!`.
    struct HostileRecv;
    impl RecvTransport for HostileRecv {
        fn recv_bytes(&mut self, _buf: &mut [u8]) -> Result<usize, TransportError> {
            Err(TransportError::Closed)
        }
        fn max_payload(&self) -> usize {
            usize::MAX
        }
        fn is_alive(&self) -> bool {
            false
        }
    }

    #[test]
    fn hostile_max_payload_does_not_oom_constructor() {
        // Without the clamp this would attempt to allocate ~usize::MAX bytes
        // and abort. With it, the buffer is bounded to MAX_RECV_BUFFER.
        let r = RawReceiver::new(HostileRecv, RawReceiverConfig::default());
        // No assertion on capacity beyond "we got here without aborting", but
        // confirm the buffer is the bounded size, not usize::MAX.
        assert!(r.buf.capacity() <= crate::MAX_RECV_BUFFER);
    }

    #[test]
    fn clamp_recv_capacity_bounds_and_passes_through() {
        assert_eq!(crate::clamp_recv_capacity(1316), 1316);
        assert_eq!(
            crate::clamp_recv_capacity(crate::MAX_RECV_BUFFER),
            crate::MAX_RECV_BUFFER
        );
        assert_eq!(
            crate::clamp_recv_capacity(usize::MAX),
            crate::MAX_RECV_BUFFER
        );
    }

    #[test]
    fn stats_starts_zero() {
        let r = RawReceiver::new(
            MemRecv {
                queue: Default::default(),
                alive: true,
            },
            RawReceiverConfig::default(),
        );
        let st = r.stats();
        assert_eq!(st.bytes_received, 0);
        assert_eq!(st.packets_received, 0);
    }

    #[test]
    fn stats_increment_on_recv() {
        let mut q = std::collections::VecDeque::new();
        q.push_back(vec![1u8; 100]);
        q.push_back(vec![2u8; 50]);
        let mut r = RawReceiver::new(
            MemRecv {
                queue: q,
                alive: true,
            },
            RawReceiverConfig::default(),
        );
        let _ = r.recv_one();
        let _ = r.recv_one();
        let st = r.stats();
        assert_eq!(st.bytes_received, 150);
        assert_eq!(st.packets_received, 2);
    }

    #[test]
    fn reset_zeros_counters() {
        let mut q = std::collections::VecDeque::new();
        q.push_back(vec![1u8; 100]);
        let mut r = RawReceiver::new(
            MemRecv {
                queue: q,
                alive: true,
            },
            RawReceiverConfig::default(),
        );
        let _ = r.recv_one();
        r.reset_stats();
        let st = r.stats();
        assert_eq!(st.bytes_received, 0);
        assert_eq!(st.packets_received, 0);
    }

    /// Minimal `RecvTransport` mock that plays back a fixed sequence of
    /// messages then signals closed.
    struct MockRecv {
        messages: Vec<Vec<u8>>,
        pos: usize,
        max: usize,
    }

    impl MockRecv {
        fn new(messages: Vec<Vec<u8>>) -> Self {
            let max = messages.iter().map(|m| m.len()).max().unwrap_or(1316);
            Self {
                messages,
                pos: 0,
                max,
            }
        }
    }

    impl RecvTransport for MockRecv {
        fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            if self.pos >= self.messages.len() {
                return Err(TransportError::Closed);
            }
            let msg = &self.messages[self.pos];
            let n = msg.len();
            buf[..n].copy_from_slice(msg);
            self.pos += 1;
            Ok(n)
        }

        fn max_payload(&self) -> usize {
            self.max
        }

        fn is_alive(&self) -> bool {
            self.pos < self.messages.len()
        }
    }

    #[test]
    fn raw_receiver_delivers_messages() {
        let msgs: Vec<Vec<u8>> = vec![b"hello".to_vec(), b"world".to_vec()];
        let mut rx = RawReceiver::new(MockRecv::new(msgs.clone()), RawReceiverConfig::default());

        assert_eq!(rx.recv_one().unwrap(), b"hello");
        assert_eq!(rx.recv_one().unwrap(), b"world");
        assert_eq!(
            rx.recv_one().unwrap_err().kind,
            crate::shell_error::ShellErrorKind::EndOfStream,
        );
    }

    #[test]
    fn raw_receiver_is_alive_tracks_transport() {
        let mut rx = RawReceiver::new(
            MockRecv::new(vec![b"x".to_vec()]),
            RawReceiverConfig::default(),
        );
        assert!(rx.is_alive());
        let _ = rx.recv_one(); // consume the one message
        assert!(!rx.is_alive());
    }
}
