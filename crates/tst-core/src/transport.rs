//! Transport trait definitions — the seam between shells and the wire.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! This module contains both send-side and receive-side transport traits.
//! Concrete implementations (SRT, file-replay, in-memory channels) live
//! in their own crates; only the abstract contract lives here.
//!
//! # Cancel / close / liveness contract (normative)
//!
//! Every `Transport` / `RecvTransport` implementation honours the rows
//! below; the `conformance` kit (behind the `conformance` feature) is the executable form and
//! each transport crate runs it in its `tests/*conformance.rs`.
//!
//! | Transport | after own `close()` | cancel during a parked op | `is_alive()` after `Broken` / after cancel | handle type |
//! |---|---|---|---|---|
//! | SRT (`SrtTransport`) | `Closed` | `ExplicitClose` | false / false | `ResourceCancel` |
//! | TCP / TLS (`TcpTransport`) | `Closed` | `ExplicitClose` | false / false | `TcpCancelHandle` |
//! | TCP listener (`TcpListener`) | `accept` → `Closed`; `close()` DOES latch `is_cancelled()` (a listener has no peer-EOF path, so its only terminal event is the caller stopping it) | `Closed` (a listener is not a `Transport`; the `TcpError` layer has no cancel variant) | n/a | `TcpCancelHandle` |
//! | RTP send / recv (`RtpTransport` / `RtpRecvTransport`, incl. RTSP-client recv) | `Closed` | `ExplicitClose` | false / false | `RtpCancelHandle` |
//! | UDP send / recv | `Closed` | `ExplicitClose` | false / false | `UdpCancelHandle` |
//! | RIST send / recv | `Closed` | `ExplicitClose` | false / false | `RistCancelHandle` |
//! | `ManagedTransport` (send) | `Closed` | `ExplicitClose` | false / false | `ManagedCancel` |
//! | `ManagedRecvTransport` (recv) | `ExplicitClose` (its own close is a caller-initiated end; see its docs) | `ExplicitClose` | false / false | `ManagedRecvCancel` |
//!
//! Shared rows, every transport: a second `close()` is a no-op;
//! `cancel_handle()` returns `Some` (bare test mocks may return `None`);
//! `RecvTransport::max_payload()` is the protocol's deliverable ceiling (never
//! the local send budget); an EMPTY destination buffer makes `recv_bytes`
//! return `Ok(0)` without touching the socket or the liveness flag. A cancel
//! that lands after a successful op is not an error — the NEXT op fails with
//! `ExplicitClose`. `Broken` after a cancel is impossible by construction.
//!
//! # `is_cancelled()` — the caller's intent, never liveness
//!
//! `is_cancelled()` answers **"did the CALLER end this transport?"**:
//! `false` on a fresh handle; `true` after `cancel()` on it or any alias,
//! and after the caller's own `close()` where that close is implemented by
//! firing the same handle (`Socket::close`, the managed wrappers'
//! `terminal_signal`, so `ManagedCancel` also reads `true`
//! after `close()`/`Drop` of the wrapper).
//!
//! It is **never** a liveness proxy: a peer EOF or a wire failure leaves it
//! `false`, however dead the transport is (the kit row
//! `peer_eof_is_not_a_cancel`). This line is load-bearing — the bindings
//! choose between "caller closed" and "stream ended" from this bit alone,
//! so a handle that latches on a peer-side end turns every clean EOF into a
//! reported caller close. Two implementations had to be split to honour it:
//! `TcpCancelHandle` (was `!alive`) and `ResourceCancel` (was "the closer
//! has run", which `Socket::drop` also triggers on every error path that
//! retires the socket — hence `ResourceCancel::close_without_cancel` for
//! owner teardown).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use thiserror::Error;

// Executable form of the contract table above (Provisional), behind the
// non-default `conformance` feature: every transport crate enables it from its
// dev-dependencies and runs the kit in `tests/*conformance.rs`. The summary
// rustdoc lives in the module file's own `//!` docs — an outer `///` here
// would make rustdoc resolve the module's intra-doc links in THIS module's
// scope and break every one of them.
#[cfg(feature = "conformance")]
pub mod conformance;

// ============================================================
// Wire-level socket stats (shared between Transport + RecvTransport)
// ============================================================

crate::binding_record! {
    /// Wire-level transport stats sourced from the underlying network library:
    /// the same sixteen counters on every transport.
    ///
    /// All bandwidth fields are in bits per second; RTT is in microseconds;
    /// buffer-depth fields are in packets. A transport that does not expose a
    /// particular value reports `0`. For the SRT-specific extras (estimated
    /// bandwidth, the symmetric send/recv-side loss split) read the SRT stats
    /// of an `srt://` shell.
    ///
    /// <!-- rust-only -->
    /// A transport that has no telemetry at all reports `None` from
    /// [`Transport::socket_stats`] / [`RecvTransport::socket_stats`] instead
    /// of a struct of zeros.
    ///
    /// **Per-transport mapping documentation:** each concrete `Transport` impl
    /// documents the source of each field in its own rustdoc — see
    /// [`SrtTransport`](https://docs.rs/tst-srt/latest/tst_srt/transport/struct.SrtTransport.html)
    /// for the libsrt `CBytePerfMon` mapping and `RtpTransport` (in `tst-rtp`)
    /// for its per-field mapping table.
    ///
    /// Pre-1.0 the struct is `#[non_exhaustive]` so adding a field is not a
    /// breaking change.
    #[must_use]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    #[non_exhaustive]
    pub struct SocketStats {
        /// Smoothed round-trip time in microseconds. `0` means "not yet
        /// measured" (no samples observed). Transports that don't expose RTT
        /// report `0`; saturates at the 32-bit maximum (about 71 minutes) where the
        /// underlying value exceeds the field range.
        pub rtt_us: u32,

        /// Send-rate estimate in bits per second (application-observed, not
        /// link-layer). `0` when the transport doesn't compute this.
        pub send_bandwidth_bps: u64,
        /// Receive-rate estimate in bits per second. `0` when not computed.
        pub recv_bandwidth_bps: u64,
        /// Link-capacity estimate in bits per second. `0` when the transport
        /// has no link estimate (RTP, for example).
        pub link_bandwidth_bps: u64,

        /// Bytes accepted by the transport for send, cumulative since connect.
        /// Receivers read `0`.
        pub bytes_sent: u64,
        /// Packets accepted by the transport for send. Receivers read `0`.
        pub packets_sent: u64,

        /// Bytes delivered by the transport to the application, cumulative
        /// since connect. Senders read `0`.
        pub bytes_received: u64,
        /// Packets delivered by the transport to the application. Senders
        /// read `0`.
        pub packets_received: u64,

        /// Bytes lost (network drops, not recovered). `0` when the transport
        /// can't compute byte-level loss from its loss-detection mechanism
        /// (RTP, for example, detects loss via sequence-number gaps which
        /// don't carry byte counts).
        pub bytes_lost_recv: u64,
        /// Packets lost on the receive side, cumulative.
        pub packets_lost_recv: u64,
        /// Packets declared lost on the send side via receiver feedback
        /// (NAKs for SRT, RTCP RR fraction-lost for RTP).
        pub packets_lost_send: u64,

        /// Retransmitted packets (sender-side; sum over all retransmit rounds).
        /// `0` for transports without retransmission (plain RTP without
        /// RFC 4588 NACK).
        pub packets_retransmitted: u64,

        /// Packets dropped by the send-side buffer (overrun or drop-late).
        pub packets_dropped_send: u64,
        /// Packets dropped by the receive-side buffer.
        pub packets_dropped_recv: u64,

        /// Current send-side buffer occupancy in packets. Spot reading; not
        /// cumulative. `0` when the transport doesn't expose buffer occupancy.
        pub send_buffer_packets: u32,
        /// Current receive-side buffer occupancy in packets. Spot reading.
        pub recv_buffer_packets: u32,
    }
}

// ============================================================
// Send side: Transport + TransportError + TransportCancel
// ============================================================

/// Failure shape for `Transport::send_bytes`.
///
/// Two semantic categories:
///
/// - **Recoverable** — the transport is alive but momentarily refused
///   (libsrt's `SRT_EASYNCSND` from a full send buffer, e.g.). Caller may
///   retry without rebuilding the transport.
/// - **Broken** — the transport is dead and must be re-established. Plain
///   senders surface this to the caller; `ManagedTransport` triggers
///   reconnect.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum TransportError {
    /// Transport was alive but couldn't accept the bytes right now (full
    /// send buffer, transient backpressure). Retrying the same bytes later
    /// is reasonable.
    ///
    /// `errno_code` carries the wire-level transport error code where the
    /// implementation can supply one. Per-transport documentation defines
    /// the code space:
    /// - `SrtTransport` uses the libsrt major-category error code (see
    ///   `tst-srt` docs for the full code table).
    /// - `RistTransport` (`tst-rist`) uses librist's `rist_sender_data_write`
    ///   return code: `Some(-2)` means the sender queue was full and this one
    ///   message was dropped (retry it; the transport is still alive). Any
    ///   other negative librist code is fatal and rides [`Self::Broken`].
    /// - `RtpTransport`'s send side never produces `Backpressure` (UDP
    ///   either accepts the datagram or surfaces an error); its OS-`errno`
    ///   codes ride [`Self::Broken`]. On the recv side, `tst-rtp`'s
    ///   deadline-bounded methods surface deadline expiry with
    ///   `errno_code: None`: `H264Receiver::recv_au_timeout` returns
    ///   `Err(Backpressure)` directly, while `RtpRecvTransport::recv_timeout`
    ///   maps the same internal signal to `Ok(None)` instead, mirroring
    ///   `UdpRecvTransport::recv_timeout`'s shape.
    /// - Test mocks and in-memory channels pass `None`.
    ///
    /// Surfaced as a typed-source aid for binding-crate consumers
    /// (`tst-jni`, `tst-uniffi`) that discriminate on the wire-level cause
    /// without parsing the message string.
    #[error("transport temporarily unavailable: {msg}")]
    Backpressure {
        /// Human-readable diagnostic detail.
        msg: String,
        /// Wire-level transport errno code; `None` when the implementation
        /// doesn't expose one. See the variant-level doc above for the
        /// per-transport code-space definitions.
        errno_code: Option<i32>,
    },

    /// Transport is dead. Caller must rebuild it (or rely on a managed
    /// wrapper to do so).
    ///
    /// See [`Self::Backpressure`] for the `errno_code` semantics — same
    /// rules apply here. `cause` is the structured discriminator for the
    /// one case callers routinely need to tell apart without parsing
    /// `msg`: a clean end of stream versus a wire failure (see
    /// [`BrokenCause`]).
    #[error("transport broken: {msg}")]
    Broken {
        /// Human-readable diagnostic detail.
        msg: String,
        /// Wire-level transport errno code; `None` when the
        /// implementation doesn't expose one.
        errno_code: Option<i32>,
        /// Why the transport broke. [`BrokenCause::Unspecified`] unless the
        /// producer can vouch for a finer classification.
        cause: BrokenCause,
    },

    /// Transport was already closed.
    #[error("transport closed")]
    Closed,

    /// `len > maximum payload size`. For SRT live mode, the cap is
    /// `SRTO_PAYLOADSIZE` (default 1316). Caller is responsible for
    /// chunking on their own framing semantics.
    #[error("message too large: {len} bytes exceeds payload-size cap of {max} bytes")]
    TooLarge { len: usize, max: usize },

    /// Caller invoked `close()` or `cancel()` on this transport (or on a
    /// shell that owns it). Distinguished from [`Self::Closed`] which means
    /// "peer closed the connection / end-of-stream observed on the wire."
    ///
    /// **Producers:** every cancel-aware transport, once its cancel handle
    /// has fired and a blocking (or subsequent) op observes it — the
    /// per-transport table in the module docs is normative and the
    /// `conformance` kit pins it. `ManagedRecvTransport::recv_bytes`
    /// also produces it after its own `close()`; the bare transports and
    /// `ManagedTransport` produce [`Self::Closed`] there instead (sender
    /// shells map both to `ShellErrorKind::Closed`).
    ///
    /// **Shell-layer mapping (`kind_of_transport`):**
    /// - `ExplicitClose` → `ShellErrorKind::Closed` (caller-initiated)
    /// - `Closed` on a receiver shell → `ShellErrorKind::EndOfStream` (peer-initiated)
    /// - `Closed` on a sender shell → `ShellErrorKind::Closed` (caller-initiated; sender shells expose `close()` and produce this on post-close calls)
    #[error("transport explicitly closed by caller")]
    ExplicitClose,
}

/// The category of a [`TransportError`], for exhaustive matching.
///
/// [`TransportError::kind`] classifies every variant inside this crate with no
/// wildcard, and this enum is deliberately **not** `#[non_exhaustive]`: a
/// new category is meant to be a compile error in every exhaustive match
/// downstream (the binding kind table is one). See `conventions.md`
/// §"Exhaustive kind enums".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportErrorKind {
    /// [`TransportError::Backpressure`].
    Backpressure,
    /// [`TransportError::Broken`].
    Broken,
    /// [`TransportError::Closed`].
    Closed,
    /// [`TransportError::TooLarge`].
    TooLarge,
    /// [`TransportError::ExplicitClose`].
    ExplicitClose,
}

impl TransportError {
    /// This error's [`TransportErrorKind`]. Exhaustive (no wildcard: the match is in
    /// the defining crate).
    #[must_use]
    pub fn kind(&self) -> TransportErrorKind {
        match self {
            TransportError::Backpressure { .. } => TransportErrorKind::Backpressure,
            TransportError::Broken { .. } => TransportErrorKind::Broken,
            TransportError::Closed => TransportErrorKind::Closed,
            TransportError::TooLarge { .. } => TransportErrorKind::TooLarge,
            TransportError::ExplicitClose => TransportErrorKind::ExplicitClose,
        }
    }
}

/// Why a [`TransportError::Broken`] was raised.
///
/// Lets a caller act differently on a clean end of stream than on a wire
/// failure without parsing the error's `msg`. It does **not** change how the
/// error is handled by the shells: `Broken` stays the retryable reconnect
/// trigger for `ManagedTransport` / `ManagedRecvTransport` whatever the
/// cause, and the pipeline shells map it to the same `ShellErrorKind`.
///
/// `#[non_exhaustive]`: match with a wildcard arm; further causes may be
/// added without a major bump.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum BrokenCause {
    /// No finer classification — a read or write failure, a zero-length
    /// write mid-message, an exhausted reconnect budget, … Read `msg` and
    /// `errno_code` for the detail.
    #[default]
    Unspecified,
    /// The peer ended the stream cleanly: a zero-length read — a TCP FIN, or
    /// on `tcps://` a TLS `close_notify` — with nothing lost. The stream is
    /// simply over. Producer today: `tst-tcp`'s `TcpTransport::recv_bytes`.
    /// A zero-length *write* is not a clean EOF (the stream is desynced
    /// mid-message) and stays [`Self::Unspecified`].
    CleanEof,
}

/// One-shot byte transport. Each `send_bytes` call sends exactly one
/// outbound message.
///
/// Implementors are expected to be `Send` so a single transport can move
/// between threads (e.g., a builder thread hands off to a sender thread).
/// They are NOT expected to be `Sync` — concurrent sends through one
/// transport are the caller's responsibility (the sender shells handle
/// this internally where their thread-safety contract requires it).
pub trait Transport: Send {
    /// Send one message. Returns success once libsrt (or the equivalent)
    /// has accepted the bytes — not when they reach the peer.
    ///
    /// On `Err(TransportError::Backpressure)`, the bytes have NOT been
    /// partially consumed; callers may retry the identical slice. On any
    /// other error the transport state is undefined and retrying the same
    /// bytes is not safe (rebuild the transport, or rely on a managed
    /// wrapper).
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError>;

    /// Maximum payload size for this transport. For SRT live mode this is
    /// `SRTO_PAYLOADSIZE` (default 1316). Senders use this to size
    /// per-send buffers and to validate raw-sender input. This is the
    /// **send-side budget**; the receive-side counterpart
    /// ([`RecvTransport::max_payload`]) reports the protocol's
    /// deliverable ceiling instead, which may be larger.
    fn max_payload(&self) -> usize;

    /// Advisory liveness check. Returns `false` when the transport is
    /// known dead (closed or previously broken). `true` does not
    /// guarantee that the next `send_bytes` succeeds — the transport
    /// may die between this call and the send. Callers must handle
    /// `TransportError::Broken` regardless of what `is_alive` returns.
    fn is_alive(&self) -> bool;

    /// Close the transport. Idempotent — calling close twice is fine.
    /// After close, `send_bytes` returns `TransportError::Closed`.
    ///
    /// The `&mut self` requirement means callers (or the sender shells
    /// wrapping the transport) are responsible for serializing close
    /// with concurrent sends. The "close from any thread is always safe"
    /// contract in the wider design lives at the sender-shell layer,
    /// not on `Transport` itself.
    ///
    /// # Asymmetry with [`RecvTransport::close`]
    ///
    /// This method has **no default body** — every `Transport` impl MUST
    /// implement it explicitly. This is the deliberate counterpart to
    /// [`RecvTransport::close`], which defaults to a no-op. See that
    /// method's docs for the full rationale: send-side close owns the
    /// underlying resource and must release it; receive-side close
    /// typically rides on a socket whose lifetime is owned by the paired
    /// send-side transport, so a default no-op is correct for the common
    /// case.
    fn close(&mut self);

    /// Optional cancellation accessor. Implementors that own a wakeable
    /// blocking primitive (a real socket, an MPSC channel sender, etc.)
    /// return `Some(handle)`; pure in-memory test mocks return `None`.
    ///
    /// The returned `Arc` is `Send + Sync` and can be moved or cloned to
    /// any thread; calling `cancel()` while another thread is parked in
    /// [`Self::send_bytes`] makes that parked call return
    /// [`TransportError::ExplicitClose`] (module-doc table), and every
    /// later call returns it at its entry check.
    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        None
    }

    /// Wire-level stats (RTT, packet loss, bandwidth, queue depths) from the
    /// underlying network library. Returns `None` when the transport doesn't
    /// expose comparable telemetry (test mocks, in-memory channels) or when
    /// a managed wrapper has no live inner socket.
    ///
    /// # Errors
    ///
    /// Implementors that fetch stats from a system call MAY swallow a
    /// per-call failure into `None`; this trait does not surface a
    /// distinct error variant. The transport's liveness (see
    /// [`Self::is_alive`]) is the right channel for connection state.
    fn socket_stats(&self) -> Option<SocketStats> {
        None
    }

    /// The concrete transport, for a binding that must reach a
    /// transport-specific accessor through a type-erased shell (e.g. SRT's
    /// libsrt statistics). `None` unless the transport opts in.
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        None
    }

    /// Called by `MuxSender` once per video access unit, before it is muxed.
    ///
    /// A segmenting transport cuts its open segment here when `key_frame`
    /// is set and returns `Ok(true)`, so the caller re-emits PAT/PMT and the
    /// keyframe opens the new segment. Every other transport keeps the
    /// default, which does nothing and returns `Ok(false)`. An `Err` fails
    /// the send before the access unit is touched.
    fn on_video_au(
        &mut self,
        pts: crate::mpegts::common::Pts90khz,
        key_frame: bool,
    ) -> Result<bool, TransportError> {
        let _ = (pts, key_frame);
        Ok(false)
    }
}

/// Type-erased cancel-handle accessor returned by
/// [`Transport::cancel_handle`] and [`RecvTransport::cancel_handle`].
///
/// `cancel()` from any thread interrupts the transport's current blocking
/// send or receive; the parked call returns
/// [`TransportError::ExplicitClose`] (the per-transport table in the
/// module docs is normative, and the `conformance` kit pins it).
/// Idempotent.
///
/// `is_cancelled()` reads the handle's latch: `false` on a fresh handle,
/// `true` once `cancel()` has run on it or on any clone/alias sharing the
/// same underlying resource — a handle obtained from the transport AFTER
/// the cancel reads `true` too. It is never a liveness proxy: a peer
/// disconnect or a wire failure leaves it `false` (the kit row
/// `peer_eof_is_not_a_cancel`), because the bindings relabel a
/// caller-initiated end from exactly this bit. Where a transport's own
/// `close()` is implemented by firing the same handle — `Socket::close` and
/// the send-side `ManagedCancel` (whose `close()`/`Drop` run the same
/// `terminal_signal`) — `is_cancelled()` also reads `true` after
/// that `close()`. `ManagedRecvCancel` does NOT: its `close()` only clears
/// the slot, so it reads `true` for a cross-thread cancel alone. Callers
/// that need "cancelled by someone else" keep their own flag on top (the
/// binding layer's `Owned::is_cancelled` does exactly that).
///
/// # Handle aliases vs composing wrappers
///
/// "Any clone/alias" above means another handle over the SAME underlying
/// resource — two `TcpCancelHandle`s minted from one transport, say. Those
/// must share one latch, and the `conformance` row `is_cancelled_flips`
/// checks it.
///
/// It does NOT extend to a wrapper that OWNS an inner handle and forwards
/// `cancel()` to it (the bindings' `Owned`, Python's `CancelSource`). Such a
/// wrapper's `is_cancelled()` reports ITS OWN latch: it answers "was this
/// object cancelled", which is the question its callers ask. Composing the
/// inner handle's latch in is the job of whichever layer needs both — in
/// this tree exactly one does, `Owned::is_cancelled`, and it ORs them.
/// Pushing the OR further down would widen what a binding's user-visible
/// `is_cancelled()` answers with nobody asking for it.
///
/// `Send + Sync` is required so consumers can stash one in an
/// `Arc<dyn TransportCancel>` and share it across worker threads.
pub trait TransportCancel: Send + Sync {
    /// Interrupt the transport's current blocking call. Idempotent.
    fn cancel(&self);
    /// `true` once [`cancel`](Self::cancel) has run on this handle or on
    /// any alias of it (see the trait docs). Required — every handle has a
    /// latch to read.
    fn is_cancelled(&self) -> bool;
}

/// Why a receive stream ended, recorded once by the site that saw it end.
///
/// One vocabulary for every receive surface; each surface produces only a
/// subset (the table below), enforced where it records. Match with a
/// wildcard arm: the enum is `#[non_exhaustive]` and grows with new
/// surfaces (a WebRTC session will add its own).
///
/// | Surface | Can end with |
/// |---|---|
/// | `tst_pipeline::ManagedDemuxReceiver` | `ReconnectExhausted`, `Cancelled` |
/// | `tst_rtp::RtpRecvTransport` / `H264Receiver` fed by an `RtspClient` | `CleanTeardown`, `SessionExpired`, `KeepaliveFailed`, `TransportFailed`, `ProtocolError`, `Cancelled` |
/// | a plain `rtp://` `RtpRecvTransport` | `Cancelled` |
/// | the application transport of an RTSP publish mount | `CleanTeardown`, `Cancelled` |
///
/// The C binding reports these through `tst_stream_end_reason`
/// (`EndOfStream` → `CLEAN_TEARDOWN`, `ReconnectExhausted` →
/// `TRANSPORT_FAILED`).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    /// The transport reported a clean end of stream that was not a
    /// reconnect budget running out. No surface produces it today; it is
    /// kept for a receive transport that can tell the two apart.
    EndOfStream,
    /// A reconnecting receiver gave up after exhausting its policy's
    /// attempts — the peer never came back.
    ReconnectExhausted,
    /// The caller cancelled or closed the receiver — not a wire failure.
    Cancelled,
    /// The peer closed the connection in an orderly way (TCP EOF, or an
    /// RTSP mount closed by its server) with no protocol or transport error.
    CleanTeardown,
    /// The server no longer honours the session: a keepalive was answered
    /// `454 Session Not Found`.
    SessionExpired,
    /// The keepalive thread failed to encode or send a ping.
    KeepaliveFailed {
        /// Detail from the underlying failure.
        msg: String,
    },
    /// A hard I/O error on the underlying transport (a read failure other
    /// than a clean EOF).
    TransportFailed {
        /// Detail from the underlying failure.
        msg: String,
    },
    /// The peer violated the wire protocol (oversized or malformed framing,
    /// a queue flood) and the session was failed.
    ProtocolError {
        /// Detail from the underlying failure.
        msg: String,
    },
}

/// A shared, first-writer-wins cell holding a stream's [`EndReason`].
///
/// Clones share one cell, and a value recorded on one thread is visible to
/// a clone read on another, so a watchdog can poll a receiver another
/// thread drives — and keep polling after the receiver is gone, if it took
/// the handle first. The first site to see the stream end owns the
/// reason; a later, less specific signal cannot overwrite it.
#[cfg(feature = "std")]
#[derive(Debug, Clone, Default)]
pub struct EndReasonHandle(std::sync::Arc<std::sync::OnceLock<EndReason>>);

#[cfg(feature = "std")]
impl EndReasonHandle {
    /// Record `r` unless a reason is already recorded.
    pub fn record(&self, r: EndReason) {
        let _ = self.0.set(r);
    }

    /// The recorded reason, or `None` while the stream is live (or when it
    /// ended through a path that records nothing).
    #[must_use]
    pub fn get(&self) -> Option<EndReason> {
        self.0.get().cloned()
    }
}

// ============================================================
// Receive side: RecvTransport
// ============================================================

/// Receive-side counterpart to [`Transport`].
///
/// Each receive shell (`RawReceiver`, `Receiver`, `DemuxReceiver`) is generic
/// over a `RecvTransport`. `SrtTransport` implements both `Transport` and
/// `RecvTransport`. Test mocks and file-replay sources implement this trait
/// without needing a real socket.
pub trait RecvTransport: Send {
    /// Receive one message into `buf`. Returns the number of bytes written.
    ///
    /// - `Err(TransportError::Closed)` once the transport has been closed by
    ///   its own `close()` (bare transports) or the peer ended the stream
    ///   where the protocol can say so;
    /// - `Err(TransportError::Broken)` on a wire failure (the transport is
    ///   dead; `is_alive()` reads `false` afterwards);
    /// - `Err(TransportError::ExplicitClose)` once the cancel handle has
    ///   fired — for the call that was parked and for every later call;
    /// - `Err(TransportError::Backpressure)` on a recv timeout — the
    ///   transport is still alive and the caller may retry;
    /// - `Ok(0)` immediately for an EMPTY `buf`, touching nothing
    ///   (the kit row `empty_recv_is_noop`);
    /// - never `Ok(0)` for a NON-empty `buf`: a datagram transport that
    ///   receives an empty datagram skips it (tst-udp, tst-rtp) or reports
    ///   `Backpressure` (tst-rist) — the receive shells treat `Ok(0)` as
    ///   "closed".
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;

    /// Upper bound on the bytes a single `recv_bytes` call may deliver.
    /// Receive shells size their internal buffers from this value on
    /// construction, so it must be the **deliverable ceiling of the
    /// underlying protocol** — the largest message a conformant remote
    /// sender can legally produce — NOT the local send-side packet-size
    /// budget (that is [`Transport::max_payload`]). A remote peer does
    /// not know or honor our local packet-size configuration; returning
    /// the local budget here makes conformant oversize traffic surface
    /// as a broken transport (or silent loss) at the shell layer.
    fn max_payload(&self) -> usize;

    /// Advisory liveness check. Returns `false` when the transport is known
    /// dead. `true` does not guarantee the next `recv_bytes` succeeds.
    fn is_alive(&self) -> bool;

    /// Close the transport. Idempotent. After close, `recv_bytes` returns
    /// `TransportError::Closed`.
    ///
    /// # Asymmetry with [`Transport::close`] — intentional, not an oversight
    ///
    /// This method defaults to a no-op, while [`Transport::close`] has no
    /// default and MUST be implemented by every send-side impl. The two
    /// halves of the contract form a deliberate pair:
    ///
    /// - **Send-side close is mandatory.** Every `Transport` impl owns its
    ///   underlying resource (a libsrt socket, a file handle, an MPSC
    ///   channel sender) and is responsible for releasing it.
    /// - **Receive-side close is OFTEN a no-op.** The typical `RecvTransport`
    ///   impl holds a *shared* socket whose lifetime is owned by the matching
    ///   `Transport` (e.g., `SrtTransport` implements both traits over one
    ///   socket; closing it twice would be incorrect — at best redundant,
    ///   at worst a double-close error from the underlying library).
    ///
    /// Defaulting to no-op means simple `RecvTransport` impls (test mocks,
    /// channel-backed receivers that share the send-side handle) get the
    /// right behavior for free.
    ///
    /// # When you MUST override
    ///
    /// Impls that own a resource NOT shared with a paired `Transport` MUST
    /// override this default. Examples:
    ///
    /// - File-backed recv (owns a `File` handle to flush/close).
    /// - Network-backed recv that does not share its socket with a sender.
    /// - Any impl whose `recv_bytes` parks on a wakeable primitive that
    ///   needs explicit shutdown to release a thread blocked elsewhere.
    ///
    /// Forgetting to override in these cases leaks the resource silently —
    /// the compiler will not flag it. If in doubt, override and call the
    /// underlying close; an extra explicit close is preferable to a leak.
    fn close(&mut self) {}

    /// Optional cancellation accessor. Wakes a thread parked in `recv_bytes`,
    /// which returns [`TransportError::ExplicitClose`].
    /// See [`Transport::cancel_handle`] for the general shape.
    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        None
    }

    /// Wire-level stats. See [`Transport::socket_stats`] for shape +
    /// failure semantics; for receivers `bytes_sent` / `packets_sent`
    /// will read 0 on libsrt.
    fn socket_stats(&self) -> Option<SocketStats> {
        None
    }

    /// The concrete transport, for a binding that must reach a
    /// transport-specific accessor through a type-erased shell (e.g. SRT's
    /// libsrt statistics). `None` unless the transport opts in.
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        None
    }

    /// A handle reporting why this stream ended, for transports that
    /// record one ([`EndReason`]'s table lists them). Obtain it before
    /// moving the transport into a shell or a binding handle. Defaults to
    /// `None`.
    #[cfg(feature = "std")]
    fn end_reason_handle(&self) -> Option<EndReasonHandle> {
        None
    }
}

// ============================================================
// Blanket impls for Box<T: ?Sized>
// ============================================================
//
// These let `Box<dyn Transport>` and `Box<dyn RecvTransport>` satisfy the
// `T: Transport` / `T: RecvTransport` trait bounds on the pipeline shells, so
// a caller can erase the transport type: `MuxSender<Box<dyn Transport>>`.
//
// Plain forwarding — no behavior change. Both source traits are object-safe;
// these impls let consumers wrap any `Box<dyn TraitObj>` exactly the same way
// they'd wrap a concrete `T: Trait + Sized` value.

impl<T: Transport + ?Sized> Transport for Box<T> {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        (**self).send_bytes(msg)
    }
    fn max_payload(&self) -> usize {
        (**self).max_payload()
    }
    fn is_alive(&self) -> bool {
        (**self).is_alive()
    }
    fn close(&mut self) {
        (**self).close()
    }
    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        (**self).cancel_handle()
    }
    fn socket_stats(&self) -> Option<SocketStats> {
        (**self).socket_stats()
    }
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        (**self).as_any()
    }
    fn on_video_au(
        &mut self,
        pts: crate::mpegts::common::Pts90khz,
        key_frame: bool,
    ) -> Result<bool, TransportError> {
        (**self).on_video_au(pts, key_frame)
    }
}

impl<T: RecvTransport + ?Sized> RecvTransport for Box<T> {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        (**self).recv_bytes(buf)
    }
    fn max_payload(&self) -> usize {
        (**self).max_payload()
    }
    fn is_alive(&self) -> bool {
        (**self).is_alive()
    }
    fn close(&mut self) {
        (**self).close()
    }
    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        (**self).cancel_handle()
    }
    fn socket_stats(&self) -> Option<SocketStats> {
        (**self).socket_stats()
    }
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        (**self).as_any()
    }
    #[cfg(feature = "std")]
    fn end_reason_handle(&self) -> Option<EndReasonHandle> {
        (**self).end_reason_handle()
    }
}

#[cfg(test)]
mod blanket_impl_tests {
    use super::*;

    /// Smoke test: `Box<dyn Transport>` must satisfy `T: Transport`, so a shell
    /// over an erased transport (`MuxSender<Box<dyn Transport>>`) compiles.
    #[test]
    fn box_dyn_transport_satisfies_transport_bound() {
        fn assert_transport<T: Transport>() {}
        assert_transport::<Box<dyn Transport>>();
    }

    /// Smoke test: same for `Box<dyn RecvTransport>`.
    #[test]
    fn box_dyn_recv_transport_satisfies_recv_transport_bound() {
        fn assert_recv<T: RecvTransport>() {}
        assert_recv::<Box<dyn RecvTransport>>();
    }

    /// Smoke test: the defaulted `socket_stats` body returns `None` so test
    /// mocks (and any transport that doesn't own a libsrt socket) opt out
    /// automatically.
    #[test]
    fn box_dyn_transport_socket_stats_defaults_to_none() {
        struct DummyTransport;
        impl Transport for DummyTransport {
            fn send_bytes(&mut self, _: &[u8]) -> Result<(), TransportError> {
                Ok(())
            }
            fn max_payload(&self) -> usize {
                1316
            }
            fn is_alive(&self) -> bool {
                true
            }
            fn close(&mut self) {}
        }
        let t: Box<dyn Transport> = Box::new(DummyTransport);
        assert!(t.socket_stats().is_none());
    }
}

#[cfg(all(test, feature = "std"))]
mod end_reason_tests {
    use super::{EndReason, EndReasonHandle, RecvTransport, TransportError};

    #[test]
    fn first_writer_wins_and_clones_share_the_cell() {
        let h = EndReasonHandle::default();
        let other = h.clone();
        assert_eq!(other.get(), None);
        h.record(EndReason::Cancelled);
        h.record(EndReason::TransportFailed { msg: "late".into() });
        assert_eq!(other.get(), Some(EndReason::Cancelled));
    }

    #[test]
    fn a_handle_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EndReasonHandle>();
    }

    struct Silent;
    impl RecvTransport for Silent {
        fn recv_bytes(&mut self, _: &mut [u8]) -> Result<usize, TransportError> {
            Ok(0)
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn is_alive(&self) -> bool {
            true
        }
    }

    #[test]
    fn a_transport_that_records_nothing_returns_no_handle_even_boxed() {
        assert!(Silent.end_reason_handle().is_none());
        let boxed: std::boxed::Box<dyn RecvTransport> = std::boxed::Box::new(Silent);
        assert!(boxed.end_reason_handle().is_none());
    }
}

#[cfg(test)]
mod kind_tests {
    use super::*;

    #[test]
    fn transport_error_kind_is_exhaustive() {
        assert_eq!(TransportError::Closed.kind(), TransportErrorKind::Closed);
        assert_eq!(
            TransportError::ExplicitClose.kind(),
            TransportErrorKind::ExplicitClose
        );
        assert_eq!(
            TransportError::TooLarge { len: 2, max: 1 }.kind(),
            TransportErrorKind::TooLarge
        );
    }
}
