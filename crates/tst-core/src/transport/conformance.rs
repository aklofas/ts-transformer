//! Transport conformance kit — the executable form of the cancel / close /
//! liveness contract table in [`super`]'s module docs.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Each transport crate adds one `tests/conformance.rs` that hands the kit
//! a loopback *factory* (a fresh, connected transport per call — every row
//! consumes one) and, for receivers, a *feed* closure. The feed contract is
//! deliberately weak: after `feed(bytes)` the transport under test must
//! deliver *something* on its next successful `recv_bytes`; byte fidelity
//! is not asserted here (stream transports split messages, RTSP mounts
//! re-mux) — the pipeline round-trip tests pin that.
//!
//! A "parked" op is modelled as a worker thread looping the op and treating
//! `Backpressure` (and, for receivers, `Ok(n)`) as "keep going", so a
//! transport with a configured timeout and one that truly blocks look the
//! same; each row asserts the loop's TERMINAL result. Every wait is bounded
//! by [`WATCHDOG`] and FAILS with the row's name (`conformance: <row>:
//! <what>`); a worker still parked after a failed verdict is abandoned,
//! never joined. No row asserts elapsed time — the watchdog is a failure
//! bound only, never a duration assertion on the success path.
//!
//! **The factory must hand back FRESH underlying state per call.** Rows
//! latch things for good (the cancel rows cancel; the broken rows break),
//! so a factory that reuses one connection would hand the next row an
//! already-cancelled transport. The kit's own
//! `shared_wire_factory_is_rejected_by_is_cancelled_flips` self-test pins
//! that a reusing factory is rejected rather than silently passing.
//!
//! Rows in the aggregates: `post_close_is_closed`, `close_twice_is_ok`,
//! `cancel_during_park_is_explicit_close`,
//! `cancel_before_op_is_explicit_close`, `is_cancelled_flips`,
//! `not_alive_after_cancel`, `not_alive_after_broken` (driven by a
//! [`BrokenSource`]; `NotProducible` is a visible skip), and (recv only)
//! `empty_recv_is_noop` and `peer_eof_is_not_a_cancel`. Standalone, because
//! it needs the protocol ceiling a factory cannot express:
//! [`recv_max_payload_ge_ceiling`]. The fed-recv row waits up to
//! [`WATCHDOG`] for the first delivered byte and every recv loop treats
//! extra `Ok(n)` as "keep going", so a slow start (RIST's ~2 s handshake)
//! or duplicate delivery never fails a row.
//!
//! `is_cancelled()` is a CANCEL latch, never a liveness proxy: the
//! `peer_eof_is_not_a_cancel` row pins that a peer-side break leaves it
//! `false`. The bindings relabel a caller-initiated end from that bit, so a
//! transport that aliases liveness there turns every clean peer EOF into a
//! reported caller close.

use super::{RecvTransport, Transport, TransportCancel, TransportError};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Upper bound on every wait in the kit. A row that reaches it fails; it
/// never hangs the test binary.
pub const WATCHDOG: Duration = Duration::from_secs(10);

/// After a worker has latched "about to call", the time it is given to
/// actually reach the blocking call before the row fires the cancel. Not
/// an assertion — a cancel that lands a little early is still observed at
/// the op's next entry check.
const SETTLE: Duration = Duration::from_millis(50);

/// One MPEG-TS null packet (PID `0x1FFF`, no adaptation field, payload
/// `0xFF`). Every transport in the project carries it unchanged and the
/// RTP receiver's MP2T shape guard (`len % 188 == 0`, leading `0x47`)
/// accepts it — the one probe the feed contract can rely on everywhere.
#[must_use]
pub fn probe_packet() -> [u8; 188] {
    let mut p = [0xFFu8; 188];
    p[0] = 0x47;
    p[1] = 0x1F;
    p[2] = 0xFF;
    p[3] = 0x10;
    p
}

/// What a transport's OWN `close()` makes later ops return.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostCloseKind {
    /// The bare transports and `ManagedTransport`.
    Closed,
    /// `ManagedRecvTransport` (its own close is a caller-initiated end).
    ExplicitClose,
}

/// How `cancel_during_park_is_explicit_close` drives the send side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendPark {
    /// A worker loops `send_bytes` (retrying `Ok`/`Backpressure`) while the
    /// row cancels from the calling thread; the loop's terminal result is
    /// asserted. Works for transports that park (TCP behind a silent peer,
    /// SRT with a full buffer) AND for ones that never can (UDP): there the
    /// loop just keeps succeeding until the cancel lands.
    Loop,
    /// Cancel first, then exactly one `send_bytes`. For callers that must
    /// not spam the wire.
    NextCall,
}

/// How the `not_alive_after_broken` and `peer_eof_is_not_a_cancel` rows
/// obtain a peer-side break on this transport.
///
/// The inducer either breaks the wire around the transport (capture the
/// fixture, drop the peer, ignore the `&mut T`) or drives the transport
/// into `Broken` itself (an oversized UDP datagram through `send_bytes`);
/// the rows then assert `!is_alive()`, and only if the transport was still
/// alive after the inducer, that the next op's terminal result is `Broken`.
pub enum BrokenSource<T> {
    /// Break the wire. Called once per row that needs a break.
    Induce(Box<dyn Fn(&mut T)>),
    /// Nothing can break this transport from the outside (a bound UDP or
    /// RIST receiver, a RIST sender, a datagram sender with no peer): the
    /// rows print a visible skip line and assert nothing.
    NotProducible,
}

/// Rows of the send-side aggregate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendRow {
    /// After the transport's own `close()`, sends report the configured kind.
    PostCloseIsClosed,
    /// A second `close()` is a no-op.
    CloseTwiceIsOk,
    /// A cancel fired while a send is parked ends that send `ExplicitClose`.
    CancelDuringParkIsExplicitClose,
    /// Cancel fires BEFORE the first op; the first and the next op both report `ExplicitClose`.
    CancelBeforeOpIsExplicitClose,
    /// `is_cancelled()` is false when fresh, true after `cancel()`, and
    /// shared with a handle obtained afterwards.
    IsCancelledFlips,
    /// After a cancel and one op, `is_alive()` is false.
    NotAliveAfterCancel,
    /// After a peer-side break and one op, `is_alive()` is false and the op
    /// reported `Broken`.
    NotAliveAfterBroken,
}

impl SendRow {
    /// Every send row, in the order the aggregate runs them.
    pub const ALL: &'static [SendRow] = &[
        SendRow::PostCloseIsClosed,
        SendRow::CloseTwiceIsOk,
        SendRow::CancelDuringParkIsExplicitClose,
        SendRow::CancelBeforeOpIsExplicitClose,
        SendRow::IsCancelledFlips,
        SendRow::NotAliveAfterCancel,
        SendRow::NotAliveAfterBroken,
    ];

    /// The name that appears in the row's panic message.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            SendRow::PostCloseIsClosed => "post_close_is_closed",
            SendRow::CloseTwiceIsOk => "close_twice_is_ok",
            SendRow::CancelDuringParkIsExplicitClose => "cancel_during_park_is_explicit_close",
            SendRow::CancelBeforeOpIsExplicitClose => "cancel_before_op_is_explicit_close",
            SendRow::IsCancelledFlips => "is_cancelled_flips",
            SendRow::NotAliveAfterCancel => "not_alive_after_cancel",
            SendRow::NotAliveAfterBroken => "not_alive_after_broken",
        }
    }

    /// `ALL` minus the given rows — for a crate that runs its cancel rows
    /// under `#[ignore]` while their mechanism is still pending.
    #[must_use]
    pub fn all_except(skip: &[SendRow]) -> Vec<SendRow> {
        Self::ALL
            .iter()
            .copied()
            .filter(|r| !skip.contains(r))
            .collect()
    }
}

/// Rows of the receive-side aggregate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecvRow {
    /// After the transport's own `close()`, receives report the configured kind.
    PostCloseIsClosed,
    /// A second `close()` is a no-op.
    CloseTwiceIsOk,
    /// A cancel fired while a recv is parked ends that recv `ExplicitClose`.
    CancelDuringParkIsExplicitClose,
    /// Cancel fires BEFORE the first op; the first and the next op both report `ExplicitClose`.
    CancelBeforeOpIsExplicitClose,
    /// `is_cancelled()` is false when fresh, true after `cancel()`, and
    /// shared with a handle obtained afterwards.
    IsCancelledFlips,
    /// After a cancel and one op, `is_alive()` is false.
    NotAliveAfterCancel,
    /// An empty destination buffer is a no-op returning `Ok(0)` (X-CORR-07).
    EmptyRecvIsNoop,
    /// After a peer-side break and one op, `is_alive()` is false and the op
    /// reported `Broken`.
    NotAliveAfterBroken,
    /// A peer-side break must NOT latch `is_cancelled()` — that bit means
    /// "the caller cancelled", never "the transport is dead".
    PeerEofIsNotACancel,
}

impl RecvRow {
    /// Every receive row, in the order the aggregate runs them.
    pub const ALL: &'static [RecvRow] = &[
        RecvRow::PostCloseIsClosed,
        RecvRow::CloseTwiceIsOk,
        RecvRow::CancelDuringParkIsExplicitClose,
        RecvRow::CancelBeforeOpIsExplicitClose,
        RecvRow::IsCancelledFlips,
        RecvRow::NotAliveAfterCancel,
        RecvRow::EmptyRecvIsNoop,
        RecvRow::NotAliveAfterBroken,
        RecvRow::PeerEofIsNotACancel,
    ];

    /// The name that appears in the row's panic message.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            RecvRow::PostCloseIsClosed => "post_close_is_closed",
            RecvRow::CloseTwiceIsOk => "close_twice_is_ok",
            RecvRow::CancelDuringParkIsExplicitClose => "cancel_during_park_is_explicit_close",
            RecvRow::CancelBeforeOpIsExplicitClose => "cancel_before_op_is_explicit_close",
            RecvRow::IsCancelledFlips => "is_cancelled_flips",
            RecvRow::NotAliveAfterCancel => "not_alive_after_cancel",
            RecvRow::EmptyRecvIsNoop => "empty_recv_is_noop",
            RecvRow::NotAliveAfterBroken => "not_alive_after_broken",
            RecvRow::PeerEofIsNotACancel => "peer_eof_is_not_a_cancel",
        }
    }

    /// `ALL` minus the given rows — for a crate that runs its cancel rows
    /// under `#[ignore]` while their mechanism is still pending.
    #[must_use]
    pub fn all_except(skip: &[RecvRow]) -> Vec<RecvRow> {
        Self::ALL
            .iter()
            .copied()
            .filter(|r| !skip.contains(r))
            .collect()
    }
}

/// Per-transport knobs for the send-side aggregate.
#[derive(Clone, Copy, Debug)]
pub struct SendOptions {
    /// What the transport's own `close()` makes later sends return.
    pub post_close: PostCloseKind,
    /// How the cancel-during-park row drives the send.
    pub park: SendPark,
}

impl Default for SendOptions {
    fn default() -> Self {
        Self {
            post_close: PostCloseKind::Closed,
            park: SendPark::Loop,
        }
    }
}

/// Per-transport knobs for the receive-side aggregate.
#[derive(Clone, Copy, Debug)]
pub struct RecvOptions {
    /// What the transport's own `close()` makes later receives return.
    pub post_close: PostCloseKind,
}

impl Default for RecvOptions {
    fn default() -> Self {
        Self {
            post_close: PostCloseKind::Closed,
        }
    }
}

// ------------------------------------------------------------------
// internal helpers
// ------------------------------------------------------------------

fn fail(row: &str, what: impl core::fmt::Display) -> ! {
    panic!("conformance: {row}: {what}")
}

fn wait_until(deadline: Duration, mut f: impl FnMut() -> bool) -> bool {
    let t0 = Instant::now();
    while !f() {
        if t0.elapsed() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
    true
}

/// Run `op` on its own thread and wait at most [`WATCHDOG`] for its value.
fn bounded<R: Send + 'static>(
    row: &'static str,
    what: &'static str,
    op: impl FnOnce() -> R + Send + 'static,
) -> R {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(op());
    });
    match rx.recv_timeout(WATCHDOG) {
        Ok(v) => v,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            fail(row, format!("{what} did not return within {WATCHDOG:?}"))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => fail(row, format!("{what} panicked")),
    }
}

/// The third parameter exists only so call sites read
/// `handle_of(ROW, t.cancel_handle(), &t)` and method resolution inside the
/// generic fns stays on the ONE trait in the bound — for a type that
/// implements both `Transport` and `RecvTransport`, `t.cancel_handle()` in a
/// `T: Transport` context is then unambiguous.
fn handle_of<H: ?Sized>(
    row: &'static str,
    h: Option<Arc<dyn TransportCancel + Send + Sync>>,
    _: &H,
) -> Arc<dyn TransportCancel + Send + Sync> {
    h.unwrap_or_else(|| {
        fail(
            row,
            "cancel_handle() returned None — every transport must have one",
        )
    })
}

fn probe_for(max_payload: usize) -> Vec<u8> {
    let p = probe_packet();
    p[..p.len().min(max_payload.max(1))].to_vec()
}

fn is_kind(result: &Result<(), TransportError>, k: PostCloseKind) -> bool {
    match k {
        PostCloseKind::Closed => matches!(result, Err(TransportError::Closed)),
        PostCloseKind::ExplicitClose => matches!(result, Err(TransportError::ExplicitClose)),
    }
}

fn is_kind_recv(result: &Result<usize, TransportError>, k: PostCloseKind) -> bool {
    match k {
        PostCloseKind::Closed => matches!(result, Err(TransportError::Closed)),
        PostCloseKind::ExplicitClose => matches!(result, Err(TransportError::ExplicitClose)),
    }
}

/// Shared post-cancel budget check for the two park rows: one non-terminal
/// result observed after the cancel definitely fired is tolerated (a timeout
/// tick already in flight), a second is the defect.
fn check_post_cancel_budget(row: &str, after_cancel: u32) {
    if after_cancel > 1 {
        fail(
            row,
            format!(
                "the op returned {after_cancel} non-terminal results after cancel() \
                 (one in-flight tick is tolerated)"
            ),
        );
    }
}

// ------------------------------------------------------------------
// send-side rows
// ------------------------------------------------------------------

/// After the transport's own `close()`, a send reports `expect` and the
/// transport is no longer alive.
pub fn send_post_close_is_closed<T: Transport>(mut t: T, expect: PostCloseKind) {
    const ROW: &str = "post_close_is_closed";
    let msg = probe_for(t.max_payload());
    t.close();
    let result = t.send_bytes(&msg);
    if !is_kind(&result, expect) {
        fail(
            ROW,
            format!("expected Err({expect:?}) after close(), got {result:?}"),
        );
    }
    if t.is_alive() {
        fail(ROW, "is_alive() is true after close()");
    }
}

/// A second `close()` is a no-op, not a panic or a resurrection.
pub fn send_close_twice_is_ok<T: Transport>(mut t: T) {
    const ROW: &str = "close_twice_is_ok";
    t.close();
    t.close();
    if t.is_alive() {
        fail(ROW, "is_alive() is true after a double close()");
    }
}

/// A cancel fired while a send is parked ends that send `ExplicitClose`.
///
/// See [`recv_cancel_during_park_is_explicit_close`] for what "parked" can
/// and cannot prove from outside a real transport.
pub fn send_cancel_during_park_is_explicit_close<T: Transport + 'static>(mut t: T, park: SendPark) {
    const ROW: &str = "cancel_during_park_is_explicit_close";
    let handle = handle_of(ROW, t.cancel_handle(), &t);
    let msg = probe_for(t.max_payload());
    let (tx, rx) = mpsc::channel::<(Result<(), TransportError>, u32)>();
    match park {
        SendPark::Loop => {
            let entered = Arc::new(AtomicBool::new(false));
            let cancelled = Arc::new(AtomicBool::new(false));
            let after = Arc::new(AtomicU32::new(0));
            let (e, c, a) = (
                Arc::clone(&entered),
                Arc::clone(&cancelled),
                Arc::clone(&after),
            );
            thread::spawn(move || {
                e.store(true, Ordering::SeqCst);
                let result = loop {
                    match t.send_bytes(&msg) {
                        Ok(()) | Err(TransportError::Backpressure { .. }) => {
                            if c.load(Ordering::SeqCst) && a.fetch_add(1, Ordering::SeqCst) + 1 > 1
                            {
                                break Err(TransportError::Backpressure {
                                    msg: "kit: still non-terminal after cancel".into(),
                                    errno_code: None,
                                });
                            }
                            thread::yield_now();
                            continue;
                        }
                        other => break other,
                    }
                };
                let _ = tx.send((result, a.load(Ordering::SeqCst)));
            });
            if !wait_until(WATCHDOG, || entered.load(Ordering::SeqCst)) {
                fail(ROW, "the send worker never started");
            }
            thread::sleep(SETTLE);
            // Fire the cancel FIRST, then open the counting window: the flag
            // means "the cancel has definitely fired", so anything counted
            // after it is genuinely a post-cancel observation. Setting it
            // before `cancel()` would count sends that completed in the gap
            // between the two statements and fail a conformant transport.
            handle.cancel();
            cancelled.store(true, Ordering::SeqCst);
        }
        SendPark::NextCall => {
            // The transport cannot park on send; the row degenerates to the
            // cancel-before-op ordering (also pinned by its own row).
            handle.cancel();
            thread::spawn(move || {
                let _ = tx.send((t.send_bytes(&msg), 0));
            });
        }
    }
    let (result, after_cancel) = match rx.recv_timeout(WATCHDOG) {
        Ok(r) => r,
        Err(_) => fail(
            ROW,
            format!("the send did not return within {WATCHDOG:?} after cancel()"),
        ),
    };
    check_post_cancel_budget(ROW, after_cancel);
    if !matches!(result, Err(TransportError::ExplicitClose)) {
        fail(ROW, format!("got {result:?}"));
    }
}

/// Cancel BEFORE the first send; the first send and the next one both
/// report `ExplicitClose` (sticky reason; no retry loop).
pub fn send_cancel_before_op_is_explicit_close<T: Transport>(mut t: T) {
    const ROW: &str = "cancel_before_op_is_explicit_close";
    let handle = handle_of(ROW, t.cancel_handle(), &t);
    handle.cancel();
    let msg = probe_for(t.max_payload());
    let result = t.send_bytes(&msg);
    if !matches!(result, Err(TransportError::ExplicitClose)) {
        fail(
            ROW,
            format!("first send after a pre-op cancel got {result:?}"),
        );
    }
    let again = t.send_bytes(&msg);
    if !matches!(again, Err(TransportError::ExplicitClose)) {
        fail(
            ROW,
            format!("second send after a pre-op cancel got {again:?} (the reason must be sticky)"),
        );
    }
}

/// `is_cancelled()` is false when fresh, true after `cancel()`, and a
/// handle obtained afterwards reads the same shared latch.
pub fn send_is_cancelled_flips<T: Transport>(t: T) {
    const ROW: &str = "is_cancelled_flips";
    let h = handle_of(ROW, t.cancel_handle(), &t);
    if h.is_cancelled() {
        fail(ROW, "a fresh transport's handle reads cancelled");
    }
    h.cancel();
    if !h.is_cancelled() {
        fail(ROW, "is_cancelled() stayed false after cancel()");
    }
    if let Some(again) = t.cancel_handle() {
        if !again.is_cancelled() {
            fail(
                ROW,
                "a handle obtained after cancel() does not read the shared latch",
            );
        }
    }
}

/// Op-observed liveness: cancel, run ONE bounded send, then `!is_alive()`.
/// A transport whose liveness only latches when an op observes the cancel
/// still conforms.
pub fn send_not_alive_after_cancel<T: Transport + 'static>(mut t: T) {
    const ROW: &str = "not_alive_after_cancel";
    let h = handle_of(ROW, t.cancel_handle(), &t);
    let msg = probe_for(t.max_payload());
    h.cancel();
    let (t, result) = bounded(ROW, "a send after cancel()", move || {
        let r = t.send_bytes(&msg);
        (t, r)
    });
    if t.is_alive() {
        fail(
            ROW,
            format!("is_alive() is true after cancel() (the send after it returned {result:?})"),
        );
    }
}

// ------------------------------------------------------------------
// receive-side rows
// ------------------------------------------------------------------

/// After the transport's own `close()`, a receive reports `expect` and the
/// transport is no longer alive.
pub fn recv_post_close_is_closed<R: RecvTransport>(mut r: R, expect: PostCloseKind) {
    const ROW: &str = "post_close_is_closed";
    let mut buf = vec![0u8; r.max_payload().max(1)];
    r.close();
    let result = r.recv_bytes(&mut buf);
    if !is_kind_recv(&result, expect) {
        fail(
            ROW,
            format!("expected Err({expect:?}) after close(), got {result:?}"),
        );
    }
    if r.is_alive() {
        fail(ROW, "is_alive() is true after close()");
    }
}

/// A second `close()` is a no-op, not a panic or a resurrection.
pub fn recv_close_twice_is_ok<R: RecvTransport>(mut r: R) {
    const ROW: &str = "close_twice_is_ok";
    r.close();
    r.close();
    if r.is_alive() {
        fail(ROW, "is_alive() is true after a double close()");
    }
}

/// "Parked" cannot be observed from outside a blocking call: `entered` is
/// set immediately BEFORE the call and the kit waits `SETTLE` after it, so
/// the worker is almost certainly inside `recv_bytes` when `cancel()` fires
/// — but not provably from outside a real transport. The cancel-BEFORE-op
/// ordering is therefore its own row ([`recv_cancel_before_op_is_explicit_close`])
/// rather than an accepted outcome of this one, and the PROVABLE form —
/// the very invocation that was parked returns `ExplicitClose` — is pinned
/// where a transport can be observed from inside: the kit self-tests
/// `parked_recv_is_interrupted_in_place` / `parked_send_is_interrupted_in_place`
/// (the in-memory `Wire` counts `in_call`) and the C1.5 pipeline row
/// `managed_send_parked_in_inner_is_interrupted_in_place`. Permitted race:
/// a cancel that lands after an operation has already COMPLETED cannot
/// change that completed result; this row asserts only the terminal
/// outcome of the first call that observes the cancel. Non-terminal results
/// (`Ok`, `Backpressure`) are retried, but every one observed AFTER the
/// cancel definitely fired is counted: one is tolerated (a timeout tick
/// already in flight when the latch flipped), a second fails the row — so a
/// transport that keeps answering `Backpressure` after a cancel cannot hide
/// behind the retry loop.
pub fn recv_cancel_during_park_is_explicit_close<R: RecvTransport + 'static>(mut r: R) {
    const ROW: &str = "cancel_during_park_is_explicit_close";
    let handle = handle_of(ROW, r.cancel_handle(), &r);
    let entered = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));
    let after = Arc::new(AtomicU32::new(0));
    let (e, c, a) = (
        Arc::clone(&entered),
        Arc::clone(&cancelled),
        Arc::clone(&after),
    );
    let (tx, rx) = mpsc::channel::<(Result<usize, TransportError>, u32)>();
    thread::spawn(move || {
        let mut buf = vec![0u8; r.max_payload().max(1)];
        e.store(true, Ordering::SeqCst);
        let result = loop {
            match r.recv_bytes(&mut buf) {
                Ok(_) | Err(TransportError::Backpressure { .. }) => {
                    if c.load(Ordering::SeqCst) && a.fetch_add(1, Ordering::SeqCst) + 1 > 1 {
                        break Err(TransportError::Backpressure {
                            msg: "kit: still non-terminal after cancel".into(),
                            errno_code: None,
                        });
                    }
                    continue;
                }
                other => break other,
            }
        };
        let _ = tx.send((result, a.load(Ordering::SeqCst)));
    });
    if !wait_until(WATCHDOG, || entered.load(Ordering::SeqCst)) {
        fail(ROW, "the recv worker never started");
    }
    thread::sleep(SETTLE);
    // Cancel first, then open the counting window — see the send twin.
    handle.cancel();
    cancelled.store(true, Ordering::SeqCst);
    let (result, after_cancel) = match rx.recv_timeout(WATCHDOG) {
        Ok(r) => r,
        Err(_) => fail(
            ROW,
            format!("the recv did not return within {WATCHDOG:?} after cancel()"),
        ),
    };
    check_post_cancel_budget(ROW, after_cancel);
    if !matches!(result, Err(TransportError::ExplicitClose)) {
        fail(ROW, format!("got {result:?}"));
    }
}

/// The ordering the park row cannot prove: the cancel lands BEFORE the
/// operation is entered. The very first op must report `ExplicitClose`
/// (no retry loop — a non-terminal answer here is the defect), and so must
/// the next one (the reason is sticky).
pub fn recv_cancel_before_op_is_explicit_close<R: RecvTransport>(mut r: R) {
    const ROW: &str = "cancel_before_op_is_explicit_close";
    let handle = handle_of(ROW, r.cancel_handle(), &r);
    handle.cancel();
    let mut buf = vec![0u8; r.max_payload().max(1)];
    let result = r.recv_bytes(&mut buf);
    if !matches!(result, Err(TransportError::ExplicitClose)) {
        fail(
            ROW,
            format!("first recv after a pre-op cancel got {result:?}"),
        );
    }
    let again = r.recv_bytes(&mut buf);
    if !matches!(again, Err(TransportError::ExplicitClose)) {
        fail(
            ROW,
            format!("second recv after a pre-op cancel got {again:?} (the reason must be sticky)"),
        );
    }
}

/// `is_cancelled()` is false when fresh, true after `cancel()`, and a
/// handle obtained afterwards reads the same shared latch.
pub fn recv_is_cancelled_flips<R: RecvTransport>(r: R) {
    const ROW: &str = "is_cancelled_flips";
    let h = handle_of(ROW, r.cancel_handle(), &r);
    if h.is_cancelled() {
        fail(ROW, "a fresh transport's handle reads cancelled");
    }
    h.cancel();
    if !h.is_cancelled() {
        fail(ROW, "is_cancelled() stayed false after cancel()");
    }
    if let Some(again) = r.cancel_handle() {
        if !again.is_cancelled() {
            fail(
                ROW,
                "a handle obtained after cancel() does not read the shared latch",
            );
        }
    }
}

/// Op-observed liveness: cancel, run ONE bounded recv, then `!is_alive()`.
pub fn recv_not_alive_after_cancel<R: RecvTransport + 'static>(r: R) {
    const ROW: &str = "not_alive_after_cancel";
    let h = handle_of(ROW, r.cancel_handle(), &r);
    h.cancel();
    let (r, result) = bounded(ROW, "a recv after cancel()", move || {
        let mut r = r;
        let mut buf = vec![0u8; r.max_payload().max(1)];
        let res = r.recv_bytes(&mut buf);
        (r, res)
    });
    if r.is_alive() {
        fail(
            ROW,
            format!("is_alive() is true after cancel() (the recv after it returned {result:?})"),
        );
    }
}

/// X-CORR-07: an empty destination is a no-op — `Ok(0)` at once, nothing
/// touched — and the transport still delivers afterwards.
pub fn recv_empty_recv_is_noop<R: RecvTransport + 'static>(r: R, feed: &dyn Fn(&[u8])) {
    const ROW: &str = "empty_recv_is_noop";
    let (r, result) = bounded(ROW, "recv_bytes(&mut [])", move || {
        let mut r = r;
        let res = r.recv_bytes(&mut []);
        (r, res)
    });
    if !matches!(result, Ok(0)) {
        fail(
            ROW,
            format!("expected Ok(0) for an empty buffer, got {result:?}"),
        );
    }
    if !r.is_alive() {
        fail(ROW, "an empty recv latched the transport dead");
    }
    feed(&probe_packet());
    let (r, got) = bounded(ROW, "recv_bytes after the empty read", move || {
        let mut r = r;
        let mut buf = vec![0u8; r.max_payload().max(1)];
        let res = loop {
            match r.recv_bytes(&mut buf) {
                Err(TransportError::Backpressure { .. }) => continue,
                other => break other,
            }
        };
        (r, res)
    });
    match got {
        Ok(n) if n > 0 => {}
        other => fail(
            ROW,
            format!("after an empty read the transport did not deliver: {other:?}"),
        ),
    }
    drop(r);
}

/// A peer-side break must NOT latch `is_cancelled()`.
///
/// `is_cancelled()` means "the caller called `cancel()` on this handle",
/// never "the transport is dead". A transport that aliases liveness there
/// (tst-tcp's handle read `!alive` before WP-C1) makes every clean peer EOF
/// look like a caller close to the bindings, which relabel the end reason
/// from exactly this bit — `TST_E_CLOSED` instead of `TST_E_END_OF_STREAM`.
///
/// For a transport that implements both traits, one handle serves both
/// directions, so this row covers the send side too.
pub fn recv_peer_eof_is_not_a_cancel<R: RecvTransport + 'static>(
    mut r: R,
    induce: &dyn Fn(&mut R),
) {
    const ROW: &str = "peer_eof_is_not_a_cancel";
    let handle = handle_of(ROW, r.cancel_handle(), &r);
    if handle.is_cancelled() {
        fail(ROW, "a fresh transport's handle reads cancelled");
    }
    induce(&mut r);
    // Op-observed, like `not_alive_after_cancel`: give a transport that only
    // latches when an op sees the break its chance to do so.
    let (r, result) = bounded(ROW, "an op after the wire broke", move || {
        let mut r = r;
        let mut buf = vec![0u8; r.max_payload().max(1)];
        let res = loop {
            match r.recv_bytes(&mut buf) {
                Ok(_) | Err(TransportError::Backpressure { .. }) => continue,
                other => break other,
            }
        };
        (r, res)
    });
    if handle.is_cancelled() {
        fail(
            ROW,
            format!(
                "a peer-side break latched is_cancelled() (the op returned {result:?}) — \
                 is_cancelled() is the caller's cancel latch, never a liveness proxy"
            ),
        );
    }
    if let Some(again) = r.cancel_handle() {
        if again.is_cancelled() {
            fail(
                ROW,
                "a handle obtained after a peer-side break reads cancelled",
            );
        }
    }
}

// ------------------------------------------------------------------
// standalone rows
// ------------------------------------------------------------------

/// See [`BrokenSource`]. The aggregate runs this for `Induce`; callable
/// on its own with any `&dyn Fn(&mut T)`.
pub fn not_alive_after_broken_send<T: Transport + 'static>(mut t: T, induce: &dyn Fn(&mut T)) {
    const ROW: &str = "not_alive_after_broken";
    let msg = probe_for(t.max_payload());
    induce(&mut t);
    if !t.is_alive() {
        // The inducer drove the transport into Broken itself (UDP EMSGSIZE
        // shape): the latch is the verdict, nothing more to observe.
        return;
    }
    let (t, result) = bounded(ROW, "send_bytes after the wire broke", move || {
        let mut t = t;
        let res = loop {
            match t.send_bytes(&msg) {
                Ok(()) | Err(TransportError::Backpressure { .. }) => {
                    thread::yield_now();
                    continue;
                }
                other => break other,
            }
        };
        (t, res)
    });
    if !matches!(result, Err(TransportError::Broken { .. })) {
        fail(
            ROW,
            format!("expected Err(Broken) after the wire broke, got {result:?}"),
        );
    }
    if t.is_alive() {
        fail(ROW, "is_alive() is true after Broken");
    }
}

/// Receive twin of [`not_alive_after_broken_send`].
pub fn not_alive_after_broken_recv<R: RecvTransport + 'static>(mut r: R, induce: &dyn Fn(&mut R)) {
    const ROW: &str = "not_alive_after_broken";
    induce(&mut r);
    if !r.is_alive() {
        return;
    }
    let (r, result) = bounded(ROW, "recv_bytes after the wire broke", move || {
        let mut r = r;
        let mut buf = vec![0u8; r.max_payload().max(1)];
        let res = loop {
            match r.recv_bytes(&mut buf) {
                Ok(_) | Err(TransportError::Backpressure { .. }) => continue,
                other => break other,
            }
        };
        (r, res)
    });
    if !matches!(result, Err(TransportError::Broken { .. })) {
        fail(
            ROW,
            format!("expected Err(Broken) after the wire broke, got {result:?}"),
        );
    }
    if r.is_alive() {
        fail(ROW, "is_alive() is true after Broken");
    }
}

/// `RecvTransport::max_payload` must be the protocol's deliverable
/// ceiling (what a conformant foreign sender can legally produce), never
/// the local send budget — the PR #97 truncation class.
pub fn recv_max_payload_ge_ceiling<R: RecvTransport>(r: &R, ceiling: usize) {
    const ROW: &str = "recv_max_payload_ge_ceiling";
    let got = r.max_payload();
    if got < ceiling {
        fail(
            ROW,
            format!("max_payload() = {got} < protocol ceiling {ceiling}"),
        );
    }
}

// ------------------------------------------------------------------
// aggregates
// ------------------------------------------------------------------

/// Visible skip for a row a `BrokenSource::NotProducible` transport cannot
/// run. The parenthetical is per-row: the two rows skip for related but
/// different reasons, and a reader scanning `--nocapture` output should not
/// have to guess which.
fn skip_row(row: &str, why: &str) {
    eprintln!("conformance: {row}: skipped ({why})");
}

/// Run the given send rows, taking a fresh transport from `factory` for each.
pub fn assert_send_rows<T: Transport + 'static>(
    factory: impl Fn() -> T,
    broken: BrokenSource<T>,
    opts: SendOptions,
    rows: &[SendRow],
) {
    for row in rows {
        match row {
            SendRow::PostCloseIsClosed => send_post_close_is_closed(factory(), opts.post_close),
            SendRow::CloseTwiceIsOk => send_close_twice_is_ok(factory()),
            SendRow::CancelDuringParkIsExplicitClose => {
                send_cancel_during_park_is_explicit_close(factory(), opts.park)
            }
            SendRow::CancelBeforeOpIsExplicitClose => {
                send_cancel_before_op_is_explicit_close(factory())
            }
            SendRow::IsCancelledFlips => send_is_cancelled_flips(factory()),
            SendRow::NotAliveAfterCancel => send_not_alive_after_cancel(factory()),
            SendRow::NotAliveAfterBroken => match &broken {
                BrokenSource::Induce(induce) => {
                    not_alive_after_broken_send(factory(), induce.as_ref())
                }
                BrokenSource::NotProducible => skip_row(
                    SendRow::NotAliveAfterBroken.name(),
                    "Broken not producible on this transport",
                ),
            },
        }
    }
}

/// Every send row with the bare-transport defaults (`Closed` after own
/// close, `SendPark::Loop`).
pub fn assert_send_contract<T: Transport + 'static>(
    factory: impl Fn() -> T,
    broken: BrokenSource<T>,
) {
    assert_send_rows(factory, broken, SendOptions::default(), SendRow::ALL);
}

/// Run the given receive rows, taking a fresh transport from `factory` for each.
pub fn assert_recv_rows<R: RecvTransport + 'static>(
    factory: impl Fn() -> R,
    feed: impl Fn(&[u8]),
    broken: BrokenSource<R>,
    opts: RecvOptions,
    rows: &[RecvRow],
) {
    for row in rows {
        match row {
            RecvRow::PostCloseIsClosed => recv_post_close_is_closed(factory(), opts.post_close),
            RecvRow::CloseTwiceIsOk => recv_close_twice_is_ok(factory()),
            RecvRow::CancelDuringParkIsExplicitClose => {
                recv_cancel_during_park_is_explicit_close(factory())
            }
            RecvRow::CancelBeforeOpIsExplicitClose => {
                recv_cancel_before_op_is_explicit_close(factory())
            }
            RecvRow::IsCancelledFlips => recv_is_cancelled_flips(factory()),
            RecvRow::NotAliveAfterCancel => recv_not_alive_after_cancel(factory()),
            RecvRow::EmptyRecvIsNoop => recv_empty_recv_is_noop(factory(), &feed),
            RecvRow::NotAliveAfterBroken => match &broken {
                BrokenSource::Induce(induce) => {
                    not_alive_after_broken_recv(factory(), induce.as_ref())
                }
                BrokenSource::NotProducible => skip_row(
                    RecvRow::NotAliveAfterBroken.name(),
                    "Broken not producible on this transport",
                ),
            },
            RecvRow::PeerEofIsNotACancel => match &broken {
                BrokenSource::Induce(induce) => {
                    recv_peer_eof_is_not_a_cancel(factory(), induce.as_ref())
                }
                BrokenSource::NotProducible => skip_row(
                    RecvRow::PeerEofIsNotACancel.name(),
                    "no peer-side break is producible on this transport",
                ),
            },
        }
    }
}

/// Every receive row with the bare-transport defaults (`Closed` after own
/// close).
pub fn assert_recv_contract<R: RecvTransport + 'static>(
    factory: impl Fn() -> R,
    feed: impl Fn(&[u8]),
    broken: BrokenSource<R>,
) {
    assert_recv_rows(factory, feed, broken, RecvOptions::default(), RecvRow::ALL);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::thread;

    /// Queue cap for the in-memory wire. The `SendPark::Loop` row drives a
    /// non-parking sender as fast as it can for `SETTLE`; a ring keeps the
    /// fixture's memory bounded without changing the "send always succeeds"
    /// shape the row is there to exercise.
    const WIRE_CAP: usize = 256;

    /// In-memory pair state: a byte queue plus the cancel/break flags a
    /// conformant transport consults. The `legacy` variant returns the
    /// pre-Arc-2 `Broken("cancelled")` so the kit's cancel row must reject
    /// it with the exact message the crates' RED runs show.
    struct Wire {
        queue: Mutex<VecDeque<Vec<u8>>>,
        cv: Condvar,
        cancelled: AtomicBool,
        broken: AtomicBool,
        legacy: bool,
        /// Number of `recv_bytes`/`send_bytes` calls currently INSIDE the
        /// transport (RAII-guarded) — the observation the generic park row
        /// cannot make; `parked_*_is_interrupted_in_place` waits on it.
        in_call: AtomicUsize,
        /// When set, `MemSend::send_bytes` parks instead of enqueuing.
        park_sends: AtomicBool,
    }

    /// Decrements `in_call` on every exit path, including an unwind.
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
        fn new(legacy: bool) -> Arc<Self> {
            Arc::new(Self {
                queue: Mutex::new(VecDeque::new()),
                cv: Condvar::new(),
                cancelled: AtomicBool::new(false),
                broken: AtomicBool::new(false),
                legacy,
                in_call: AtomicUsize::new(0),
                park_sends: AtomicBool::new(false),
            })
        }
        fn push(&self, b: &[u8]) {
            let mut q = self.queue.lock().unwrap();
            if q.len() >= WIRE_CAP {
                q.pop_front();
            }
            q.push_back(b.to_vec());
            self.cv.notify_all();
        }
        fn cancel_error(&self) -> TransportError {
            if self.legacy {
                TransportError::Broken {
                    msg: "cancelled".into(),
                    errno_code: None,
                    cause: super::super::BrokenCause::Unspecified,
                }
            } else {
                TransportError::ExplicitClose
            }
        }
        fn broke(&self) -> TransportError {
            TransportError::Broken {
                msg: "wire broke".into(),
                errno_code: None,
                cause: super::super::BrokenCause::Unspecified,
            }
        }
        fn break_now(&self) {
            self.broken.store(true, Ordering::SeqCst);
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

    struct MemRecv {
        wire: Arc<Wire>,
        alive: bool,
    }
    impl RecvTransport for MemRecv {
        fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            if buf.is_empty() {
                return Ok(0);
            }
            // Cancel is checked BEFORE liveness so the reason stays sticky
            // across calls (`cancel_before_op_is_explicit_close`): a
            // transport that reported ExplicitClose once must not downgrade
            // to Closed on the next call.
            if self.wire.cancelled.load(Ordering::SeqCst) {
                self.alive = false;
                return Err(self.wire.cancel_error());
            }
            if !self.alive {
                return Err(TransportError::Closed);
            }
            let _in = InCall::enter(&self.wire.in_call);
            let mut q = self.wire.queue.lock().unwrap();
            loop {
                if self.wire.cancelled.load(Ordering::SeqCst) {
                    self.alive = false;
                    return Err(self.wire.cancel_error());
                }
                if self.wire.broken.load(Ordering::SeqCst) {
                    self.alive = false;
                    return Err(self.wire.broke());
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

    struct MemSend {
        wire: Arc<Wire>,
        alive: bool,
    }
    impl Transport for MemSend {
        fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
            // Same cancel-before-liveness ordering as `MemRecv`.
            if self.wire.cancelled.load(Ordering::SeqCst) {
                self.alive = false;
                return Err(self.wire.cancel_error());
            }
            if !self.alive {
                return Err(TransportError::Closed);
            }
            let _in = InCall::enter(&self.wire.in_call);
            // `park_sends` = the libsrt full-send-buffer shape: block until
            // cancelled/broken (the provable-park self-test flips it).
            let mut q = self.wire.queue.lock().unwrap();
            loop {
                if self.wire.cancelled.load(Ordering::SeqCst) {
                    self.alive = false;
                    return Err(self.wire.cancel_error());
                }
                if self.wire.broken.load(Ordering::SeqCst) {
                    self.alive = false;
                    return Err(self.wire.broke());
                }
                if !self.wire.park_sends.load(Ordering::SeqCst) {
                    if q.len() >= WIRE_CAP {
                        q.pop_front();
                    }
                    q.push_back(msg.to_vec());
                    self.wire.cv.notify_all();
                    return Ok(());
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

    /// The PROVABLE form of the park row (handoff validation 2026-09-18):
    /// wait until the worker is INSIDE `recv_bytes` (`in_call == 1`), fire the
    /// cancel, and assert the result of THAT invocation — no retry loop, no
    /// `SETTLE`. Mutation pin: comment out `InCall::enter` in `MemRecv` →
    /// the wait times out with "never entered".
    #[test]
    fn parked_recv_is_interrupted_in_place() {
        let wire = Wire::new(false);
        let mut r = MemRecv {
            wire: Arc::clone(&wire),
            alive: true,
        };
        let handle = r.cancel_handle().expect("MemRecv has a cancel handle");
        let worker = thread::spawn(move || {
            let mut buf = [0u8; 188];
            r.recv_bytes(&mut buf)
        });
        assert!(
            wait_until(WATCHDOG, || wire.in_call.load(Ordering::SeqCst) == 1),
            "the worker never entered recv_bytes"
        );
        handle.cancel();
        let result = worker.join().expect("worker did not panic");
        assert!(
            matches!(result, Err(TransportError::ExplicitClose)),
            "got {result:?}"
        );
        assert_eq!(
            wire.in_call.load(Ordering::SeqCst),
            0,
            "InCall released on exit"
        );
    }

    /// Send twin: `park_sends` makes `MemSend::send_bytes` block (the libsrt
    /// full-buffer shape), so the cancel provably lands inside the call.
    #[test]
    fn parked_send_is_interrupted_in_place() {
        let wire = Wire::new(false);
        wire.park_sends.store(true, Ordering::SeqCst);
        let mut t = MemSend {
            wire: Arc::clone(&wire),
            alive: true,
        };
        let handle = t.cancel_handle().expect("MemSend has a cancel handle");
        let worker = thread::spawn(move || t.send_bytes(&[0x47; 188]));
        assert!(
            wait_until(WATCHDOG, || wire.in_call.load(Ordering::SeqCst) == 1),
            "the worker never entered send_bytes"
        );
        handle.cancel();
        let result = worker.join().expect("worker did not panic");
        assert!(
            matches!(result, Err(TransportError::ExplicitClose)),
            "got {result:?}"
        );
        assert_eq!(
            wire.in_call.load(Ordering::SeqCst),
            0,
            "InCall released on exit"
        );
    }

    fn panic_text(r: std::thread::Result<()>) -> String {
        match r {
            Ok(()) => String::new(),
            Err(p) => p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "<non-string panic>".into()),
        }
    }

    /// The kit must REJECT the pre-Arc-2 outcome with the row's exact
    /// message prefix — this is what the SRT/TCP RED runs paste into the
    /// ledger.
    #[test]
    fn legacy_broken_after_cancel_fails_the_cancel_row_with_the_row_name() {
        let wire = Wire::new(true);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            recv_cancel_during_park_is_explicit_close(MemRecv { wire, alive: true })
        }));
        let text = panic_text(r);
        assert!(
            text.starts_with("conformance: cancel_during_park_is_explicit_close: got Err(Broken"),
            "unexpected verdict text: {text:?}"
        );
    }

    /// Each row must get FRESH underlying state: the cancel row latches
    /// `Wire::cancelled` for good, so a factory that reused one `Wire`
    /// would hand the next row (`is_cancelled_flips`) an already-cancelled
    /// transport and the aggregate could never pass. The factory therefore
    /// creates a new `Wire` per call and publishes it as the CURRENT wire;
    /// the feed and the inducer always act on that current wire.
    type WireSlot = Arc<Mutex<Option<Arc<Wire>>>>;

    fn fresh_wire_factory() -> (WireSlot, impl Fn() -> Arc<Wire>) {
        let current: WireSlot = Arc::new(Mutex::new(None));
        let c = Arc::clone(&current);
        (current, move || {
            let w = Wire::new(false);
            *c.lock().unwrap() = Some(Arc::clone(&w));
            w
        })
    }
    fn current(slot: &WireSlot) -> Arc<Wire> {
        Arc::clone(
            slot.lock()
                .unwrap()
                .as_ref()
                .expect("factory not called yet"),
        )
    }

    #[test]
    fn conformant_in_memory_pair_passes_every_row() {
        let (cur, mk) = fresh_wire_factory();
        let cf = Arc::clone(&cur);
        let cb = Arc::clone(&cur);
        assert_recv_contract(
            move || MemRecv {
                wire: mk(),
                alive: true,
            },
            move |b| current(&cf).push(b),
            BrokenSource::Induce(Box::new(move |_r: &mut MemRecv| current(&cb).break_now())),
        );

        let (cur, mk) = fresh_wire_factory();
        let cb = Arc::clone(&cur);
        assert_send_contract(
            move || MemSend {
                wire: mk(),
                alive: true,
            },
            BrokenSource::Induce(Box::new(move |_t: &mut MemSend| current(&cb).break_now())),
        );

        // The NextCall park shape (a transport that cannot park on send).
        let wire = Wire::new(false);
        send_cancel_during_park_is_explicit_close(
            MemSend { wire, alive: true },
            SendPark::NextCall,
        );

        // NotProducible must be a visible skip, not a failure (and the other rows still run).
        let (_cur, mk) = fresh_wire_factory();
        assert_send_rows(
            move || MemSend {
                wire: mk(),
                alive: true,
            },
            BrokenSource::NotProducible,
            SendOptions::default(),
            &[SendRow::NotAliveAfterBroken, SendRow::CloseTwiceIsOk],
        );

        // An inducer that drives the transport into Broken ITSELF (the UDP
        // EMSGSIZE shape) is accepted: the row sees `!is_alive()` and skips the op.
        let wire = Wire::new(false);
        not_alive_after_broken_send(MemSend { wire, alive: true }, &|t: &mut MemSend| {
            t.wire.break_now();
            let _ = t.send_bytes(&[0u8; 8]);
            assert!(!t.is_alive());
        });

        let wire = Wire::new(false);
        recv_max_payload_ge_ceiling(&MemRecv { wire, alive: true }, 1316);
    }

    /// Mutation pin for the fixture rule above: a factory that reuses ONE
    /// wire across rows must fail the aggregate on `is_cancelled_flips`
    /// (the row after the cancel row) — proves the rows do not leak state.
    #[test]
    fn shared_wire_factory_is_rejected_by_is_cancelled_flips() {
        let wire = Wire::new(false);
        let w = Arc::clone(&wire);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_send_rows(
                move || MemSend {
                    wire: Arc::clone(&w),
                    alive: true,
                },
                BrokenSource::NotProducible,
                SendOptions::default(),
                &[
                    SendRow::CancelDuringParkIsExplicitClose,
                    SendRow::IsCancelledFlips,
                ],
            )
        }));
        let text = panic_text(r);
        assert!(
            text.starts_with(
                "conformance: is_cancelled_flips: a fresh transport's handle reads cancelled"
            ),
            "{text:?}"
        );
    }

    /// A peer-side break must not read as a caller cancel. Mutation pin:
    /// make `WireCancel::is_cancelled` return `cancelled || broken` (the
    /// tst-tcp `!alive` shape) and this row fails.
    #[test]
    fn peer_break_that_latched_is_cancelled_is_rejected() {
        /// A handle that aliases liveness — exactly what WP-C1 forbids.
        struct AliasingCancel(Arc<Wire>);
        impl TransportCancel for AliasingCancel {
            fn cancel(&self) {
                self.0.cancelled.store(true, Ordering::SeqCst);
                self.0.cv.notify_all();
            }
            fn is_cancelled(&self) -> bool {
                self.0.cancelled.load(Ordering::SeqCst) || self.0.broken.load(Ordering::SeqCst)
            }
        }
        struct AliasingRecv(MemRecv);
        impl RecvTransport for AliasingRecv {
            fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
                self.0.recv_bytes(buf)
            }
            fn max_payload(&self) -> usize {
                self.0.max_payload()
            }
            fn is_alive(&self) -> bool {
                self.0.is_alive()
            }
            fn close(&mut self) {
                self.0.close()
            }
            fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
                Some(Arc::new(AliasingCancel(Arc::clone(&self.0.wire))))
            }
        }

        let wire = Wire::new(false);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            recv_peer_eof_is_not_a_cancel(
                AliasingRecv(MemRecv { wire, alive: true }),
                &|r: &mut AliasingRecv| r.0.wire.break_now(),
            )
        }));
        let text = panic_text(r);
        assert!(
            text.starts_with(
                "conformance: peer_eof_is_not_a_cancel: a peer-side break latched is_cancelled()"
            ),
            "unexpected verdict text: {text:?}"
        );
    }

    #[test]
    fn managed_style_post_close_explicit_close_is_selectable() {
        struct ExplicitAfterClose(MemRecv);
        impl RecvTransport for ExplicitAfterClose {
            fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
                if !self.0.alive && !buf.is_empty() {
                    return Err(TransportError::ExplicitClose);
                }
                self.0.recv_bytes(buf)
            }
            fn max_payload(&self) -> usize {
                self.0.max_payload()
            }
            fn is_alive(&self) -> bool {
                self.0.is_alive()
            }
            fn close(&mut self) {
                self.0.close()
            }
            fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
                self.0.cancel_handle()
            }
        }
        let wire = Wire::new(false);
        let w = Arc::clone(&wire);
        recv_post_close_is_closed(
            ExplicitAfterClose(MemRecv {
                wire: w,
                alive: true,
            }),
            PostCloseKind::ExplicitClose,
        );
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            recv_post_close_is_closed(
                ExplicitAfterClose(MemRecv { wire, alive: true }),
                PostCloseKind::Closed,
            )
        }));
        assert!(
            panic_text(r).starts_with("conformance: post_close_is_closed: expected Err(Closed)")
        );
    }

    /// `ALL` must list every variant exactly once, and the aggregate must
    /// dispatch every listed row exactly once.
    ///
    /// Three-way enforcement for a newly added variant: `name()`'s match and
    /// the aggregate's match stop compiling, and the explicit count below
    /// fails until `ALL` gains the row. The factory-call count is the
    /// per-pattern hit count — every arm takes exactly one transport.
    #[test]
    fn send_all_lists_each_row_once_and_the_aggregate_dispatches_each_once() {
        assert_eq!(SendRow::ALL.len(), 7, "SendRow::ALL is missing a variant");
        let mut names: Vec<&str> = SendRow::ALL.iter().map(|r| r.name()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "SendRow::ALL contains a duplicate row");
        assert_eq!(SendRow::all_except(&[]), SendRow::ALL.to_vec());

        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let (cur, mk) = fresh_wire_factory();
        let cb = Arc::clone(&cur);
        assert_send_rows(
            move || {
                c.fetch_add(1, Ordering::SeqCst);
                MemSend {
                    wire: mk(),
                    alive: true,
                }
            },
            BrokenSource::Induce(Box::new(move |_t: &mut MemSend| current(&cb).break_now())),
            SendOptions::default(),
            SendRow::ALL,
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            SendRow::ALL.len(),
            "each send row must consume exactly one transport"
        );
    }

    /// Receive twin of the coverage/dispatch pin above.
    #[test]
    fn recv_all_lists_each_row_once_and_the_aggregate_dispatches_each_once() {
        assert_eq!(RecvRow::ALL.len(), 9, "RecvRow::ALL is missing a variant");
        let mut names: Vec<&str> = RecvRow::ALL.iter().map(|r| r.name()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "RecvRow::ALL contains a duplicate row");
        assert_eq!(RecvRow::all_except(&[]), RecvRow::ALL.to_vec());

        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let (cur, mk) = fresh_wire_factory();
        let cf = Arc::clone(&cur);
        let cb = Arc::clone(&cur);
        assert_recv_rows(
            move || {
                c.fetch_add(1, Ordering::SeqCst);
                MemRecv {
                    wire: mk(),
                    alive: true,
                }
            },
            move |b| current(&cf).push(b),
            BrokenSource::Induce(Box::new(move |_r: &mut MemRecv| current(&cb).break_now())),
            RecvOptions::default(),
            RecvRow::ALL,
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            RecvRow::ALL.len(),
            "each recv row must consume exactly one transport"
        );
    }

    /// `all_except` really removes rows (and nothing else).
    #[test]
    fn all_except_drops_only_the_named_rows() {
        let kept = SendRow::all_except(&[SendRow::CancelDuringParkIsExplicitClose]);
        assert_eq!(kept.len(), SendRow::ALL.len() - 1);
        assert!(!kept.contains(&SendRow::CancelDuringParkIsExplicitClose));
        let kept = RecvRow::all_except(&[RecvRow::EmptyRecvIsNoop, RecvRow::NotAliveAfterBroken]);
        assert_eq!(kept.len(), RecvRow::ALL.len() - 2);
        assert!(!kept.contains(&RecvRow::EmptyRecvIsNoop));
        assert!(!kept.contains(&RecvRow::NotAliveAfterBroken));
    }

    /// The probe is a well-formed TS null packet — the one thing the feed
    /// contract relies on across every transport.
    #[test]
    fn probe_packet_is_a_ts_null_packet() {
        let p = probe_packet();
        assert_eq!(p.len(), 188);
        assert_eq!(p[0], 0x47, "sync byte");
        assert_eq!(
            (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]),
            0x1FFF,
            "PID"
        );
        assert_eq!(p[3] & 0x30, 0x10, "payload only, no adaptation field");
    }
}
