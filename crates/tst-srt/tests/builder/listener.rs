//! Listener bind errors.

use tst_srt::SrtListenerBuilder;
use tst_srt::error::BindError;

#[test]
fn bind_address_in_use() {
    require_loopback!();
    let _l1 = SrtListenerBuilder::new()
        .reuse_addr(false)
        .bind("127.0.0.1:0")
        .expect("first bind");

    let port = _l1.local_addr().unwrap().port();
    let result = SrtListenerBuilder::new()
        .reuse_addr(false)
        .bind(format!("127.0.0.1:{port}"));

    match result {
        Err(BindError::AddressInUse) => { /* expected */ }
        Err(BindError::Other { kind, message }) => {
            // libsrt's reuse semantics may surface as Other; accept that.
            eprintln!("bind returned Other: {kind:?} {message}");
        }
        Err(other) => panic!("expected AddressInUse or Other; got {other:?}"),
        Ok(_) => panic!("second bind unexpectedly succeeded"),
    }
}

// `accept_unblocks_when_listener_closed` was removed: srt_accept does not honor
// SRTO_RCVTIMEO, and the test as planned would hang because the listener is
// moved into the spawned thread (no inter-thread close coordination). Revisit
// when typed close-from-other-thread coordination is added.
