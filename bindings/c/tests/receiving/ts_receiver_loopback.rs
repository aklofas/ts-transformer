//! End-to-end loopback: tst_sender_t (caller, TS-framed) ↔
//! tst_receiver_t (listener). Validates the aligned-packet path,
//! including byte-for-byte equality between the sent TS bytes and
//! the received packets, plus the EOS code on graceful peer
//! disconnect.
//!
//! Uses the rlib output so this integration test can call the crate's
//! Rust API directly — the same `unsafe extern "C"` functions exported
//! to C consumers.
//!
//! Threading pattern mirrors raw_receiver_loopback.rs exactly: the
//! receiver thread binds + accepts, signals readiness via mpsc, then
//! enters recv_packet loop; the sender thread retries connect until
//! ready, sends a synthetic aligned TS stream, then closes.

// Drives the SRT-backed TS sender/receiver pair.
#![cfg(feature = "srt")]

use std::ffi::CString;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tstrans::config::{tst_sender_config_free, tst_sender_config_new};
use tstrans::error::{TstError, tst_get_last_error_str};
use tstrans::receiver::ts_receiver::{
    tst_receiver_close, tst_receiver_open_listener, tst_receiver_recv_packet,
};
use tstrans::sender::ts_sender::{tst_sender_close, tst_sender_open, tst_sender_send_ts};

fn last_error_msg() -> String {
    unsafe {
        let p = tst_get_last_error_str();
        if p.is_null() {
            return "<null>".into();
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// Build a synthetic aligned TS stream of `n` packets. Each packet
/// starts with the 0x47 sync byte at offset 0 and has its second-byte
/// payload set to the packet index modulo 256 so per-packet identity
/// can be asserted in the receiver loop.
fn synthetic_ts(n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n * 188);
    for i in 0..n {
        out.push(0x47);
        out.push((i & 0xff) as u8);
        out.extend(std::iter::repeat_n(0u8, 186));
    }
    out
}

#[test]
fn loopback_ts_sender_to_ts_receiver_delivers_aligned_packets_and_eos() {
    // Pick a port in the ephemeral range, offset by pid to reduce
    // collisions when tests run concurrently or restart quickly.
    let port: u16 = 28_000 + (std::process::id() as u16 % 1000);

    // ready_tx fires after open_listener unblocks (peer accepted),
    // telling the sender it can start sending.
    let (ready_tx, ready_rx) = mpsc::channel::<()>();

    // 28 packets (4 × 7-packet bundles) is enough to exercise the syncer
    // past its VERIFY threshold and several follow-on packets. Using an
    // exact multiple of the Sender's 7-packet bundle size ensures all
    // packets are sent during send_ts (no partial bundle held until
    // close), so the 200 ms drain window reliably covers in-flight data.
    // Keep this an exact multiple of 7 — a remainder leaves a partial
    // bundle in-flight during the close/drain window, which can race with
    // the EOS signal on slower CI hosts.
    const N_PACKETS: usize = 28;

    let receiver_thread = thread::spawn(move || {
        let url = CString::new(format!("srt://:{port}")).unwrap();
        let rx = unsafe { tst_receiver_open_listener(url.as_ptr()) };
        if rx.is_null() {
            let msg = last_error_msg();
            panic!("tst_receiver_open_listener failed: {msg}");
        }

        // Peer connected — unblock the sender to start sending.
        ready_tx.send(()).expect("ready channel dropped");

        // Receive exactly N_PACKETS packets, asserting each one.
        let mut received: Vec<[u8; 188]> = Vec::with_capacity(N_PACKETS);
        let mut buf = [0u8; 188];
        for i in 0..N_PACKETS {
            let rc = unsafe { tst_receiver_recv_packet(rx, buf.as_mut_ptr()) };
            assert_eq!(
                rc,
                0,
                "recv_packet[{i}] expected 0, got {rc}: {}",
                last_error_msg()
            );
            assert_eq!(buf[0], 0x47, "packet[{i}] missing sync byte");
            received.push(buf);
        }

        // After the sender calls _close the next recv_packet must
        // return EOS (-12) — graceful peer disconnect.
        let rc = unsafe { tst_receiver_recv_packet(rx, buf.as_mut_ptr()) };
        assert_eq!(
            rc,
            TstError::EndOfStream as i32,
            "expected TST_E_END_OF_STREAM (-12), got {rc}: {}",
            last_error_msg()
        );

        unsafe { tst_receiver_close(rx) };
        received
    });

    let sender_thread = thread::spawn(move || {
        let url = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();
        let cfg = unsafe { tst_sender_config_new() };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);

        let tx = loop {
            let handle = unsafe { tst_sender_open(url.as_ptr(), cfg) };
            if !handle.is_null() {
                break handle;
            }
            if std::time::Instant::now() > deadline {
                unsafe { tst_sender_config_free(cfg) };
                panic!("tst_sender_open timed out after 5s: {}", last_error_msg());
            }
            thread::sleep(Duration::from_millis(50));
        };

        // Wait for the receiver to be past accept, so the ready
        // signal reflects readiness to recv, not just to accept.
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("receiver did not signal ready within 5s");

        // Send the entire synthetic stream as one send_ts call. The
        // sender's TS framing layer slices it back into 188-byte
        // packets on the wire — exactly what the receiver expects.
        let stream = synthetic_ts(N_PACKETS);
        let rc = unsafe { tst_sender_send_ts(tx, stream.as_ptr(), stream.len()) };
        assert_eq!(rc, 0, "send_ts expected 0, got {rc}: {}", last_error_msg());

        // Drain pause before close — SRT's send queue is asynchronous
        // with respect to close. 1 s comfortably covers SRT's default
        // 120 ms latency budget plus loopback scheduling jitter on every
        // platform.
        //
        // Darwin scheduling on Apple Silicon pushes timing past a 200 ms
        // window. Linux loopback tolerates the smaller value but the extra
        // headroom is platform-stable.
        thread::sleep(Duration::from_secs(1));

        unsafe { tst_sender_close(tx) };
        unsafe { tst_sender_config_free(cfg) };
    });

    let received = receiver_thread.join().expect("receiver thread panicked");
    sender_thread.join().expect("sender thread panicked");

    assert_eq!(
        received.len(),
        N_PACKETS,
        "expected {N_PACKETS} packets, got {}",
        received.len()
    );

    // Per-packet identity check — the second byte is the index modulo
    // 256, set by synthetic_ts. Confirms aligned delivery in order.
    for (i, pkt) in received.iter().enumerate() {
        assert_eq!(pkt[0], 0x47, "packet[{i}] missing sync byte");
        assert_eq!(
            pkt[1],
            (i & 0xff) as u8,
            "packet[{i}] payload byte mismatch — out of order or corrupted"
        );
    }
}
