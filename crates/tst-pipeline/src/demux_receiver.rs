//! `DemuxReceiver<R>` — full receive: RecvTransport → Receiver → Demuxer.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Mirrors the `MuxSender` shape on the receive side. Includes
//! `add_byte_sink` for fan-out: callbacks see every 188-byte TS packet
//! pulled from the transport, in registration order, before the demuxer
//! parses them. Useful for "write to disk + forward via RTP + demux for
//! KLV" workflows where multiple consumers tee off the same byte stream.
//!
//! # Byte-sink contract
//!
//! - Sinks are called once per TS packet (188 bytes), in the order they were
//!   registered via [`DemuxReceiver::add_byte_sink`].
//! - The slice passed to the sink is valid only for the duration of the call.
//!   Copy bytes into an owned buffer if they need to outlive the callback.
//! - Sinks must not panic. A panicking sink will unwind through `recv_event`.
//!
//! # Stream-end flush
//!
//! `DemuxReceiver` calls `Demuxer::flush` before surfacing *any* terminal
//! condition that ends the receive loop — not just a clean
//! `TransportError::Closed` (peer-EOS, → `Ok(None)`), but also
//! `ShellErrorKind::TransportBroken` (socket died) and
//! `ShellErrorKind::Closed` (caller-initiated `close()`/`cancel()` from
//! another thread while `recv_event` was blocked). This surfaces any
//! partial PES sitting in reassembly state (typically the final video AU,
//! whose PES packet length is 0 — length-unknown — and is only flushed when
//! the next PES starts or the stream ends). All three paths funnel through
//! the same flush call because `Demuxer::flush` is PID-agnostic: it drains
//! whatever partial PES state exists across every stream (video, audio,
//! KLV), not just video.
//!
//! `Backpressure` is deliberately excluded — it means "try the same recv
//! again", not "the loop is over", so flushing there would prematurely
//! finalize an access unit that's still being received.
//!
//! In normal live streams the flush emits nothing (no partial PES pending);
//! for finite test data, or a session that ends mid-transmission, it
//! recovers whatever was buffered. On a genuinely mid-stream break this can
//! mean the flushed sample is a **truncated** access unit rather than a
//! complete one — the same trade-off the clean-EOF path already accepted
//! (a sender that stops mid-PES still looks like ordinary EOF to some
//! transports). This is not a new risk: `DemuxEvent::Sample`/`Metadata`
//! payloads from any receive path are never guaranteed complete against the
//! original encoder-side AU boundaries when the wire data itself is
//! truncated — callers already need to tolerate arbitrary payload content
//! from hostile or interrupted wire data.

use crate::receiver::{Receiver, ReceiverConfig, ReceiverErrorSource};
use crate::shell_error::ShellErrorKind;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
#[cfg(feature = "std")]
use core::sync::atomic::{AtomicU64, Ordering};
use tracing::info_span;
use tst_core::error::DemuxError;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, DemuxerConfig};
use tst_core::transport::RecvTransport;
use tst_core::transport::TransportError;

/// Type alias for a boxed byte-fanout callback registered via
/// [`DemuxReceiver::add_byte_sink`]. The callback receives one TS packet (188
/// bytes) per call.
pub type ByteSink = Box<dyn FnMut(&[u8]) + Send>;

/// Reconnect-epoch observer installed by `ManagedDemuxReceiver` (std only:
/// the counter it watches is `ManagedRecvTransport`'s `Arc<AtomicU64>`,
/// and the bare-metal targets have no 64-bit atomics). `None` on the
/// plain shell — `recv_event` then never resets or emits a
/// `ReconnectDiscontinuity`.
#[cfg(feature = "std")]
struct ReconnectEpoch {
    /// The transport's successful-rebuild counter.
    counter: Arc<AtomicU64>,
    /// Counter snapshot from the last loop iteration; a rise means a
    /// reconnect fired since the previous `recv_event`.
    last: u64,
    /// Set when a reconnect was detected mid-loop; consumed by the next
    /// loop turn to yield `DemuxEvent::ReconnectDiscontinuity` before any
    /// post-reconnect event. A `bool` rather than a queued event because
    /// `Demuxer::reset_sync` clears the demuxer's queue and the marker
    /// must survive that clear.
    pending: bool,
}

/// Full receive shell: `RecvTransport → Receiver → Demuxer`, with optional
/// byte-sink fan-out.
///
/// `R` is any [`RecvTransport`] — typically an SRT-specific transport for live
/// connections, or a test mock (e.g. `CannedTransport` in the integration
/// tests).
///
/// # Usage
///
/// ```ignore
/// let mut rx = DemuxReceiver::new(transport);
/// rx.add_byte_sink(Box::new(|pkt| { /* write pkt to disk, etc. */ }));
/// for result in &mut rx {
///     match result.unwrap() {
///         DemuxEvent::ProgramMap(pmt) => { /* inspect PMT */ }
///         DemuxEvent::Sample { stream, payload, .. } => { /* forward AU */ }
///         _ => {}
///     }
/// }
/// ```
///
/// # Closing
///
/// `DemuxReceiver` supports three shutdown patterns:
///
/// 1. **Drop** — the [`Drop`] impl emits a tracing event and lets the
///    inner [`Receiver`] / transport `Drop` chain run, which closes the
///    libsrt socket. Synchronous; bounded by `SRTO_LINGER` (libsrt
///    default 30 s, configurable via `SocketBuilder::linger`).
/// 2. **Explicit close** — call [`Self::close`]. Closes the underlying
///    recv transport; the next `recv_event` flushes the demuxer and
///    returns `Ok(None)` once the queue drains. Idempotent.
/// 3. **Cross-thread cancel** — call [`Self::cancel_handle`] to obtain a
///    `Send + Sync` [`tst_core::transport::TransportCancel`] handle,
///    then `cancel()` from any thread. Wakes a peer thread parked in
///    `recv_event`'s underlying `next_packet` call within one libsrt
///    I/O cycle (~3-10 ms).
///
/// C ABI for the receiver surface (`tst_demux_receiver_open` /
/// `_recv_event` / `_close` / `_cancel` / `_get_stats` and the typed
/// event arena) shipped via the receiver-surface plans (raw byte recv
/// → TS-aligned recv → typed demux events). See
/// `bindings/c/include/tstrans.h` for the C surface.
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
/// | C | Call `tst_demux_receiver_close(p)` (idempotent NULL-safe); or `tst_demux_receiver_cancel(p)` from another thread to wake a blocked `tst_demux_receiver_recv_event` |
///
/// See [`docs/reference/srt-cancel-handle.md`](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/srt-cancel-handle.md) for the full cancel-handle pattern.
pub struct DemuxReceiver<R: RecvTransport> {
    ts: Receiver<R>,
    demux: Demuxer,
    byte_sinks: Vec<ByteSink>,
    /// Set when a terminal transport error (`TransportBroken` / `Closed`)
    /// is observed but `Demuxer::flush` produced queued events first — the
    /// events drain through `recv_event`'s normal fast path on subsequent
    /// calls, and this error is returned once that queue is empty. See
    /// the "Stream-end flush" module docs.
    terminal_error: Option<DemuxReceiverError>,
    /// See [`ReconnectEpoch`]; installed via `set_reconnect_epoch`.
    #[cfg(feature = "std")]
    reconnect_epoch: Option<ReconnectEpoch>,
    /// Lifetime span — see [`crate::shell_error::ShellSpan`] for the
    /// unwind-safe rationale. Private; never exposed publicly.
    _span: crate::shell_error::ShellSpan,
}

impl<R: RecvTransport> core::fmt::Debug for DemuxReceiver<R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DemuxReceiver")
            .field("is_alive", &self.is_alive())
            .field("byte_sinks", &self.byte_sinks.len())
            .field("transport_kind", &core::any::type_name::<R>())
            .finish()
    }
}

impl<R: RecvTransport> DemuxReceiver<R> {
    /// Wrap a transport with default demuxer options (lenient mode).
    pub fn new(transport: R) -> Self {
        Self::with_demux_options(transport, DemuxerConfig::default())
    }

    /// Wrap a transport with custom demuxer options (e.g. strict mode).
    pub fn with_demux_options(transport: R, options: DemuxerConfig) -> Self {
        let span = info_span!(
            target: "tst_pipeline::demux_receiver",
            "demux_receiver",
            transport_kind = core::any::type_name::<R>(),
        );
        let _enter = span.enter();
        tracing::info!("DemuxReceiver opened");
        drop(_enter);
        Self {
            ts: Receiver::new(transport, ReceiverConfig::default()),
            demux: Demuxer::with_config(options),
            byte_sinks: Vec::new(),
            terminal_error: None,
            #[cfg(feature = "std")]
            reconnect_epoch: None,
            _span: core::panic::AssertUnwindSafe(span),
        }
    }

    /// Register a byte-fanout sink.
    ///
    /// The sink is called once per 188-byte TS packet, in registration order,
    /// before the demuxer processes the packet. Multiple sinks can be
    /// registered; each sees the same bytes.
    pub fn add_byte_sink(&mut self, sink: ByteSink) {
        self.byte_sinks.push(sink);
    }

    /// Drop the syncer's buffered bytes and the demuxer's PSI/PES/CC state
    /// — the reconnect boundary reset `ManagedDemuxReceiver` performs.
    /// Available on every build; the no_std receiver shells never call it.
    pub(crate) fn reset_for_reconnect(&mut self) {
        self.ts.reset_sync();
        self.demux.reset_sync();
    }

    /// Watch a transport's reconnect counter: when it rises between
    /// packets, the just-read packet is discarded, the sync/demux state is
    /// reset via [`Self::reset_for_reconnect`], and the next `recv_event`
    /// yields `DemuxEvent::ReconnectDiscontinuity` first. Installed by
    /// `ManagedDemuxReceiver` at construction; the snapshot starts at 0
    /// (the counter is 0 for a freshly built managed transport).
    #[cfg(feature = "std")]
    pub(crate) fn set_reconnect_epoch(&mut self, reconnects: Arc<AtomicU64>) {
        self.reconnect_epoch = Some(ReconnectEpoch {
            counter: reconnects,
            last: 0,
            pending: false,
        });
    }

    /// Loop-top half of the epoch observer: hand out the pending
    /// discontinuity marker exactly once. Always `false` without an
    /// observer (and on no_std builds, which have none).
    #[cfg(feature = "std")]
    fn take_pending_reconnect(&mut self) -> bool {
        match self.reconnect_epoch.as_mut() {
            Some(ep) if ep.pending => {
                ep.pending = false;
                true
            }
            _ => false,
        }
    }

    #[cfg(not(feature = "std"))]
    fn take_pending_reconnect(&mut self) -> bool {
        false
    }

    /// Post-`next_packet` half of the epoch observer: `true` when the
    /// counter rose since the last turn (the marker is then armed and the
    /// caller resets state and drops the packet it just read).
    #[cfg(feature = "std")]
    fn observe_reconnect(&mut self) -> bool {
        let Some(ep) = self.reconnect_epoch.as_mut() else {
            return false;
        };
        let current = ep.counter.load(Ordering::Acquire);
        if current > ep.last {
            ep.last = current;
            ep.pending = true;
            true
        } else {
            false
        }
    }

    #[cfg(not(feature = "std"))]
    fn observe_reconnect(&mut self) -> bool {
        false
    }

    /// Pull one [`DemuxEvent`].
    ///
    /// Blocks until either:
    /// - An event is available in the demuxer's internal queue → `Ok(Some(e))`.
    /// - The transport closes cleanly → flushes the demuxer and returns
    ///   `Ok(None)` once the queue is drained.
    /// - The transport fails terminally (broken socket, or a cross-thread
    ///   `close()`/`cancel()`) → flushes the demuxer (same as the clean-close
    ///   path — see the "Stream-end flush" module docs) and drains any
    ///   resulting events as `Ok(Some(e))` before returning `Err(e)` with
    ///   `e.kind` of `TransportBroken` or `Closed` — inspect `e.source` for
    ///   the inner [`DemuxReceiverErrorSource::Transport`] variant.
    /// - The demuxer rejects a packet in strict mode → `Err(e)` with `e.kind`
    ///   of `InputMalformed` — inspect `e.source` for the inner
    ///   [`DemuxReceiverErrorSource::Demux`] variant.
    ///
    /// # MalformedPes note
    ///
    /// In lenient mode (default, `StrictMode::Off`), a `MalformedPes` from the
    /// inner demuxer is converted to a `NonConformant` event
    /// (`NonConformantIssue::MalformedPes { pid, reason }`) and the receive
    /// loop continues — a single corrupt PES on one PID no longer tears down
    /// the receiver. In strict modes that reject `MalformedPes` (today
    /// `StrictMode::Full`), the error propagates with `kind` `InputMalformed`
    /// and terminates the loop.
    ///
    /// # C ABI
    ///
    /// **Single-consumer contract (C ABI):** `tst_event_t` pointer fields
    /// returned by `tst_demux_receiver_recv_event` are valid only until the
    /// next `tst_demux_receiver_recv_event` (or `tst_demux_receiver_close`)
    /// call on the same handle **from any thread**. Concurrent pulls on one
    /// handle silently invalidate the first caller's borrowed pointers; use
    /// one consumer thread per handle. Rust callers are unaffected — this
    /// method returns an owned [`DemuxEvent`] and `&mut self` already
    /// serializes access.
    ///
    /// # Errors
    ///
    /// Returns [`DemuxReceiverError`] with `kind` one of:
    /// - [`ShellErrorKind::InputMalformed`] — demuxer rejected a packet
    ///   (strict-mode violation, unrecoverable malformation, or malformed PES).
    /// - [`ShellErrorKind::TransportBroken`] — transport socket is broken.
    ///   The demuxer is flushed first (see "Stream-end flush" module docs);
    ///   any events that produced drain via `Ok(Some(e))` on this and
    ///   subsequent calls before this error is finally returned.
    /// - [`ShellErrorKind::Closed`] — `close()` was invoked from another
    ///   thread while this call was blocked, or the cancel signal fired
    ///   (`ExplicitClose` path). Flushed the same way as `TransportBroken`
    ///   above. Same-thread `close()` then `recv_event()` instead routes
    ///   through `TransportError::Closed` → `EndOfStream` and drains
    ///   buffered events and returns `Ok(None)`.
    /// - [`ShellErrorKind::EndOfStream`] — peer closed the connection cleanly
    ///   (`TransportError::Closed`), but only surfaced here if a partial PES
    ///   flush fails; the normal EOF path returns `Ok(None)`.
    ///
    /// # Example
    /// ```
    /// use std::collections::VecDeque;
    /// use tst_pipeline::DemuxReceiver;
    /// use tst_core::transport::{RecvTransport, TransportError};
    ///
    /// // In-memory source; real callers plug in `tst_srt::SrtTransport`.
    /// // Returns Closed once the queue is drained — that's the EOF signal
    /// // DemuxReceiver converts to `Ok(None)` after flushing the demuxer.
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
    /// // Empty source → first `recv_event` flushes (no partial PES) and
    /// // returns `Ok(None)`. The Iterator impl makes this idiomatic:
    /// // `for ev in &mut rx { match ev? { ... } }` exits on EOF.
    /// let mut rx = DemuxReceiver::new(Source(VecDeque::new()));
    /// assert!(rx.recv_event()?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    pub fn recv_event(&mut self) -> Result<Option<DemuxEvent>, DemuxReceiverError> {
        loop {
            // Managed shells only: a reconnect detected on the previous turn
            // is surfaced BEFORE the demuxer's queue — after the reset that
            // queue is empty, but ordering the boundary marker first is
            // what guarantees no post-reconnect event precedes it.
            if self.take_pending_reconnect() {
                return Ok(Some(DemuxEvent::ReconnectDiscontinuity));
            }
            // Fast path: demuxer already has a queued event.
            if let Some(e) = self.demux.next_event() {
                return Ok(Some(e));
            }
            // A previous call hit a terminal transport error and flushed the
            // demuxer; the events that produced have now drained through the
            // fast path above. Surface the error that ended the loop.
            if let Some(err) = self.terminal_error.take() {
                return Err(err);
            }
            // Pull the next aligned 188-byte TS packet.
            let pkt = match self.ts.next_packet() {
                Ok(p) => p,
                Err(e) if e.kind == crate::shell_error::ShellErrorKind::EndOfStream => {
                    // Stream end: flush any partial PES sitting in reassembly
                    // (e.g. the final video AU whose PES length field is 0).
                    self.demux.flush();
                    // Drain any events the flush produced before signaling EOF.
                    if let Some(ev) = self.demux.next_event() {
                        return Ok(Some(ev));
                    }
                    return Ok(None);
                }
                Err(e)
                    if matches!(
                        e.kind,
                        crate::shell_error::ShellErrorKind::TransportBroken
                            | crate::shell_error::ShellErrorKind::Closed
                    ) =>
                {
                    // Terminal, but not a clean peer EOS: the socket died, or
                    // a caller closed/cancelled from another thread while
                    // this call was blocked. Either way no further
                    // `next_packet` call will make progress, so flush the
                    // same partial-PES state the EndOfStream path recovers —
                    // see the "Stream-end flush" module docs — before
                    // surfacing the error. `Backpressure` deliberately does
                    // NOT hit this arm: it means "retry", not "the loop is
                    // over", and flushing there would prematurely finalize
                    // an access unit that's still being received.
                    self.demux.flush();
                    let ReceiverErrorSource::Transport(te) = e.source;
                    let err = DemuxReceiverError::from(te);
                    if let Some(ev) = self.demux.next_event() {
                        self.terminal_error = Some(err);
                        return Ok(Some(ev));
                    }
                    return Err(err);
                }
                Err(e) => {
                    // Re-classify via DemuxReceiverError's From<TransportError>
                    // impl so kind routing is applied.
                    let ReceiverErrorSource::Transport(te) = e.source;
                    return Err(te.into());
                }
            };
            // Managed shells only: the reconnect check sits AFTER
            // `next_packet` because a reconnect can fire DURING a single
            // `next_packet` call (several `recv_bytes` over one packet
            // boundary). If the counter rose, this packet may be spliced
            // from dead-tail bytes plus fresh ones — drop it, reset the
            // sync/demux state, and let the syncer re-lock on the next
            // fresh bytes (the documented data-loss budget on reconnect).
            // Byte sinks do not see the dropped packet: it was never
            // parsed, and its bytes are not trustworthy.
            if self.observe_reconnect() {
                self.reset_for_reconnect();
                continue;
            }
            // Fan-out to byte sinks in registration order before demuxing.
            for sink in &mut self.byte_sinks {
                sink(&pkt);
            }
            // Feed to demuxer via the aligned fast path — the Receiver
            // transport layer already produces [u8; 188] packets so no sync
            // buffering or 0x47 hunt is needed.  In lenient mode this only
            // errors on Unrecoverable (caller violated alignment contract) or
            // MalformedPsi (MalformedPes is converted to a NonConformant
            // event by the inner demuxer). In strict mode it can also return
            // StrictRejection.
            self.demux
                .feed_aligned(&pkt)
                .map_err(DemuxReceiverError::from)?;
        }
    }

    /// Advisory liveness check. Delegates to the underlying transport.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.ts.is_alive()
    }

    /// Close the underlying transport. Idempotent.
    ///
    /// After close, the next `recv_event` call will flush the demuxer and
    /// return `Ok(None)` once the event queue is drained.
    pub fn close(&mut self) {
        self.ts.close();
    }

    /// Snapshot of the underlying recv-transport's cancel handle. Wakes
    /// a thread parked in `recv_event()`'s `next_packet()` call.
    pub fn cancel_handle(
        &self,
    ) -> Option<Arc<dyn tst_core::transport::TransportCancel + Send + Sync>> {
        self.ts.cancel_handle()
    }
}

/// `DemuxReceiver` implements `Iterator` so callers can use `for result in &mut rx`
/// or `.collect()` patterns. EOF (`Ok(None)`) terminates the iterator.
/// Errors are surfaced as `Some(Err(e))` so the caller can distinguish a
/// transport error from a clean end of stream.
impl<R: RecvTransport> Iterator for DemuxReceiver<R> {
    type Item = Result<DemuxEvent, DemuxReceiverError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.recv_event() {
            Ok(Some(e)) => Some(Ok(e)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

impl<R: RecvTransport> Drop for DemuxReceiver<R> {
    fn drop(&mut self) {
        let _enter = self._span.0.enter();
        tracing::info!("DemuxReceiver closed");
    }
}

/// Type alias for [`DemuxReceiver`] with a boxed [`RecvTransport`] trait object.
///
/// See [`BoxedMuxSender`](crate::mux_sender::BoxedMuxSender) for rationale.
///
/// # Example
/// ```no_run
/// use tst_pipeline::demux_receiver::BoxedDemuxReceiver;
/// use tst_pipeline::DemuxReceiver;
/// use tst_core::RecvTransport;
///
/// fn open(transport: Box<dyn RecvTransport>) -> BoxedDemuxReceiver {
///     DemuxReceiver::new(transport)
/// }
/// ```
pub type BoxedDemuxReceiver = DemuxReceiver<Box<dyn crate::RecvTransport>>;

/// Error returned by [`DemuxReceiver`] methods.
///
/// # Categorization
///
/// Bindings categorize failures via [`Self::kind`] (one of 6
/// [`ShellErrorKind`] variants); power users inspect [`Self::source`]
/// for the typed inner error.
///
/// # Reachable kinds
///
/// `DemuxReceiver` can produce: `Backpressure`, `InputMalformed`,
/// `TransportBroken`, `Closed`, `EndOfStream`. `Backpressure` is
/// produced when the underlying transport returns
/// `TransportError::Backpressure` on a recv timeout.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
#[error("DemuxReceiver error ({kind:?}): {source}")]
pub struct DemuxReceiverError {
    pub kind: ShellErrorKind,
    #[source]
    pub source: DemuxReceiverErrorSource,
}

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum DemuxReceiverErrorSource {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Demux(#[from] DemuxError),
}

impl From<TransportError> for DemuxReceiverError {
    fn from(e: TransportError) -> Self {
        Self {
            kind: crate::shell_error::kind_from_transport(&e, crate::shell_error::Direction::Recv),
            source: DemuxReceiverErrorSource::Transport(e),
        }
    }
}

impl From<DemuxError> for DemuxReceiverError {
    fn from(e: DemuxError) -> Self {
        Self {
            kind: crate::shell_error::kind_from_demux(&e),
            source: DemuxReceiverErrorSource::Demux(e),
        }
    }
}

impl crate::shell_error::ShellError for DemuxReceiverError {
    fn kind(&self) -> ShellErrorKind {
        self.kind
    }

    fn errno_code(&self) -> Option<i32> {
        match &self.source {
            DemuxReceiverErrorSource::Transport(t) => {
                crate::shell_error::errno_code_from_transport(t)
            }
            DemuxReceiverErrorSource::Demux(_) => None,
        }
    }
}

/// Stats snapshot for [`DemuxReceiver`]. Composes the underlying
/// [`crate::receiver::ReceiverStats`] (bytes/packets received, sync-recovery
/// counters) with the [`tst_core::mpegts::demux::DemuxerStats`] (events emitted,
/// per-PID counters). Sync-recovery counters (`bytes_skipped_for_sync`,
/// `resync_events`) live only on `ReceiverStats` — call
/// `Receiver::stats()` directly to read them.
#[must_use]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DemuxReceiverStats {
    pub bytes_received: u64,
    pub packets_received: u64,
    pub program_maps_seen: u64,
    pub pmt_versions_seen: u64,
    pub discontinuities: u64,
    pub nonconformant: u64,
    pub per_stream: BTreeMap<u16, tst_core::mpegts::stats::StreamStats>,
}

impl<R: RecvTransport> DemuxReceiver<R> {
    /// Snapshot the current counters. Composes transport-layer byte/packet
    /// counts from the inner `Receiver` with demux-layer event counts from
    /// the inner `Demuxer`.
    pub fn stats(&self) -> DemuxReceiverStats {
        let ts = self.ts.stats();
        let dx = self.demux.stats();
        DemuxReceiverStats {
            bytes_received: ts.bytes_received,
            packets_received: ts.packets_received,
            program_maps_seen: dx.program_maps_seen,
            pmt_versions_seen: dx.pmt_versions_seen,
            discontinuities: dx.discontinuities,
            nonconformant: dx.nonconformant,
            per_stream: dx.per_stream,
        }
    }

    /// Wire-level transport stats (RTT, packet loss, bandwidth, queue
    /// depths) sourced from the underlying
    /// [`RecvTransport::socket_stats`] implementation. Delegates to the
    /// inner `Receiver`, which holds the transport. Returns `None` when
    /// the transport doesn't expose comparable telemetry or when a
    /// managed wrapper has no live inner socket.
    ///
    /// # C ABI
    ///
    /// `tst_demux_receiver_get_socket_stats` — see
    /// `bindings/c/include/tstrans.h`.
    pub fn socket_stats(&self) -> Option<tst_core::transport::SocketStats> {
        self.ts.socket_stats()
    }

    /// Per-PID codec-specific counters. Delegates to the inner
    /// [`tst_core::mpegts::demux::Demuxer::stream_codec_stats`].
    ///
    /// See [`tst_core::mpegts::stats::StreamCodecStats`] for the
    /// semantics of the return value.
    ///
    /// Result does NOT vary with transport reconnect state — the
    /// Demuxer's per-PID state is independent of the live socket. The C
    /// ABI's `tst_managed_demux_receiver_get_stream_codec_stats` returns
    /// the same values as `tst_demux_receiver_get_stream_codec_stats`
    /// during reconnect; no `TST_E_NOT_AVAILABLE` is returned for
    /// codec stats.
    ///
    /// # C ABI
    ///
    /// `tst_demux_receiver_get_stream_codec_stats` (plain) +
    /// `tst_managed_demux_receiver_get_stream_codec_stats` (managed wrapper) —
    /// see `bindings/c/include/tstrans.h`.
    pub fn stream_codec_stats(
        &self,
        pid: u16,
    ) -> Option<tst_core::mpegts::stats::StreamCodecStats> {
        self.demux.stream_codec_stats(pid)
    }

    /// Reset all counters to zero. Delegates to both the inner `Receiver`
    /// and the inner `Demuxer`.
    pub fn reset_stats(&mut self) {
        self.ts.reset_stats();
        self.demux.reset_stats();
    }
}

#[cfg(test)]
mod stats_tests {
    use super::*;
    use std::collections::VecDeque;
    use tst_core::transport::RecvTransport;
    use tst_core::transport::TransportError;

    struct CannedRecv {
        chunks: VecDeque<Vec<u8>>,
        alive: bool,
    }

    impl RecvTransport for CannedRecv {
        fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            match self.chunks.pop_front() {
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

        fn close(&mut self) {
            self.alive = false;
        }
    }

    #[test]
    fn stats_starts_zero_with_empty_per_stream() {
        let r = DemuxReceiver::new(CannedRecv {
            chunks: VecDeque::new(),
            alive: true,
        });
        let st = r.stats();
        assert_eq!(st.bytes_received, 0);
        assert_eq!(st.packets_received, 0);
        assert_eq!(st.program_maps_seen, 0);
        assert_eq!(st.per_stream.len(), 0);
    }

    #[test]
    fn reset_stats_clears_per_stream() {
        let mut r = DemuxReceiver::new(CannedRecv {
            chunks: VecDeque::new(),
            alive: true,
        });
        r.reset_stats();
        let st = r.stats();
        assert!(st.per_stream.is_empty());
    }
}
