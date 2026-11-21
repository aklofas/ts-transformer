//! `DemuxReceiver<R>` — full receive: RecvTransport → Receiver → Demuxer.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Mirrors the `MuxSender` shape on the receive side: one transport in,
//! `DemuxEvent`s out.
//!
//! # Raw TS access
//!
//! The shell offers no hook that hands raw bytes to caller code. To count,
//! read [`DemuxReceiver::ts_bytes_demuxed`] (or a
//! [`TsBytesDemuxedHandle`] that outlives the shell): the aligned TS bytes
//! fed to the demuxer, read without waiting on a parked receive. To record,
//! relay or inspect packets, compose a [`Receiver`] with a [`Demuxer`] and
//! own the loop:
//!
//! ```no_run
//! use tst_core::mpegts::demux::{Demuxer, DemuxerConfig};
//! use tst_pipeline::{Receiver, ReceiverConfig, RecvTransport, ShellErrorKind, TransportError};
//!
//! /// Stands in for any real transport.
//! struct Source;
//! impl RecvTransport for Source {
//!     fn recv_bytes(&mut self, _buf: &mut [u8]) -> Result<usize, TransportError> {
//!         Err(TransportError::Closed)
//!     }
//!     fn max_payload(&self) -> usize { 1316 }
//!     fn is_alive(&self) -> bool { true }
//! }
//!
//! let mut rx = Receiver::new(Source, ReceiverConfig::default());
//! let mut demux = Demuxer::with_config(DemuxerConfig::default());
//! let mut recorded: Vec<u8> = Vec::new();
//! loop {
//!     let pkt = match rx.next_packet() {
//!         Ok(pkt) => pkt,
//!         Err(e) if e.kind == ShellErrorKind::EndOfStream => break,
//!         // A terminal error also falls through to the flush below.
//!         Err(_) => break,
//!     };
//!     recorded.extend_from_slice(&pkt); // your consumer
//!     if demux.feed_aligned(&pkt).is_err() {
//!         break;
//!     }
//!     while let Some(_event) = demux.next_event() { /* handle it */ }
//! }
//! demux.flush();
//! while let Some(_event) = demux.next_event() { /* handle it */ }
//! ```
//!
//! The loop leaves two duties to the caller. Both a clean end and a terminal
//! error must `flush` the demuxer, or the last length-0 PES is lost. Over a
//! `ManagedRecvTransport`, a rise of `reconnects_handle()` means the packet
//! just read belongs to the new link: drop it and call `reset_sync()` on the
//! [`Receiver`] and the [`Demuxer`]. `ManagedDemuxReceiver` does both for you.
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

use crate::binding::kind::{coarsen, kind_of_demux, kind_of_transport};
use crate::receiver::engine::{End, ReceiveEngine, Step};
use crate::receiver::{Receiver, ReceiverConfig, ReceiverErrorSource};
use crate::shell_error::ShellErrorKind;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
#[cfg(feature = "std")]
use core::sync::atomic::{AtomicU64, Ordering};
use tracing::info_span;
use tst_core::error::DemuxError;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, DemuxerConfig};
use tst_core::transport::RecvTransport;
use tst_core::transport::TransportError;

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
}

/// Full receive shell: `RecvTransport → Receiver → Demuxer`.
///
/// `R` is any [`RecvTransport`] — typically an SRT-specific transport for live
/// connections, or a test mock (e.g. `CannedTransport` in the integration
/// tests).
///
/// # Usage
///
/// ```ignore
/// let mut rx = DemuxReceiver::new(transport);
/// for result in &mut rx {
///     match result.unwrap() {
///         DemuxEvent::ProgramMap(pmt) => { /* inspect PMT */ }
///         DemuxEvent::Sample { stream, payload, .. } => { /* forward AU */ }
///         _ => {}
///     }
/// }
/// ```
///
/// # Sync lock
///
/// Packets reach the demuxer through the inner [`Receiver`], which locks
/// only after four aligned TS packets (752 bytes) and, after any sync
/// loss, strands up to three unconfirmed trailing packets at
/// end-of-stream — see [`Receiver`]'s "Sync lock and the trailing
/// window". A session of fewer than four packets therefore yields no
/// events at all, and the last access unit after a mid-stream corruption
/// may never complete.
///
/// # Closing
///
/// `DemuxReceiver` supports three shutdown patterns:
///
/// 1. **Drop** — the [`Drop`] impl emits a tracing event and lets the
///    inner [`Receiver`] / transport `Drop` chain run. Synchronous: it
///    takes as long as the transport's close takes; see its docs.
/// 2. **Explicit close** — call [`Self::close`]. Closes the underlying
///    recv transport; the next `recv_event` flushes the demuxer and
///    returns `Ok(None)` once the queue drains. Idempotent.
/// 3. **Cross-thread cancel** — call [`Self::cancel_handle`] to obtain a
///    `Send + Sync` [`tst_core::transport::TransportCancel`] handle,
///    then `cancel()` from any thread. Wakes a peer thread parked in
///    `recv_event`'s underlying `next_packet` call; how soon depends on
///    the transport (SRT closes the socket, the others notice at their
///    next poll tick); see
///    [the cancel coverage table](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/srt-cancel-handle.md#cancel-coverage-by-transport).
///
/// C ABI for the receiver surface: `tst_demux_receiver_open` /
/// `_recv_event` / `_close` / `_cancel` / `_get_stats` and the typed
/// event arena. See `bindings/c/include/tstrans.h` for the C surface.
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
    /// The demuxer, the owed reconnect marker and the end of the stream;
    /// `recv_event` is its driver.
    engine: ReceiveEngine,
    /// Aligned TS bytes handed to the demuxer; see
    /// [`DemuxReceiver::ts_bytes_demuxed`].
    #[cfg(feature = "std")]
    ts_bytes_demuxed: Arc<AtomicU64>,
    /// See [`ReconnectEpoch`]; installed via `set_reconnect_epoch`.
    #[cfg(feature = "std")]
    reconnect_epoch: Option<ReconnectEpoch>,
    /// Lifetime span — see [`crate::shell_error::ShellSpan`] for the
    /// unwind-safe rationale. Private; never exposed publicly.
    _span: crate::shell_error::ShellSpan,
}

/// Reader for [`DemuxReceiver::ts_bytes_demuxed`] that does not borrow the
/// shell. Obtain it with [`DemuxReceiver::ts_bytes_demuxed_handle`].
#[cfg(feature = "std")]
#[derive(Clone, Debug)]
pub struct TsBytesDemuxedHandle(Arc<AtomicU64>);

#[cfg(feature = "std")]
impl TsBytesDemuxedHandle {
    /// Current value of the counter.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl<R: RecvTransport> core::fmt::Debug for DemuxReceiver<R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DemuxReceiver")
            .field("is_alive", &self.is_alive())
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
            engine: ReceiveEngine::new(Demuxer::with_config(options)),
            #[cfg(feature = "std")]
            ts_bytes_demuxed: Arc::new(AtomicU64::new(0)),
            #[cfg(feature = "std")]
            reconnect_epoch: None,
            _span: core::panic::AssertUnwindSafe(span),
        }
    }

    /// Total aligned TS bytes this shell has handed to its demuxer, in
    /// multiples of 188. An atomic load that never waits. `recv_event`
    /// borrows the shell mutably, so to read the count from another thread
    /// while one is parked, take a [`Self::ts_bytes_demuxed_handle`] first.
    ///
    /// - Counted: every aligned packet fed to the demuxer, including ones a
    ///   strict-mode demuxer then rejects.
    /// - Not counted: bytes the syncer is still holding, bytes it skipped
    ///   before locking, and the one packet a managed shell drops at a
    ///   reconnect.
    /// - Monotonic from creation: `reset_stats`, a reconnect, `cancel` and
    ///   `close` never lower it, so it can exceed
    ///   [`DemuxReceiverStats::bytes_received`] after `reset_stats`. After a
    ///   reconnect `bytes_received` leads by the dropped splice packet.
    ///
    /// Absent without the `std` feature.
    #[cfg(feature = "std")]
    #[must_use]
    pub fn ts_bytes_demuxed(&self) -> u64 {
        self.ts_bytes_demuxed.load(Ordering::Relaxed)
    }

    /// A cheap, cloneable reader of [`Self::ts_bytes_demuxed`] that outlives
    /// the shell and reads without touching it.
    #[cfg(feature = "std")]
    #[must_use]
    pub fn ts_bytes_demuxed_handle(&self) -> TsBytesDemuxedHandle {
        TsBytesDemuxedHandle(Arc::clone(&self.ts_bytes_demuxed))
    }

    /// Watch a transport's reconnect counter: when it rises between
    /// packets, the just-read packet is discarded, the sync/demux state is
    /// reset, and the next `recv_event` yields
    /// `DemuxEvent::ReconnectDiscontinuity` first. Installed by
    /// `ManagedDemuxReceiver` at construction; the snapshot starts at 0
    /// (the counter is 0 for a freshly built managed transport).
    #[cfg(feature = "std")]
    pub(crate) fn set_reconnect_epoch(&mut self, reconnects: Arc<AtomicU64>) {
        self.reconnect_epoch = Some(ReconnectEpoch {
            counter: reconnects,
            last: 0,
        });
    }

    /// The epoch observer, asked after each `next_packet`: `true` when the
    /// counter rose since the last turn (the caller then resets state and
    /// drops the packet it just read). Always `false` without an observer
    /// (and on no_std builds, which have none).
    #[cfg(feature = "std")]
    fn observe_reconnect(&mut self) -> bool {
        let Some(ep) = self.reconnect_epoch.as_mut() else {
            return false;
        };
        let current = ep.counter.load(Ordering::Acquire);
        if current > ep.last {
            ep.last = current;
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
    /// # After the end
    ///
    /// The first `Ok(None)`, or the first error of kind `TransportBroken` or
    /// `Closed`, is final: the transport is not read again and every later
    /// call returns that same result, whatever the transport would answer
    /// by then (an error is rebuilt from the transport error that ended the
    /// stream). `Backpressure` and `InputMalformed` are not ends.
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
            // The engine's order: a reconnect marker before anything of the
            // new connection, then the demuxer's queue (after an end, the
            // events its flush produced), then the end itself. Once it has
            // an end the transport is not read again.
            match self.engine.poll() {
                Step::Event(e) => return Ok(Some(e)),
                Step::Ended(Ok(())) => return Ok(None),
                Step::Ended(Err(te)) => return Err(te.into()),
                Step::NeedInput => {}
            }
            // Pull the next aligned 188-byte TS packet.
            let pkt = match self.ts.next_packet() {
                Ok(p) => p,
                Err(e) => {
                    let ReceiverErrorSource::Transport(te) = e.source;
                    match e.kind {
                        // Peer end-of-stream.
                        ShellErrorKind::EndOfStream => self.engine.handle_end(End::Clean),
                        // Terminal, but not a clean peer EOS: the socket
                        // died, or a caller closed/cancelled from another
                        // thread while this call was blocked. Either way no
                        // further `next_packet` call will make progress.
                        ShellErrorKind::TransportBroken | ShellErrorKind::Closed => {
                            self.engine.handle_end(End::Failed(te));
                        }
                        // `Backpressure` means "retry", not "the loop is
                        // over": the engine is not told, so nothing is
                        // flushed (that would finalize an access unit still
                        // being received) and nothing is kept.
                        _ => return Err(te.into()),
                    }
                    continue;
                }
            };
            // Managed shells only: the reconnect check sits AFTER
            // `next_packet` because a reconnect can fire DURING a single
            // `next_packet` call (several `recv_bytes` over one packet
            // boundary). If the counter rose, this packet may be spliced
            // from dead-tail bytes plus fresh ones — drop it, reset the
            // sync/demux state, and let the syncer re-lock on the next
            // fresh bytes (the documented data-loss budget on reconnect).
            // The dropped packet was never parsed and its bytes are not
            // trustworthy.
            if self.observe_reconnect() {
                self.ts.reset_sync();
                self.engine.handle_reconnect();
                continue;
            }
            // Counted here, after the splice drop above: a dropped packet
            // never reaches this line. Strict-mode rejections below still
            // count, since the packet was handed to the demuxer.
            #[cfg(feature = "std")]
            self.ts_bytes_demuxed.fetch_add(
                tst_core::mpegts::common::TS_PACKET_SIZE as u64,
                Ordering::Relaxed,
            );
            // Feed to demuxer via the aligned fast path — the Receiver
            // transport layer already produces [u8; 188] packets so no sync
            // buffering or 0x47 hunt is needed.  In lenient mode this only
            // errors on Unrecoverable (caller violated alignment contract);
            // MalformedPes is converted to a NonConformant event by the inner
            // demuxer. In strict mode it can also return StrictRejection or
            // MalformedPes.
            self.engine
                .handle_packet(&pkt)
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
            kind: coarsen(kind_of_transport(&e, crate::shell_error::Direction::Recv)),
            source: DemuxReceiverErrorSource::Transport(e),
        }
    }
}

impl From<DemuxError> for DemuxReceiverError {
    fn from(e: DemuxError) -> Self {
        Self {
            kind: coarsen(kind_of_demux(&e)),
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

tst_core::binding_record! {
    /// Counters of a demuxing receiver: the aligned TS packets it received
    /// and what its demuxer saw in them. Counters are cumulative since the
    /// receiver was built or its stats were last reset.
    ///
    /// <!-- rust-only -->
    /// Stats snapshot for [`DemuxReceiver`]. Composes the underlying
    /// [`crate::receiver::ReceiverStats`] (bytes/packets received) with the
    /// [`tst_core::mpegts::demux::DemuxerStats`] (events emitted, per-PID
    /// counters). The sync-recovery counters (`bytes_skipped_for_sync`,
    /// `resync_events`) live only on `ReceiverStats`.
    #[must_use]
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct DemuxReceiverStats {
        /// Bytes of the aligned 188-byte TS packets the receiver took from
        /// its transport, `packets_received` times 188. A managed receiver
        /// also counts the one packet it drops at each reconnect. A stats
        /// reset zeroes it; the receiver's demuxed-bytes count is the one
        /// that never resets and never waits for the receiver.
        ///
        /// <!-- rust-only -->
        /// [`DemuxReceiver::reset_stats`] zeroes it;
        /// [`DemuxReceiver::ts_bytes_demuxed`] is the never-waiting count.
        pub bytes_received: u64,
        /// Aligned 188-byte TS packets the receiver took from its transport.
        /// A stats reset zeroes it.
        pub packets_received: u64,
        /// Program maps the demuxer reported: one each time a program's PMT
        /// version changes, the first sighting included. A stats reset
        /// zeroes it.
        pub program_maps_seen: u64,
        /// PMT versions taken: one each time a program's PMT version
        /// changes, the first sighting included, so a version that comes
        /// back counts again and two programs count one each; the same
        /// count as `program_maps_seen`. A stats reset zeroes it and keeps
        /// the known versions; a managed receiver's reconnect forgets them,
        /// so the next PMT after it counts even at the same version.
        pub pmt_versions_seen: u64,
        /// Discontinuities the demuxer reported, over every PID. A stats
        /// reset zeroes it.
        pub discontinuities: u64,
        /// Non-conformant inputs the demuxer reported, over every PID. A
        /// stats reset zeroes it.
        pub nonconformant: u64,
        /// Per-PID counters, copied from the demuxer's stats.
        @[skip]
        pub per_stream: BTreeMap<u16, tst_core::mpegts::stats::StreamStats>,
    }
}

impl<R: RecvTransport> DemuxReceiver<R> {
    /// Snapshot the current counters. Composes transport-layer byte/packet
    /// counts from the inner `Receiver` with demux-layer event counts from
    /// the inner `Demuxer`.
    pub fn stats(&self) -> DemuxReceiverStats {
        let ts = self.ts.stats();
        let dx = self.engine.demuxer().stats();
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
        self.engine.demuxer().stream_codec_stats(pid)
    }

    /// Reset all counters to zero. Delegates to both the inner `Receiver`
    /// and the inner `Demuxer`.
    pub fn reset_stats(&mut self) {
        self.ts.reset_stats();
        self.engine.demuxer_mut().reset_stats();
    }
}

#[cfg(test)]
mod stats_tests {
    use super::*;
    use alloc::vec::Vec;
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
    fn demux_receiver_stats_schema_lists_the_six_counters() {
        let schema = DemuxReceiverStats::BINDING_SCHEMA;
        let names: Vec<&str> = schema.fields.iter().map(|f| f.name).collect();
        assert_eq!(
            names,
            [
                "bytes_received",
                "packets_received",
                "program_maps_seen",
                "pmt_versions_seen",
                "discontinuities",
                "nonconformant",
                "per_stream",
            ]
        );
        let crossing: Vec<&str> = schema
            .fields
            .iter()
            .filter(|f| f.mark != tst_core::binding_schema::FieldMark::Skip)
            .map(|f| f.name)
            .collect();
        assert_eq!(crossing.len(), 6, "{crossing:?}");
        assert!(!crossing.contains(&"per_stream"));
    }

    const PMT_PID: u16 = 0x0100;
    const VIDEO_PID: u16 = 0x0101;

    fn ts_packet(pid: u16, payload: &[u8], pusi: bool, cc: u8) -> Vec<u8> {
        let mut p = vec![
            0x47,
            (u8::from(pusi) << 6) | (pid >> 8) as u8,
            pid as u8,
            0x10 | (cc & 0x0F),
        ];
        p.extend_from_slice(payload);
        p.resize(188, 0xFF);
        p
    }

    /// One section after a zero pointer field, its CRC added (and broken
    /// when `bad_crc`).
    fn psi_packet(pid: u16, section: &[u8], cc: u8, bad_crc: bool) -> Vec<u8> {
        let mut crc = tst_core::mpegts::common::crc32::crc32_mpeg2(section);
        if bad_crc {
            crc ^= 1;
        }
        let mut payload = vec![0x00];
        payload.extend_from_slice(section);
        payload.extend_from_slice(&crc.to_be_bytes());
        ts_packet(pid, &payload, true, cc)
    }

    fn pat_packet() -> Vec<u8> {
        let section = [
            0x00,
            0xB0,
            13,
            0x00,
            0x01,
            0xC1,
            0x00,
            0x00,
            0x00,
            0x01,
            0xE0 | (PMT_PID >> 8) as u8,
            PMT_PID as u8,
        ];
        psi_packet(0x0000, &section, 0, false)
    }

    /// The PMT of program 1: one H.264 stream on `VIDEO_PID`.
    fn pmt_packet(version: u8, cc: u8, bad_crc: bool) -> Vec<u8> {
        let section = [
            0x02,
            0xB0,
            18,
            0x00,
            0x01,
            0xC1 | ((version & 0x1F) << 1),
            0x00,
            0x00,
            0xE0 | (VIDEO_PID >> 8) as u8,
            VIDEO_PID as u8,
            0xF0,
            0x00,
            0x1B,
            0xE0 | (VIDEO_PID >> 8) as u8,
            VIDEO_PID as u8,
            0xF0,
            0x00,
        ];
        psi_packet(PMT_PID, &section, cc, bad_crc)
    }

    /// Two PMT versions (version 0 repeated), three PMTs with a broken CRC,
    /// and a video PES whose packets skip three continuity counters: three
    /// discontinuities, and four non-conformant inputs (the three CRCs and
    /// the PES the gaps cut).
    fn counted_capture() -> Vec<Vec<u8>> {
        let pes_start = [
            0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x09,
            0xF0,
        ];
        vec![
            pat_packet(),
            pmt_packet(0, 0, false),
            pmt_packet(0, 1, false),
            ts_packet(VIDEO_PID, &pes_start, true, 0),
            ts_packet(VIDEO_PID, &[], false, 1),
            ts_packet(VIDEO_PID, &[], false, 3),
            ts_packet(VIDEO_PID, &[], false, 5),
            ts_packet(VIDEO_PID, &[], false, 7),
            pmt_packet(1, 2, true),
            pmt_packet(1, 3, true),
            pmt_packet(1, 4, true),
            pmt_packet(1, 5, false),
            ts_packet(0x1FFF, &[], false, 0),
            ts_packet(0x1FFF, &[], false, 1),
            ts_packet(0x1FFF, &[], false, 2),
        ]
    }

    /// Each counter driven to its own value, so a record that copies one
    /// counter into another field fails.
    #[test]
    fn stats_names_every_counter_by_its_own_value() {
        let capture = counted_capture();
        let n = capture.len() as u64;
        // Two packets a read: no read ends a PSI section early.
        let chunks: VecDeque<Vec<u8>> = capture.chunks(2).map(|c| c.concat()).collect();
        let mut r = DemuxReceiver::new(CannedRecv {
            chunks,
            alive: true,
        });
        while r.recv_event().is_ok_and(|e| e.is_some()) {}
        let st = r.stats();
        assert_eq!(st.program_maps_seen, 2, "{st:?}");
        assert_eq!(st.pmt_versions_seen, 2, "{st:?}");
        assert_eq!(st.discontinuities, 3, "{st:?}");
        assert_eq!(st.nonconformant, 4, "{st:?}");
        assert_eq!(st.packets_received, n, "{st:?}");
        assert_eq!(st.bytes_received, n * 188, "{st:?}");
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

    /// One H.264 stream on PID 0x100: PSI and two access units from one
    /// muxer, then a third access unit with no PSI ahead of it.
    fn two_reads_of_one_stream() -> (Vec<u8>, Vec<u8>) {
        use tst_core::mpegts::common::Pts90khz;
        use tst_core::mpegts::mux::{Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        let mut m = Muxer::new(b.build().unwrap()).unwrap();
        let au = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB, 0xBB, 0xBB];
        let pull = |m: &mut Muxer| {
            let mut out = vec![0u8; 188 * 16];
            let n = m.pull(&mut out);
            out.truncate(n);
            out
        };
        m.push_video(&au, Pts90khz::new(0), true).unwrap();
        m.push_video(&au, Pts90khz::new(3000), true).unwrap();
        let first = pull(&mut m);
        m.push_video(&au, Pts90khz::new(6000), false).unwrap();
        let second: Vec<u8> = pull(&mut m)
            .chunks_exact(188)
            .filter(|p| (u16::from(p[1] & 0x1F) << 8 | u16::from(p[2])) == 0x100)
            .flatten()
            .copied()
            .collect();
        // `CannedRecv` truncates a chunk to the read buffer.
        assert!(first.len() <= 1316 && second.len() <= 1316);
        (first, second)
    }

    #[test]
    fn reset_stats_zeros_observed_counters() {
        let (first, second) = two_reads_of_one_stream();
        let mut r = DemuxReceiver::new(CannedRecv {
            chunks: VecDeque::from([first, second]),
            alive: true,
        });
        let is_sample = |ev: &DemuxEvent| matches!(ev, DemuxEvent::Sample { .. });
        while !is_sample(&r.recv_event().unwrap().expect("a sample before EOF")) {}
        let st = r.stats();
        assert!(st.packets_received > 0, "packets were counted");
        assert!(!st.per_stream.is_empty(), "the stream was counted");
        #[cfg(feature = "std")]
        let n = {
            let n = r.ts_bytes_demuxed();
            assert!(n > 0);
            assert_eq!(n, st.bytes_received);
            n
        };

        r.reset_stats();
        let st = r.stats();
        assert_eq!(st.packets_received, 0);
        assert_eq!(st.bytes_received, 0);
        assert!(st.per_stream.is_empty());
        #[cfg(feature = "std")]
        assert_eq!(r.ts_bytes_demuxed(), n, "reset_stats leaves the counter");

        // A later access unit still demuxes (parser state kept); the last
        // read carries no PSI.
        while !is_sample(&r.recv_event().unwrap().expect("a sample after the reset")) {}
        #[cfg(feature = "std")]
        assert!(r.ts_bytes_demuxed() > n);
    }

    #[cfg(feature = "std")]
    #[test]
    fn ts_bytes_demuxed_handle_outlives_the_shell() {
        let (first, second) = two_reads_of_one_stream();
        let mut r = DemuxReceiver::new(CannedRecv {
            chunks: VecDeque::from([first, second]),
            alive: true,
        });
        let h = r.ts_bytes_demuxed_handle();
        while r.recv_event().unwrap().is_some() {}
        let recorded = r.ts_bytes_demuxed();
        assert!(recorded > 0);
        drop(r);
        assert_eq!(h.get(), recorded);
        assert_eq!(recorded % 188, 0);
    }

    #[cfg(feature = "std")]
    #[test]
    fn ts_bytes_demuxed_is_zero_before_sync_lock() {
        let mut bytes = Vec::new();
        for _ in 0..3 {
            let mut p = [0xFFu8; 188];
            p[0] = 0x47;
            p[1] = 0x1F;
            p[2] = 0xFF;
            p[3] = 0x10;
            bytes.extend_from_slice(&p);
        }
        bytes.extend_from_slice(&[0xAB; 100]);
        let mut r = DemuxReceiver::new(CannedRecv {
            chunks: VecDeque::from([bytes]),
            alive: true,
        });
        assert!(r.recv_event().unwrap().is_none());
        assert_eq!((r.ts_bytes_demuxed(), r.stats().bytes_received), (0, 0));
    }
}
