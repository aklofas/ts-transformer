//! Verifies SRTO_UDP_RCVBUF / SRTO_UDP_SNDBUF setters reach libsrt.

use std::ffi::c_int;
use std::time::Duration;
use tst_srt::{ListenerBuilder, SocketBuilder};

fn read_u32(handle: srt_sys::SRTSOCKET, opt: srt_sys::SRT_SOCKOPT) -> Option<u32> {
    let mut value: c_int = 0;
    let mut len = std::mem::size_of::<c_int>() as c_int;
    let rc =
        unsafe { srt_sys::srt_getsockflag(handle, opt, (&raw mut value).cast(), &raw mut len) };
    if rc < 0 || value < 0 {
        return None;
    }
    Some(value as u32)
}

#[test]
fn udp_buffer_sizes_round_trip() {
    require_loopback!();
    let mut builder = ListenerBuilder::new();
    builder
        .udp_recv_buffer_bytes(2_000_000)
        .recv_timeout(Duration::from_secs(5));
    let lb = crate::common::Loopback::bind_with(builder);
    let port = lb.port;

    let accept = lb.spawn_accept(|sock| sock);
    accept.wait_ready();

    let socket = SocketBuilder::new()
        .udp_recv_buffer_bytes(2_000_000)
        .udp_send_buffer_bytes(2_000_000)
        .send_timeout(Duration::from_secs(5))
        .connect(("127.0.0.1", port))
        .expect("connect");

    // Keep the accepted peer alive across the option reads — same race as
    // srt_buf_bytes.rs: a dropped peer closes the far side and getsockopt
    // starts failing once libsrt notices (first hit under ASan timing).
    let peer = accept.join();

    let handle = socket.raw_handle();
    let rcv = read_u32(handle, srt_sys::SRT_SOCKOPT_SRTO_UDP_RCVBUF).expect("read RCVBUF");
    let snd = read_u32(handle, srt_sys::SRT_SOCKOPT_SRTO_UDP_SNDBUF).expect("read SNDBUF");
    drop(peer);

    // Kernel may clamp to net.core.{r,w}mem_max; verify it's well above
    // OS default (~208 KB on Linux). Use 1 MB as conservative floor.
    assert!(rcv >= 1_000_000, "UDP_RCVBUF too small: {rcv}");
    assert!(snd >= 1_000_000, "UDP_SNDBUF too small: {snd}");
}
