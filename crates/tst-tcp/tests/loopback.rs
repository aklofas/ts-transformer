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
