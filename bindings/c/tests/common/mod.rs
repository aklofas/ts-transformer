//! Helpers shared by every tst-c integration-test binary. Each binary root
//! declares `#[allow(dead_code)] mod common;` because per-file feature
//! gates decide which helpers a given build actually calls.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tstrans::error::tst_get_last_error_str;

/// Read the C ABI's thread-local last-error string as an owned `String`;
/// `"<null>"` when the slot is empty, so a failing assertion still prints
/// something useful.
pub(crate) fn last_error_msg() -> String {
    unsafe {
        let p = tst_get_last_error_str();
        if p.is_null() {
            return "<null>".into();
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// One 188-byte null TS packet (PID 0x1FFF), the smallest valid payload for
/// the sender tests.
pub(crate) fn ts_null_packet() -> [u8; 188] {
    let mut p = [0xFFu8; 188];
    p[0] = 0x47;
    p[1] = 0x1F;
    p[2] = 0xFF;
    p[3] = 0x10;
    p
}

/// `base` stretched by the `TST_TEST_TIMEOUT_SCALE` environment variable
/// (a whole number, default 1). The sanitizer jobs set it because they run
/// the peer threads several times slower. Unset, the margin is unchanged.
pub(crate) fn scaled(base: Duration) -> Duration {
    let scale = std::env::var("TST_TEST_TIMEOUT_SCALE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(1);
    base * scale
}

/// `_cancel` is documented callable from any thread; the raw pointer just
/// needs to cross the thread boundary to get there.
pub(crate) struct SendPtr<T>(pub *mut T);
unsafe impl<T> Send for SendPtr<T> {}

/// Park `recv` on a reader thread, cancel from this thread, return the code
/// the reader observed. Nothing ever sends to the socket, so the receive is
/// genuinely parked.
///
/// `rescue` is what frees the reader if the cancel does NOT wake it: it owns
/// the handle and closes it, which ends the parked call so the thread can be
/// joined. Without it the watchdog would fail the test and then HANG in
/// `join()` — the same shape `receiving/cancel_first.rs` uses for its peer.
pub(crate) fn parked<T: 'static>(
    h: *mut T,
    recv: impl Fn(*mut T) -> i32 + Send + 'static,
    cancel: impl FnOnce(*mut T) -> i32,
    rescue: impl FnOnce(*mut T),
) -> i32 {
    let (tx, rx) = mpsc::channel::<i32>();
    let p = SendPtr(h);
    let reader = thread::spawn(move || {
        let p = p; // whole-struct capture: SendPtr is Send, its field is not
        let SendPtr(h) = p;
        tx.send(recv(h)).unwrap();
    });
    thread::sleep(Duration::from_millis(200)); // let the reader park
    assert_eq!(cancel(h), 0);
    let rc = rx.recv_timeout(Duration::from_secs(10)).ok();
    if rc.is_none() {
        // Close the handle so the parked recv returns, then drain and join
        // before asserting: a failure must be a failure, never a wedge.
        rescue(h);
        let _ = rx.recv_timeout(Duration::from_secs(5));
    }
    reader.join().expect("reader thread");
    rc.expect("_cancel did not wake the parked receive within 10 s")
}

/// A UDP port the kernel just handed out: bind :0, read it, drop the socket.
/// For handles with no local-port getter; libsrt binds the same UDP space.
pub(crate) fn free_udp_port() -> u16 {
    let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    s.local_addr().unwrap().port()
}
