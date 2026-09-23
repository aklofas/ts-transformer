//! `tst_udp_receiver_close` from another thread must end a `_recv_ts`
//! parked on the UDP poll loop with `TST_E_CLOSED` (Arc 2 WP-D: the UDP
//! transport now has a real cancel handle, and `Owned::close` fires it
//! before taking the slot). Before WP-D the handle's cancel slot held
//! `binding::FlagCancel` — a latch the transport never read — so a
//! cross-thread `_close` blocked on the handle mutex until a datagram
//! arrived.
//!
//! Why closing from another thread is safe here, even though `_close`
//! FREES the handle: `Owned::close` cancels (lock-free) and then `take()`s
//! the slot, which BLOCKS until the parked `_recv_ts` releases it. The free
//! therefore strictly follows the reader's last access to the handle.
//!
//! Failure is bounded and NEVER hangs. If `_close` does not unblock the
//! reader, a rescue burst is sent so the reader returns, the closer
//! completes its take, and both threads are joined before the test FAILS
//! with a message. The burst is 16 packets in ONE datagram, not one packet:
//! `Receiver` feeds bytes through the TS syncer, which "locks after four
//! aligned packets — the candidate `0x47` plus three confirmations"
//! (`crates/tst-pipeline/src/receiver/mod.rs`), so a single 188-byte
//! datagram would leave the reader parked and `join()` would hang. If even
//! the burst does not free it, the test panics WITHOUT joining (both
//! threads are detached and the harness ends the process) — a failing test
//! must fail, never wedge.
#![cfg(feature = "udp")]

use std::ffi::CString;
use std::net::UdpSocket;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tstrans::error::TstError;
use tstrans::udp::{
    TstUdpReceiver, tst_udp_receiver_close, tst_udp_receiver_recv_ts, tst_udp_recv_open,
};

/// `_close` is documented callable from any thread; the raw pointer just
/// needs to cross the thread boundary to get there.
struct SendPtr(*mut TstUdpReceiver);
unsafe impl Send for SendPtr {}

/// One MPEG-TS null packet (PID 0x1FFF) — what the rescue burst is made of.
fn ts_null_packet() -> [u8; 188] {
    let mut p = [0xFFu8; 188];
    p[0] = 0x47;
    p[1] = 0x1F;
    p[2] = 0xFF;
    p[3] = 0x10;
    p
}

/// Discover a free loopback port the way the Python suite does: bind :0,
/// read the port, drop — then open the C receiver on it (the C ABI has no
/// local-port getter on this handle).
fn free_port() -> u16 {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.local_addr().unwrap().port()
}

#[test]
fn udp_receiver_close_from_other_thread_loopback_unblocks_parked_recv_ts() {
    let port = free_port();
    let url = CString::new(format!("udp://127.0.0.1:{port}")).unwrap();
    let rx = unsafe { tst_udp_recv_open(url.as_ptr()) };
    assert!(!rx.is_null(), "tst_udp_recv_open failed");

    let (tx, done) = mpsc::channel::<i32>();
    let reader_ptr = SendPtr(rx);
    let reader = thread::spawn(move || {
        let p = reader_ptr; // whole-struct capture: SendPtr is Send, its field is not
        let mut buf = vec![0u8; 188];
        let mut n = 0usize;
        let rc = unsafe { tst_udp_receiver_recv_ts(p.0, buf.as_mut_ptr(), buf.len(), &mut n) };
        let _ = tx.send(rc);
    });
    thread::sleep(Duration::from_millis(300)); // reader is parked on a poll tick

    // The cross-thread close: must return promptly (cancel-first) and end
    // the parked recv with TST_E_CLOSED.
    let closer_ptr = SendPtr(rx);
    let closer = thread::spawn(move || {
        let p = closer_ptr;
        unsafe { tst_udp_receiver_close(p.0) };
    });

    let rc = match done.recv_timeout(Duration::from_secs(5)) {
        Ok(rc) => rc,
        Err(_) => {
            // Rescue: enough packets for the syncer to lock, so the parked
            // recv returns and releases the slot the closer is blocked on.
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut burst = Vec::with_capacity(16 * 188);
            for _ in 0..16 {
                burst.extend_from_slice(&ts_null_packet());
            }
            let _ = s.send_to(&burst, ("127.0.0.1", port));
            if done.recv_timeout(Duration::from_secs(5)).is_ok() {
                let _ = reader.join();
                let _ = closer.join();
            }
            // If the rescue did not free it either, both threads stay
            // detached: fail loudly rather than wedge in join().
            panic!(
                "tst_udp_receiver_close from another thread did not unblock _recv_ts within 5 s"
            );
        }
    };
    reader.join().unwrap();
    closer.join().unwrap();
    assert_eq!(
        rc,
        TstError::Closed as i32,
        "a cross-thread close must surface as TST_E_CLOSED (-7), got {rc}"
    );
}
