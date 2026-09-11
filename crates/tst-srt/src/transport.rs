//! `SrtTransport` — the canonical `Transport` impl backed by a `srt::Socket`.
//!
//! Wraps the safe-Rust `Socket` and translates `SendError` to the
//! `Transport`-trait shape. Used as the inner of `ManagedTransport` in
//! the canonical reconnecting setup.

use crate::Socket;
use crate::error::{SendError, SrtErrno};
use std::sync::Arc;
use tst_core::transport::{BrokenCause, SocketStats, Transport, TransportCancel, TransportError};

/// SRT live-mode wire ceiling for a single message payload — the
/// maximum value libsrt accepts for `SRTO_PAYLOADSIZE`. The option is
/// LOCAL (not negotiated between peers), so a foreign peer configured
/// at the maximum can deliver messages of this size regardless of our
/// local setting; the recv-side deliverable ceiling can never be below
/// it.
const SRT_LIVE_MAX_PAYLOAD: usize = 1456;

/// SRT-backed [`Transport`] and [`tst_core::transport::RecvTransport`] impl.
///
/// Wraps a connected [`Socket`] and translates libsrt send/receive errors
/// into the scheme-neutral [`TransportError`] shape. Intended to be used as
/// the inner of `ManagedTransport` in the canonical reconnecting setup.
///
/// # `SocketStats` field mapping
///
/// When `socket_stats()` returns `Some(stats)`, fields are sourced from
/// libsrt's `CBytePerfMon` (`bstats()`) as follows:
///
/// | [`SocketStats`] field | libsrt source | Notes |
/// |---|---|---|
/// | `rtt_us` | `msRTT` × 1000 | rounded, saturated at `u32::MAX` |
/// | `send_bandwidth_bps` | `mbpsSendRate` × 1e6 | rounded, saturated |
/// | `recv_bandwidth_bps` | `mbpsRecvRate` × 1e6 | rounded, saturated |
/// | `link_bandwidth_bps` | `mbpsBandwidth` × 1e6 | rounded, saturated |
/// | `bytes_sent` | `byteSentTotal` | |
/// | `packets_sent` | `pktSentTotal` | |
/// | `bytes_received` | `byteRecvTotal` | |
/// | `packets_received` | `pktRecvTotal` | |
/// | `bytes_lost_recv` | `byteRcvLossTotal` | |
/// | `packets_lost_recv` | `pktRcvLossTotal` | |
/// | `packets_lost_send` | `pktSndLossTotal` | from NAK reports |
/// | `packets_retransmitted` | `pktRetransTotal` | sum across all retx rounds |
/// | `packets_dropped_send` | `pktSndDropTotal` | overrun / drop-late |
/// | `packets_dropped_recv` | `pktRcvDropTotal` | |
/// | `send_buffer_packets` | `pktSndBuf` | spot reading |
/// | `recv_buffer_packets` | `pktRcvBuf` | spot reading |
///
/// # `TransportError::Backpressure` / `Broken` `errno_code` mapping
///
/// For [`SrtTransport`], `errno_code` carries the libsrt `MJ_*` major
/// category from `srt_getlasterror()`:
///
/// | Value | libsrt constant |
/// |---|---|
/// | 1 | `MJ_SETUP` |
/// | 2 | `MJ_CONNECTION` |
/// | 3 | `MJ_SYSTEMRES` |
/// | 4 | `MJ_FILESYSTEM` |
/// | 5 | `MJ_NOTSUP` |
/// | 6 | `MJ_AGAIN` (async — typically `Backpressure`) |
/// | 7 | `MJ_PEERERROR` |
/// | other | raw libsrt errno value |
pub struct SrtTransport {
    socket: Option<Socket>,
    max_payload: usize,
}

impl SrtTransport {
    /// Default SRT live-mode payload size (libsrt's `SRTO_PAYLOADSIZE` default).
    ///
    /// Derived from [`tst_core::mpegts::common::SRT_TS_BUNDLE_BYTES`] — the
    /// standard 7-packet × 188-byte TS bundle libsrt uses for live mode.
    /// Used as a fallback only — [`Self::new`] queries the socket directly.
    pub const DEFAULT_PAYLOAD: usize = tst_core::mpegts::common::SRT_TS_BUNDLE_BYTES;

    /// Wrap an already-connected `Socket`. Caller is responsible for
    /// configuring it (passphrase, latency, etc.) before passing in.
    ///
    /// `max_payload` is read from the socket's negotiated `SRTO_PAYLOADSIZE`
    /// (via [`Socket::payload_limit`]). On a fresh post-handshake socket
    /// this matches whatever both peers agreed during the SRT handshake —
    /// libsrt's default of 1316 bytes for unconfigured sockets, or a
    /// configured value like 1456 when the `payloadsize=` URL key (or
    /// [`SocketConfig::payload_size`]) was set on either side.
    ///
    /// Callers that need a different `max_payload` value (e.g., they're
    /// wrapping a non-libsrt transport, or testing without a live socket)
    /// can override via [`with_max_payload`] after construction.
    ///
    /// [`Socket::payload_limit`]: crate::Socket::payload_limit
    /// [`SocketConfig::payload_size`]: crate::config::SocketConfig::payload_size
    /// [`with_max_payload`]: SrtTransport::with_max_payload
    pub fn new(socket: Socket) -> Self {
        let max_payload = socket.payload_limit();
        Self {
            socket: Some(socket),
            max_payload,
        }
    }

    /// Override the max payload after construction.
    ///
    /// Normally unnecessary — [`Self::new`] reads
    /// [`Socket::payload_limit`] which already reflects the negotiated
    /// `SRTO_PAYLOADSIZE`. Provided as an escape hatch for callers who
    /// (a) wrap a non-libsrt `Socket`-shaped transport via an alternative
    /// constructor in the future, or (b) need to artificially constrain
    /// the per-send size below what libsrt agreed to.
    ///
    /// **Setting this larger than the negotiated `SRTO_PAYLOADSIZE` will
    /// cause libsrt to reject sends with `PayloadTooLarge` at runtime.**
    /// Use it to lower the bound, not raise it.
    ///
    /// [`Socket::payload_limit`]: crate::Socket::payload_limit
    pub fn with_max_payload(mut self, n: usize) -> Self {
        self.max_payload = n;
        self
    }

    /// Snapshot the libsrt-flavored 17-field [`Stats`] for the underlying
    /// socket. Returns `Err(IoError::SocketClosed)` once the transport
    /// has been closed (either explicitly via [`Transport::close`] or
    /// implicitly after a `Broken` send/recv tore the socket down).
    ///
    /// Use [`Transport::socket_stats`] / [`tst_core::transport::RecvTransport::socket_stats`]
    /// for the scheme-neutral 16-field projection; this accessor exposes
    /// the SRT-specific extras (`mbps_estimated_bandwidth`, the
    /// `rtt: Duration`, the symmetric send/recv-side byte-loss split).
    ///
    /// [`Stats`]: crate::Stats
    pub fn stats(&self) -> Result<crate::Stats, crate::error::IoError> {
        let socket = self
            .socket
            .as_ref()
            .ok_or(crate::error::IoError::SocketClosed)?;
        socket.stats()
    }
}

impl Transport for SrtTransport {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        if msg.len() > self.max_payload {
            return Err(TransportError::TooLarge {
                len: msg.len(),
                max: self.max_payload,
            });
        }
        let socket = self.socket.as_mut().ok_or(TransportError::Closed)?;
        match socket.send(msg) {
            Ok(_) => Ok(()),
            Err(SendError::TimedOut) => Err(TransportError::Backpressure {
                msg: "send timed out".into(),
                // libsrt SRT_ETIMEOUT — typed as SrtErrno::Async major
                // category (6) by From<RawError>; recorded here so JNI/
                // UniFFI bindings can discriminate on the wire-level
                // code without parsing the message.
                errno_code: Some(SrtErrno::Async.raw_code()),
            }),
            Err(SendError::QueueFull) => Err(TransportError::Backpressure {
                msg: "send queue full".into(),
                errno_code: Some(SrtErrno::Async.raw_code()),
            }),
            Err(SendError::PayloadTooLarge { actual, .. }) => Err(TransportError::TooLarge {
                len: actual,
                max: self.max_payload,
            }),
            Err(SendError::ConnectionBroken) => {
                self.socket = None;
                Err(TransportError::Broken {
                    msg: "connection broken".into(),
                    errno_code: Some(SrtErrno::Connection.raw_code()),
                    cause: BrokenCause::Unspecified,
                })
            }
            Err(SendError::System(e)) => {
                self.socket = None;
                Err(TransportError::Broken {
                    msg: format!("system error: {e}"),
                    // OS-level IO error — not a libsrt MJ_* code. None
                    // is the honest signal; bindings should treat
                    // None+Broken as "wire-level cause not exposed."
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                })
            }
            Err(SendError::Other { kind, message }) => {
                // SrtErrno::Async coarsens libsrt's async-class category. The only
                // sub-code that can reach this arm on a send is SRT_EASYNCSND
                // (send-buffer-full in non-blocking mode) — SRT_ETIMEOUT is
                // pre-consumed into SendError::TimedOut by From<RawError>, and
                // SRT_EASYNCRCV / SRT_EASYNCFAIL don't fire on srt_sendmsg2.
                // Everything else → broken (rebuild the transport).
                let errno_code = Some(kind.raw_code());
                if matches!(kind, SrtErrno::Async) {
                    Err(TransportError::Backpressure {
                        msg: message,
                        errno_code,
                    })
                } else {
                    self.socket = None;
                    Err(TransportError::Broken {
                        msg: message,
                        errno_code,
                        cause: BrokenCause::Unspecified,
                    })
                }
            }
        }
    }

    fn max_payload(&self) -> usize {
        self.max_payload
    }

    fn is_alive(&self) -> bool {
        self.socket.is_some()
    }

    fn close(&mut self) {
        if let Some(socket) = self.socket.take() {
            // Socket::close consumes self; ignore the error — we're closing.
            let _ = socket.close();
        }
    }

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        self.socket
            .as_ref()
            .map(|s| Arc::new(s.cancel_handle()) as Arc<dyn TransportCancel + Send + Sync>)
    }

    fn socket_stats(&self) -> Option<SocketStats> {
        let socket = self.socket.as_ref()?;
        let s = socket.stats().ok()?;
        Some(map_stats(&s))
    }
}

impl tst_core::transport::RecvTransport for SrtTransport {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        use crate::error::RecvError;
        let socket = self.socket.as_mut().ok_or(TransportError::Closed)?;
        match socket.recv(buf) {
            Ok(n) => Ok(n),
            Err(RecvError::TimedOut) => Err(TransportError::Backpressure {
                msg: "recv timed out".into(),
                errno_code: Some(SrtErrno::Async.raw_code()),
            }),
            Err(RecvError::ConnectionBroken) => {
                // Peer hung up or mid-stream abort. Surface as Broken (not
                // Closed) so a managed receive decorator can distinguish a
                // self-initiated close from a peer-initiated break and drive
                // reconnect. Matches the send-side mapping for the same
                // RecvError-equivalent variant.
                self.socket = None;
                Err(TransportError::Broken {
                    msg: "connection broken".into(),
                    errno_code: Some(SrtErrno::Connection.raw_code()),
                    cause: BrokenCause::Unspecified,
                })
            }
            Err(RecvError::BufferTooSmall {
                buf_len,
                message_len,
            }) => {
                // The caller passed a buf smaller than the incoming message.
                // Surface as Broken — the receive shell is misconfigured (it
                // should have sized buf to at least max_payload()).
                self.socket = None;
                Err(TransportError::Broken {
                    msg: format!("recv buf too small: {buf_len} < {message_len}"),
                    // Caller-misconfiguration shape, not a libsrt errno;
                    // pass None to keep the signal honest.
                    errno_code: None,
                    cause: BrokenCause::Unspecified,
                })
            }
            Err(other) => {
                // Catch-all for the remaining RecvError variants
                // (`System(io::Error)`, `Other { kind, message }`, and
                // any future #[non_exhaustive] additions). Carries the
                // raw libsrt errno when the underlying typed variant
                // exposes one; otherwise None.
                let errno_code = match &other {
                    RecvError::Other { kind, .. } => Some(kind.raw_code()),
                    _ => None,
                };
                self.socket = None;
                Err(TransportError::Broken {
                    msg: other.to_string(),
                    errno_code,
                    cause: BrokenCause::Unspecified,
                })
            }
        }
    }

    fn max_payload(&self) -> usize {
        // Recv-side deliverable ceiling (see RecvTransport::max_payload
        // in tst-core). SRTO_PAYLOADSIZE is a local option — a foreign
        // peer at the live-mode max delivers 1456-byte messages
        // regardless of our local value, and on vendored libsrt 1.5.7
        // srt_recvmsg SILENTLY TRUNCATES to the caller's buffer (the
        // BufferTooSmall→Broken mapping appears unreachable; kept as
        // defence). The max() keeps an explicitly-configured larger
        // value honored. The send-side Transport::max_payload keeps
        // returning the local budget.
        self.max_payload.max(SRT_LIVE_MAX_PAYLOAD)
    }

    fn is_alive(&self) -> bool {
        self.socket.is_some()
    }

    fn close(&mut self) {
        <Self as tst_core::transport::Transport>::close(self);
    }

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        <Self as tst_core::transport::Transport>::cancel_handle(self)
    }

    fn socket_stats(&self) -> Option<SocketStats> {
        <Self as tst_core::transport::Transport>::socket_stats(self)
    }
}

impl Drop for SrtTransport {
    fn drop(&mut self) {
        self.close();
    }
}

/// Map the libsrt-flavored `crate::socket::Stats` into the abstract
/// `tst_core::transport::SocketStats`. Unit conversions:
/// * RTT: `Duration` → microseconds, saturating at `u32::MAX`.
/// * Bandwidth: libsrt-side u64 bps is passed through; the redundant
///   `mbps_estimated_bandwidth` f64 field is multiplied by 1e6 and
///   saturated into `link_bandwidth_bps`.
/// * Send-side byte loss: libsrt doesn't export `byteSndLossTotal`, so
///   the abstract struct has no `bytes_lost_send` field. The local
///   `Stats::bytes_lost_send_side` is always 0 today and is dropped.
fn map_stats(s: &crate::socket::Stats) -> SocketStats {
    let rtt_us = u32::try_from(s.rtt.as_micros()).unwrap_or(u32::MAX);
    let link_bandwidth_bps =
        if s.mbps_estimated_bandwidth.is_finite() && s.mbps_estimated_bandwidth >= 0.0 {
            let scaled = s.mbps_estimated_bandwidth * 1e6;
            if scaled >= u64::MAX as f64 {
                u64::MAX
            } else {
                scaled.round() as u64
            }
        } else {
            0
        };
    // `#[non_exhaustive]` blocks the `SocketStats { ... }` struct-literal
    // form from outside tst-core (Rust E0639), and `..Default::default()`
    // doesn't lift the restriction outside the defining crate. Default-
    // and-assign is the only pattern that preserves the #[non_exhaustive]
    // forward-compatibility guarantee.
    let mut out = SocketStats::default();
    out.rtt_us = rtt_us;
    out.send_bandwidth_bps = s.send_bandwidth_bps;
    out.recv_bandwidth_bps = s.recv_bandwidth_bps;
    out.link_bandwidth_bps = link_bandwidth_bps;
    out.bytes_sent = s.bytes_sent;
    out.packets_sent = s.packets_sent;
    out.bytes_received = s.bytes_received;
    out.packets_received = s.packets_received;
    out.bytes_lost_recv = s.bytes_lost_recv_side;
    out.packets_lost_recv = s.packets_lost_recv_side;
    out.packets_lost_send = s.packets_lost_send_side;
    out.packets_retransmitted = s.packets_retransmitted;
    out.packets_dropped_send = s.packets_dropped_send_side;
    out.packets_dropped_recv = s.packets_dropped_recv_side;
    out.send_buffer_packets = s.send_buffer_packets;
    out.recv_buffer_packets = s.recv_buffer_packets;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_transport_max_payload_matches_default() {
        // Without an actual connected Socket, just verify the constant.
        // SrtTransport::DEFAULT_PAYLOAD == 1316 (libsrt default).
        assert_eq!(
            SrtTransport::DEFAULT_PAYLOAD,
            tst_core::mpegts::common::SRT_TS_BUNDLE_BYTES
        );
        // Cross-check the literal value to catch unrelated regressions.
        assert_eq!(SrtTransport::DEFAULT_PAYLOAD, 1316);
    }

    /// `cancel_handle()` returns Some when a Socket is held; calling
    /// cancel() flips the inner socket to None on the next send_bytes
    /// (which now returns Closed because we proactively dropped it).
    #[test]
    #[ignore = "needs live SRT socket; covered by tests/cancellation_loopback.rs"]
    fn cancel_handle_some_when_alive() {}

    /// validate-1 D5: ensure `SrtErrno::raw_code()` produces the libsrt
    /// MJ_* major-category integers the `errno_code` field on
    /// `TransportError::{Backpressure, Broken}` carries. Bindings that
    /// pattern-match on these codes need them stable across releases —
    /// the return value is always in the `0..=7` range (or the major
    /// component of an unknown errno).
    #[test]
    fn srt_errno_raw_code_maps_to_libsrt_major() {
        assert_eq!(SrtErrno::Setup.raw_code(), 1);
        assert_eq!(SrtErrno::Connection.raw_code(), 2);
        assert_eq!(SrtErrno::SystemRes.raw_code(), 3);
        assert_eq!(SrtErrno::FileSystem.raw_code(), 4);
        assert_eq!(SrtErrno::Notsup.raw_code(), 5);
        assert_eq!(SrtErrno::Async.raw_code(), 6);
        assert_eq!(SrtErrno::PeerError.raw_code(), 7);
        // D5 follow-up: Unknown(raw) folds back to the major category
        // via `raw / 1000`. Bindings that match on `code <= 7` should
        // see 6 here, not the full encoded 6002. Callers that need the
        // raw sub-code can match `SrtErrno::Unknown(raw)` directly.
        assert_eq!(SrtErrno::Unknown(6002).raw_code(), 6);
        // Sub-code 0 (i.e., raw == major exactly) still produces major.
        assert_eq!(SrtErrno::Unknown(5000).raw_code(), 5);
        // Defensive: out-of-range major still folds (caller decides
        // what to do with an unrecognized major).
        assert_eq!(SrtErrno::Unknown(99999).raw_code(), 99);
    }

    /// validate-1 D5: the transport error variants carry an optional
    /// errno code. Verify the field is constructible (non-SRT producers
    /// pass None) and round-trips through pattern destructuring.
    #[test]
    fn transport_error_errno_code_destructure() {
        let err = TransportError::Backpressure {
            msg: "test".into(),
            errno_code: Some(SrtErrno::Async.raw_code()),
        };
        if let TransportError::Backpressure {
            msg: _,
            errno_code: Some(c),
        } = err
        {
            assert_eq!(c, 6, "Backpressure should carry SRT Async major (6)");
        } else {
            panic!("expected Backpressure with errno");
        }

        let err = TransportError::Broken {
            msg: "test".into(),
            errno_code: Some(SrtErrno::Connection.raw_code()),
            cause: BrokenCause::Unspecified,
        };
        if let TransportError::Broken {
            msg: _,
            errno_code: Some(c),
            cause: BrokenCause::Unspecified,
        } = err
        {
            assert_eq!(c, 2, "Broken should carry SRT Connection major (2)");
        } else {
            panic!("expected Broken with errno");
        }
    }
}
