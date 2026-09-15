//! Regression tests for send-side stalls after a partial write (CORR-22 / Q6).
//!
//! Under a stalled peer the kernel send buffer fills. A single `send_bytes`
//! message can be partially committed to the wire, after which the next
//! internal `write()` hits the 100 ms write timeout and returns `WouldBlock`.
//! The bytes the kernel accepted are already on the wire *in order*, so the
//! stream is intact: `write_loop` keeps writing the remainder, checking the
//! cancel flag at every ~100 ms tick, and the peer sees one contiguous byte
//! stream once it resumes reading. Only a cancel/close bounds that loop.
//!
//! The contract (tst_core::transport::Transport::send_bytes): a *zero-progress*
//! WouldBlock is reported as `Backpressure` (the slice is intact, retry it);
//! `Ok(0)` and hard errors are `Broken` + dead. A partial prefix is never a
//! reason to tear the connection down — the old "Broken + rebuild" answer was
//! what desynced the peer's 188-byte framing (the managed reconnect started a
//! fresh connection mid-message).
//!
//! Both tests carry `loopback` in their names so nextest funnels them through
//! the serialised `network` group (`.config/nextest.toml`).

use std::io::Read;
use std::net::TcpListener as StdTcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tst_core::transport::{Transport, TransportError};
use tst_tcp::TcpTransport;

/// Total bytes pushed through the stalled connection: four 32 KiB messages
/// (the default `pkt_size` is 64 KiB), far larger than the ~12 KiB in-flight
/// window `?sndbuf=4096` + the peer's 2 KiB `SO_RCVBUF` allow, so the first
/// `send_bytes` is guaranteed to commit a partial prefix and then hit the
/// 100 ms write timeout while the peer is not reading.
const MSG_LEN: usize = 32 * 1024;
const MSG_COUNT: usize = 4;

/// Position-tagged byte stream: byte `k` of the whole stream is `k % 251`, so
/// any duplicated prefix or dropped remainder shifts the pattern and fails the
/// equality below at the exact offset.
fn tagged_stream() -> Vec<u8> {
    (0..MSG_LEN * MSG_COUNT).map(|k| (k % 251) as u8).collect()
}

/// A peer that accepts, pins a tiny receive buffer, stalls for `stall`, then
/// reads everything to EOF and hands the bytes back.
fn stalling_peer(stall: Duration) -> (u16, mpsc::Receiver<Vec<u8>>) {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        socket2::SockRef::from(&sock)
            .set_recv_buffer_size(2048)
            .expect("SO_RCVBUF on the peer");
        // Stall: nothing is read, so the sender's kernel buffer fills and its
        // 100 ms write timeout fires mid-message.
        thread::sleep(stall);
        let mut got = Vec::new();
        let _ = sock.read_to_end(&mut got);
        let _ = tx.send(got);
    });
    (port, rx)
}

/// CORR-22 / Q6 RED: a peer that stops reading for 500 ms (five write
/// timeouts) and then resumes must receive one contiguous byte stream. On
/// `9b3fe2ee` the first `send_bytes` returns
/// `Broken { msg: "partial write then WouldBlock (N/32768 bytes); …" }` and
/// the `expect` below fires.
#[test]
fn partial_write_stall_loopback_stream_stays_contiguous() {
    let (port, got_rx) = stalling_peer(Duration::from_millis(500));
    let url = format!("tcp://127.0.0.1:{port}?sndbuf=4096");
    let mut send = TcpTransport::connect(&url).expect("connect");
    let handle = send.cancel_handle();
    let expected = tagged_stream();

    // The sends run on a helper thread so a regression that parks a send
    // forever fails the watchdog below instead of hanging the binary.
    let (done_tx, done_rx) = mpsc::channel::<Result<(), TransportError>>();
    let stream = expected.clone();
    let sender = thread::spawn(move || {
        let mut result = Ok(());
        for chunk in stream.chunks(MSG_LEN) {
            // Zero-progress Backpressure is still part of the contract: the
            // slice is intact, retry it. Anything else ends the run.
            loop {
                match send.send_bytes(chunk) {
                    Ok(()) => break,
                    Err(TransportError::Backpressure { .. }) => continue,
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
            if result.is_err() {
                break;
            }
        }
        // Orderly close so the peer's read_to_end sees EOF.
        send.close();
        let _ = done_tx.send(result);
    });

    let outcome = match done_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(r) => r,
        Err(_) => {
            handle.cancel();
            panic!("sends did not complete within 10 s after the peer resumed");
        }
    };
    sender.join().unwrap();
    outcome.expect("send_bytes must complete once the peer resumes reading");

    let got = got_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("peer did not hand back the stream");
    assert_eq!(got.len(), expected.len(), "byte count differs");
    assert!(
        got == expected,
        "stream is not contiguous: first mismatch at offset {}",
        got.iter()
            .zip(&expected)
            .position(|(a, b)| a != b)
            .unwrap_or(got.len())
    );
}

/// The remainder loop is bounded by cancel: against a peer that NEVER reads,
/// a `cancel()` from another thread ends the parked `send_bytes` with
/// `Closed` within a couple of poll ticks and leaves the transport dead. On
/// `9b3fe2ee` the send returns `Broken { msg: "partial write …" }` long
/// before the cancel lands, so the `Closed` match fails.
///
/// DEVIATION from the brief (see the WP-4b report): loopback's TCP receive
/// window is negotiated at accept time, *before* the peer's `SO_RCVBUF`
/// shrink takes effect, and how much of it survives the shrink is racy —
/// empirically anywhere from ~64 KiB to well beyond that before a send
/// against this never-reading peer first blocks. A fixed one- or two-chunk
/// prime (what the brief's literal test does) is absorbed by that window
/// often enough to make the test flaky on this box. Instead the sender
/// loop below just keeps pushing the same chunk, retrying `Backpressure`
/// (the crate's documented retryable outcome) exactly like a real caller
/// would, until the window genuinely closes — whichever chunk straddles
/// that boundary is the one that commits a partial prefix and parks inside
/// `write_loop`'s new alive-check, so the test still pins Task 4b.1's fix
/// regardless of exactly how large the free burst turns out to be. Every
/// call, parked or not, starts with the crate's own entry check
/// (`send_bytes` returns `Closed` immediately once `alive` is false), so
/// the loop is bounded by `cancel()` either way.
#[test]
fn partial_write_stall_loopback_cancel_unblocks_parked_send() {
    // A 60 s stall is "never" for this test; the thread is detached.
    let (port, _got_rx) = stalling_peer(Duration::from_secs(60));
    let url = format!("tcp://127.0.0.1:{port}?sndbuf=4096");
    let mut send = TcpTransport::connect(&url).expect("connect");
    let handle = send.cancel_handle();
    let chunk = vec![0x47u8; MSG_LEN];

    let (done_tx, done_rx) = mpsc::channel::<(Result<(), TransportError>, bool)>();
    let sender = thread::spawn(move || {
        let mut result;
        loop {
            result = send.send_bytes(&chunk);
            match &result {
                Ok(()) => continue,
                Err(TransportError::Backpressure { .. }) => continue,
                Err(_) => break,
            }
        }
        let alive = send.is_alive();
        let _ = done_tx.send((result, alive));
    });

    // Let the loop exhaust the free burst and park for real, then cancel.
    thread::sleep(Duration::from_millis(300));
    handle.cancel();

    let (result, alive) = done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("cancel did not unblock the parked send within 2 s");
    sender.join().unwrap();
    assert!(
        matches!(result, Err(TransportError::Closed)),
        "expected Closed after cancel mid-message, got {result:?}"
    );
    assert!(!alive, "a cancelled transport must report dead");
}
