//! End-to-end RTP loopback over 127.0.0.1.
//!
//! Wires a `tst-rtp` send transport to a `tst-rtp` recv transport on
//! the same host, sends a small synthetic TS payload, and verifies the
//! recv side observes the original bytes after stripping the RTP
//! header.
//!
//! Why two threads: `RtpRecvTransport::recv_bytes` blocks until a
//! datagram arrives or the cancel handle fires; we send from the main
//! thread and recv from a child to avoid deadlocking on a single-thread
//! recv-before-send ordering.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tst_core::transport::{RecvTransport, Transport};
use tst_rtp::{RtpRecvTransport, RtpTransport};

/// 188 bytes of arbitrary payload — one MPEG-TS packet's worth.
/// Realistic enough to exercise the wire path without pulling in
/// `tst-pipeline` for this integration test.
fn synthetic_ts_packet(seq_byte: u8) -> [u8; 188] {
    let mut out = [seq_byte; 188];
    out[0] = 0x47; // TS sync byte
    out
}

/// Find a free RTP port base on `127.0.0.1` where both `base` and `base + 1`
/// (the RTCP companion auto-bound on `port + 1` per RFC 3550 §11) are
/// currently bindable, then release them so the transport under test can bind
/// them itself.
///
/// This replaces the old hard-coded 55004/55008/… ports, which intermittently
/// collided with the Windows dynamic-exclusion / reserved UDP ranges (shift
/// per runner boot) and failed with `WSAEACCES (os error 10013)`. An
/// OS-assigned ephemeral port is never drawn from a reserved range, so this
/// removes that flake class. We can't hand the pre-bound socket to the
/// transport (`RtpRecvTransport::listen` binds by URL and exposes no
/// local-addr accessor), so we discover-then-release; the serialized `network`
/// nextest test-group means no sibling test races for the freed port.
fn free_rtp_port_base() -> u16 {
    use std::net::UdpSocket;
    // Bounded so a host under unusual port pressure fails deterministically with
    // a clear message instead of spinning until nextest's timeout kills it.
    for _ in 0..1000 {
        let s = UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral udp");
        let base = s.local_addr().unwrap().port();
        if base < u16::MAX {
            if let Ok(companion) = UdpSocket::bind(("127.0.0.1", base + 1)) {
                drop(companion);
                drop(s);
                return base;
            }
        }
        drop(s); // base + 1 was taken (or base == u16::MAX); retry.
        std::thread::yield_now();
    }
    panic!(
        "free_rtp_port_base: no free base/base+1 UDP port pair on 127.0.0.1 \
         after 1000 attempts (host under port pressure?)"
    );
}

#[test]
fn unicast_loopback_round_trip() {
    // 1. Bind the receiver first so the sender's send-buffer flushes
    //    immediately. An ephemeral base (with base+1 free for the RTCP
    //    companion auto-bound per RFC 3550 §11) avoids
    //    the Windows reserved-range WSAEACCES flake of fixed ports.
    let base = free_rtp_port_base();
    let url = format!("rtp://127.0.0.1:{base}");
    let mut recv = RtpRecvTransport::listen(&url).unwrap();

    // 2. Spawn a recv thread that reads N packets and pushes them to a
    //    channel.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let _recv_thread = thread::spawn(move || {
        let mut buf = vec![0u8; recv.max_payload() + 64];
        for _ in 0..3 {
            match recv.recv_bytes(&mut buf) {
                Ok(n) => tx.send(buf[..n].to_vec()).unwrap(),
                Err(e) => {
                    eprintln!("recv error: {e:?}");
                    break;
                }
            }
        }
    });

    // 3. Send 3 packets. Tiny sleep lets the OS schedule the recv side
    //    so we don't lose to startup race.
    thread::sleep(Duration::from_millis(20));
    let mut send = RtpTransport::connect(&url).unwrap();
    // (RTCP companion: send-side uses an ephemeral local port; recv-
    // side bound base+1 above.)
    let pkts = [
        synthetic_ts_packet(0x01),
        synthetic_ts_packet(0x02),
        synthetic_ts_packet(0x03),
    ];
    for p in &pkts {
        send.send_bytes(p).unwrap();
    }

    // 4. Collect 3 recvs, assert payloads match.
    for expected in pkts.iter() {
        let got = rx.recv_timeout(Duration::from_secs(5)).expect("no recv");
        assert_eq!(got.as_slice(), expected.as_slice());
    }
}

#[test]
fn cancel_wakes_blocked_recv() {
    let base = free_rtp_port_base();
    let recv = RtpRecvTransport::listen(&format!("rtp://127.0.0.1:{base}")).unwrap();
    let cancel = recv.cancel_handle().expect("cancel handle");
    let handle = thread::spawn(move || {
        let mut recv = recv;
        let mut buf = vec![0u8; 4096];
        recv.recv_bytes(&mut buf)
    });
    thread::sleep(Duration::from_millis(50));
    cancel.cancel();
    let result = handle.join().unwrap();
    // ExplicitClose is the expected outcome.
    use tst_core::transport::TransportError;
    assert!(matches!(result, Err(TransportError::ExplicitClose)));
}

/// SSRC and seq advance per packet on the send side. Verified by parsing
/// the inbound RTP header from the recv side (which we don't do above —
/// the recv side strips the header before returning). To verify
/// seq-increment we'd need a custom socket bypass, so this test instead
/// confirms the SocketStats counter increments.
#[test]
fn send_stats_increment_per_packet() {
    let base = free_rtp_port_base();
    let url = format!("rtp://127.0.0.1:{base}");
    let _recv = RtpRecvTransport::listen(&url).unwrap();
    let mut send = RtpTransport::connect(&url).unwrap();
    let pkt = synthetic_ts_packet(0xFF);
    send.send_bytes(&pkt).unwrap();
    send.send_bytes(&pkt).unwrap();
    send.send_bytes(&pkt).unwrap();
    let stats = send.socket_stats().expect("alive transport");
    assert_eq!(stats.packets_sent, 3);
    assert_eq!(stats.bytes_sent, 3 * (pkt.len() + 12) as u64);
    drop(send);
    let _ = Arc::<()>::default(); // silence unused-arc lint
}

// --- RTCP socket pair ---
//
// Why these tests live in the existing loopback file: they probe the
// same `listen()` / builder entry points the tests above cover. Putting
// them here keeps the per-port test grouping in one spot and avoids a
// separate test binary for three tiny tests.

#[test]
fn rtcp_socket_pair_off_by_default() {
    let base = free_rtp_port_base();
    let r = RtpRecvTransport::listen(&format!("rtp://127.0.0.1:{base}")).unwrap();
    // The experimental SR/RR reporter (and its companion RTCP socket on
    // port+1) is OFF by default. So port+1 must stay free — we can
    // bind it ourselves. (A caller who explicitly opts in via
    // `.rtcp(true)` gets the socket — see `rtcp_socket_pair_opens_when_opted_in`.)
    let probe = std::net::UdpSocket::bind(("127.0.0.1", base + 1));
    assert!(
        probe.is_ok(),
        "RTCP port {} should be free when the reporter is off by default",
        base + 1
    );
    drop(r);
}

#[test]
fn rtcp_opt_out_skips_second_socket() {
    let base = free_rtp_port_base();
    let r = tst_rtp::RtpRecvSocketBuilder::from_url(&format!("rtp://127.0.0.1:{base}"))
        .unwrap()
        .rtcp(false)
        .build()
        .unwrap();
    // With opt-out, port+1 (base+1) stays free.
    let probe = std::net::UdpSocket::bind(("127.0.0.1", base + 1));
    assert!(
        probe.is_ok(),
        "RTCP port {} should be free when builder opts out of RTCP",
        base + 1
    );
    drop(r);
}

#[test]
fn rtcp_socket_pair_opens_when_opted_in() {
    // The experimental reporter is off by default; explicit opt-in
    // must still bind the companion RTCP socket on port+1.
    let base = free_rtp_port_base();
    let r = tst_rtp::RtpRecvSocketBuilder::from_url(&format!("rtp://127.0.0.1:{base}"))
        .unwrap()
        .rtcp(true)
        .build()
        .unwrap();
    // With opt-in, port+1 is held by the transport — binding it ourselves
    // must fail specifically with AddrInUse (any other error would pass
    // spuriously without proving RTCP is bound).
    match std::net::UdpSocket::bind(("127.0.0.1", base + 1)) {
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {}
        other => panic!(
            "RTCP port {} should be held by the opted-in RtpRecvTransport (AddrInUse); got {other:?}",
            base + 1
        ),
    }
    drop(r);
}

#[test]
fn rtcp_stats_accessor_exists() {
    let base = free_rtp_port_base();
    let r = RtpRecvTransport::listen(&format!("rtp://127.0.0.1:{base}")).unwrap();
    let stats = r.rtcp_stats();
    // Counter stays at zero — the experimental RR reporter is off by
    // default, so no reporter thread is spawned and no RR is sent.
    assert_eq!(stats.rr_packets_sent, 0);
    drop(r);
}

// --- UDP-path MP2T shape validation (loopback) ---
//
// Sends a crafted raw UDP datagram carrying a valid RTP header (V=2, PT=33)
// but a non-188-aligned payload to confirm the shape guard fires on the real
// UDP recv path, not just the mpsc unit-test seam.

/// Build a raw RTP datagram: 12-byte header (V=2, P=0, X=0, CC=0, M=0,
/// PT=33, seq=1, ts=0, ssrc=0xDEAD_BEEF) followed by `payload`.
fn make_rtp_datagram(payload: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(12 + payload.len());
    pkt.push(0x80); // V=2, P=0, X=0, CC=0
    pkt.push(33); // M=0, PT=33 (MP2T)
    pkt.extend_from_slice(&1u16.to_be_bytes()); // seq
    pkt.extend_from_slice(&0u32.to_be_bytes()); // timestamp
    pkt.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes()); // ssrc
    pkt.extend_from_slice(payload);
    pkt
}

/// A datagram with a valid RTP header (PT=33) but a 100-byte
/// non-0x47 payload must be dropped. The recv transport returns only the
/// subsequent valid packet and ticks malformed_packets=1.
#[test]
fn udp_path_malformed_mp2t_payload_dropped_and_counted() {
    use std::net::UdpSocket;
    use tst_core::transport::RecvTransport;

    let base = free_rtp_port_base();
    let url = format!("rtp://127.0.0.1:{base}");
    let mut recv = RtpRecvTransport::listen(&url).unwrap();
    let cancel = recv.cancel_handle().expect("cancel handle");

    // Recv thread: wait for exactly one valid payload.
    let (result_tx, result_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let recv_thread = thread::spawn(move || {
        let mut buf = vec![0u8; 4096];
        // Block until one packet arrives (or an error). The shape guard
        // drops the malformed datagram internally and continues the inner
        // loop; recv_bytes only returns once a valid packet passes.
        if let Ok(n) = recv.recv_bytes(&mut buf) {
            result_tx.send(buf[..n].to_vec()).ok();
        }
        recv // return transport so caller can inspect stats
    });

    // Allow the recv thread to enter the socket.
    thread::sleep(Duration::from_millis(20));

    // Send 1: crafted raw RTP with 100-byte non-0x47 payload — should be
    // dropped by the shape guard.
    let malformed_payload = vec![0xAAu8; 100];
    let malformed_dgram = make_rtp_datagram(&malformed_payload);
    let raw = UdpSocket::bind("127.0.0.1:0").unwrap();
    raw.send_to(&malformed_dgram, format!("127.0.0.1:{base}"))
        .expect("send malformed datagram");

    // Send 2: valid 188-byte TS packet via the real RtpTransport (which
    // always emits PT=33 + correct 188-byte aligned payloads).
    thread::sleep(Duration::from_millis(5));
    let mut send = RtpTransport::connect(&url).unwrap();
    let valid_pkt = synthetic_ts_packet(0x42);
    send.send_bytes(&valid_pkt).unwrap();

    // Wait for the recv thread to surface the valid packet.
    let got = result_rx.recv_timeout(Duration::from_secs(5));

    // Cancel the recv transport before joining so the thread is reaped on
    // every code path — including a timeout where recv_bytes is still blocked.
    cancel.cancel();
    let recv_transport = recv_thread.join().expect("recv thread panicked");

    let got = got.expect("recv timed out — valid packet not delivered");
    assert_eq!(got.as_slice(), valid_pkt.as_slice(), "payload mismatch");
    assert_eq!(
        recv_transport.rtp_stats().malformed_packets,
        1,
        "malformed_packets must be 1 after one shape-invalid UDP datagram"
    );
}

/// Foreign senders (gst/ffmpeg defaults) pack 7×188 = 1316-byte MP2T
/// payloads — a 1328-byte datagram with the RTP header. Before the
/// recv-ceiling fix, the pipeline shells sized their receive buffer from
/// max_payload() = pkt_size − 12 = 1304, so this conformant bundle
/// surfaced as Broken("recv buf too small: 1304 < 1316") (v0.2.0
/// silently truncated it instead). Regression: the whole bundle must
/// flow through a `Receiver` shell intact, and the recv-side ceiling
/// must be the deliverable ceiling, not the send budget.
#[test]
fn full_mtu_foreign_bundle_through_receiver_shell() {
    use tst_pipeline::{Receiver, ReceiverConfig};

    let base = free_rtp_port_base();
    let url = format!("rtp://127.0.0.1:{base}");
    let recv = RtpRecvTransport::listen(&url).unwrap();
    assert_eq!(
        RecvTransport::max_payload(&recv),
        65535 - 12,
        "recv-side ceiling must be RECV_SCRATCH_LEN - RTP_HEADER_LEN, \
         not the pkt_size send budget"
    );

    // The Receiver shell sizes its internal buffer from max_payload()
    // at construction — that sizing is exactly what this test pins.
    let (tx, rx) = std::sync::mpsc::channel::<[u8; 188]>();
    let recv_thread = thread::spawn(move || {
        let mut shell = Receiver::new(recv, ReceiverConfig::default());
        for _ in 0..7 {
            match shell.next_packet() {
                Ok(p) => {
                    if tx.send(p).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("shell error: {e:?}");
                    break;
                }
            }
        }
    });

    // Hand-craft the foreign datagram: 12-byte RTP header (V=2, PT=33)
    // + 7 × 188-byte TS packets. Our own RtpTransport cannot send this
    // (its send budget caps below full-MTU) — which is exactly why the
    // foreign-sender path needs a raw std socket.
    thread::sleep(Duration::from_millis(20));
    let mut datagram = vec![
        0x80, // V=2, P=0, X=0, CC=0
        33,   // M=0, PT=33 (MP2T)
        0x12, 0x34, // sequence number
        0x00, 0x00, 0x00, 0x01, // timestamp
        0xde, 0xad, 0xbe, 0xef, // SSRC
    ];
    let pkts: Vec<[u8; 188]> = (1..=7).map(|i| synthetic_ts_packet(i as u8)).collect();
    for p in &pkts {
        datagram.extend_from_slice(p);
    }
    assert_eq!(datagram.len(), 1328);
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.send_to(&datagram, ("127.0.0.1", base)).unwrap();

    for expected in pkts.iter() {
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("full-MTU bundle packet not delivered through the shell");
        assert_eq!(&got[..], &expected[..]);
    }
    recv_thread.join().unwrap();
}

/// Recv-side URL rejection: `?pkt_size=` is a send-side knob since the
/// recv-ceiling change; receive entry points reject it with a teaching
/// error rather than silently ignoring it.
#[test]
fn recv_url_pkt_size_rejected_with_teaching_error() {
    let result = tst_rtp::RtpRecvTransport::listen("rtp://127.0.0.1:0?pkt_size=1316");
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("recv URL with ?pkt_size= must be rejected"),
    };
    assert!(
        matches!(
            err,
            tst_rtp::ConnectError::Url(tst_rtp::RtpUrlError::RecvPktSize)
        ),
        "wrong error variant: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("send-side knob"),
        "teaching text missing: {msg}"
    );
}

/// Send-side URL `?pkt_size=` still works and still drives the send
/// budget (max_payload = pkt_size - RTP_HEADER_LEN), i.e. the
/// Option-ification did not change send behavior.
#[test]
fn send_url_pkt_size_still_drives_send_budget() {
    use tst_core::transport::Transport;
    // RTP header is 12 bytes; max_payload() returns pkt_size - 12.
    let b = tst_rtp::RtpSocketBuilder::from_url("rtp://127.0.0.1:5004?pkt_size=376")
        .expect("send URL with ?pkt_size= must still parse");
    let t = b.build().expect("unicast connect");
    assert_eq!(
        t.max_payload(),
        376 - 12,
        "explicit pkt_size drives send budget"
    );

    let b = tst_rtp::RtpSocketBuilder::from_url("rtp://127.0.0.1:5004").expect("bare send URL");
    let t = b.build().expect("unicast connect");
    assert_eq!(
        t.max_payload(),
        1316 - 12,
        "absent pkt_size resolves the 1316 default"
    );
}
