//! WP-C1 — the tst-core conformance kit over `ManagedTransport` /
//! `ManagedRecvTransport` wrapping an in-memory mock whose behaviour is the
//! post-Arc-2 inner contract (parks until its handle fires, then
//! `ExplicitClose`; `Ok(0)` on an empty buffer).
//!
//! Two rows differ from the bare-transport defaults, both documented in the
//! `tst_core::transport` table: the receive wrapper answers `ExplicitClose`
//! after its OWN `close()` (`managed_receive.rs` "close() is a
//! caller-initiated path"), and — until WP-C2 — the send wrapper answers
//! `Closed` to a cancel that lands during a parked send (the inner's
//! `ExplicitClose` falls through `send_managed`'s `Err(_)` arm into
//! `reconnect_and_drain`, whose `closed` check returns `Closed`).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tst_core::transport::conformance::{
    self as kit, BrokenSource, PostCloseKind, RecvOptions, RecvRow, SendPark, SendRow,
};
use tst_core::transport::{BrokenCause, RecvTransport, Transport, TransportCancel, TransportError};
use tst_pipeline::{
    BackoffStrategy, ManagedRecvTransport, ManagedTransport, OverflowPolicy, ReconnectMode,
    ReconnectPolicy,
};

struct Wire {
    queue: Mutex<VecDeque<Vec<u8>>>,
    cv: Condvar,
    cancelled: AtomicBool,
    /// Calls currently INSIDE `MockSend::send_bytes` / `MockRecv::recv_bytes`
    /// (RAII-guarded): lets a test prove the managed wrapper's invocation was
    /// parked in the INNER call when the cancel fired.
    in_call: AtomicUsize,
}

struct InCall<'a>(&'a AtomicUsize);
impl<'a> InCall<'a> {
    fn enter(c: &'a AtomicUsize) -> Self {
        c.fetch_add(1, Ordering::SeqCst);
        Self(c)
    }
}
impl Drop for InCall<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Wire {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            cancelled: AtomicBool::new(false),
            in_call: AtomicUsize::new(0),
        })
    }
    fn push(&self, b: &[u8]) {
        self.queue.lock().unwrap().push_back(b.to_vec());
        self.cv.notify_all();
    }
}

struct WireCancel(Arc<Wire>);

impl TransportCancel for WireCancel {
    fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        self.0.cv.notify_all();
    }
    fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }
}

/// Receive mock: parks on the queue until fed or cancelled.
struct MockRecv {
    wire: Arc<Wire>,
    alive: bool,
}

impl RecvTransport for MockRecv {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if buf.is_empty() {
            return Ok(0);
        }
        // Cancel before liveness so the reason stays sticky across calls.
        if self.wire.cancelled.load(Ordering::SeqCst) {
            self.alive = false;
            return Err(TransportError::ExplicitClose);
        }
        if !self.alive {
            return Err(TransportError::Closed);
        }
        let _in = InCall::enter(&self.wire.in_call);
        let mut q = self.wire.queue.lock().unwrap();
        loop {
            if self.wire.cancelled.load(Ordering::SeqCst) {
                self.alive = false;
                return Err(TransportError::ExplicitClose);
            }
            if let Some(m) = q.pop_front() {
                let n = m.len().min(buf.len());
                buf[..n].copy_from_slice(&m[..n]);
                return Ok(n);
            }
            q = self
                .wire
                .cv
                .wait_timeout(q, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }
    fn max_payload(&self) -> usize {
        1316
    }
    fn is_alive(&self) -> bool {
        self.alive && !self.wire.cancelled.load(Ordering::SeqCst)
    }
    fn close(&mut self) {
        self.alive = false;
    }
    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        Some(Arc::new(WireCancel(Arc::clone(&self.wire))))
    }
}

/// Send mock: PARKS in `send_bytes` until its handle fires (the libsrt
/// full-send-buffer shape), then reports `ExplicitClose`. Parking is what
/// makes the kit's cancel row land INSIDE the inner call rather than between
/// calls — the case the managed wrapper mishandles on main.
struct MockSend {
    wire: Arc<Wire>,
    alive: bool,
}

impl Transport for MockSend {
    fn send_bytes(&mut self, _msg: &[u8]) -> Result<(), TransportError> {
        if self.wire.cancelled.load(Ordering::SeqCst) {
            self.alive = false;
            return Err(TransportError::ExplicitClose);
        }
        if !self.alive {
            return Err(TransportError::Closed);
        }
        let _in = InCall::enter(&self.wire.in_call);
        let mut q = self.wire.queue.lock().unwrap();
        loop {
            if self.wire.cancelled.load(Ordering::SeqCst) {
                self.alive = false;
                return Err(TransportError::ExplicitClose);
            }
            q = self
                .wire
                .cv
                .wait_timeout(q, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }
    fn max_payload(&self) -> usize {
        1316
    }
    fn is_alive(&self) -> bool {
        self.alive && !self.wire.cancelled.load(Ordering::SeqCst)
    }
    fn close(&mut self) {
        self.alive = false;
    }
    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        Some(Arc::new(WireCancel(Arc::clone(&self.wire))))
    }
}

fn policy() -> ReconnectPolicy {
    ReconnectPolicy {
        // One attempt, zero backoff: the kit never wants a reconnect, and the
        // factories below refuse anyway — the budget just keeps a refusal
        // from looping.
        max_attempts: Some(1),
        backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
        gap_buffer_capacity: 64,
        overflow_policy: OverflowPolicy::DropOldest,
        mode: ReconnectMode::Blocking,
    }
}

fn refuse<T>() -> Result<T, TransportError> {
    Err(TransportError::Broken {
        msg: "conformance: no reconnect".into(),
        errno_code: None,
        cause: BrokenCause::Unspecified,
    })
}

fn managed_send() -> ManagedTransport<MockSend> {
    managed_send_on(&Wire::new())
}

fn managed_send_on(wire: &Arc<Wire>) -> ManagedTransport<MockSend> {
    ManagedTransport::new(
        MockSend {
            wire: Arc::clone(wire),
            alive: true,
        },
        refuse,
        policy(),
    )
}

fn managed_recv(wire: &Arc<Wire>) -> ManagedRecvTransport<MockRecv> {
    ManagedRecvTransport::new(
        MockRecv {
            wire: Arc::clone(wire),
            alive: true,
        },
        Box::new(refuse),
        policy(),
    )
}

#[test]
fn managed_send_contract_all_but_the_cancel_rows() {
    // NotProducible on the wrappers for a reason the kit's skip line does not
    // state: it prints "(Broken not producible on this transport)", but a
    // broken inner IS producible here — it is exactly what the wrapper
    // reconnects through, and once the budget is exhausted the terminal it
    // reports is `Closed`, not `Broken`. So the row cannot assert what it
    // wants to and is skipped. Divergence recorded rather than papered over;
    // WP-D should give `NotProducible` a caller-supplied reason string.
    kit::assert_send_rows(
        managed_send,
        BrokenSource::NotProducible,
        kit::SendOptions::default(),
        &SendRow::all_except(&[
            SendRow::CancelDuringParkIsExplicitClose,
            SendRow::CancelBeforeOpIsExplicitClose,
        ]),
    );
}

#[test]
fn managed_send_cancel_rows_are_explicit_close() {
    kit::send_cancel_during_park_is_explicit_close(managed_send(), SendPark::Loop);
    kit::send_cancel_before_op_is_explicit_close(managed_send());
}

/// PROVABLE park (handoff validation 2026-09-18, A2-V4): the inner mock's
/// `in_call` shows the managed `send_bytes` is INSIDE `MockSend::send_bytes`
/// when the MANAGED cancel handle fires; the result of that same invocation
/// is asserted — no retry loop, no `SETTLE`. Cancel-after-completion is the
/// permitted race and is not what this test exercises: the send cannot
/// complete while the inner parks.
#[test]
fn managed_send_parked_in_inner_is_interrupted_in_place() {
    let wire = Wire::new();
    let mut m = managed_send_on(&wire);
    let handle = Transport::cancel_handle(&m).expect("ManagedTransport has a cancel handle");
    let worker = std::thread::spawn(move || m.send_bytes(&[0x47; 188]));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while wire.in_call.load(Ordering::SeqCst) != 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "the managed send never entered the inner send_bytes"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    handle.cancel();
    let result = worker.join().expect("send worker did not panic");
    assert!(
        matches!(result, Err(TransportError::ExplicitClose)),
        "got {result:?}"
    );
    assert_eq!(
        wire.in_call.load(Ordering::SeqCst),
        0,
        "the inner call exited"
    );
}

#[test]
fn managed_recv_contract() {
    // A FRESH wire per factory call: the rows that cancel latch `cancelled`
    // for good, so a shared wire would hand the next row an already-cancelled
    // inner. `feed` targets whichever wire the current row is holding.
    let current: Mutex<Option<Arc<Wire>>> = Mutex::new(None);
    let factory = || {
        let w = Wire::new();
        *current.lock().unwrap() = Some(Arc::clone(&w));
        managed_recv(&w)
    };
    let feed = |b: &[u8]| {
        current
            .lock()
            .unwrap()
            .as_ref()
            .expect("a live wire")
            .push(b)
    };
    kit::assert_recv_rows(
        factory,
        feed,
        BrokenSource::NotProducible,
        RecvOptions {
            post_close: PostCloseKind::ExplicitClose,
        },
        RecvRow::ALL,
    );
    kit::recv_max_payload_ge_ceiling(&managed_recv(&Wire::new()), 1316);
}
