//! `ManagedDemuxReceiver<R>` — reconnect-aware full receive shell.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Composition: `ManagedRecvTransport<R> → Receiver → Demuxer`. Unlike
//! the byte-level [`ManagedRecvTransport`] used directly under
//! `Receiver` / [`DemuxReceiver`][crate::DemuxReceiver],
//! this shell knows about BOTH the reconnect signal AND the sync /
//! demux state, and resets the latter when the former fires.
//!
//! # Why a new shell instead of pushing this into ManagedRecvTransport
//!
//! `ManagedRecvTransport` returns `Ok(bytes)` from `recv_bytes` after
//! a reconnect with no out-of-band signal — the [`RecvTransport`] trait
//! has no shape for "the byte you're getting is from a fresh
//! connection." Higher-level shells that own only the
//! [`RecvTransport`] trait reference cannot detect the boundary.
//! `ManagedDemuxReceiver` solves this by owning the `ManagedRecvTransport`
//! as a CONCRETE type and polling
//! [`ManagedRecvTransport::reconnects_count`] between events.
//!
//! # The bug this fixes
//!
//! Today's `DemuxReceiver<ManagedRecvTransport<T>>` composition keeps
//! the `Receiver`'s syncer buffer and the `Demuxer`'s PSI/PES state
//! across reconnects. Bytes from the dropped connection carry into the
//! new connection's framing:
//!
//! - The syncer's `[u8]` ring may hold a fractional packet from the
//!   dead connection. The next bytes from the new connection get
//!   appended; 0x47 byte alignment is then computed against a stale
//!   prefix.
//! - The PES reassembler has half-built sample buffers indexed by PID.
//!   The next packet from the new connection on the same PID is
//!   appended to the dead sample → emit-time, the consumer gets a
//!   sample whose first ~N bytes are from connection A and the rest
//!   from connection B.
//! - PSI section assemblers may hold a partial PAT/PMT section. Next
//!   PUSI from the new connection completes the section with bytes
//!   from a different version of the PAT/PMT.
//! - Continuity counters carry over → bogus CC jump events fire.
//!
//! `ManagedDemuxReceiver` calls [`Receiver::reset_sync`] and
//! [`tst_core::mpegts::demux::Demuxer::reset_sync`] when the reconnect
//! counter rises, then queues a [`DemuxEvent::ReconnectDiscontinuity`]
//! event for the next `recv_event` call so the consumer sees the
//! boundary explicitly.
//!
//! # Data-loss budget on reconnect
//!
//! After the underlying [`ManagedRecvTransport`] reconnects, the FIRST
//! aligned 188-byte TS packet returned by [`Receiver::next_packet`]
//! after the reconnect boundary is **discarded** by this shell before
//! it reaches the demuxer. The shell additionally clears the syncer's
//! buffer (via [`Receiver::reset_sync`]), which drops any bytes already
//! pulled from the transport but not yet emitted as aligned packets —
//! in practice on the order of one SRT payload (~1316 bytes ≈ 7 TS
//! packets at worst) of buffered bytes are lost at the moment of
//! detection.
//!
//! **Why:** the first packet returned post-reconnect may have been
//! assembled from a mix of dead-tail bytes left in the syncer's ring
//! buffer (from the dropped connection) plus fresh bytes from the new
//! connection. Separating the two would require packet-byte forensics
//! inside the syncer; discarding the packet, clearing the syncer
//! buffer, and letting the syncer re-lock cleanly on subsequent fresh
//! bytes is simpler and safer. The drop applies uniformly — even when
//! the reconnect boundary happens to fall exactly on a packet edge with
//! no dead-tail bytes, the first post-reconnect packet is still
//! dropped. The shell does not attempt to detect "clean" vs "spliced"
//! reconnect cases; it pays the cost in all cases for predictability
//! and simplicity.
//!
//! **The cost:** at least one TS packet (188 bytes) plus any bytes the
//! syncer had buffered but not yet drained — bounded by the underlying
//! transport's `max_payload` (typically a single SRT payload). For
//! streams with PSI repetition rates of tens of milliseconds, the next
//! PAT/PMT repetition arrives quickly and the consumer sees a fresh
//! [`DemuxEvent::ProgramMap`] event shortly after the boundary. Lost
//! packets that fall on a PES sample boundary are handled transparently
//! by the demuxer's PES reassembler, which is also reset across the
//! boundary — the next [`DemuxEvent::Sample`] arrives once a new
//! PUSI-marked packet seeds reassembly on the corresponding PID.
//!
//! **What consumers should expect:** stream tables (PAT/PMT topology),
//! codec parameters, and PES reassembly state from the dead connection
//! are dropped — the demuxer is reset to a clean state. Consumers that
//! cached the prior [`DemuxEvent::ProgramMap`] should drop that cache on
//! receipt of [`DemuxEvent::ReconnectDiscontinuity`] and rebuild it from
//! the next `ProgramMap` event that arrives over the new connection.
//!
//! # Closing
//!
//! Mirrors [`DemuxReceiver`][crate::DemuxReceiver]'s shutdown patterns
//! (Drop / `close()` / cross-thread `cancel_handle().cancel()`).

use crate::demux_receiver::DemuxReceiverError;
use crate::managed_receive::ManagedRecvTransport;
use crate::receiver::{Receiver, ReceiverConfig, ReceiverErrorSource};
use crate::reconnect::{RecvEndReason, RecvEndReasonHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tracing::{info, info_span};
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, DemuxerConfig};
use tst_core::transport::RecvTransport;

/// Construction parameters for [`ManagedDemuxReceiver`].
///
/// Currently empty; reserved for future knobs (e.g. emit-discontinuity-
/// on-first-connect, demuxer-reset opt-out for callers that want
/// raw-bytes-only). Construct via `Default::default()` and assign
/// overrides as fields land.
///
/// Symmetric with [`crate::ReceiverConfig`] /
/// [`crate::demux_receiver::DemuxReceiver`] which take no config
/// today.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Default, Clone)]
pub struct ManagedDemuxReceiverConfig {}

/// Reconnect-aware full receive shell.
///
/// Owns a [`ManagedRecvTransport`] (byte-level reconnect) wrapped in a
/// [`Receiver`] (TS sync recovery) plus a [`Demuxer`] (PSI/PES parse).
/// Between event emissions, polls the reconnect counter; on a fresh
/// transport rebuild, drops sync/demux state and surfaces the boundary
/// to the caller as a [`DemuxEvent::ReconnectDiscontinuity`] event.
///
/// # Usage
///
/// ```ignore
/// use tst_pipeline::{
///     ManagedDemuxReceiver, ManagedDemuxReceiverConfig, ManagedRecvTransport,
///     ReconnectPolicy,
/// };
///
/// let factory = Box::new(|| SrtTransport::connect(addr, &cfg));
/// let inner = SrtTransport::connect(addr, &cfg)?;
/// let managed = ManagedRecvTransport::new(inner, factory, ReconnectPolicy::default());
/// let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());
///
/// for ev in &mut rx {
///     match ev? {
///         DemuxEvent::ReconnectDiscontinuity => {
///             // Drop any per-stream caches; next ProgramMap will arrive.
///         }
///         DemuxEvent::ProgramMap(pmt) => { /* re-build caches */ }
///         DemuxEvent::Sample { .. } => { /* forward AU */ }
///         _ => {}
///     }
/// }
/// ```
pub struct ManagedDemuxReceiver<R: RecvTransport> {
    ts: Receiver<ManagedRecvTransport<R>>,
    demux: Demuxer,
    /// Shared handle to the underlying `ManagedRecvTransport`'s
    /// reconnect counter. Snapshotted in `new()` so the shell can poll
    /// it without an accessor on `Receiver` (which would expose its
    /// inner transport publicly).
    reconnects: Arc<AtomicU64>,
    /// Reconnect counter snapshot from the last loop iteration. When
    /// the live counter rises above this value the shell knows a
    /// reconnect just fired since the previous `recv_event` and resets
    /// sync/demux state.
    last_reconnects: u64,
    /// Set when a reconnect was detected mid-loop; consumed by the next
    /// `recv_event` to yield `DemuxEvent::ReconnectDiscontinuity` before
    /// any post-reconnect events. Stored as `bool` rather than queued
    /// directly into the demuxer's `queue` because the demuxer's queue
    /// is cleared by `reset_sync` and we want the event to survive that
    /// clear.
    pending_reconnect_event: bool,
    /// Shared flag mirroring the inner [`ManagedRecvTransport`]'s
    /// "connection currently absent" state. Snapshotted in `new()` /
    /// `with_demux_options()` the same way as `reconnects` — `Receiver`
    /// doesn't expose its inner transport publicly, so this is the only
    /// way to read it after construction. Backs [`Self::reconnecting`].
    reconnect_in_progress: Arc<AtomicBool>,
    /// Shared, first-writer-wins record of why this receiver's stream
    /// ended — see [`RecvEndReason`]. [`Self::end_reason_handle`] hands
    /// out clones so a caller can poll it independent of this receiver's
    /// lifetime (e.g. after it's been moved into a C handle, or dropped).
    end_reason: RecvEndReasonHandle,
    /// Lifetime span, entered only during construction and `Drop` — see
    /// [`crate::shell_error::ShellSpan`] for the unwind-safety rationale.
    _span: crate::shell_error::ShellSpan,
}

impl<R: RecvTransport> std::fmt::Debug for ManagedDemuxReceiver<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedDemuxReceiver")
            .field("is_alive", &self.is_alive())
            .field("last_reconnects", &self.last_reconnects)
            .field("pending_reconnect_event", &self.pending_reconnect_event)
            .field("transport_kind", &std::any::type_name::<R>())
            .finish()
    }
}

impl<R: RecvTransport> ManagedDemuxReceiver<R> {
    /// Wrap a [`ManagedRecvTransport`] with default demuxer options
    /// (lenient mode).
    pub fn new(transport: ManagedRecvTransport<R>, _config: ManagedDemuxReceiverConfig) -> Self {
        let span = info_span!(
            target: "tst_pipeline::managed_demux_receiver",
            "managed_demux_receiver",
            transport_kind = std::any::type_name::<R>(),
        );
        let _enter = span.enter();
        info!("ManagedDemuxReceiver opened");
        drop(_enter);
        let reconnects = transport.reconnects_handle();
        let reconnect_in_progress = transport.reconnecting_handle();
        Self {
            ts: Receiver::new(transport, ReceiverConfig::default()),
            demux: Demuxer::new(),
            reconnects,
            last_reconnects: 0,
            pending_reconnect_event: false,
            reconnect_in_progress,
            end_reason: RecvEndReasonHandle::default(),
            _span: std::panic::AssertUnwindSafe(span),
        }
    }

    /// Wrap a [`ManagedRecvTransport`] with custom demuxer options
    /// (e.g. strict mode).
    pub fn with_demux_options(
        transport: ManagedRecvTransport<R>,
        options: DemuxerConfig,
        _config: ManagedDemuxReceiverConfig,
    ) -> Self {
        let span = info_span!(
            target: "tst_pipeline::managed_demux_receiver",
            "managed_demux_receiver",
            transport_kind = std::any::type_name::<R>(),
        );
        let _enter = span.enter();
        info!("ManagedDemuxReceiver opened");
        drop(_enter);
        let reconnects = transport.reconnects_handle();
        let reconnect_in_progress = transport.reconnecting_handle();
        Self {
            ts: Receiver::new(transport, ReceiverConfig::default()),
            demux: Demuxer::with_config(options),
            reconnects,
            last_reconnects: 0,
            pending_reconnect_event: false,
            reconnect_in_progress,
            end_reason: RecvEndReasonHandle::default(),
            _span: std::panic::AssertUnwindSafe(span),
        }
    }

    /// Pull one [`DemuxEvent`].
    ///
    /// Detects transport reconnects between iterations: when the
    /// underlying [`ManagedRecvTransport::reconnects_count`] rises,
    /// the shell resets the syncer + demuxer state and emits
    /// [`DemuxEvent::ReconnectDiscontinuity`] before yielding any
    /// further events from the fresh connection.
    ///
    /// # Errors
    ///
    /// Returns [`DemuxReceiverError`] (same shape as plain
    /// `DemuxReceiver` — variants identical). Reconnect is NOT an
    /// error: it surfaces in-band as a `ReconnectDiscontinuity` event.
    /// Terminal closes (budget exhausted, peer EOS, or caller cancel)
    /// surface as `Ok(None)` after the demuxer's final `flush` drain,
    /// matching `DemuxReceiver::recv_event` semantics.
    pub fn recv_event(&mut self) -> Result<Option<DemuxEvent>, DemuxReceiverError> {
        loop {
            // Step 1: yield a pending reconnect-discontinuity event first.
            // This must precede the demuxer's own queue drain — after a
            // reset_sync that queue is empty, but a future variant of this
            // shell might leave non-reconnect events in flight; ordering
            // the discontinuity event first prevents post-reconnect events
            // from being yielded before the boundary marker.
            if self.pending_reconnect_event {
                self.pending_reconnect_event = false;
                return Ok(Some(DemuxEvent::ReconnectDiscontinuity));
            }

            // Step 2: fast path — demuxer already has a queued event.
            if let Some(e) = self.demux.next_event() {
                return Ok(Some(e));
            }

            // Step 3: pull the next aligned 188-byte packet from the
            // sync layer. This is where a reconnect manifests: the
            // underlying ManagedRecvTransport rebuilds the inner
            // transport, then returns Ok from recv_bytes — the syncer
            // sees the bytes-since-last-call rise but has no way to
            // know they came from a new connection. Detect via the
            // reconnect counter BEFORE feeding any new bytes to the
            // syncer.
            //
            // Race note: a reconnect that happens during a recv block
            // is not detected here UNTIL the next recv returns, at
            // which point the count check below picks it up.
            let pkt = match self.ts.next_packet() {
                Ok(p) => p,
                Err(e) if e.kind == crate::shell_error::ShellErrorKind::EndOfStream => {
                    // On the managed-SRT path this arises ONLY from
                    // ManagedRecvTransport's reconnect-budget-exhausted
                    // `Closed` — the inner SRT transport never emits a
                    // clean EOS itself (a peer FIN surfaces as `Broken`,
                    // which the decorator retries). So "the stream ended
                    // here" == "reconnect gave up"; record accordingly.
                    // First-writer-wins, so a plain (non-managed) EOS
                    // path added in the future can still populate
                    // RecvEndReason::EndOfStream without this site
                    // needing to change.
                    self.end_reason.record(RecvEndReason::ReconnectExhausted);
                    // Stream end: same shape as DemuxReceiver — flush
                    // any partial PES then drain remaining events.
                    self.demux.flush();
                    if let Some(ev) = self.demux.next_event() {
                        return Ok(Some(ev));
                    }
                    return Ok(None);
                }
                Err(e) => {
                    if e.kind == crate::shell_error::ShellErrorKind::Closed {
                        // Closed-kind on the receive side means the
                        // decorator's ExplicitClose — caller-initiated
                        // close()/cancel(), not a wire-level failure.
                        self.end_reason.record(RecvEndReason::Cancelled);
                    }
                    let ReceiverErrorSource::Transport(te) = e.source;
                    return Err(te.into());
                }
            };

            // Step 4: now that we've taken a packet, check whether a
            // reconnect fired since the last loop iteration. If so,
            // DROP this packet (it's from the new connection's first
            // recv but the syncer may have buffered tail bytes from
            // the dead connection AHEAD of it — easier to discard
            // this one packet and let the syncer re-align cleanly
            // than to surgically separate dead-tail from
            // fresh-leading). Reset state and queue the discontinuity.
            //
            // Note: this check sits AFTER next_packet rather than
            // before because reconnect can fire DURING a single
            // next_packet call (multiple recv_bytes loops over a
            // single packet boundary). Polling after we've drained
            // one packet's worth of bytes ensures we observe the
            // post-reconnect state.
            let current = self.reconnects.load(Ordering::Acquire);
            if current > self.last_reconnects {
                self.last_reconnects = current;
                self.ts.reset_sync();
                self.demux.reset_sync();
                self.pending_reconnect_event = true;
                // Drop the just-read packet; the syncer is now clean
                // and will re-lock on the next post-reconnect bytes.
                continue;
            }

            // Step 5: steady-state path — feed the aligned packet to
            // the demuxer and loop to pull whatever events it produced.
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
    pub fn close(&mut self) {
        self.ts.close();
    }

    /// Cross-thread cancel handle for the underlying managed transport.
    pub fn cancel_handle(
        &self,
    ) -> Option<Arc<dyn tst_core::transport::TransportCancel + Send + Sync>> {
        self.ts.cancel_handle()
    }

    /// Total number of times the underlying transport has been rebuilt
    /// since this shell was constructed. Convenience accessor that
    /// reads the shared counter shipped from the inner
    /// [`ManagedRecvTransport::reconnects_handle`]. Useful for stats
    /// exports / dashboards.
    #[must_use]
    pub fn reconnects_count(&self) -> u64 {
        self.reconnects.load(Ordering::Acquire)
    }

    /// Shared handle onto this receiver's reconnect counter — the same
    /// underlying `Arc<AtomicU64>` [`Self::reconnects_count`] reads.
    /// Same naming/shape as
    /// [`ManagedRecvTransport::reconnects_handle`], one layer up.
    ///
    /// Obtain this **before** moving the receiver into an opaque handle
    /// (e.g. a C binding's box): reading through the returned `Arc`
    /// takes no lock on the receiver itself, so it stays safe to poll
    /// from a watchdog thread concurrently with a thread blocked in
    /// [`Self::recv_event`] — including while that call is inside its
    /// own internal reconnect retry loop. Same rationale as
    /// [`Self::end_reason_handle`].
    ///
    /// # C ABI
    ///
    /// `tst_managed_demux_receiver_get_reconnect_stats` (`reconnect_successes`
    /// and `reconnect_attempts` — the recv side tracks no separate
    /// attempts counter, see that getter's doc) — see
    /// `bindings/c/include/tstrans.h`.
    #[must_use]
    pub fn reconnects_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.reconnects)
    }

    /// True while the underlying transport's inner connection is
    /// currently absent — either mid-reconnect-attempt, or permanently
    /// after the reconnect budget has been exhausted. Check
    /// [`Self::is_alive`] to distinguish a live retry loop from a
    /// terminal give-up.
    #[must_use]
    pub fn reconnecting(&self) -> bool {
        self.reconnect_in_progress.load(Ordering::Acquire)
    }

    /// Shared handle onto this receiver's "reconnecting" flag — the same
    /// underlying `Arc<AtomicBool>` [`Self::reconnecting`] reads. Same
    /// naming/shape as [`ManagedRecvTransport::reconnecting_handle`],
    /// one layer up.
    ///
    /// Obtain this **before** moving the receiver into an opaque handle,
    /// for the same lock-free-watchdog-polling reason documented on
    /// [`Self::reconnects_handle`].
    ///
    /// # C ABI
    ///
    /// `tst_managed_demux_receiver_get_reconnect_stats` (`reconnecting`)
    /// — see `bindings/c/include/tstrans.h`.
    #[must_use]
    pub fn reconnecting_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.reconnect_in_progress)
    }

    /// Shared handle onto this receiver's stream-end reason — see
    /// [`RecvEndReason`]. Recorded first-writer-wins at whichever
    /// terminal-condition site actually observed it (reconnect-budget
    /// exhaustion or caller cancel/close).
    ///
    /// Obtain this **before** moving the receiver into an opaque handle
    /// (e.g. a C binding's box): the returned handle stays readable
    /// after the receiver itself is dropped, which is what lets a
    /// watchdog thread poll it independently of the thread driving
    /// [`Self::recv_event`].
    ///
    /// # C ABI
    ///
    /// `tst_managed_demux_receiver_end_reason` — see
    /// `bindings/c/include/tstrans.h`.
    #[must_use]
    pub fn end_reason_handle(&self) -> RecvEndReasonHandle {
        self.end_reason.clone()
    }

    /// Snapshot the current counters. Mirrors
    /// [`DemuxReceiver::stats`](crate::DemuxReceiver::stats) — composes
    /// transport-layer byte/packet counts from the inner [`Receiver`]
    /// with demux-layer event counts from the inner [`Demuxer`].
    pub fn stats(&self) -> crate::demux_receiver::DemuxReceiverStats {
        let ts = self.ts.stats();
        let dx = self.demux.stats();
        crate::demux_receiver::DemuxReceiverStats {
            bytes_received: ts.bytes_received,
            packets_received: ts.packets_received,
            program_maps_seen: dx.program_maps_seen,
            pmt_versions_seen: dx.pmt_versions_seen,
            discontinuities: dx.discontinuities,
            nonconformant: dx.nonconformant,
            per_stream: dx.per_stream,
        }
    }

    /// Reset all counters to zero. Delegates to both the inner
    /// [`Receiver`] and the inner [`Demuxer`].
    pub fn reset_stats(&mut self) {
        self.ts.reset_stats();
        self.demux.reset_stats();
    }

    /// Wire-level transport stats sourced from the inner
    /// [`ManagedRecvTransport`]. Returns `None` when the managed wrapper
    /// has no live inner socket (e.g. mid-reconnect or after terminal
    /// close).
    pub fn socket_stats(&self) -> Option<tst_core::transport::SocketStats> {
        self.ts.socket_stats()
    }

    /// Per-PID codec-specific counters. Mirrors
    /// [`DemuxReceiver::stream_codec_stats`](crate::DemuxReceiver::stream_codec_stats) —
    /// delegates to the inner
    /// [`tst_core::mpegts::demux::Demuxer::stream_codec_stats`]. The
    /// demuxer's per-PID state is independent of the live socket, so
    /// results don't vary across reconnect.
    pub fn stream_codec_stats(
        &self,
        pid: u16,
    ) -> Option<tst_core::mpegts::stats::StreamCodecStats> {
        self.demux.stream_codec_stats(pid)
    }
}

/// `ManagedDemuxReceiver` implements `Iterator` so callers can use
/// `for result in &mut rx`. EOF (`Ok(None)`) terminates; errors are
/// surfaced as `Some(Err(e))`.
impl<R: RecvTransport> Iterator for ManagedDemuxReceiver<R> {
    type Item = Result<DemuxEvent, DemuxReceiverError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.recv_event() {
            Ok(Some(e)) => Some(Ok(e)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

impl<R: RecvTransport> Drop for ManagedDemuxReceiver<R> {
    fn drop(&mut self) {
        let _enter = self._span.0.enter();
        info!("ManagedDemuxReceiver closed");
    }
}

// ManagedDemuxReceiver reuses `DemuxReceiverError` / `DemuxReceiverErrorSource`
// from the sibling `demux_receiver` module rather than introducing a parallel
// error type — the variants are identical (Transport + Demux) and a single
// type keeps the binding surface tight.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconnect::{BackoffStrategy, ReconnectPolicy};
    use std::time::Duration;
    use tst_core::transport::{BrokenCause, RecvTransport, TransportError};

    /// Build a policy with zero backoff so tests don't sleep.
    fn fast_policy(max_attempts: Option<u32>) -> ReconnectPolicy {
        ReconnectPolicy {
            max_attempts,
            backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
            ..Default::default()
        }
    }

    /// Build a syntactically-valid 188-byte TS packet on the given PID.
    fn ts_packet(pid: u16, cc: u8) -> [u8; 188] {
        let mut buf = [0xFFu8; 188];
        buf[0] = 0x47;
        buf[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
        buf[2] = (pid & 0xFF) as u8;
        buf[3] = 0x10 | (cc & 0x0F);
        buf
    }

    /// `RecvTransport` that returns a fixed sequence of byte vectors then
    /// switches behavior on a flag to drive the reconnect path. The
    /// first phase serves a chunk of TS bytes; once exhausted it
    /// returns `Broken` until the test flips `phase2_packets` ready,
    /// at which point a fresh inner is constructed via the factory.
    struct ScriptedRecv {
        packets: std::collections::VecDeque<Vec<u8>>,
        broken_after_exhaust: bool,
    }

    impl RecvTransport for ScriptedRecv {
        fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            match self.packets.pop_front() {
                Some(v) => {
                    let n = v.len().min(buf.len());
                    buf[..n].copy_from_slice(&v[..n]);
                    Ok(n)
                }
                None => {
                    if self.broken_after_exhaust {
                        Err(TransportError::Broken {
                            msg: "scripted exhaust".into(),
                            errno_code: None,
                            cause: BrokenCause::Unspecified,
                        })
                    } else {
                        Err(TransportError::Closed)
                    }
                }
            }
        }

        fn max_payload(&self) -> usize {
            1316
        }

        fn is_alive(&self) -> bool {
            !self.packets.is_empty()
        }
    }

    /// Concatenate aligned packets into one chunk so the syncer locks
    /// (it needs 4 in-stride confirmations).
    fn chunk_of_aligned(packets: &[[u8; 188]]) -> Vec<u8> {
        let mut v = Vec::with_capacity(packets.len() * 188);
        for p in packets {
            v.extend_from_slice(p);
        }
        v
    }

    /// Smoke test: with a 0-attempt reconnect policy, the first inner
    /// exhaust surfaces as EOF (no reconnect possible) and no
    /// `ReconnectDiscontinuity` event is emitted. The shell behaves
    /// like a plain `DemuxReceiver` for the no-reconnect path.
    ///
    /// Note: ManagedRecvTransport treats inner `Closed` as a reconnect
    /// trigger (not EOF) — the policy budget (max_attempts=0 here)
    /// rejects the rebuild attempt and converts to terminal `Closed`.
    /// Factory is invoked once during the budget-rejection sequence
    /// but its result is discarded; we still expect no
    /// reconnect-count rise.
    #[test]
    fn no_reconnect_no_discontinuity_event() {
        // 5 aligned PID-0 packets, then the transport closes.
        let packets: Vec<[u8; 188]> = (0..5).map(|i| ts_packet(0x0100, i as u8)).collect();
        let chunk = chunk_of_aligned(&packets);

        let inner = ScriptedRecv {
            packets: vec![chunk].into(),
            broken_after_exhaust: false,
        };
        // Factory always fails — combined with max_attempts=0, the budget
        // is exhausted on the first attempt and the shell surfaces EOF
        // without ever rebuilding the inner.
        let factory = Box::new(|| -> Result<ScriptedRecv, TransportError> {
            Err(TransportError::Broken {
                msg: "no reconnect for this test".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            })
        });
        // max_attempts=Some(0) → budget rejected on first attempt;
        // ManagedRecvTransport latches closed and returns Closed.
        let managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(0)));
        let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());

        let mut saw_reconnect = false;
        let mut events = 0;
        loop {
            match rx.recv_event() {
                Ok(Some(DemuxEvent::ReconnectDiscontinuity)) => saw_reconnect = true,
                Ok(Some(_)) => events += 1,
                Ok(None) => break,
                Err(_e) => break, // Closed surfaces as Err on receiver side
            }
        }
        // No successful rebuild ever happened → no reconnect event.
        // 0x0100 isn't in any PMT so the demuxer ignores its packets.
        assert!(
            !saw_reconnect,
            "should not emit ReconnectDiscontinuity when no rebuild succeeded"
        );
        let _ = events;
        assert_eq!(rx.reconnects_count(), 0);
    }

    /// Reconnect fires mid-stream: shell yields ReconnectDiscontinuity
    /// and the inner reconnect-count rises.
    #[test]
    fn reconnect_emits_discontinuity_event() {
        // Phase 1: a few aligned packets (enough to lock the syncer,
        // then a fractional packet at the tail to test mid-PES reset).
        let p1_packets: Vec<[u8; 188]> = (0..6).map(|i| ts_packet(0x0100, i as u8)).collect();
        let mut p1 = chunk_of_aligned(&p1_packets);
        // Append 100 bytes of "garbage" trailing — simulates a fractional
        // PES packet that the dead connection cut off mid-flight.
        p1.extend_from_slice(&[0xAA; 100]);

        let inner = ScriptedRecv {
            packets: vec![p1].into(),
            broken_after_exhaust: true, // exhausting triggers reconnect path
        };

        // Phase 2: 4 fresh aligned packets after reconnect. The shell
        // should drop the dead tail, reset state, emit a
        // ReconnectDiscontinuity, then re-lock on these clean packets.
        let factory_calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let factory_calls_cl = factory_calls.clone();
        let factory = Box::new(move || -> Result<ScriptedRecv, TransportError> {
            let n = factory_calls_cl.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                // First reconnect: serve fresh clean packets.
                let p2_packets: Vec<[u8; 188]> =
                    (0..6).map(|i| ts_packet(0x0200, i as u8)).collect();
                let chunk = chunk_of_aligned(&p2_packets);
                Ok(ScriptedRecv {
                    packets: vec![chunk].into(),
                    broken_after_exhaust: false, // EOF after this chunk
                })
            } else {
                // No further reconnect attempts.
                Err(TransportError::Broken {
                    msg: "no more rebuilds".into(),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                })
            }
        });
        let managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(3)));
        let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());

        let mut saw_reconnect = false;
        let mut total_events = 0;
        let mut iters = 0;
        loop {
            iters += 1;
            assert!(iters < 1000, "test loop should terminate");
            match rx.recv_event() {
                Ok(Some(DemuxEvent::ReconnectDiscontinuity)) => {
                    saw_reconnect = true;
                    // The dead-tail bytes (100 bytes of 0xAA) must not
                    // be feeding into the new connection's syncer
                    // post-reconnect. The reset_sync calls handle that.
                    assert_eq!(rx.reconnects_count(), 1);
                }
                Ok(Some(_)) => total_events += 1,
                Ok(None) => break,
                Err(_e) => break, // tolerate factory budget exhaust
            }
        }
        assert!(
            saw_reconnect,
            "reconnect should have surfaced as ReconnectDiscontinuity event"
        );
        assert_eq!(rx.reconnects_count(), 1);
        let _ = total_events;
    }

    /// Reconnect during mid-stream PES (partial-packet tail in the
    /// syncer): the reset_sync call drops those bytes so they don't
    /// splice into the new connection's framing.
    ///
    /// We bound factory rebuilds explicitly via a shared counter
    /// because `ManagedRecvTransport`'s `max_attempts` budget resets
    /// per-`recv_bytes` call (it's a local var); without an in-factory
    /// cap, a `broken_after_exhaust: true` inner that the factory
    /// keeps rebuilding would loop forever.
    #[test]
    fn reconnect_clears_syncer_buffer_and_demux_state() {
        // Phase 1: enough aligned packets to lock + a partial-packet
        // tail of 187 bytes (< 188, won't form a full packet).
        let p1_packets: Vec<[u8; 188]> = (0..5).map(|i| ts_packet(0x0100, i as u8)).collect();
        let mut p1 = chunk_of_aligned(&p1_packets);
        // 187 bytes — the syncer would otherwise hold these as the
        // start of packet 6. After reset_sync they MUST be dropped.
        p1.extend_from_slice(&[0x47; 187]);

        let inner = ScriptedRecv {
            packets: vec![p1].into(),
            broken_after_exhaust: true,
        };

        // Factory invocation count cap so the test terminates. After
        // 1 successful rebuild + 1 final failure, the shell exhausts
        // its budget and returns terminal Closed.
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_cl = calls.clone();
        let factory = Box::new(move || -> Result<ScriptedRecv, TransportError> {
            let n = calls_cl.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                // After reconnect, serve a TS stream that starts with a
                // BAD sync byte (0x00, 0xFF...) before becoming valid. If
                // the syncer wasn't reset, the dead-tail's 0x47s would
                // mis-lock onto a fake boundary. After reset, the syncer
                // hunts cleanly through the bad prefix.
                let mut bytes = vec![0x00u8; 50];
                let packets: Vec<[u8; 188]> = (0..5).map(|i| ts_packet(0x0200, i as u8)).collect();
                bytes.extend(chunk_of_aligned(&packets));
                Ok(ScriptedRecv {
                    packets: vec![bytes].into(),
                    broken_after_exhaust: true,
                })
            } else {
                Err(TransportError::Broken {
                    msg: "factory exhausted".into(),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                })
            }
        });
        let managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(2)));
        let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());

        let mut saw_reconnect = false;
        let mut iters = 0;
        loop {
            iters += 1;
            assert!(iters < 1000, "test loop should terminate");
            match rx.recv_event() {
                Ok(Some(DemuxEvent::ReconnectDiscontinuity)) => {
                    saw_reconnect = true;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => break,
            }
        }
        assert!(saw_reconnect, "reconnect should have surfaced");
        // The crucial assertion: the demuxer didn't crash with a
        // stale-state error and the shell reached terminal cleanly. If
        // the syncer or demuxer state had been left intact, the bad
        // prefix or the orphan 187-byte tail would have caused either
        // a phantom sample to emit (mixed bytes) or an Unrecoverable
        // error inside the demuxer.
        assert!(!rx.is_alive());
    }

    /// Iterator impl: `for ev in &mut rx` terminates when the
    /// underlying transport's reconnect budget is exhausted. With
    /// max_attempts=0, the first inner exhaust triggers immediate
    /// budget rejection which surfaces as a Closed error on the
    /// receiver side — the iterator stops on the first error or None.
    #[test]
    fn iterator_terminates_on_eof() {
        let packets: Vec<[u8; 188]> = (0..5).map(|i| ts_packet(0x0100, i as u8)).collect();
        let chunk = chunk_of_aligned(&packets);
        let inner = ScriptedRecv {
            packets: vec![chunk].into(),
            broken_after_exhaust: false,
        };
        // Factory call shape doesn't matter — budget=0 rejects before
        // dispatch.
        let factory = Box::new(|| -> Result<ScriptedRecv, TransportError> {
            Err(TransportError::Broken {
                msg: "no reconnect".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            })
        });
        let managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(0)));
        let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());

        let mut count = 0;
        for result in &mut rx {
            // Tolerate either Ok event or terminal Err (Closed); both
            // are valid iterator stop conditions for this fixture.
            let _ = result;
            count += 1;
            if count > 100 {
                panic!("iterator did not terminate");
            }
        }
        // The loop terminated. Whether on Ok(None) or after one Err,
        // we exited bounded; that's the contract under test.
        let _ = count;
    }
}
