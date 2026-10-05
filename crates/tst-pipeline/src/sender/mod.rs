//! `Sender<T: Transport>` — pre-muxed TS bytes → SRT, with framing.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! See `framing.rs` for the sync-acquisition / loss-detection state
//! machine. `Sender` composes `TsFraming` with a `Transport`.

mod framing;

pub use framing::{SenderStats, TsFraming, TsFramingError, TsFramingMode};

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use tracing::info_span;
use tst_core::transport::Transport;

/// Construction-time knobs for [`Sender`].
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct SenderConfig {
    pub framing_mode: TsFramingMode,
    /// Threshold (bytes consumed while UNSYNCED) above which RECOVER mode
    /// flags that sync has not been acquired. Default 18,800 (≈100
    /// packets' worth).
    ///
    /// When scanning for sync consumes more than this many bytes without
    /// acquiring it, `send_ts` returns
    /// [`TsFramingError::NoSyncAfterLimit`]. The count accumulates across
    /// `send_ts` calls until sync is acquired OR the watchdog fires —
    /// whichever comes first — and resets to zero either way, so RECOVER
    /// mode keeps scanning afterward and a persistently non-TS source trips
    /// the watchdog again every `max_unsynced_bytes`, not just once. Because
    /// the counter also resets on genuine sync acquisition, whether a given
    /// amount of garbage trips the watchdog can depend on how it's chunked
    /// (e.g. it clears on the call where sync is finally found, even if
    /// that call alone pushed more unsynced bytes than the limit, because
    /// the limit is only checked while still scanning). Set to
    /// `usize::MAX` to effectively disable the watchdog.
    pub max_unsynced_bytes: usize,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            framing_mode: TsFramingMode::Recover,
            max_unsynced_bytes: 18_800,
        }
    }
}

use crate::shell_error::ShellErrorKind;

/// Error returned by [`Sender`] methods.
///
/// # Categorization
///
/// Bindings categorize failures via [`Self::kind`] (one of 6
/// [`ShellErrorKind`] variants); power users inspect [`Self::source`]
/// for the typed inner error.
///
/// # Reachable kinds
///
/// `Sender` can produce: `InputMalformed` (STRICT-mode framing failure),
/// `Backpressure`, `TransportBroken`, `Closed`. `ConfigInvalid` and
/// `EndOfStream` are unreachable (no muxer, sender-only).
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
#[error("Sender error ({kind:?}): {source}")]
pub struct SenderError {
    pub kind: ShellErrorKind,
    #[source]
    pub source: SenderErrorSource,
    /// Whether THIS call's input was consumed by the framer.
    ///
    /// - `Some(false)` — not consumed; retrying the same input cannot
    ///   duplicate data.
    /// - `Some(true)` — consumed: framed and retained in the pending
    ///   queue (drains exactly once on the next `send_ts`/`flush`); do
    ///   NOT push the same input again. Also `Some(true)` for RECOVER
    ///   mode's `NoSyncAfterLimit` — see `send_ts`'s Retention contract.
    /// - `None` — the error did not originate from a `send_ts` input
    ///   path (e.g. `flush`, which has no per-call input).
    pub input_consumed: Option<bool>,
}

impl SenderError {
    pub(crate) fn with_input_consumed(mut self, consumed: bool) -> Self {
        self.input_consumed = Some(consumed);
        self
    }
}

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum SenderErrorSource {
    #[error(transparent)]
    Framing(#[from] TsFramingError),
    #[error(transparent)]
    Transport(#[from] tst_core::transport::TransportError),
}

impl From<TsFramingError> for SenderError {
    fn from(e: TsFramingError) -> Self {
        Self {
            kind: crate::shell_error::kind_from_framing(&e),
            source: SenderErrorSource::Framing(e),
            input_consumed: None,
        }
    }
}

impl From<tst_core::transport::TransportError> for SenderError {
    fn from(e: tst_core::transport::TransportError) -> Self {
        Self {
            kind: crate::shell_error::kind_from_transport(&e, crate::shell_error::Direction::Send),
            source: SenderErrorSource::Transport(e),
            input_consumed: None,
        }
    }
}

impl crate::shell_error::ShellError for SenderError {
    fn kind(&self) -> ShellErrorKind {
        self.kind
    }

    fn errno_code(&self) -> Option<i32> {
        match &self.source {
            SenderErrorSource::Transport(t) => crate::shell_error::errno_code_from_transport(t),
            SenderErrorSource::Framing(_) => None,
        }
    }
}

/// Pre-muxed TS bytes → SRT transport with sync framing/recovery.
///
/// # Closing
///
/// `Sender` supports three shutdown patterns:
///
/// 1. **Drop** — the [`Drop`] impl best-effort flushes any buffered
///    partial bundle and closes the underlying transport. Synchronous;
///    bounded by `SRTO_LINGER` (libsrt default 30 s, configurable via
///    `SocketBuilder::linger`).
/// 2. **Explicit close** — call [`Self::close`]. Best-effort flushes
///    any buffered partial bundle (same as Drop), marks the sender
///    closed so subsequent `send_ts` / `flush` calls return
///    [`SenderErrorSource::Transport`]`(`[`tst_core::transport::TransportError::Closed`]`)`,
///    then closes the transport. Idempotent. Equivalent to Drop —
///    `AutoCloseable`/`__exit__`/`.use { }` bindings that call
///    `close()` in their cleanup path do not lose any buffered bytes.
/// 3. **Cross-thread cancel** — call [`Self::cancel_handle`] to obtain a
///    `Send + Sync` [`tst_core::transport::TransportCancel`] handle,
///    then `cancel()` from any thread. Wakes a peer thread parked in
///    `send_ts` within one libsrt I/O cycle (~3-10 ms).
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
/// | C | `tst_sender_close(sender)` (explicit; mirrors `Drop`) |
///
/// See [`docs/reference/srt-cancel-handle.md`](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/srt-cancel-handle.md) for the full cancel-handle pattern.
pub struct Sender<T: Transport> {
    framing: TsFraming,
    transport: T,
    closed: bool,
    mode: TsFramingMode,
    /// Bundles that were framed and partially sent but whose transport call
    /// failed mid-sequence. The failed bundle and any remaining bundles from
    /// that `send_ts`/`flush` call are retained here and drained first on the
    /// next `send_ts`/`flush` call — the canonical "what the transport still
    /// needs to receive" list, delivered exactly once, in order. NOTE: an
    /// `Err` from `send_ts` means the input was consumed into this queue;
    /// recovery is `flush()`/the next call with NEW data — re-sending the
    /// same input would duplicate (see `send_ts`'s retention contract).
    pending_bundles: VecDeque<Vec<u8>>,
    /// Lifetime span — see [`crate::shell_error::ShellSpan`] for the
    /// unwind-safe rationale. Private; never exposed publicly.
    _span: crate::shell_error::ShellSpan,
}

impl<T: Transport> core::fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sender")
            .field("closed", &self.closed)
            .field("mode", &self.mode)
            .field("pending_bundles", &self.pending_bundles.len())
            .field("transport_kind", &core::any::type_name::<T>())
            .finish()
    }
}

impl<T: Transport> Sender<T> {
    pub fn new(transport: T, config: SenderConfig) -> Self {
        let span = info_span!(
            target: "tst_pipeline::sender",
            "sender",
            transport_kind = core::any::type_name::<T>(),
        );
        let _enter = span.enter();
        tracing::info!("Sender opened");
        drop(_enter);
        Self {
            framing: TsFraming::new(config.max_unsynced_bytes),
            transport,
            closed: false,
            mode: config.framing_mode,
            pending_bundles: VecDeque::new(),
            _span: core::panic::AssertUnwindSafe(span),
        }
    }

    /// Push pre-muxed TS bytes. RECOVER mode silently skips/recovers; in
    /// STRICT mode returns an error on misalignment.
    ///
    /// `bytes` need not be a whole number of 188-byte packets — partial
    /// packets are buffered until the next call (or [`Self::flush`]).
    /// Each emitted bundle to the underlying transport is sized to fit
    /// `transport.max_payload()`.
    ///
    /// # C ABI
    ///
    /// `tst_sender_send_ts` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// - [`SenderErrorSource::Framing`] in STRICT mode when the input fails
    ///   to align on a TS sync byte (`0x47`); in RECOVER mode when
    ///   scanning for sync consumes more than
    ///   [`SenderConfig::max_unsynced_bytes`] without acquiring it
    ///   ([`TsFramingError::NoSyncAfterLimit`]).
    /// - [`SenderErrorSource::Transport`] when the underlying [`Transport`]
    ///   returns an error (e.g. `Closed`, `Broken`, `Backpressure`).
    ///
    /// # Retention contract
    ///
    /// On a transport error the sender retains any bundles it could not send
    /// (the failed bundle and any that followed it within this call) in an
    /// internal queue. The next call to `send_ts` or `flush` drains that queue
    /// first before processing new input, so nothing is lost.
    ///
    /// Whether THIS call's `bytes` were consumed is reported via
    /// [`SenderError::input_consumed`] — `Some(true)` means `bytes` were
    /// already absorbed into internal state before the error: either framed
    /// and queued for the transport (do **not** call `send_ts` again with
    /// the same bytes, or the already-queued prefix is sent a second time),
    /// or — for RECOVER mode's [`TsFramingError::NoSyncAfterLimit`] —
    /// extended into the framing scan buffer, which may retain a tail of
    /// this call's bytes (a "safe" retry would duplicate that tail once
    /// sync is eventually acquired). Either way, do not resend the same
    /// `bytes`. `Some(false)` means the failure happened before `bytes` was
    /// touched (e.g. draining bundles retained by a PREVIOUS call), so
    /// retrying the same input is safe. To recover after `Backpressure`,
    /// back off and then call [`Self::flush`] (or the next `send_ts` with
    /// *new* data); the retained bundles drain first, exactly once, in
    /// order.
    ///
    /// # Example
    /// ```
    /// use tst_pipeline::{Sender, SenderConfig, TsFramingMode};
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
    /// // STRICT mode rejects any input that doesn't start with sync 0x47;
    /// // RECOVER (the default) silently resyncs.
    /// let mut cfg = SenderConfig::default();
    /// cfg.framing_mode = TsFramingMode::Strict;
    /// let mut sender = Sender::new(Sink(Vec::new()), cfg);
    ///
    /// // Three whole TS packets — STRICT-mode sync-verify needs 0x47 at
    /// // offsets 0, 188, and 376 before declaring the stream synced, so
    /// // a 2-packet input would buffer without entering SYNCED.
    /// let mut bytes = Vec::new();
    /// for _ in 0..3 {
    ///     bytes.push(0x47);
    ///     bytes.extend(vec![0u8; 187]);
    /// }
    /// sender.send_ts(&bytes)?;
    /// // 3 < 7 (one bundle), so no transport send yet — flush emits the
    /// // partial bundle of 3 packets.
    /// sender.flush()?;
    /// assert_eq!(sender.stats().packets_sent, 3);
    /// # Ok(())
    /// # }
    /// ```
    pub fn send_ts(&mut self, bytes: &[u8]) -> Result<(), SenderError> {
        if self.closed {
            return Err(
                SenderError::from(tst_core::transport::TransportError::Closed)
                    .with_input_consumed(false),
            );
        }
        // Drain any leftover from a previous failed call first. A failure
        // here happens BEFORE this call's `bytes` are touched.
        self.drain_pending()
            .map_err(|e| e.with_input_consumed(false))?;
        let bundles = if self.mode == TsFramingMode::Recover {
            // NoSyncAfterLimit is the only error this path can raise, and by
            // the time it fires `bytes` has already been extended into the
            // framing buffer (see TsFraming::push) — consumed, not untouched.
            self.framing
                .push(bytes)
                .map(|(bundles, _stats)| bundles)
                .map_err(|e| SenderError::from(e).with_input_consumed(true))?
        } else {
            self.framing
                .push_strict(bytes)
                .map_err(|e| SenderError::from(e).with_input_consumed(false))?
        };
        let mut iter = bundles.into_iter();
        for bundle in &mut iter {
            if let Err(e) = self.transport.send_bytes(&bundle) {
                // Retain the failed bundle + any remaining so the next
                // send_ts/flush call can re-try them in order. From here
                // `bytes` is framed and retained: a failure leaves it in
                // the pending queue, draining exactly once on the next call.
                self.pending_bundles.push_back(bundle);
                self.pending_bundles.extend(iter);
                return Err(SenderError::from(e).with_input_consumed(true));
            }
        }
        Ok(())
    }

    /// Emit any buffered partial bundle.
    ///
    /// # C ABI
    ///
    /// `tst_sender_flush` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// Returns [`SenderErrorSource::Transport`] when the underlying [`Transport`]
    /// rejects the flushed bundle (typically `Closed` after a prior
    /// [`Self::close`], or `Broken` on transport flap, or `Backpressure`).
    /// On `Backpressure` the bundle is retained (see the retention contract on
    /// [`Self::send_ts`]) and will be re-attempted on the next call.
    ///
    /// `flush` has no per-call input of its own (it only drains
    /// framing-internal and previously-retained bundles), so
    /// [`SenderError::input_consumed`] is always `None` on an error from
    /// this method.
    pub fn flush(&mut self) -> Result<(), SenderError> {
        if self.closed {
            return Err(tst_core::transport::TransportError::Closed.into());
        }
        self.drain_pending()?;
        let bundles = self.framing.flush();
        let mut iter = bundles.into_iter();
        for bundle in &mut iter {
            if let Err(e) = self.transport.send_bytes(&bundle) {
                self.pending_bundles.push_back(bundle);
                self.pending_bundles.extend(iter);
                return Err(e.into());
            }
        }
        Ok(())
    }

    /// Drain the `pending_bundles` queue. Returns on the first transport
    /// error, leaving the remaining bundles in the queue.
    fn drain_pending(&mut self) -> Result<(), SenderError> {
        while let Some(bundle) = self.pending_bundles.front() {
            match self.transport.send_bytes(bundle) {
                Ok(()) => {
                    self.pending_bundles.pop_front();
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    pub fn stats(&self) -> SenderStats {
        self.framing.stats()
    }

    /// Zero all stats counters. The framing state machine is untouched —
    /// only the counters on top of it.
    pub fn reset_stats(&mut self) {
        self.framing.reset_stats();
    }

    /// Wire-level transport stats (RTT, packet loss, bandwidth, queue
    /// depths) sourced from the underlying [`Transport::socket_stats`]
    /// implementation. Returns `None` when the transport doesn't expose
    /// comparable telemetry (test mocks) or when a managed wrapper has
    /// no live inner socket.
    ///
    /// # C ABI
    ///
    /// `tst_sender_get_socket_stats` — see
    /// `bindings/c/include/tstrans.h`.
    pub fn socket_stats(&self) -> Option<tst_core::transport::SocketStats> {
        self.transport.socket_stats()
    }

    pub fn close(&mut self) {
        // Best-effort flush of any buffered partial bundle BEFORE marking
        // closed (otherwise the subsequent flush() would early-return on
        // the `self.closed` guard). Mirrors Drop semantics so explicit
        // close == drop for AutoCloseable / __exit__ / .use { } /
        // tst_sender_close(...) idioms.
        let _ = self.flush();
        self.closed = true;
        self.transport.close();
    }

    #[must_use]
    pub fn is_alive(&self) -> bool {
        !self.closed && self.transport.is_alive()
    }

    /// Snapshot of the underlying transport's cancel handle. See
    /// [`crate::MuxSender::cancel_handle`] for the rationale.
    ///
    /// # C ABI
    ///
    /// `tst_sender_cancel` — see `bindings/c/include/tstrans.h`.
    pub fn cancel_handle(
        &self,
    ) -> Option<Arc<dyn tst_core::transport::TransportCancel + Send + Sync>> {
        self.transport.cancel_handle()
    }

    /// Borrow the underlying transport. Bindings use this to reach
    /// transport-specific telemetry not surfaced through the abstract
    /// [`Transport::socket_stats`] (e.g. SRT's 17-field `Stats`).
    ///
    /// Returns a shared reference; mutation is intentionally not
    /// exposed — the shell owns the transport's send-side lifecycle.
    pub fn transport(&self) -> &T {
        &self.transport
    }
}

/// Type alias for [`Sender`] with a boxed [`Transport`] trait object.
///
/// See [`BoxedMuxSender`](crate::mux_sender::BoxedMuxSender) for rationale and the per-binding pattern.
///
/// # Example — opaque sender from a runtime-chosen transport
/// ```no_run
/// use tst_pipeline::sender::BoxedSender;
/// use tst_pipeline::Sender;
/// use tst_core::Transport;
///
/// fn open(transport: Box<dyn Transport>) -> BoxedSender {
///     Sender::new(transport, Default::default())
/// }
/// ```
pub type BoxedSender = Sender<Box<dyn crate::Transport>>;

/// Drop flushes any buffered partial bundle (best-effort), then closes
/// the transport. Equivalent to calling [`Sender::close`] explicitly —
/// FFI bindings may use either idiom.
impl<T: Transport> Drop for Sender<T> {
    fn drop(&mut self) {
        if !self.closed {
            // Best-effort flush; ignore errors.
            let _ = self.flush();
            self.transport.close();
        }
        let _enter = self._span.0.enter();
        tracing::info!("Sender closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tst_core::transport::{Transport, TransportError};

    struct Mem;
    impl Transport for Mem {
        fn send_bytes(&mut self, _: &[u8]) -> Result<(), TransportError> {
            Ok(())
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn close(&mut self) {}
        fn is_alive(&self) -> bool {
            true
        }
    }

    #[test]
    fn reset_stats_zeros_counters_in_ts_sender() {
        let mut s = Sender::new(Mem, SenderConfig::default());
        // One 188-byte TS packet starting with the sync byte.
        let mut pkt = vec![0x47u8];
        pkt.extend(vec![0u8; 187]);
        s.send_ts(&pkt).unwrap();
        assert!(s.stats().bytes_pushed > 0);
        s.reset_stats();
        let st = s.stats();
        assert_eq!(st.bytes_pushed, 0);
        assert_eq!(st.bytes_skipped_for_sync, 0);
        assert_eq!(st.resync_events, 0);
        assert_eq!(st.packets_sent, 0);
    }

    /// Recording transport that captures every byte sent. Used to assert
    /// what was emitted across the lifecycle boundary.
    struct Recorder(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl Transport for Recorder {
        fn send_bytes(&mut self, b: &[u8]) -> Result<(), TransportError> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(())
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn close(&mut self) {}
        fn is_alive(&self) -> bool {
            true
        }
    }

    #[test]
    fn close_flushes_buffered_partial_packets() {
        // Sender::close must flush the framing buffer
        // before marking the sender closed; otherwise 1-6 partial TS packets
        // that fit inside TsFraming::buffer are silently dropped.
        //
        // Setup: push 3 whole TS packets (564 bytes) which is less than one
        // bundle (7 packets = 1316 bytes), so they remain buffered inside
        // TsFraming until flush or close.
        let bytes_sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = Recorder(bytes_sink.clone());
        let mut sender = Sender::new(recorder, SenderConfig::default());

        let mut input = Vec::new();
        for _ in 0..3 {
            input.push(0x47);
            input.extend(vec![0u8; 187]);
        }
        sender.send_ts(&input).unwrap();
        // Nothing flushed yet (3 < 7-packet bundle).
        assert_eq!(bytes_sink.lock().unwrap().len(), 0);

        sender.close();

        // Pre-fix: 0 bytes captured (partial bundle dropped).
        // Post-fix: 564 bytes captured (flush ran on close).
        assert_eq!(
            bytes_sink.lock().unwrap().len(),
            3 * 188,
            "Sender::close must flush buffered partial TS packets (parity with Drop)"
        );
    }

    /// Transport that fails the first N `send_bytes` calls with Backpressure,
    /// then succeeds. Captures all bytes written on successful calls.
    struct FailFirst {
        remaining_failures: usize,
        sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }
    impl FailFirst {
        fn new(n: usize, sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> Self {
            Self {
                remaining_failures: n,
                sink,
            }
        }
    }
    impl Transport for FailFirst {
        fn send_bytes(&mut self, b: &[u8]) -> Result<(), TransportError> {
            if self.remaining_failures > 0 {
                self.remaining_failures -= 1;
                return Err(TransportError::Backpressure {
                    msg: "test backpressure".into(),
                    errno_code: None,
                });
            }
            self.sink.lock().unwrap().extend_from_slice(b);
            Ok(())
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn close(&mut self) {}
        fn is_alive(&self) -> bool {
            true
        }
    }

    /// Build N synthetic TS packets starting with 0x47.
    fn ts_packets(n: usize) -> Vec<u8> {
        let mut buf = Vec::with_capacity(n * 188);
        for i in 0..n {
            buf.push(0x47);
            for j in 1..188usize {
                buf.push(((i & 0xFF) as u8).wrapping_add(j as u8));
            }
        }
        buf
    }

    #[test]
    fn send_ts_retains_failed_bundle_and_retry_delivers_exactly_once() {
        // 14 TS packets → 2 bundles (7 packets × 1316 bytes each).
        // Transport fails the first send_bytes call (bundle 0), so bundle 0
        // ends up in pending_bundles and bundle 1 is not attempted.
        // On retry (second send_ts call), pending_bundles drains first (bundle
        // 0 goes through), then the new call pushes nothing (empty input).
        // Total output must equal 14 packets = 2 × 1316 bytes, in order.
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let transport = FailFirst::new(1, sink.clone());
        let mut sender = Sender::new(transport, SenderConfig::default());

        // First call: produces 2 bundles; transport fails on the 1st.
        let input = ts_packets(14);
        let err = sender.send_ts(&input).unwrap_err();
        assert_eq!(
            err.kind,
            crate::shell_error::ShellErrorKind::Backpressure,
            "expected Backpressure, got {:?}",
            err
        );
        // Nothing sent yet.
        assert_eq!(sink.lock().unwrap().len(), 0);

        // Second call (empty input to flush pending_bundles only).
        sender.send_ts(&[]).unwrap();
        // Both bundles must have been delivered: 14 × 188 = 2632.
        assert_eq!(
            sink.lock().unwrap().len(),
            14 * 188,
            "retry must deliver all retained bundles exactly once"
        );
    }

    #[test]
    fn flush_retains_failed_bundle_and_retry_flush_delivers_exactly_once() {
        // Push 3 TS packets (< 7 → no bundle emitted). Transport fails the
        // flush call. Retry flush must deliver the bundle without duplication.
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let transport = FailFirst::new(1, sink.clone());
        let mut sender = Sender::new(transport, SenderConfig::default());

        let input = ts_packets(3);
        sender.send_ts(&input).unwrap(); // no bundle emitted yet
        assert_eq!(sink.lock().unwrap().len(), 0);

        // flush() produces 1 partial bundle; transport rejects it.
        let err = sender.flush().unwrap_err();
        assert_eq!(err.kind, crate::shell_error::ShellErrorKind::Backpressure);
        assert_eq!(sink.lock().unwrap().len(), 0);

        // Second flush: drains pending, framing is already empty.
        sender.flush().unwrap();
        assert_eq!(
            sink.lock().unwrap().len(),
            3 * 188,
            "retry flush must deliver the retained bundle exactly once, no duplication"
        );
    }

    #[test]
    fn send_ts_mid_multi_bundle_failure_retains_remaining() {
        // 21 packets → 3 bundles. Transport fails on bundle index 1 (the
        // second bundle). After retry, all 3 bundles must be delivered.
        struct FailAt {
            fail_on: usize,
            calls: usize,
            sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        }
        impl Transport for FailAt {
            fn send_bytes(&mut self, b: &[u8]) -> Result<(), TransportError> {
                let call = self.calls;
                self.calls += 1;
                if call == self.fail_on {
                    return Err(TransportError::Backpressure {
                        msg: "test backpressure".into(),
                        errno_code: None,
                    });
                }
                self.sink.lock().unwrap().extend_from_slice(b);
                Ok(())
            }
            fn max_payload(&self) -> usize {
                1316
            }
            fn close(&mut self) {}
            fn is_alive(&self) -> bool {
                true
            }
        }
        let sink2 = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut sender2 = Sender::new(
            FailAt {
                fail_on: 1,
                calls: 0,
                sink: sink2.clone(),
            },
            SenderConfig::default(),
        );

        let input = ts_packets(21); // 3 bundles
        // First call: bundle 0 succeeds, bundle 1 fails, bundle 2 retained.
        let err = sender2.send_ts(&input).unwrap_err();
        assert_eq!(err.kind, crate::shell_error::ShellErrorKind::Backpressure);
        // Only bundle 0 sent (1316 bytes).
        assert_eq!(
            sink2.lock().unwrap().len(),
            1316,
            "only the first bundle should have been sent before the failure"
        );

        // Retry: drain pending (bundles 1 and 2) with empty input.
        sender2.send_ts(&[]).unwrap();
        // All 3 bundles must be present: 3 × 1316 = 3948.
        assert_eq!(
            sink2.lock().unwrap().len(),
            21 * 188,
            "after retry, all 3 bundles must be present exactly once"
        );
    }

    /// Phase discrimination on SenderError: first failure (this call's
    /// bundles) → Some(true); failure draining retained bundles before new
    /// input → Some(false).
    #[test]
    fn send_ts_reports_input_consumed_per_phase() {
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        // Fail the first TWO send_bytes calls: call 1 fails on its own
        // bundle (consumed=true); call 2 fails draining the retained
        // bundle before touching its input (consumed=false).
        let transport = FailFirst::new(2, sink.clone());
        let mut sender = Sender::new(transport, SenderConfig::default());

        let e1 = sender.send_ts(&ts_packets(7)).unwrap_err();
        assert_eq!(e1.input_consumed, Some(true));

        let e2 = sender.send_ts(&ts_packets(7)).unwrap_err();
        assert_eq!(e2.input_consumed, Some(false));

        // Healed: drain delivers retained bundle 1 + this call's bundle.
        sender.send_ts(&ts_packets(7)).unwrap();
        assert_eq!(sink.lock().unwrap().len() % 188, 0);
    }

    /// The `self.closed` early-return tag site: `send_ts` after `close()`
    /// never touches `bytes`, so it must report `Some(false)`.
    #[test]
    fn send_ts_after_close_reports_input_consumed_false() {
        let mut sender = Sender::new(Mem, SenderConfig::default());
        sender.close();

        let err = sender.send_ts(&ts_packets(7)).unwrap_err();
        assert_eq!(err.input_consumed, Some(false));
    }

    /// The STRICT-mode `push_strict` tag site: a misaligned first byte is
    /// rejected before anything is queued, so it must report `Some(false)`.
    #[test]
    fn strict_mode_misaligned_input_reports_input_consumed_false() {
        let cfg = SenderConfig {
            framing_mode: TsFramingMode::Strict,
            ..Default::default()
        };
        let mut sender = Sender::new(Mem, cfg);

        let mut input = vec![0xAB, 0xCD];
        input.extend(ts_packets(3));
        let err = sender.send_ts(&input).unwrap_err();
        assert_eq!(err.input_consumed, Some(false));
    }
}
