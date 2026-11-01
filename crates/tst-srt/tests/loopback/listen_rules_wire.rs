//! Listener admission over hand-built handshakes: what our listener does
//! with callers libsrt's own caller cannot produce. The tests here are the
//! fixture's positive controls.

use crate::common::Loopback;
use crate::common::raw_handshake::{HsOutcome, HsVersion, RawCaller};
use std::time::Duration;
use tst_srt::error::RejectReason;
use tst_srt::{ListenRule, SrtListenerBuilder};
use tst_test_helpers::timing::scaled;

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
