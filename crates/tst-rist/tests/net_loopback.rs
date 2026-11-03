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
/// Main profile (any parity): the zero-nonce receiver and the key-bit proxy
/// in front of it, then the proxy's unencrypted control pair.
#[cfg(feature = "mbedtls")]
const PORT_ZERO_NONCE_RECV: u16 = 33031;
#[cfg(feature = "mbedtls")]
const PORT_ZERO_NONCE_PROXY: u16 = 33033;
#[cfg(feature = "mbedtls")]
const PORT_PROXY_CONTROL_RECV: u16 = 33035;
#[cfg(feature = "mbedtls")]
const PORT_PROXY_CONTROL_PROXY: u16 = 33037;
/// Main profile: the encrypted receiver and the proxy for the rekey-throttle test.
#[cfg(feature = "mbedtls")]
const PORT_THROTTLE_RECV: u16 = 33039;
#[cfg(feature = "mbedtls")]
const PORT_THROTTLE_PROXY: u16 = 33041;
/// Main profile: the encrypted receiver and the proxy for the lockout-recovery test.
#[cfg(feature = "mbedtls")]
const PORT_LOCKOUT_RECV: u16 = 33111;
#[cfg(feature = "mbedtls")]
const PORT_LOCKOUT_PROXY: u16 = 33113;
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
/// Ignored when pkg-config linked a system librist, which still has the
/// null dereference (see `rist-sys`'s source fixup).
///
/// Not on Windows: librist sets SO_REUSEADDR, which on Windows lets its bind
/// take over the held port instead of failing.
#[cfg(not(windows))]
#[cfg_attr(
    not(librist_source_fixups),
    ignore = "needs the vendored librist build: a system librist lacks the RTCP null check"
)]
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

/// How [`spawn_key_bit_proxy`] rewrites each datagram from the sender.
#[cfg(feature = "mbedtls")]
#[derive(Clone, Copy)]
enum Rewrite {
    /// Forward unchanged: proves the proxy itself delivers.
    PassThrough,
    /// Give a clear GRE packet an all-zero key (nonce) field, so an
    /// encrypted receiver sees "encrypted" data it must refuse to decrypt.
    ZeroNonce,
    /// Per real datagram `D` (round `r`): five copies of `D` with nonce
    /// `[1, r, 0x5a, 0xa5]`, then `D` with nonce `[2, r, 0x5a, 0xa5]` inside
    /// the receiver's rekey throttle window. Rounds start at least 15 ms apart so the
    /// first copy of each round is outside the previous round's window.
    ThrottleSchedule,
    /// Once [`ProxyHandle::arm`] is called, flip the RTP version bits of every
    /// keyed data-path datagram, and drop every other datagram from the sender,
    /// until `garbage` media datagrams have been flipped. CTR mode flips the
    /// same plaintext bits, so each data packet decrypts to an invalid header:
    /// one decryption-failure strike under the sender's own nonce. Nothing
    /// reaches the receiver unflipped during the burst, so no valid packet
    /// decays the count between strikes. Then every datagram is forwarded unchanged; once
    /// [`ProxyHandle::release`] is called, the proxy waits 50 ms, sends one
    /// header-only datagram under a nonce no sender uses (same key slot), so
    /// the sender's nonce is fresh again, and waits another 50 ms.
    GarbageThenPass { garbage: u32 },
}

/// For a keyed, sequenced GRE datagram on librist's data path, the offsets of
/// its key (nonce) field, of its encrypted payload, and of the first byte of
/// the RTP or RTCP header inside that payload. The payload opens with the
/// 4-byte port subheader, after a 4-byte VSF subheader when the protocol is
/// VSF (`0xCCE0`, the GRE version 2 framing) rather than reduced (`0x88B6`).
#[cfg(feature = "mbedtls")]
fn keyed_data_offsets(dgram: &[u8]) -> Option<(usize, usize, usize)> {
    if dgram.len() < 4 || dgram[0] & 0x30 != 0x30 {
        return None;
    }
    let subheaders = match [dgram[2], dgram[3]] {
        [0x88, 0xb6] => 4,
        [0xcc, 0xe0] => 8,
        _ => return None,
    };
    let nonce = 4 + if dgram[0] & 0x80 != 0 { 4 } else { 0 };
    let payload = nonce + 8;
    Some((nonce, payload, payload + subheaders))
}

/// Stops the proxy thread when dropped.
#[cfg(feature = "mbedtls")]
struct ProxyHandle {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Datagrams given a zero nonce, throttle rounds emitted, or garbage and
    /// nonce-reset datagrams sent.
    rewritten: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Starts [`Rewrite::GarbageThenPass`]; the other rewrites ignore it.
    armed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Sends [`Rewrite::GarbageThenPass`]'s nonce-reset datagram.
    released: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(feature = "mbedtls")]
impl ProxyHandle {
    fn rewritten(&self) -> usize {
        self.rewritten.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn release(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(feature = "mbedtls")]
impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A one-socket UDP relay on `127.0.0.1:listen`: datagrams from the sender go
/// to `127.0.0.1:forward_to` after `rewrite`; datagrams from the receiver go
/// back to the last sender address untouched.
#[cfg(feature = "mbedtls")]
fn spawn_key_bit_proxy(listen: u16, forward_to: u16, rewrite: Rewrite) -> ProxyHandle {
    use std::net::{SocketAddr, UdpSocket};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let sock = UdpSocket::bind(("127.0.0.1", listen)).expect("bind the proxy");
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .expect("proxy read timeout");
    let receiver: SocketAddr = ([127, 0, 0, 1], forward_to).into();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let rewritten = Arc::new(AtomicUsize::new(0));
    let rewritten_thread = Arc::clone(&rewritten);
    let armed = Arc::new(AtomicBool::new(false));
    let armed_thread = Arc::clone(&armed);
    let released = Arc::new(AtomicBool::new(false));
    let released_thread = Arc::clone(&released);
    let thread = thread::spawn(move || {
        let mut sender: Option<SocketAddr> = None;
        let mut round: u8 = 0;
        let mut garbled: u32 = 0;
        let mut reset: Option<Vec<u8>> = None;
        let mut buf = vec![0u8; 65536];
        while !stop_thread.load(Ordering::SeqCst) {
            if released_thread.load(Ordering::SeqCst)
                && let Some(reset) = reset.take()
            {
                // Past librist's 10 ms rekey throttle either side.
                thread::sleep(Duration::from_millis(50));
                let _ = sock.send_to(&reset, receiver);
                rewritten_thread.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(50));
            }
            let (n, from) = match sock.recv_from(&mut buf) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if from == receiver {
                if let Some(to) = sender {
                    let _ = sock.send_to(&buf[..n], to);
                }
                continue;
            }
            sender = Some(from);
            let mut dgram = buf[..n].to_vec();
            if let Rewrite::ZeroNonce = rewrite {
                // GRE base header = flags1, flags2, protocol type; librist
                // puts the key field right after it, before the sequence.
                if dgram.len() >= 4 && dgram[0] & 0x20 == 0 {
                    dgram.splice(4..4, [0u8; 4]);
                    dgram[0] |= 0x20;
                    rewritten_thread.fetch_add(1, Ordering::SeqCst);
                }
            }
            if let Rewrite::ThrottleSchedule = rewrite {
                if dgram.len() >= 4 && dgram[0] & 0x20 == 0 {
                    let keyed = |nonce: [u8; 4]| {
                        let mut d = dgram.clone();
                        d.splice(4..4, nonce);
                        d[0] |= 0x20;
                        d
                    };
                    let garbage = keyed([0x01, round, 0x5a, 0xa5]);
                    for _ in 0..5 {
                        let _ = sock.send_to(&garbage, receiver);
                    }
                    let _ = sock.send_to(&keyed([0x02, round, 0x5a, 0xa5]), receiver);
                    round = round.wrapping_add(1);
                    rewritten_thread.fetch_add(1, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(15));
                }
                continue;
            }
            if let Rewrite::GarbageThenPass { garbage } = rewrite {
                let in_burst = armed_thread.load(Ordering::SeqCst) && garbled < garbage;
                // The RTP (or RTCP) header's top two bits must read 0b10.
                if let Some((nonce, payload, rtp)) = keyed_data_offsets(&dgram)
                    && in_burst
                    && dgram.len() > rtp
                {
                    dgram[rtp] ^= 0xc0;
                    let _ = sock.send_to(&dgram, receiver);
                    // Only a datagram carrying a whole TS packet is surely
                    // media; a smaller one may be a control message, which
                    // takes no strike.
                    if dgram.len() >= rtp + 12 + 188 {
                        garbled += 1;
                        rewritten_thread.fetch_add(1, Ordering::SeqCst);
                    }
                    if garbled == garbage {
                        // Header plus four bytes: the decrypt rekeys, then the
                        // datagram is too short to strike.
                        let mut r = dgram[..payload + 4].to_vec();
                        r[nonce] = (r[nonce] & 0x80) | 0x5a;
                        r[nonce + 1..nonce + 4].copy_from_slice(&[0xa5, 0x5a, 0xa5]);
                        reset = Some(r);
                    }
                    continue;
                }
                if in_burst {
                    // Not a shape the burst can flip: never let it through.
                    continue;
                }
            }
            let _ = sock.send_to(&dgram, receiver);
        }
    });
    ProxyHandle {
        stop,
        rewritten,
        armed,
        released,
        thread: Some(thread),
    }
}

/// Sends `count` packets, `gap` apart, from an unencrypted Main sender through
/// a proxy to `recv` and returns what `recv` delivered within `drain` (same
/// connect waits as the AES test).
#[cfg(feature = "mbedtls")]
fn send_clear_through_proxy(
    recv: tst_rist::RistRecvTransport,
    proxy_port: u16,
    count: u8,
    gap: Duration,
    drain: Duration,
) -> Vec<Vec<u8>> {
    let (tx_payloads, rx_payloads) = mpsc::channel::<Vec<Vec<u8>>>();
    let _recv_thread = thread::spawn(move || {
        let collected = drain_n(recv, usize::from(count), drain);
        let _ = tx_payloads.send(collected);
    });

    thread::sleep(Duration::from_millis(300));
    let mut send = RistTransportBuilder::new(&format!("rist://127.0.0.1:{proxy_port}"))
        .unwrap()
        .profile(RistProfile::Main)
        .connect()
        .expect("connect");
    thread::sleep(Duration::from_millis(1200));

    for i in 1..=count {
        send.send_bytes(&synthetic_ts_packet(i)).expect("send");
        thread::sleep(gap);
    }
    rx_payloads
        .recv_timeout(drain + Duration::from_secs(3))
        .expect("recv thread didn't return in time")
}

/// librist's PSK decrypt returns early on an all-zero nonce without flagging
/// the packet, and the Main-profile caller decrypts in place, so a clear
/// packet given a zero key field was delivered as if it had been decrypted.
/// An encrypted receiver must deliver nothing it did not decrypt.
///
/// Ignored when pkg-config linked a system librist, which still accepts them
/// (see `rist-sys`'s source fixups).
#[cfg(feature = "mbedtls")]
#[cfg_attr(
    not(librist_source_fixups),
    ignore = "needs the vendored librist build: a system librist lacks the PSK decrypt-status fix"
)]
#[test]
fn main_profile_encrypted_receiver_drops_zero_nonce_clear_packets() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());

    // Control: the proxy delivers between two unencrypted ends.
    let recv =
        RistRecvTransportBuilder::new(&format!("rist://@127.0.0.1:{PORT_PROXY_CONTROL_RECV}"))
            .unwrap()
            .profile(RistProfile::Main)
            .listen()
            .expect("listen");
    let proxy = spawn_key_bit_proxy(
        PORT_PROXY_CONTROL_PROXY,
        PORT_PROXY_CONTROL_RECV,
        Rewrite::PassThrough,
    );
    let collected = send_clear_through_proxy(
        recv,
        PORT_PROXY_CONTROL_PROXY,
        5,
        Duration::ZERO,
        // Ends early once all 5 arrive; the cap keeps both halves inside the
        // nextest network kill (20 s).
        Duration::from_secs(6),
    );
    drop(proxy);
    assert!(
        collected.len() >= 3,
        "control: expected at least 3 of 5 packets through the proxy; got {}",
        collected.len()
    );

    // Case: the same clear packets, given a zero nonce, at an encrypted receiver.
    let recv = RistRecvTransportBuilder::new(&format!("rist://@127.0.0.1:{PORT_ZERO_NONCE_RECV}"))
        .unwrap()
        .profile(RistProfile::Main)
        .encryption(EncryptionKey::aes256("loopback-test-secret-keep-private"))
        .listen()
        .expect("listen");
    let proxy = spawn_key_bit_proxy(
        PORT_ZERO_NONCE_PROXY,
        PORT_ZERO_NONCE_RECV,
        Rewrite::ZeroNonce,
    );
    let collected = send_clear_through_proxy(
        recv,
        PORT_ZERO_NONCE_PROXY,
        5,
        Duration::ZERO,
        // Nothing should arrive, so this always runs out; a delivered packet
        // would land within the receiver's ~1 s recovery buffer of the sends.
        Duration::from_secs(5),
    );
    let rewritten = proxy.rewritten();
    drop(proxy);
    assert!(
        rewritten >= 1,
        "the proxy rewrote no datagram, so the case proves nothing"
    );
    assert!(
        collected.is_empty(),
        "encrypted receiver delivered {} clear packet(s)",
        collected.len()
    );
}

/// librist throttles rekeys once an encrypted receiver has seen garbage, and
/// the throttled packet used to be handed on as if it had been decrypted. Each
/// round here makes the receiver adopt a nonce, strike on garbage, then see
/// the real clear datagram under a second nonce inside the throttle window.
/// Every datagram, RTCP included, takes that path, so the flow authenticates
/// and delivers media unless a throttled packet is dropped.
#[cfg(feature = "mbedtls")]
#[cfg_attr(
    not(librist_source_fixups),
    ignore = "needs the vendored librist build: a system librist lacks the PSK decrypt-status fix"
)]
#[test]
fn main_profile_encrypted_receiver_drops_clear_packets_inside_the_rekey_throttle() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());

    let recv = RistRecvTransportBuilder::new(&format!("rist://@127.0.0.1:{PORT_THROTTLE_RECV}"))
        .unwrap()
        .profile(RistProfile::Main)
        .encryption(EncryptionKey::aes256("loopback-test-secret-keep-private"))
        .listen()
        .expect("listen");
    let proxy = spawn_key_bit_proxy(
        PORT_THROTTLE_PROXY,
        PORT_THROTTLE_RECV,
        Rewrite::ThrottleSchedule,
    );
    let collected = send_clear_through_proxy(
        recv,
        PORT_THROTTLE_PROXY,
        20,
        Duration::from_millis(50),
        Duration::from_secs(6),
    );
    let rounds = proxy.rewritten();
    drop(proxy);
    assert!(
        rounds >= 10,
        "the proxy emitted {rounds} throttle round(s), expected at least 10"
    );
    assert!(
        collected.is_empty(),
        "encrypted receiver delivered {} clear packet(s)",
        collected.len()
    );
}

/// Seven undecryptable packets latch librist's PSK lockout, which then drops
/// every packet until a fresh nonce rekeys the key; upstream rekeyed but never
/// cleared the latch, so after one burst of garbage the receiver refused even a
/// legitimate sender with the right passphrase until it was recreated. Here the
/// burst is the sender's own first datagrams made undecryptable. The test
/// first checks that the lockout holds (unchanged packets under the same nonce
/// are refused), then sends a datagram under another nonce and the sender's
/// nonce again: both rekeys succeed, so the stream must resume.
#[cfg(feature = "mbedtls")]
#[cfg_attr(
    not(librist_source_fixups),
    ignore = "needs the vendored librist build: a system librist keeps the PSK lockout"
)]
#[test]
fn main_profile_encrypted_receiver_recovers_after_a_garbage_lockout() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    const GARBAGE: u32 = 12;
    let psk = "loopback-test-secret-keep-private";
    let before = synthetic_ts_packet(0xdd);
    let filler = synthetic_ts_packet(0xee);
    let held = synthetic_ts_packet(0xcc);
    let pkts: Vec<[u8; 188]> = (1..=20).map(synthetic_ts_packet).collect();

    // A latched peer receives no valid packet, so librist's liveness clock
    // stops for it; a long session timeout keeps the peer (and its latched
    // key) alive for the whole test instead of letting a timeout reset it.
    let mut recv = RistRecvTransportBuilder::new(&format!("rist://@127.0.0.1:{PORT_LOCKOUT_RECV}"))
        .unwrap()
        .profile(RistProfile::Main)
        .encryption(EncryptionKey::aes256(psk))
        .session_timeout(Duration::from_secs(30))
        .listen()
        .expect("listen");
    let proxy = spawn_key_bit_proxy(
        PORT_LOCKOUT_PROXY,
        PORT_LOCKOUT_RECV,
        Rewrite::GarbageThenPass { garbage: GARBAGE },
    );

    // Shared so the test can read what arrived while the lockout holds.
    let delivered_so_far = std::sync::Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let out = std::sync::Arc::clone(&delivered_so_far);
    let wanted = pkts.clone();
    // Set once the numbered packets are sent: the drain ends 3 s later even
    // if one was lost.
    let drain_until = std::sync::Arc::new(Mutex::new(None::<Instant>));
    let until = std::sync::Arc::clone(&drain_until);
    let (tx_done, rx_done) = mpsc::channel::<()>();
    let _recv_thread = thread::spawn(move || {
        // Runs until every numbered packet arrived or 14 s passed; inside the
        // nextest network kill (20 s).
        let mut buf = vec![0u8; recv.max_payload() + 64];
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(14)
            && until
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_none_or(|t| Instant::now() < t)
        {
            match recv.recv_bytes(&mut buf) {
                Ok(n) => out
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(buf[..n].to_vec()),
                Err(TransportError::Backpressure { .. }) => {}
                Err(e) => {
                    eprintln!("recv error: {e:?}");
                    break;
                }
            }
            let out = out.lock().unwrap_or_else(|p| p.into_inner());
            if wanted
                .iter()
                .all(|w| out.iter().any(|o| o.as_slice() == w.as_slice()))
            {
                break;
            }
        }
        let _ = tx_done.send(());
    });

    thread::sleep(Duration::from_millis(300));
    let mut send = RistTransportBuilder::new(&format!("rist://127.0.0.1:{PORT_LOCKOUT_PROXY}"))
        .unwrap()
        .profile(RistProfile::Main)
        .encryption(EncryptionKey::aes256(psk))
        .connect()
        .expect("connect");

    // Before the flow forms the sender emits only sparse control traffic, so
    // the burst starts once it carries media: the filler and the retransmits
    // the dropped datagrams provoke supply it.
    thread::sleep(Duration::from_millis(1200));
    for _ in 0..5 {
        send.send_bytes(&before).expect("send before the burst");
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(300));
    proxy.arm();
    // 3 s keeps the burst well clear of the receiver's 14 s cap, so the drain
    // after the numbered packets is never cut short.
    let burst_deadline = Instant::now() + Duration::from_secs(3);
    while proxy.rewritten() < GARBAGE as usize && Instant::now() < burst_deadline {
        send.send_bytes(&filler).expect("send filler");
        thread::sleep(Duration::from_millis(20));
    }
    let rewritten = proxy.rewritten();
    assert!(
        rewritten >= GARBAGE as usize,
        "the proxy flipped {rewritten} of {GARBAGE} media datagrams, so the case proves nothing"
    );

    // The latch holds: packets the proxy now forwards unchanged, under the
    // sender's nonce, are refused. 800 ms is four times the receiver's 200 ms
    // buffer, so a receiver that accepted them would have delivered them.
    thread::sleep(Duration::from_millis(100));
    for _ in 0..10 {
        send.send_bytes(&held).expect("send while locked out");
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(800));
    let (fillers, helds, others) = {
        let so_far = delivered_so_far.lock().unwrap();
        let count = |p: &[u8; 188]| {
            so_far
                .iter()
                .filter(|g| g.as_slice() == p.as_slice())
                .count()
        };
        let known = count(&before) + count(&filler) + count(&held);
        (count(&filler), count(&held), so_far.len() - known)
    };
    assert_eq!(
        (fillers, helds, others),
        (0, 0, 0),
        "after the burst the receiver delivered (filler, held, other) packets, so no lockout held"
    );

    proxy.release();
    let reset_deadline = Instant::now() + Duration::from_secs(2);
    while proxy.rewritten() <= GARBAGE as usize && Instant::now() < reset_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        proxy.rewritten() > GARBAGE as usize,
        "the proxy never sent its nonce-reset datagram"
    );
    // Settle past the receiver's recovery.
    thread::sleep(Duration::from_millis(300));
    for p in &pkts {
        send.send_bytes(p).expect("send");
        thread::sleep(Duration::from_millis(20));
    }

    *drain_until.lock().unwrap() = Some(Instant::now() + Duration::from_secs(3));
    rx_done
        .recv_timeout(Duration::from_secs(15))
        .expect("recv thread didn't return in time");
    drop(proxy);
    let collected = delivered_so_far.lock().unwrap().clone();
    // A filler or held packet may still arrive as a retransmit after recovery.
    for got in &collected {
        assert!(
            got.as_slice() == before.as_slice()
                || got.as_slice() == filler.as_slice()
                || got.as_slice() == held.as_slice()
                || pkts.iter().any(|p| got.as_slice() == p.as_slice()),
            "decrypted payload mismatch"
        );
    }
    assert!(
        collected
            .iter()
            .any(|got| got.as_slice() == before.as_slice()),
        "control: nothing arrived before the burst, so the case proves nothing"
    );
    let delivered = pkts
        .iter()
        .filter(|p| collected.iter().any(|got| got.as_slice() == p.as_slice()))
        .count();
    assert!(
        delivered >= 15,
        "expected at least 15 of 20 packets after the lockout; got {delivered}"
    );
}
