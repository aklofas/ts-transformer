//! Per-stream-ID listen rules: the listener's table decides each handshake
//! before `accept()` sees the connection.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tst_srt::error::RejectReason;
use tst_srt::{
    AcceptError, ListenRule, ListenRuleError, SrtConnectError, SrtListener, SrtListenerBuilder,
    SrtSocket, SrtSocketBuilder, StreamId,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(2);

fn listener() -> (SrtListener, u16) {
    let listener = SrtListenerBuilder::new()
        .bind("127.0.0.1:0")
        .expect("bind 127.0.0.1:0");
    let port = listener.local_addr().expect("local_addr").port();
    (listener, port)
}

fn connect(port: u16, stream_id: Option<&str>) -> Result<SrtSocket, SrtConnectError> {
    let mut b = SrtSocketBuilder::new();
    b.connect_timeout(CONNECT_TIMEOUT);
    if let Some(id) = stream_id {
        b.stream_id(StreamId::new(id).expect("stream id"));
    }
    b.connect(("127.0.0.1", port))
}

#[test]
fn reject_rule_refuses_the_caller_with_its_reason_and_accept_never_sees_it() {
    require_loopback!();
    let (mut listener, port) = listener();
    listener
        .rules()
        .set("publish:amber", ListenRule::Reject(RejectReason::Forbidden))
        .unwrap();
    let accept = std::thread::spawn(move || listener.accept_timeout(ACCEPT_TIMEOUT).map(|_| ()));
    crate::common::assert_rejected(
        connect(port, Some("publish:amber")),
        RejectReason::Forbidden,
    );
    assert!(matches!(
        accept.join().expect("join accept"),
        Err(AcceptError::TimedOut)
    ));
}

#[test]
fn default_rule_switch_applies_to_the_next_caller() {
    require_loopback!();
    let (mut listener, port) = listener();
    let rules = listener.rules();

    let _first = connect(port, Some("publish:amber")).expect("first caller connects");
    listener
        .accept_timeout(ACCEPT_TIMEOUT)
        .expect("accept first");

    rules
        .set_default(ListenRule::Reject(RejectReason::Overload))
        .unwrap();
    crate::common::assert_rejected(connect(port, Some("publish:amber")), RejectReason::Overload);

    rules.set_default(ListenRule::Accept).unwrap();
    let _third = connect(port, Some("publish:amber")).expect("third caller connects");
    listener
        .accept_timeout(ACCEPT_TIMEOUT)
        .expect("accept third");
}

#[test]
fn remove_falls_back_to_the_default() {
    require_loopback!();
    let (mut listener, port) = listener();
    let rules = listener.rules();
    rules
        .set("a", ListenRule::Reject(RejectReason::Conflict))
        .unwrap();
    crate::common::assert_rejected(connect(port, Some("a")), RejectReason::Conflict);
    assert!(rules.remove("a"));
    let _sock = connect(port, Some("a")).expect("connects once the rule is gone");
    listener.accept_timeout(ACCEPT_TIMEOUT).expect("accept");
}

#[test]
fn untouched_table_accepts_as_before() {
    require_loopback!();
    let (mut listener, port) = listener();
    let _with_id = connect(port, Some("publish:amber")).expect("caller with a stream id");
    let _without_id = connect(port, None).expect("caller without a stream id");
    listener.accept_timeout(ACCEPT_TIMEOUT).expect("accept 1");
    listener.accept_timeout(ACCEPT_TIMEOUT).expect("accept 2");
}

#[test]
fn rules_handle_outlives_the_listener_and_reports_closed() {
    require_loopback!();
    let (listener, _port) = listener();
    let rules = listener.rules();
    rules.set("a", ListenRule::Accept).unwrap();
    drop(listener);
    assert!(matches!(
        rules.set("a", ListenRule::Accept),
        Err(ListenRuleError::Closed)
    ));
    assert!(matches!(
        rules.set_default(ListenRule::Accept),
        Err(ListenRuleError::Closed)
    ));
    assert!(!rules.remove("a"));
}

#[test]
fn cross_thread_cancel_closes_the_rules_too() {
    require_loopback!();
    let (mut listener, _port) = listener();
    let cancel = listener.cancel_handle();
    let rules = listener.rules();
    let accept = std::thread::spawn(move || listener.accept().map(|_| ()));
    cancel.cancel();
    // The cancel may land before the accept starts; either way it fails.
    assert!(accept.join().expect("join accept").is_err());
    assert!(matches!(
        rules.set("a", ListenRule::Accept),
        Err(ListenRuleError::Closed)
    ));
}

#[test]
fn edits_race_handshakes_without_deadlock() {
    require_loopback!();
    let (mut listener, port) = listener();
    let rules = listener.rules();
    let done = Arc::new(AtomicBool::new(false));

    let flipper = {
        let done = done.clone();
        std::thread::spawn(move || {
            let mut flips = 0u32;
            while flips < 200 || !done.load(Ordering::Acquire) {
                let rule = if flips.is_multiple_of(2) {
                    ListenRule::Reject(RejectReason::Overload)
                } else {
                    ListenRule::Accept
                };
                rules.set_default(rule).unwrap();
                flips += 1;
                // RwLock prefers writers: let the hook's read in.
                std::thread::yield_now();
            }
        })
    };
    let drain = {
        let done = done.clone();
        std::thread::spawn(move || {
            // Accepted sockets stay open too: closing one right away can
            // break a caller that is still finishing its connect.
            let mut accepted = Vec::new();
            loop {
                match listener.accept_timeout(Duration::from_millis(200)) {
                    Ok(conn) => accepted.push(conn),
                    Err(AcceptError::TimedOut) if done.load(Ordering::Acquire) => {
                        return (listener, accepted);
                    }
                    Err(AcceptError::TimedOut) => {}
                    Err(e) => panic!("accept: {e:?}"),
                }
            }
        })
    };

    // Admitted callers stay open until the drain has finished: a caller that
    // vanishes right after its handshake can leave the accepted socket broken
    // before `accept_timeout` configures it, which that call reports as an
    // error.
    let mut admitted = Vec::new();
    for i in 0..20 {
        match connect(port, Some("publish:amber")) {
            Ok(sock) => admitted.push(sock),
            Err(SrtConnectError::Rejected {
                reason: RejectReason::Overload,
                ..
            }) => {}
            Err(e) => panic!("caller {i}: {e:?}"),
        }
    }
    done.store(true, Ordering::Release);
    flipper.join().expect("join flipper");
    // The listener outlives the flipper's last edit: dropping it first would
    // make that edit report `Closed`.
    let kept = drain.join().expect("join drain");
    drop((admitted, kept));
}
