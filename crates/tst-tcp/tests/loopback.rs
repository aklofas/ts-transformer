//! Loopback round-trip tests for TcpTransport (plain, all 4 caller/listener × send/recv combos).

use std::io::{Read, Write};
use std::net::TcpListener as StdTcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tst_core::transport::{RecvTransport, Transport, TransportError};
use tst_tcp::TcpListener;
use tst_tcp::TcpTransport;

#[test]
fn caller_sender_to_std_listener() {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let _t = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = vec![0u8; 1024];
        let n = sock.read(&mut buf).unwrap();
        let _ = tx.send(buf[..n].to_vec());
    });

    let mut send = TcpTransport::connect(&format!("tcp://127.0.0.1:{port}")).unwrap();
    send.send_bytes(&[0x47u8; 188]).unwrap();

    let got = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(got, vec![0x47u8; 188]);
}

#[test]
fn caller_receiver_from_std_sender() {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let _t = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.write_all(&[0x47u8; 188]).unwrap();
    });

    let mut recv = TcpTransport::connect(&format!("tcp://127.0.0.1:{port}")).unwrap();
    let mut buf = vec![0u8; 1024];
    let n = recv.recv_bytes(&mut buf).unwrap();
    assert_eq!(n, 188);
    assert!(buf[..n].iter().all(|&b| b == 0x47));
}

#[test]
fn listener_receiver_from_std_caller() {
    let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = listener.local_addr().unwrap().port();

    let _t = thread::spawn(move || {
        let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.write_all(&[0x47u8; 188]).unwrap();
    });

    let mut accepted = listener.accept_blocking().unwrap();
    let mut buf = vec![0u8; 1024];
    let n = accepted.recv_bytes(&mut buf).unwrap();
    assert_eq!(n, 188);
    assert!(buf[..n].iter().all(|&b| b == 0x47));
}

#[test]
fn listener_sender_to_std_caller() {
    let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = listener.local_addr().unwrap().port();

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let _t = thread::spawn(move || {
        let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut buf = vec![0u8; 1024];
        let n = sock.read(&mut buf).unwrap();
        let _ = tx.send(buf[..n].to_vec());
    });

    let mut accepted = listener.accept_blocking().unwrap();
    accepted.send_bytes(&[0x47u8; 188]).unwrap();
    let got = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(got, vec![0x47u8; 188]);
}

/// Connect a `TcpTransport` to a freshly bound loopback listener and accept
/// the peer synchronously, so both ends of the connection are established
/// before returning. The returned peer `TcpStream` is held silent (no reads
/// or writes) — callers use it to keep the connection alive while parking
/// the transport's `recv_bytes` for a cancel test.
fn loopback_pair() -> (TcpTransport, std::net::TcpStream) {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let client = TcpTransport::connect(&format!("tcp://127.0.0.1:{port}")).unwrap();
    let (server, _) = listener.accept().unwrap();
    (client, server)
}

/// CORR-08: `TcpCancelHandle` must be reachable through both trait objects
/// (`Transport::cancel_handle` / `RecvTransport::cancel_handle`), not just
/// the inherent `TcpTransport::cancel_handle` method — generic shells and
/// the managed wrappers only ever hold a `dyn Transport`/`dyn RecvTransport`.
/// The socket's fixed ~100 ms read-timeout poll (`apply_knobs`) is what lets
/// a parked `recv_bytes` observe the cancel flag; watchdog via `join()`.
#[test]
fn cancel_handle_is_visible_through_both_transport_traits() {
    let (mut client, _server) = loopback_pair();
    let t: &dyn Transport = &client;
    let r: &dyn RecvTransport = &client;
    assert!(t.cancel_handle().is_some());
    assert!(r.cancel_handle().is_some());
    let h = r.cancel_handle().unwrap();

    // Bounded watchdog (mirrors cancel_handle_unblocks_parked_recv below):
    // the thread reports through a channel with a 3 s recv_timeout rather
    // than a bare join(), so a regression that breaks the trait-level
    // cancel_handle() wiring fails the assertion instead of hanging.
    let (tx, rx) = mpsc::channel::<Result<usize, TransportError>>();
    let parked = thread::spawn(move || {
        let mut b = [0u8; 16];
        let result = client.recv_bytes(&mut b);
        let _ = tx.send(result);
    });
    thread::sleep(Duration::from_millis(100));
    h.cancel();

    let result = rx
        .recv_timeout(Duration::from_secs(3))
        .expect("recv_bytes did not unblock within watchdog period after cancel");
    parked.join().unwrap();

    assert!(
        matches!(result, Err(TransportError::Closed)),
        "got {result:?}"
    );
}

/// Thread A parks in `recv_bytes` on a connected-but-silent peer; thread B
/// calls `cancel_handle.cancel()`. Thread A must exit with `Closed` (or
/// `ExplicitClose`) within ≤1 poll interval (~100 ms) plus scheduling slack.
/// Watchdog: 3 s.
#[test]
fn cancel_handle_unblocks_parked_recv() {
    // Set up a silent peer: accept the connection but send nothing.
    let peer_listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let port = peer_listener.local_addr().unwrap().port();
    let _peer = thread::spawn(move || {
        // Accept and hold the socket open so recv_bytes isn't unblocked by a
        // connection-close event.
        let (_sock, _) = peer_listener.accept().unwrap();
        thread::sleep(Duration::from_secs(10));
    });

    let mut transport = TcpTransport::connect(&format!("tcp://127.0.0.1:{port}")).unwrap();
    let handle = transport.cancel_handle();

    // Channel: thread A signals when recv_bytes returned.
    let (tx, rx) = mpsc::channel::<Result<usize, TransportError>>();
    let _recv_thread = thread::spawn(move || {
        let mut buf = vec![0u8; 1024];
        let result = transport.recv_bytes(&mut buf);
        let _ = tx.send(result);
    });

    // Give the recv thread time to park in recv_bytes.
    thread::sleep(Duration::from_millis(50));

    // Cancel from the main thread.
    handle.cancel();

    // Recv thread must unblock within 3 s (≤100 ms + scheduling slack).
    let result = rx
        .recv_timeout(Duration::from_secs(3))
        .expect("recv_bytes did not unblock within watchdog period after cancel");

    // The transport must report it is no longer alive.
    assert!(
        matches!(
            result,
            Err(TransportError::Closed) | Err(TransportError::ExplicitClose)
        ),
        "expected Closed/ExplicitClose after cancel, got {:?}",
        result
    );
}

/// CORR-11: a peer that closes cleanly (FIN) must leave the transport dead.
///
/// `recv_bytes` reports the EOF as `Broken`, so the connection is over — but
/// before the fix only the *send* path stored `alive = false`, leaving
/// `is_alive()` reporting `true` after an observed terminal failure. Managed
/// wrappers poll `is_alive()` to decide when to rebuild, so a stale `true`
/// keeps a dead receiver in service.
#[test]
fn peer_eof_marks_transport_dead() {
    let (mut client, server) = loopback_pair();
    // Orderly close: the peer sends FIN, so the next read returns Ok(0).
    drop(server);

    let mut buf = [0u8; 188];
    let result = client.recv_bytes(&mut buf);
    assert!(
        matches!(result, Err(TransportError::Broken { .. })),
        "expected Broken on peer EOF, got {result:?}"
    );
    assert!(
        !RecvTransport::is_alive(&client),
        "peer EOF must mark the transport dead"
    );
    assert!(
        !Transport::is_alive(&client),
        "the send-side view of liveness must agree after a peer EOF"
    );
}

/// CORR-11 twin: a *fatal read error* (not a clean EOF) must also leave the
/// transport dead. `SO_LINGER 0` on the peer turns its close into an RST, so
/// the client's next read fails with `ECONNRESET` — the terminal `Err(e)` arm
/// of `recv_bytes` rather than the `Ok(0)` arm covered above.
#[test]
fn fatal_read_error_marks_transport_dead() {
    let (mut client, server) = loopback_pair();
    socket2::SockRef::from(&server)
        .set_linger(Some(Duration::ZERO))
        .expect("SO_LINGER 0 on the peer socket");
    // Closing a SO_LINGER-0 socket sends RST instead of FIN.
    drop(server);

    // Watchdog: `recv_bytes` retries its ~100 ms poll indefinitely, so if the
    // RST never materialises the read would park forever. Cancelling after 3 s
    // turns that into `Closed`, which fails the `Broken` assertion below
    // instead of hanging the test binary until nextest kills it.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let handle = client.cancel_handle();
    let watchdog = thread::spawn(move || {
        if done_rx.recv_timeout(Duration::from_secs(3)).is_err() {
            handle.cancel();
        }
    });

    let mut buf = [0u8; 188];
    let result = client.recv_bytes(&mut buf);
    let _ = done_tx.send(());
    watchdog.join().unwrap();

    match &result {
        Err(TransportError::Broken { msg, .. }) => {
            assert!(
                msg.contains("read error"),
                "expected a fatal read, got {msg}"
            );
        }
        other => panic!("expected Broken(read error) after the peer's RST, got {other:?}"),
    }
    assert!(
        !RecvTransport::is_alive(&client),
        "a fatal read error must mark the transport dead"
    );
}
