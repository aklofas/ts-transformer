//! `Sender<T: Transport>` — pre-muxed TS bytes → SRT, with framing.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! See `framing.rs` for the sync-acquisition / loss-detection state
//! machine. `Sender` composes `TsFraming` with a `Transport`.

mod framing;

pub use framing::{SenderStats, TsFraming, TsFramingError, TsFramingMode};

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use tracing::info_span;
use tst_core::mpegts::common::TS_PACKET_SIZE;
use tst_core::transport::{Transport, TransportError};

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

use crate::binding::kind::{coarsen, kind_of_transport};
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
/// `Sender` can produce: `InputMalformed` (a framing failure: STRICT-mode
/// misalignment, RECOVER mode's scan limit, bytes [`Sender::finish`] could
/// not send as TS), `Backpressure`, `TransportBroken`, `Closed`.
/// `ConfigInvalid` and
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
    ///   path (e.g. `flush` and `finish`, which have no per-call input).
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
            kind: match &e {
                TsFramingError::SyncLost { .. }
                | TsFramingError::NoSyncAfterLimit { .. }
                | TsFramingError::UnsentTail { .. } => ShellErrorKind::InputMalformed,
            },
            source: SenderErrorSource::Framing(e),
            input_consumed: None,
        }
    }
}

impl From<tst_core::transport::TransportError> for SenderError {
    fn from(e: tst_core::transport::TransportError) -> Self {
        Self {
            kind: coarsen(kind_of_transport(&e, crate::shell_error::Direction::Send)),
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
/// `Sender` supports four shutdown patterns:
///
/// 1. **Drop** — the [`Drop`] impl best-effort flushes any buffered
///    partial bundle, makes the same end-of-stream pass as
///    [`Self::finish`] and closes the underlying transport. It reports
///    nothing: bytes it could not send as TS are dropped, with one
///    `tracing` warning that carries their count. Synchronous:
///    it takes as long as those sends and the transport's own close
///    take; see the transport's docs. A binding object dropped without
///    `close()` (a C handle freed, a JVM or Python object collected)
///    cancels the transport first, so this flush fails fast instead of
///    parking in a `ManagedTransport` reconnect loop against a
///    lost peer; the buffered partial bundle is not sent.
/// 2. **Explicit close** — call [`Self::close`]. Best-effort flushes
///    any buffered partial bundle and ends the stream (same as Drop,
///    as silently), marks the sender
///    closed so subsequent `send_ts` / `flush` calls return
///    [`SenderErrorSource::Transport`]`(`[`tst_core::transport::TransportError::Closed`]`)`,
///    then closes the transport. Idempotent. Called directly in Rust it
///    is equivalent to Drop. A binding's `close()`
///    (`AutoCloseable`/`__exit__`/`.use { }`/`tst_sender_close`) cancels
///    the transport first and then closes, so a partial bundle buffered
///    against a lost peer is dropped rather than replayed through a
///    reconnect.
/// 3. **Graceful finish** — call [`Self::finish`]. Sends the retained
///    bundles, the partial bundle and whatever its end-of-stream pass
///    still verifies to the still-live transport, and reports what it
///    could not send: a transport error, or `InputMalformed` for a
///    partial last packet and for bytes that never confirmed their
///    alignment. Marks the
///    sender closed, then closes the transport, with no cancel first, so a
///    transport that queues (an RTSP push) writes what it holds. A binding's
///    `finish()` calls it without cancelling. A later `close()` or Drop
///    does nothing.
/// 4. **Cross-thread cancel** — call [`Self::cancel_handle`] to obtain a
///    `Send + Sync` [`tst_core::transport::TransportCancel`] handle,
///    then `cancel()` from any thread. Wakes a peer thread parked in
///    `send_ts`; how soon depends on the transport (SRT closes the
///    socket, the others notice at their next poll tick); see
///    [the cancel coverage table](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/srt-cancel-handle.md#cancel-coverage-by-transport).
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
/// | C | `tst_sender_close(sender)` (explicit; mirrors `Drop`); no `finish` twin |
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
    /// Each bundle is split into whole-packet messages of at most
    /// `transport.max_payload()` bytes; a ceiling below one TS packet is
    /// refused before any input is consumed.
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
    /// touched (e.g. draining bundles retained by a PREVIOUS call, or
    /// STRICT mode's [`TsFramingError::SyncLost`], which checks every
    /// packet boundary of the call before taking any of it and leaves the
    /// sender unchanged), so retrying the same input — or a corrected
    /// version of it — is safe. To recover after `Backpressure`,
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
    /// // Three whole TS packets. STRICT mode checks that every packet of
    /// // a call starts with 0x47; an input of one or two packets would
    /// // be sent by `flush` just the same.
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
        self.check_ceiling()
            .map_err(|e| SenderError::from(e).with_input_consumed(false))?;
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
            // push_strict validates the whole call before committing, so a
            // SyncLost leaves `bytes` untouched.
            self.framing
                .push_strict(bytes)
                .map_err(|e| SenderError::from(e).with_input_consumed(false))?
        };
        let mut iter = bundles.into_iter();
        for bundle in &mut iter {
            if let Err(e) = self.send_fitted(bundle) {
                // The unsent remainder is already queued; retain the bundles
                // not yet attempted behind it so the next send_ts/flush call
                // re-tries them in order. From here `bytes` is framed and
                // retained: a failure leaves it in the pending queue,
                // draining exactly once on the next call.
                self.pending_bundles.extend(iter);
                return Err(SenderError::from(e).with_input_consumed(true));
            }
        }
        Ok(())
    }

    /// Emit any buffered partial bundle.
    ///
    /// A stream of one or two TS packets is sent too. In
    /// [`TsFramingMode::Strict`] every packet boundary is checked on push.
    /// In the default [`TsFramingMode::Recover`] a stream that has been
    /// aligned since its first byte (no byte skipped, no boundary failed so
    /// far) is flushed on the same evidence before three packets have
    /// confirmed it: the whole packets held are sent when every boundary
    /// present reads `0x47`, the first byte of a partial packet included.
    /// A flush is therefore the caller's word that what was sent so far is
    /// a stream: bytes that merely begin with `0x47` and are flushed inside
    /// their first 376 bytes go out as one or two packets. Once a byte was
    /// skipped or a boundary failed, `flush` sends nothing more until three
    /// consecutive sync bytes have confirmed the alignment again; what it
    /// then holds back is sent or reported by [`Self::finish`].
    ///
    /// Bundles are sent in whole-packet slices that fit `max_payload()`; a
    /// ceiling below one TS packet returns `TooLarge` and keeps the bundle
    /// queued.
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
        self.send_framed(bundles)
    }

    /// Send the bundles the framer handed out. On a failed send the
    /// bundles not yet attempted are retained behind the failed one.
    fn send_framed(&mut self, bundles: Vec<Vec<u8>>) -> Result<(), SenderError> {
        let mut iter = bundles.into_iter();
        for bundle in &mut iter {
            if let Err(e) = self.send_fitted(bundle) {
                self.pending_bundles.extend(iter);
                return Err(e.into());
            }
        }
        Ok(())
    }

    /// Drain the `pending_bundles` queue. Returns on the first transport
    /// error, leaving the remaining bundles in the queue.
    fn drain_pending(&mut self) -> Result<(), SenderError> {
        while let Some(bundle) = self.pending_bundles.pop_front() {
            self.send_fitted(bundle)?;
        }
        Ok(())
    }

    /// Refuse a transport whose ceiling cannot carry one TS packet; return
    /// the ceiling that was checked.
    fn check_ceiling(&self) -> Result<usize, TransportError> {
        let max = self.transport.max_payload();
        if max < TS_PACKET_SIZE {
            return Err(TransportError::TooLarge {
                len: TS_PACKET_SIZE,
                max,
            });
        }
        Ok(max)
    }

    /// Send `bundle` in whole-packet slices that fit the transport's ceiling.
    /// On a failed slice the unsent remainder, starting at that slice's first
    /// byte, goes back to the front of the pending queue.
    fn send_fitted(&mut self, bundle: Vec<u8>) -> Result<(), TransportError> {
        // One read: a managed transport's ceiling can change between calls,
        // and a second read below one packet would make the step zero.
        let max = match self.check_ceiling() {
            Ok(max) => max,
            Err(e) => {
                self.pending_bundles.push_front(bundle);
                return Err(e);
            }
        };
        let step = max / TS_PACKET_SIZE * TS_PACKET_SIZE;
        let mut sent = 0;
        while sent < bundle.len() {
            let end = bundle.len().min(sent + step);
            if let Err(e) = self.transport.send_bytes(&bundle[sent..end]) {
                self.pending_bundles.push_front(bundle[sent..].to_vec());
                return Err(e);
            }
            sent = end;
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

    /// End the stream, then close: the fallible counterpart to
    /// [`Self::close`]. It sends what [`Self::flush`] sends and then makes
    /// one end-of-stream pass over what the framer still holds: packets
    /// behind damage that a later `send_ts` would have verified (three
    /// consecutive sync bytes) are sent now, because no later call comes.
    /// Nothing is cancelled first, so the sends go to the still-live
    /// transport and the transport's own close runs uncancelled (a queuing
    /// transport writes its queue). Idempotent: `Ok(())` on a sender that
    /// is already closed. Mirrors [`crate::MuxSender::finish`].
    ///
    /// **May block** as long as the sends and the transport's close
    /// take; a watchdog holding [`Self::cancel_handle`] can end it from
    /// another thread, and the parked send's error is returned.
    ///
    /// # Errors
    ///
    /// - The first send error (as [`Self::flush`]); the bundles not yet
    ///   sent are abandoned.
    /// - Otherwise [`TsFramingError::UnsentTail`] (kind `InputMalformed`)
    ///   when bytes are left that could not be sent as whole aligned TS
    ///   packets: a partial last packet (in both framing modes), fewer
    ///   than three packets behind skipped bytes, bytes that are not TS.
    ///   Everything that could be verified was sent first, also behind
    ///   more garbage than [`SenderConfig::max_unsynced_bytes`]: the end
    ///   pass scans all the sender holds. The count is
    ///   what was still held at the end, not everything the stream lost:
    ///   bytes the scan skipped on the way, a good packet in front of
    ///   early damage among them, are in
    ///   [`SenderStats::bytes_skipped_for_sync`] only.
    ///
    /// The sender is marked closed and the transport closed whatever is
    /// returned.
    pub fn finish(&mut self) -> Result<(), SenderError> {
        if self.closed {
            return Ok(());
        }
        self.end()
    }

    /// Ends the stream as [`Self::finish`] does, best-effort and without
    /// a report: a send error is dropped, and bytes that could not be sent
    /// as TS leave one `tracing` warning with their count. Mirrors Drop,
    /// so explicit close == drop for AutoCloseable / __exit__ / .use { } /
    /// tst_sender_close(...) idioms.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.end_quietly();
    }

    /// The end of the stream, shared by `finish`, `close` and drop: one
    /// attempt at everything a flush sends and everything the framer's
    /// end-of-stream pass still verifies, then the sender is closed and
    /// the transport with it. A send error is the result; without one,
    /// bytes the framer could not verify are.
    fn end(&mut self) -> Result<(), SenderError> {
        let sent = self.drain_pending().and_then(|()| {
            let bundles = self.framing.finish();
            self.send_framed(bundles)
        });
        let left = self.framing.buffered_bytes();
        self.closed = true;
        self.transport.close();
        sent?;
        if left > 0 {
            return Err(TsFramingError::UnsentTail { bytes: left as u64 }.into());
        }
        Ok(())
    }

    fn end_quietly(&mut self) {
        if let Err(SenderError {
            source: SenderErrorSource::Framing(TsFramingError::UnsentTail { bytes }),
            ..
        }) = self.end()
        {
            tracing::warn!(
                bytes,
                "Sender closed with bytes it could not send as whole aligned TS packets"
            );
        }
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

/// Drop ends the stream (best-effort: the buffered partial bundle and the
/// end-of-stream pass), then closes the transport. Equivalent to calling
/// [`Sender::close`] explicitly — FFI bindings may use either idiom.
impl<T: Transport> Drop for Sender<T> {
    fn drop(&mut self) {
        if !self.closed {
            self.end_quietly();
        }
        let _enter = self._span.0.enter();
        tracing::info!("Sender closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The log-capture macro and the error-text assertions name these; a
    // test build without `std` has no prelude that brings them.
    use alloc::string::{String, ToString};
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

    /// Build N synthetic TS packets starting with 0x47 (the same bytes as
    /// `framing::tests::synthetic_ts_stream`).
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
    fn strict_send_ts_error_is_unconsumed_and_a_fixed_retry_delivers() {
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let cfg = SenderConfig {
            framing_mode: TsFramingMode::Strict,
            ..SenderConfig::default()
        };
        let mut s = Sender::new(Recorder(sent.clone()), cfg);
        let good7 = ts_packets(7);
        let mut bad = good7.clone();
        bad.extend(ts_packets(7));
        bad[(7 + 3) * 188] = 0x00;
        let e = s.send_ts(&bad).unwrap_err();
        assert_eq!(e.input_consumed, Some(false));
        assert!(
            sent.lock().unwrap().is_empty(),
            "no bundle of a rejected call goes out"
        );
        s.send_ts(&good7).unwrap();
        assert_eq!(*sent.lock().unwrap(), good7);
    }

    #[test]
    fn send_ts_delivers_a_framed_bundle_before_the_watchdog_error() {
        let sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let cfg = SenderConfig {
            max_unsynced_bytes: 18_800,
            ..SenderConfig::default()
        };
        let mut s = Sender::new(Recorder(sent.clone()), cfg);
        let good7 = ts_packets(7);
        let mut input = good7.clone();
        input.extend_from_slice(&[0x00; 20_000]);
        s.send_ts(&input).unwrap();
        assert_eq!(*sent.lock().unwrap(), good7);
        let e = s.send_ts(&[]).unwrap_err();
        assert!(matches!(
            e.source,
            SenderErrorSource::Framing(TsFramingError::NoSyncAfterLimit { max: 18_800 })
        ));
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

    /// A transport that queues what it is given and hands the queue on at
    /// `close()` unless its cancel fired first, the way a push session's
    /// close waits for its media queue. The first `fail_sends` sends are
    /// refused with `Backpressure`; with `broken` every send fails.
    #[derive(Default)]
    struct QueuingState {
        queued: Vec<u8>,
        delivered: Vec<u8>,
        dropped_at_close: usize,
        closes: usize,
        /// Every `send_bytes` call, refused ones included.
        sends: usize,
    }
    struct Queuing {
        state: std::sync::Arc<std::sync::Mutex<QueuingState>>,
        cancel: std::sync::Arc<tst_core::cancel::FlagCancel>,
        fail_sends: usize,
        broken: bool,
    }
    impl Queuing {
        fn new(
            fail_sends: usize,
            broken: bool,
        ) -> (Self, std::sync::Arc<std::sync::Mutex<QueuingState>>) {
            let state = std::sync::Arc::new(std::sync::Mutex::new(QueuingState::default()));
            (
                Self {
                    state: state.clone(),
                    cancel: std::sync::Arc::new(tst_core::cancel::FlagCancel::new()),
                    fail_sends,
                    broken,
                },
                state,
            )
        }
    }
    impl Transport for Queuing {
        fn send_bytes(&mut self, b: &[u8]) -> Result<(), TransportError> {
            self.state.lock().unwrap().sends += 1;
            if self.broken {
                return Err(TransportError::Broken {
                    msg: "test broken".into(),
                    errno_code: None,
                    cause: tst_core::transport::BrokenCause::Unspecified,
                });
            }
            if self.fail_sends > 0 {
                self.fail_sends -= 1;
                return Err(TransportError::Backpressure {
                    msg: "test backpressure".into(),
                    errno_code: None,
                });
            }
            self.state.lock().unwrap().queued.extend_from_slice(b);
            Ok(())
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn close(&mut self) {
            let mut st = self.state.lock().unwrap();
            st.closes += 1;
            let q = core::mem::take(&mut st.queued);
            if self.cancel.is_cancelled() {
                st.dropped_at_close += q.len();
            } else {
                st.delivered.extend_from_slice(&q);
            }
        }
        fn is_alive(&self) -> bool {
            self.state.lock().unwrap().closes == 0
        }
        fn cancel_handle(
            &self,
        ) -> Option<std::sync::Arc<dyn tst_core::transport::TransportCancel + Send + Sync>>
        {
            Some(self.cancel.clone())
        }
    }

    fn unsent_tail(e: &SenderError) -> Option<u64> {
        match e.source {
            SenderErrorSource::Framing(TsFramingError::UnsentTail { bytes }) => Some(bytes),
            _ => None,
        }
    }

    /// A sender over a queuing transport that accepts everything, after
    /// `input` went in as one `send_ts`.
    fn sender_with(
        input: &[u8],
        mode: TsFramingMode,
    ) -> (
        Sender<Queuing>,
        std::sync::Arc<std::sync::Mutex<QueuingState>>,
    ) {
        let (t, state) = Queuing::new(0, false);
        let cfg = SenderConfig {
            framing_mode: mode,
            ..SenderConfig::default()
        };
        let mut s = Sender::new(t, cfg);
        s.send_ts(input).unwrap();
        (s, state)
    }

    /// `input` with 100 bytes of garbage in front.
    fn behind_garbage(input: &[u8]) -> Vec<u8> {
        let mut v = vec![0x00; 100];
        v.extend_from_slice(input);
        v
    }

    /// Seven packets (one bundle), 100 bytes of garbage, three packets.
    fn damaged_ten() -> (Vec<u8>, Vec<u8>) {
        let all = ts_packets(10);
        let mut input = all[..7 * 188].to_vec();
        input.extend_from_slice(&[0x00; 100]);
        input.extend_from_slice(&all[7 * 188..]);
        (input, all)
    }

    #[test]
    fn finish_sends_a_one_packet_stream() {
        for n in [1, 2] {
            let input = ts_packets(n);
            let (mut s, state) = sender_with(&input, TsFramingMode::Recover);
            assert!(state.lock().unwrap().queued.is_empty());
            s.finish().expect("a clean short stream ends without error");
            let st = state.lock().unwrap();
            assert_eq!(st.delivered, input, "{n} packet(s)");
            assert_eq!(st.closes, 1);
        }
    }

    #[test]
    fn close_alone_sends_a_two_packet_stream() {
        for n in [1, 2] {
            let input = ts_packets(n);
            let (mut s, state) = sender_with(&input, TsFramingMode::Recover);
            s.close();
            let st = state.lock().unwrap();
            assert_eq!(st.delivered, input, "{n} packet(s)");
            assert_eq!(st.closes, 1);
        }
    }

    #[test]
    fn drop_alone_sends_a_one_packet_stream() {
        for n in [1, 2] {
            let input = ts_packets(n);
            let (s, state) = sender_with(&input, TsFramingMode::Recover);
            drop(s);
            let st = state.lock().unwrap();
            assert_eq!(st.delivered, input, "{n} packet(s)");
            assert_eq!(st.closes, 1);
        }
    }

    #[test]
    fn flush_sends_a_short_stream_at_once() {
        let input = ts_packets(1);
        let (mut s, state) = sender_with(&input, TsFramingMode::Recover);
        s.flush().unwrap();
        assert_eq!(state.lock().unwrap().queued, input);
        assert_eq!(s.stats().packets_sent, 1);
    }

    #[test]
    fn finish_reports_packets_it_could_not_verify() {
        let mut damaged_third = ts_packets(3);
        damaged_third[376] = 0x00;
        let mut non_ts = vec![0x11; 300];
        non_ts[0] = 0x47;
        let mut stray_byte = ts_packets(1);
        stray_byte.push(0x00);
        for (input, left) in [
            (behind_garbage(&ts_packets(1)), 188),
            (behind_garbage(&ts_packets(2)), 376),
            (damaged_third, 376),
            (non_ts, 300),
            (stray_byte, 189),
        ] {
            let (mut s, state) = sender_with(&input, TsFramingMode::Recover);
            let e = s.finish().unwrap_err();
            assert_eq!(e.kind, ShellErrorKind::InputMalformed, "{e:?}");
            assert_eq!(unsent_tail(&e), Some(left), "{e:?}");
            assert_eq!(e.input_consumed, None);
            let st = state.lock().unwrap();
            assert!(st.delivered.is_empty(), "{left} bytes left: none is sent");
            assert_eq!(st.closes, 1);
            drop(st);
            assert!(!s.is_alive());
        }
    }

    #[test]
    fn finish_reports_a_partial_last_packet_in_both_modes() {
        let all = ts_packets(4);
        for mode in [TsFramingMode::Recover, TsFramingMode::Strict] {
            for n in 1..=3 {
                let (mut s, state) = sender_with(&all[..n * 188 + 100], mode);
                let e = s.finish().unwrap_err();
                assert_eq!(e.kind, ShellErrorKind::InputMalformed, "{mode:?} {n}");
                assert_eq!(unsent_tail(&e), Some(100), "{mode:?} {n}: {e:?}");
                let st = state.lock().unwrap();
                assert_eq!(st.delivered, all[..n * 188], "{mode:?} {n}");
                assert_eq!(st.closes, 1);
            }
        }
    }

    #[test]
    fn finish_recovers_the_packets_behind_damage() {
        let (input, all) = damaged_ten();
        let (mut s, state) = sender_with(&input, TsFramingMode::Recover);
        assert_eq!(state.lock().unwrap().queued, all[..7 * 188]);
        s.finish().expect("three packets behind the damage verify");
        let st = state.lock().unwrap();
        assert_eq!(st.delivered, all);
        assert_eq!(st.closes, 1);
        drop(st);
        assert_eq!(s.stats().bytes_skipped_for_sync, 100);
        assert_eq!(s.stats().packets_sent, 10);
    }

    #[test]
    fn close_recovers_the_packets_behind_damage_silently() {
        let (input, all) = damaged_ten();
        let (mut s, state) = sender_with(&input, TsFramingMode::Recover);
        s.close();
        assert_eq!(state.lock().unwrap().delivered, all);
        assert_eq!(state.lock().unwrap().closes, 1);

        let (s, state) = sender_with(&input, TsFramingMode::Recover);
        drop(s);
        assert_eq!(state.lock().unwrap().delivered, all);
        assert_eq!(state.lock().unwrap().closes, 1);
    }

    #[test]
    fn a_good_packet_before_early_damage_is_skipped_not_reported() {
        // One packet, garbage, three packets: the scan never verified the
        // first one, so it is skipped and counted like the garbage.
        let all = ts_packets(4);
        let mut input = all[..188].to_vec();
        input.extend_from_slice(&[0x00; 100]);
        input.extend_from_slice(&all[188..]);
        let (mut s, state) = sender_with(&input, TsFramingMode::Recover);
        s.finish().expect("nothing is left buffered");
        assert_eq!(state.lock().unwrap().delivered, all[188..]);
        assert_eq!(s.stats().bytes_skipped_for_sync, 288);
    }

    #[test]
    fn finish_recovers_packets_behind_more_garbage_than_the_scan_limit() {
        // A bundle, then garbage with a false candidate past a scan limit
        // of 50, then five good packets: `send_ts` framed a bundle, so the
        // limit's error waits for a next call. `finish` scans on and sends
        // the five.
        let all = ts_packets(12);
        let mut input = all[..7 * 188].to_vec();
        input.extend_from_slice(&[0x00; 100]);
        input.push(0x47);
        input.extend_from_slice(&[0x00; 399]);
        input.extend_from_slice(&all[7 * 188..]);
        let (t, state) = Queuing::new(0, false);
        let cfg = SenderConfig {
            max_unsynced_bytes: 50,
            ..SenderConfig::default()
        };
        let mut s = Sender::new(t, cfg);
        s.send_ts(&input).unwrap();
        s.finish().expect("five packets behind the garbage verify");
        let st = state.lock().unwrap();
        assert_eq!(st.delivered, all);
        assert_eq!(st.closes, 1);
    }

    #[test]
    fn finish_under_backpressure_reports_the_backpressure() {
        // The end pass's bundle is refused, and a partial packet is left
        // as well: the refusal is the report.
        let mut input = behind_garbage(&ts_packets(4));
        input.truncate(100 + 3 * 188 + 100);
        let (t, state) = Queuing::new(1, false);
        let mut s = Sender::new(t, SenderConfig::default());
        s.send_ts(&input).unwrap();
        let e = s.finish().unwrap_err();
        assert_eq!(e.kind, ShellErrorKind::Backpressure, "{e:?}");
        assert!(
            matches!(
                e.source,
                SenderErrorSource::Transport(TransportError::Backpressure { .. })
            ),
            "{e:?}"
        );
        {
            let st = state.lock().unwrap();
            assert!(st.delivered.is_empty());
            assert_eq!((st.sends, st.closes), (1, 1));
        }
        assert!(s.finish().is_ok(), "a second finish is a no-op");
        assert_eq!(state.lock().unwrap().sends, 1, "and sends nothing");

        // A bundle retained from an earlier refusal is refused again: the
        // packets still in the framer are not reported as unsendable.
        let (t, state) = Queuing::new(2, false);
        let mut s = Sender::new(t, SenderConfig::default());
        s.send_ts(&ts_packets(9)).unwrap_err();
        let e = s.finish().unwrap_err();
        assert_eq!(e.kind, ShellErrorKind::Backpressure, "{e:?}");
        assert_eq!(unsent_tail(&e), None);
        assert_eq!(state.lock().unwrap().closes, 1);
    }

    #[test]
    fn a_second_finish_reports_nothing() {
        let (mut s, state) = sender_with(&ts_packets(1)[..100], TsFramingMode::Recover);
        assert_eq!(unsent_tail(&s.finish().unwrap_err()), Some(100));
        assert!(s.finish().is_ok(), "the report is made once");
        s.close();
        drop(s);
        let st = state.lock().unwrap();
        assert_eq!(st.closes, 1, "neither finish, close nor drop closes again");
        assert_eq!(st.sends, 0);
    }

    #[test]
    fn drop_runs_the_end_pass_once_and_never_retries() {
        // Two packets wait in the framer; the transport refuses every send.
        let (t, state) = Queuing::new(usize::MAX, false);
        let mut s = Sender::new(t, SenderConfig::default());
        s.send_ts(&ts_packets(2)).unwrap();
        assert_eq!(state.lock().unwrap().sends, 0);
        drop(s);
        {
            let st = state.lock().unwrap();
            assert_eq!(st.sends, 1, "one attempt, no retry");
            assert_eq!(st.closes, 1);
        }

        // A retained bundle is refused again: the pass stops there, with
        // the framer's packets never attempted.
        let (t, state) = Queuing::new(usize::MAX, false);
        let mut s = Sender::new(t, SenderConfig::default());
        s.send_ts(&ts_packets(9)).unwrap_err();
        assert_eq!(state.lock().unwrap().sends, 1);
        s.close();
        let st = state.lock().unwrap();
        assert_eq!(st.sends, 2, "one attempt, no retry");
        assert_eq!(st.closes, 1);
    }

    #[test]
    #[tracing_test::traced_test]
    fn the_unsent_tail_error_text_carries_no_payload_bytes() {
        let mut input = ts_packets(1);
        input.push(0x47);
        input.extend_from_slice(b"SECRET-PAYLOAD-SECRET-PAYLOAD");
        let tail = (input.len() - 188) as u64;

        let (mut s, _state) = sender_with(&input, TsFramingMode::Recover);
        let e = s.finish().unwrap_err();
        assert_eq!(
            e.to_string(),
            format!(
                "Sender error (InputMalformed): {tail} byte(s) at the end of the stream \
                 were not sent: not whole aligned TS packets"
            )
        );
        assert_eq!(
            format!("{:?}", e.source),
            format!("Framing(UnsentTail {{ bytes: {tail} }})")
        );
        assert!(
            !logs_contain("were not sent"),
            "finish reports, it does not log"
        );

        // close() and drop say it in the log instead, with the count only.
        let (mut s, _state) = sender_with(&input, TsFramingMode::Recover);
        s.close();
        assert!(logs_contain(&format!("bytes={tail}")));
        assert!(!logs_contain("SECRET"));
    }

    #[test]
    fn finish_delivers_every_retained_bundle_then_closes() {
        let (t, state) = Queuing::new(1, false);
        let mut s = Sender::new(t, SenderConfig::default());
        let input = ts_packets(9);
        // The first bundle (7 packets) is refused and retained; 2 packets
        // stay in the framing buffer.
        let e = s.send_ts(&input).unwrap_err();
        assert_eq!(e.input_consumed, Some(true));
        assert!(state.lock().unwrap().queued.is_empty());

        s.finish().expect("finish drains and closes");
        let st = state.lock().unwrap();
        assert_eq!(st.delivered, input, "every byte, in order");
        assert_eq!(st.dropped_at_close, 0);
        assert_eq!(st.closes, 1);
        drop(st);
        assert!(!s.is_alive());
    }

    #[test]
    fn finish_reports_a_flush_error_and_still_closes() {
        let (t, state) = Queuing::new(0, true);
        let mut s = Sender::new(t, SenderConfig::default());
        s.send_ts(&ts_packets(3)).unwrap();
        let e = s.finish().unwrap_err();
        assert!(
            matches!(
                e.source,
                SenderErrorSource::Transport(TransportError::Broken { .. })
            ),
            "got {e:?}"
        );
        assert_eq!(state.lock().unwrap().closes, 1, "closed despite the error");
        assert!(!s.is_alive());
        assert!(s.finish().is_ok(), "a second finish is a no-op");
        assert_eq!(state.lock().unwrap().closes, 1);
    }

    #[test]
    fn close_after_finish_does_nothing() {
        let (t, state) = Queuing::new(0, false);
        let mut s = Sender::new(t, SenderConfig::default());
        s.send_ts(&ts_packets(3)).unwrap();
        s.finish().unwrap();
        s.close();
        drop(s);
        let st = state.lock().unwrap();
        assert_eq!(st.closes, 1, "neither close nor drop closes again");
        assert_eq!(st.delivered, ts_packets(3));
    }

    #[test]
    fn send_after_finish_is_refused() {
        let (t, state) = Queuing::new(0, false);
        let mut s = Sender::new(t, SenderConfig::default());
        s.finish().unwrap();
        let e = s.send_ts(&ts_packets(1)).unwrap_err();
        assert!(
            matches!(
                e.source,
                SenderErrorSource::Transport(TransportError::Closed)
            ),
            "got {e:?}"
        );
        assert_eq!(e.kind, ShellErrorKind::Closed);
        assert_eq!(e.input_consumed, Some(false));
        assert!(matches!(
            s.flush().unwrap_err().source,
            SenderErrorSource::Transport(TransportError::Closed)
        ));
        assert!(state.lock().unwrap().delivered.is_empty());
    }
}
