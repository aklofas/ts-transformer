//! A zero-length datagram — `nc -zu`, a NAT "UDP ping", an empty keepalive
//! from a misconfigured peer — must be skipped: UDP has no end of stream.
//! If `UdpRecvTransport::recv_bytes` returned `Ok(0)` for it, the receive
//! shells would read that as "closed" and one packet from any host that
//! could reach the port would end a `Receiver` / `DemuxReceiver` with
//! `EndOfStream` (terminal on the managed wrappers too).

use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tst_core::transport::{RecvTransport, TransportError};
use tst_pipeline::{Receiver, ReceiverConfig};
use tst_udp::UdpRecvTransport;

/// One null TS packet (PID 0x1FFF) — enough for the Syncer when repeated.
fn null_ts_packet() -> [u8; 188] {
    let mut p = [0xFFu8; 188];
    p[0] = 0x47;
    p[1] = 0x1F;
    p[2] = 0xFF;
    p[3] = 0x10;
    p
}

#[test]
fn an_empty_datagram_is_skipped_by_recv_bytes() {
    let mut recv = UdpRecvTransport::listen("udp://@127.0.0.1:0").expect("bind");
    let addr = recv.local_addr();
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.send_to(&[], addr).unwrap(); // the probe
    s.send_to(&null_ts_packet(), addr).unwrap();
    let mut buf = [0u8; 65535];
    assert_eq!(
        recv.recv_bytes(&mut buf),
        Ok(188),
        "the empty datagram must be skipped, not reported as Ok(0)"
    );
    assert_eq!(buf[0], 0x47);
}

#[test]
fn an_empty_datagram_mid_stream_does_not_end_a_receiver_shell() {
    let t = UdpRecvTransport::listen("udp://@127.0.0.1:0").expect("bind");
    let addr = t.local_addr();
    // Safety bound: a lost loopback datagram would park `next_packet`
    // forever. A watchdog cancels the transport after a generous 30 s so the
    // test FAILS with a clear message instead of hanging to the runner
    // timeout (a bound, not a speed assertion).
    let handle = t.cancel_handle();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let fired = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let fired = Arc::clone(&fired);
        thread::spawn(move || {
            if done_rx.recv_timeout(Duration::from_secs(30)).is_err() {
                fired.store(true, Ordering::SeqCst);
                handle.cancel();
            }
        })
    };
    let mut rx = Receiver::new(t, ReceiverConfig::default());
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    let five: Vec<u8> = null_ts_packet().repeat(5); // Syncer: 4 confirming sync bytes
    s.send_to(&five, addr).unwrap();
    let first = rx.next_packet();
    assert!(
        !fired.load(Ordering::SeqCst),
        "no first packet within the 30 s safety bound (lost datagram?)"
    );
    assert!(first.is_ok(), "stream established");
    s.send_to(&[], addr).unwrap(); // the probe, mid-stream
    s.send_to(&five, addr).unwrap();
    // The Syncer still buffers (most of) the first burst, so drain past it:
    // 8 more packets forces a `recv_bytes` that meets the empty datagram
    // whether the Syncer emitted 4 or all 5 of the first burst.
    for i in 0..8 {
        if let Err(e) = rx.next_packet() {
            assert!(
                !fired.load(Ordering::SeqCst),
                "no packet {i} within the 30 s safety bound (lost datagram?)"
            );
            panic!("an empty datagram must not end the stream (packet {i}): {e}");
        }
    }
    done_tx.send(()).unwrap();
    watchdog.join().unwrap();
}

/// Empty datagrams are skipped, and a cancel that lands while
/// only empty datagrams arrive is still observed — the skip loops back
/// through the flag checks. Two phases so no assertion depends on scheduling.
#[test]
fn empty_datagrams_do_not_hide_a_cancel() {
    let mut recv = UdpRecvTransport::listen("udp://@127.0.0.1:0").expect("bind");
    let addr = recv.local_addr();
    let handle = recv.cancel_handle();
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();

    // Part 1 — the skip path, deterministically: one loopback socket pair
    // delivers in FIFO order, so the first `recv_bytes` must skip the three
    // empties and return the sentinel. Queued before the worker starts.
    for _ in 0..3 {
        s.send_to(&[], addr).unwrap();
    }
    s.send_to(&[0x47u8; 188], addr).unwrap(); // the sentinel

    let (first_tx, first_rx) = mpsc::channel();
    let (entering_tx, entering_rx) = mpsc::channel::<()>();
    let (second_tx, second_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut buf = [0u8; 65535];
        let first = recv.recv_bytes(&mut buf);
        // `recv_bytes` has returned, so the stats are settled.
        first_tx
            .send((first, recv.stats().datagrams_received))
            .unwrap();
        entering_tx.send(()).unwrap(); // about to park in the second recv
        second_tx.send(recv.recv_bytes(&mut buf)).unwrap();
    });

    let (first, datagrams_received) = first_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("first recv_bytes did not return within the 30 s safety bound");
    assert_eq!(first, Ok(188), "the three empties must be skipped");
    assert_eq!(
        datagrams_received, 4,
        "3 skipped empties + 1 delivered sentinel"
    );

    // Part 2 — empties do not hide a cancel. Nothing here counts on timing:
    // if the skip loop stopped re-checking the cancel flag, the second
    // `recv_bytes` would never return and the bounded wait below fails.
    entering_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("worker never reached the second recv_bytes within the 30 s safety bound");
    let stop = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicUsize::new(0));
    let prober = {
        let (stop, sent) = (Arc::clone(&stop), Arc::clone(&sent));
        thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                s.send_to(&[], addr).unwrap();
                sent.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while sent.load(Ordering::SeqCst) < 5 {
        assert!(
            Instant::now() < deadline,
            "prober sent fewer than 5 probes within the 30 s safety bound"
        );
        thread::sleep(Duration::from_millis(1));
    }
    handle.cancel();
    let r = second_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("recv_bytes did not return after the cancel within the 30 s safety bound");
    stop.store(true, Ordering::SeqCst);
    assert!(matches!(r, Err(TransportError::ExplicitClose)), "got {r:?}");
    worker.join().unwrap();
    prober.join().unwrap();
}
