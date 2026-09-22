//! ARCH-08: the receive-side decorator counts factory CALLS (attempts)
//! next to successful rebuilds (reconnects), so the bindings stop wrapping
//! the factory in a counting closure of their own. The send side already
//! counted both (`ManagedTransportStats::reconnect_attempts`); this makes
//! the two sides symmetric and exposes the counter lock-free.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tst_pipeline::{
    BackoffStrategy, BrokenCause, ManagedDemuxReceiver, ManagedDemuxReceiverConfig,
    ManagedRecvTransport, ReconnectPolicy, RecvTransport, TransportError,
};

/// Bound on anything that must NOT wait on the thread owning the
/// decorator. Deliberately loose: it never asserts that a read was *fast*,
/// only that it happened at all — the failure it catches (a handle that
/// queued behind the parked owner) blocks forever on this scale, and these
/// tests also run under ASan/TSan where scheduling is 5-15x slower.
const PROMPT: Duration = Duration::from_secs(10);

/// Latch-and-poll with a bounded watchdog — never a wall-clock assert.
fn wait_for(deadline: Duration, f: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    f()
}

fn broken(msg: &str) -> TransportError {
    TransportError::Broken {
        msg: msg.into(),
        errno_code: None,
        cause: BrokenCause::Unspecified,
    }
}

/// A scripted inner: `good == false` breaks on every recv (forces the
/// reconnect path); `good == true` delivers four bytes.
struct Scripted {
    good: bool,
}

impl RecvTransport for Scripted {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if self.good {
            buf[..4].copy_from_slice(b"data");
            Ok(4)
        } else {
            Err(broken("broken (test)"))
        }
    }

    fn max_payload(&self) -> usize {
        1316
    }

    fn is_alive(&self) -> bool {
        true
    }
}

fn fail_twice_then_succeed(
    calls: Arc<AtomicU32>,
) -> Box<dyn FnMut() -> Result<Scripted, TransportError> + Send> {
    Box::new(move || {
        let n = calls.fetch_add(1, Ordering::SeqCst);
        if n < 2 {
            Err(broken("factory down"))
        } else {
            Ok(Scripted { good: true })
        }
    })
}

fn fast_policy() -> ReconnectPolicy {
    ReconnectPolicy {
        max_attempts: Some(10),
        backoff: BackoffStrategy::Constant(Duration::ZERO),
        ..Default::default()
    }
}

#[test]
fn attempts_count_factory_calls_and_reconnects_count_successes() {
    let calls = Arc::new(AtomicU32::new(0));
    let mut managed = ManagedRecvTransport::new(
        Scripted { good: false },
        fail_twice_then_succeed(Arc::clone(&calls)),
        fast_policy(),
    );
    let attempts = managed.attempts_handle();
    let reconnects = managed.reconnects_handle();
    assert_eq!(
        (
            attempts.load(Ordering::Acquire),
            reconnects.load(Ordering::Acquire)
        ),
        (0, 0),
        "nothing has been attempted before the first recv"
    );

    let mut buf = [0u8; 16];
    let n = managed
        .recv_bytes(&mut buf)
        .expect("the 3rd factory call rebuilds a working inner");
    assert_eq!(&buf[..n], b"data");
    assert_eq!(
        attempts.load(Ordering::Acquire),
        3,
        "two failed factory calls + one success"
    );
    assert_eq!(
        reconnects.load(Ordering::Acquire),
        1,
        "only the installed rebuild counts"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "attempts == real factory invocations (no double count, none missed)"
    );
}

/// The demux shell snapshots the transport's counter before the move, the
/// same way it snapshots `reconnects` / `reconnecting` — one `Arc`, not a
/// copy.
#[test]
fn managed_demux_receiver_exposes_the_transport_attempts_counter() {
    let managed = ManagedRecvTransport::new(
        Scripted { good: true },
        fail_twice_then_succeed(Arc::new(AtomicU32::new(0))),
        fast_policy(),
    );
    let from_transport = managed.attempts_handle();
    let rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());
    assert!(
        Arc::ptr_eq(&from_transport, &rx.attempts_handle()),
        "the shell must hand out the transport's own counter"
    );
}

/// The handle is lock-free in the sense the bindings need: obtained before
/// the decorator moves onto another thread, it stays readable while that
/// thread is parked deep inside `recv_bytes` — the shape of a receive
/// parked in libsrt that only a cancel can return (A1.5). The *value* is
/// the load-bearing part: the in-flight attempt is already counted, which
/// pins the bump BEFORE the factory call rather than after it.
#[test]
fn attempts_handle_reads_while_the_owner_is_parked_in_the_factory() {
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let factory = {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        let mut first = true;
        Box::new(move || -> Result<Scripted, TransportError> {
            if first {
                first = false;
                entered.store(true, Ordering::SeqCst);
                assert!(
                    wait_for(PROMPT, || release.load(Ordering::SeqCst)),
                    "parked factory was never released — watchdog"
                );
                return Err(broken("factory down"));
            }
            Ok(Scripted { good: true })
        }) as Box<dyn FnMut() -> Result<Scripted, TransportError> + Send>
    };

    let mut managed = ManagedRecvTransport::new(Scripted { good: false }, factory, fast_policy());
    // Obtain BEFORE the move — the only way a binding can reach it.
    let attempts = managed.attempts_handle();
    let mut buf = [0u8; 16];
    let owner = std::thread::spawn(move || {
        let n = managed.recv_bytes(&mut buf).expect("2nd factory call wins");
        assert_eq!(&buf[..n], b"data");
    });

    assert!(
        wait_for(PROMPT, || entered.load(Ordering::SeqCst)),
        "the factory was never entered"
    );
    let reader = {
        let attempts = Arc::clone(&attempts);
        std::thread::spawn(move || attempts.load(Ordering::Acquire))
    };
    assert!(
        wait_for(PROMPT, || reader.is_finished()),
        "reading the attempts handle waited on the parked owner"
    );
    assert_eq!(
        reader.join().expect("reader thread"),
        1,
        "the in-flight factory call is already counted — the bump is before the call"
    );

    release.store(true, Ordering::SeqCst);
    owner.join().expect("owner thread");
    assert_eq!(
        attempts.load(Ordering::Acquire),
        2,
        "the second call is counted too"
    );
}

/// Compile-time pin of the produced signature: a plain `Arc<AtomicU64>`,
/// so a binding can hold it with no knowledge of the decorator.
#[test]
fn attempts_handles_are_plain_shared_atomics() {
    let managed = ManagedRecvTransport::new(
        Scripted { good: true },
        fail_twice_then_succeed(Arc::new(AtomicU32::new(0))),
        fast_policy(),
    );
    let _: Arc<AtomicU64> = managed.attempts_handle();
    let rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());
    let _: Arc<AtomicU64> = rx.attempts_handle();
}
