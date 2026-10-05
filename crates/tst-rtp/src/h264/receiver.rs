//! [`H264Receiver`] — blocking I/O shell for the RFC 6184 H.264 depacketizer.
//!
//! Wraps a UDP socket or mpsc channel (TCP-interleaved bridge), polls
//! `Source::recv_raw` with cancel-poll cadence, decodes the RTP header,
//! filters by payload type, and feeds each packet into
//! [`H264Depacketizer`], surfacing reassembled [`H264Au`]s via
//! [`H264Receiver::recv_au`].
//!
//! # URL form
//!
//! ```text
//! rtp://127.0.0.1:0?pt=96
//! ```
//!
//! `?pt=` is **required** (range 1..=127, 33 rejected — use the MP2T
//! receiver for MPEG-TS). The MP2T constructors (`RtpTransport::connect*`
//! / `RtpRecvTransport::listen*`) conversely reject URLs carrying `?pt=`.
//!
//! # Blocking / threading model
//!
//! [`H264Receiver::recv_au`] blocks the calling thread. Callers that need
//! concurrent mux/demux should run the receiver on a dedicated thread and
//! channel AUs out. A single thread must own the receiver — concurrent
//! calls to `recv_au` are not supported.
//!
//! # RTCP
//!
//! RTCP is **not** implemented on this path. No RTCP socket is bound; no
//! RR/SR packets are sent or received. This is a recorded deferral: see
//! `docs/project/deferred-features.md`, "RTP jitter/reorder buffer +
//! H.264-path RTCP".
//!
//! # EOS contract
//!
//! `recv_au` always drains already-assembled AUs before signalling EOS:
//!
//! 1. On cancel (`close()`) or clean disconnect (mpsc pump dropped its
//!    `Sender`, indicating RTSP teardown): the depacketizer is flushed,
//!    which may yield one final partial AU. Any queued complete AUs are
//!    returned first via `Ok(Some(au))`.
//! 2. `Ok(None)` is the terminal value — it is returned only after all
//!    queued and flushed AUs have been surfaced.
//!
//! Any other hard I/O error surfaces as `Err(TransportError::Broken {..})`.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tst_core::net::udp_socket::CANCEL_POLL_INTERVAL;
use tst_core::transport::{SocketStats, TransportError};

use crate::cancel::RtpCancelHandle;
use crate::h264::depacketizer::{H264Au, H264Depacketizer, H264DepayConfig, H264DepayStats};
use crate::packet::RtpHeader;
use crate::rtsp::client::end_reason::{EndReasonSlot, StreamEndReason};
use crate::transport::{ConnectError, MPSC_PUMP_DISCONNECTED, RECV_SCRATCH_LEN, RtpStats, Source};
use crate::url::RtpUrl;

/// Blocking H.264-over-RTP receive shell.
///
/// # Constructing
///
/// - [`Self::listen`] — parse `rtp://host:port?pt=N`, bind, and return a
///   ready receiver. `?pt=` is required; value 33 is rejected.
/// - [`Self::listen_with`] — same, but from an already-parsed [`RtpUrl`].
/// - `from_udp_socket_with` (`pub(crate)`) — wrap an already-bound socket;
///   used by the RTSP session bridge.
/// - `from_mpsc_with_rtcp_drain` (`pub(crate)`) — wrap a TCP-interleaved mpsc
///   channel; used by the RTSP session bridge. Accepts the pump's
///   RTCP channel so the pump never sees `Disconnected` on it (which would
///   kill the session at the first server RTCP Sender Report). Pass `None` for
///   `rtcp_rx` on paths where there is no RTCP channel.
///
/// # recv_au loop contract
///
/// 1. Drain `depay.next_au()` first — the depacketizer may have queued
///    AUs from a prior `feed` call.
/// 2. If the EOS flag is set, return `Ok(None)`.
/// 3. Call `source.recv_raw(scratch, cancel, deadline)`:
///    - `Ok(n)` — tick counters, decode RTP header (malformed → tick +
///      continue), check payload type (wrong PT → tick + continue), feed
///      depacketizer, go to step 1.
///    - `Err(ExplicitClose)` — set EOS, flush depacketizer, return
///      flushed AU if any else `Ok(None)`.
///    - `Err(Broken)` carrying the `MPSC_PUMP_DISCONNECTED` sentinel
///      (`pub(crate)` const in `transport.rs`) — same EOS path (clean
///      RTSP teardown).
///    - `Err(Broken)` otherwise — clear source, propagate.
///
/// # Stats
///
/// Three complementary views, mirroring `RtpRecvTransport`'s split:
///
/// - [`Self::socket_stats`] — **throughput**: wire-level
///   `bytes_received` / `packets_received`, counted before RTP-header or
///   PT validation.
/// - [`Self::rtp_stats`] — **protocol anomalies**: the malformed-packet
///   counter (bad RTP header, or PT mismatch against the configured
///   `?pt=`).
/// - [`Self::depay_stats`] — **RFC 6184 depacketizer internals**: AU
///   counts, sequence gaps, parameter-set updates.
///
/// # RTCP
///
/// RTCP is not implemented on this path. No RTCP companion socket is
/// bound. See `docs/project/deferred-features.md`.
///
/// # Send
///
/// This type is `Send`: moving it to a dedicated receive/watchdog thread
/// is a supported, documented use — a regression here is a breaking
/// change.
pub struct H264Receiver {
    source: Option<Source>,
    scratch: Vec<u8>,
    cancel: Arc<RtpCancelHandle>,
    depay: H264Depacketizer,
    pt: u8,
    bytes_received: u64,
    packets_received: u64,
    malformed_packets: u64,
    local_addr: Option<SocketAddr>,
    eos: bool,
    /// Persistent deadline for [`Self::recv_au`], set via
    /// [`Self::set_recv_timeout`]. `None` (the default) blocks
    /// indefinitely. [`Self::recv_au_timeout`] ignores this field — its
    /// explicit argument always wins for that call.
    recv_timeout: Option<Duration>,
    /// For TCP-interleaved sessions: the pump's RTCP channel (`rtcp_rx` from
    /// `RtspSession`). RTCP is not processed on the H.264 path (a recorded
    /// deferral; see `docs/project/deferred-features.md`), but if this receiver is
    /// dropped the pump's `rtcp_tx.try_send()` returns `Disconnected` and the
    /// pump exits — which drops `data_tx` — causing `recv_au`'s next
    /// `recv_raw` to return `MPSC_PUMP_DISCONNECTED` (a clean-EOS sentinel)
    /// prematurely at the very first server RTCP SR. We keep the receiver here
    /// and drain it on each `recv_au` iteration (non-blocking, discard-only),
    /// ensuring the pump always finds its RTCP sender intact.
    ///
    /// `None` on UDP and plain-mpsc paths.
    rtcp_drain: Option<std::sync::mpsc::Receiver<bytes::Bytes>>,
    /// Structured record of why the session ended — see
    /// `RtpRecvTransport::end_reason`'s doc for the same split between
    /// an RTSP-shared slot (set via [`Self::set_end_reason_slot`]) and a
    /// fresh slot for a plain `rtp://` receiver, written only by
    /// [`Self::close`] / an explicit [`Self::cancel_handle`] fire.
    end_reason: EndReasonSlot,
}

impl H264Receiver {
    /// Bind to `url`'s host:port and return a ready receiver.
    ///
    /// `url` must carry `?pt=N` (1..=127, 33 rejected). The default
    /// [`H264DepayConfig`] is constructed from the URL's `pt` value.
    ///
    /// # Errors
    ///
    /// Returns [`ConnectError::MissingPayloadTypeParam`] when `?pt=` is
    /// absent, [`ConnectError::Url`] on parse failure (including a `?pt=`
    /// outside 1..=127 or equal to 33, and a receive-side `?pkt_size=`),
    /// [`ConnectError::HostNotLiteral`] when the host is not a literal IP,
    /// or [`ConnectError::Io`] on bind failure.
    pub fn listen(url: &str) -> Result<Self, ConnectError> {
        let parsed = RtpUrl::parse(url).map_err(ConnectError::Url)?;
        Self::listen_with(&parsed, H264DepayConfig::default())
    }

    /// Bind using an already-parsed URL with an explicit depacketizer
    /// config. `url.pt` must be `Some(_)`.
    ///
    /// # Errors
    ///
    /// Returns [`ConnectError::MissingPayloadTypeParam`] when `url.pt` is
    /// `None`, [`ConnectError::Url`] when `url.pkt_size` is set (a
    /// send-side knob), [`ConnectError::HostNotLiteral`] when the host is
    /// not a literal IP, or [`ConnectError::Io`] on bind failure.
    pub fn listen_with(url: &RtpUrl, mut config: H264DepayConfig) -> Result<Self, ConnectError> {
        if url.pkt_size.is_some() {
            return Err(ConnectError::Url(crate::url::UrlError::RecvPktSize));
        }
        let pt = url.pt.ok_or(ConnectError::MissingPayloadTypeParam)?;
        config.payload_type = pt;
        let ip: std::net::IpAddr = url.host.parse().map_err(|e: std::net::AddrParseError| {
            ConnectError::HostNotLiteral {
                host: url.host.clone(),
                detail: e.to_string(),
            }
        })?;
        let local = SocketAddr::new(ip, url.port);
        let sock = UdpSocket::bind(local).map_err(ConnectError::Io)?;
        let mut receiver = Self::from_udp_socket_with(sock, config)?;
        receiver.set_recv_timeout(url.recv_timeout);
        Ok(receiver)
    }

    /// Wrap an already-bound socket. Sets the cancel-poll read timeout to
    /// match the rest of the recv-side machinery.
    ///
    /// Used by the RTSP session bridge.
    pub(crate) fn from_udp_socket_with(
        sock: UdpSocket,
        config: H264DepayConfig,
    ) -> Result<Self, ConnectError> {
        sock.set_read_timeout(Some(CANCEL_POLL_INTERVAL))
            .map_err(ConnectError::Io)?;
        let local_addr = sock.local_addr().ok();
        let pt = config.payload_type;
        Ok(Self {
            source: Some(Source::Udp(sock)),
            scratch: vec![0u8; RECV_SCRATCH_LEN],
            cancel: RtpCancelHandle::new(),
            depay: H264Depacketizer::new(config),
            pt,
            bytes_received: 0,
            packets_received: 0,
            malformed_packets: 0,
            local_addr,
            eos: false,
            recv_timeout: None,
            rtcp_drain: None,
            end_reason: EndReasonSlot::default(),
        })
    }

    /// Wrap an mpsc channel fed by the RTSP client's TCP-interleaved pump,
    /// optionally also holding the pump's RTCP channel so it is never
    /// `Disconnected`.
    ///
    /// `rtcp_rx` should be `Some(_)` for TCP-interleaved sessions and `None`
    /// for the plain-mpsc (non-RTSP) path. When `Some`, each [`recv_au`]
    /// iteration drains the RTCP channel with `try_recv` (non-blocking,
    /// discard-only). This keeps the pump's `rtcp_tx.try_send()` from ever
    /// seeing `TrySendError::Disconnected`, which would cause the pump to exit
    /// — dropping `data_tx` — and produce a premature clean-EOS at the first
    /// server RTCP Sender Report.
    ///
    /// RTCP frames are discarded here; no RTCP processing is done on the
    /// H.264 path (a recorded deferral; see `docs/project/deferred-features.md`).
    ///
    /// Used by the RTSP session bridge.
    pub(crate) fn from_mpsc_with_rtcp_drain(
        rx: std::sync::mpsc::Receiver<bytes::Bytes>,
        rtcp_rx: Option<std::sync::mpsc::Receiver<bytes::Bytes>>,
        config: H264DepayConfig,
    ) -> Self {
        let pt = config.payload_type;
        Self {
            source: Some(Source::Mpsc(rx)),
            scratch: vec![0u8; RECV_SCRATCH_LEN],
            cancel: RtpCancelHandle::new(),
            depay: H264Depacketizer::new(config),
            pt,
            bytes_received: 0,
            packets_received: 0,
            malformed_packets: 0,
            local_addr: None,
            eos: false,
            recv_timeout: None,
            rtcp_drain: rtcp_rx,
            end_reason: EndReasonSlot::default(),
        }
    }

    /// Receive the next reassembled H.264 Access Unit.
    ///
    /// Returns `Ok(Some(au))` when a complete AU is available,
    /// `Ok(None)` at EOS (cancel/close or clean RTSP teardown), and
    /// `Err(TransportError::Broken {..})` on a hard I/O error.
    ///
    /// Blocks the calling thread. See the [struct-level doc](Self) for the
    /// full loop contract. Honors a persistent deadline configured via
    /// [`Self::set_recv_timeout`] — see that method for the full contract.
    pub fn recv_au(&mut self) -> Result<Option<H264Au>, TransportError> {
        // checked_add mirrors `RtpRecvTransport::recv_bytes`'s saturation:
        // a configured timeout too large to represent as an `Instant`
        // (e.g. `Duration::MAX`) means "no deadline" rather than a panic
        // on a public input.
        let deadline = self
            .recv_timeout
            .and_then(|t| Instant::now().checked_add(t));
        self.recv_au_inner(deadline)
    }

    /// Configure a persistent receive deadline for [`Self::recv_au`].
    ///
    /// With `Some(timeout)`, a `recv_au` call that assembles no complete
    /// AU within `timeout` returns [`TransportError::Backpressure`]; the
    /// session stays valid — no eos latch, no depacketizer flush, exactly
    /// as [`Self::recv_au_timeout`]'s expiry behaves — and the next call
    /// starts a fresh deadline. This is the configured-knob mirror of
    /// [`Self::recv_au_timeout`]'s one-shot deadline, aligned with
    /// `RtpRecvTransport::set_recv_timeout`: it makes a deadline-driven
    /// stall watchdog possible with no cancel thread — a stalled-but-
    /// healthy session (peer stops sending; no error, no EOS) hands
    /// control back to the caller every `timeout`.
    ///
    /// `None` (the default) restores the indefinite-block behavior.
    ///
    /// Deadline granularity is the internal cancel-poll interval
    /// (~100 ms). A `timeout` too large to represent as a deadline (e.g.
    /// `Duration::MAX`) saturates to "no deadline". [`Self::recv_au_timeout`]
    /// ignores this setting — its explicit argument always wins for that
    /// call.
    pub fn set_recv_timeout(&mut self, timeout: Option<Duration>) {
        self.recv_timeout = timeout;
    }

    /// The recorded [`StreamEndReason`], or `None` if the session hasn't
    /// ended yet (or ended through a path this arc doesn't instrument —
    /// see the field doc on `end_reason` for the plain-`rtp://` case).
    pub fn end_reason(&self) -> Option<StreamEndReason> {
        self.end_reason.get()
    }

    /// Replace this receiver's end-reason slot with `slot`. Used by
    /// [`crate::rtsp::client::session::RtspSession::into_h264_receiver`]
    /// to swap in the owning `RtspClient`'s shared slot — mirrors
    /// `RtpRecvTransport::set_end_reason_slot`.
    pub(crate) fn set_end_reason_slot(&mut self, slot: EndReasonSlot) {
        self.end_reason = slot;
    }

    /// [`Self::recv_au`] with a deadline. Returns
    /// `Err(TransportError::Backpressure)` if no complete AU arrives within
    /// `timeout` — the session stays valid, call again to keep waiting.
    /// Deadline granularity is the internal cancel-poll interval (~100 ms).
    /// Never panics on extreme inputs: a `timeout` too large to represent
    /// as an [`Instant`] (e.g. [`Duration::MAX`]) behaves as "no deadline";
    /// `Duration::ZERO` expires at the first poll.
    /// This is the intended primitive for stall watchdogs (a healthy RTSP
    /// session whose server stops sending produces no error and no EOS —
    /// without a deadline the caller blocks forever).
    pub fn recv_au_timeout(&mut self, timeout: Duration) -> Result<Option<H264Au>, TransportError> {
        // checked_add: a timeout too large to represent as an `Instant`
        // (e.g. `Duration::MAX`) saturates to "no deadline" — semantically
        // indistinguishable from a bound that outlives the process — rather
        // than panicking on a public input.
        self.recv_au_inner(Instant::now().checked_add(timeout))
    }

    fn recv_au_inner(
        &mut self,
        deadline: Option<Instant>,
    ) -> Result<Option<H264Au>, TransportError> {
        loop {
            // ── RTCP drain (TCP-interleaved sessions only) ─────────────────
            // Keep the pump's RTCP sender alive by non-blocking-draining the
            // channel before each recv_raw. RTCP SR cadence is typically one
            // packet per few seconds; the CANCEL_POLL_INTERVAL (~100 ms)
            // drains the 64-deep queue far faster than any legitimate server
            // fills it. Without this drain the pump sees
            // `TrySendError::Disconnected` on its first RTCP frame (we dropped
            // `rtcp_rx` at construction), exits, drops `data_tx`, and
            // `recv_raw` returns the MPSC_PUMP_DISCONNECTED sentinel — a false
            // clean-EOS at the very first server Sender Report.
            if let Some(rx) = &self.rtcp_drain {
                while rx.try_recv().is_ok() {}
            }

            // ── Step 1: drain the depacketizer's ready queue ──────────────
            if let Some(au) = self.depay.next_au() {
                return Ok(Some(au));
            }

            // ── Step 2: check EOS ─────────────────────────────────────────
            if self.eos {
                return Ok(None);
            }

            // ── Step 3: recv_raw ──────────────────────────────────────────
            let source = match self.source.as_ref() {
                Some(s) => s,
                None => return Err(TransportError::Closed),
            };
            let raw_result = source.recv_raw(&mut self.scratch, &self.cancel, deadline);
            let n = match raw_result {
                Ok(n) => n,
                Err(TransportError::ExplicitClose) => {
                    // Cancel / close — drain depacketizer then EOS. Same
                    // reasoning as `RtpRecvTransport::recv_bytes_inner`'s
                    // ExplicitClose arm: `Self::close` already records
                    // this directly when there's no recv in flight, so
                    // this branch is reached for a bare
                    // `cancel_handle().cancel()` fired mid-recv.
                    self.end_reason.record(StreamEndReason::Cancelled);
                    self.eos = true;
                    return Ok(self.depay.flush());
                }
                Err(TransportError::Broken { ref msg, .. }) if msg == MPSC_PUMP_DISCONNECTED => {
                    // Clean RTSP teardown — same EOS path.
                    self.eos = true;
                    return Ok(self.depay.flush());
                }
                Err(e @ TransportError::Broken { .. }) => {
                    self.source = None;
                    return Err(e);
                }
                Err(e @ TransportError::Backpressure { .. }) => {
                    // Deadline elapsed with no complete AU. Session stays
                    // valid — no eos, no depacketizer flush — so the caller
                    // can call recv_au_timeout again to keep waiting.
                    return Err(e);
                }
                Err(e) => return Err(e),
            };

            // ── Count at wire-level, before validation ────────────────────
            self.bytes_received = self.bytes_received.saturating_add(n as u64);
            self.packets_received = self.packets_received.saturating_add(1);

            // ── Decode RTP header ─────────────────────────────────────────
            let parsed = match RtpHeader::decode(&self.scratch[..n]) {
                Ok(p) => p,
                Err(_) => {
                    self.malformed_packets = self.malformed_packets.saturating_add(1);
                    continue;
                }
            };

            // ── PT filter ────────────────────────────────────────────────
            if parsed.header.payload_type != self.pt {
                self.malformed_packets = self.malformed_packets.saturating_add(1);
                continue;
            }

            // ── Feed depacketizer ─────────────────────────────────────────
            let payload = &self.scratch[parsed.payload_offset..parsed.payload_end];
            self.depay.feed(&parsed.header, payload);
            // Go back to step 1 to drain next_au.
        }
    }

    /// Statistics from the RFC 6184 depacketizer (AU counts, seq gaps,
    /// parameter-set updates, etc.).
    pub fn depay_stats(&self) -> H264DepayStats {
        self.depay.stats()
    }

    /// RTP-protocol-level statistics (malformed packet counter).
    pub fn rtp_stats(&self) -> RtpStats {
        RtpStats {
            malformed_packets: self.malformed_packets,
        }
    }

    /// Throughput counters projected into [`SocketStats`], mirroring
    /// `RtpRecvTransport::socket_stats`'s field mapping:
    ///
    /// | [`SocketStats`] field | Source |
    /// |---|---|
    /// | `bytes_received` / `packets_received` | Local counters; incremented on every received datagram/chunk before RTP-header or PT validation. Malformed-but-received packets are counted here; their drops are separately tracked in [`RtpStats::malformed_packets`] via [`Self::rtp_stats`]. |
    /// | `rtt_us` / `packets_lost_send` | Always 0 — RTCP is not implemented on this path (see the struct-level RTCP section). |
    /// | `bytes_sent` / `packets_sent` | 0 (this is the receive half) |
    /// | All other fields | 0 |
    pub fn socket_stats(&self) -> SocketStats {
        #[allow(clippy::field_reassign_with_default)]
        // SocketStats is non_exhaustive in tst-core, so the
        // default-and-assign pattern is the only way to construct one
        // from outside that crate. (Spelled without the attribute
        // syntax to keep the CI non_exhaustive line-grep honest.)
        let mut s = SocketStats::default();
        s.bytes_received = self.bytes_received;
        s.packets_received = self.packets_received;
        s
    }

    /// The local address the UDP socket is bound to, or `None` for the
    /// mpsc (TCP-interleaved) path.
    ///
    /// Tests use this to discover the ephemeral port assigned by the kernel
    /// when `port=0` was specified in the URL.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    /// Return a clone of the cancel handle. Setting the flag causes
    /// [`recv_au`](Self::recv_au) to stop blocking on I/O; it will not
    /// initiate any new reads. Already-assembled AUs and any partial AU
    /// produced by the flush are still returned before the terminal
    /// `Ok(None)` — see the [EOS contract](Self#eos-contract) above.
    ///
    /// The returned value is an `Arc<RtpCancelHandle>`-compatible clone —
    /// callers hold a shared reference and set it from any thread.
    pub fn cancel_handle(&self) -> Arc<RtpCancelHandle> {
        self.cancel.clone()
    }

    /// Cancel any in-progress [`recv_au`](Self::recv_au) and drop the
    /// underlying source. Idempotent.
    pub fn close(&mut self) {
        self.cancel.cancel();
        self.source = None;
        self.eos = true;
        self.end_reason.record(StreamEndReason::Cancelled);
    }
}

impl Drop for H264Receiver {
    fn drop(&mut self) {
        // Mirrors `RtpRecvTransport::drop`, which calls its own `close()`
        // — records `Cancelled` on drop, not just a bare cancel-flag set,
        // so the two mirrored receive shells have symmetric drop
        // semantics. `close()` is documented idempotent, so this is safe
        // even if the caller already called it explicitly.
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::depacketizer::H264DepayConfig;

    /// Minimal UDP loopback: bind an `H264Receiver` on an ephemeral port,
    /// send one well-formed IDR NALU packet (single-NALU mode, M=1, PT=96),
    /// verify the reassembled AU has the expected Annex B framing and the
    /// `key_frame` flag set.
    ///
    /// Packet layout (RFC 3550 §5.1 + RFC 6184 §5.6):
    ///   Byte 0:  0x80  (V=2, P=0, X=0, CC=0)
    ///   Byte 1:  0x80 | 96  (M=1, PT=96)
    ///   Bytes 2..4: seq=1
    ///   Bytes 4..8: ts=0x00002328 (9000 ticks)
    ///   Bytes 8..12: ssrc=9
    ///   Byte 12: 0x65 (NALU type 5 = IDR)
    ///   Bytes 13..: payload bytes
    #[test]
    fn h264_receiver_udp_loopback_single_au() {
        let mut rx = H264Receiver::listen("rtp://127.0.0.1:0?pt=96").unwrap();
        let dst = rx.local_addr().unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        // Hand-built per RFC 3550 §5.1: V=2, M=1, PT=96, seq=1, ts=9000,
        // ssrc=9; payload = type-5 IDR bytes 0x65 0xAB 0xCD.
        let pkt = [
            0x80u8,
            0x80 | 96,
            0,
            1,
            0,
            0,
            0x23,
            0x28,
            0,
            0,
            0,
            9,
            0x65,
            0xAB,
            0xCD,
        ];
        tx.send_to(&pkt, dst).unwrap();
        let au = rx.recv_au().unwrap().expect("AU expected");
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x65, 0xAB, 0xCD]);
        assert!(au.key_frame);
        // Throughput counters: exactly one wire packet of pkt.len() bytes.
        let stats = rx.socket_stats();
        assert_eq!(stats.packets_received, 1);
        assert_eq!(stats.bytes_received, pkt.len() as u64);
        rx.close();
    }

    /// The `?recv_timeout=<ms>` URL knob must arm the receiver on the
    /// direct `rtp://...?pt=...` listen path — not just the RTSP-session
    /// conversions in `session.rs`. Mirrors `transport.rs`'s
    /// `url_recv_timeout_arms_transport_without_explicit_setter`: a silent
    /// UDP source (nothing ever sent) must return
    /// `Err(TransportError::Backpressure)` from `recv_au()` within a
    /// bounded wait, with NO explicit `set_recv_timeout` call. Asserts the
    /// error kind, not timing — same hang-proof + unblock-lever pattern as
    /// the transport.rs sibling test (no local-addr getter issue here since
    /// `local_addr()` exists, but the lever still guards against a
    /// regression to infinite block).
    #[test]
    fn url_recv_timeout_arms_h264_receiver_without_explicit_setter() {
        let mut rx = H264Receiver::listen("rtp://127.0.0.1:0?pt=96&recv_timeout=200").unwrap();
        let dst = rx.local_addr().unwrap();

        let worker = std::thread::spawn(move || {
            let r = rx.recv_au();
            (r, rx)
        });
        let hang_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() && std::time::Instant::now() < hang_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !worker.is_finished() {
            // Unblock lever: a valid RTP+H264 packet makes a hung recv
            // return data so the test FAILS instead of wedging.
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            let pkt = [0x80u8, 0x80 | 96, 0, 1, 0, 0, 0x23, 0x28, 0, 0, 0, 9, 0x65];
            let _ = s.send_to(&pkt, dst);
            let _ = worker.join();
            panic!("?recv_timeout= URL knob did not arm the H264Receiver (blocked >= 5 s)");
        }
        let (result, _rx) = worker.join().unwrap();
        match result {
            Err(TransportError::Backpressure { .. }) => {}
            other => panic!("expected Backpressure on expiry, got {other:?}"),
        }
    }

    /// `listen` without `?pt=` must return `MissingPayloadTypeParam`.
    #[test]
    fn listen_without_pt_errors() {
        let result = H264Receiver::listen("rtp://127.0.0.1:0").map(|_| ());
        let err = result.expect_err("listen without ?pt= must fail");
        assert!(
            matches!(err, ConnectError::MissingPayloadTypeParam),
            "expected MissingPayloadTypeParam, got {err:?}"
        );
    }

    /// `?pt=33` is rejected at the URL-parse level (`BadPayloadType`),
    /// BEFORE the missing-pt check ever runs — distinct from
    /// `MissingPayloadTypeParam` (absent `?pt=`, previous test).
    #[test]
    fn listen_with_pt_33_rejected_at_url_level() {
        let err = H264Receiver::listen("rtp://127.0.0.1:0?pt=33")
            .map(|_| ())
            .expect_err("?pt=33 must fail at URL parse");
        assert!(
            matches!(
                err,
                ConnectError::Url(crate::url::UrlError::BadPayloadType { .. })
            ),
            "expected Url(BadPayloadType), got {err:?}"
        );
    }

    /// `from_mpsc_with_rtcp_drain(rx, None, config)` + send a single IDR packet via the channel.
    #[test]
    fn h264_receiver_mpsc_single_au() {
        use crate::packet::RTP_HEADER_LEN;
        use bytes::Bytes;

        let (tx, rx) = std::sync::mpsc::channel::<Bytes>();
        let config = H264DepayConfig {
            payload_type: 96,
            ..H264DepayConfig::default()
        };
        let mut receiver = H264Receiver::from_mpsc_with_rtcp_drain(rx, None, config);

        // Build a whole RTP packet (header + payload).
        let mut pkt = vec![0u8; RTP_HEADER_LEN];
        let mut hdr = crate::packet::RtpHeader::new(1, 9000, 0xABCD);
        hdr.marker = true;
        hdr.payload_type = 96;
        hdr.encode_into(&mut pkt);
        pkt.extend_from_slice(&[0x65u8, 0xEF]); // IDR slice bytes
        tx.send(Bytes::from(pkt)).unwrap();

        let au = receiver.recv_au().unwrap().expect("AU expected");
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x65, 0xEF]);
        assert!(au.key_frame);
    }

    /// EOS via mpsc disconnect — dropping the sender should yield `Ok(None)`.
    #[test]
    fn mpsc_disconnect_yields_eos() {
        use bytes::Bytes;
        let (tx, rx) = std::sync::mpsc::channel::<Bytes>();
        let config = H264DepayConfig {
            payload_type: 96,
            ..H264DepayConfig::default()
        };
        let mut receiver = H264Receiver::from_mpsc_with_rtcp_drain(rx, None, config);
        drop(tx); // causes Disconnected on recv_raw
        let result = receiver.recv_au().unwrap();
        assert!(result.is_none(), "expected Ok(None) at EOS, got {result:?}");
    }

    /// A stalled source (no packets, no disconnect) must not block
    /// `recv_au_timeout` past its deadline — the stall-watchdog case.
    /// The session must stay usable afterward: a
    /// second timed wait still works, and a subsequent disconnect still
    /// surfaces as clean EOS (not swallowed by the deadline path).
    #[test]
    fn recv_au_timeout_returns_backpressure_on_stalled_source() {
        use std::time::{Duration, Instant};
        let (tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut r = H264Receiver::from_mpsc_with_rtcp_drain(rx, None, H264DepayConfig::default());
        let start = Instant::now();
        let res = r.recv_au_timeout(Duration::from_millis(300));
        assert!(
            matches!(res, Err(TransportError::Backpressure { .. })),
            "got {res:?}"
        );
        let dt = start.elapsed();
        assert!(
            dt >= Duration::from_millis(250) && dt < Duration::from_secs(5),
            "elapsed {dt:?}"
        );
        // Session stays valid: a second wait works, and EOS still surfaces.
        let res2 = r.recv_au_timeout(Duration::from_millis(100));
        assert!(matches!(res2, Err(TransportError::Backpressure { .. })));
        drop(tx);
        assert!(matches!(
            r.recv_au_timeout(Duration::from_secs(2)),
            Ok(None)
        ));
    }

    /// Extreme timeout inputs are specified, not panics: `Duration::ZERO`
    /// expires at the first poll; `Duration::MAX` (unrepresentable as an
    /// `Instant`) saturates to "no deadline" via `checked_add` — the old
    /// unchecked `Instant::now() + timeout` panicked on it.
    #[test]
    fn recv_au_timeout_extreme_durations_never_panic() {
        let (tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut r = H264Receiver::from_mpsc_with_rtcp_drain(rx, None, H264DepayConfig::default());
        let res = r.recv_au_timeout(std::time::Duration::ZERO);
        assert!(
            matches!(res, Err(TransportError::Backpressure { .. })),
            "ZERO must expire promptly, got {res:?}"
        );
        // MAX: with the source disconnected, the no-deadline wait still
        // terminates via clean EOS — proving the call neither panicked
        // nor treated saturation as an instant expiry.
        drop(tx);
        assert!(matches!(
            r.recv_au_timeout(std::time::Duration::MAX),
            Ok(None)
        ));
    }

    /// The persistent `set_recv_timeout` knob applies to `recv_au()`: a
    /// silent source expires with `Backpressure` (no eos, no depacketizer
    /// flush), and the session stays usable afterward — a subsequent
    /// `recv_au_timeout` against a now-sending source still yields the AU.
    #[test]
    fn recv_au_honors_knob_then_session_recovers() {
        use crate::packet::RTP_HEADER_LEN;
        use bytes::Bytes;

        let (tx, rx) = std::sync::mpsc::channel::<Bytes>();
        let mut r = H264Receiver::from_mpsc_with_rtcp_drain(rx, None, H264DepayConfig::default());
        r.set_recv_timeout(Some(Duration::from_millis(150)));

        // Silent source: recv_au() (no explicit timeout arg) must honor
        // the configured knob and expire, not block forever.
        let res = r.recv_au();
        assert!(
            matches!(res, Err(TransportError::Backpressure { .. })),
            "got {res:?}"
        );

        // Session stayed valid: send an AU and confirm the one-shot
        // recv_au_timeout still receives it normally.
        let mut pkt = vec![0u8; RTP_HEADER_LEN];
        let mut hdr = crate::packet::RtpHeader::new(1, 9000, 0xABCD);
        hdr.marker = true;
        hdr.payload_type = 96;
        hdr.encode_into(&mut pkt);
        pkt.extend_from_slice(&[0x65u8, 0xEF]);
        tx.send(Bytes::from(pkt)).unwrap();
        let au = r
            .recv_au_timeout(Duration::from_secs(2))
            .unwrap()
            .expect("AU expected after knob expiry");
        assert_eq!(au.annexb, [0, 0, 0, 1, 0x65, 0xEF]);
    }

    /// `set_recv_timeout(Some(Duration::MAX))` must not panic (the
    /// `checked_add` saturation idiom), and must not be mistaken for an
    /// instant expiry: a disconnect-driven `recv_au()` call under the MAX
    /// knob still resolves via clean EOS (not a hang, not a panic). The
    /// one-shot `recv_au_timeout` with a short duration still expires
    /// promptly — proving the MAX knob didn't leak into its own deadline.
    #[test]
    fn set_recv_timeout_max_saturates_no_deadline() {
        let (tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut r = H264Receiver::from_mpsc_with_rtcp_drain(rx, None, H264DepayConfig::default());
        r.set_recv_timeout(Some(Duration::MAX));

        // Per-call still ignores the knob and expires on its own short
        // duration.
        let res = r.recv_au_timeout(Duration::from_millis(50));
        assert!(
            matches!(res, Err(TransportError::Backpressure { .. })),
            "got {res:?}"
        );

        // recv_au() under the MAX (no-deadline) knob doesn't hang or
        // panic — a clean disconnect still resolves it via EOS.
        drop(tx);
        assert!(matches!(r.recv_au(), Ok(None)));
    }

    /// `recv_au_timeout`'s explicit argument always wins over a configured
    /// knob — a long knob does not stretch a short one-shot call.
    #[test]
    fn recv_au_timeout_ignores_knob() {
        let (tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut r = H264Receiver::from_mpsc_with_rtcp_drain(rx, None, H264DepayConfig::default());
        r.set_recv_timeout(Some(Duration::from_secs(10)));
        let res = r.recv_au_timeout(Duration::from_millis(50));
        assert!(
            matches!(res, Err(TransportError::Backpressure { .. })),
            "got {res:?}"
        );
        drop(tx);
    }

    // ── StreamEndReason: plain rtp:// receiver (no owning RtspClient) ───

    /// A fresh, never-touched receiver reports `None`.
    #[test]
    fn end_reason_none_on_fresh_receiver() {
        let (_tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let r = H264Receiver::from_mpsc_with_rtcp_drain(rx, None, H264DepayConfig::default());
        assert!(r.end_reason().is_none());
    }

    /// `close()` is the ONLY writer for a plain (non-RTSP) receiver — it
    /// must record `Cancelled`.
    #[test]
    fn close_records_cancelled_on_plain_receiver() {
        let (_tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut r = H264Receiver::from_mpsc_with_rtcp_drain(rx, None, H264DepayConfig::default());
        r.close();
        assert!(
            matches!(r.end_reason(), Some(StreamEndReason::Cancelled)),
            "close() must record Cancelled, got {:?}",
            r.end_reason()
        );
    }

    /// Firing the cancel handle (without calling `close()`) is observed
    /// on the next `recv_au()` call — flushes via the normal EOS path and
    /// records `Cancelled`.
    #[test]
    fn cancel_handle_fire_records_cancelled_on_next_recv() {
        let (_tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut r = H264Receiver::from_mpsc_with_rtcp_drain(rx, None, H264DepayConfig::default());
        r.cancel_handle().cancel();
        let result = r.recv_au();
        assert!(
            matches!(result, Ok(None)),
            "a cancelled receiver must still surface clean EOS, got {result:?}"
        );
        assert!(
            matches!(r.end_reason(), Some(StreamEndReason::Cancelled)),
            "a cancel-handle fire observed at recv must record Cancelled, got {:?}",
            r.end_reason()
        );
    }

    /// `Drop` must record `Cancelled` too, mirroring
    /// `RtpRecvTransport::drop` (which calls its own `close()`) —
    /// otherwise a caller who just drops the receiver without an
    /// explicit `close()` call gets no end-reason record at all.
    /// `H264Receiver` has no post-drop accessor (no
    /// `end_reason_handle()`), so this clones the private `end_reason`
    /// slot before dropping the receiver — the slot's underlying cell is
    /// shared by clone, so it still observes whatever `Drop` records.
    #[test]
    fn drop_records_cancelled() {
        let (_tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let r = H264Receiver::from_mpsc_with_rtcp_drain(rx, None, H264DepayConfig::default());
        let slot = r.end_reason.clone();
        assert!(slot.get().is_none(), "fresh receiver must start unrecorded");
        drop(r);
        assert!(
            matches!(slot.get(), Some(StreamEndReason::Cancelled)),
            "Drop must record Cancelled, got {:?}",
            slot.get()
        );
    }
}
