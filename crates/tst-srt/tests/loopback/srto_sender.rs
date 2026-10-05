//! SRTO_SENDER=1 is set whenever the role is Sender.

use std::ffi::c_int;
use std::time::Duration;
use tst_srt::{Role, SocketBuilder};

fn read_srto_sender(handle: srt_sys::SRTSOCKET) -> i32 {
    // Initialize to 0 — libsrt's getsockflag for SRTO_SENDER writes a single
    // byte (the bool) and updates `len` to 1, leaving the upper 3 bytes
    // untouched. Pre-zeroing keeps the read deterministic.
    let mut value: c_int = 0;
    let mut len = std::mem::size_of::<c_int>() as c_int;
    let rc = unsafe {
        srt_sys::srt_getsockflag(
            handle,
            srt_sys::SRT_SOCKOPT_SRTO_SENDER,
            (&raw mut value).cast(),
            &raw mut len,
        )
    };
    assert!(rc >= 0, "srt_getsockflag(SRTO_SENDER) failed");
    value
}

#[test]
fn role_sender_sets_srto_sender_on_caller() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;

    let accept = lb.spawn_accept(|sock| sock);
    accept.wait_ready();

    let socket = SocketBuilder::new()
        .role(Role::Sender)
        .send_timeout(Duration::from_secs(5))
        .connect(("127.0.0.1", port))
        .expect("connect");

    let _peer = accept.join();

    assert_eq!(
        read_srto_sender(socket.raw_handle()),
        1,
        "expected SRTO_SENDER=1 on caller with role=Sender",
    );
}

#[test]
fn role_receiver_leaves_srto_sender_at_default() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;

    let accept = lb.spawn_accept(|sock| sock);
    accept.wait_ready();

    let socket = SocketBuilder::new()
        .send_timeout(Duration::from_secs(5))
        .connect(("127.0.0.1", port))
        .expect("connect");

    let _peer = accept.join();

    assert_eq!(
        read_srto_sender(socket.raw_handle()),
        0,
        "expected SRTO_SENDER=0 (libsrt default) when role=Receiver",
    );
}
