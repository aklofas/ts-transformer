//! Listener admission over hand-built handshakes: what our listener does
//! with callers libsrt's own caller cannot produce. The tests here are the
//! fixture's positive controls.

use crate::common::Loopback;
use crate::common::raw_handshake::{HsOutcome, HsVersion, RawCaller};
use std::time::Duration;
use tst_srt::error::RejectReason;
use tst_srt::{AcceptError, ListenRule, Passphrase, SrtListenerBuilder};
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
