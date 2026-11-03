//! Helpers for the network (`net_*`) tst-c test binaries only: they open sockets,
//! so the non-network binaries must not include this file.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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
