//! Loopback round-trip tests for TcpTransport (plain, all 4 caller/listener × send/recv combos).

use std::io::{Read, Write};
use std::net::TcpListener as StdTcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tst_core::transport::{BrokenCause, RecvTransport, Transport, TransportError};
use tst_tcp::TcpListener;
use tst_tcp::TcpTransport;
use tst_tcp::error::TcpError;

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

/// WP-C1: `is_cancelled()` is the CANCEL latch, never a liveness proxy.
///
/// `TcpCancelHandle::is_cancelled()` used to be `!alive`, and tst-tcp drops
/// `alive` on a clean peer EOF as well as on a cancel. Once the binding
/// layer's `Owned::is_cancelled` ORs the transport's latch in (WP-C1), that
/// spelling would relabel every clean TCP peer EOF as a caller cancel —
/// `TST_E_CLOSED` instead of `TST_E_END_OF_STREAM` at the C ABI, and the
/// same one-kind shift in Python and the JVM. Pinned here at the crate
/// level; the `peer_eof_is_not_a_cancel` conformance-kit row generalises it.
#[test]
fn peer_eof_is_not_a_cancel() {
    let (mut client, server) = loopback_pair();
    let h = client.cancel_handle();
    assert!(!h.is_cancelled(), "fresh handle");

    // Clean EOF: the peer shuts the connection down without anyone
    // cancelling. `recv_bytes` reports it and latches `alive = false`.
    drop(server);
    let mut b = [0u8; 16];
    let got = client.recv_bytes(&mut b);
    assert!(
        matches!(
            got,
            Err(TransportError::Broken {
                cause: BrokenCause::CleanEof,
                ..
            })
        ),
        "expected a clean-EOF Broken, got {got:?}"
    );

    assert!(
        !RecvTransport::is_alive(&client),
        "a peer EOF kills liveness"
    );
    assert!(
        !h.is_cancelled(),
        "a peer EOF must NOT read as a caller cancel"
    );
    // A handle minted after the EOF must agree.
    assert!(!client.cancel_handle().is_cancelled());

    // And a real cancel still latches, on this handle and on a later one.
    h.cancel();
    assert!(h.is_cancelled());
    assert!(client.cancel_handle().is_cancelled());
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
    // Set up a silent peer: accept the connection but send nothing. The peer
    // holds its socket open until the test releases it through `release_tx`,
    // so recv_bytes cannot be unblocked by a connection-close event — and the
    // peer is released on every path (and joined when the test passes)
    // instead of being left to sleep out a fixed window past the end of the
    // test.
    let peer_listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let port = peer_listener.local_addr().unwrap().port();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let peer = thread::spawn(move || {
        let (_sock, _) = peer_listener.accept().unwrap();
        // Either the release arrives (the guard below fired) or the sender
        // was dropped by an unwinding test; both end the hold, and `_sock`
        // drops here.
        let _ = release_rx.recv();
    });
    // Released when this scope ends — AFTER the assertions below, so the
    // peer's own close can never feed the state under test, and on every
    // path, so a failing assertion never leaves the thread parked; joined
    // too when the test passes (see `HeldPeer`).
    let _peer_guard = HeldPeer {
        release: Some(release_tx),
        thread: Some(peer),
    };

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

/// A silent peer's cleanup, run by `Drop` so it happens at scope end: after
/// the assertions on success, during unwinding on failure. Always signals
/// the peer to drop its held socket; joins it only on the non-panicking
/// path (see the comment in `drop`).
struct HeldPeer {
    release: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for HeldPeer {
    fn drop(&mut self) {
        if let Some(tx) = self.release.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            // Join only on the non-panicking path. During unwinding the peer
            // may never have left `accept()` (the client's connect is what
            // releases it), so a join there could turn an assertion failure
            // into a hung test run; the release above plus the dropped sender
            // let it exit on its own instead.
            if thread::panicking() {
                return;
            }
            t.join().expect("peer thread panicked");
        }
    }
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
    // The structured discriminator: a peer FIN is a *clean* end of stream, and
    // callers must be able to tell it from a read error without parsing `msg`.
    match &result {
        Err(TransportError::Broken {
            cause, errno_code, ..
        }) => {
            assert_eq!(
                *cause,
                BrokenCause::CleanEof,
                "a peer FIN must be reported as a clean EOF"
            );
            assert_eq!(*errno_code, None, "a clean EOF carries no errno_code");
        }
        other => panic!("expected Broken on peer EOF, got {other:?}"),
    }
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
        Err(TransportError::Broken { msg, cause, .. }) => {
            assert!(
                msg.contains("read error"),
                "expected a fatal read, got {msg}"
            );
            // An RST is not a clean end of stream: the discriminator must stay
            // at its default so callers cannot mistake it for a peer FIN.
            assert_eq!(
                *cause,
                BrokenCause::Unspecified,
                "a fatal read error must not claim a clean EOF"
            );
        }
        other => panic!("expected Broken(read error) after the peer's RST, got {other:?}"),
    }
    assert!(
        !RecvTransport::is_alive(&client),
        "a fatal read error must mark the transport dead"
    );
}

/// X-CORR-07 / Q5 (E07): an empty destination must be a no-op, not a fake
/// EOF. `TcpStream::read(&mut [])` returns `Ok(0)` on an OPEN peer, and the
/// `Ok(0)` arm of `recv_bytes` used to read that as a peer FIN — latching
/// `alive = false` and returning `Broken { cause: CleanEof }` while the peer
/// was still connected and about to send.
#[test]
fn empty_recv_does_not_fake_eof() {
    let (mut client, mut server) = loopback_pair();

    let r = client.recv_bytes(&mut []);
    assert!(
        matches!(r, Ok(0)),
        "an empty destination must be an Ok(0) no-op, got {r:?}"
    );
    assert!(
        RecvTransport::is_alive(&client),
        "an empty receive must not touch liveness"
    );

    // The peer is still open: a real payload arrives on the next call.
    server.write_all(&[0x47u8; 188]).unwrap();
    let mut buf = [0u8; 1024];
    let n = client
        .recv_bytes(&mut buf)
        .expect("real payload after the empty call");
    assert_eq!(n, 188);
    assert!(buf[..n].iter().all(|&b| b == 0x47));
}

/// CORR-12: a `TcpListener` parked in `accept_blocking` must be
/// unblockable from another thread. On `9b3fe2ee` this does not compile
/// (`no method named cancel_handle found for struct TcpListener`) and the
/// accept is a bare blocking `accept(2)` with no cancel path at any layer.
#[test]
fn listener_cancel_handle_unblocks_parked_accept() {
    let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let handle = listener.cancel_handle();

    let (tx, rx) = mpsc::channel::<Result<(), TcpError>>();
    let parked = thread::spawn(move || {
        let result = listener.accept_blocking().map(|_| ());
        let _ = tx.send(result);
    });
    // Let the thread park in the accept poll.
    thread::sleep(Duration::from_millis(150));
    handle.cancel();

    let result = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("accept_blocking did not unblock within 1 s after cancel");
    parked.join().unwrap();
    assert!(
        matches!(result, Err(TcpError::Closed)),
        "expected TcpError::Closed after cancel, got {result:?}"
    );
}

/// `close()` is the same flag from the listener's own side: it unblocks a
/// parked accept AND latches, so every later accept returns `Closed` at
/// its entry check without touching the socket.
#[test]
fn listener_close_unblocks_parked_accept_and_latches() {
    let listener = std::sync::Arc::new(TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap());

    let (tx, rx) = mpsc::channel::<Result<(), TcpError>>();
    let l = listener.clone();
    let parked = thread::spawn(move || {
        let _ = tx.send(l.accept_blocking().map(|_| ()));
    });
    thread::sleep(Duration::from_millis(150));
    listener.close();

    let result = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("accept_blocking did not unblock within 1 s after close");
    parked.join().unwrap();
    assert!(matches!(result, Err(TcpError::Closed)), "got {result:?}");

    // Latched: a fresh accept returns immediately.
    let again = listener.accept_blocking().map(|_| ());
    assert!(matches!(again, Err(TcpError::Closed)), "got {again:?}");
}

/// The non-blocking poll must still hand back a working, BLOCKING transport:
/// the std peer connects only after 300 ms, so the first `accept()` inside
/// the loop is guaranteed to have returned `WouldBlock` at least twice
/// before the connection lands, and the accepted transport then receives
/// a real payload through its normal 100 ms read-timeout poll.
#[test]
fn listener_accept_resumes_after_wouldblock_polls() {
    let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = listener.local_addr().unwrap().port();

    let _t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.write_all(&[0x47u8; 188]).unwrap();
        // Keep the peer open long enough for the read below; the sender
        // side then closes on drop.
        thread::sleep(Duration::from_millis(200));
    });

    let mut accepted = listener.accept_blocking().expect("accept after polling");
    let mut buf = vec![0u8; 1024];
    let n = accepted.recv_bytes(&mut buf).unwrap();
    assert_eq!(n, 188);
    assert!(buf[..n].iter().all(|&b| b == 0x47));
}
