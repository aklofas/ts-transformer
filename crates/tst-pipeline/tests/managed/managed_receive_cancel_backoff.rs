//! A cancel that lands while a `ManagedRecvTransport` is waiting out its
//! reconnect backoff must interrupt the wait, not ride it out — the
//! receive-side counterpart of the send side's interruptible backoff.
//! With the default exponential policy the wait can be 10 s,
//! which is not "prompt" for a Ctrl-C.

use crate::common::{ScriptedRecv, broken};

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};
use tst_core::transport::{RecvTransport, TransportError};
use tst_pipeline::{BackoffStrategy, ManagedRecvTransport, ReconnectPolicy};
use tst_test_helpers::timing::scaled;

/// Raises `reached` when the receive loop logs that it is about to wait out
/// the backoff. That event follows the loop's last cancel check, so a cancel
/// fired after it can only be honoured by the wait itself.
struct BackoffProbe {
    reached: Arc<AtomicBool>,
}

struct MessageIs<'a>(&'a str, bool);

impl Visit for MessageIs<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" && format!("{value:?}") == self.0 {
            self.1 = true;
        }
    }
}

impl Subscriber for BackoffProbe {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut v = MessageIs("backoff before next attempt", false);
        event.record(&mut v);
        if v.1 {
            self.reached.store(true, Ordering::SeqCst);
        }
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

#[test]
fn cancel_interrupts_the_backoff_wait() {
    let factory_calls = Arc::new(AtomicU32::new(0));
    let calls = Arc::clone(&factory_calls);
    let factory: Box<dyn FnMut() -> Result<ScriptedRecv, TransportError> + Send> =
        Box::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(broken("peer still down"))
        });

    // A 60 s constant backoff: the first reconnect attempt cannot begin
    // before 60 s. The cancel lands once the loop has committed to that
    // wait, and the 30 s bound below is half the wait it must cut short, so it needs
    // no scaling.
    let policy = ReconnectPolicy {
        max_attempts: Some(3),
        backoff: BackoffStrategy::Constant(Duration::from_secs(60)),
        ..Default::default()
    };
    // The canceller waits for the backoff event instead of guessing with a
    // sleep: a cancel that landed earlier would be caught by the loop's own
    // cancel check and pass even against a wait that cannot be interrupted.
    let reached = Arc::new(AtomicBool::new(false));
    let probe = BackoffProbe {
        reached: Arc::clone(&reached),
    };
    let mut managed = ManagedRecvTransport::new(
        ScriptedRecv::failing(broken("dead on arrival")),
        factory,
        policy,
    );
    let cancel = managed.cancel_handle().expect("managed cancel handle");

    let canceller = std::thread::spawn(move || {
        let deadline = Instant::now() + scaled(Duration::from_secs(10));
        while !reached.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                // Release the receive first, then report.
                cancel.cancel();
                panic!("the managed transport never reached its backoff wait");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        cancel.cancel();
    });

    let start = Instant::now();
    let mut buf = [0u8; 1316];
    let result = tracing::subscriber::with_default(probe, || managed.recv_bytes(&mut buf));
    let elapsed = start.elapsed();
    canceller.join().expect("canceller thread");

    assert!(
        matches!(result, Err(TransportError::ExplicitClose)),
        "expected ExplicitClose after a cross-thread cancel, got {result:?}"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "cancel did not interrupt the backoff wait: recv_bytes returned after {elapsed:?}"
    );
    assert_eq!(
        factory_calls.load(Ordering::SeqCst),
        0,
        "the factory must not be called once the cancel has landed"
    );
}
