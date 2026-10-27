//! Pipeline-shell integration over TCP: `MuxSender<TcpTransport>` → TCP →
//! `DemuxReceiver<TcpTransport>` recovers the `ProgramMap` (shared body in
//! `tst-pipeline/tests/support/round_trip.rs`), plus a cancel test for a
//! managed TCP receive against a silent peer.

#[path = "../../tst-pipeline/tests/support/round_trip.rs"]
mod round_trip;

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tst_pipeline::{
    BackoffStrategy, BrokenCause, ManagedRecvTransport, ReconnectPolicy, RecvTransport,
    TransportError,
};
use tst_tcp::{TcpListener, TcpTransport};

#[test]
fn mux_via_tcp_demux_round_trip_recovers_program_map() {
    // Bind first so the kernel picks the port; accept on the receive thread.
    let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    let port = listener.local_addr().unwrap().port();
    round_trip::assert_program_map_round_trip(
        move || listener.accept_blocking().expect("accept"),
        move || TcpTransport::connect(&format!("tcp://127.0.0.1:{port}")).expect("connect"),
        round_trip::LOOPBACK,
    );
}

/// A `ManagedRecvTransport<TcpTransport>` in `recv_bytes` against a real
/// (silent) TCP peer: a cancel from another thread ends the call with
/// `ExplicitClose`. The test does not prove the read was parked when the
/// cancel landed; a cancel before the read ends it the same way.
///
/// Without `TcpTransport::cancel_handle` wired through the trait-level
/// `Transport`/`RecvTransport` overrides, `ManagedRecvTransport` — which can
/// only reach the inner through `dyn RecvTransport` — got `None` back from
/// `inner.cancel_handle()` at construction and never installed a wake handle
/// in its `CancelSlot`. A `cancel()` afterwards latched the managed
/// wrapper's own flag but had nothing to fire against the parked inner read,
/// so the recv thread would never unblock.
///
/// The managed wrapper maps *every* caller-initiated cancel/close to
/// `TransportError::ExplicitClose` — regardless of what the inner transport
/// itself reports (a bare `TcpTransport::recv_bytes` returns `Closed` on
/// cancel; see `loopback.rs`'s `cancel_handle_unblocks_parked_recv`). See the
/// entry-gate + post-inner-call handling in
/// `tst_pipeline::managed_receive::ManagedRecvTransport::recv_bytes`.
#[test]
fn managed_tcp_recv_cancel_unblocks_parked_read() {
    // Silent peer: connect first (the kernel completes the handshake into
    // the listen backlog without anyone calling accept), then accept
    // synchronously and hold the peer socket in a local for the rest of the
    // test. No background thread and no timed sleep to keep it alive -- the
    // connection simply stays open, reading and writing nothing, so the
    // parked recv has no data/EOF/RST to react to and only an explicit
    // cancel can unblock it.
    let peer_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = peer_listener.local_addr().unwrap().port();

    let inner = TcpTransport::connect(&format!("tcp://127.0.0.1:{port}")).expect("connect");
    let (_peer_sock, _) = peer_listener.accept().expect("accept");

    // The factory is never actually invoked in this test: cancelling a
    // parked receive is caller-initiated and short-circuits to
    // ExplicitClose before any reconnect attempt is made. max_attempts: 0
    // plus a zero backoff keep the policy inert if that assumption is ever
    // wrong, rather than hanging the test on a real reconnect loop.
    let factory: Box<dyn FnMut() -> Result<TcpTransport, TransportError> + Send> = Box::new(|| {
        Err(TransportError::Broken {
            msg: "factory should not run in this test".into(),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        })
    });
    let policy = ReconnectPolicy {
        max_attempts: Some(0),
        backoff: BackoffStrategy::Constant(Duration::ZERO),
        ..Default::default()
    };

    let mut managed = ManagedRecvTransport::new(inner, factory, policy);

    // Obtain the cancel handle BEFORE moving `managed` into the receiving
    // thread -- this mirrors real usage, where a caller elsewhere holds the
    // handle while a separate loop owns the transport.
    let handle = managed
        .cancel_handle()
        .expect("ManagedRecvTransport::cancel_handle is unconditionally Some");

    let (tx, rx) = mpsc::channel::<Result<usize, TransportError>>();
    let recv_thread = thread::spawn(move || {
        let mut buf = vec![0u8; 1024];
        let result = managed.recv_bytes(&mut buf);
        let _ = tx.send(result);
    });

    // Give the recv thread time to park inside the inner TCP read.
    thread::sleep(Duration::from_millis(100));

    handle.cancel();

    let result = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("cancel did not unblock the parked managed receive within 1 s");
    recv_thread.join().unwrap();

    assert_eq!(
        result,
        Err(TransportError::ExplicitClose),
        "managed cancel of a parked TCP read must surface ExplicitClose"
    );
}
