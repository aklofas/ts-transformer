//! A cancel that lands while a `ManagedRecvTransport` is waiting out its
//! reconnect backoff must interrupt the wait, not ride it out — the
//! receive-side counterpart of the send side's interruptible backoff
//! (PR #158). With the default exponential policy the wait can be 10 s,
//! which is not "prompt" for a Ctrl-C.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tst_core::transport::{BrokenCause, RecvTransport, TransportError};
use tst_pipeline::{BackoffStrategy, ManagedRecvTransport, ReconnectPolicy};

struct DeadInner;

impl RecvTransport for DeadInner {
    fn recv_bytes(&mut self, _buf: &mut [u8]) -> Result<usize, TransportError> {
        Err(TransportError::Broken {
            msg: "dead on arrival".into(),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        })
    }

    fn max_payload(&self) -> usize {
        1316
    }

    fn is_alive(&self) -> bool {
        false
    }
}

#[test]
fn cancel_interrupts_the_backoff_wait() {
    let factory_calls = Arc::new(AtomicU32::new(0));
    let calls = Arc::clone(&factory_calls);
    let factory: Box<dyn FnMut() -> Result<DeadInner, TransportError> + Send> =
        Box::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(TransportError::Broken {
                msg: "peer still down".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            })
        });

    // A 5 s constant backoff: the first reconnect attempt cannot begin
    // before 5 s. The cancel lands at 150 ms.
    let policy = ReconnectPolicy {
        max_attempts: Some(3),
        backoff: BackoffStrategy::Constant(Duration::from_secs(5)),
        ..Default::default()
    };
    let mut managed = ManagedRecvTransport::new(DeadInner, factory, policy);
    let cancel = managed.cancel_handle().expect("managed cancel handle");

    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        cancel.cancel();
    });

    let start = Instant::now();
    let mut buf = [0u8; 1316];
    let result = managed.recv_bytes(&mut buf);
    let elapsed = start.elapsed();
    canceller.join().expect("canceller thread");

    assert!(
        matches!(result, Err(TransportError::ExplicitClose)),
        "expected ExplicitClose after a cross-thread cancel, got {result:?}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "cancel did not interrupt the backoff wait: recv_bytes returned after {elapsed:?}"
    );
    assert_eq!(
        factory_calls.load(Ordering::SeqCst),
        0,
        "the factory must not be called once the cancel has landed"
    );
}
