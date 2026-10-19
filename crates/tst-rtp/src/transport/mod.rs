//! `RtpTransport` (send) + `RtpRecvTransport` (recv) — sync UDP socket
//! wrappers behind the [`tst_core::transport`] traits.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! The UDP data plane; the RTSP control plane lives in `rtsp`. On this
//! path sender + receiver agree on a fixed `host:port` and use it directly.
//!
//! The sending half ([`RtpTransport`]) and the receiving half
//! ([`RtpRecvTransport`], with its byte source) live in separate files,
//! with the thread that feeds RTCP reports from an interleaved RTSP session
//! into a receiver's statistics beside them.

use std::io;

use crate::url::RtpUrlError;

mod recv;
mod rtcp_ingest;
mod send;

pub(crate) use recv::{MPSC_PUMP_DISCONNECTED, RECV_SCRATCH_LEN, Source, record_end};
// The publisher adapters validate TS payloads with the receiver's rule; the
// bare plane has no publisher, so the re-export follows the server feature.
#[cfg(feature = "rtsp-server")]
pub(crate) use recv::is_valid_mp2t_payload;
pub use recv::{RtpRecvTransport, RtpStats};
pub use send::RtpTransport;

/// Convert an `io::Error` from the shared UDP socket helpers into a
/// `RtpConnectError`. `io::ErrorKind::Unsupported` (produced by
/// `apply_multicast_iface` / `set_multicast_hops_v6` / `set_multicast_if_v4`)
/// maps to `RtpConnectError::IfaceUnsupported`; everything else maps to
/// `RtpConnectError::Io`.
pub(super) fn udp_err_to_connect(e: io::Error) -> RtpConnectError {
    if e.kind() == io::ErrorKind::Unsupported {
        RtpConnectError::IfaceUnsupported {
            iface: String::new(),
            detail: e.to_string(),
        }
    } else {
        RtpConnectError::Io(e)
    }
}

/// Failure shape for [`RtpTransport::connect`] / [`RtpTransport::connect_with`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RtpConnectError {
    #[error("URL has ?pt= (elementary RTP): use H264Receiver, not the MP2T transport")]
    PayloadTypeParam,
    #[error("H264Receiver requires ?pt=<dynamic PT> on the URL")]
    MissingPayloadTypeParam,
    #[error("URL parse failed: {0}")]
    Url(#[from] RtpUrlError),
    /// `RtpUrl::host` couldn't be parsed as a literal IP. The RTP
    /// transport doesn't do DNS resolution — callers can pre-resolve and pass the
    /// literal.
    #[error("host '{host}' is not a literal IPv4/IPv6 address: {detail}")]
    HostNotLiteral { host: String, detail: String },
    /// OS-level socket failure (bind, connect, setsockopt).
    #[error("UDP socket error: {0}")]
    Io(#[from] io::Error),
    /// `?iface=` couldn't be applied — typically because the platform
    /// requires a different form (e.g., IPv6 needs scope-id integer).
    #[error("multicast iface '{iface}' unsupported: {detail}")]
    IfaceUnsupported { iface: String, detail: String },
}

/// The name this type had before it was renamed.
#[deprecated(since = "0.9.0", note = "renamed to `RtpConnectError`")]
pub type ConnectError = RtpConnectError;

/// Helper: 4 random bytes from `getrandom`.
pub(super) fn random_u32() -> u32 {
    let mut buf = [0u8; 4];
    // `getrandom` Result type cannot fail on a healthy system; if it
    // somehow does, fall back to a process-stable default (0). Logging
    // a tracing event preserves the diagnostic.
    if let Err(e) = getrandom::fill(&mut buf) {
        tracing::warn!(error = %e, "getrandom failed; using zero for RTP randomness field");
    }
    u32::from_be_bytes(buf)
}

#[cfg(test)]
mod tests {
    use std::net::UdpSocket;
    use std::time::Duration;

    use tst_core::transport::{EndReason, RecvTransport, Transport, TransportError};

    use super::recv::rtp_receiver_can_end_with;
    use super::*;
    use crate::packet::{RTP_HEADER_LEN, RTP_PT_MP2T};
    use crate::url::RtpUrl;

    /// Build a minimal valid RTP packet (V=2, P=0, X=0, CC=0, PT=33)
    /// wrapping `payload`. Used by mpsc-path tests that feed whole RTP
    /// packets into `from_packet_channel`, matching the pump's
    /// contract (the pump does not strip the header before enqueuing).
    fn make_rtp_packet(payload: &[u8]) -> bytes::Bytes {
        use crate::packet::RtpHeader;
        let mut pkt = vec![0u8; RTP_HEADER_LEN];
        RtpHeader::new(0, 0, 0).encode_into(&mut pkt);
        pkt.extend_from_slice(payload);
        bytes::Bytes::from(pkt)
    }

    /// socket_stats() returns Some(_) on a live transport. bytes_sent /
    /// packets_sent advance in the integration tests; here we just check
    /// the shape.
    #[test]
    fn socket_stats_returns_some_when_alive() {
        let url = RtpUrl::parse("rtp://127.0.0.1:1").unwrap();
        let t = RtpTransport::connect_with(&url).unwrap();
        let stats = t.socket_stats().expect("alive transport reports stats");
        assert_eq!(stats.bytes_sent, 0);
        assert_eq!(stats.packets_sent, 0);
        // RTCP-derived fields stay zero on the send side.
        assert_eq!(stats.rtt_us, 0);
        assert_eq!(stats.packets_lost_send, 0);
    }

    /// A quiet socket (no sender) must not block `recv_timeout` past its
    /// deadline — the RTP-side counterpart of the quiet-peer stall
    /// case. Mirrors `tst-udp`'s `close_unblocks_recv_bytes_after_recv_timeout`
    /// shape: `Ok(None)` on expiry, transport still alive.
    #[test]
    fn rtp_recv_timeout_returns_none_on_quiet_socket() {
        let mut t = RtpRecvTransport::listen_with(&RtpUrl::new("127.0.0.1", 0)).unwrap();
        let mut buf = vec![0u8; 2048];
        let start = std::time::Instant::now();
        let res = t
            .recv_timeout(&mut buf, std::time::Duration::from_millis(300))
            .unwrap();
        assert!(res.is_none());
        let dt = start.elapsed();
        assert!(
            dt >= Duration::from_millis(250) && dt < Duration::from_secs(5),
            "elapsed {dt:?}"
        );
        assert!(t.is_alive(), "transport must stay usable after a timeout");
        // A second call after an expiry must behave identically — pins
        // that a timeout leaves no poisoned state behind (deadline
        // bookkeeping, socket read-timeout restoration).
        let res2 = t
            .recv_timeout(&mut buf, std::time::Duration::from_millis(150))
            .unwrap();
        assert!(res2.is_none());
        assert!(t.is_alive(), "transport must survive repeated timeouts");
    }

    /// Extreme timeout inputs are specified, not panics: ZERO expires at
    /// the first poll (`Ok(None)`); MAX saturates to "no deadline" via
    /// `checked_add` (an unchecked add would panic) — proven by a
    /// delayed packet actually being received under a MAX timeout.
    #[test]
    fn rtp_recv_timeout_extreme_durations_never_panic() {
        // Discover a free base/base+1 pair first (RTP transports expose no
        // local-addr accessor; RTCP auto-binds port+1) — the same pattern
        // as the loopback integration tests.
        let port = {
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let base = s.local_addr().unwrap().port();
            assert!(base < u16::MAX, "ephemeral port at u16::MAX");
            drop(std::net::UdpSocket::bind(("127.0.0.1", base + 1)).unwrap());
            base
        };
        let mut t = RtpRecvTransport::listen_with(&RtpUrl::new("127.0.0.1", port)).unwrap();
        let mut buf = vec![0u8; 2048];
        let res = t.recv_timeout(&mut buf, std::time::Duration::ZERO).unwrap();
        assert!(res.is_none(), "ZERO must expire promptly");
        // MAX: a peer sends one minimal RTP packet after a short delay;
        // the saturated no-deadline wait must deliver it (not panic, not
        // return instantly empty).
        let sender = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            // RTP header (V=2, PT=33) + one valid MP2T packet as payload —
            // the receive path discards degenerate payloads (must be a
            // non-empty 188-multiple starting with the 0x47 sync byte),
            // so a header-only packet would be dropped and the wait would
            // continue.
            let mut pkt = vec![0u8; 12 + 188];
            pkt[0] = 0x80;
            pkt[1] = 33;
            pkt[12] = 0x47;
            let _ = s.send_to(&pkt, ("127.0.0.1", port));
        });
        let res = t.recv_timeout(&mut buf, std::time::Duration::MAX).unwrap();
        assert!(res.is_some(), "MAX wait must deliver the delayed packet");
        sender.join().unwrap();
    }

    /// Configured-timeout knob (`set_recv_timeout`): the blocking TRAIT
    /// `recv_bytes` path must honor a persistent deadline, surfacing
    /// `TransportError::Backpressure` on expiry with the transport left
    /// alive — the RTP mirror of `tst-srt`'s `SocketBuilder::recv_timeout`.
    /// Runs on the mpsc source: both sources share `recv_bytes_inner`'s
    /// deadline plumbing (per-source expiry mechanics are pinned by the
    /// one-shot `recv_timeout` tests above), and mpsc gives a
    /// deterministic unblock lever (drop the sender) so a regression to
    /// infinite-block fails loudly instead of wedging the suite.
    #[test]
    fn configured_recv_timeout_bounds_trait_recv_bytes() {
        let (tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_packet_channel(rx);
        t.set_recv_timeout(Some(Duration::from_millis(300)));

        let worker = std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            let start = std::time::Instant::now();
            let r = t.recv_bytes(&mut buf);
            (r, start.elapsed(), t)
        });
        // Hang-proof join: if recv_bytes ignores the configured deadline
        // (the pre-knob behavior), dropping the sender unblocks the
        // parked recv via the mpsc-disconnect path so this test FAILS
        // instead of hanging the binary.
        let hang_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() && std::time::Instant::now() < hang_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !worker.is_finished() {
            drop(tx);
            let _ = worker.join();
            panic!("recv_bytes ignored the configured timeout (blocked >= 5 s)");
        }
        let (result, elapsed, mut t) = worker.join().unwrap();
        match result {
            Err(TransportError::Backpressure { .. }) => {}
            other => panic!("expected Backpressure on expiry, got {other:?}"),
        }
        assert!(
            elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(5),
            "elapsed {elapsed:?}"
        );
        assert!(t.is_alive(), "transport must stay alive after expiry");

        // The session stays usable: a packet queued after the expiry is
        // delivered by the very next call.
        let payload = {
            let mut p = [0u8; 188];
            p[0] = 0x47;
            p
        };
        tx.send(make_rtp_packet(&payload)).unwrap();
        let mut buf = vec![0u8; 2048];
        let n = t
            .recv_bytes(&mut buf)
            .expect("post-expiry recv must deliver");
        assert_eq!(n, 188);
        assert_eq!(buf[0], 0x47);

        // And a second quiet interval expires the same way — pins that a
        // timeout leaves no poisoned deadline bookkeeping behind.
        match t.recv_bytes(&mut buf) {
            Err(TransportError::Backpressure { .. }) => {}
            other => panic!("expected Backpressure on second expiry, got {other:?}"),
        }
    }

    /// The headline consumer contract: `DemuxReceiver` over an
    /// `RtpRecvTransport` with a configured timeout surfaces
    /// `ShellErrorKind::Backpressure` on a stalled-but-healthy session
    /// (peer stops sending; no error, no EOS) — the shell's documented
    /// Backpressure-on-recv-timeout path — and the same receiver keeps
    /// demuxing once bytes flow again.
    /// A deadline-driven stall watchdog with no cancel thread.
    #[test]
    fn demux_receiver_surfaces_backpressure_then_keeps_demuxing() {
        use tst_core::mpegts::common::Pts90khz;
        use tst_core::mpegts::demux::DemuxEvent;
        use tst_core::mpegts::mux::{Muxer, MuxerConfig};
        use tst_pipeline::{DemuxReceiver, ShellErrorKind};

        // Real muxed TS bytes (PAT + PMT + video PES) so the demuxer has
        // something to emit once the stall ends. Ten AUs, not one: the
        // receive shell's TS syncer needs several consecutive aligned
        // packets to declare lock before it emits the FIRST packet (see
        // `Receiver::next_packet`'s doc example), so a 3-packet burst
        // would never reach the demuxer. Minimal Annex-B AUs: the muxer
        // doesn't parse past the start-code shape.
        let mut mux = Muxer::new(MuxerConfig::default()).expect("valid default config");
        let au = [0x00, 0x00, 0x00, 0x01, 0x65, 0xBB];
        for i in 0..10i64 {
            mux.push_video(&au, Pts90khz::new(i * 3000), i == 0)
                .expect("push_video");
        }
        let mut ts = Vec::new();
        let mut pkt = [0u8; 188];
        loop {
            let n = mux.pull(&mut pkt);
            if n == 0 {
                break;
            }
            ts.extend_from_slice(&pkt[..n]);
        }
        assert!(
            !ts.is_empty() && ts.len() % 188 == 0,
            "muxer output shape (got {} bytes)",
            ts.len()
        );

        let (tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_packet_channel(rx);
        t.set_recv_timeout(Some(Duration::from_millis(300)));
        let mut shell = DemuxReceiver::new(t);

        let worker = std::thread::spawn(move || {
            let r = shell.recv_event();
            (r, shell)
        });
        // Same hang-proofing as the trait-level test above.
        let hang_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() && std::time::Instant::now() < hang_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !worker.is_finished() {
            drop(tx);
            let _ = worker.join();
            panic!("recv_event ignored the configured timeout (blocked >= 5 s)");
        }
        let (result, mut shell) = worker.join().unwrap();
        let err = match result {
            Err(e) => e,
            other => panic!("expected a Backpressure-kind error on stall, got {other:?}"),
        };
        assert_eq!(
            err.kind,
            ShellErrorKind::Backpressure,
            "stall must surface as Backpressure, got {err:?}"
        );

        // Stall ends: the muxed bytes arrive as one RTP packet and the
        // SAME receiver emits the PSI map — no rebuild, no cancel thread.
        tx.send(make_rtp_packet(&ts)).unwrap();
        let ev = shell
            .recv_event()
            .expect("post-stall recv_event must succeed")
            .expect("post-stall recv_event must yield an event");
        assert!(
            matches!(ev, DemuxEvent::ProgramMap(_)),
            "first event should be the PSI map"
        );
    }

    /// `Duration::MAX` as a configured timeout must saturate to "no
    /// deadline" (checked_add), not panic — the configured-path mirror
    /// of the one-shot extreme-duration pin above.
    #[test]
    fn configured_recv_timeout_duration_max_saturates() {
        let (tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_packet_channel(rx);
        t.set_recv_timeout(Some(Duration::MAX));
        let payload = {
            let mut p = [0u8; 188];
            p[0] = 0x47;
            p
        };
        tx.send(make_rtp_packet(&payload)).unwrap();
        let mut buf = vec![0u8; 2048];
        let n = t
            .recv_bytes(&mut buf)
            .expect("MAX-saturated recv must deliver");
        assert_eq!(n, 188);
    }

    /// `set_recv_timeout(None)` restores the indefinite-block contract:
    /// after clearing a previously configured 250 ms timeout, a quiet
    /// recv must NOT surface Backpressure at the old deadline. The
    /// timing assertion points in the safe direction — an early worker
    /// return can only mean the cleared timeout still fired.
    #[test]
    fn set_recv_timeout_none_restores_infinite_block() {
        let (tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_packet_channel(rx);
        t.set_recv_timeout(Some(Duration::from_millis(250)));
        t.set_recv_timeout(None);

        let worker = std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            t.recv_bytes(&mut buf)
        });
        // Watch well past the cleared 250 ms deadline (plus the ~100 ms
        // poll granularity): the worker must still be parked throughout.
        let check_deadline = std::time::Instant::now() + Duration::from_millis(900);
        while std::time::Instant::now() < check_deadline {
            std::thread::sleep(Duration::from_millis(50));
            assert!(
                !worker.is_finished(),
                "recv_bytes returned early — the cleared timeout still fired"
            );
        }
        // Unblock via channel disconnect and confirm the exit is the
        // disconnect error, not a stale Backpressure.
        drop(tx);
        match worker.join().unwrap() {
            Err(TransportError::Backpressure { .. }) => {
                panic!("cleared timeout must not produce Backpressure")
            }
            Err(_) => {}
            Ok(n) => panic!("unexpected data on an empty channel: {n}"),
        }
    }

    /// The one-shot `recv_timeout` ignores the configured persistent
    /// value — its explicit argument wins. With a 60 s persistent
    /// timeout configured, an explicit 250 ms one-shot must still
    /// expire at ~250 ms (`Ok(None)`), not inherit the 60 s value.
    #[test]
    fn one_shot_recv_timeout_wins_over_configured() {
        let (tx, rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_packet_channel(rx);
        t.set_recv_timeout(Some(Duration::from_secs(60)));

        let worker = std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            let start = std::time::Instant::now();
            let r = t.recv_timeout(&mut buf, Duration::from_millis(250));
            (r, start.elapsed())
        });
        let hang_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() && std::time::Instant::now() < hang_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !worker.is_finished() {
            drop(tx);
            let _ = worker.join();
            panic!("one-shot recv_timeout inherited the 60 s configured value");
        }
        let (result, elapsed) = worker.join().unwrap();
        assert!(
            result.expect("one-shot expiry is Ok(None)").is_none(),
            "expected expiry, got data"
        );
        assert!(
            elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(5),
            "elapsed {elapsed:?}"
        );
    }

    /// The configured knob on the REAL UDP source — the persistent-path
    /// twin of the mpsc trait-level test above: a quiet socket with a
    /// 300 ms configured timeout must surface `Backpressure` through the
    /// trait `recv_bytes`, transport still alive. The port must be known
    /// up front (the transport exposes no local-addr getter and the
    /// wake-packet unblock lever below needs a destination), so the test
    /// discovers-then-releases an ephemeral port and retries the build
    /// on the rare cross-process steal.
    #[test]
    fn configured_recv_timeout_bounds_udp_trait_recv_bytes() {
        let (mut t, port) = {
            let mut out = None;
            for _ in 0..50 {
                let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                let candidate = s.local_addr().unwrap().port();
                drop(s);
                if let Ok(built) =
                    RtpRecvTransport::listen_with(&RtpUrl::new("127.0.0.1", candidate))
                {
                    out = Some((built, candidate));
                    break;
                }
            }
            out.expect("could not allocate a loopback UDP port in 50 attempts")
        };
        t.set_recv_timeout(Some(Duration::from_millis(300)));

        let worker = std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            let start = std::time::Instant::now();
            let r = t.recv_bytes(&mut buf);
            (r, start.elapsed(), t)
        });
        let hang_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() && std::time::Instant::now() < hang_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !worker.is_finished() {
            // Unblock lever: a valid RTP+MP2T packet makes a hung recv
            // return data so the test FAILS instead of wedging.
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut pkt = vec![0u8; 12 + 188];
            pkt[0] = 0x80;
            pkt[1] = 33;
            pkt[12] = 0x47;
            let _ = s.send_to(&pkt, ("127.0.0.1", port));
            let _ = worker.join();
            panic!("UDP trait recv_bytes ignored the configured timeout");
        }
        let (result, elapsed, t) = worker.join().unwrap();
        match result {
            Err(TransportError::Backpressure { .. }) => {}
            other => panic!("expected Backpressure on expiry, got {other:?}"),
        }
        assert!(
            elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(5),
            "elapsed {elapsed:?}"
        );
        assert!(t.is_alive(), "transport must stay alive after expiry");
    }

    /// The `?recv_timeout=<ms>` URL knob (parsed by `RtpUrl`) must arm
    /// the transport with NO explicit `set_recv_timeout` call —
    /// `RtpRecvTransport::listen`'s URL path is the sibling of the
    /// builder/trait-level knob tests above, proving the query key alone
    /// is enough. Same ephemeral-port-discovery + unblock-lever pattern as
    /// `configured_recv_timeout_bounds_udp_trait_recv_bytes` (the transport
    /// exposes no local-addr getter). Asserts the error kind, not timing —
    /// the elapsed-duration pin already lives on the setter-driven test
    /// above.
    #[test]
    fn url_recv_timeout_arms_transport_without_explicit_setter() {
        let (mut t, port) = {
            let mut out = None;
            for _ in 0..50 {
                let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                let candidate = s.local_addr().unwrap().port();
                drop(s);
                let url = format!("rtp://127.0.0.1:{candidate}?recv_timeout=200");
                if let Ok(built) = RtpRecvTransport::listen(&url) {
                    out = Some((built, candidate));
                    break;
                }
            }
            out.expect("could not allocate a loopback UDP port in 50 attempts")
        };

        let worker = std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            let r = t.recv_bytes(&mut buf);
            (r, t)
        });
        let hang_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !worker.is_finished() && std::time::Instant::now() < hang_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !worker.is_finished() {
            // Unblock lever: a valid RTP+MP2T packet makes a hung recv
            // return data so the test FAILS instead of wedging.
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut pkt = vec![0u8; 12 + 188];
            pkt[0] = 0x80;
            pkt[1] = 33;
            pkt[12] = 0x47;
            let _ = s.send_to(&pkt, ("127.0.0.1", port));
            let _ = worker.join();
            panic!("?recv_timeout= URL knob did not arm the transport (blocked >= 5 s)");
        }
        let (result, t) = worker.join().unwrap();
        match result {
            Err(TransportError::Backpressure { .. }) => {}
            other => panic!("expected Backpressure on expiry, got {other:?}"),
        }
        assert!(t.is_alive(), "transport must stay alive after expiry");
    }

    /// Compile-time check: RtpTransport satisfies Transport (so
    /// MuxSender<RtpTransport> works) and RtpRecvTransport satisfies
    /// RecvTransport (so DemuxReceiver<RtpRecvTransport> works).
    /// Catches signature drift if the pipeline shell trait bounds tighten.
    #[test]
    fn satisfies_pipeline_trait_bounds() {
        fn accept_send<T: Transport>(_: T) {}
        fn accept_recv<T: RecvTransport>(_: T) {}
        // Use port 0 / 1 to avoid binding conflicts in CI.
        let send_url = RtpUrl::parse("rtp://127.0.0.1:1").unwrap();
        let recv_url = RtpUrl::parse("rtp://127.0.0.1:0").unwrap();
        let send = RtpTransport::connect_with(&send_url).unwrap();
        let recv = RtpRecvTransport::listen_with(&recv_url).unwrap();
        accept_send(send);
        accept_recv(recv);
    }

    /// An RR pushed onto the rtcp_rx channel of an `RtpRecvTransport`
    /// built via `from_mpsc_with_rtcp` reaches the ingest thread,
    /// populates `RtcpStats.cumulative_lost_send`, and is projected into
    /// `socket_stats().packets_lost_send`.
    #[test]
    fn rr_on_rtcp_rx_populates_packets_lost_send() {
        use crate::rtcp::{ReceiverReport, ReportBlock};

        let (_data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let (rtcp_tx, rtcp_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let t = RtpRecvTransport::from_mpsc_with_rtcp(data_rx, rtcp_rx);
        let our_ssrc = t.ssrc;

        // Build an RR whose first report block references our SSRC and
        // reports 2024 cumulative losses.
        let rb = ReportBlock {
            ssrc: our_ssrc,
            fraction_lost: 0x40, // 25% q8
            cumulative_lost: 2024,
            extended_highest_seq: 9000,
            jitter: 0,
            last_sr: 0,
            delay_since_last_sr: 0,
        };
        let rr = ReceiverReport {
            ssrc: 0xDEAD_BEEF,
            report_blocks: vec![rb],
        };
        let bytes = bytes::Bytes::from(rr.encode().unwrap());
        rtcp_tx.send(bytes).expect("send RR onto rtcp_rx");

        // Spin briefly for the ingest thread to wake + process.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut surfaced = 0u64;
        while std::time::Instant::now() < deadline {
            let s = t.socket_stats().expect("alive transport reports stats");
            if s.packets_lost_send > 0 {
                surfaced = s.packets_lost_send;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(
            surfaced, 2024,
            "expected RR-derived loss 2024 surfaced as packets_lost_send"
        );

        // RTT stays 0 because no SR anchor preceded the RR.
        let s = t.socket_stats().expect("alive transport reports stats");
        assert_eq!(s.rtt_us, 0);
    }

    /// SR + RR leave `rtt_us` at 0 after the ingest thread processes both
    /// packets. RTT computation is deferred (see
    /// `docs/project/deferred-features.md`): a non-zero RTT from SR→RR
    /// here would be wrong, because the anchor NTP comes from the PEER's
    /// SR (wrong clock domain for the formula).
    #[test]
    fn sr_then_rr_leaves_rtt_us_zero() {
        use crate::rtcp::{ReceiverReport, ReportBlock, SenderReport};

        let (_data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let (rtcp_tx, rtcp_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let t = RtpRecvTransport::from_mpsc_with_rtcp(data_rx, rtcp_rx);
        let our_ssrc = t.ssrc;

        // Peer SR — establishes the anchor.
        let sr_ntp_full: u64 = 0xDEAD_BEEF_0000_0000u64; // arbitrary
        let sr = SenderReport {
            ssrc: 0xCAFEBABE,
            ntp_timestamp: sr_ntp_full,
            rtp_timestamp: 0,
            sender_packet_count: 0,
            sender_octet_count: 0,
            report_blocks: vec![],
        };
        rtcp_tx
            .send(bytes::Bytes::from(sr.encode().unwrap()))
            .expect("send SR");

        // Deterministic wait: the SR must actually be ingested before we
        // proceed — otherwise the final rtt_us == 0 assertion would be
        // vacuously true (0 is also the never-processed default).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while t.rtcp_stats().sr_packets_received == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for SR ingest"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // Peer RR — RB's `last_sr` mirrors the anchor's mid-32 NTP.
        let last_sr_mid = ((sr_ntp_full >> 16) & 0xFFFF_FFFF) as u32;
        let rb = ReportBlock {
            ssrc: our_ssrc,
            fraction_lost: 0,
            cumulative_lost: 0,
            extended_highest_seq: 0,
            jitter: 0,
            last_sr: last_sr_mid,
            delay_since_last_sr: 0,
        };
        let rr = ReceiverReport {
            ssrc: 0xCAFEBABE,
            report_blocks: vec![rb],
        };
        rtcp_tx
            .send(bytes::Bytes::from(rr.encode().unwrap()))
            .expect("send RR");

        // Same deterministic wait for the RR: only after it is provably
        // ingested does the rtt_us == 0 assertion pin the new behavior.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while t.rtcp_stats().rr_packets_received == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for RR ingest"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let s = t.socket_stats().expect("alive transport reports stats");
        assert_eq!(
            s.rtt_us, 0,
            "rtt_us must stay 0 after SR+RR (RTT computation is deferred); got {}",
            s.rtt_us
        );
    }

    /// A malformed RTCP packet (PT=201 but truncated)
    /// increments `rtcp_stats().rr_parse_errors` and does NOT crash
    /// or corrupt the other stats.
    #[test]
    fn malformed_rr_increments_parse_error_counter() {
        let (_data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let (rtcp_tx, rtcp_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let t = RtpRecvTransport::from_mpsc_with_rtcp(data_rx, rtcp_rx);

        // 2-byte truncated "RR" — PT byte is 201 so the dispatcher
        // attempts to decode but ReceiverReport::decode rejects on
        // length check.
        let bytes = bytes::Bytes::from(vec![0x80u8, 201]);
        rtcp_tx.send(bytes).expect("send truncated RR");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut parse_errs = 0u64;
        while std::time::Instant::now() < deadline {
            parse_errs = t.rtcp_stats().rr_parse_errors;
            if parse_errs > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(parse_errs, 1);
        // Stats unaffected.
        assert_eq!(t.rtcp_stats().rr_packets_received, 0);
    }

    /// Guard: RtpTransport::connect with port 65535 must not panic (no u16
    /// overflow anywhere on the bind path). Ok or Err both accepted.
    #[test]
    fn rtp_connect_port_65535_does_not_panic() {
        let _ = RtpTransport::connect("rtp://127.0.0.1:65535");
    }

    /// Guard: RtpRecvTransport::listen with port 65535 must not panic.
    #[test]
    fn rtp_listen_port_65535_does_not_panic() {
        let _ = RtpRecvTransport::listen("rtp://127.0.0.1:65535");
    }

    // ── ?pt= rejection guards ─────────────────────────────────────────────────

    /// A `?pt=` URL must be rejected by the MP2T `RtpTransport::connect*` path.
    #[test]
    fn mp2t_connect_rejects_pt_param() {
        let url = RtpUrl::parse("rtp://127.0.0.1:5004?pt=96").unwrap();
        let err = RtpTransport::connect_with(&url)
            .map(|_| ())
            .expect_err("connect_with ?pt= must fail");
        assert!(
            matches!(err, RtpConnectError::PayloadTypeParam),
            "expected PayloadTypeParam, got {err:?}"
        );
    }

    /// A `?pt=` URL must be rejected by the MP2T `RtpRecvTransport::listen*` path.
    #[test]
    fn mp2t_listen_rejects_pt_param() {
        let url = RtpUrl::parse("rtp://127.0.0.1:0?pt=96").unwrap();
        let err = RtpRecvTransport::listen_with(&url)
            .map(|_| ())
            .expect_err("listen_with ?pt= must fail");
        assert!(
            matches!(err, RtpConnectError::PayloadTypeParam),
            "expected PayloadTypeParam, got {err:?}"
        );
    }

    // --- MP2T payload shape validation ---
    //
    // These tests drive the recv path via the mpsc seam (from_packet_channel).
    // The pump pushes whole RTP packets, so tests feed whole packets
    // (built with make_rtp_packet) rather than raw payload bytes directly.
    // The mpsc path is representative because the UDP path shares
    // is_valid_mp2t_payload and the same RTP header decode logic.

    /// A 100-byte payload that doesn't start with 0x47 must be dropped and
    /// must tick malformed_packets.
    #[test]
    fn mp2t_shape_invalid_non_aligned_drops_and_ticks_counter() {
        use tst_core::transport::RecvTransport;

        let (data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_packet_channel(data_rx);

        // Push a whole RTP packet wrapping a 100-byte non-0x47 TS "payload"
        // (not 188-aligned, wrong sync byte) — shape check rejects it.
        let malformed_ts = vec![0xAAu8; 100];
        data_tx
            .send(make_rtp_packet(&malformed_ts))
            .expect("send malformed payload");

        // Push a valid 188-byte TS packet (0x47 sync) so recv_bytes has
        // something to return after discarding the malformed one.
        let mut valid_pkt = vec![0x00u8; 188];
        valid_pkt[0] = 0x47;
        data_tx
            .send(make_rtp_packet(&valid_pkt))
            .expect("send valid payload");

        let mut buf = vec![0u8; 4096];
        // recv_bytes must skip the malformed payload and return the valid one.
        let n = t.recv_bytes(&mut buf).expect("expected valid packet");
        assert_eq!(n, 188, "valid 188-byte packet must be returned");
        assert_eq!(buf[0], 0x47, "returned packet must start with TS sync byte");

        // Counter must be 1 — exactly the malformed payload that was dropped.
        assert_eq!(
            t.rtp_stats().malformed_packets,
            1,
            "malformed_packets must be 1 after one shape-invalid payload"
        );
    }

    /// A valid 188-byte TS payload (starts with 0x47) must pass through
    /// unchanged and must NOT tick malformed_packets.
    #[test]
    fn mp2t_shape_valid_single_packet_passes_through() {
        use tst_core::transport::RecvTransport;

        let (data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_packet_channel(data_rx);

        let mut pkt = vec![0xABu8; 188];
        pkt[0] = 0x47;
        data_tx
            .send(make_rtp_packet(&pkt))
            .expect("send valid payload");

        let mut buf = vec![0u8; 4096];
        let n = t.recv_bytes(&mut buf).expect("valid packet must not error");
        assert_eq!(n, 188);
        assert_eq!(&buf[..188], pkt.as_slice());
        assert_eq!(
            t.rtp_stats().malformed_packets,
            0,
            "valid payload must not tick malformed_packets"
        );
    }

    /// A multi-packet 188*7=1316 byte bundle (all starting with 0x47 at
    /// offset 0) must pass through — RFC 2250 allows up to 7 TS packets
    /// per RTP datagram.
    #[test]
    fn mp2t_shape_valid_bundle_passes_through() {
        use tst_core::transport::RecvTransport;

        let (data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_packet_channel(data_rx);

        // 7-packet bundle: only the FIRST byte must be 0x47 for the shape check.
        let mut bundle = vec![0x00u8; 188 * 7];
        bundle[0] = 0x47;
        data_tx
            .send(make_rtp_packet(&bundle))
            .expect("send valid bundle");

        let mut buf = vec![0u8; 4096];
        let n = t.recv_bytes(&mut buf).expect("valid bundle must not error");
        assert_eq!(n, 188 * 7);
        assert_eq!(t.rtp_stats().malformed_packets, 0);
    }

    /// An empty TS payload (inside a valid RTP header) must be dropped and
    /// tick malformed_packets.
    #[test]
    fn mp2t_shape_empty_payload_drops_and_ticks_counter() {
        use tst_core::transport::RecvTransport;

        let (data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_packet_channel(data_rx);

        // Empty TS payload wrapped in a valid RTP header.
        data_tx
            .send(make_rtp_packet(&[]))
            .expect("send empty payload");

        // Follow with a valid packet so recv_bytes can return.
        let mut valid_pkt = vec![0u8; 188];
        valid_pkt[0] = 0x47;
        data_tx
            .send(make_rtp_packet(&valid_pkt))
            .expect("send valid payload");

        let mut buf = vec![0u8; 4096];
        let n = t.recv_bytes(&mut buf).expect("must return valid packet");
        assert_eq!(n, 188);
        assert_eq!(
            t.rtp_stats().malformed_packets,
            1,
            "empty payload must tick malformed_packets"
        );
    }

    /// Wire-level counter semantics on the mpsc path: `bytes_received` and
    /// `packets_received` must increment before MP2T-shape validation, so a
    /// malformed payload still advances the counters (same as the UDP path,
    /// which increments on `Ok(n)` before any header or shape check).
    ///
    /// We send one malformed (100-byte, wrong sync, wrapped in RTP) + one
    /// valid (188-byte, 0x47 sync, wrapped in RTP). After recv_bytes returns
    /// the valid one we expect packets_received == 2 and bytes_received to
    /// sum both whole RTP packets.
    #[test]
    fn mpsc_bytes_packets_received_counted_at_wire_level() {
        use tst_core::transport::RecvTransport;

        let (data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_packet_channel(data_rx);

        // Malformed TS payload: not 188-aligned, wrong sync byte.
        let malformed_ts = vec![0xAAu8; 100];
        let malformed_pkt = make_rtp_packet(&malformed_ts);
        let malformed_len = malformed_pkt.len(); // RTP_HEADER_LEN + 100

        // Valid: exactly one 188-byte TS packet.
        let mut valid_ts = vec![0x00u8; 188];
        valid_ts[0] = 0x47;
        let valid_pkt = make_rtp_packet(&valid_ts);
        let valid_len = valid_pkt.len(); // RTP_HEADER_LEN + 188

        data_tx.send(malformed_pkt).expect("send malformed payload");
        data_tx.send(valid_pkt).expect("send valid payload");

        let mut buf = vec![0u8; 4096];
        let n = t.recv_bytes(&mut buf).expect("valid packet must not error");
        assert_eq!(n, 188);

        let s = t.socket_stats().expect("alive transport reports stats");
        assert_eq!(
            s.packets_received, 2,
            "both malformed and valid packets must be counted at wire-level"
        );
        assert_eq!(
            s.bytes_received,
            (malformed_len + valid_len) as u64,
            "bytes_received must sum both whole RTP packets"
        );
        assert_eq!(
            t.rtp_stats().malformed_packets,
            1,
            "malformed_packets must be 1 for the one dropped payload"
        );
    }

    /// Regression: full-MTU RTP truncation. A conformant peer sending a
    /// full 7×188 MP2T bundle emits a 1328-byte datagram (12-byte RTP
    /// header + 1316-byte payload) — larger than a `pkt_size`-sized
    /// (1316 B) recv scratch, which `UdpSocket::recv` would silently
    /// truncate (and the shape guard would then drop). The scratch is
    /// sized to `RECV_SCRATCH_LEN` (the UDP datagram ceiling), so the whole
    /// payload must arrive intact with `malformed_packets == 0`.
    ///
    /// With a short scratch this test HANGS rather than failing cleanly: the truncated
    /// payload fails the shape guard, is dropped, and `recv_bytes` blocks
    /// waiting for a next packet that never comes (harness timeout catches
    /// a regression).
    #[test]
    fn udp_full_mtu_datagram_delivered_whole() {
        let recv_sock = UdpSocket::bind("127.0.0.1:0").expect("bind recv socket");
        let recv_addr = recv_sock.local_addr().expect("recv local addr");
        let mut t = RtpRecvTransport::from_udp_socket(recv_sock).expect("wrap recv socket");

        // 7×188 = 1316-byte TS bundle; every packet 0x47-led.
        let mut bundle = vec![0u8; 188 * 7];
        for chunk in bundle.chunks_mut(188) {
            chunk[0] = 0x47;
        }
        let pkt = make_rtp_packet(&bundle);
        assert_eq!(pkt.len(), 1328, "whole datagram must be 1328 bytes");

        let send_sock = UdpSocket::bind("127.0.0.1:0").expect("bind send socket");
        send_sock.send_to(&pkt, recv_addr).expect("send datagram");

        let mut buf = vec![0u8; 4096];
        let n = t
            .recv_bytes(&mut buf)
            .expect("full-MTU datagram must be delivered");
        assert_eq!(n, 188 * 7, "whole 1316-byte payload must be returned");
        assert_eq!(&buf[..n], bundle.as_slice(), "payload must be untruncated");
        assert_eq!(
            t.rtp_stats().malformed_packets,
            0,
            "conformant full-MTU datagram must not be counted malformed"
        );
    }

    /// Regression twin with a CSRC-bearing header: CC=15 adds 60 bytes of
    /// CSRC list, so the whole datagram is 12 + 60 + 1316 = 1388 bytes —
    /// larger than even `pkt_size + RTP_HEADER_LEN`. Conformant per
    /// RFC 3550 §5.1; must be delivered whole.
    ///
    /// Pre-fix this test HANGS rather than failing cleanly (truncated →
    /// shape-guard drop → recv blocks on a packet that never comes);
    /// the harness timeout catches a regression.
    #[test]
    fn udp_csrc_bearing_oversize_datagram_delivered_whole() {
        let recv_sock = UdpSocket::bind("127.0.0.1:0").expect("bind recv socket");
        let recv_addr = recv_sock.local_addr().expect("recv local addr");
        let mut t = RtpRecvTransport::from_udp_socket(recv_sock).expect("wrap recv socket");

        let mut bundle = vec![0u8; 188 * 7];
        for chunk in bundle.chunks_mut(188) {
            chunk[0] = 0x47;
        }

        // V=2 | P=0 | X=0 | CC=15 → 0x8F; M=0 | PT=33.
        let mut pkt = vec![0x8Fu8, RTP_PT_MP2T];
        pkt.extend_from_slice(&[0u8; 10]); // seq/ts/ssrc
        pkt.extend_from_slice(&[0u8; 60]); // 15 CSRC entries × 4 bytes
        pkt.extend_from_slice(&bundle);
        assert_eq!(pkt.len(), 1388, "whole datagram must be 1388 bytes");

        let send_sock = UdpSocket::bind("127.0.0.1:0").expect("bind send socket");
        send_sock.send_to(&pkt, recv_addr).expect("send datagram");

        let mut buf = vec![0u8; 4096];
        let n = t
            .recv_bytes(&mut buf)
            .expect("CSRC-bearing datagram must be delivered");
        assert_eq!(n, 188 * 7, "payload after CSRC strip must be 1316 bytes");
        assert_eq!(&buf[..n], bundle.as_slice(), "payload must be untruncated");
        assert_eq!(t.rtp_stats().malformed_packets, 0);
    }

    /// Mpsc twin of the full-MTU pin: a whole 1328-byte interleaved frame
    /// (12-byte header + 7×188 payload) must round-trip through recv_raw's
    /// scratch copy without truncation or drop.
    #[test]
    fn mpsc_full_frame_1328_delivered_whole() {
        let (data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_packet_channel(data_rx);

        let mut bundle = vec![0u8; 188 * 7];
        for chunk in bundle.chunks_mut(188) {
            chunk[0] = 0x47;
        }
        let pkt = make_rtp_packet(&bundle);
        assert_eq!(pkt.len(), 1328, "whole frame must be 1328 bytes");
        data_tx.send(pkt).expect("send frame");

        let mut buf = vec![0u8; 4096];
        let n = t
            .recv_bytes(&mut buf)
            .expect("1328-byte frame must be delivered");
        assert_eq!(n, 188 * 7);
        assert_eq!(&buf[..n], bundle.as_slice());
        assert_eq!(t.rtp_stats().malformed_packets, 0);
    }

    /// Transport level: a whole RTP packet with CSRC list
    /// (CC>0), header extension (X=1), and trailing padding (P=1) arriving on
    /// the mpsc path must have ONLY the true TS payload reach the caller —
    /// CSRC words and extension skipped, padding trimmed. The pump delivers
    /// the whole packet; stripping is the transport's responsibility.
    #[test]
    fn interleaved_rtp_csrc_extension_padding_stripped_at_recv_site() {
        use crate::packet::RTP_PT_MP2T;
        use tst_core::transport::RecvTransport;

        // Build a whole RTP packet with CC=1, X=1, P=1, PT=33.
        //   Octet 0: V=2 | P=1 | X=1 | CC=1 = 0b10_1_1_0001 = 0xB1
        //   Octet 1: M=0 | PT=33
        //   Octets 2..12: seq/ts/ssrc (all zero)
        //   Octets 12..16: 1 CSRC entry (4 bytes)
        //   Octets 16..20: extension header (profile=0xBEDE, len=1 word)
        //   Octets 20..24: 1 word (4 bytes) of extension data
        //   Octets 24..212: 188-byte TS payload (0x47 sync + fill)
        //   Octets 212..214: 2 padding bytes (last byte = pad count = 2)
        let mut ts_payload = vec![0xABu8; 188];
        ts_payload[0] = 0x47;

        let mut rtp = vec![0xB1u8, RTP_PT_MP2T];
        rtp.extend_from_slice(&[0u8; 10]); // seq/ts/ssrc
        rtp.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // CSRC[0]
        rtp.extend_from_slice(&[0xBE, 0xDE, 0x00, 0x01]); // ext header, len=1 word
        rtp.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]); // ext data word
        rtp.extend_from_slice(&ts_payload); // 188-byte TS payload
        rtp.extend_from_slice(&[0x00, 0x02]); // 2 padding bytes (count=2)

        let (data_tx, data_rx) = std::sync::mpsc::channel::<bytes::Bytes>();
        let mut t = RtpRecvTransport::from_packet_channel(data_rx);

        data_tx
            .send(bytes::Bytes::from(rtp))
            .expect("send whole RTP packet");

        let mut buf = vec![0u8; 4096];
        let n = t
            .recv_bytes(&mut buf)
            .expect("valid MP2T payload must not error");
        assert_eq!(
            n, 188,
            "only the 188-byte TS payload must be returned (CSRC+ext skipped, padding trimmed)"
        );
        assert_eq!(
            &buf[..188],
            ts_payload.as_slice(),
            "returned bytes must be the exact TS payload"
        );
        assert_eq!(
            t.rtp_stats().malformed_packets,
            0,
            "well-formed packet must not tick malformed_packets"
        );
    }

    /// is_valid_mp2t_payload covers the helper directly.
    #[test]
    fn is_valid_mp2t_payload_covers_edge_cases() {
        // Empty — invalid.
        assert!(!recv::is_valid_mp2t_payload(&[]));
        // 100 bytes, 0x47 start — not 188-aligned, invalid.
        let mut v = vec![0x47u8; 100];
        assert!(!recv::is_valid_mp2t_payload(&v));
        // 188 bytes, wrong sync — invalid.
        v = vec![0xAAu8; 188];
        assert!(!recv::is_valid_mp2t_payload(&v));
        // 188 bytes, correct sync — valid.
        v = vec![0u8; 188];
        v[0] = 0x47;
        assert!(recv::is_valid_mp2t_payload(&v));
        // 376 bytes (2 packets), correct sync — valid.
        v = vec![0u8; 376];
        v[0] = 0x47;
        assert!(recv::is_valid_mp2t_payload(&v));
        // 189 bytes — not 188-aligned, invalid.
        v = vec![0x47u8; 189];
        assert!(!recv::is_valid_mp2t_payload(&v));
    }

    // ── EndReason: plain rtp:// transport (no owning RtspClient) ──

    /// A fresh, never-touched transport reports `None` — the session
    /// hasn't ended.
    #[test]
    fn a_boxed_rtp_receiver_still_reports_its_end_reason() {
        let t = RtpRecvTransport::listen("rtp://127.0.0.1:0").unwrap();
        let cancel =
            tst_core::transport::RecvTransport::cancel_handle(&t).expect("RTP has a cancel handle");
        let mut boxed: Box<dyn tst_core::transport::RecvTransport> = Box::new(t);
        let handle = boxed
            .end_reason_handle()
            .expect("RTP records its end reason");
        cancel.cancel();
        let mut buf = [0u8; 2048];
        let _ = boxed.recv_bytes(&mut buf);
        assert_eq!(
            handle.get(),
            Some(tst_core::transport::EndReason::Cancelled)
        );
    }

    #[test]
    fn an_rtp_or_rtsp_receiver_ends_only_with_the_documented_reasons() {
        use tst_core::transport::EndReason as E;
        let all = [
            E::EndOfStream,
            E::ReconnectExhausted,
            E::Cancelled,
            E::CleanTeardown,
            E::SessionExpired,
            E::KeepaliveFailed { msg: String::new() },
            E::TransportFailed { msg: String::new() },
            E::ProtocolError { msg: String::new() },
        ];
        let cannot: Vec<_> = all
            .iter()
            .filter(|r| !rtp_receiver_can_end_with(r))
            .cloned()
            .collect();
        assert_eq!(cannot, vec![E::EndOfStream, E::ReconnectExhausted]);
    }

    #[test]
    fn end_reason_none_on_fresh_transport() {
        let t = RtpRecvTransport::listen("rtp://127.0.0.1:0").unwrap();
        assert!(t.end_reason().is_none());
        assert!(t.end_reason_handle().get().is_none());
    }

    /// `RecvTransport::close()` is the ONLY writer for a plain `rtp://`
    /// transport (no pump, no keepalive) — it must record `Cancelled`.
    #[test]
    fn close_records_cancelled_on_plain_udp_transport() {
        let mut t = RtpRecvTransport::listen("rtp://127.0.0.1:0").unwrap();
        RecvTransport::close(&mut t);
        assert!(
            matches!(t.end_reason(), Some(EndReason::Cancelled)),
            "close() must record Cancelled, got {:?}",
            t.end_reason()
        );
    }

    /// Firing the transport's own cancel handle (without calling
    /// `close()`) must be observed on the NEXT `recv_bytes` call — it
    /// records `Cancelled` and returns `ExplicitClose`, and the recorded
    /// reason is visible through a handle obtained BEFORE the fire (the
    /// cross-thread-watchdog shape).
    #[test]
    fn cancel_handle_fire_records_cancelled_on_next_recv() {
        let mut t = RtpRecvTransport::listen("rtp://127.0.0.1:0").unwrap();
        let handle = t.end_reason_handle();
        assert!(handle.get().is_none());

        let cancel = t
            .cancel_handle()
            .expect("recv transport exposes a cancel handle");
        cancel.cancel();

        let mut buf = vec![0u8; 2048];
        let result = RecvTransport::recv_bytes(&mut t, &mut buf);
        assert!(
            matches!(result, Err(TransportError::ExplicitClose)),
            "expected ExplicitClose, got {result:?}"
        );
        assert!(
            matches!(t.end_reason(), Some(EndReason::Cancelled)),
            "a cancel-handle fire observed at recv must record Cancelled, got {:?}",
            t.end_reason()
        );
        assert!(
            matches!(handle.get(), Some(EndReason::Cancelled)),
            "a handle obtained before the fire must observe the same recording"
        );
    }

    /// "is_alive after cancel = false": once a cancel has been observed by
    /// an op, the transport must stop claiming it is alive. An `is_alive`
    /// that read only `source.is_some()` would stay true, because the
    /// ExplicitClose arm does not clear the source.
    #[test]
    fn recv_is_alive_is_false_once_a_cancel_is_observed() {
        let mut t = RtpRecvTransport::listen("rtp://127.0.0.1:0").unwrap();
        let cancel = t
            .cancel_handle()
            .expect("recv transport exposes a cancel handle");
        cancel.cancel();
        let mut buf = vec![0u8; 2048];
        let result = RecvTransport::recv_bytes(&mut t, &mut buf);
        assert!(
            matches!(result, Err(TransportError::ExplicitClose)),
            "got {result:?}"
        );
        assert!(
            !RecvTransport::is_alive(&t),
            "is_alive() must read false after a cancel was observed"
        );
    }

    /// An empty destination returns `Ok(0)` at once and touches nothing;
    /// without the guard it parks waiting for a datagram.
    #[test]
    fn recv_empty_buffer_is_a_noop() {
        let mut t = RtpRecvTransport::listen("rtp://127.0.0.1:0").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            let r = RecvTransport::recv_bytes(&mut t, &mut []);
            let alive = RecvTransport::is_alive(&t);
            let _ = tx.send((r, alive));
        });
        let (r, alive) = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("recv_bytes(&mut []) parked instead of returning Ok(0)");
        h.join().unwrap();
        assert!(matches!(r, Ok(0)), "got {r:?}");
        assert!(alive, "an empty recv must not latch the transport dead");
    }

    /// The send twin of the liveness row.
    #[test]
    fn send_is_alive_is_false_after_a_cancel() {
        let t = RtpTransport::connect("rtp://127.0.0.1:9").unwrap();
        let cancel = Transport::cancel_handle(&t).expect("send transport exposes a cancel handle");
        assert!(Transport::is_alive(&t));
        cancel.cancel();
        assert!(
            !Transport::is_alive(&t),
            "is_alive() must read false once the cancel handle has fired"
        );
    }

    /// What the kernel reports for `SO_RCVBUF` / `SO_SNDBUF` after the same
    /// request on a fresh control socket. Kernels differ (Linux doubles the
    /// request and clamps to `net.core.rmem_max`; macOS and Windows store it
    /// as given), so the transport's socket is compared against a control
    /// socket rather than against a fixed number.
    fn control_buffer_sizes(rcv: usize, snd: usize) -> (usize, usize, usize, usize) {
        let c = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sr = socket2::SockRef::from(&c);
        let (rcv_default, snd_default) = (
            sr.recv_buffer_size().unwrap(),
            sr.send_buffer_size().unwrap(),
        );
        sr.set_recv_buffer_size(rcv).unwrap();
        sr.set_send_buffer_size(snd).unwrap();
        (
            rcv_default,
            sr.recv_buffer_size().unwrap(),
            snd_default,
            sr.send_buffer_size().unwrap(),
        )
    }

    #[test]
    fn rcvbuf_query_sizes_the_rtp_receive_socket() {
        let (rcv_default, rcv_expected, _, _) = control_buffer_sizes(1 << 20, 1 << 20);
        assert_ne!(
            rcv_default, rcv_expected,
            "a 1 MiB request must be distinguishable from the default on this host"
        );
        let url = RtpUrl::parse("rtp://127.0.0.1:0?rcvbuf=1M").unwrap();
        let t = RtpRecvTransport::listen_with(&url).unwrap();
        let Some(Source::Udp(sock)) = t.source.as_ref() else {
            panic!("listen builds a UDP source");
        };
        let got = socket2::SockRef::from(sock).recv_buffer_size().unwrap();
        assert_eq!(got, rcv_expected, "SO_RCVBUF not applied from ?rcvbuf=");
    }

    #[test]
    fn sndbuf_query_sizes_the_rtp_send_socket() {
        let (_, _, snd_default, snd_expected) = control_buffer_sizes(1 << 20, 1 << 20);
        assert_ne!(
            snd_default, snd_expected,
            "a 1 MiB request must be distinguishable from the default on this host"
        );
        let url = RtpUrl::parse("rtp://127.0.0.1:5004?sndbuf=1M").unwrap();
        let t = RtpTransport::connect_with(&url).unwrap();
        let sock = t.socket.as_ref().expect("live transport holds its socket");
        let got = socket2::SockRef::from(sock).send_buffer_size().unwrap();
        assert_eq!(got, snd_expected, "SO_SNDBUF not applied from ?sndbuf=");
    }
}
