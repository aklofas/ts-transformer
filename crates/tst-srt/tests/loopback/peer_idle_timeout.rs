//! `peer_idle_timeout` (`SRTO_PEERIDLETIMEO`): a value below one millisecond is refused, a listener's
//! value reaches its accepted sockets, and a caller's value is applied
//! before connect.

use std::time::Duration;
use tst_srt::error::{BindError, SrtConnectError};
use tst_srt::{SrtListenerBuilder, SrtSocketBuilder};

fn peer_idle_timeout_ms(handle: srt_sys::SRTSOCKET) -> i32 {
    let mut value: std::ffi::c_int = 0;
    let mut len = std::mem::size_of::<std::ffi::c_int>() as std::ffi::c_int;
    let rc = unsafe {
        srt_sys::srt_getsockflag(
            handle,
            srt_sys::SRT_SOCKOPT_SRTO_PEERIDLETIMEO,
            (&raw mut value).cast(),
            &raw mut len,
        )
    };
    assert!(rc >= 0, "srt_getsockflag(SRTO_PEERIDLETIMEO) failed");
    value
}

#[test]
fn below_one_millisecond_is_refused_on_caller_and_listener() {
    require_loopback!();
    // libsrt accepts 0 and then breaks the connection on the first timer
    // tick, so the option layer refuses it before anything is dialled.
    // A positive sub-millisecond duration truncates to 0 ms, so it is
    // refused as well.
    for value in [Duration::ZERO, Duration::from_micros(1)] {
        let caller = SrtSocketBuilder::new()
            .peer_idle_timeout(value)
            .connect("127.0.0.1:9");
        assert!(
            matches!(caller, Err(SrtConnectError::InvalidOption(_))),
            "caller {value:?}: {:?}",
            caller.err()
        );
        let listener = SrtListenerBuilder::new()
            .peer_idle_timeout(value)
            .bind("127.0.0.1:0");
        assert!(
            matches!(listener, Err(BindError::InvalidOption(_))),
            "listener {value:?}: {:?}",
            listener.err()
        );
    }
}

#[test]
fn accepted_socket_inherits_listener_value() {
    require_loopback!();
    let mut lb = SrtListenerBuilder::new();
    lb.peer_idle_timeout(Duration::from_millis(1500));
    let fixture = crate::common::Loopback::bind_with(lb);
    let port = fixture.port;
    let accept = fixture.spawn_accept(|sock| peer_idle_timeout_ms(sock.raw_handle()));
    accept.wait_ready();
    let _caller = SrtSocketBuilder::new()
        .connect(("127.0.0.1", port))
        .expect("connect");
    assert_eq!(accept.join(), 1500);
}

#[test]
fn caller_applies_the_value_before_connect() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let port = lb.port;
    let accept = lb.spawn_accept(|sock| sock);
    accept.wait_ready();
    let caller = SrtSocketBuilder::new()
        .peer_idle_timeout(Duration::from_millis(7000))
        .connect(("127.0.0.1", port))
        .expect("connect");
    let _peer = accept.join();
    assert_eq!(peer_idle_timeout_ms(caller.raw_handle()), 7000);
}
