//! Passphrase listen rules over the encrypted loopback: the rule's secret
//! is installed during the handshake, before libsrt checks key material.
#![cfg(feature = "mbedtls")]

use std::time::Duration;
use tst_srt::error::RejectReason;
use tst_srt::{
    ListenRule, Passphrase, SrtConnectError, SrtListenerBuilder, SrtSocket, SrtSocketBuilder,
    StreamId,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(3);

fn pw(s: &str) -> Passphrase {
    Passphrase::new(s).unwrap()
}

fn connect(
    port: u16,
    stream_id: &str,
    passphrase: Option<&str>,
) -> Result<SrtSocket, SrtConnectError> {
    let mut b = SrtSocketBuilder::new();
    b.connect_timeout(CONNECT_TIMEOUT)
        .stream_id(StreamId::new(stream_id).expect("stream id"));
    if let Some(p) = passphrase {
        b.passphrase(pw(p));
    }
    b.connect(("127.0.0.1", port))
}

fn bind(builder: SrtListenerBuilder) -> (tst_srt::SrtListener, u16) {
    let listener = builder.bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local_addr").port();
    (listener, port)
}

const SECRET: &str = "amber-secret-123";
const OTHER: &str = "other-secret-456";

#[test]
fn accept_with_passphrase_admits_the_matching_caller_and_rejects_the_wrong_one() {
    require_loopback!();
    let (mut listener, port) = bind(SrtListenerBuilder::new());
    listener
        .rules()
        .set(
            "publish:amber",
            ListenRule::AcceptWith {
                passphrase: pw(SECRET),
                key_length: None,
            },
        )
        .unwrap();

    let _ok = connect(port, "publish:amber", Some(SECRET)).expect("matching caller");
    let (accepted, _peer) = listener.accept_timeout(ACCEPT_TIMEOUT).expect("accept");
    assert_eq!(accepted.stream_id(), Some("publish:amber"));

    crate::common::assert_rejected(
        connect(port, "publish:amber", Some(OTHER)),
        RejectReason::BadSecret,
    );
    crate::common::assert_rejected(connect(port, "publish:amber", None), RejectReason::Unsecure);
}

#[test]
fn per_id_passphrase_overrides_the_builder_wide_one() {
    require_loopback!();
    let mut lb = SrtListenerBuilder::new();
    lb.passphrase(pw(SECRET));
    let (mut listener, port) = bind(lb);
    listener
        .rules()
        .set(
            "b",
            ListenRule::AcceptWith {
                passphrase: pw(OTHER),
                key_length: None,
            },
        )
        .unwrap();

    let _a = connect(port, "a", Some(SECRET)).expect("builder-wide secret on id a");
    listener.accept_timeout(ACCEPT_TIMEOUT).expect("accept a");
    let _b = connect(port, "b", Some(OTHER)).expect("per-id secret on id b");
    listener.accept_timeout(ACCEPT_TIMEOUT).expect("accept b");
    crate::common::assert_rejected(connect(port, "b", Some(SECRET)), RejectReason::BadSecret);
}

#[test]
fn reject_rule_wins_over_a_wrong_passphrase() {
    require_loopback!();
    let mut lb = SrtListenerBuilder::new();
    lb.passphrase(pw(SECRET));
    let (listener, port) = bind(lb);
    listener
        .rules()
        .set("d", ListenRule::Reject(RejectReason::Forbidden))
        .unwrap();
    crate::common::assert_rejected(connect(port, "d", Some(OTHER)), RejectReason::Forbidden);
}
