//! `tst_managed_demux_receiver_cancel` must wake a listener-mode managed
//! receiver whose reconnect is parked in the re-accept after its peer
//! disconnected. Without a cancellable re-accept the reader thread would sit
//! in `srt_accept` until a new peer showed up, silently ignoring `_cancel`.
//!
//! Choreography:
//!   1. Reader thread: `_open_listener` (blocks until a peer connects),
//!      then `_recv_event` in a loop until it returns non-zero.
//!   2. Main: connect a raw SRT caller, send a few TS null packets, close
//!      it — the managed receiver sees the break and re-enters the factory
//!      (bind + accept) after its first backoff.
//!   3. Main: `_cancel` from this thread; the reader must return
//!      `TST_E_CLOSED` within a couple of seconds.
//!
//! If the cancel does NOT wake the accept, a rescue peer is connected so
//! the reader thread can be joined and the test fails with a clear
//! message instead of hanging to nextest's kill.

#![cfg(feature = "srt")]

use std::ffi::{CStr, CString};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tstrans::error::{TstError, tst_get_last_error_str};
use tstrans::event::TstEvent;
use tstrans::receiver::demux_receiver::managed::{
    TstManagedDemuxReceiver, tst_managed_demux_receiver_cancel, tst_managed_demux_receiver_close,
    tst_managed_demux_receiver_open_listener, tst_managed_demux_receiver_recv_event,
};
use tstrans::sender::raw_sender::{tst_raw_sender_close, tst_raw_sender_open, tst_raw_sender_send};

/// The C ABI documents `_cancel` as callable from any thread; the raw
/// pointer just needs to cross the thread boundary to get there.
struct SendPtr(*mut TstManagedDemuxReceiver);
unsafe impl Send for SendPtr {}

fn last_error_msg() -> String {
    unsafe {
        let p = tst_get_last_error_str();
        if p.is_null() {
            return "<null>".into();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// Connect a raw SRT caller to the listener, retrying while it is not yet
/// bound (the listener re-binds between rounds).
fn connect_raw_caller(
    url: &CString,
    budget: Duration,
) -> *mut tstrans::sender::raw_sender::TstRawSender {
    let deadline = Instant::now() + budget;
    loop {
        let tx = unsafe { tst_raw_sender_open(url.as_ptr(), std::ptr::null()) };
        if !tx.is_null() {
            return tx;
        }
        if Instant::now() > deadline {
            panic!(
                "raw sender could not connect within {budget:?}: {}",
                last_error_msg()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn loopback_cancel_wakes_managed_listener_parked_in_reaccept() {
    // Own band (31_000) — see the sibling loopback tests for the others.
    let port: u16 = 31_000 + (std::process::id() as u16 % 1000);
    let listen_url = CString::new(format!("srt://:{port}")).unwrap();
    let caller_url = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();

    let (handle_tx, handle_rx) = mpsc::channel::<SendPtr>();
    let (done_tx, done_rx) = mpsc::channel::<(i32, Duration)>();

    let reader = thread::spawn(move || {
        // Blocks until the first peer connects.
        // NULL policy = library defaults (10 attempts, exponential 100 ms .. 10 s).
        let rx = unsafe {
            tst_managed_demux_receiver_open_listener(listen_url.as_ptr(), std::ptr::null())
        };
        if rx.is_null() {
            panic!("open_listener failed: {}", last_error_msg());
        }
        handle_tx.send(SendPtr(rx)).expect("handle channel");

        let started = Instant::now();
        let mut ev = TstEvent::default();
        let rc = loop {
            let rc = unsafe { tst_managed_demux_receiver_recv_event(rx, &mut ev) };
            if rc != 0 {
                break rc;
            }
        };
        done_tx.send((rc, started.elapsed())).expect("done channel");
        unsafe { tst_managed_demux_receiver_close(rx) };
    });

    // Step 2: first peer connects, sends, disconnects.
    let tx = connect_raw_caller(&caller_url, Duration::from_secs(5));
    let SendPtr(rx_ptr) = handle_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("reader never got past open_listener");
    let mut pkt = [0xFFu8; 188];
    pkt[0] = 0x47;
    pkt[1] = 0x1F;
    pkt[2] = 0xFF;
    pkt[3] = 0x10;
    for _ in 0..3 {
        let rc = unsafe { tst_raw_sender_send(tx, pkt.as_ptr(), pkt.len()) };
        assert_eq!(rc, 0, "raw send failed: {}", last_error_msg());
    }
    thread::sleep(Duration::from_millis(200));
    unsafe { tst_raw_sender_close(tx) };

    // Give the managed receiver time to notice the break, run its first
    // backoff (default policy: 100 ms) and park in the re-accept.
    thread::sleep(Duration::from_millis(1000));

    // Step 3: cancel from this thread.
    let t0 = Instant::now();
    let rc = unsafe { tst_managed_demux_receiver_cancel(rx_ptr) };
    assert_eq!(rc, 0, "_cancel returned {rc}: {}", last_error_msg());

    match done_rx.recv_timeout(Duration::from_secs(3)) {
        Ok((rc, _)) => {
            let woke_after = t0.elapsed();
            reader.join().expect("reader thread");
            assert_eq!(
                rc,
                TstError::Closed as i32,
                "expected TST_E_CLOSED after cancel, got {rc}"
            );
            assert!(
                woke_after < Duration::from_secs(2),
                "cancel took {woke_after:?} to wake the parked re-accept"
            );
        }
        Err(_) => {
            // Rescue: a new peer releases the accept so the reader can be
            // joined; the cancel already latched, so the reader then exits.
            let rescue = connect_raw_caller(&caller_url, Duration::from_secs(5));
            let _ = done_rx.recv_timeout(Duration::from_secs(5));
            unsafe { tst_raw_sender_close(rescue) };
            reader.join().expect("reader thread");
            panic!("_cancel did not wake the managed listener parked in re-accept within 3 s");
        }
    }
}
