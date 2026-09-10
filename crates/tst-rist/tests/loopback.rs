//! End-to-end RIST loopback tests.
//!
//! Verifies that bytes pushed into a `RistTransport` on 127.0.0.1 reach a
//! `RistRecvTransport` listening on the same port. Covers Simple Profile
//! (unencrypted) and Main Profile with AES-256 (mbedtls feature only).
//!
//! librist's handshake is slower than UDP — Simple is ~500ms, Main+AES is
//! ~800-1500ms — so we sleep before the first send and use a retry loop
//! on the recv side that tolerates Backpressure timeouts.
//!
//! Runs on Windows too (un-gated 2026-07-26): this file was gated off
//! windows-msvc from 2026-05-29 because vendored librist ≤ 0.2.16 hung ~14s+
//! in `rist_destroy` on Windows teardown and delivered no data. Both bugs are
//! fixed upstream in librist 0.2.18 (CI diagnostic run 30136835805: teardown
//! 10–31 ms, full delivery, both Simple and Main+AES-256 profiles).

use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tst_core::transport::{RecvTransport, Transport, TransportError};
use tst_rist::{
    EncryptionKey, RistErrorKind, RistProfile, RistRecvTransportBuilder, RistTransportBuilder,
};

/// Serializes RIST loopback tests within this test binary. (Cross-binary
/// serialization isn't needed because each test in this file uses a distinct
/// hardcoded port; see PORT_SIMPLE / PORT_AES below.)
static SERIAL: Mutex<()> = Mutex::new(());

/// Hardcoded distinct ports per test. Avoids the ephemeral-bind + librist-
/// rebind race that broke find_free_udp_port-based discovery: cargo runs
/// integration-test binaries in parallel, each with its own static Mutex,
/// so different files cannot synchronize through process-shared state.
///
/// **Simple Profile requires an EVEN port** — librist uses port + port+1 for
/// RTP + RTCP and rist_peer_create returns -1 with "port must be even" if
/// the bind port is odd. See vendor/librist/src/rist.c:866.
/// Main Profile multiplexes RTCP into the same socket so any port works.
const PORT_SIMPLE: u16 = 33010;
const PORT_AES: u16 = 33013;
const PORT_OVERSIZE: u16 = 33016;
const PORT_V6: u16 = 33022;
/// Never actually bound — the guard under test returns `Err` before librist
/// touches a socket. Must be EVEN: an odd port trips librist's own "port
/// must be even" check first (Simple profile) and returns gracefully
/// *without* reaching the buggy RTCP-peer path this guard exists for, which
/// would make this test pass for the wrong reason both with and without
/// the guard.
const PORT_V6_SIMPLE_REFUSED: u16 = 33024;

/// 188 bytes of arbitrary payload — one MPEG-TS packet's worth.
fn synthetic_ts_packet(seq_byte: u8) -> [u8; 188] {
    let mut out = [seq_byte; 188];
    out[0] = 0x47; // TS sync byte
    out
}

/// Read up to `n_packets` from `recv` within `overall_timeout`, retrying on
/// Backpressure (librist's poll timeout). Returns the collected payloads.
fn drain_n(
    mut recv: tst_rist::RistRecvTransport,
    n_packets: usize,
    overall_timeout: Duration,
) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(n_packets);
    let mut buf = vec![0u8; recv.max_payload() + 64];
    let start = Instant::now();
    while out.len() < n_packets {
        if start.elapsed() >= overall_timeout {
            break;
        }
        match recv.recv_bytes(&mut buf) {
            Ok(n) => out.push(buf[..n].to_vec()),
            Err(TransportError::Backpressure { .. }) => continue,
            Err(e) => {
                eprintln!("recv error: {e:?}");
                break;
            }
        }
    }
    out
}

#[test]
fn simple_profile_unicast_loopback_round_trip() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let port = PORT_SIMPLE;
    let bind_url = format!("rist://@127.0.0.1:{port}");
    let connect_url = format!("rist://127.0.0.1:{port}");

    let recv = RistRecvTransportBuilder::new(&bind_url)
        .unwrap()
        .profile(RistProfile::Simple)
        .listen()
        .expect("listen");

    let (tx_payloads, rx_payloads) = mpsc::channel::<Vec<Vec<u8>>>();
    let _recv_thread = thread::spawn(move || {
        // librist Simple handshake ~500ms; give 8s overall for safety
        // on slow CI runners.
        let collected = drain_n(recv, 5, Duration::from_secs(8));
        let _ = tx_payloads.send(collected);
    });

    // Connect after the listener thread is running. Sleep gives the
    // recv-side a head-start to fully bind before we initiate.
    thread::sleep(Duration::from_millis(200));
    let mut send = RistTransportBuilder::new(&connect_url)
        .unwrap()
        .profile(RistProfile::Simple)
        .connect()
        .expect("connect");

    // Sleep again to let the librist handshake settle.
    thread::sleep(Duration::from_millis(600));

    let pkts: Vec<[u8; 188]> = (1..=5).map(|i| synthetic_ts_packet(i as u8)).collect();
    for p in &pkts {
        send.send_bytes(p).expect("send");
    }

    let collected = rx_payloads
        .recv_timeout(Duration::from_secs(10))
        .expect("recv thread didn't return in time");

    // librist's first few packets sometimes go missing during the
    // handshake settling phase. Accept any 3+ of the 5 reaching us — the
    // test is verifying the data-plane works, not that librist is
    // lossless across the first packet boundary.
    assert!(
        collected.len() >= 3,
        "expected at least 3 of 5 packets; got {}",
        collected.len()
    );

    // Each collected payload should be one of the originals (any order).
    for got in &collected {
        let matched = pkts.iter().any(|orig| got.as_slice() == orig.as_slice());
        assert!(
            matched,
            "received payload did not match any sent packet: {:?}",
            &got[..8.min(got.len())]
        );
    }
}

#[cfg(feature = "mbedtls")]
#[test]
fn main_profile_aes256_loopback_round_trip() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let port = PORT_AES;
    let bind_url = format!("rist://@127.0.0.1:{port}");
    let connect_url = format!("rist://127.0.0.1:{port}");
    let psk = "loopback-test-secret-keep-private";

    let recv = RistRecvTransportBuilder::new(&bind_url)
        .unwrap()
        .profile(RistProfile::Main)
        .encryption(EncryptionKey::aes256(psk))
        .listen()
        .expect("listen");

    let (tx_payloads, rx_payloads) = mpsc::channel::<Vec<Vec<u8>>>();
    let _recv_thread = thread::spawn(move || {
        // AES handshake takes longer; give 12s overall.
        let collected = drain_n(recv, 5, Duration::from_secs(12));
        let _ = tx_payloads.send(collected);
    });

    thread::sleep(Duration::from_millis(300));
    let mut send = RistTransportBuilder::new(&connect_url)
        .unwrap()
        .profile(RistProfile::Main)
        .encryption(EncryptionKey::aes256(psk))
        .connect()
        .expect("connect");

    // AES handshake is the slow one — 800ms-1.2s typical on Linux loopback.
    thread::sleep(Duration::from_millis(1200));

    let pkts: Vec<[u8; 188]> = (1..=5).map(|i| synthetic_ts_packet(i as u8)).collect();
    for p in &pkts {
        send.send_bytes(p).expect("send");
    }

    let collected = rx_payloads
        .recv_timeout(Duration::from_secs(15))
        .expect("recv thread didn't return in time");
    assert!(
        collected.len() >= 3,
        "expected at least 3 of 5 packets; got {}",
        collected.len()
    );
    for got in &collected {
        let matched = pkts.iter().any(|orig| got.as_slice() == orig.as_slice());
        assert!(matched, "decrypted payload mismatch");
    }
}

/// A foreign RIST sender may bundle more bytes per block than our
/// configured pkt_size — RIST rides RTP over UDP, so anything up to the
/// 16-bit datagram ceiling is legal on the wire. Before the
/// recv-ceiling fix, max_payload() returned pkt_size (1316 default), so
/// a buffer sized from it sent every oversize block into the
/// DropOversize arm: silent stream loss (packets_dropped ticked, data
/// gone). The recv-side ceiling must accept any legal block.
#[test]
fn oversize_foreign_block_delivered_not_dropped() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let port = PORT_OVERSIZE;
    let bind_url = format!("rist://@127.0.0.1:{port}");
    let connect_url = format!("rist://127.0.0.1:{port}");

    let recv = RistRecvTransportBuilder::new(&bind_url)
        .unwrap()
        .profile(RistProfile::Simple)
        .listen()
        .expect("listen");
    assert_eq!(
        RecvTransport::max_payload(&recv),
        65535,
        "recv-side ceiling must be the UDP datagram maximum"
    );

    let (tx_payloads, rx_payloads) = mpsc::channel::<Vec<Vec<u8>>>();
    let _recv_thread = thread::spawn(move || {
        // drain_n sizes its buffer from recv.max_payload() — the same
        // idiom the pipeline shells use, so this pins shell behavior.
        let collected = drain_n(recv, 1, Duration::from_secs(8));
        let _ = tx_payloads.send(collected);
    });

    thread::sleep(Duration::from_millis(200));
    let mut send = RistTransportBuilder::new(&connect_url)
        .unwrap()
        .profile(RistProfile::Simple)
        .pkt_size(1880) // the "foreign" sender's bigger bundle budget
        .connect()
        .expect("connect");
    thread::sleep(Duration::from_millis(600));

    // One 10×188 = 1880-byte block — bigger than the receiver's 1316
    // default pkt_size. Send several times: librist may lose the first
    // packets during handshake settling (same tolerance as the sibling
    // round-trip test).
    let mut block = Vec::with_capacity(1880);
    for i in 1..=10u8 {
        block.extend_from_slice(&synthetic_ts_packet(i));
    }
    for _ in 0..5 {
        send.send_bytes(&block).expect("send oversize block");
        thread::sleep(Duration::from_millis(100));
    }

    let collected = rx_payloads
        .recv_timeout(Duration::from_secs(10))
        .expect("recv thread didn't return in time");
    assert!(
        !collected.is_empty(),
        "oversize foreign block must be delivered, not silently dropped"
    );
    assert_eq!(
        collected[0], block,
        "delivered block must be byte-identical"
    );
}

/// IPv6 round-trip through the CORR-09 fix: `native_endpoint` renders the
/// peer/bind URL via `SocketAddr`'s `Display`, which brackets IPv6, so
/// librist's `udpsocket_parse_url` sees `[::1]:port` instead of splitting a
/// bare `::1:port` at the first colon into host "" + port 0.
///
/// Uses `RistProfile::Main`, NOT `Simple` (unlike the sibling v4 test this
/// one mirrors) — Simple Profile's receiver-side RTCP-peer creation has a
/// separate, pre-existing librist bug that SIGSEGVs when binding an IPv6
/// address (`rist.c`'s `rist_receiver_peer_create` dereferences the RTCP
/// peer's `peer_ssrc` field before its own null check, and the RTCP peer's
/// re-derived bind URL fails to come up as IPv6). That bug is orthogonal to
/// CORR-09 (it reproduces with a hand-built bracketed URL too, and the
/// sender side's `rist_sender_peer_create` has no equivalent bug — it checks
/// for null first) and out of scope for this fix; see the task report for
/// the full repro. Main Profile multiplexes RTCP into the data socket and
/// never takes that path, so it exercises the bracket fix without tripping
/// the unrelated crash.
#[test]
fn ipv6_loopback_round_trip() {
    if !ipv6_loopback_available() {
        eprintln!("skipping: IPv6 loopback unavailable on this host");
        return;
    }

    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let port = PORT_V6;
    let bind_url = format!("rist://@[::1]:{port}");
    let connect_url = format!("rist://[::1]:{port}");

    let recv = RistRecvTransportBuilder::new(&bind_url)
        .unwrap()
        .profile(RistProfile::Main)
        .listen()
        .expect("listen");

    let (tx_payloads, rx_payloads) = mpsc::channel::<Vec<Vec<u8>>>();
    let _recv_thread = thread::spawn(move || {
        // librist's unencrypted Main-profile handshake is comparable to
        // Simple's ~500ms; give 8s overall for safety on slow CI runners.
        let collected = drain_n(recv, 5, Duration::from_secs(8));
        let _ = tx_payloads.send(collected);
    });

    // Connect after the listener thread is running. Sleep gives the
    // recv-side a head-start to fully bind before we initiate.
    thread::sleep(Duration::from_millis(200));
    let mut send = RistTransportBuilder::new(&connect_url)
        .unwrap()
        .profile(RistProfile::Main)
        .connect()
        .expect("connect");

    // Sleep again to let the librist handshake settle.
    thread::sleep(Duration::from_millis(600));

    let pkts: Vec<[u8; 188]> = (1..=5).map(|i| synthetic_ts_packet(i as u8)).collect();
    for p in &pkts {
        send.send_bytes(p).expect("send");
    }

    let collected = rx_payloads
        .recv_timeout(Duration::from_secs(10))
        .expect("recv thread didn't return in time");

    // librist's first few packets sometimes go missing during the
    // handshake settling phase. Accept any 3+ of the 5 reaching us — the
    // test is verifying the data-plane works, not that librist is
    // lossless across the first packet boundary.
    assert!(
        collected.len() >= 3,
        "expected at least 3 of 5 packets; got {}",
        collected.len()
    );

    // Each collected payload should be one of the originals (any order).
    for got in &collected {
        let matched = pkts.iter().any(|orig| got.as_slice() == orig.as_slice());
        assert!(
            matched,
            "received payload did not match any sent packet: {:?}",
            &got[..8.min(got.len())]
        );
    }
}

/// Vendored librist 0.2.20's Simple-profile receiver-side RTCP-peer creation
/// dereferences the new peer before its own null check (`rist.c`'s
/// `rist_receiver_peer_create`), and that null case is reachable for an IPv6
/// bind — SIGSEGV. `listen_with_config` must refuse this combination BEFORE
/// any librist context or peer creation (never reaching `rist_receiver_create`),
/// not let the process crash. This must NOT be run without the guard: prior to the fix,
/// this exact profile+URL combination segfaults the whole test process (see
/// the task report for the gdb-verified repro), so there is no "assert it
/// panics" fallback here — the guard is the only safe way to exercise this.
#[test]
fn ipv6_simple_profile_listen_is_refused() {
    if !ipv6_loopback_available() {
        eprintln!("skipping: IPv6 loopback unavailable on this host");
        return;
    }

    let bind_url = format!("rist://@[::1]:{PORT_V6_SIMPLE_REFUSED}");
    let result = RistRecvTransportBuilder::new(&bind_url)
        .unwrap()
        .profile(RistProfile::Simple)
        .listen();
    match result {
        Err(err) => assert_eq!(
            err.kind(),
            RistErrorKind::InvalidConfig,
            "got {err:?}, expected InvalidConfig"
        ),
        Ok(_) => panic!("Simple-profile IPv6 receiver bind must be refused, not attempted"),
    }
}

/// Some CI environments disable IPv6 loopback (`ip6_disabled`, unprivileged
/// containers without a v6 stack). Probe by trying to bind a UDP socket; if
/// that fails, skip the test. Same probe as
/// `crates/tst-srt/tests/loopback/ipv6_loopback.rs`.
fn ipv6_loopback_available() -> bool {
    std::net::UdpSocket::bind("[::1]:0").is_ok()
}
