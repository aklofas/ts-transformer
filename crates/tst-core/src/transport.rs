//! Transport trait definitions — the seam between shells and the wire.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! This module contains both send-side and receive-side transport traits.
//! Concrete implementations (SRT, file-replay, in-memory channels) live
//! in their own crates; only the abstract contract lives here.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use thiserror::Error;

// ============================================================
// Wire-level socket stats (shared between Transport + RecvTransport)
// ============================================================

/// Wire-level transport stats sourced from the underlying network library.
///
/// All bandwidth fields are in bits per second; RTT is in microseconds;
/// buffer-depth fields are in packets. Transport implementations that
/// don't expose a particular value report `0` (or `None` from
/// [`Transport::socket_stats`] / [`RecvTransport::socket_stats`] for the
/// whole struct if no telemetry is available).
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
    /// report `0`; saturates at `u32::MAX` (~71 minutes) where the
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
    /// **Producers:** `ManagedRecvTransport::recv_bytes` when its own
    /// cancel signal has fired, and cancel-aware bare transports — the RTP
    /// transports (`RtpTransport` / `RtpRecvTransport` in `tst-rtp`)
    /// return it once their cancel handle fires. `SrtTransport` does not
    /// produce this variant — it maps both caller-close and peer-EOS to
    /// [`Self::Closed`] because the libsrt-level distinction isn't
    /// reliably observable. The pipeline-shell layer treats the two the
    /// same on the send side (`Closed` is always caller-initiated for
    /// senders) and only distinguishes on the receive side via
    /// `ManagedRecvTransport`'s extra tracking.
    ///
    /// **Shell-layer mapping (`kind_from_transport`):**
    /// - `ExplicitClose` → `ShellErrorKind::Closed` (caller-initiated)
    /// - `Closed` on a receiver shell → `ShellErrorKind::EndOfStream` (peer-initiated)
    /// - `Closed` on a sender shell → `ShellErrorKind::Closed` (caller-initiated; sender shells expose `close()` and produce this on post-close calls)
    #[error("transport explicitly closed by caller")]
    ExplicitClose,
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

#[cfg(feature = "std")]
impl TransportError {
    /// True when this error carries an OS errno identifying a refused
    /// connection (ICMP port-unreachable surfaced as `ECONNREFUSED`).
    /// Uses the std `io::ErrorKind` mapping so callers never hard-code the
    /// platform errno split (111 Linux / 61 BSD / 10061 Windows).
    #[must_use]
    pub fn is_connection_refused(&self) -> bool {
        let errno = match self {
            Self::Backpressure { errno_code, .. } | Self::Broken { errno_code, .. } => *errno_code,
            _ => None,
        };
        errno.is_some_and(|c| {
            std::io::Error::from_raw_os_error(c).kind() == std::io::ErrorKind::ConnectionRefused
        })
    }
}

#[cfg(all(test, feature = "std"))]
mod error_tests {
    use super::*;

    #[test]
    fn is_connection_refused_classifies_portably() {
        #[cfg(target_os = "linux")]
        let refused: i32 = 111; // ECONNREFUSED
        #[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
        let refused: i32 = 61;
        #[cfg(windows)]
        let refused: i32 = 10061; // WSAECONNREFUSED
        let e = TransportError::Broken {
            msg: "send error".into(),
            errno_code: Some(refused),
            cause: BrokenCause::Unspecified,
        };
        assert!(e.is_connection_refused());
        let none = TransportError::Broken {
            msg: "x".into(),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        };
        assert!(!none.is_connection_refused());
        assert!(!TransportError::Closed.is_connection_refused());
    }
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
    /// `TransportError::Broken`.
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
}

/// Type-erased cancel-handle accessor returned by
/// [`Transport::cancel_handle`] and [`RecvTransport::cancel_handle`].
///
/// `cancel()` from any thread interrupts the transport's current blocking
/// send or receive — the parked call returns `Broken` (or whatever the
/// inner mapping produces). Idempotent.
///
/// `Send + Sync` is required so consumers can stash one in an
/// `Arc<dyn TransportCancel>` and share it across worker threads.
pub trait TransportCancel: Send + Sync {
    fn cancel(&self);
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
    /// Returns `Err(TransportError::Closed)` once the transport is closed or
    /// the connection has been broken and no further receive is possible.
    /// Returns `Err(TransportError::Backpressure)` on a recv timeout — the
    /// transport is still alive and the caller may retry.
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

    /// Optional cancellation accessor. Wakes a thread parked in `recv_bytes`.
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
}

// ============================================================
// Blanket impls for Box<T: ?Sized>
// ============================================================
//
// These let `Box<dyn Transport>` and `Box<dyn RecvTransport>` satisfy the
// `T: Transport` / `T: RecvTransport` trait bounds on the pipeline shells.
// Required for the dyn-erased aliases (`BoxedMuxSender`, `BoxedDemuxReceiver`,
// etc.) used by the FFI binding crates (`tst-jni`, `tst-uniffi`, `tst-pyo3`).
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
}

#[cfg(test)]
mod blanket_impl_tests {
    use super::*;

    /// Smoke test: `Box<dyn Transport>` must satisfy `T: Transport` for the
    /// dyn-erased pipeline shell aliases (`BoxedMuxSender`, etc.) to compile.
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
