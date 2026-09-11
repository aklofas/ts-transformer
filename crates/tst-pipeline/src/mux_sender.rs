//! `MuxSender<T: Transport>` — composes `mpegts::mux::Muxer` with a
//! `Transport` for the canonical NAL+KLV → TS → SRT send path.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Internally synchronized: `send_video` and `send_klv` may be called
//! from different threads concurrently. The lock is held across push →
//! mux drain → transport send for correct back-pressure.
//!
//! Lossless on transient transport errors: drained-but-not-yet-sent
//! bytes are retained in `pending_bytes` and drained first on the next
//! call. Only catastrophic transport failures (Broken/Closed) are
//! propagated to the caller; those are the cases where `ManagedTransport`
//! is the right wrapper.
//!
//! # Input consumption and retry
//!
//! Every error reports whether this call's input was consumed via
//! [`MuxSenderError::input_consumed`]:
//!
//! - `Some(false)` — input NOT consumed (muxer state unchanged): a
//!   mux-side rejection, a closed transport, or a failure while draining
//!   bytes retained by a PREVIOUS call. Retrying the same input after
//!   fixing the cause cannot duplicate data.
//! - `Some(true)` — input consumed: muxed (continuity counters advanced)
//!   and retained in the pending queue, which drains first on the next
//!   `send_*` call, exactly once, in order. Do NOT push the same input
//!   again — the stream would carry duplicate access units.
//! - `None` — not a `send_*` input-path error (e.g. poisoned lock).
//!
//! ```
//! # use tst_pipeline::{MuxSender, MuxSenderError};
//! # fn retry_policy<T: tst_core::transport::Transport>(
//! #     sender: &MuxSender<T>, nal: &[u8], pts: tst_core::mpegts::common::Pts90khz,
//! # ) {
//! match sender.send_video(nal, pts, true) {
//!     Ok(()) => {}
//!     Err(e) if e.input_consumed == Some(false) => {
//!         // safe to retry the same input after fixing the cause
//!     }
//!     Err(_) => {
//!         // consumed (or indeterminate): do not resend this input;
//!         // pending bytes drain on the next send_* call
//!     }
//! }
//! # }
//! ```
//!
//! Callers that must not drop access units across repeated transport
//! failures should wrap the transport in [`crate::ManagedTransport`]
//! rather than hand-rolling recovery on the bare shell.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use tracing::info_span;
use tst_core::error::MuxError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    AudioStreamHandle, DataStreamHandle, KlvStreamHandle, Muxer, MuxerConfig, SubtitleStreamHandle,
    VideoStreamHandle,
};
use tst_core::transport::{BrokenCause, Transport, TransportError};

use crate::mutex::ShellMutex;
use crate::shell_error::ShellErrorKind;

/// Stats snapshot for [`MuxSender`].
#[must_use]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MuxSenderStats {
    /// Cumulative bytes successfully handed off to the transport.
    pub bytes_sent: u64,
    /// Cumulative chunk count successfully handed off to the transport.
    /// Each chunk is one `transport.send_bytes` call that returned `Ok`.
    pub packets_sent: u64,
    /// Live gauge — bytes currently buffered in `pending_bytes` after
    /// a transport flap. NOT a counter; reflects current state.
    pub pending_bytes_queued: u64,
    /// Live gauge — chunk count currently in the pending buffer.
    pub pending_chunks_queued: u64,
    /// Number of programs (PAT entries) in the muxer configuration.
    /// Delegated from the inner `MuxerStats`.
    pub programs_configured: u32,
    /// Per-stream push counters, keyed by PID. Delegated from the wrapped
    /// `Muxer`; not double-booked here.
    pub per_stream: BTreeMap<u16, tst_core::mpegts::stats::StreamStats>,
}

/// Composes [`Muxer`] with a [`Transport`] for the canonical NAL+KLV → TS →
/// transport send path. See the module docs for shape and back-pressure
/// behavior.
///
/// # Panics
///
/// No method panics on a poisoned inner mutex. If a prior call panicked
/// mid-mutation and poisoned the lock, each method falls back gracefully:
/// fallible methods (`send_*`, `*_handles_for_program`) return a typed
/// [`MuxSenderError`] with kind [`ShellErrorKind::TransportBroken`] (or
/// [`MuxError::ProgramNotFound`]); infallible methods (`*_handles`,
/// `stats`, `socket_stats`, `stream_codec_stats`, `reset_stats`,
/// `is_alive`) return the corresponding safe default (`Vec::new()`,
/// `MuxSenderStats::default()`, `None`, silent no-op, `false`). `close`
/// and `Drop` already used `if let Ok` before this policy was formalized.
///
/// # Closing
///
/// `MuxSender` is `Send + Sync` (when `T: Transport + Send + Sync`) and
/// supports four shutdown patterns:
///
/// 1. **Drop** — the [`Drop`] impl best-effort drains `pending_bytes` and
///    closes the underlying transport. Synchronous; bounded by
///    `SRTO_LINGER` (libsrt default 30 s, configurable via
///    `SocketBuilder::linger`).
/// 2. **Explicit prompt close** — call [`Self::close`]. Cancels the
///    transport *before* taking the inner lock, so a peer thread parked
///    in `send_video` / `send_klv` returns
///    [`MuxSenderErrorSource::Transport`]`(`[`TransportError::Broken`]`)` within
///    one libsrt I/O cycle (~3-10 ms). Idempotent. Prompt by
///    construction — the price is that `pending_bytes` retained from a
///    prior transient send error are abandoned.
/// 3. **Graceful finish** — call [`Self::finish`]. Drains
///    `pending_bytes` to the still-live transport FIRST (fallible — the
///    caller learns whether the tail was delivered), then closes. May
///    block like `Drop`; a watchdog with [`Self::cancel_handle`] can
///    unblock it.
/// 4. **Cross-thread cancel** — call [`Self::cancel_handle`] to obtain a
///    `Send + Sync` [`tst_core::transport::TransportCancel`] handle,
///    then `cancel()` from any thread. Wakes a parked send without
///    closing the `MuxSender` itself; equivalent to what `close()`
///    fires internally.
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
/// | C | `tst_mux_sender_close(sender)` (explicit; mirrors [`Self::close`] — prompt, abandons the retained tail; no C mirror of [`Self::finish`] yet) |
///
/// See [`docs/reference/srt-cancel-handle.md`](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/srt-cancel-handle.md) for the full cancel-handle pattern.
///
/// # `no_std` concurrency
///
/// Under `--no-default-features` (`no_std`) the inner lock is a
/// `spin::Mutex` with no priority inheritance and no interrupt masking.
/// **Drive each `MuxSender` from a single task** (one-sender-per-task).
/// The type still implements `Sync`, but sharing one instance across
/// preemptive tasks (e.g. FreeRTOS tasks at different priorities) can
/// livelock: a higher-priority task spins on the lock while the preempted
/// holder never runs. Under `std` this section does not apply — the lock
/// is `std::sync::Mutex` and cross-thread sharing is fully supported.
pub struct MuxSender<T: Transport> {
    inner: ShellMutex<Inner<T>>,
    /// Cancel handle snapshot, taken from the transport at construction
    /// time. Held outside the inner Mutex so `close()` can fire it
    /// without competing with a concurrent `send_*` for the lock.
    cancel: Option<Arc<dyn tst_core::transport::TransportCancel + Send + Sync>>,
    /// Lifetime span — see [`crate::shell_error::ShellSpan`] for the
    /// unwind-safe rationale. Private; never exposed publicly.
    _span: crate::shell_error::ShellSpan,
}

impl<T: Transport> core::fmt::Debug for MuxSender<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Acquire the inner Mutex briefly to read identity + lifecycle.
        // If poisoned (a panic happened mid-send), report poisoned-state
        // rather than panicking the formatter.
        match self.inner.lock() {
            Ok(inner) => f
                .debug_struct("MuxSender")
                .field("closed", &inner.closed)
                .field("video_streams", &inner.muxer.video_handles().len())
                .field("klv_streams", &inner.muxer.klv_handles().len())
                .field("audio_streams", &inner.muxer.audio_handles().len())
                .field("subtitle_streams", &inner.muxer.subtitle_handles().len())
                .field("pending_chunks", &inner.pending_bytes.len())
                .field("transport_kind", &core::any::type_name::<T>())
                .finish(),
            Err(_) => f
                .debug_struct("MuxSender")
                .field("inner", &"<poisoned>")
                .field("transport_kind", &core::any::type_name::<T>())
                .finish(),
        }
    }
}

struct Inner<T: Transport> {
    muxer: Muxer,
    transport: T,
    /// Drained-but-not-yet-sent TS chunks, oldest first. Drained on each
    /// send_* call before any new push.
    ///
    /// Unbounded across repeated transport failures — the bare `MuxSender`
    /// has no cap. Callers expecting prolonged transport unavailability
    /// should wrap with `ManagedTransport`, which adds a gap-buffer with
    /// overflow policy.
    pending_bytes: VecDeque<Vec<u8>>,
    closed: bool,
    bytes_sent: u64,
    packets_sent: u64,
    /// Last back-pressure state sampled by `maybe_warn_backpressure`.
    /// Used to fire `tracing::warn!` only on threshold-crossing
    /// (Ok→Warn or Warn→Overflow), not on every `send_*` call. Recovery
    /// transitions (Warn→Ok / Overflow→Warn) are silent.
    last_backpressure_state: BackpressureState,
    /// Reusable scratch buffer for `drain_muxer`. Sized to
    /// `transport.max_payload()` and grown lazily. Avoids a fresh
    /// heap allocation on every muxer drain call.
    scratch: Vec<u8>,
}

/// Back-pressure tier on the muxer's internal packet queue. Ordering
/// matters: `Ok < Warn < Overflow`, so a strictly-greater comparison
/// (`new > last`) gives the threshold-crossing semantics that suppress
/// log spam at high pps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BackpressureState {
    /// `pending / cap < 0.8`.
    Ok,
    /// `0.8 <= pending / cap < 1.0` — one warn fires on entry.
    Warn,
    /// `pending / cap >= 1.0` — one warn fires on entry; the next
    /// `push_*` will return `MuxError::BufferFull`.
    Overflow,
}

/// Build the poisoned-lock error for a named `send_*` site.
///
/// Called when `self.inner.lock()` returns `Err` (the Mutex is poisoned
/// because a previous `send_*` call panicked mid-mutation). Routes to
/// `MuxSenderError` with kind `TransportBroken` and a site-specific message
/// so the C ABI surfaces a useful diagnostic via `tst_get_last_error_str()`.
fn lock_poisoned(site: &'static str) -> MuxSenderError {
    MuxSenderError::from(TransportError::Broken {
        msg: alloc::format!("mux_sender: inner lock poisoned during {site}"),
        errno_code: None,
        cause: BrokenCause::Unspecified,
    })
}

impl<T: Transport> MuxSender<T> {
    pub fn new(transport: T, config: MuxerConfig) -> Result<Self, MuxError> {
        let span = info_span!(
            target: "tst_pipeline::mux_sender",
            "mux_sender",
            program_count = config.programs.len(),
            transport_kind = core::any::type_name::<T>(),
        );
        let _enter = span.enter();
        let muxer = Muxer::new(config)?;
        let cancel = transport.cancel_handle();
        tracing::info!("MuxSender opened");
        drop(_enter);
        Ok(Self {
            inner: ShellMutex::new(Inner {
                muxer,
                transport,
                pending_bytes: VecDeque::new(),
                closed: false,
                bytes_sent: 0,
                packets_sent: 0,
                last_backpressure_state: BackpressureState::Ok,
                scratch: Vec::new(),
            }),
            cancel,
            _span: core::panic::AssertUnwindSafe(span),
        })
    }

    /// Send one video access unit. Annex-B framing is required.
    /// `pts` is in 90 kHz ticks (the TS clock); `key_frame` should
    /// be true for IDR.
    ///
    /// Resolves only when exactly one video stream is configured; with
    /// multiple video streams the muxer surfaces
    /// [`MuxError::AmbiguousTarget`] inside [`MuxSenderErrorSource::Mux`] —
    /// use [`Self::send_video_to`] in that case.
    ///
    /// # C ABI
    ///
    /// `tst_mux_sender_send_video` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Typed PTS
    ///
    /// `pts: Pts90khz` is a newtype around the raw 90 kHz tick count. Construct
    /// from raw ticks with [`Pts90khz::new`] or from milliseconds with
    /// [`Pts90khz::from_millis`]. Internal arithmetic across the workspace still
    /// uses raw `i64`; a deferred item tracked in `docs/project/deferred-features.md`
    /// will design wrap-vs-saturate semantics on `Pts90khz` and do the full
    /// internal sweep.
    ///
    /// [`Pts90khz::new`]: tst_core::mpegts::common::Pts90khz::new
    /// [`Pts90khz::from_millis`]: tst_core::mpegts::common::Pts90khz::from_millis
    ///
    /// # Errors
    /// - [`MuxSenderErrorSource::Mux`] wraps [`MuxError`] from the underlying
    ///   muxer (e.g. `AmbiguousTarget` when more than one video stream
    ///   is configured, `InvalidStreamHandle` from `send_video_to`).
    /// - [`MuxSenderErrorSource::Transport`] wraps a [`TransportError`]; on
    ///   transport flap the unsent TS chunks are retained for a later
    ///   `send_*` call to drain.
    ///   Whether the input was consumed depends on the failure point — see
    ///   the module-level *Input consumption and retry* section before
    ///   deciding whether to resend.
    ///
    /// # Example
    /// ```
    /// use tst_pipeline::MuxSender;
    /// use tst_core::mpegts::common::Pts90khz;
    /// use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
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
    /// let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    /// prog.add_video(0x1011, VideoCodec::H264);
    /// let mut b = MuxerConfig::builder();
    /// b.add_program(prog.build());
    /// let cfg = b.build()?;
    /// let sender = MuxSender::new(Sink(Vec::new()), cfg)?;
    ///
    /// // Minimal Annex-B H.264 IDR NAL (start code + nal_unit_type=5).
    /// let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
    /// sender.send_video(&nal, Pts90khz::new(0), true)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn send_video(
        &self,
        nal: &[u8],
        pts: Pts90khz,
        key_frame: bool,
    ) -> Result<(), MuxSenderError> {
        // Mutex-poisoning policy (recoverable path): poisoned inner lock means a
        // previous panic happened mid-mutation. `lock_poisoned` routes to
        // `MuxSenderError` with kind `TransportBroken` and a site-specific
        // message so the C ABI surfaces a useful diagnostic via
        // `tst_get_last_error_str()`. Precedent: the gap-buffer policy in
        // `ManagedTransport::send_managed` / `drain_gap_if_alive` in reconnect/mod.rs.
        let mut inner = self.inner.lock().map_err(|_| lock_poisoned("send_video"))?;
        inner.send_video(nal, pts.as_ticks(), key_frame)
    }

    /// Send one pre-built KLV blob. `pts` is in 90 kHz units (the
    /// TS clock); ignored unless the configured KLV stream carries PTS.
    ///
    /// `metadata_service_id` is written into the AU cell header per
    /// ITU-T H.222.0 V9 §2.12.4.2 / ST 1402.2 App. B Table 2 only for
    /// [`tst_core::mpegts::mux::KlvStreamType::SynchronousMetadata`] streams;
    /// ignored on [`tst_core::mpegts::mux::KlvStreamType::PrivateData`]
    /// streams. The spec default is `0x00`.
    ///
    /// # C ABI
    ///
    /// `tst_mux_sender_send_klv` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// - [`MuxSenderErrorSource::Mux`] wraps [`MuxError`] from the inner muxer:
    ///   [`MuxError::NoKlvStreamsConfigured`] if no KLV streams exist;
    ///   [`MuxError::AmbiguousTarget`] when more than one is configured
    ///   (use [`Self::send_klv_to`]); [`MuxError::KlvTooLarge`] if the
    ///   blob would overflow `PES_packet_length`;
    ///   [`MuxError::BufferFull`] if the muxer's outbound queue is at
    ///   `MuxerConfig::buffer_packets`.
    /// - [`MuxSenderErrorSource::Transport`] wraps a [`TransportError`]; on
    ///   transport flap the unsent TS chunks are retained for a later
    ///   `send_*` call to drain.
    ///   Whether the input was consumed depends on the failure point — see
    ///   the module-level *Input consumption and retry* section before
    ///   deciding whether to resend.
    pub fn send_klv(
        &self,
        klv: &[u8],
        pts: Pts90khz,
        metadata_service_id: u8,
    ) -> Result<(), MuxSenderError> {
        // Mutex-poisoning policy — see send_video for rationale.
        let mut inner = self.inner.lock().map_err(|_| lock_poisoned("send_klv"))?;
        inner.send_klv(klv, pts.as_ticks(), metadata_service_id)
    }

    /// Send one video access unit to a specific configured video stream.
    /// `handle` is obtained from [`Self::video_handles`]; passing a handle
    /// from a different sender / muxer surfaces as
    /// [`MuxError::InvalidStreamHandle`] inside [`MuxSenderErrorSource::Mux`].
    ///
    /// # C ABI
    ///
    /// `tst_mux_sender_send_video_to` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// - [`MuxSenderErrorSource::Mux`] wraps [`MuxError`] from the inner muxer:
    ///   [`MuxError::InvalidStreamHandle`] if `handle`'s index is out of
    ///   range for this muxer's video streams; [`MuxError::InvalidNal`]
    ///   if `nal` does not begin with an Annex-B start code (H.264 /
    ///   H.265 / H.266 only — AV1 OBU payloads skip this check);
    ///   [`MuxError::BufferFull`] if the muxer's outbound queue is at
    ///   `MuxerConfig::buffer_packets`.
    /// - [`MuxSenderErrorSource::Transport`] wraps a [`TransportError`]; on
    ///   transport flap the unsent TS chunks are retained for a later
    ///   `send_*` call to drain.
    ///   Whether the input was consumed depends on the failure point — see
    ///   the module-level *Input consumption and retry* section before
    ///   deciding whether to resend.
    pub fn send_video_to(
        &self,
        handle: VideoStreamHandle,
        nal: &[u8],
        pts: Pts90khz,
        key_frame: bool,
    ) -> Result<(), MuxSenderError> {
        // Mutex-poisoning policy — see send_video for rationale.
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| lock_poisoned("send_video_to"))?;
        inner.send_video_to(handle, nal, pts.as_ticks(), key_frame)
    }

    /// Send one access unit with explicit composition (PTS) and decode (DTS)
    /// timestamps. Required for reordered codecs (H.264/H.265/H.266/AV1
    /// streams with B-frames).
    ///
    /// Mirrors [`tst_core::mpegts::mux::Muxer::push_video_to_with_dts`]:
    /// the muxer emits PES with `PTS_DTS_flags = '11'` per
    /// ISO/IEC 13818-1 §2.4.3.6, carrying both timestamps. When
    /// `pts == dts`, prefer [`Self::send_video_to`] for the smaller
    /// 5-byte PTS-only PES encoding.
    ///
    /// **Caller invariant:** `dts <= pts` per §2.4.3.6. The muxer does
    /// not enforce this; receivers will reject inverted timestamps.
    ///
    /// # C ABI
    ///
    /// Not yet exposed via the C ABI. Callers needing B-frame support
    /// from C should bridge through the Rust API or open an issue
    /// requesting the C entry.
    ///
    /// # Errors
    /// - [`MuxSenderErrorSource::Mux`] wraps [`MuxError`] from the inner
    ///   muxer (same variants as [`Self::send_video_to`]).
    /// - [`MuxSenderErrorSource::Transport`] wraps a [`TransportError`]; on
    ///   transport flap the unsent TS chunks are retained for a later
    ///   `send_*` call to drain.
    ///   Whether the input was consumed depends on the failure point — see
    ///   the module-level *Input consumption and retry* section before
    ///   deciding whether to resend.
    pub fn send_video_to_with_dts(
        &self,
        handle: VideoStreamHandle,
        nal: &[u8],
        pts: Pts90khz,
        dts: Pts90khz,
        key_frame: bool,
    ) -> Result<(), MuxSenderError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| lock_poisoned("send_video_to_with_dts"))?;
        inner.send_video_to_with_dts(handle, nal, pts.as_ticks(), dts.as_ticks(), key_frame)
    }

    /// Send one video access unit with an ST 0604 MISP timestamp SEI spliced
    /// before the first VCL NAL. Annex-B framing is required.
    ///
    /// Splices the ST 0604 MISP SEI — see `Muxer::push_video_misp_to`.
    /// See [`Self::send_video_to`] for the input-consumption/retry contract.
    ///
    /// # C ABI — not exposed
    ///
    /// Not yet exposed via the C ABI. Open an issue if C support is needed.
    ///
    /// # Errors
    /// - [`MuxSenderErrorSource::Mux`] wraps [`MuxError`] from the inner
    ///   muxer (same variants as [`Self::send_video_to`], plus
    ///   [`tst_core::error::MuxError::MispTime`] when the SEI cannot be built).
    /// - [`MuxSenderErrorSource::Transport`] wraps a [`TransportError`]; on
    ///   transport flap the unsent TS chunks are retained for a later
    ///   `send_*` call to drain.
    ///   Whether the input was consumed depends on the failure point — see
    ///   the module-level *Input consumption and retry* section before
    ///   deciding whether to resend.
    pub fn send_video_misp_to(
        &self,
        handle: VideoStreamHandle,
        nal: &[u8],
        pts: Pts90khz,
        key_frame: bool,
        misp: &tst_core::codec::misp_time::MispTimestamp,
    ) -> Result<(), MuxSenderError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| lock_poisoned("send_video_misp_to"))?;
        inner.send_video_misp_to(handle, nal, pts.as_ticks(), key_frame, misp)
    }

    /// PTS+DTS variant of [`Self::send_video_misp_to`] for reordered codecs.
    ///
    /// Splices the ST 0604 MISP SEI — see `Muxer::push_video_misp_to`.
    /// See [`Self::send_video_to_with_dts`] for the DTS contract and the
    /// input-consumption/retry pointer.
    ///
    /// **Caller invariant:** `dts <= pts` per ISO/IEC 13818-1 §2.4.3.6.
    ///
    /// # C ABI — not exposed
    ///
    /// Not yet exposed via the C ABI. Open an issue if C support is needed.
    ///
    /// # Errors
    /// - [`MuxSenderErrorSource::Mux`] wraps [`MuxError`] from the inner
    ///   muxer (same variants as [`Self::send_video_to`], plus
    ///   [`tst_core::error::MuxError::MispTime`] when the SEI cannot be built).
    /// - [`MuxSenderErrorSource::Transport`] wraps a [`TransportError`]; on
    ///   transport flap the unsent TS chunks are retained for a later
    ///   `send_*` call to drain.
    ///   Whether the input was consumed depends on the failure point — see
    ///   the module-level *Input consumption and retry* section before
    ///   deciding whether to resend.
    pub fn send_video_misp_to_with_dts(
        &self,
        handle: VideoStreamHandle,
        nal: &[u8],
        pts: Pts90khz,
        dts: Pts90khz,
        key_frame: bool,
        misp: &tst_core::codec::misp_time::MispTimestamp,
    ) -> Result<(), MuxSenderError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| lock_poisoned("send_video_misp_to_with_dts"))?;
        inner.send_video_misp_to_with_dts(
            handle,
            nal,
            pts.as_ticks(),
            dts.as_ticks(),
            key_frame,
            misp,
        )
    }

    /// Send one KLV blob to a specific configured KLV stream.
    ///
    /// `metadata_service_id` is written into the AU cell header per
    /// ITU-T H.222.0 V9 §2.12.4.2 / ST 1402.2 App. B Table 2 only for
    /// [`tst_core::mpegts::mux::KlvStreamType::SynchronousMetadata`] streams;
    /// ignored on [`tst_core::mpegts::mux::KlvStreamType::PrivateData`]
    /// streams. The spec default is `0x00`.
    ///
    /// # C ABI
    ///
    /// `tst_mux_sender_send_klv_to` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// - [`MuxSenderErrorSource::Mux`] wraps [`MuxError`] from the inner muxer:
    ///   [`MuxError::InvalidStreamHandle`] if `handle`'s index is out of
    ///   range for this muxer's KLV streams; [`MuxError::KlvTooLarge`]
    ///   if the blob would overflow `PES_packet_length` (with a 5-byte
    ///   AU cell header reservation for `SynchronousMetadata` streams);
    ///   [`MuxError::BufferFull`] if the muxer's outbound queue is at
    ///   `MuxerConfig::buffer_packets`.
    /// - [`MuxSenderErrorSource::Transport`] wraps a [`TransportError`]; on
    ///   transport flap the unsent TS chunks are retained for a later
    ///   `send_*` call to drain.
    ///   Whether the input was consumed depends on the failure point — see
    ///   the module-level *Input consumption and retry* section before
    ///   deciding whether to resend.
    pub fn send_klv_to(
        &self,
        handle: KlvStreamHandle,
        klv: &[u8],
        pts: Pts90khz,
        metadata_service_id: u8,
    ) -> Result<(), MuxSenderError> {
        // Mutex-poisoning policy — see send_video for rationale.
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| lock_poisoned("send_klv_to"))?;
        inner.send_klv_to(handle, klv, pts.as_ticks(), metadata_service_id)
    }

    /// Send one audio frame buffer. `pts` is in 90 kHz ticks (the
    /// TS clock); audio always carries PTS (no DTS). `frames` is one or
    /// more pre-framed audio frames concatenated by the caller.
    ///
    /// Resolves only when exactly one audio stream is configured; with
    /// zero or multiple audio streams the muxer surfaces
    /// [`MuxError::AmbiguousTarget`] inside [`MuxSenderErrorSource::Mux`] — use
    /// [`Self::send_audio_to`] in that case.
    ///
    /// # C ABI
    ///
    /// `tst_mux_sender_send_audio` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// - [`MuxSenderErrorSource::Mux`] wraps [`MuxError`] from the inner muxer:
    ///   [`MuxError::NoAudioStreamsConfigured`] if no audio streams exist;
    ///   [`MuxError::AmbiguousTarget`] when more than one is configured;
    ///   [`MuxError::AudioTooLarge`] if `frames.len()` would overflow
    ///   `PES_packet_length`; [`MuxError::BufferFull`] if the muxer's
    ///   outbound queue is at `MuxerConfig::buffer_packets`.
    /// - [`MuxSenderErrorSource::Transport`] wraps a [`TransportError`]; on
    ///   transport flap the unsent TS chunks are retained for a later
    ///   `send_*` call to drain.
    ///   Whether the input was consumed depends on the failure point — see
    ///   the module-level *Input consumption and retry* section before
    ///   deciding whether to resend.
    pub fn send_audio(&self, frames: &[u8], pts: Pts90khz) -> Result<(), MuxSenderError> {
        // Mutex-poisoning policy — see send_video for rationale.
        let mut inner = self.inner.lock().map_err(|_| lock_poisoned("send_audio"))?;
        inner.send_audio(frames, pts.as_ticks())
    }

    /// Send one audio frame buffer to a specific configured audio stream.
    /// `handle` is obtained from [`Self::audio_handles`]; passing a handle
    /// from a different sender / muxer surfaces as
    /// [`MuxError::InvalidStreamHandle`] inside [`MuxSenderErrorSource::Mux`].
    ///
    /// # C ABI
    ///
    /// `tst_mux_sender_send_audio_to` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// - [`MuxSenderErrorSource::Mux`] wraps [`MuxError`] from the inner muxer:
    ///   [`MuxError::InvalidStreamHandle`] if `handle`'s index is out of
    ///   range for this muxer's audio streams;
    ///   [`MuxError::AudioTooLarge`] if `frames.len()` would overflow
    ///   `PES_packet_length`; [`MuxError::BufferFull`] if the muxer's
    ///   outbound queue is at `MuxerConfig::buffer_packets`.
    /// - [`MuxSenderErrorSource::Transport`] wraps a [`TransportError`]; on
    ///   transport flap the unsent TS chunks are retained for a later
    ///   `send_*` call to drain.
    ///   Whether the input was consumed depends on the failure point — see
    ///   the module-level *Input consumption and retry* section before
    ///   deciding whether to resend.
    pub fn send_audio_to(
        &self,
        handle: AudioStreamHandle,
        frames: &[u8],
        pts: Pts90khz,
    ) -> Result<(), MuxSenderError> {
        // Mutex-poisoning policy — see send_video for rationale.
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| lock_poisoned("send_audio_to"))?;
        inner.send_audio_to(handle, frames, pts.as_ticks())
    }

    /// Send one subtitle PES unit. `pts` is in 90 kHz ticks (the
    /// TS clock); subtitles carry PTS only. `payload` is one complete
    /// logical subtitle unit (DVB-sub composition page, teletext data
    /// field, CEA-708 service block, or WebVTT cue) — fragmentation
    /// across PES is not used.
    ///
    /// Resolves only when exactly one subtitle stream is configured;
    /// with zero or multiple subtitle streams the muxer surfaces
    /// [`MuxError::AmbiguousTarget`] inside [`MuxSenderErrorSource::Mux`] — use
    /// [`Self::send_subtitle_to`] in that case.
    ///
    /// # C ABI
    ///
    /// `tst_mux_sender_send_subtitle` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// - [`MuxSenderErrorSource::Mux`] wraps [`MuxError`] from the inner muxer:
    ///   [`MuxError::NoSubtitleStreamsConfigured`] if no subtitle streams
    ///   exist; [`MuxError::AmbiguousTarget`] when more than one is
    ///   configured; [`MuxError::SubtitleTooLarge`] if `payload.len()`
    ///   would overflow `PES_packet_length`; [`MuxError::BufferFull`] if
    ///   the muxer's outbound queue is at `MuxerConfig::buffer_packets`.
    /// - [`MuxSenderErrorSource::Transport`] wraps a [`TransportError`]; on
    ///   transport flap the unsent TS chunks are retained for a later
    ///   `send_*` call to drain.
    ///   Whether the input was consumed depends on the failure point — see
    ///   the module-level *Input consumption and retry* section before
    ///   deciding whether to resend.
    pub fn send_subtitle(&self, payload: &[u8], pts: Pts90khz) -> Result<(), MuxSenderError> {
        // Mutex-poisoning policy — see send_video for rationale.
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| lock_poisoned("send_subtitle"))?;
        inner.send_subtitle(payload, pts.as_ticks())
    }

    /// Send one subtitle PES unit to a specific configured subtitle stream.
    /// `handle` is obtained from [`Self::subtitle_handles`]; passing a
    /// handle from a different sender / muxer surfaces as
    /// [`MuxError::InvalidStreamHandle`] inside [`MuxSenderErrorSource::Mux`].
    ///
    /// # C ABI
    ///
    /// `tst_mux_sender_send_subtitle_to` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// - [`MuxSenderErrorSource::Mux`] wraps [`MuxError`] from the inner muxer:
    ///   [`MuxError::InvalidStreamHandle`] if `handle`'s index is out of
    ///   range for this muxer's subtitle streams;
    ///   [`MuxError::SubtitleTooLarge`] if `payload.len()` would overflow
    ///   `PES_packet_length`; [`MuxError::BufferFull`] if the muxer's
    ///   outbound queue is at `MuxerConfig::buffer_packets`.
    /// - [`MuxSenderErrorSource::Transport`] wraps a [`TransportError`]; on
    ///   transport flap the unsent TS chunks are retained for a later
    ///   `send_*` call to drain.
    ///   Whether the input was consumed depends on the failure point — see
    ///   the module-level *Input consumption and retry* section before
    ///   deciding whether to resend.
    pub fn send_subtitle_to(
        &self,
        handle: SubtitleStreamHandle,
        payload: &[u8],
        pts: Pts90khz,
    ) -> Result<(), MuxSenderError> {
        // Mutex-poisoning policy — see send_video for rationale.
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| lock_poisoned("send_subtitle_to"))?;
        inner.send_subtitle_to(handle, payload, pts.as_ticks())
    }

    /// Send one data payload on the muxer's single data stream. `pts` is
    /// in 90 kHz units (the TS clock); written into the PES header only
    /// when the stream was configured with `carries_pts: true`, and
    /// always used for PSI/PCR pacing decisions.
    ///
    /// Data streams are a PES **pass-through** — no AU-cell wrap, no
    /// framing, no payload inspection.
    /// [`tst_core::mpegts::mux::Muxer::push_data_to`] is the contract
    /// holder; see its docs for the full pass-through guarantees and the
    /// no-PTS-stream behavior.
    ///
    /// # C ABI
    ///
    /// `tst_mux_sender_send_data` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// - [`MuxSenderErrorSource::Mux`] wraps [`MuxError`] from the inner muxer:
    ///   [`MuxError::NoDataStreamsConfigured`] if no data streams exist;
    ///   [`MuxError::AmbiguousTarget`] when more than one is configured
    ///   (use [`Self::send_data_to`]); [`MuxError::DataTooLarge`] if the
    ///   payload would overflow `PES_packet_length`;
    ///   [`MuxError::BufferFull`] if the muxer's outbound queue is at
    ///   `MuxerConfig::buffer_packets`.
    /// - [`MuxSenderErrorSource::Transport`] wraps a [`TransportError`]; on
    ///   transport flap the unsent TS chunks are retained for a later
    ///   `send_*` call to drain.
    ///   Whether the input was consumed depends on the failure point — see
    ///   the module-level *Input consumption and retry* section before
    ///   deciding whether to resend.
    pub fn send_data(&self, data: &[u8], pts: Pts90khz) -> Result<(), MuxSenderError> {
        // Mutex-poisoning policy — see send_video for rationale.
        let mut inner = self.inner.lock().map_err(|_| lock_poisoned("send_data"))?;
        inner.send_data(data, pts.as_ticks())
    }

    /// Send one data payload to a specific configured data stream.
    /// `handle` is obtained from [`Self::data_handles`]; passing a handle
    /// from a different sender / muxer surfaces as
    /// [`MuxError::InvalidStreamHandle`] inside [`MuxSenderErrorSource::Mux`].
    ///
    /// Data streams are a PES **pass-through** — no AU-cell wrap, no
    /// framing, no payload inspection.
    /// [`tst_core::mpegts::mux::Muxer::push_data_to`] is the contract
    /// holder; see its docs for the full pass-through guarantees and the
    /// no-PTS-stream behavior.
    ///
    /// # C ABI
    ///
    /// `tst_mux_sender_send_data_to` — see `bindings/c/include/tstrans.h`.
    ///
    /// # Errors
    /// - [`MuxSenderErrorSource::Mux`] wraps [`MuxError`] from the inner muxer:
    ///   [`MuxError::InvalidStreamHandle`] if `handle`'s index is out of
    ///   range for this muxer's data streams; [`MuxError::DataTooLarge`]
    ///   if the payload would overflow `PES_packet_length`;
    ///   [`MuxError::BufferFull`] if the muxer's outbound queue is at
    ///   `MuxerConfig::buffer_packets`.
    /// - [`MuxSenderErrorSource::Transport`] wraps a [`TransportError`]; on
    ///   transport flap the unsent TS chunks are retained for a later
    ///   `send_*` call to drain.
    ///   Whether the input was consumed depends on the failure point — see
    ///   the module-level *Input consumption and retry* section before
    ///   deciding whether to resend.
    pub fn send_data_to(
        &self,
        handle: DataStreamHandle,
        data: &[u8],
        pts: Pts90khz,
    ) -> Result<(), MuxSenderError> {
        // Mutex-poisoning policy — see send_video for rationale.
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| lock_poisoned("send_data_to"))?;
        inner.send_data_to(handle, data, pts.as_ticks())
    }

    /// Snapshot all video stream handles for this sender's muxer, in
    /// declaration order. Allocates an owned Vec so callers don't need
    /// to hold the lock.
    pub fn video_handles(&self) -> Vec<VideoStreamHandle> {
        // Safe-default on poison: a poisoned inner lock returns an empty
        // handle list — matches the "no live muxer state" answer.
        // Precedent: `ManagedTransport::socket_stats` → None on poison (reconnect/mod.rs).
        if let Ok(inner) = self.inner.lock() {
            inner.muxer.video_handles()
        } else {
            Vec::new()
        }
    }

    /// Snapshot all KLV stream handles for this sender's muxer.
    pub fn klv_handles(&self) -> Vec<KlvStreamHandle> {
        // Mutex-poisoning policy (safe-default on poison) — see video_handles for rationale.
        if let Ok(inner) = self.inner.lock() {
            inner.muxer.klv_handles()
        } else {
            Vec::new()
        }
    }

    /// Snapshot all audio stream handles for this sender's muxer, in
    /// declaration order.
    pub fn audio_handles(&self) -> Vec<AudioStreamHandle> {
        // Mutex-poisoning policy (safe-default on poison) — see video_handles for rationale.
        if let Ok(inner) = self.inner.lock() {
            inner.muxer.audio_handles()
        } else {
            Vec::new()
        }
    }

    /// Audio stream handles for the named program, in declaration order.
    /// Returns `Err(MuxError::ProgramNotFound)` if no program with the
    /// given number exists in this sender's muxer configuration.
    pub fn audio_handles_for_program(
        &self,
        program_number: u16,
    ) -> Result<Vec<AudioStreamHandle>, MuxError> {
        // Mutex-poisoning policy (recoverable path / closest-semantic mapping):
        // poisoned inner lock returns ProgramNotFound since the muxer state is
        // unreachable — the closest existing semantic for "no programs available"
        // given the function's narrow MuxError surface. The alternative — a new
        // MuxError::LockPoisoned variant — was rejected due to the public-api
        // baseline bump + binding-surface ripple.
        self.inner
            .lock()
            .map_err(|_| MuxError::ProgramNotFound { program_number })?
            .muxer
            .audio_handles_for_program(program_number)
    }

    /// Snapshot all subtitle stream handles for this sender's muxer.
    pub fn subtitle_handles(&self) -> Vec<SubtitleStreamHandle> {
        // Mutex-poisoning policy (safe-default on poison) — see video_handles for rationale.
        if let Ok(inner) = self.inner.lock() {
            inner.muxer.subtitle_handles()
        } else {
            Vec::new()
        }
    }

    /// Subtitle stream handles for the named program, in declaration
    /// order. Returns `Err(MuxError::ProgramNotFound)` if no program
    /// with the given number exists in this sender's muxer
    /// configuration.
    pub fn subtitle_handles_for_program(
        &self,
        program_number: u16,
    ) -> Result<Vec<SubtitleStreamHandle>, MuxError> {
        // Mutex-poisoning policy (recoverable path / closest-semantic mapping) —
        // see audio_handles_for_program for rationale.
        self.inner
            .lock()
            .map_err(|_| MuxError::ProgramNotFound { program_number })?
            .muxer
            .subtitle_handles_for_program(program_number)
    }

    /// Snapshot all data stream handles for this sender's muxer.
    pub fn data_handles(&self) -> Vec<DataStreamHandle> {
        // Mutex-poisoning policy (safe-default on poison) — see video_handles for rationale.
        if let Ok(inner) = self.inner.lock() {
            inner.muxer.data_handles()
        } else {
            Vec::new()
        }
    }

    /// Return a point-in-time stats snapshot. `per_stream` is delegated from
    /// the inner `Muxer`; `pending_*` fields are live gauges.
    pub fn stats(&self) -> MuxSenderStats {
        // Mutex-poisoning policy (safe-default on poison): zeroed stats matches
        // "no live state available."
        let Ok(inner) = self.inner.lock() else {
            return MuxSenderStats::default();
        };
        let mux_stats = inner.muxer.stats();
        let pending_bytes_queued: u64 = inner.pending_bytes.iter().map(|c| c.len() as u64).sum();
        let pending_chunks_queued = inner.pending_bytes.len() as u64;
        MuxSenderStats {
            bytes_sent: inner.bytes_sent,
            packets_sent: inner.packets_sent,
            pending_bytes_queued,
            pending_chunks_queued,
            programs_configured: mux_stats.programs_configured,
            per_stream: mux_stats.per_stream,
        }
    }

    /// Wire-level transport stats (RTT, packet loss, bandwidth, queue
    /// depths) sourced from the underlying [`Transport::socket_stats`]
    /// implementation. Returns `None` when the transport doesn't expose
    /// comparable telemetry (test mocks) or when a managed wrapper has
    /// no live inner socket (mid-reconnect).
    ///
    /// # C ABI
    ///
    /// `tst_mux_sender_get_socket_stats` — see
    /// `bindings/c/include/tstrans.h`.
    pub fn socket_stats(&self) -> Option<tst_core::transport::SocketStats> {
        // Mutex-poisoning policy (safe-default on poison): mirrors reconnect/mod.rs
        // verbatim — None on poison, indistinguishable from "no live socket."
        // C ABI surfaces this as TST_E_NOT_AVAILABLE (-13).
        self.inner
            .lock()
            .ok()
            .and_then(|i| i.transport.socket_stats())
    }

    /// Per-PID codec-specific counters. Delegates to the inner
    /// [`tst_core::mpegts::mux::Muxer::stream_codec_stats`].
    ///
    /// See [`tst_core::mpegts::stats::StreamCodecStats`] for the
    /// semantics of the return value (`None` vs `Some(Unknown)` vs
    /// typed variant).
    ///
    /// Result does NOT vary with transport reconnect state — the
    /// Muxer's per-PID state is independent of the live socket. The C
    /// ABI's `tst_managed_mux_sender_get_stream_codec_stats` returns
    /// the same values as `tst_mux_sender_get_stream_codec_stats`
    /// during reconnect; no `TST_E_NOT_AVAILABLE` is returned for
    /// codec stats.
    ///
    /// # C ABI
    ///
    /// `tst_mux_sender_get_stream_codec_stats` (plain) +
    /// `tst_managed_mux_sender_get_stream_codec_stats` (managed wrapper) —
    /// see `bindings/c/include/tstrans.h`.
    pub fn stream_codec_stats(
        &self,
        pid: u16,
    ) -> Option<tst_core::mpegts::stats::StreamCodecStats> {
        // Mutex-poisoning policy (safe-default on poison): None on poison —
        // same shape as socket_stats.
        self.inner
            .lock()
            .ok()
            .and_then(|i| i.muxer.stream_codec_stats(pid))
    }

    /// Zero all flow counters and delegate to `Muxer::reset_stats`.
    /// `pending_bytes_queued` / `pending_chunks_queued` are live gauges and
    /// are NOT cleared.
    pub fn reset_stats(&self) {
        // Mutex-poisoning policy (silent no-op on poison): reset_stats on a
        // poisoned state is naturally a no-op since the stats are already
        // lost. Matches close() + Drop shape verbatim.
        if let Ok(mut inner) = self.inner.lock() {
            inner.bytes_sent = 0;
            inner.packets_sent = 0;
            inner.muxer.reset_stats();
        }
    }

    /// Close the sender promptly. Idempotent.
    ///
    /// Cancels the underlying transport BEFORE acquiring the inner lock,
    /// so a peer thread parked inside `send_video` / `send_klv` /
    /// `send_*_to` (e.g. libsrt's `srt_sendmsg` blocked on a full send
    /// buffer) returns [`TransportError::Broken`] within one transport
    /// I/O cycle (~3-10 ms on SRT) — `close()` never deadlocks against a
    /// parked send and never waits on a slow network. This is the
    /// emergency/prompt shutdown primitive; its price is that
    /// `pending_bytes` retained from a prior transient send error are
    /// ABANDONED (the post-cancel drain attempt finds the transport
    /// already cancelled and gives up fast). Use [`Self::finish`] when
    /// the buffered tail must be delivered — it drains first, reports
    /// failure, and only then closes.
    ///
    /// Poisoned-lock handling: if a prior panic poisoned the inner
    /// mutex, `close` still cancels (waking any parked peer) and skips
    /// the drain/close bookkeeping rather than panicking — parity with
    /// `Drop`.
    pub fn close(&self) {
        if let Some(c) = &self.cancel {
            c.cancel();
        }
        if let Ok(mut inner) = self.inner.lock() {
            let _ = inner.drain_pending();
            inner.closed = true;
            inner.transport.close();
        }
    }

    /// Drain any `pending_bytes`, then close: the fallible, lossless
    /// counterpart to [`Self::close`]. Nothing is cancelled first, so
    /// the drain sends go to the still-live transport and a tail
    /// retained from a prior transient send error is delivered rather
    /// than abandoned. Idempotent: returns `Ok(())` on an
    /// already-closed sender.
    ///
    /// **May block.** Each drain send can take as long as the
    /// transport's send timeout — unbounded for a blocking transport
    /// with no send timeout (the same bound `Drop`'s best-effort drain
    /// has always had). A watchdog holding [`Self::cancel_handle`] can
    /// unblock a stuck `finish` from another thread: the parked drain
    /// send returns [`TransportError::Broken`], which `finish` surfaces
    /// as its error.
    ///
    /// # Errors
    ///
    /// The first drain error, after which the remaining pending bytes
    /// are abandoned; the sender is marked closed and the transport
    /// closed regardless of the drain outcome (`finish` never leaves
    /// the sender half-open). A poisoned inner lock surfaces as
    /// [`MuxSenderErrorSource::Transport`] with a `finish`-site message,
    /// matching the send-path poisoning policy.
    pub fn finish(&self) -> Result<(), MuxSenderError> {
        let mut inner = self.inner.lock().map_err(|_| lock_poisoned("finish"))?;
        if inner.closed {
            return Ok(());
        }
        let drained = inner.drain_pending();
        inner.closed = true;
        inner.transport.close();
        drained
    }

    /// Consume the sender and hand back the owned transport.
    ///
    /// Best-effort: `pending_bytes` retained from a prior transient
    /// transport error are drained first (drain failures abandon them,
    /// matching `close`/`Drop`). The transport is **not** closed — the
    /// caller owns it live and is responsible for shutdown. Un-drained
    /// muxer state is discarded. If the inner lock was poisoned the
    /// transport is still returned (poison recovered — mirrors
    /// [`MuxPublisher::finish`](crate::mux_publisher::MuxPublisher::finish)).
    ///
    /// `std` only — the sibling std-only shells (`MuxPublisher`) share
    /// this same shape; the `no_std` sender path has no analogous need
    /// to reclaim a live transport mid-mission.
    #[cfg(feature = "std")]
    pub fn into_inner(self) -> T {
        // MuxSender has a Drop impl, so a plain destructuring move out of
        // `self` is rejected by the compiler (E0509) — go through
        // ManuallyDrop + ptr::read instead. COMPLETENESS: every field of
        // MuxSender must be read exactly once below — re-check against the
        // struct definition when editing (a missed field leaks; a double
        // read double-drops).
        let this = std::mem::ManuallyDrop::new(self);
        // SAFETY: `this` is ManuallyDrop so Drop::drop never runs; each
        // field is moved out exactly once via ptr::read and `this` is
        // never touched afterwards (not even implicitly — it falls out of
        // scope with no destructor to run).
        let (inner, cancel, span) = unsafe {
            (
                std::ptr::read(&this.inner),
                std::ptr::read(&this.cancel),
                std::ptr::read(&this._span),
            )
        };
        drop(cancel);
        let mut inner = inner.into_inner().unwrap_or_else(|e| e.into_inner());
        {
            // Keep the sender's span alive and entered across the final
            // drain so its tracing events still attribute to this
            // MuxSender (the span used to be dropped first, orphaning
            // the drain's events).
            let _enter = span.0.enter();
            let _ = inner.drain_pending();
        }
        drop(span);
        inner.transport
    }

    /// Snapshot of the underlying transport's cancel handle, if it
    /// supports cancellation. Equivalent to what `close()` calls
    /// internally; exposed for callers who want to keep the MuxSender
    /// alive but still have an out-of-band wake-up mechanism.
    ///
    /// # C ABI
    ///
    /// `tst_mux_sender_cancel` — see `bindings/c/include/tstrans.h`.
    pub fn cancel_handle(
        &self,
    ) -> Option<Arc<dyn tst_core::transport::TransportCancel + Send + Sync>> {
        self.cancel.clone()
    }

    #[must_use]
    pub fn is_alive(&self) -> bool {
        // Mutex-poisoning policy (safe-default on poison): poisoned state is not
        // alive. False matches the "wrapper unusable" answer.
        if let Ok(inner) = self.inner.lock() {
            !inner.closed && inner.transport.is_alive()
        } else {
            false
        }
    }
}

impl<T: Transport> Drop for MuxSender<T> {
    fn drop(&mut self) {
        let _enter = self._span.0.enter();
        // Best-effort drain of pending_bytes on drop; if transport rejects,
        // they're discarded. Gate on `!inner.closed` to mirror Sender::Drop —
        // a prior explicit close() already drained + closed, so close-then-drop
        // would otherwise call transport.close() twice. Idempotent in practice,
        // but the gate keeps the contract consistent across the three shells.
        if let Ok(mut inner) = self.inner.lock() {
            if !inner.closed {
                let _ = inner.drain_pending();
                inner.transport.close();
            }
        }
        tracing::info!("MuxSender closed");
    }
}

/// Type alias for [`MuxSender`] with a boxed [`Transport`] trait object.
///
/// Bindings code (`tst-jni`, `tst-uniffi`, `tst-pyo3`) targets this single
/// concrete type instead of cubing per-`T` instantiation. Rust callers with a
/// custom transport keep the generic `MuxSender<MyTransport>` shape.
///
/// # Example — opaque sender from a runtime-chosen transport
/// ```no_run
/// use tst_pipeline::mux_sender::BoxedMuxSender;
/// use tst_pipeline::MuxSender;
/// use tst_core::Transport;
///
/// fn open(transport: Box<dyn Transport>) -> Result<BoxedMuxSender, Box<dyn std::error::Error>> {
///     Ok(MuxSender::new(transport, Default::default())?)
/// }
/// ```
pub type BoxedMuxSender = MuxSender<Box<dyn crate::Transport>>;

impl<T: Transport> Inner<T> {
    /// Shared body for every `send_*` path: closed-check → drain pending →
    /// push (via the caller-supplied closure) → back-pressure sample →
    /// drain muxer. Called with a closure so the push arguments (handles,
    /// PTS, data slices) don't need to be marshalled into a common enum.
    fn push_then_drain(
        &mut self,
        push: impl FnOnce(&mut Muxer) -> Result<(), MuxError>,
    ) -> Result<(), MuxSenderError> {
        if self.closed {
            return Err(MuxSenderError::from(TransportError::Closed).with_input_consumed(false));
        }
        // Drain any leftover from a previous failed call first. A failure
        // here happens BEFORE this call's input is touched.
        self.drain_pending()
            .map_err(|e| e.with_input_consumed(false))?;
        // Push new content. Sample back-pressure between the push (queue at
        // peak) and the drain (queue back to zero).
        let push_result = push(&mut self.muxer);
        self.maybe_warn_backpressure(matches!(push_result, Err(MuxError::BufferFull { .. })));
        push_result.map_err(|e| MuxSenderError::from(e).with_input_consumed(false))?;
        // From here the input is muxed and retained: a failure leaves it in
        // the pending queue, draining exactly once on the next call.
        self.drain_muxer().map_err(|e| e.with_input_consumed(true))
    }

    fn send_video(
        &mut self,
        nal: &[u8],
        pts_90khz: i64,
        key_frame: bool,
    ) -> Result<(), MuxSenderError> {
        self.push_then_drain(|m| m.push_video(nal, Pts90khz::new(pts_90khz), key_frame))
    }

    fn send_klv(
        &mut self,
        klv: &[u8],
        pts_90khz: i64,
        metadata_service_id: u8,
    ) -> Result<(), MuxSenderError> {
        self.push_then_drain(|m| m.push_klv(klv, Pts90khz::new(pts_90khz), metadata_service_id))
    }

    fn send_video_to(
        &mut self,
        handle: VideoStreamHandle,
        nal: &[u8],
        pts_90khz: i64,
        key_frame: bool,
    ) -> Result<(), MuxSenderError> {
        self.push_then_drain(|m| m.push_video_to(handle, nal, Pts90khz::new(pts_90khz), key_frame))
    }

    fn send_video_to_with_dts(
        &mut self,
        handle: VideoStreamHandle,
        nal: &[u8],
        pts_90khz: i64,
        dts_90khz: i64,
        key_frame: bool,
    ) -> Result<(), MuxSenderError> {
        self.push_then_drain(|m| {
            m.push_video_to_with_dts(
                handle,
                nal,
                Pts90khz::new(pts_90khz),
                Pts90khz::new(dts_90khz),
                key_frame,
            )
        })
    }

    fn send_video_misp_to(
        &mut self,
        handle: VideoStreamHandle,
        nal: &[u8],
        pts_90khz: i64,
        key_frame: bool,
        misp: &tst_core::codec::misp_time::MispTimestamp,
    ) -> Result<(), MuxSenderError> {
        self.push_then_drain(|m| {
            m.push_video_misp_to(handle, nal, Pts90khz::new(pts_90khz), key_frame, misp)
        })
    }

    fn send_video_misp_to_with_dts(
        &mut self,
        handle: VideoStreamHandle,
        nal: &[u8],
        pts_90khz: i64,
        dts_90khz: i64,
        key_frame: bool,
        misp: &tst_core::codec::misp_time::MispTimestamp,
    ) -> Result<(), MuxSenderError> {
        self.push_then_drain(|m| {
            m.push_video_misp_to_with_dts(
                handle,
                nal,
                Pts90khz::new(pts_90khz),
                Pts90khz::new(dts_90khz),
                key_frame,
                misp,
            )
        })
    }

    fn send_klv_to(
        &mut self,
        handle: KlvStreamHandle,
        klv: &[u8],
        pts_90khz: i64,
        metadata_service_id: u8,
    ) -> Result<(), MuxSenderError> {
        self.push_then_drain(|m| {
            m.push_klv_to(handle, klv, Pts90khz::new(pts_90khz), metadata_service_id)
        })
    }

    fn send_audio(&mut self, frames: &[u8], pts_90khz: i64) -> Result<(), MuxSenderError> {
        self.push_then_drain(|m| m.push_audio(frames, Pts90khz::new(pts_90khz)))
    }

    fn send_audio_to(
        &mut self,
        handle: AudioStreamHandle,
        frames: &[u8],
        pts_90khz: i64,
    ) -> Result<(), MuxSenderError> {
        // Muxer parameter order is `(handle, pts, frames)`; the public
        // pipeline API mirrors `send_video` / `send_klv` (data first).
        self.push_then_drain(|m| m.push_audio_to(handle, Pts90khz::new(pts_90khz), frames))
    }

    fn send_subtitle(&mut self, payload: &[u8], pts_90khz: i64) -> Result<(), MuxSenderError> {
        // Muxer parameter order is `(pts, payload)`; we present
        // `(payload, pts)` for symmetry with `send_video` / `send_klv`.
        self.push_then_drain(|m| m.push_subtitle(Pts90khz::new(pts_90khz), payload))
    }

    fn send_subtitle_to(
        &mut self,
        handle: SubtitleStreamHandle,
        payload: &[u8],
        pts_90khz: i64,
    ) -> Result<(), MuxSenderError> {
        self.push_then_drain(|m| m.push_subtitle_to(handle, Pts90khz::new(pts_90khz), payload))
    }

    fn send_data(&mut self, data: &[u8], pts_90khz: i64) -> Result<(), MuxSenderError> {
        self.push_then_drain(|m| m.push_data(data, Pts90khz::new(pts_90khz)))
    }

    fn send_data_to(
        &mut self,
        handle: DataStreamHandle,
        data: &[u8],
        pts_90khz: i64,
    ) -> Result<(), MuxSenderError> {
        self.push_then_drain(|m| m.push_data_to(handle, data, Pts90khz::new(pts_90khz)))
    }

    /// Sample muxer queue depth and emit `tracing::warn!` once when the
    /// back-pressure tier transitions UP (`Ok→Warn` at >=80% of cap, or
    /// `Warn→Overflow` at >=100% of cap). Recovery transitions are
    /// silent. Called between `push_*` and `drain_muxer`, when the
    /// queue is at its peak depth for this `send_*` cycle.
    ///
    /// `push_was_buffer_full` flags the case where the just-attempted
    /// `push_*` returned [`MuxError::BufferFull`]: the queue depth is
    /// unchanged from before the failed push (so it may read below the
    /// cap), but the user-observable signal — a push that didn't fit —
    /// is the overflow transition.
    fn maybe_warn_backpressure(&mut self, push_was_buffer_full: bool) {
        let cap = self.muxer.capacity_packets();
        if cap == 0 {
            return;
        }
        let pending = self.muxer.pending_packets();
        // Integer arithmetic: `pending * 5 >= cap * 4` is exactly
        // `pending / cap >= 0.8`, no f64 dependency, no boundary rounding.
        let new_state = if push_was_buffer_full || pending >= cap {
            BackpressureState::Overflow
        } else if pending.saturating_mul(5) >= cap.saturating_mul(4) {
            BackpressureState::Warn
        } else {
            BackpressureState::Ok
        };
        if new_state > self.last_backpressure_state {
            match new_state {
                BackpressureState::Warn => tracing::warn!(
                    target: "tst_pipeline::mux_sender",
                    pending,
                    cap,
                    "back-pressure approaching cap (>=80%)",
                ),
                BackpressureState::Overflow => tracing::warn!(
                    target: "tst_pipeline::mux_sender",
                    pending,
                    cap,
                    "back-pressure at cap — sends will block or fail",
                ),
                BackpressureState::Ok => {}
            }
        }
        self.last_backpressure_state = new_state;
    }

    /// Drain the muxer's internal buffer and forward each chunk to the
    /// transport. On transport error, captures any unsent chunks into
    /// `pending_bytes` and returns the error.
    fn drain_muxer(&mut self) -> Result<(), MuxSenderError> {
        let max = self.transport.max_payload();
        // Grow the scratch buffer lazily. The Transport trait does not
        // guarantee a fixed max_payload() across calls, so re-check each
        // time and resize only when the current allocation is too small.
        if self.scratch.len() < max {
            self.scratch.resize(max, 0);
        }
        loop {
            // Cap the view at the CURRENT max_payload — the scratch only
            // grows, so after a max_payload shrink the full buffer would
            // let `pull` produce chunks larger than the transport accepts.
            let n = self.muxer.pull(&mut self.scratch[..max]);
            if n == 0 {
                return Ok(());
            }
            match self.transport.send_bytes(&self.scratch[..n]) {
                Ok(()) => {
                    // Happy path: no allocation needed; bytes are in flight.
                    self.bytes_sent += n as u64;
                    self.packets_sent += 1;
                }
                Err(e) => {
                    // Transport rejected the chunk — buffer it; do NOT count as sent.
                    self.pending_bytes.push_back(self.scratch[..n].to_vec());
                    // Drain any further muxer output into pending_bytes too,
                    // so the muxer's internal buffer doesn't fill up while
                    // transport is unavailable.
                    loop {
                        let n2 = self.muxer.pull(&mut self.scratch[..max]);
                        if n2 == 0 {
                            break;
                        }
                        self.pending_bytes.push_back(self.scratch[..n2].to_vec());
                    }
                    return Err(e.into());
                }
            }
        }
    }

    fn drain_pending(&mut self) -> Result<(), MuxSenderError> {
        while let Some(chunk) = self.pending_bytes.front() {
            let len = chunk.len() as u64;
            self.transport.send_bytes(chunk)?;
            // Only count after successful send.
            self.bytes_sent += len;
            self.packets_sent += 1;
            self.pending_bytes.pop_front();
        }
        Ok(())
    }
}

/// Error returned by [`MuxSender`] methods.
///
/// # Categorization
///
/// Bindings categorize failures via [`Self::kind`] (one of 6
/// [`ShellErrorKind`] variants); power users inspect [`Self::source`]
/// for the typed inner error.
///
/// # Reachable kinds
///
/// `MuxSender` can produce: `ConfigInvalid`, `InputMalformed`,
/// `Backpressure`, `TransportBroken`, `Closed`. `EndOfStream` is
/// receiver-only.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
#[error("MuxSender error ({kind:?}): {source}")]
pub struct MuxSenderError {
    /// Categorical reason for this failure.
    pub kind: ShellErrorKind,
    /// Typed inner error (the actual `MuxError` or `TransportError`
    /// instance produced by the underlying muxer / transport).
    #[source]
    pub source: MuxSenderErrorSource,
    /// Whether THIS call's input was consumed by the muxer.
    ///
    /// - `Some(false)` — not consumed; retrying the same input cannot
    ///   duplicate data.
    /// - `Some(true)` — consumed: muxed and retained in the pending
    ///   queue (drains exactly once on the next `send_*`); do NOT push
    ///   the same input again.
    /// - `None` — the error did not originate from a `send_*` input
    ///   path (e.g. a poisoned internal lock; state indeterminate).
    pub input_consumed: Option<bool>,
}

impl MuxSenderError {
    pub(crate) fn with_input_consumed(mut self, consumed: bool) -> Self {
        self.input_consumed = Some(consumed);
        self
    }
}

/// Typed source enum for [`MuxSenderError`]. One variant per error type
/// the underlying `MuxSender` internals can produce.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum MuxSenderErrorSource {
    #[error(transparent)]
    Mux(#[from] MuxError),
    #[error(transparent)]
    Transport(#[from] TransportError),
}

impl From<MuxError> for MuxSenderError {
    fn from(e: MuxError) -> Self {
        Self {
            kind: crate::shell_error::kind_from_mux(&e),
            source: MuxSenderErrorSource::Mux(e),
            input_consumed: None,
        }
    }
}

impl From<TransportError> for MuxSenderError {
    fn from(e: TransportError) -> Self {
        Self {
            kind: crate::shell_error::kind_from_transport(&e, crate::shell_error::Direction::Send),
            source: MuxSenderErrorSource::Transport(e),
            input_consumed: None,
        }
    }
}

impl crate::shell_error::ShellError for MuxSenderError {
    fn kind(&self) -> ShellErrorKind {
        self.kind
    }

    fn errno_code(&self) -> Option<i32> {
        match &self.source {
            MuxSenderErrorSource::Transport(t) => crate::shell_error::errno_code_from_transport(t),
            MuxSenderErrorSource::Mux(_) => None,
        }
    }
}

#[cfg(test)]
mod multi_stream_tests {
    use super::*;
    use tst_core::mpegts::mux::{
        AudioCodec, KlvStreamType, MuxerProgramConfigBuilder, StreamKind, SubtitleCodec, VideoCodec,
    };
    use tst_core::transport::{Transport, TransportError};

    /// In-memory transport that records every byte sent.
    struct MemTransport {
        bytes: std::sync::Mutex<Vec<u8>>,
        alive: std::sync::atomic::AtomicBool,
    }
    impl MemTransport {
        fn new() -> Self {
            Self {
                bytes: std::sync::Mutex::new(Vec::new()),
                alive: std::sync::atomic::AtomicBool::new(true),
            }
        }
    }
    impl Transport for MemTransport {
        fn send_bytes(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            Ok(())
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn close(&mut self) {
            self.alive.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        fn is_alive(&self) -> bool {
            self.alive.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[test]
    fn sender_video_handles_returns_one_per_configured_video_stream() {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x1011, VideoCodec::H264);
            prog.add_video(0x1021, VideoCodec::H264);
            prog.add_klv(0x1031, KlvStreamType::PrivateData, false);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let s = MuxSender::new(MemTransport::new(), cfg).unwrap();
        assert_eq!(s.video_handles().len(), 2);
        assert_eq!(s.klv_handles().len(), 1);
    }

    #[test]
    fn sender_send_video_to_routes_through() {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x1011, VideoCodec::H264);
            prog.add_video(0x1021, VideoCodec::H264);
            prog.add_klv(0x1031, KlvStreamType::PrivateData, false);
            prog.pcr_pid(0x1011);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let s = MuxSender::new(MemTransport::new(), cfg).unwrap();
        let ir = s.video_handles()[1];
        let nal = [0x00, 0x00, 0x00, 0x01, 0x67, 0xBB];
        s.send_video_to(ir, &nal, Pts90khz::new(0), true).unwrap();
        // We can't read the transport bytes directly from outside the lock,
        // but we can confirm the call returns Ok and the sender is alive.
        assert!(s.is_alive());
    }

    /// Collects every byte sent to it behind a shared handle — unlike
    /// `MemTransport`, the MISP round-trip tests below need to read the
    /// bytes back out *after* the `MuxSender` (which owns the transport) is
    /// dropped, hence the `Arc<Mutex<..>>` instead of an owned buffer.
    struct SnoopTransport(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl Transport for SnoopTransport {
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

    /// AUD + SPS + PPS + IDR — canonical H.264 keyframe AU, used by the MISP
    /// round-trip tests below.
    fn h264_keyframe_au() -> Vec<u8> {
        fn nal(nal_type: u8, nri: u8, body: &[u8]) -> Vec<u8> {
            let mut v = vec![0x00, 0x00, 0x00, 0x01, (nri << 5) | nal_type];
            v.extend_from_slice(body);
            v
        }
        let mut au = Vec::new();
        au.extend(nal(9, 0b00, &[0xF0])); // AUD
        au.extend(nal(7, 0b11, &[0x42, 0xC0, 0x28])); // SPS
        au.extend(nal(8, 0b11, &[0xCE, 0x38])); // PPS
        au.extend(nal(5, 0b11, &[0x88, 0x84, 0x0A])); // IDR
        au
    }

    /// Shared body for the two MISP round-trip tests below: mux a keyframe
    /// AU carrying a MISP timestamp — via `send_video_misp_to`, or
    /// `send_video_misp_to_with_dts` when `dts` is `Some` — then demux the
    /// wire bytes and assert the timestamp survives the round trip.
    fn assert_misp_round_trips(dts: Option<Pts90khz>) {
        use tst_core::codec::misp_time::MispTimestamp;
        use tst_core::mpegts::demux::event::{DemuxEvent, SamplePayload};
        use tst_core::mpegts::demux::{Demuxer, DemuxerConfig};

        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x1011, VideoCodec::H264);
            prog.pcr_pid(0x1011);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };

        // Collect transport bytes via snoop transport.
        let snoop = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let s = MuxSender::new(SnoopTransport(snoop.clone()), cfg).unwrap();
        let handle = s.video_handles()[0];

        let nal = h264_keyframe_au();
        let misp = MispTimestamp::micros(0x0102_0304_0506_0708, 0x1F);
        let pts = Pts90khz::new(90_000);
        match dts {
            Some(dts) => s
                .send_video_misp_to_with_dts(handle, &nal, pts, dts, true, &misp)
                .unwrap(),
            None => s
                .send_video_misp_to(handle, &nal, pts, true, &misp)
                .unwrap(),
        }
        drop(s);

        // Demux the captured TS bytes and extract the MISP timestamp.
        let ts_bytes = snoop.lock().unwrap().clone();
        let mut demuxer = Demuxer::with_config(DemuxerConfig::builder().build());
        let mut found_misp: Option<MispTimestamp> = None;
        demuxer.feed(&ts_bytes).unwrap();
        demuxer.flush();
        loop {
            match demuxer.next_event() {
                Some(DemuxEvent::Sample {
                    payload: SamplePayload::Video { raw, .. },
                    ..
                }) => {
                    let extracted = tst_core::codec::misp_time::extract(
                        &raw,
                        tst_core::mpegts::mux::VideoCodec::H264,
                    )
                    .unwrap();
                    if extracted.is_some() {
                        found_misp = extracted;
                        break;
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
        let recovered = found_misp.expect("MISP timestamp must be present in demuxed AU");
        assert_eq!(recovered.value, misp.value, "timestamp value mismatch");
        assert_eq!(
            recovered.time_status, misp.time_status,
            "status byte mismatch"
        );
    }

    /// `send_video_misp_to` splices the ST 0604 SEI and emits valid TS bytes
    /// that can be demuxed; `misp_time::extract` recovers the timestamp.
    #[test]
    fn sender_send_video_misp_to_recovers_timestamp() {
        assert_misp_round_trips(None);
    }

    #[test]
    fn sender_send_video_misp_to_with_dts_recovers_timestamp() {
        // DTS strictly less than PTS.
        assert_misp_round_trips(Some(Pts90khz::new(87_000)));
    }

    #[test]
    fn stats_starts_with_per_stream_entries_for_configured_streams() {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, VideoCodec::H264);
            prog.add_klv(0x101, KlvStreamType::PrivateData, false);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let s = MuxSender::new(MemTransport::new(), cfg).unwrap();
        let st = s.stats();
        assert_eq!(st.bytes_sent, 0);
        assert_eq!(st.packets_sent, 0);
        assert_eq!(st.pending_bytes_queued, 0);
        assert_eq!(st.pending_chunks_queued, 0);
        assert_eq!(st.per_stream.len(), 2);
        assert!(st.per_stream.contains_key(&0x100));
    }

    #[test]
    fn stats_count_video_pushes() {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, VideoCodec::H264);
            prog.add_klv(0x101, KlvStreamType::PrivateData, false);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let s = MuxSender::new(MemTransport::new(), cfg).unwrap();
        let nal: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x67, 0xBB];
        s.send_video(nal, Pts90khz::new(0), true).unwrap();
        let st = s.stats();
        assert_eq!(st.per_stream[&0x100].items, 1);
        assert_eq!(st.per_stream[&0x100].bytes, nal.len() as u64);
        assert!(st.bytes_sent > 0);
        assert!(st.packets_sent > 0);
    }

    #[test]
    fn reset_stats_zeros_counters_keeps_per_stream() {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, VideoCodec::H264);
            prog.add_klv(0x101, KlvStreamType::PrivateData, false);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let s = MuxSender::new(MemTransport::new(), cfg).unwrap();
        let nal: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x67, 0xBB];
        s.send_video(nal, Pts90khz::new(0), true).unwrap();
        s.reset_stats();
        let st = s.stats();
        assert_eq!(st.bytes_sent, 0);
        assert_eq!(st.packets_sent, 0);
        assert_eq!(st.per_stream.len(), 2);
        assert_eq!(st.per_stream[&0x100].items, 0);
    }

    #[test]
    fn send_audio_pushes_through_pipeline() {
        // Single program, video + one audio stream. The bare send_audio
        // shorthand resolves because total_audio == 1 across the muxer.
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, VideoCodec::H264);
            prog.add_audio(0x200, AudioCodec::Aac);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let s = MuxSender::new(MemTransport::new(), cfg).unwrap();
        // Synthetic audio frame bytes — the muxer doesn't validate the
        // codec payload here, so any non-empty buffer suffices.
        let frames = vec![0xFFu8; 64];
        s.send_audio(&frames, Pts90khz::new(90_000)).unwrap();
        let st = s.stats();
        assert_eq!(st.per_stream[&0x200].items, 1);
        assert_eq!(st.per_stream[&0x200].bytes, frames.len() as u64);
        assert!(st.bytes_sent > 0);
        assert!(st.packets_sent > 0);
    }

    #[test]
    fn send_audio_to_routes_by_handle() {
        // Two audio streams — bare send_audio would reject with
        // AmbiguousTarget; send_audio_to disambiguates via handle.
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, VideoCodec::H264);
            prog.add_audio(0x200, AudioCodec::Aac);
            prog.add_audio(0x201, AudioCodec::Mp2);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let s = MuxSender::new(MemTransport::new(), cfg).unwrap();
        let handles = s.audio_handles();
        assert_eq!(handles.len(), 2);
        let frames = vec![0xAAu8; 32];
        s.send_audio_to(handles[1], &frames, Pts90khz::new(90_000))
            .unwrap();
        let st = s.stats();
        assert_eq!(st.per_stream[&0x201].items, 1);
        assert_eq!(st.per_stream[&0x200].items, 0);
        assert!(s.is_alive());
    }

    #[test]
    fn send_subtitle_pushes_through_pipeline() {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, VideoCodec::H264);
            prog.add_subtitle(0x300, SubtitleCodec::WebVttInTs);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let s = MuxSender::new(MemTransport::new(), cfg).unwrap();
        // A minimal WebVTT-in-TS cue body (the muxer doesn't validate
        // contents — it just frames the bytes into a PES).
        let cue = b"WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nhello\n";
        s.send_subtitle(cue, Pts90khz::new(90_000)).unwrap();
        let st = s.stats();
        assert_eq!(st.per_stream[&0x300].items, 1);
        assert_eq!(st.per_stream[&0x300].bytes, cue.len() as u64);
        assert!(st.bytes_sent > 0);
        assert!(st.packets_sent > 0);
    }

    #[test]
    fn send_subtitle_to_routes_by_handle() {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, VideoCodec::H264);
            prog.add_subtitle(0x300, SubtitleCodec::WebVttInTs);
            prog.add_subtitle(0x301, SubtitleCodec::WebVttInTs);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let s = MuxSender::new(MemTransport::new(), cfg).unwrap();
        let handles = s.subtitle_handles();
        assert_eq!(handles.len(), 2);
        let cue = b"WEBVTT\n\n00:00:03.000 --> 00:00:04.000\nrouted\n";
        s.send_subtitle_to(handles[1], cue, Pts90khz::new(90_000))
            .unwrap();
        let st = s.stats();
        assert_eq!(st.per_stream[&0x301].items, 1);
        assert_eq!(st.per_stream[&0x300].items, 0);
        assert!(s.is_alive());
    }

    #[test]
    fn send_data_pushes_through_pipeline() {
        // Single program, video + one data stream. The bare send_data
        // shorthand resolves because total_data == 1 across the muxer.
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, VideoCodec::H264);
            prog.add_data(0x1100, 0xF0, /*carries_pts=*/ true);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let s = MuxSender::new(MemTransport::new(), cfg).unwrap();
        // Synthetic payload bytes — data streams are a pass-through, so
        // any non-empty buffer suffices.
        let payload = vec![0x42u8; 64];
        s.send_data(&payload, Pts90khz::new(90_000)).unwrap();
        let st = s.stats();
        assert_eq!(st.per_stream[&0x1100].items, 1);
        assert_eq!(st.per_stream[&0x1100].bytes, payload.len() as u64);
        assert!(st.bytes_sent > 0);
        assert!(st.packets_sent > 0);
    }

    #[test]
    fn send_data_to_routes_by_handle() {
        // Two data streams — bare send_data would reject with
        // AmbiguousTarget; send_data_to disambiguates via handle.
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, VideoCodec::H264);
            prog.add_data(0x1100, 0xF0, /*carries_pts=*/ true);
            prog.add_data(0x1101, 0xF1, /*carries_pts=*/ true);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let s = MuxSender::new(MemTransport::new(), cfg).unwrap();
        let handles = s.data_handles();
        assert_eq!(handles.len(), 2);
        let payload = vec![0x42u8; 32];
        s.send_data_to(handles[1], &payload, Pts90khz::new(90_000))
            .unwrap();
        let st = s.stats();
        assert_eq!(st.per_stream[&0x1101].items, 1);
        assert_eq!(st.per_stream[&0x1100].items, 0);
        assert!(s.is_alive());
    }

    #[test]
    fn send_data_rejects_oversized_payload() {
        // 70_000 bytes overflows PES_packet_length (ceiling 65527 with
        // PTS); the muxer's DataTooLarge must pass through the shell's
        // error wrapping intact.
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, VideoCodec::H264);
            prog.add_data(0x1100, 0xF0, /*carries_pts=*/ true);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let s = MuxSender::new(MemTransport::new(), cfg).unwrap();
        let payload = vec![0x42u8; 70_000];
        let err = s.send_data(&payload, Pts90khz::new(0)).unwrap_err();
        match err.source {
            MuxSenderErrorSource::Mux(MuxError::DataTooLarge { size: 70_000, .. }) => {}
            other => panic!("expected DataTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn sender_send_video_rejects_when_multiple_video_streams_configured() {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x1011, VideoCodec::H264);
            prog.add_video(0x1021, VideoCodec::H264);
            prog.add_klv(0x1031, KlvStreamType::PrivateData, false);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let s = MuxSender::new(MemTransport::new(), cfg).unwrap();
        let nal = [0x00, 0x00, 0x00, 0x01, 0x67];
        let err = s.send_video(&nal, Pts90khz::new(0), true).unwrap_err();
        match err.source {
            MuxSenderErrorSource::Mux(MuxError::AmbiguousTarget {
                kind: StreamKind::Video,
                count: 2,
            }) => {}
            other => panic!("expected AmbiguousTarget, got {other:?}"),
        }
    }

    /// Transport that errors the first N send_bytes calls (back-pressure
    /// simulation), then accepts. Captured bytes are exposed via an external
    /// Arc<Mutex<Vec<u8>>> snoop slot since MuxSender takes the transport by
    /// value (MemTransport above isn't observable post-construction).
    struct BackpressureOnce {
        fail_remaining: std::sync::atomic::AtomicUsize,
        bytes: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }
    impl BackpressureOnce {
        fn new(fail_first: usize, snoop: std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> Self {
            Self {
                fail_remaining: std::sync::atomic::AtomicUsize::new(fail_first),
                bytes: snoop,
            }
        }
    }
    impl Transport for BackpressureOnce {
        fn send_bytes(&mut self, b: &[u8]) -> Result<(), TransportError> {
            let prev = self
                .fail_remaining
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            if prev > 0 {
                return Err(TransportError::Backpressure {
                    msg: "backpressure-once".to_string(),
                    errno_code: None,
                });
            }
            self.bytes.lock().unwrap().extend_from_slice(b);
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
    fn finish_drains_pending_bytes() {
        // The lossless explicit-shutdown path (PIPE-02's ask, reshaped at
        // the 0.5.0 release gate): finish() must drain pending_bytes
        // before marking closed — close() is the prompt/lossy primitive
        // and deliberately does not make this guarantee.
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, VideoCodec::H264);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        // Snoop slot exposes BackpressureOnce's captured bytes externally.
        let snoop = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        // Fail first send_bytes: forces the muxer's emitted bundle to land in
        // pending_bytes (Inner::drain_muxer reacts to TransportError::Backpressure).
        let transport = BackpressureOnce::new(1, snoop.clone());
        let sender = MuxSender::new(transport, cfg).unwrap();

        // Minimal Annex-B H.264 IDR NAL.
        let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
        // First send: muxer emits a bundle, transport rejects, bundle lands
        // in pending_bytes. send_video returns Err(Backpressure) — ignore;
        // the relevant assertion is about close's post-condition.
        let _ = sender.send_video(&nal, Pts90khz::new(0), true);

        sender
            .finish()
            .expect("drain succeeds on the healed transport");

        let captured = snoop.lock().unwrap().len();
        assert!(
            captured > 0,
            "MuxSender::finish must drain pending_bytes; captured = {captured}"
        );
    }

    /// finish() on a transport that never heals surfaces the drain error
    /// and still leaves the sender closed (never half-open).
    #[test]
    fn finish_surfaces_drain_failure_and_closes() {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, VideoCodec::H264);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let snoop = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        // Fails every send: the seeded pending bytes can never drain.
        let transport = BackpressureOnce::new(usize::MAX, snoop.clone());
        let sender = MuxSender::new(transport, cfg).unwrap();
        let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
        let _ = sender.send_video(&nal, Pts90khz::new(0), true);

        let res = sender.finish();
        assert!(res.is_err(), "drain failure must surface, got {res:?}");
        assert!(
            !sender.is_alive(),
            "finish must close even on drain failure"
        );
        assert!(snoop.lock().unwrap().is_empty());
        // Idempotent second call on the now-closed sender.
        assert!(sender.finish().is_ok());
    }

    /// Transport whose sends BLOCK until its cancel handle fires — the
    /// blocking-transport-with-pending shape from the release-gate audit
    /// (a ManagedTransport mid-reconnect after pending_bytes was seeded).
    /// close() must return promptly by cancelling first; the 0.5.0-rc1
    /// drain-first close() hung here indefinitely.
    struct BlocksUntilCancelled {
        cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
        fail_first: std::sync::atomic::AtomicUsize,
        blocked_too_long: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    struct FlagCancel(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl tst_core::transport::TransportCancel for FlagCancel {
        fn cancel(&self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    impl Transport for BlocksUntilCancelled {
        fn send_bytes(&mut self, _b: &[u8]) -> Result<(), TransportError> {
            use std::sync::atomic::Ordering;
            if self.fail_first.fetch_sub(1, Ordering::SeqCst) > 0 {
                return Err(TransportError::Backpressure {
                    msg: "seed pending".into(),
                    errno_code: None,
                });
            }
            // Block until cancelled; bail after 10 s so a regression fails
            // the test instead of hanging the runner.
            let start = std::time::Instant::now();
            while !self.cancelled.load(Ordering::SeqCst) {
                if start.elapsed() > std::time::Duration::from_secs(10) {
                    self.blocked_too_long.store(true, Ordering::SeqCst);
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(TransportError::Broken {
                msg: "cancelled".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            })
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn close(&mut self) {}
        fn is_alive(&self) -> bool {
            true
        }
        fn cancel_handle(
            &self,
        ) -> Option<std::sync::Arc<dyn tst_core::transport::TransportCancel + Send + Sync>>
        {
            Some(std::sync::Arc::new(FlagCancel(self.cancelled.clone())))
        }
    }

    #[test]
    fn close_returns_promptly_on_blocking_transport_with_pending() {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, VideoCodec::H264);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let blocked_too_long = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let transport = BlocksUntilCancelled {
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fail_first: std::sync::atomic::AtomicUsize::new(1),
            blocked_too_long: blocked_too_long.clone(),
        };
        let sender = MuxSender::new(transport, cfg).unwrap();
        let nal = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
        // Seed pending_bytes via the transient failure.
        let _ = sender.send_video(&nal, Pts90khz::new(0), true);

        let t0 = std::time::Instant::now();
        sender.close();
        let elapsed = t0.elapsed();
        assert!(
            !blocked_too_long.load(std::sync::atomic::Ordering::SeqCst),
            "close() drained against an un-cancelled blocking transport"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "close() must be prompt, took {elapsed:?}"
        );
    }
}

#[cfg(test)]
mod input_consumed_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use tst_core::mpegts::mux::{MuxerProgramConfigBuilder, VideoCodec};

    /// Transport that fails while `failing` is set; delivers otherwise.
    struct SwitchableTransport {
        failing: Arc<AtomicBool>,
        sink: Arc<Mutex<Vec<u8>>>,
    }
    impl Transport for SwitchableTransport {
        fn send_bytes(&mut self, b: &[u8]) -> Result<(), TransportError> {
            if self.failing.load(Ordering::SeqCst) {
                return Err(TransportError::Backpressure {
                    msg: "test outage".into(),
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

    fn mk_sender(
        failing: Arc<AtomicBool>,
        sink: Arc<Mutex<Vec<u8>>>,
    ) -> MuxSender<SwitchableTransport> {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x1011, VideoCodec::H264);
            prog.pcr_pid(0x1011);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        MuxSender::new(SwitchableTransport { failing, sink }, cfg).unwrap()
    }

    const NAL: [u8; 6] = [0x00, 0x00, 0x00, 0x01, 0x67, 0xBB];

    /// Phase-2 failure: input muxed, then its bytes fail to send →
    /// `Some(true)`. After healing, the NEXT call drains it exactly once.
    #[test]
    fn transport_failure_after_mux_reports_consumed() {
        let failing = Arc::new(AtomicBool::new(true));
        let sink = Arc::new(Mutex::new(Vec::new()));
        let s = mk_sender(failing.clone(), sink.clone());

        let err = s.send_video(&NAL, Pts90khz::new(0), true).unwrap_err();
        assert_eq!(err.input_consumed, Some(true));

        failing.store(false, Ordering::SeqCst);
        s.send_video(&NAL, Pts90khz::new(3000), true).unwrap();
        // Both AUs delivered; pending drained exactly once (byte count is
        // a multiple of 188 and strictly more than one AU's packets).
        let n = sink.lock().unwrap().len();
        assert!(n > 0 && n % 188 == 0);
    }

    /// Phase-1 failure: retained bytes fail to drain BEFORE the new input
    /// is touched → `Some(false)`; retrying the same input later loses
    /// nothing and duplicates nothing.
    #[test]
    fn drain_phase_failure_reports_not_consumed() {
        let failing = Arc::new(AtomicBool::new(true));
        let sink = Arc::new(Mutex::new(Vec::new()));
        let s = mk_sender(failing.clone(), sink.clone());

        // Seed pending: first call consumes input, fails sending it.
        let e1 = s.send_video(&NAL, Pts90khz::new(0), true).unwrap_err();
        assert_eq!(e1.input_consumed, Some(true));
        // Second call fails in the pending drain — its input NOT consumed.
        let e2 = s.send_video(&NAL, Pts90khz::new(3000), true).unwrap_err();
        assert_eq!(e2.input_consumed, Some(false));
    }

    /// Mux-source rejection is atomic → `Some(false)`.
    #[test]
    fn mux_error_reports_not_consumed() {
        let failing = Arc::new(AtomicBool::new(false));
        let sink = Arc::new(Mutex::new(Vec::new()));
        let s = mk_sender(failing, sink);
        // No KLV stream is configured on `mk_sender`'s program → rejected
        // with `MuxError::NoKlvStreamsConfigured` before any state
        // mutation (checked first thing in `Muxer::push_klv`).
        let err = s.send_klv(&[0u8; 4], Pts90khz::new(0), 0).unwrap_err();
        assert_eq!(err.input_consumed, Some(false));
    }

    /// Closed sender → `Some(false)`.
    #[test]
    fn closed_reports_not_consumed() {
        let failing = Arc::new(AtomicBool::new(false));
        let sink = Arc::new(Mutex::new(Vec::new()));
        let s = mk_sender(failing, sink);
        s.close();
        let err = s.send_video(&NAL, Pts90khz::new(0), true).unwrap_err();
        assert_eq!(err.input_consumed, Some(false));
    }
}

#[cfg(test)]
mod cancel_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tst_core::mpegts::mux::{KlvStreamType, MuxerProgramConfigBuilder, VideoCodec};
    use tst_core::transport::{Transport, TransportCancel, TransportError};

    /// Mock transport whose send_bytes blocks (parks) until cancel is
    /// triggered, simulating libsrt's send buffer being full.
    struct ParkableTransport {
        cancelled: Arc<AtomicBool>,
    }
    struct ParkableCancel {
        cancelled: Arc<AtomicBool>,
    }
    impl TransportCancel for ParkableCancel {
        fn cancel(&self) {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }
    impl Transport for ParkableTransport {
        fn send_bytes(&mut self, _: &[u8]) -> Result<(), TransportError> {
            // Spin-park until cancelled, then return Broken.
            for _ in 0..1000 {
                if self.cancelled.load(Ordering::SeqCst) {
                    return Err(TransportError::Broken {
                        msg: "cancelled".into(),
                        errno_code: None,
                        cause: BrokenCause::Unspecified,
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(TransportError::Broken {
                msg: "test timeout (cancel never fired)".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            })
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn close(&mut self) {
            self.cancelled.store(true, Ordering::SeqCst);
        }
        fn is_alive(&self) -> bool {
            !self.cancelled.load(Ordering::SeqCst)
        }
        fn cancel_handle(&self) -> Option<std::sync::Arc<dyn TransportCancel + Send + Sync>> {
            Some(std::sync::Arc::new(ParkableCancel {
                cancelled: self.cancelled.clone(),
            }))
        }
    }

    /// `close()` from another thread unblocks a sender thread parked
    /// inside `send_video()`. Without cancel-first, the close call would
    /// itself block on the inner Mutex held by the parked sender.
    #[test]
    fn close_unblocks_parked_sender_thread() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x100, VideoCodec::H264);
            prog.add_klv(0x101, KlvStreamType::PrivateData, false);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().unwrap()
        };
        let s = Arc::new(
            MuxSender::new(
                ParkableTransport {
                    cancelled: cancelled.clone(),
                },
                cfg,
            )
            .unwrap(),
        );
        let s_send = s.clone();

        let nal = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xBB];
        let send_thread =
            std::thread::spawn(move || s_send.send_video(&nal, Pts90khz::new(0), true));

        // Give the send thread a moment to grab the lock and park.
        std::thread::sleep(std::time::Duration::from_millis(50));

        // close() must NOT itself block on the inner Mutex; it cancels
        // first, the parked send returns Broken, then close lock-acquires.
        let close_start = std::time::Instant::now();
        s.close();
        let close_elapsed = close_start.elapsed();

        // Allow generous slack: the send thread sleeps 1ms between
        // checks, so the parked send returns within ~5ms after cancel.
        assert!(
            close_elapsed < std::time::Duration::from_millis(200),
            "close() blocked for {close_elapsed:?} — should have been near-instant via cancel"
        );

        let result = send_thread.join().unwrap();
        assert!(matches!(
            result,
            Err(ref err) if err.kind == ShellErrorKind::TransportBroken
        ));
    }

    /// Transport that panics on every `send_bytes` call. Used to poison
    /// the inner `Mutex<Inner<T>>` by triggering a panic with the lock
    /// held (the `MutexGuard` drops during unwinding, auto-poisoning).
    struct PanicOnSend;
    impl Transport for PanicOnSend {
        fn send_bytes(&mut self, _b: &[u8]) -> Result<(), TransportError> {
            panic!("intentional poison-the-lock panic for poisoned-lock test")
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn close(&mut self) {}
        fn is_alive(&self) -> bool {
            true
        }
    }

    /// Build a `MuxerConfig` with one stream of every type under program 1.
    /// This is the canonical config shared by Tasks 2 and 3 poisoned-lock
    /// regression tests — stream handle indices are deterministic for a given
    /// config layout, so both tests can use handles snapshotted from any
    /// sender built from this config.
    fn all_streams_config() -> MuxerConfig {
        use tst_core::mpegts::mux::{AudioCodec, KlvStreamType, SubtitleCodec};
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        prog.add_audio(0x102, AudioCodec::Aac);
        prog.add_subtitle(0x103, SubtitleCodec::WebVttInTs);
        prog.add_data(0x104, 0xF0, /*carries_pts=*/ true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    }

    /// Create a `MuxSender<PanicOnSend>` whose inner mutex has been
    /// poisoned. Poison mechanism: a spawned thread calls `send_video`
    /// which reaches `transport.send_bytes`, which panics while holding
    /// the `MutexGuard`, auto-poisoning `Inner` during stack unwinding.
    ///
    /// The returned `Arc` is shared-ownership so callers can invoke
    /// methods on the already-poisoned value without taking ownership.
    fn poison_sender() -> Arc<MuxSender<PanicOnSend>> {
        let sender = Arc::new(MuxSender::new(PanicOnSend, all_streams_config()).unwrap());
        let s = sender.clone();
        let h = std::thread::spawn(move || {
            // Minimal Annex-B IDR NAL — the muxer emits TS packets into
            // drain_muxer, which calls send_bytes, which panics.
            let nal = [0x00, 0x00, 0x00, 0x01, 0x67, 0xBB];
            let _ = s.send_video(&nal, Pts90khz::new(0), true);
        });
        let _ = h.join(); // panics; inner is now poisoned
        sender
    }

    /// PIPE-02 secondary regression: explicit `close()` on a `MuxSender`
    /// whose inner mutex was poisoned by a panic-during-send must NOT
    /// itself panic — it returns silently via the `if let Ok` branch,
    /// matching `Drop`'s graceful poisoned-lock catch.
    #[test]
    fn close_does_not_panic_on_poisoned_lock() {
        // Surviving the call IS the assertion.
        poison_sender().close();
    }

    /// Regression: every fallible-return method on
    /// `MuxSender` converts a poisoned inner lock to a typed error instead
    /// of panicking. The 10 `send_*` methods must return a `MuxSenderError`
    /// whose kind is `ShellErrorKind::TransportBroken`; the 2
    /// `*_handles_for_program` methods must return `MuxError::ProgramNotFound`.
    #[test]
    fn mux_sender_inner_lock_poisoned_returns_broken_error() {
        // Snapshot handles from a fresh (unpoisoned) sender with the same
        // config — stream handles are packed indices deterministic for a given
        // config layout, so any sender built from all_streams_config() has the
        // same handles.
        let fresh = MuxSender::new(PanicOnSend, all_streams_config()).unwrap();
        let video_h = fresh.video_handles()[0];
        let klv_h = fresh.klv_handles()[0];
        let audio_h = fresh.audio_handles()[0];
        let subtitle_h = fresh.subtitle_handles()[0];
        let data_h = fresh.data_handles()[0];
        drop(fresh);

        let sender = poison_sender();

        // --- 10 send_* methods: must return TransportBroken, not panic ---

        let nal = [0x00, 0x00, 0x00, 0x01, 0x67, 0xBB];
        let pts = Pts90khz::new(0);

        let err = sender.send_video(&nal, pts, true).unwrap_err();
        assert_eq!(
            err.kind,
            ShellErrorKind::TransportBroken,
            "send_video: expected TransportBroken, got {:?}",
            err.kind
        );
        assert!(
            matches!(&err.source, MuxSenderErrorSource::Transport(TransportError::Broken { msg, .. }) if msg.contains("send_video")),
            "send_video: message should contain 'send_video', got: {err:?}"
        );

        let err = sender.send_klv(&[0xAA, 0xBB, 0xCC], pts, 0x00).unwrap_err();
        assert_eq!(err.kind, ShellErrorKind::TransportBroken, "send_klv");
        assert!(
            matches!(&err.source, MuxSenderErrorSource::Transport(TransportError::Broken { msg, .. }) if msg.contains("send_klv")),
            "send_klv: message should contain 'send_klv', got: {err:?}"
        );

        let err = sender.send_video_to(video_h, &nal, pts, true).unwrap_err();
        assert_eq!(err.kind, ShellErrorKind::TransportBroken, "send_video_to");
        assert!(
            matches!(&err.source, MuxSenderErrorSource::Transport(TransportError::Broken { msg, .. }) if msg.contains("send_video_to")),
            "send_video_to: message should contain 'send_video_to', got: {err:?}"
        );

        let err = sender
            .send_klv_to(klv_h, &[0xAA, 0xBB, 0xCC], pts, 0x00)
            .unwrap_err();
        assert_eq!(err.kind, ShellErrorKind::TransportBroken, "send_klv_to");
        assert!(
            matches!(&err.source, MuxSenderErrorSource::Transport(TransportError::Broken { msg, .. }) if msg.contains("send_klv_to")),
            "send_klv_to: message should contain 'send_klv_to', got: {err:?}"
        );

        let err = sender.send_audio(&[0xFF; 32], pts).unwrap_err();
        assert_eq!(err.kind, ShellErrorKind::TransportBroken, "send_audio");
        assert!(
            matches!(&err.source, MuxSenderErrorSource::Transport(TransportError::Broken { msg, .. }) if msg.contains("send_audio")),
            "send_audio: message should contain 'send_audio', got: {err:?}"
        );

        let err = sender.send_audio_to(audio_h, &[0xFF; 32], pts).unwrap_err();
        assert_eq!(err.kind, ShellErrorKind::TransportBroken, "send_audio_to");
        assert!(
            matches!(&err.source, MuxSenderErrorSource::Transport(TransportError::Broken { msg, .. }) if msg.contains("send_audio_to")),
            "send_audio_to: message should contain 'send_audio_to', got: {err:?}"
        );

        let err = sender.send_subtitle(b"WEBVTT cue", pts).unwrap_err();
        assert_eq!(err.kind, ShellErrorKind::TransportBroken, "send_subtitle");
        assert!(
            matches!(&err.source, MuxSenderErrorSource::Transport(TransportError::Broken { msg, .. }) if msg.contains("send_subtitle")),
            "send_subtitle: message should contain 'send_subtitle', got: {err:?}"
        );

        let err = sender
            .send_subtitle_to(subtitle_h, b"WEBVTT cue", pts)
            .unwrap_err();
        assert_eq!(
            err.kind,
            ShellErrorKind::TransportBroken,
            "send_subtitle_to"
        );
        assert!(
            matches!(&err.source, MuxSenderErrorSource::Transport(TransportError::Broken { msg, .. }) if msg.contains("send_subtitle_to")),
            "send_subtitle_to: message should contain 'send_subtitle_to', got: {err:?}"
        );

        let err = sender.send_data(&[0x42; 32], pts).unwrap_err();
        assert_eq!(err.kind, ShellErrorKind::TransportBroken, "send_data");
        assert!(
            matches!(&err.source, MuxSenderErrorSource::Transport(TransportError::Broken { msg, .. }) if msg.contains("send_data")),
            "send_data: message should contain 'send_data', got: {err:?}"
        );

        let err = sender.send_data_to(data_h, &[0x42; 32], pts).unwrap_err();
        assert_eq!(err.kind, ShellErrorKind::TransportBroken, "send_data_to");
        assert!(
            matches!(&err.source, MuxSenderErrorSource::Transport(TransportError::Broken { msg, .. }) if msg.contains("send_data_to")),
            "send_data_to: message should contain 'send_data_to', got: {err:?}"
        );

        // --- 2 *_handles_for_program methods: must return ProgramNotFound ---

        let err = sender.audio_handles_for_program(1).unwrap_err();
        assert!(
            matches!(err, MuxError::ProgramNotFound { program_number: 1 }),
            "audio_handles_for_program: expected ProgramNotFound{{1}}, got: {err:?}"
        );

        let err = sender.subtitle_handles_for_program(1).unwrap_err();
        assert!(
            matches!(err, MuxError::ProgramNotFound { program_number: 1 }),
            "subtitle_handles_for_program: expected ProgramNotFound{{1}}, got: {err:?}"
        );
    }

    /// Regression: every infallible-return method on a `MuxSender` with a
    /// poisoned inner mutex returns a safe default instead of panicking.
    /// Safe defaults match the "no live muxer state" answer.
    ///
    /// Uses the same `poison_sender()` helper as the recoverable-path test —
    /// same poison mechanism, same config layout.
    #[test]
    fn mux_sender_inner_lock_poisoned_returns_safe_default() {
        let sender = poison_sender();

        // *_handles → empty Vec
        assert!(
            sender.video_handles().is_empty(),
            "video_handles: expected empty vec on poisoned lock"
        );
        assert!(
            sender.klv_handles().is_empty(),
            "klv_handles: expected empty vec on poisoned lock"
        );
        assert!(
            sender.audio_handles().is_empty(),
            "audio_handles: expected empty vec on poisoned lock"
        );
        assert!(
            sender.subtitle_handles().is_empty(),
            "subtitle_handles: expected empty vec on poisoned lock"
        );
        assert!(
            sender.data_handles().is_empty(),
            "data_handles: expected empty vec on poisoned lock"
        );

        // stats → MuxSenderStats::default() (zeroed)
        let s = sender.stats();
        assert_eq!(
            s.bytes_sent, 0,
            "stats.bytes_sent should be 0 on poisoned lock"
        );
        assert_eq!(
            s.packets_sent, 0,
            "stats.packets_sent should be 0 on poisoned lock"
        );

        // socket_stats → None
        assert!(
            sender.socket_stats().is_none(),
            "socket_stats: expected None on poisoned lock"
        );

        // stream_codec_stats → None (any PID)
        assert!(
            sender.stream_codec_stats(0x100).is_none(),
            "stream_codec_stats: expected None on poisoned lock"
        );

        // is_alive → false
        assert!(
            !sender.is_alive(),
            "is_alive: expected false on poisoned lock"
        );

        // reset_stats → must not panic (call and proceed)
        sender.reset_stats();
    }

    fn video_only_config() -> MuxerConfig {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.pcr_pid(0x1011);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    }

    /// Interop-arc regression: explicit close() must be as lossless as Drop.
    /// Models SRT/RIST semantics where cancel() kills in-flight/subsequent
    /// sends: if close() cancels before draining pending_bytes, the tail is
    /// forfeited.
    #[derive(Default)]
    struct TailLossState {
        sent: Vec<Vec<u8>>,
        reject_sends: bool, // one-shot Backpressure window to seed pending_bytes
        cancelled: bool,    // set by the cancel handle; sends fail afterwards
    }
    struct TailLossTransport(Arc<std::sync::Mutex<TailLossState>>);
    struct TailLossCancel(Arc<std::sync::Mutex<TailLossState>>);
    impl TransportCancel for TailLossCancel {
        fn cancel(&self) {
            self.0.lock().unwrap().cancelled = true;
        }
    }
    impl Transport for TailLossTransport {
        fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
            let mut s = self.0.lock().unwrap();
            if s.cancelled {
                return Err(TransportError::Broken {
                    msg: "cancelled".into(),
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                });
            }
            if s.reject_sends {
                return Err(TransportError::Backpressure {
                    msg: "full".into(),
                    errno_code: None,
                });
            }
            s.sent.push(msg.to_vec());
            Ok(())
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn is_alive(&self) -> bool {
            !self.0.lock().unwrap().cancelled
        }
        fn close(&mut self) {}
        fn socket_stats(&self) -> Option<tst_core::transport::SocketStats> {
            None
        }
        fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
            Some(Arc::new(TailLossCancel(self.0.clone())))
        }
    }

    #[test]
    fn explicit_finish_drains_pending_without_cancel() {
        // Reshaped at the 0.5.0 release gate: the lossless explicit
        // shutdown is finish(), which drains WITHOUT cancelling (cancel
        // is what forfeited the tail); close() is the prompt/lossy
        // primitive and cancels first by contract.
        let state = Arc::new(std::sync::Mutex::new(TailLossState {
            reject_sends: true,
            ..Default::default()
        }));
        let sender = MuxSender::new(TailLossTransport(state.clone()), video_only_config()).unwrap();
        // Seed pending_bytes: transport rejects with Backpressure.
        let nal = [0x00, 0x00, 0x00, 0x01, 0x67, 0xBB];
        let _ = sender.send_video(&nal, Pts90khz::new(0), true);
        state.lock().unwrap().reject_sends = false; // transport healthy again
        sender.finish().expect("drain on the healed transport");
        let s = state.lock().unwrap();
        let total: usize = s.sent.iter().map(|c| c.len()).sum();
        assert!(
            total > 0,
            "pending tail lost: finish() failed to drain before closing"
        );
        assert!(total % 188 == 0);
        // finish() must not cancel — the drain needs the live transport.
        assert!(!s.cancelled);
    }

    // into_inner() is std-only (see its rustdoc); gate its tests to match
    // rather than relying on the module's other std-only code (e.g.
    // std::thread::spawn above) to carry them.
    #[cfg(feature = "std")]
    #[test]
    fn into_inner_returns_transport_with_all_bytes() {
        let state = Arc::new(std::sync::Mutex::new(TailLossState::default()));
        let sender = MuxSender::new(TailLossTransport(state.clone()), video_only_config()).unwrap();
        let nal = [0x00, 0x00, 0x00, 0x01, 0x67, 0xBB];
        sender.send_video(&nal, Pts90khz::new(0), true).unwrap();
        let _t: TailLossTransport = sender.into_inner();
        let s = state.lock().unwrap();
        let total: usize = s.sent.iter().map(|c| c.len()).sum();
        assert!(total > 0 && total % 188 == 0);
        assert!(
            !s.cancelled,
            "into_inner must not cancel/close the transport"
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn into_inner_drains_pending_bytes() {
        let state = Arc::new(std::sync::Mutex::new(TailLossState {
            reject_sends: true,
            ..Default::default()
        }));
        let sender = MuxSender::new(TailLossTransport(state.clone()), video_only_config()).unwrap();
        let nal = [0x00, 0x00, 0x00, 0x01, 0x67, 0xBB];
        let _ = sender.send_video(&nal, Pts90khz::new(0), true); // pending seeded
        state.lock().unwrap().reject_sends = false;
        let _t = sender.into_inner();
        assert!(
            state
                .lock()
                .unwrap()
                .sent
                .iter()
                .map(|c| c.len())
                .sum::<usize>()
                > 0
        );
    }
}
