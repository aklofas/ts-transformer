//! Passphrase-encrypted SRT send + recv in one process.
//!
//! Spawns a listener thread on 127.0.0.1, then a sender thread that connects
//! with the matching passphrase. Sends 16 messages, receives them all,
//! verifies bytes match. Exits cleanly.
//!
//!   cargo run -p tst-examples --example encrypted_send_recv
//!
//! Demonstrates: SocketBuilder/ListenerBuilder + Passphrase + KeyLength +
//! StreamId. The same shape applies across-network — only the bind/connect
//! addresses change.

use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tst_srt::{KeyLength, ListenerBuilder, Passphrase, SocketBuilder, StreamId};

// PASSPHRASE is the shared secret both peers must agree on for AES-CTR
// encryption. Named `shared-secret-not-for-production` so anyone reading the
// example or scanning for hardcoded secrets immediately sees it isn't a real
// credential. Real deployments load passphrases from env (`Passphrase::from_env`)
// or a key file (`Passphrase::from_file`).
//
// STREAM_ID is the caller's `SRTO_STREAMID` — an opaque identifier the listener
// can read post-accept to route or authorize the connection (multi-publisher
// servers use this; the field is also where the SRT Access Control URI scheme
// lives if a deployment needs it).
//
// NUM_MESSAGES is small on purpose — the example is a smoke test, not a
// throughput demo.
const PASSPHRASE: &str = "shared-secret-not-for-production";
const STREAM_ID: &str = "encrypted-demo";
const NUM_MESSAGES: usize = 16;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Pick a free ephemeral port by briefly binding a TCP listener on port 0
    // (the kernel picks an unused port), reading the assigned port, and
    // dropping the listener. SRT runs over UDP, so the TCP listener doesn't
    // conflict with the SRT bind that follows — we're only using TCP because
    // its `local_addr()` after `bind(0)` is the standard way to ask the
    // kernel for a free port. There's a small TOCTOU race between drop and
    // the SRT bind, but on a single dev machine it's effectively zero.
    let port = {
        let l = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
        let p = l.local_addr()?.port();
        drop(l);
        p
    };
    let bind_addr = format!("127.0.0.1:{port}");
    let connect_addr = bind_addr.clone();

    // `ready_tx` is a one-shot handshake: the listener thread signals once
    // its `bind()` has returned, so the sender doesn't try to connect to a
    // socket that doesn't yet exist. `done_tx` carries the final received
    // count back to main so we can assert end-to-end delivery.
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<usize>();

    // Listener thread: owns the bound SRT socket, accepts one peer, drains
    // messages until either NUM_MESSAGES arrive or the peer closes. Run on
    // its own thread because `accept()` and `recv()` block; we want the
    // sender's `connect()` to run concurrently. The closure returns
    // `Result<(), Box<dyn Error + Send + Sync>>` rather than the bare
    // `Box<dyn Error>` main uses — `Send + Sync` is required to move the
    // error across the thread boundary in `join()`.
    let listener_handle = thread::spawn(
        move || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            // `Passphrase::new` validates: 10–79 ASCII-printable bytes (libsrt's
            // own constraint). The constant above sits comfortably inside that.
            let passphrase = Passphrase::new(PASSPHRASE)?;
            // Bind-then-step shape (`ListenerBuilder` is `&mut self -> &mut Self`).
            let mut lb = ListenerBuilder::new();
            lb.passphrase(passphrase);
            // AES-256 is overkill for 16 short messages but mirrors what a
            // production deployment would pick; switching the constant to
            // `Aes128` or `Aes192` is a one-line change.
            lb.key_length(KeyLength::Aes256);
            lb.latency(Duration::from_millis(120));
            let mut listener = lb.bind(bind_addr.as_str())?;

            // Bind succeeded — wake the main thread so it can launch the sender.
            // `.ok()` because if the receiver was dropped we don't care: we'll
            // discover the failure via the next `accept()` or `recv()`.
            ready_tx.send(()).ok();

            // Blocking accept. Returns when the sender's connect handshake
            // completes — and that handshake includes the SRT key-material
            // exchange (KMREQ/KMRSP), so a successful `accept` means encryption
            // is already negotiated. A passphrase mismatch would surface here as
            // an error instead.
            let (mut socket, peer) = listener.accept()?;
            // Once accepted, the listener can read the caller's stream ID — this
            // is the natural place for routing or per-stream authorization in a
            // real server (look up the publisher, attach a recording sink, etc.).
            eprintln!(
                "listener: accepted from {peer}, stream_id={:?}",
                socket.stream_id()
            );

            // 1500 bytes is comfortably above SRT's default payload size of 1316
            // (`SRTO_PAYLOADSIZE`), so each `recv` returns a whole message.
            let mut buf = [0u8; 1500];
            let mut received = 0usize;
            loop {
                // Same three-arm pattern as in `srt_listener_to_file`: Ok counts
                // a message, ConnectionBroken is the canonical close signal,
                // TimedOut is defensive (no recv timeout is set so it shouldn't
                // fire), other errors are fatal.
                match socket.recv(&mut buf) {
                    Ok(n) => {
                        eprintln!("listener: recv {n} bytes (msg {received})");
                        received += 1;
                        if received >= NUM_MESSAGES {
                            break;
                        }
                    }
                    Err(tst_srt::error::RecvError::ConnectionBroken) => break,
                    Err(tst_srt::error::RecvError::TimedOut) => continue,
                    Err(e) => return Err(Box::new(e)),
                }
            }
            done_tx.send(received).ok();
            Ok(())
        },
    );

    // Belt-and-suspenders. `ready_rx.recv()` already proves bind() returned;
    // the brief sleep gives the kernel a tick to start servicing UDP on the
    // listening socket before the sender's first handshake datagram lands.
    // On a fast loopback this is paranoia, but it removes a class of flaky
    // test failures we don't want anyone to hit.
    ready_rx.recv()?;
    thread::sleep(Duration::from_millis(50));

    // MuxSender thread (could be inline; threaded to mirror real deployments).
    let sender_handle = thread::spawn(
        move || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            let passphrase = Passphrase::new(PASSPHRASE)?;
            let stream_id = StreamId::new(STREAM_ID)?;
            // Bind-then-step shape (`SocketBuilder` is `&mut self -> &mut Self`).
            let mut sb = SocketBuilder::new();
            sb.passphrase(passphrase);
            sb.key_length(KeyLength::Aes256);
            sb.stream_id(stream_id);
            sb.latency(Duration::from_millis(120));
            let mut socket = sb.connect(connect_addr.as_str())?;

            // 20 ms cadence is deliberately slow so the listener's per-message
            // log line is human-readable as the example runs. A real publisher
            // would push messages back-to-back and let SRT's pacing layer
            // handle wire-rate shaping — pacing in the application layer
            // is the wrong place to do it.
            for i in 0..NUM_MESSAGES {
                let msg = format!("encrypted message {i:02}").into_bytes();
                socket.send(&msg)?;
                thread::sleep(Duration::from_millis(20));
            }
            eprintln!("sender: sent {NUM_MESSAGES} messages");
            // Give the receiver a beat to drain in-flight packets before close.
            // SRT's latency window (120 ms here) covers retransmits inside that
            // budget, but `close()` tears the link down promptly — sleeping more
            // than `latency` lets the tail of the stream get ACKed and delivered
            // before the socket goes away. Without this, the close can race the
            // last few datagrams and the listener sees fewer messages than sent.
            thread::sleep(Duration::from_millis(200));
            socket.close();
            Ok(())
        },
    );

    // Each `.join()` returns `Result<Result<(), BoxedSendSyncError>, JoinError>`.
    // The outer Result is the panic-or-not from the thread; the inner Result
    // is the closure's own Result. The `.map_err(|e| -> Box<dyn Error> { e })`
    // coerces the inner `Box<dyn Error + Send + Sync>` into the plain
    // `Box<dyn Error>` that main returns — `?` does not auto-coerce between
    // those two trait-object types because the marker bounds differ.
    sender_handle
        .join()
        .expect("sender thread")
        .map_err(|e| -> Box<dyn std::error::Error> { e })?;
    listener_handle
        .join()
        .expect("listener thread")
        .map_err(|e| -> Box<dyn std::error::Error> { e })?;
    // Bounded wait on the final count. If the listener thread silently
    // dropped its `done_tx` (e.g. crashed before send), an unbounded
    // `recv()` would deadlock the test forever; the timeout converts that
    // into a clean error.
    let received = done_rx.recv_timeout(Duration::from_secs(2))?;
    assert_eq!(
        received, NUM_MESSAGES,
        "received {received} of {NUM_MESSAGES}"
    );

    println!("OK: {NUM_MESSAGES} encrypted messages round-tripped");
    Ok(())
}
