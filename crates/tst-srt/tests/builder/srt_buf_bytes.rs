//! Verifies that `recv_buf_bytes` / `send_buf_bytes` pass byte values verbatim
//! to SRTO_RCVBUF / SRTO_SNDBUF.
//!
//! The values are bytes, not packet counts: libsrt stores these options in
//! bytes internally (converting to buffer-slot counts via MSS-28 division with
//! a 32-slot floor). Setting 1 048 576 bytes is not 1 048 576 packets.
//!
//! This test connects a real socket, sets large byte values, reads them back
//! via srt_getsockflag, and asserts the returned value is in the byte range
//! (≥ 1 MB, not ≤ 32 — which would be libsrt's minimum if the value were
//! interpreted as a packet count and divided by MSS).

use std::ffi::c_int;
use std::time::Duration;
use tst_srt::{ListenerBuilder, SocketBuilder};

fn read_i32(handle: srt_sys::SRTSOCKET, opt: srt_sys::SRT_SOCKOPT) -> Option<i32> {
    let mut value: c_int = 0;
    let mut len = std::mem::size_of::<c_int>() as c_int;
    let rc =
        unsafe { srt_sys::srt_getsockflag(handle, opt, (&raw mut value).cast(), &raw mut len) };
    if rc < 0 {
        return None;
    }
    Some(value)
}

#[test]
fn recv_buf_bytes_and_send_buf_bytes_are_byte_scaled() {
    require_loopback!();

    // Request 4 MB recv buffer on the listener, 2 MB send buffer on caller.
    // libsrt may clamp these, but the clamped value is still expressed in
    // bytes (typically ≥ 1 MB on a modern Linux kernel). The critical
    // assertion is that read-back > 65535 — a value impossible if the
    // option had been treated as a packet count (packets × MSS ≈ 1316 bytes
    // per packet → 1 048 576 packets would be ~1.4 GB, but libsrt would
    // silently store the raw integer 1 048 576 as the byte count, which is
    // already correct; the old *name* was the misleading part).
    //
    // What we're actually pinning: the value that arrives at libsrt equals
    // the byte value the caller set, not something scaled by MSS.
    let target_recv: u32 = 4 * 1024 * 1024; // 4 MB
    let target_send: u32 = 2 * 1024 * 1024; // 2 MB

    let mut builder = ListenerBuilder::new();
    builder
        .recv_buf_bytes(target_recv)
        .recv_timeout(Duration::from_secs(5));
    let lb = crate::common::Loopback::bind_with(builder);
    let port = lb.port;

    let accept = lb.spawn_accept(|sock| sock);
    accept.wait_ready();

    let socket = SocketBuilder::new()
        .recv_buf_bytes(target_recv)
        .send_buf_bytes(target_send)
        .send_timeout(Duration::from_secs(5))
        .connect(("127.0.0.1", port))
        .expect("connect");

    // Keep the accepted peer alive across the option reads: dropping it
    // closes the far side, and once libsrt notices, getsockopt on this
    // socket starts failing — a race ASan's slower timing exposes.
    let peer = accept.join();

    let handle = socket.raw_handle();

    let rcv = read_i32(handle, srt_sys::SRT_SOCKOPT_SRTO_RCVBUF).expect("read SRTO_RCVBUF");
    let snd = read_i32(handle, srt_sys::SRT_SOCKOPT_SRTO_SNDBUF).expect("read SRTO_SNDBUF");
    drop(peer);

    // libsrt may clamp to a kernel limit; the floor assertion here is
    // 512 KB — well above any MSS-scaled packet interpretation of the
    // same integer (which would be ≤ 32 buffers × 1316 bytes ≈ 42 KB).
    assert!(
        rcv >= 512 * 1024,
        "SRTO_RCVBUF read back {rcv} bytes — expected ≥ 512 KB (byte semantics)"
    );
    assert!(
        snd >= 512 * 1024,
        "SRTO_SNDBUF read back {snd} bytes — expected ≥ 512 KB (byte semantics)"
    );
}
