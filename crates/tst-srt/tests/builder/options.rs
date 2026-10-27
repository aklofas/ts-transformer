//! Sample of SocketConfig field round-trips. Confirms the option-application
//! layer doesn't silently drop fields.

use std::ffi::c_int;
use std::time::Duration;
use tst_srt::{Congestion, MaxBandwidth, SrtListenerBuilder, SrtSocketBuilder};

fn get_i32(handle: srt_sys::SRTSOCKET, opt: srt_sys::SRT_SOCKOPT) -> i32 {
    let mut value: c_int = 0;
    let mut len = std::mem::size_of::<c_int>() as c_int;
    let rc =
        unsafe { srt_sys::srt_getsockflag(handle, opt, (&raw mut value).cast(), &raw mut len) };
    assert!(rc >= 0, "srt_getsockflag failed");
    value
}

fn get_i64(handle: srt_sys::SRTSOCKET, opt: srt_sys::SRT_SOCKOPT) -> i64 {
    let mut value: i64 = 0;
    let mut len = std::mem::size_of::<i64>() as c_int;
    let rc =
        unsafe { srt_sys::srt_getsockflag(handle, opt, (&raw mut value).cast(), &raw mut len) };
    assert!(rc >= 0, "srt_getsockflag failed");
    value
}

fn get_string(handle: srt_sys::SRTSOCKET, opt: srt_sys::SRT_SOCKOPT) -> String {
    let mut buf = [0u8; 16];
    let mut len = buf.len() as c_int;
    let rc =
        unsafe { srt_sys::srt_getsockflag(handle, opt, buf.as_mut_ptr().cast(), &raw mut len) };
    assert!(rc >= 0, "srt_getsockflag failed");
    let n = (len as usize).min(buf.len());
    String::from_utf8_lossy(&buf[..n])
        .trim_end_matches('\0')
        .to_string()
}

#[test]
fn latency_configures_without_error() {
    require_loopback!();
    let mut builder = SrtListenerBuilder::new();
    builder
        .latency(Duration::from_millis(200))
        .recv_timeout(Duration::from_secs(5));
    let lb = crate::common::Loopback::bind_with(builder);
    let port = lb.port;

    let accept = lb.spawn_accept(|sock| {
        drop(sock);
    });
    accept.wait_ready();

    let socket = SrtSocketBuilder::new()
        .latency(Duration::from_millis(200))
        .recv_timeout(Duration::from_secs(5))
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");
    assert_eq!(
        get_i32(socket.raw_handle(), srt_sys::SRT_SOCKOPT_SRTO_LATENCY),
        200
    );

    accept.join();
}

#[test]
fn payload_size_and_mss_apply() {
    require_loopback!();
    let mut builder = SrtListenerBuilder::new();
    builder
        .mss(1316)
        .payload_size(1316)
        .recv_timeout(Duration::from_secs(5));
    let lb = crate::common::Loopback::bind_with(builder);
    let port = lb.port;

    let accept = lb.spawn_accept(|sock| {
        drop(sock);
    });
    accept.wait_ready();

    let socket = SrtSocketBuilder::new()
        .mss(1316)
        .payload_size(1316)
        .recv_timeout(Duration::from_secs(5))
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");
    assert_eq!(
        get_i32(socket.raw_handle(), srt_sys::SRT_SOCKOPT_SRTO_PAYLOADSIZE),
        1316
    );
    assert_eq!(
        get_i32(socket.raw_handle(), srt_sys::SRT_SOCKOPT_SRTO_MSS),
        1316
    );

    accept.join();
}

#[test]
fn max_bandwidth_and_congestion_apply() {
    require_loopback!();
    let mut builder = SrtListenerBuilder::new();
    builder
        .max_bandwidth(MaxBandwidth::Limited(10_000_000))
        .congestion(Congestion::Live)
        .recv_timeout(Duration::from_secs(5));
    let lb = crate::common::Loopback::bind_with(builder);
    let port = lb.port;

    let accept = lb.spawn_accept(|sock| {
        drop(sock);
    });
    accept.wait_ready();

    let socket = SrtSocketBuilder::new()
        .max_bandwidth(MaxBandwidth::Limited(10_000_000))
        .congestion(Congestion::Live)
        .recv_timeout(Duration::from_secs(5))
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");
    assert_eq!(
        get_i64(socket.raw_handle(), srt_sys::SRT_SOCKOPT_SRTO_MAXBW),
        10_000_000
    );
    assert_eq!(
        get_string(socket.raw_handle(), srt_sys::SRT_SOCKOPT_SRTO_CONGESTION),
        "live"
    );

    accept.join();
}
