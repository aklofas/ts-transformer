//! End-to-end loopback: tst_raw_sender_t (caller) ↔ tst_raw_receiver_t
//! (listener). Validates the byte-level path including the EOS code on
//! graceful peer disconnect.
//!
//! Uses the rlib output (crate-type includes "rlib") so this integration
//! test can call the crate's Rust API directly — the same `unsafe extern "C"`
//! functions exported to C consumers.
//!
//! Threading pattern:
//!   Receiver thread — calls `tst_raw_receiver_open_listener`, which binds
//!   the port and blocks on `accept()` until a peer arrives. Once the SRT
//!   handshake completes, `open_listener` returns the handle. The thread then
//!   signals `ready_tx` (peer is connected) and starts the recv loop.
//!
//!   Sender thread — retries `tst_raw_sender_open` in a 50 ms loop until the
//!   listener is ready and the connection is accepted (up to 5 s). Once
//!   connected, it waits for `ready_rx` (receiver has advanced past accept),
//!   sends 3 messages, then calls `_close`. The FIN triggers EOS on the
//!   receiver.

// Drives the SRT-backed raw sender/receiver pair.
#![cfg(feature = "srt")]

use std::ffi::CString;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tstrans::error::{TstError, tst_get_last_error_str};
use tstrans::receiver::raw_receiver::{
    tst_raw_receiver_close, tst_raw_receiver_open_listener, tst_raw_receiver_recv,
};
use tstrans::sender::raw_sender::{tst_raw_sender_close, tst_raw_sender_open, tst_raw_sender_send};

fn last_error_msg() -> String {
    unsafe {
        let p = tst_get_last_error_str();
        if p.is_null() {
            return "<null>".into();
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

#[test]
fn loopback_caller_sender_to_listener_receiver_delivers_bytes_and_eos() {
    // Pick a port in the ephemeral range. Use a per-pid offset to reduce
    // collisions when tests run concurrently or restart quickly (TIME_WAIT).
    let port: u16 = 27_000 + (std::process::id() as u16 % 1000);

    // ready_tx fires after open_listener unblocks (peer connected), telling
    // the sender it is safe to push messages. Without this gate the sender
    // could call _send before the receiver has entered _recv and the first
    // SRT data packet would sit in the libsrt receive buffer — that is fine
    // from the protocol standpoint, but the gate makes the test deterministic
    // about ordering in --nocapture output.
    let (ready_tx, ready_rx) = mpsc::channel::<()>();

    let receiver_thread = thread::spawn(move || {
        // Empty-host URL: tst_raw_receiver_open_listener accepts srt://:{port}
        // directly. The entry-point name is the authoritative listener-mode
        // signal; ?mode=listener in the URL is not required (and still works
        // if present). The empty host binds to 0.0.0.0 (wildcard), so all
        // loopback addresses are accepted.
        let url = CString::new(format!("srt://:{port}")).unwrap();

        // Blocks until a peer connects (bind + accept internally). The URL
        // has no host component — tst-c's listen helper treats an empty host
        // as 0.0.0.0 (wildcard bind), so all loopback addresses are accepted.
        let rx = unsafe { tst_raw_receiver_open_listener(url.as_ptr()) };
        if rx.is_null() {
            let msg = last_error_msg();
            panic!("tst_raw_receiver_open_listener failed: {msg}");
        }

        // Peer connected — unblock the sender to start sending messages.
        ready_tx.send(()).expect("ready channel dropped");

        // Receive exactly 3 messages.
        let mut received: Vec<Vec<u8>> = Vec::with_capacity(3);
        let mut buf = [0u8; 1500];
        for i in 0..3usize {
            let mut got: usize = 0;
            let rc = unsafe { tst_raw_receiver_recv(rx, buf.as_mut_ptr(), buf.len(), &mut got) };
            assert_eq!(
                rc,
                0,
                "recv[{i}] expected 0, got {rc}: {}",
                last_error_msg()
            );
            received.push(buf[..got].to_vec());
        }

        // After the sender calls _close the next recv must return EOS (-12).
        let mut got: usize = 0;
        let rc = unsafe { tst_raw_receiver_recv(rx, buf.as_mut_ptr(), buf.len(), &mut got) };
        assert_eq!(
            rc,
            TstError::EndOfStream as i32,
            "expected TST_E_END_OF_STREAM (-12), got {rc}: {}",
            last_error_msg()
        );

        unsafe { tst_raw_receiver_close(rx) };
        received
    });

    // Sender side: retry connect until the listener has bound and is ready to
    // accept. The listener thread starts bind + accept asynchronously, so the
    // first few connect attempts may fail with a connection-refused-equivalent.
    let sender_thread = thread::spawn(move || {
        let url = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);

        let tx = loop {
            let handle = unsafe { tst_raw_sender_open(url.as_ptr(), std::ptr::null()) };
            if !handle.is_null() {
                break handle;
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "tst_raw_sender_open timed out after 5s: {}",
                    last_error_msg()
                );
            }
            thread::sleep(Duration::from_millis(50));
        };

        // Wait until the receiver has advanced past accept so the ready signal
        // reflects actual readiness to recv, not just readiness to accept.
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("receiver did not signal ready within 5s");

        let msgs: &[&[u8]] = &[b"hello", b"world", b"goodbye"];
        for (i, m) in msgs.iter().enumerate() {
            let rc = unsafe { tst_raw_sender_send(tx, m.as_ptr(), m.len()) };
            assert_eq!(
                rc,
                0,
                "send[{i}] expected 0, got {rc}: {}",
                last_error_msg()
            );
        }

        // Drain pause before close — SRT's send queue is asynchronous with
        // respect to close, so closing immediately can drop in-flight packets
        // before the receiver's TSBPD (time-based packet delivery) releases
        // them. 1 s comfortably covers SRT's default 120 ms latency budget
        // plus loopback scheduling jitter on every platform.
        //
        // Darwin scheduling on Apple Silicon (macOS arm64 runners) pushes
        // event emission past a 200 ms window. Linux loopback tolerates the
        // smaller value but the extra headroom is platform-stable.
        thread::sleep(Duration::from_secs(1));

        // Closing the sender triggers libsrt's graceful shutdown sequence,
        // which the receiver observes as a ConnectionBroken (mapped to
        // TST_E_END_OF_STREAM at the C ABI layer).
        unsafe { tst_raw_sender_close(tx) };
    });

    let received = receiver_thread.join().expect("receiver thread panicked");
    sender_thread.join().expect("sender thread panicked");

    assert_eq!(
        received.len(),
        3,
        "expected 3 messages, got {}",
        received.len()
    );
    assert_eq!(&received[0], b"hello");
    assert_eq!(&received[1], b"world");
    assert_eq!(&received[2], b"goodbye");
}
