//! The tst-core conformance kit over `ManagedTransport` /
//! `ManagedRecvTransport` wrapping an in-memory mock whose behaviour is the
//! inner contract (parks until its handle fires, then
//! `ExplicitClose`; `Ok(0)` on an empty buffer).
//!
//! One row differs from the bare-transport defaults, documented in the
//! `tst_core::transport` table: the receive wrapper answers `ExplicitClose`
//! after its OWN `close()` (`managed_receive.rs` "close() is a
//! caller-initiated path"), where the send wrapper answers `Closed`. Both
//! answer `ExplicitClose` to a cancel (`ManagedTransport` latches a
//! cancel-only flag that `latched_error` reads at every `closed`-latch
//! exit).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use tst_core::transport::conformance::{
    self as kit, BrokenSource, PostCloseKind, RecvOptions, RecvRow, SendPark, SendRow,
};
use tst_core::transport::{BrokenCause, RecvTransport, Transport, TransportCancel, TransportError};
use tst_pipeline::{
    BackoffStrategy, ManagedDemuxReceiver, ManagedDemuxReceiverConfig, ManagedRecvTransport,
    ManagedTransport, OverflowPolicy, ReconnectMode, ReconnectPolicy, ShellErrorKind,
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
    policy_in(ReconnectMode::Blocking)
}

fn policy_in(mode: ReconnectMode) -> ReconnectPolicy {
    ReconnectPolicy {
        // One attempt, zero backoff: the kit never wants a reconnect, and the
        // factories below refuse anyway — the budget just keeps a refusal
        // from looping.
        max_attempts: Some(1),
        backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
        gap_buffer_capacity: 64,
        overflow_policy: OverflowPolicy::DropOldest,
        mode,
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
    managed_send_on(&Wire::new(), policy())
}

fn managed_send_on(wire: &Arc<Wire>, policy: ReconnectPolicy) -> ManagedTransport<MockSend> {
    ManagedTransport::new(
        MockSend {
            wire: Arc::clone(wire),
            alive: true,
        },
        refuse,
        policy,
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
    // `NotProducible` takes no caller-supplied reason string yet.
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
    for mode in [ReconnectMode::Blocking, ReconnectMode::Background] {
        let managed = || managed_send_on(&Wire::new(), policy_in(mode));
        kit::send_cancel_during_park_is_explicit_close(managed(), SendPark::Loop);
        kit::send_cancel_before_op_is_explicit_close(managed());
    }
}

/// What the invocation that was parked in the inner send reported, and what
/// the wrapper was left holding.
struct Interrupted {
    result: Result<(), TransportError>,
    gap_len: u64,
    alive: bool,
}

/// PROVABLE park: the inner mock's `in_call` shows the managed `send_bytes`
/// is INSIDE `MockSend::send_bytes` when the MANAGED cancel handle fires; the
/// result of that same invocation is returned — no retry loop, no `SETTLE`.
/// Cancel-after-completion is the permitted race and is not what this
/// exercises: the send cannot complete while the inner parks.
fn cancel_a_send_parked_in_the_inner(policy: ReconnectPolicy) -> Interrupted {
    let wire = Wire::new();
    let mut m = managed_send_on(&wire, policy);
    let handle = Transport::cancel_handle(&m).expect("ManagedTransport has a cancel handle");
    let stats = m.stats_handle();
    // Firing the inner's own handle releases the parked send whatever the
    // wrapper does, so a failure below never leaves the worker parked.
    let rescue = WireCancel(Arc::clone(&wire));

    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let r = m.send_bytes(&[0x47; 188]);
        let _ = tx.send(r);
        m // kept alive until the gauges below are read
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while wire.in_call.load(Ordering::SeqCst) != 1 {
        if Instant::now() >= deadline {
            rescue.cancel();
            panic!("the managed send never entered the inner send_bytes");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    handle.cancel();
    let Ok(result) = rx.recv_timeout(Duration::from_secs(10)) else {
        rescue.cancel();
        panic!("the cancelled send did not return within 10 s");
    };
    assert_eq!(
        wire.in_call.load(Ordering::SeqCst),
        0,
        "the inner call exited"
    );
    // The result has been sent, so the worker is past its last blocking
    // call: this join is bounded.
    let m = worker.join().expect("send worker did not panic");
    Interrupted {
        result,
        gap_len: stats.stats().expect("gap lock not poisoned").gap_len,
        alive: m.is_alive(),
    }
}

/// Both modes: the cancel is reported by the invocation it interrupted, and
/// the interrupted message is not left queued behind the cancelled wrapper
/// (in `Background` the failure is an `Ok(())` for a message handed to a
/// worker that sees the latch and exits).
#[test]
fn managed_send_parked_in_inner_is_interrupted_in_place() {
    for mode in [ReconnectMode::Blocking, ReconnectMode::Background] {
        let o = cancel_a_send_parked_in_the_inner(policy_in(mode));
        assert!(
            matches!(o.result, Err(TransportError::ExplicitClose)),
            "{mode:?}: got {:?} (gap_len = {})",
            o.result,
            o.gap_len
        );
        assert_eq!(o.gap_len, 0, "{mode:?}: queued after the cancel");
        assert!(!o.alive, "{mode:?}: alive after the cancel");
    }
}

/// The cancel outranks a full `Reject` buffer: with nowhere to queue, the
/// interrupted invocation still reports the cancel, not `Backpressure`.
#[test]
fn managed_send_cancel_outranks_a_full_reject_buffer() {
    for mode in [ReconnectMode::Blocking, ReconnectMode::Background] {
        let o = cancel_a_send_parked_in_the_inner(ReconnectPolicy {
            gap_buffer_capacity: 0,
            overflow_policy: OverflowPolicy::Reject,
            ..policy_in(mode)
        });
        assert!(
            matches!(o.result, Err(TransportError::ExplicitClose)),
            "{mode:?}: got {:?}",
            o.result
        );
    }
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

/// Run `recv` on its own thread, prove it is parked INSIDE
/// `MockRecv::recv_bytes`, fire `cancel`, and hand back what that same
/// invocation returned together with the receiver it ran on.
fn cancel_a_parked_recv<R, O>(
    wire: &Arc<Wire>,
    mut receiver: R,
    cancel: Arc<dyn TransportCancel + Send + Sync>,
    recv: impl FnOnce(&mut R) -> O + Send + 'static,
) -> (O, R)
where
    R: Send + 'static,
    O: Send + 'static,
{
    // Firing the inner's own handle releases the parked receive whatever
    // the wrapper does, so a failure below never leaves the worker parked.
    let rescue = WireCancel(Arc::clone(wire));
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let out = recv(&mut receiver);
        let _ = tx.send(());
        (out, receiver)
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while wire.in_call.load(Ordering::SeqCst) != 1 {
        if Instant::now() >= deadline {
            rescue.cancel();
            panic!("the managed receive never entered the inner recv_bytes");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    cancel.cancel();
    if rx.recv_timeout(Duration::from_secs(10)).is_err() {
        rescue.cancel();
        panic!("the cancelled receive did not return within 10 s");
    }
    // The receive has returned, so this join is bounded.
    worker.join().expect("recv worker did not panic")
}

/// The cancel is terminal for the receive it interrupts: liveness reads
/// false as soon as that call has returned, without a further receive
/// having to run into the entry gate first.
#[test]
fn managed_recv_parked_in_inner_is_not_alive_after_the_cancel() {
    let wire = Wire::new();
    let m = managed_recv(&wire);
    assert!(m.is_alive(), "precondition: alive before the receive");
    let handle = RecvTransport::cancel_handle(&m).expect("managed recv has a cancel handle");

    let (result, m) = cancel_a_parked_recv(&wire, m, handle, |m| {
        let mut buf = [0u8; 1316];
        m.recv_bytes(&mut buf)
    });

    assert!(
        matches!(result, Err(TransportError::ExplicitClose)),
        "got {result:?}"
    );
    assert!(!m.is_alive(), "alive after its receive was cancelled");
}

/// `ManagedDemuxReceiver` delegates liveness to the managed transport, so
/// the same holds one layer up.
#[test]
fn managed_demux_receiver_is_not_alive_after_the_cancel() {
    let wire = Wire::new();
    let shell =
        ManagedDemuxReceiver::new(managed_recv(&wire), ManagedDemuxReceiverConfig::default());
    assert!(shell.is_alive(), "precondition: alive before the receive");
    let handle = shell
        .cancel_handle()
        .expect("managed demux receiver has a cancel handle");

    let (result, shell) = cancel_a_parked_recv(&wire, shell, handle, |s| s.recv_event());

    let err = result.expect_err("the parked recv_event reports the cancel");
    assert_eq!(err.kind, ShellErrorKind::Closed, "got {err:?}");
    assert!(!shell.is_alive(), "alive after its receive was cancelled");
}
