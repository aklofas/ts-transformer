//! Cancel-first characterization for every C shell that has a `_cancel`:
//! the op that observes a cross-thread cancel returns a specific code, and
//! it returns PROMPTLY (10 s watchdog that FAILS, never hangs).
//!
//! Expected codes on this PR (WP-B1):
//!   managed SRT (send + recv) ....... TST_E_CLOSED (-7)   — ManagedRecv/ManagedTransport latch
//!   plain SRT (send + recv) ......... TST_E_TRANSPORT (-8) — libsrt reports the closed socket as
//!                                     Broken (tst-srt/src/transport.rs); WP-C2 (PR 8) makes it
//!                                     ExplicitClose and flips PLAIN_SRT_CANCEL_CODE to -7.
//!   rtp (see transports/rtp_cancel_first.rs) ... TST_E_CLOSED (-7) already.
//! Own port band: 32_000.

#![cfg(feature = "srt")]

use std::ffi::{CStr, CString};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tst_srt::ListenerBuilder;
use tstrans::error::{TstError, tst_get_last_error_str};

/// Plain-SRT code observed by the op that a cross-thread cancel interrupts.
/// WP-C2 tightens this to `TstError::Closed` — change ONLY this constant.
pub(crate) const PLAIN_SRT_CANCEL_CODE: i32 = TstError::Transport as i32;
pub(crate) const MANAGED_SRT_CANCEL_CODE: i32 = TstError::Closed as i32;
pub(crate) const WATCHDOG: Duration = Duration::from_secs(10);

pub(crate) fn last_error_msg() -> String {
    unsafe {
        let p = tst_get_last_error_str();
        if p.is_null() {
            return "<null>".into();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// A fresh listener port for each `parked_recv_cancel` call.
///
/// The six parked-recv tests run concurrently in ONE process under
/// `cargo test` (nextest gives each its own process, `cargo test` does
/// not), so a single per-process port would collide. Band 32_000 +
/// pid-derived offset, then one port per call.
pub(crate) fn port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let base = 32_000 + (std::process::id() as u16 % 100) * 10;
    base + NEXT.fetch_add(1, Ordering::SeqCst)
}

/// A tst-srt listener on its own thread that accepts ONE peer and holds
/// the connection open until dropped. Teardown runs on every exit path:
/// drop the hold channel, fire the listener's cancel handle (wakes a
/// parked accept — libsrt's GC can prune a fast-closing peer from the
/// accept queue, see `url_open/demux_receiver.rs`), join.
pub(crate) struct PeerListener {
    pub(crate) port: u16,
    hold: Option<mpsc::Sender<()>>,
    cancel: tst_srt::SrtCancelHandle,
    thread: Option<thread::JoinHandle<()>>,
}

impl PeerListener {
    pub(crate) fn spawn() -> Self {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        let cancel = listener.cancel_handle();
        let (hold_tx, hold_rx) = mpsc::channel::<()>();
        let thread = thread::spawn(move || {
            let _accepted = listener.accept().ok();
            hold_rx.recv_timeout(WATCHDOG + Duration::from_secs(5)).ok();
        });
        Self {
            port,
            hold: Some(hold_tx),
            cancel,
            thread: Some(thread),
        }
    }
}

impl Drop for PeerListener {
    fn drop(&mut self) {
        drop(self.hold.take());
        self.cancel.cancel();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Raw pointer crossing to the cancelling thread — `_cancel` is documented
/// callable from any thread.
pub(crate) struct SendPtr<T>(pub(crate) *mut T);
unsafe impl<T> Send for SendPtr<T> {}

/// Wait for the parked op's result with the watchdog; `rescue` runs when
/// it expires so the parked thread can be joined instead of leaked.
pub(crate) fn wait_result(
    rx: &mpsc::Receiver<(i32, String)>,
    rescue: impl FnOnce(),
) -> Option<(i32, String)> {
    match rx.recv_timeout(WATCHDOG) {
        Ok(rc) => Some(rc),
        Err(_) => {
            rescue();
            None
        }
    }
}

fn seven_null_packets() -> Vec<u8> {
    let mut pkt = vec![0u8; 188 * 7];
    for i in 0..7 {
        pkt[i * 188] = 0x47;
        pkt[i * 188 + 1] = 0x1F;
        pkt[i * 188 + 2] = 0xFF;
        pkt[i * 188 + 3] = 0x10;
    }
    pkt
}

// ------------------------------------------------------------------ senders

#[test]
fn plain_sender_cancel_then_send_reports_plain_srt_cancel_code() {
    use tstrans::config::{tst_sender_config_free, tst_sender_config_new};
    use tstrans::sender::ts_sender::{
        tst_sender_cancel, tst_sender_close, tst_sender_flush, tst_sender_open, tst_sender_send_ts,
    };
    let peer = PeerListener::spawn();
    let url = CString::new(format!("srt://127.0.0.1:{}", peer.port)).unwrap();
    let cfg = unsafe { tst_sender_config_new() };
    let tx = unsafe { tst_sender_open(url.as_ptr(), cfg) };
    unsafe { tst_sender_config_free(cfg) };
    assert!(!tx.is_null(), "open: {}", last_error_msg());
    let p = SendPtr(tx);
    let canceller = thread::spawn(move || {
        let p = p; // whole-struct capture: SendPtr is Send, its field is not
        let SendPtr(tx) = p;
        unsafe { tst_sender_cancel(tx) }
    });
    assert_eq!(canceller.join().unwrap(), 0, "cancel rc");
    let pkts = seven_null_packets();
    // One full 7-packet bundle forces a transmit; a partial bundle would
    // only be buffered (see ts_receiver_loopback.rs).
    let mut rc = unsafe { tst_sender_send_ts(tx, pkts.as_ptr(), pkts.len()) };
    if rc == 0 {
        rc = unsafe { tst_sender_flush(tx) };
    }
    assert_eq!(
        rc,
        PLAIN_SRT_CANCEL_CODE,
        "after cancel: {}",
        last_error_msg()
    );
    unsafe { tst_sender_close(tx) };
    drop(peer);
}

#[test]
fn plain_raw_sender_cancel_then_send_reports_plain_srt_cancel_code() {
    use tstrans::sender::raw_sender::{
        tst_raw_sender_cancel, tst_raw_sender_close, tst_raw_sender_open, tst_raw_sender_send,
    };
    let peer = PeerListener::spawn();
    let url = CString::new(format!("srt://127.0.0.1:{}", peer.port)).unwrap();
    let tx = unsafe { tst_raw_sender_open(url.as_ptr(), std::ptr::null()) };
    assert!(!tx.is_null(), "open: {}", last_error_msg());
    let p = SendPtr(tx);
    assert_eq!(
        thread::spawn(move || {
            let p = p; // whole-struct capture: SendPtr is Send, its field is not
            let SendPtr(tx) = p;
            unsafe { tst_raw_sender_cancel(tx) }
        })
        .join()
        .unwrap(),
        0
    );
    let pkts = seven_null_packets();
    let rc = unsafe { tst_raw_sender_send(tx, pkts.as_ptr(), 188) };
    assert_eq!(
        rc,
        PLAIN_SRT_CANCEL_CODE,
        "after cancel: {}",
        last_error_msg()
    );
    unsafe { tst_raw_sender_close(tx) };
    drop(peer);
}

#[test]
fn plain_mux_sender_cancel_then_push_reports_plain_srt_cancel_code() {
    use tstrans::config::{
        TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
        tst_mux_config_free, tst_mux_config_new,
    };
    use tstrans::sender::mux_sender::{
        tst_mux_sender_cancel, tst_mux_sender_close, tst_mux_sender_open, tst_mux_sender_send_video,
    };
    let peer = PeerListener::spawn();
    let url = CString::new(format!("srt://127.0.0.1:{}", peer.port)).unwrap();
    let (tx, cfg) = unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        (tst_mux_sender_open(url.as_ptr(), cfg), cfg)
    };
    assert!(!tx.is_null(), "open: {}", last_error_msg());
    unsafe { tst_mux_config_free(cfg) };
    let p = SendPtr(tx);
    assert_eq!(
        thread::spawn(move || {
            let p = p; // whole-struct capture: SendPtr is Send, its field is not
            let SendPtr(tx) = p;
            unsafe { tst_mux_sender_cancel(tx) }
        })
        .join()
        .unwrap(),
        0
    );
    // An IDR-sized AU (> 7 TS packets) so the muxer emits a full bundle
    // and the transport is asked to send.
    let mut au = vec![0u8; 4096];
    au[..5].copy_from_slice(&[0, 0, 0, 1, 0x65]);
    let rc = unsafe { tst_mux_sender_send_video(tx, au.as_ptr(), au.len(), 90_000, true) };
    assert_eq!(
        rc,
        PLAIN_SRT_CANCEL_CODE,
        "after cancel: {}",
        last_error_msg()
    );
    unsafe { tst_mux_sender_close(tx) };
    drop(peer);
}

// ---------------------------------------------------------------- receivers

/// Park `recv` (a closure that runs the blocking C call) on a reader
/// thread, cancel from this thread, return the code the reader observed.
/// `open` runs on the reader thread too (listener-mode opens block until a
/// peer connects); the peer is a tst-srt caller `Socket` held open.
fn parked_recv_cancel<T: 'static>(
    open: impl FnOnce(u16) -> *mut T + Send + 'static,
    recv: impl Fn(*mut T) -> i32 + Send + 'static,
    cancel: impl FnOnce(*mut T) -> i32,
    close: impl FnOnce(*mut T),
) -> (i32, String) {
    let port = port();
    let (handle_tx, handle_rx) = mpsc::channel::<SendPtr<T>>();
    let (rc_tx, rc_rx) = mpsc::channel::<(i32, String)>();
    let reader = thread::spawn(move || {
        let h = open(port);
        assert!(!h.is_null(), "open_listener: {}", last_error_msg());
        handle_tx.send(SendPtr(h)).unwrap();
        // `tst_get_last_error_str` is thread-local: read it HERE, beside the
        // rc, or a failing assertion on the test thread reports nothing.
        let rc = recv(h);
        rc_tx.send((rc, last_error_msg())).unwrap();
    });
    // Peer: connect and hold, no data → the reader parks in recv. Held in an
    // `Option` so the rescue closure can OWN it: the rescue has to actually
    // CLOSE the peer socket (dropping a `&Socket` closes nothing and the
    // reader would never return, turning a failing test into a hang).
    let mut peer = Some(
        tst_srt::SocketBuilder::new()
            .connect_timeout(Duration::from_secs(5))
            .connect(format!("127.0.0.1:{port}"))
            .expect("peer connect"),
    );
    let SendPtr(h) = handle_rx
        .recv_timeout(WATCHDOG)
        .expect("reader never got past open");
    thread::sleep(Duration::from_millis(200)); // let the reader park
    assert_eq!(cancel(h), 0, "cancel rc: {}", last_error_msg());
    let rc = wait_result(&rc_rx, || {
        // Rescue: drop (close) the peer so the parked recv returns
        // (EOS/Broken) and the reader thread can be joined below.
        peer.take();
    });
    // Join the reader exactly once, on BOTH paths: the rescue above already
    // closed the peer, so the parked call has returned (or is about to) and
    // the join cannot hang. Only then assert, so a failure is a failure.
    let timed_out = rc.is_none();
    if timed_out {
        let _ = rc_rx.recv_timeout(Duration::from_secs(5));
    }
    reader.join().expect("reader thread");
    assert!(
        !timed_out,
        "_cancel did not wake the parked recv within {WATCHDOG:?}"
    );
    close(h);
    drop(peer);
    rc.expect("checked above")
}

#[test]
fn plain_receiver_parked_recv_packet_cancelled_reports_plain_srt_cancel_code() {
    use tstrans::receiver::ts_receiver::{
        tst_receiver_cancel, tst_receiver_close, tst_receiver_open_listener,
        tst_receiver_recv_packet,
    };
    let rc = parked_recv_cancel(
        |port| {
            let u = CString::new(format!("srt://:{port}")).unwrap();
            unsafe { tst_receiver_open_listener(u.as_ptr()) }
        },
        |h| {
            let mut buf = [0u8; 188];
            unsafe { tst_receiver_recv_packet(h, buf.as_mut_ptr()) }
        },
        |h| unsafe { tst_receiver_cancel(h) },
        |h| unsafe { tst_receiver_close(h) },
    );
    assert_eq!(rc.0, PLAIN_SRT_CANCEL_CODE, "reader last-error: {}", rc.1);
}

#[test]
fn plain_raw_receiver_parked_recv_cancelled_reports_plain_srt_cancel_code() {
    use tstrans::receiver::raw_receiver::{
        tst_raw_receiver_cancel, tst_raw_receiver_close, tst_raw_receiver_open_listener,
        tst_raw_receiver_recv,
    };
    let rc = parked_recv_cancel(
        |port| {
            let u = CString::new(format!("srt://:{port}")).unwrap();
            unsafe { tst_raw_receiver_open_listener(u.as_ptr()) }
        },
        |h| {
            let mut buf = vec![0u8; 1500];
            let mut n = 0usize;
            unsafe { tst_raw_receiver_recv(h, buf.as_mut_ptr(), buf.len(), &mut n) }
        },
        |h| unsafe { tst_raw_receiver_cancel(h) },
        |h| unsafe { tst_raw_receiver_close(h) },
    );
    assert_eq!(rc.0, PLAIN_SRT_CANCEL_CODE, "reader last-error: {}", rc.1);
}

#[test]
fn plain_demux_receiver_parked_recv_event_cancelled_reports_plain_srt_cancel_code() {
    use tstrans::event::TstEvent;
    use tstrans::receiver::demux_receiver::events::{
        tst_demux_receiver_cancel, tst_demux_receiver_recv_event,
    };
    use tstrans::receiver::demux_receiver::{
        tst_demux_receiver_close, tst_demux_receiver_open_listener,
    };
    let rc = parked_recv_cancel(
        |port| {
            let u = CString::new(format!("srt://:{port}")).unwrap();
            unsafe { tst_demux_receiver_open_listener(u.as_ptr()) }
        },
        |h| {
            let mut ev = TstEvent::default();
            unsafe { tst_demux_receiver_recv_event(h, &mut ev) }
        },
        |h| unsafe { tst_demux_receiver_cancel(h) },
        |h| unsafe { tst_demux_receiver_close(h) },
    );
    assert_eq!(rc.0, PLAIN_SRT_CANCEL_CODE, "reader last-error: {}", rc.1);
}

// ---------------------------------------------------- managed senders

#[test]
fn managed_sender_cancel_then_send_reports_closed() {
    use tstrans::config::{tst_sender_config_free, tst_sender_config_new};
    use tstrans::sender::ts_sender::{
        tst_managed_sender_cancel, tst_managed_sender_close, tst_managed_sender_flush,
        tst_managed_sender_get_reconnect_stats, tst_managed_sender_open,
        tst_managed_sender_send_ts,
    };
    let peer = PeerListener::spawn();
    let url = CString::new(format!("srt://127.0.0.1:{}", peer.port)).unwrap();
    let cfg = unsafe { tst_sender_config_new() };
    let tx = unsafe { tst_managed_sender_open(url.as_ptr(), cfg, std::ptr::null()) };
    unsafe { tst_sender_config_free(cfg) };
    assert!(!tx.is_null(), "open: {}", last_error_msg());
    let p = SendPtr(tx);
    assert_eq!(
        thread::spawn(move || {
            let p = p; // whole-struct capture: SendPtr is Send, its field is not
            let SendPtr(tx) = p;
            unsafe { tst_managed_sender_cancel(tx) }
        })
        .join()
        .unwrap(),
        0
    );
    let pkts = seven_null_packets();
    let mut rc = unsafe { tst_managed_sender_send_ts(tx, pkts.as_ptr(), pkts.len()) };
    if rc == 0 {
        rc = unsafe { tst_managed_sender_flush(tx) };
    }
    assert_eq!(
        rc,
        MANAGED_SRT_CANCEL_CODE,
        "after cancel: {}",
        last_error_msg()
    );
    // Reconnect stats stay readable after cancel (side-channel snapshot).
    let mut st = tstrans::stats::TstManagedTransportStats::default();
    let src = unsafe { tst_managed_sender_get_reconnect_stats(tx, &mut st) };
    assert!(
        src == 0 || src == TstError::Closed as i32,
        "stats rc {src}: {}",
        last_error_msg()
    );
    unsafe { tst_managed_sender_close(tx) };
    drop(peer);
}

#[test]
fn managed_raw_sender_cancel_then_send_reports_closed() {
    use tstrans::sender::raw_sender::{
        tst_managed_raw_sender_cancel, tst_managed_raw_sender_close,
        tst_managed_raw_sender_get_reconnect_stats, tst_managed_raw_sender_open,
        tst_managed_raw_sender_send,
    };
    let peer = PeerListener::spawn();
    let url = CString::new(format!("srt://127.0.0.1:{}", peer.port)).unwrap();
    let tx =
        unsafe { tst_managed_raw_sender_open(url.as_ptr(), std::ptr::null(), std::ptr::null()) };
    assert!(!tx.is_null(), "open: {}", last_error_msg());
    let p = SendPtr(tx);
    assert_eq!(
        thread::spawn(move || {
            let p = p; // whole-struct capture: SendPtr is Send, its field is not
            let SendPtr(tx) = p;
            unsafe { tst_managed_raw_sender_cancel(tx) }
        })
        .join()
        .unwrap(),
        0
    );
    let pkts = seven_null_packets();
    let rc = unsafe { tst_managed_raw_sender_send(tx, pkts.as_ptr(), 188) };
    assert_eq!(
        rc,
        MANAGED_SRT_CANCEL_CODE,
        "after cancel: {}",
        last_error_msg()
    );
    let mut st = tstrans::stats::TstManagedTransportStats::default();
    let src = unsafe { tst_managed_raw_sender_get_reconnect_stats(tx, &mut st) };
    assert!(
        src == 0 || src == TstError::Closed as i32,
        "stats rc {src}: {}",
        last_error_msg()
    );
    unsafe { tst_managed_raw_sender_close(tx) };
    drop(peer);
}

#[test]
fn managed_mux_sender_cancel_then_push_reports_closed() {
    use tstrans::config::{
        TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
        tst_mux_config_free, tst_mux_config_new,
    };
    use tstrans::sender::mux_sender::{
        tst_managed_mux_sender_cancel, tst_managed_mux_sender_close,
        tst_managed_mux_sender_get_reconnect_stats, tst_managed_mux_sender_open,
        tst_managed_mux_sender_send_video,
    };
    let peer = PeerListener::spawn();
    let url = CString::new(format!("srt://127.0.0.1:{}", peer.port)).unwrap();
    let (tx, cfg) = unsafe {
        let cfg = tst_mux_config_new();
        let prog = tst_mux_config_add_program(cfg, 1, 0x1000);
        tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264);
        (
            tst_managed_mux_sender_open(url.as_ptr(), cfg, std::ptr::null()),
            cfg,
        )
    };
    assert!(!tx.is_null(), "open: {}", last_error_msg());
    unsafe { tst_mux_config_free(cfg) };
    let p = SendPtr(tx);
    assert_eq!(
        thread::spawn(move || {
            let p = p; // whole-struct capture: SendPtr is Send, its field is not
            let SendPtr(tx) = p;
            unsafe { tst_managed_mux_sender_cancel(tx) }
        })
        .join()
        .unwrap(),
        0
    );
    let mut au = vec![0u8; 4096];
    au[..5].copy_from_slice(&[0, 0, 0, 1, 0x65]);
    let rc = unsafe { tst_managed_mux_sender_send_video(tx, au.as_ptr(), au.len(), 90_000, true) };
    assert_eq!(
        rc,
        MANAGED_SRT_CANCEL_CODE,
        "after cancel: {}",
        last_error_msg()
    );
    let mut st = tstrans::stats::TstManagedTransportStats::default();
    let src = unsafe { tst_managed_mux_sender_get_reconnect_stats(tx, &mut st) };
    assert!(
        src == 0 || src == TstError::Closed as i32,
        "stats rc {src}: {}",
        last_error_msg()
    );
    unsafe { tst_managed_mux_sender_close(tx) };
    drop(peer);
}

// -------------------------------------------------- managed receivers

#[test]
fn managed_receiver_parked_recv_packet_cancelled_reports_closed() {
    use tstrans::receiver::ts_receiver::{
        tst_managed_receiver_cancel, tst_managed_receiver_close,
        tst_managed_receiver_open_listener, tst_managed_receiver_recv_packet,
    };
    let rc = parked_recv_cancel(
        |port| {
            let u = CString::new(format!("srt://:{port}")).unwrap();
            unsafe { tst_managed_receiver_open_listener(u.as_ptr(), std::ptr::null()) }
        },
        |h| {
            let mut buf = [0u8; 188];
            unsafe { tst_managed_receiver_recv_packet(h, buf.as_mut_ptr()) }
        },
        |h| unsafe { tst_managed_receiver_cancel(h) },
        |h| unsafe { tst_managed_receiver_close(h) },
    );
    assert_eq!(rc.0, MANAGED_SRT_CANCEL_CODE, "reader last-error: {}", rc.1);
}

#[test]
fn managed_raw_receiver_parked_recv_cancelled_reports_closed() {
    use tstrans::receiver::raw_receiver::{
        tst_managed_raw_receiver_cancel, tst_managed_raw_receiver_close,
        tst_managed_raw_receiver_open_listener, tst_managed_raw_receiver_recv,
    };
    let rc = parked_recv_cancel(
        |port| {
            let u = CString::new(format!("srt://:{port}")).unwrap();
            unsafe { tst_managed_raw_receiver_open_listener(u.as_ptr(), std::ptr::null()) }
        },
        |h| {
            let mut buf = vec![0u8; 1500];
            let mut n = 0usize;
            unsafe { tst_managed_raw_receiver_recv(h, buf.as_mut_ptr(), buf.len(), &mut n) }
        },
        |h| unsafe { tst_managed_raw_receiver_cancel(h) },
        |h| unsafe { tst_managed_raw_receiver_close(h) },
    );
    assert_eq!(rc.0, MANAGED_SRT_CANCEL_CODE, "reader last-error: {}", rc.1);
}

#[test]
fn managed_demux_receiver_parked_recv_event_cancelled_reports_closed() {
    use tstrans::event::TstEvent;
    use tstrans::receiver::demux_receiver::managed::{
        tst_managed_demux_receiver_cancel, tst_managed_demux_receiver_close,
        tst_managed_demux_receiver_open_listener, tst_managed_demux_receiver_recv_event,
    };
    let rc = parked_recv_cancel(
        |port| {
            let u = CString::new(format!("srt://:{port}")).unwrap();
            unsafe { tst_managed_demux_receiver_open_listener(u.as_ptr(), std::ptr::null()) }
        },
        |h| {
            let mut ev = TstEvent::default();
            unsafe { tst_managed_demux_receiver_recv_event(h, &mut ev) }
        },
        |h| unsafe { tst_managed_demux_receiver_cancel(h) },
        |h| unsafe { tst_managed_demux_receiver_close(h) },
    );
    assert_eq!(rc.0, MANAGED_SRT_CANCEL_CODE, "reader last-error: {}", rc.1);
}
