//! Listener admission over hand-built handshakes: what our listener does
//! with callers libsrt's own caller cannot produce. The tests here are the
//! fixture's positive controls.

use crate::common::Loopback;
use crate::common::raw_handshake::{HsOutcome, HsVersion, RawCaller};
use std::time::Duration;
use tst_srt::error::RejectReason;
use tst_srt::{
    AcceptError, ListenRule, Passphrase, SrtListenerBuilder, SrtSocketBuilder, StreamId,
};
use tst_test_helpers::timing::scaled;

/// `SRT_REJ_ROGUE` (srt.h): the handshake breaks the protocol.
const SRT_REJ_ROGUE: i32 = 4;

fn handshake_timeout() -> Duration {
    scaled(Duration::from_secs(3))
}

fn accept_timeout() -> Duration {
    scaled(Duration::from_secs(2))
}

#[test]
fn raw_v5_handshake_with_one_sid_is_admitted_with_that_stream_id() {
    require_loopback!();
    let Loopback { mut listener, port } = Loopback::bind_with(SrtListenerBuilder::new());
    assert_eq!(
        RawCaller::handshake(port, HsVersion::V5, &[b"cam-1"], handshake_timeout()),
        HsOutcome::Accepted
    );
    let (socket, _) = listener
        .accept_timeout(accept_timeout())
        .expect("accept the raw caller");
    assert_eq!(socket.stream_id(), Some("cam-1"));
}

#[test]
fn raw_v4_handshake_is_admitted_by_a_plain_listener() {
    require_loopback!();
    let Loopback { mut listener, port } = Loopback::bind_with(SrtListenerBuilder::new());
    assert_eq!(
        RawCaller::handshake(port, HsVersion::V4, &[], handshake_timeout()),
        HsOutcome::Accepted
    );
    let (socket, _) = listener
        .accept_timeout(accept_timeout())
        .expect("accept the raw caller");
    assert_eq!(socket.stream_id(), None);
}

#[test]
fn raw_v5_handshake_reports_the_reject_reason() {
    require_loopback!();
    let Loopback { listener, port } = Loopback::bind_with(SrtListenerBuilder::new());
    listener
        .rules()
        .set_default(ListenRule::Reject(RejectReason::Forbidden))
        .unwrap();
    assert_eq!(
        RawCaller::handshake(port, HsVersion::V5, &[b"cam-1"], handshake_timeout()),
        HsOutcome::Rejected(1403)
    );
}

#[test]
fn raw_v4_handshake_refused_by_a_rule_sees_a_shutdown() {
    require_loopback!();
    let Loopback { listener, port } = Loopback::bind_with(SrtListenerBuilder::new());
    listener
        .rules()
        .set_default(ListenRule::Reject(RejectReason::Forbidden))
        .unwrap();
    assert_eq!(
        RawCaller::handshake(port, HsVersion::V4, &[], handshake_timeout()),
        HsOutcome::Shutdown
    );
}

/// Two SID blocks: the accept hook used to read the first (`liveX`, default
/// rule) while the accepted socket kept the last (`live`, a passphrase rule
/// that was never applied). libsrt now refuses the repeat as rogue.
#[test]
#[cfg_attr(not(libsrt_source_fixups), ignore = "needs the vendored libsrt build")]
fn repeated_sid_block_is_refused() {
    require_loopback!();
    let Loopback { mut listener, port } = Loopback::bind_with(SrtListenerBuilder::new());
    listener
        .rules()
        .set(
            "live",
            ListenRule::AcceptWith {
                passphrase: Passphrase::new("amber-secret-123").unwrap(),
            },
        )
        .unwrap();
    assert_eq!(
        RawCaller::handshake(
            port,
            HsVersion::V5,
            &[b"liveX", b"live"],
            handshake_timeout()
        ),
        HsOutcome::Rejected(SRT_REJ_ROGUE)
    );
    assert!(
        matches!(
            listener.accept_timeout(scaled(Duration::from_secs(1))),
            Err(AcceptError::TimedOut)
        ),
        "a refused caller must not reach accept"
    );
}

/// libsrt answers a hook-refused v4 caller with a shutdown, not a reject code.
#[cfg(feature = "mbedtls")]
#[test]
fn hsv4_caller_refused_by_a_passphrase_listener() {
    require_loopback!();
    let mut b = SrtListenerBuilder::new();
    b.passphrase(Passphrase::new("amber-secret-123").unwrap());
    let Loopback { mut listener, port } = Loopback::bind_with(b);
    assert_eq!(
        RawCaller::handshake(port, HsVersion::V4, &[], handshake_timeout()),
        HsOutcome::Shutdown
    );
    assert!(
        matches!(
            listener.accept_timeout(scaled(Duration::from_secs(1))),
            Err(AcceptError::TimedOut)
        ),
        "a refused caller must not reach accept"
    );
}

#[cfg(feature = "mbedtls")]
#[test]
fn hsv4_caller_refused_when_the_default_rule_is_accept_with() {
    require_loopback!();
    let Loopback { mut listener, port } = Loopback::bind_with(SrtListenerBuilder::new());
    listener
        .rules()
        .set_default(ListenRule::AcceptWith {
            passphrase: Passphrase::new("amber-secret-123").unwrap(),
        })
        .unwrap();
    assert_eq!(
        RawCaller::handshake(port, HsVersion::V4, &[], handshake_timeout()),
        HsOutcome::Shutdown
    );
    assert!(
        matches!(
            listener.accept_timeout(scaled(Duration::from_secs(1))),
            Err(AcceptError::TimedOut)
        ),
        "a refused caller must not reach accept"
    );
}

/// A libsrt caller whose stream ID is set by length, so it can carry a NUL
/// byte `StreamId` refuses. Closed on drop.
struct LengthSidCaller(srt_sys::SRTSOCKET);

impl LengthSidCaller {
    /// `None` when the handshake fails.
    fn connect(port: u16, stream_id: &[u8]) -> Option<Self> {
        use std::ffi::c_int;
        // SAFETY: plain libsrt calls on a socket this function owns; the
        // listener the caller bound already initialised libsrt.
        unsafe {
            let s = srt_sys::srt_create_socket();
            assert_ne!(s, srt_sys::SRT_INVALID_SOCK, "srt_create_socket");
            let caller = Self(s);
            let rc = srt_sys::srt_setsockflag(
                s,
                srt_sys::SRT_SOCKOPT_SRTO_STREAMID,
                stream_id.as_ptr().cast(),
                stream_id.len() as c_int,
            );
            assert_eq!(rc, 0, "set SRTO_STREAMID");
            let ms = handshake_timeout().as_millis() as c_int;
            let rc = srt_sys::srt_setsockflag(
                s,
                srt_sys::SRT_SOCKOPT_SRTO_CONNTIMEO,
                (&raw const ms).cast(),
                std::mem::size_of::<c_int>() as c_int,
            );
            assert_eq!(rc, 0, "set SRTO_CONNTIMEO");
            let addr = os_socketaddr::OsSocketAddr::from(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                port,
            )));
            let rc = srt_sys::srt_connect(s, addr.as_ptr().cast(), addr.len() as c_int);
            (rc != srt_sys::SRT_ERROR).then_some(caller)
        }
    }
}

impl Drop for LengthSidCaller {
    fn drop(&mut self) {
        // SAFETY: the socket is ours and closed once.
        unsafe { srt_sys::srt_close(self.0) };
    }
}

/// A table that refuses by default and admits only `cam`: the hook reads a
/// stream ID up to its first NUL, so `cam\0x` would pass as `cam`.
fn cam_only() -> Loopback {
    let lb = Loopback::bind_with(SrtListenerBuilder::new());
    let rules = lb.listener.rules();
    rules
        .set_default(ListenRule::Reject(RejectReason::Forbidden))
        .unwrap();
    rules.set("cam", ListenRule::Accept).unwrap();
    lb
}

fn connect_cam(port: u16) -> tst_srt::SrtSocket {
    let mut b = SrtSocketBuilder::new();
    b.connect_timeout(handshake_timeout());
    b.stream_id(StreamId::new("cam").unwrap());
    b.connect(("127.0.0.1", port)).expect("connect as cam")
}

/// Connect an ordinary `cam` caller after `delay`, from another thread, so
/// it arrives while an accept call is already waiting.
fn connect_cam_after(port: u16, delay: Duration) -> std::thread::JoinHandle<tst_srt::SrtSocket> {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        connect_cam(port)
    })
}

#[test]
fn accepted_socket_with_a_nul_bearing_stream_id_is_closed() {
    require_loopback!();
    let Loopback { mut listener, port } = cam_only();
    let _nul = LengthSidCaller::connect(port, b"cam\0x").expect("the hook admits cam\\0x");
    let got = listener.accept_timeout(scaled(Duration::from_secs(1)));
    assert!(
        matches!(got, Err(AcceptError::TimedOut)),
        "the mismatched socket must not be accepted: {:?}",
        got.map(|(s, _)| s.stream_id().map(str::to_owned))
    );
}

/// One `accept_timeout` call closes the mismatch and keeps waiting for the
/// next caller inside its deadline.
#[test]
fn accept_timeout_rewaits_after_closing_a_mismatch() {
    require_loopback!();
    let Loopback { mut listener, port } = cam_only();
    let _nul = LengthSidCaller::connect(port, b"cam\0x").expect("the hook admits cam\\0x");
    let cam = connect_cam_after(port, Duration::from_millis(300));
    let (socket, _) = listener
        .accept_timeout(accept_timeout())
        .expect("accept the ordinary caller");
    assert_eq!(socket.stream_id(), Some("cam"));
    drop(cam.join().expect("join connector"));
}

#[test]
fn blocking_accept_closes_a_mismatch_and_keeps_waiting() {
    require_loopback!();
    let Loopback { mut listener, port } = cam_only();
    let cancel = listener.cancel_handle();
    let _nul = LengthSidCaller::connect(port, b"cam\0x").expect("the hook admits cam\\0x");
    let (tx, rx) = std::sync::mpsc::channel();
    let accept = std::thread::spawn(move || {
        let r = listener
            .accept()
            .map(|(s, _)| s.stream_id().map(str::to_owned));
        let _ = tx.send(r);
    });
    let cam = connect_cam_after(port, Duration::from_millis(300));
    let got = rx.recv_timeout(scaled(Duration::from_secs(3)));
    if got.is_err() {
        cancel.cancel();
    }
    accept.join().expect("join accept");
    // The connector fails once the accept thread drops the listener early.
    let _ = cam.join();
    assert_eq!(
        got.expect("accept returned").expect("accept"),
        Some("cam".to_owned())
    );
}

#[test]
fn blocking_accept_after_a_closed_mismatch_still_honours_cancel() {
    require_loopback!();
    let Loopback { mut listener, port } = cam_only();
    let cancel = listener.cancel_handle();
    let _nul = LengthSidCaller::connect(port, b"cam\0x").expect("the hook admits cam\\0x");
    let (tx, rx) = std::sync::mpsc::channel();
    let accept = std::thread::spawn(move || {
        let r = listener.accept().map(|_| ());
        let _ = tx.send(r);
    });
    // Nothing else connects: accept must still be waiting after the
    // mismatch, then return once cancelled.
    assert!(
        rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "accept returned without a matching caller"
    );
    cancel.cancel();
    let got = rx
        .recv_timeout(scaled(Duration::from_secs(2)))
        .expect("cancel wakes the accept");
    accept.join().expect("join accept");
    assert!(got.is_err(), "a cancelled accept returns an error");
}

/// The hook sees an empty string and the socket reports no stream ID: the
/// same identity, never closed.
#[test]
fn caller_without_stream_id_is_not_closed() {
    require_loopback!();
    let Loopback { mut listener, port } = Loopback::bind_with(SrtListenerBuilder::new());
    let mut b = SrtSocketBuilder::new();
    b.connect_timeout(handshake_timeout());
    let _caller = b.connect(("127.0.0.1", port)).expect("connect");
    let (socket, _) = listener
        .accept_timeout(accept_timeout())
        .expect("accept the caller");
    assert_eq!(socket.stream_id(), None);
}

/// libsrt keeps a one-byte stream ID's NUL padding on the socket; the
/// identity check reads it as padding and keeps the connection.
#[test]
fn one_byte_stream_id_is_not_closed() {
    require_loopback!();
    let Loopback { mut listener, port } = Loopback::bind_with(SrtListenerBuilder::new());
    let mut b = SrtSocketBuilder::new();
    b.connect_timeout(handshake_timeout());
    b.stream_id(StreamId::new("a").unwrap());
    let _caller = b.connect(("127.0.0.1", port)).expect("connect");
    listener
        .accept_timeout(accept_timeout())
        .expect("accept the caller");
}
