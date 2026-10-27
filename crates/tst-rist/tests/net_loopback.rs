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
//! Runs on Windows too: librist ≤ 0.2.16 hung ~14s+ in `rist_destroy` on
//! Windows teardown and delivered no data; both bugs are fixed upstream in
//! librist 0.2.18 (teardown 10–31 ms, full delivery, both Simple and
//! Main+AES-256 profiles).

use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tst_core::transport::{RecvTransport, Transport, TransportError};
use tst_rist::{
    EncryptionKey, RistError, RistProfile, RistRecvTransportBuilder, RistTransportBuilder,
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
/// Sender-only (no receiver bound). EVEN, same rule as PORT_SIMPLE.
const PORT_BURST: u16 = 33026;
/// Even data port whose RTCP companion (port + 1) the child holds itself.
/// Used only by the collision test, which does not run on Windows.
#[cfg(not(windows))]
const PORT_SIMPLE_RTCP_HELD: u16 = 33028;
#[cfg(not(windows))]
const RTCP_CHILD_ENV: &str = "TST_RIST_RTCP_COLLISION_CHILD";
#[cfg(not(windows))]
const RTCP_CHILD_OK: &str = "RTCP-COLLISION-REFUSED-THEN-BOUND";

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
/// 16-bit datagram ceiling is legal on the wire. If max_payload()
/// returned pkt_size (1316 default), a buffer sized from it would send
/// every oversize block into the DropOversize arm: silent stream loss
/// (packets_dropped ticked, data gone). The recv-side ceiling must accept
/// any legal block.
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

/// IPv6 round-trip: `native_endpoint` renders the
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
/// the bracketing (it reproduces with a hand-built bracketed URL too, and the
/// sender side's `rist_sender_peer_create` has no equivalent bug — it checks
/// for null first). Main Profile multiplexes RTCP into the data socket and
/// never takes that path, so it exercises the bracketing without tripping
/// the unrelated crash.
///
/// Not run on Windows: vendored librist 0.2.20 parses the bracketed bind URL
/// fine but `rist_peer_create` then refuses the IPv6 receiver bind there,
/// returning `PeerCreateFailed` immediately (the OS-level `[::1]` probe below
/// still succeeds, so this is librist's socket layer, not host capability).
/// IPv6 senders and all IPv4 paths are unaffected. See the RIST IPv6
/// receiver-bind entry in `docs/project/deferred-features.md`.
#[test]
fn ipv6_loopback_round_trip() {
    if cfg!(windows) {
        eprintln!("skipping: librist 0.2.20 cannot bind an IPv6 receiver peer on Windows");
        return;
    }
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

/// librist's Simple-profile receiver (`rist.c`'s `rist_receiver_peer_create`)
/// rebuilds the RTCP peer's bind address without the IPv6 brackets, so for an
/// IPv6 bind that peer never comes up. The vendored build null-checks the
/// failed peer, but a system librist 0.2.20 dereferences it and SIGSEGVs the
/// process. `listen_with_config` must refuse this combination BEFORE any
/// librist context or peer creation (never reaching `rist_receiver_create`),
/// so the outcome is the same `InvalidConfig` whichever librist is linked.
///
/// No `ipv6_loopback_available()` probe: the guard refuses before any socket
/// bind or librist context creation, and `[::1]` is parsed as a literal, so
/// this runs identically on hosts with IPv6 loopback disabled.
#[test]
fn ipv6_simple_profile_listen_is_refused() {
    let bind_url = format!("rist://@[::1]:{PORT_V6_SIMPLE_REFUSED}");
    let result = RistRecvTransportBuilder::new(&bind_url)
        .unwrap()
        .profile(RistProfile::Simple)
        .listen();
    match result {
        Err(err) => assert!(
            matches!(err, RistError::InvalidConfig(_)),
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

/// Live guard: a burst with no receiver must never latch the sender
/// dead. Every result is either `Ok` or the queue-full `Backpressure {
/// errno_code: Some(-2) }`; `is_alive()` stays true throughout. This does
/// NOT assert that `-2` occurs — librist's 524,288-entry sender queue is
/// drained by its protocol thread whether or not a peer answers, so a
/// 20,000-packet burst normally never fills it; the `-2` mapping itself is
/// pinned by `transport::tests::classify_write_rc_maps_librist_namespace`.
/// If queue-full were latched as `Broken`, this test would fail only when
/// the queue does fill (then `Broken { errno_code: Some(-2) }` trips the
/// panic below).
#[test]
fn send_burst_without_receiver_never_latches_broken() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let connect_url = format!("rist://127.0.0.1:{PORT_BURST}");
    let mut send = RistTransportBuilder::new(&connect_url)
        .unwrap()
        .profile(RistProfile::Simple)
        .connect()
        .expect("connect");

    let block = [0x47u8; 1316];
    let mut queue_full = 0usize;
    for i in 0..20_000 {
        match send.send_bytes(&block) {
            Ok(()) => {}
            Err(TransportError::Backpressure {
                errno_code: Some(-2),
                ..
            }) => queue_full += 1,
            Err(e) => panic!("send #{i}: expected Ok or queue-full Backpressure, got {e:?}"),
        }
        assert!(
            send.is_alive(),
            "send #{i}: a queue-full drop must not latch the sender dead"
        );
    }
    eprintln!("queue-full drops during burst: {queue_full}");
    send.close();
}

/// librist's Simple profile opens a second, RTCP peer on port + 1
/// (VSF TR-06-1 §5.2: RTCP on the next port). When that bind fails, the
/// listen must return an error and the process must live. A null
/// dereference here was a SIGSEGV through the safe API. Afterwards the
/// data port must be free again, so the failed open closed its first socket.
///
/// Runs in a child process so a crash shows up as the child's wait status
/// instead of killing the whole test binary.
///
/// Not on Windows: librist sets SO_REUSEADDR, which on Windows lets its bind
/// take over the held port instead of failing.
#[cfg(not(windows))]
#[test]
fn simple_profile_rtcp_port_collision_is_an_error_not_a_crash() {
    if std::env::var_os(RTCP_CHILD_ENV).is_some() {
        let holder = std::net::UdpSocket::bind(("127.0.0.1", PORT_SIMPLE_RTCP_HELD + 1))
            .expect("hold the RTCP port");
        let url = format!("rist://@127.0.0.1:{PORT_SIMPLE_RTCP_HELD}");
        let first = RistRecvTransportBuilder::new(&url)
            .unwrap()
            .profile(RistProfile::Simple)
            .listen();
        assert!(
            matches!(first, Err(RistError::PeerCreateFailed)),
            "{:?}",
            first.err()
        );
        drop(holder);
        let second = RistRecvTransportBuilder::new(&url)
            .unwrap()
            .profile(RistProfile::Simple)
            .listen();
        assert!(
            second.is_ok(),
            "data port still held after the failed open: {:?}",
            second.err()
        );
        println!("{RTCP_CHILD_OK}");
        return;
    }
    let out = run_child_with_deadline(
        "simple_profile_rtcp_port_collision_is_an_error_not_a_crash",
        RTCP_CHILD_ENV,
        Duration::from_secs(20),
    );
    assert!(
        out.status.success(),
        "child died: {:?}\n{}",
        out.status,
        out.stderr
    );
    assert!(out.stdout.contains(RTCP_CHILD_OK), "{}", out.stdout);
}

#[cfg(not(windows))]
struct ChildOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// Re-executes this test binary on `test` alone with `env` set, and kills the
/// child once `limit` (scaled by `TST_TEST_TIMEOUT_SCALE` for slow runners)
/// has passed, so a hang fails the test instead of wedging the run.
#[cfg(not(windows))]
fn run_child_with_deadline(test: &str, env: &str, limit: Duration) -> ChildOutput {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let scale = std::env::var("TST_TEST_TIMEOUT_SCALE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(1);
    let limit = limit * scale;

    let exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(exe)
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(env, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the child test process");

    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let status = child.wait().expect("wait for the killed child");
            panic!("{test}: the child did not exit within {limit:?} (killed: {status:?})");
        }
        thread::sleep(Duration::from_millis(20));
    };

    // The child prints a few lines of librist logging at most, far below a
    // pipe buffer, so reading after it exited cannot have blocked it.
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }
    ChildOutput {
        status,
        stdout,
        stderr,
    }
}
