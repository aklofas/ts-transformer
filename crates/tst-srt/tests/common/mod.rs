//! Shared scaffolding for tst-srt integration tests.
//! Loopback only; no external network.

#![allow(dead_code)]

use std::ffi::c_int;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};
use tst_core::cancel::CancelSlot;
use tst_srt::error::RejectReason;
use tst_srt::{SrtConnectError, SrtSocket};
use tst_test_helpers::timing::scaled;

pub mod raw_handshake;

/// Probe whether 127.0.0.1 is bindable. Tests gate on this so
/// sandbox/restricted CI environments don't fail dozens of tests
/// they can't possibly pass. Set env `SKIP_LOOPBACK=1` to force-skip.
///
/// Uses TCP-bind: on Linux loopback is governed by the same per-interface
/// policy for TCP and UDP, so TCP-bindability is a faithful proxy for
/// "loopback works." SRT itself is UDP; the probe is layer-agnostic.
///
/// Returns `Ok(())` if loopback is usable; `Err(reason)` otherwise.
pub fn loopback_probe() -> Result<(), &'static str> {
    if std::env::var_os("SKIP_LOOPBACK").is_some() {
        return Err("SKIP_LOOPBACK env set");
    }
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    match TcpListener::bind(addr) {
        Ok(_) => Ok(()),
        Err(_) => Err("127.0.0.1 not bindable"),
    }
}

/// Macro: emit "SKIP" line and return early if loopback is unusable.
/// Use as the first line of every loopback test body. Requires
/// `mod common;` at the top of the test file.
#[macro_export]
macro_rules! require_loopback {
    () => {
        if let Err(why) = $crate::common::loopback_probe() {
            eprintln!("SKIP: loopback unavailable ({})", why);
            return;
        }
    };
}

/// Poll-with-deadline replacement for `thread::sleep(50ms)` listener-settle.
/// Matches the `accept_done` atomic-signal precedent from
/// `cancellation_loopback.rs`: the listener thread stores `true` into a
/// shared `AtomicBool` after `Listener::bind` returns (and before the
/// blocking `accept()` call); the main thread polls until set, then
/// connects.
///
/// Panics if the signal isn't set within 2 seconds — surfaces real
/// listener-thread failures loudly instead of silent flakes.
///
/// Usage shape at each caller:
/// ```ignore
/// let ready = Arc::new(AtomicBool::new(false));
/// let r = ready.clone();
/// thread::spawn(move || {
///     let listener = Listener::bind(addr).unwrap();
///     r.store(true, Ordering::SeqCst);
///     let socket = listener.accept().unwrap();
///     // ...
/// });
/// crate::common::wait_for_ready(&ready);
/// // ... main thread connect ...
/// ```
pub fn wait_for_ready(ready: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ready.load(Ordering::SeqCst) {
        if Instant::now() > deadline {
            panic!(
                "wait_for_ready: signal not set within 2s — listener \
                 thread may have panicked before signaling, or never \
                 called ready.store(true, ...)"
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ------------------------------------------------------------------------
// SrtLoopback helper: bind + spawn accept + ready-signal in one type.
// ------------------------------------------------------------------------

/// Loopback test fixture. Encapsulates the 15-line "bind listener, spawn
/// accept thread, signal ready via AtomicBool, hand socket to closure"
/// boilerplate so each test body shrinks to ~3 lines of setup.
///
/// Usage:
///
/// ```ignore
/// mod common;
/// use std::time::Duration;
/// use tst_srt::SrtSocketBuilder;
///
/// #[test]
/// fn round_trip() {
///     require_loopback!();
///     let lb = crate::common::Loopback::bind();
///     let port = lb.port;
///
///     let accept = lb.spawn_accept(|mut sock| {
///         let mut buf = [0u8; 1500];
///         let n = sock.recv(&mut buf).expect("recv");
///         buf[..n].to_vec()
///     });
///     accept.wait_ready();
///
///     let mut socket = SrtSocketBuilder::new()
///         .recv_timeout(Duration::from_secs(5))
///         .connect(format!("127.0.0.1:{port}"))
///         .expect("connect");
///     socket.send(b"hello").expect("send");
///
///     let received = accept.join();
///     assert_eq!(received, b"hello");
/// }
/// ```
///
/// Design notes:
/// - `Loopback` is consumed by `spawn_accept` because the underlying
///   `Listener` is moved into the accept thread. Cache `port` off the
///   listener BEFORE spawn for use in `SrtSocketBuilder::connect`.
/// - The closure `f: FnOnce(Socket) -> R` receives the ACCEPTED socket
///   (the per-connection socket, not the listener). `R` is whatever the
///   test wants returned from the accept thread (typically a `Vec<u8>`
///   of received bytes, a count, or `()` for accept-then-drop).
/// - `wait_ready()` blocks until the listener thread has signaled (just
///   before entering `accept()`); the panic-with-deadline shape matches
///   `wait_for_ready` so listener-thread crashes surface loudly.
/// - `AcceptHandle::join` panics if the accept thread panicked — same
///   semantics as `JoinHandle::join().expect(...)`.
pub struct Loopback {
    pub listener: tst_srt::SrtListener,
    pub port: u16,
}

impl Loopback {
    /// Bind a listener to `127.0.0.1:0` with 5-second recv/send timeouts.
    /// Panics if bind fails — the test should have called `require_loopback!()`
    /// first.
    pub fn bind() -> Self {
        let listener = tst_srt::SrtListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .send_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind 127.0.0.1:0");
        let port = listener.local_addr().expect("local_addr").port();
        Self { listener, port }
    }

    /// Bind with a caller-supplied `ListenerBuilder` — for tests that need
    /// non-default options (encryption, custom timeouts, stream-id ACL).
    /// The builder MUST not already have called `.bind(...)`; this method
    /// supplies the bind address.
    pub fn bind_with(builder: tst_srt::SrtListenerBuilder) -> Self {
        Self::bind_at(builder, "127.0.0.1:0")
    }

    /// [`bind_with`](Self::bind_with) at a caller-supplied address — for the
    /// IPv6 loopback test (`[::1]:0`). Read `listener.local_addr()` off the
    /// returned fixture to assert the family it actually bound.
    pub fn bind_at(builder: tst_srt::SrtListenerBuilder, addr: &str) -> Self {
        let listener = builder
            .bind(addr)
            .unwrap_or_else(|e| panic!("bind {addr}: {e}"));
        let port = listener.local_addr().expect("local_addr").port();
        Self { listener, port }
    }

    /// Spawn a thread that signals ready, accepts one connection, hands
    /// the resulting `Socket` to `f`, and returns `f`'s value via the
    /// `AcceptHandle`.
    ///
    /// The listener's cancel handle is obtained BEFORE the listener moves
    /// into the thread: it is the only way left to wake that thread if its
    /// `accept()` never completes (see [`AcceptHandle`]).
    pub fn spawn_accept<F, R>(self, f: F) -> AcceptHandle<R>
    where
        F: FnOnce(tst_srt::SrtSocket) -> R + Send + 'static,
        R: Send + 'static,
    {
        let ready = Arc::new(AtomicBool::new(false));
        let accepted = Arc::new(AtomicBool::new(false));
        let (r, a) = (ready.clone(), accepted.clone());
        let cancel = self.listener.cancel_handle();
        let mut listener = self.listener;
        let handle = std::thread::spawn(move || {
            r.store(true, Ordering::SeqCst);
            let result = listener.accept();
            // Set for Ok AND Err: `join` only bounds the accept phase.
            a.store(true, Ordering::SeqCst);
            result.map(|(sock, _peer)| f(sock))
        });
        AcceptHandle {
            handle: Some(handle),
            ready,
            accepted,
            cancel,
        }
    }
}

/// How long [`AcceptHandle::join`] lets an `accept()` that has not returned
/// yet keep waiting before it concludes the connection is never coming.
/// Every caller joins AFTER its own `connect` returned, so a healthy accept
/// is already queued and completes in milliseconds; 10 s absorbs a loaded
/// 2-vCPU runner and still sits under nextest's 20 s network-group kill.
pub const ACCEPT_DEADLINE: Duration = Duration::from_secs(10);

/// Handle to the accept thread spawned by [`Loopback::spawn_accept`].
///
/// The thread's blocking `accept()` can be left with nothing to dequeue —
/// forever. libsrt's GC pass (`CUDTUnited::checkBrokenSockets`) prunes a
/// connection that breaks while still queued on the listener's accept
/// queue; a test that closes its caller socket right after `connect`
/// returns can lose that race when the accept thread is descheduled across
/// the window (a loaded 2-vCPU CI runner). Nothing else ever connects and
/// the listener is owned by the parked thread, so without help the test
/// hangs to nextest's kill. Two guards, both driven by the listener's pre-obtained cancel
/// handle (`cancel()` closes the listening socket, which wakes a parked
/// `accept()` with an error):
///
/// - [`join`](Self::join) bounds the ACCEPT phase by [`ACCEPT_DEADLINE`]:
///   on expiry it fires the cancel, reaps the woken thread, and panics
///   naming this class. It never fires the cancel while the accept can
///   still succeed, and the closure phase after a successful accept is
///   not bounded at all — long recv loops stay legal.
/// - `Drop` fires the cancel if the accept is still pending when the handle
///   is dropped without `join` — i.e. a test unwinding on an `expect`
///   before its join — then reaps the thread if it finishes inside the
///   accept budget and detaches it otherwise (a long closure phase is the
///   test's own business, as it was before this guard existed). A leaked
///   thread parked in `srt_accept` does worse than leak: it holds the port
///   AND stalls process exit (`srt_cleanup` runs at exit). The drop path
///   swallows a peer panic (a panic is already in flight); the happy
///   path's explicit `join` is what reports one.
pub struct AcceptHandle<R> {
    handle: Option<std::thread::JoinHandle<Result<R, tst_srt::AcceptError>>>,
    ready: Arc<AtomicBool>,
    /// Set by the thread the moment `accept()` returns, Ok or Err.
    accepted: Arc<AtomicBool>,
    cancel: tst_srt::ResourceCancel,
}

impl<R: Send + 'static> AcceptHandle<R> {
    /// Block until the listener thread has signaled ready (just before
    /// `accept()`). Same semantics as [`wait_for_ready`] — panics after
    /// 2 seconds if the signal hasn't appeared.
    pub fn wait_ready(&self) {
        wait_for_ready(&self.ready);
    }

    /// Consume the handle and return the closure's value. Panics if the
    /// accept failed, if the listener thread panicked, or if the accept
    /// is still parked after [`ACCEPT_DEADLINE`] (see the type docs).
    pub fn join(self) -> R {
        self.join_within(ACCEPT_DEADLINE)
    }

    /// [`join`](Self::join) with a caller-chosen accept-phase deadline.
    /// Only the test that pins the deadline behaviour needs a short one;
    /// everything else uses `join`.
    pub fn join_within(mut self, accept_deadline: Duration) -> R {
        let handle = self.handle.take().expect("join handle taken once");
        let deadline = Instant::now() + accept_deadline;
        while !self.accepted.load(Ordering::SeqCst) {
            if Instant::now() > deadline {
                // Wake the parked accept (idempotent if it just returned).
                self.cancel.cancel();
                match handle.join() {
                    // The accept can still have won the race against the
                    // cancel and delivered a socket; only a lost one fails.
                    Ok(Ok(value)) => return value,
                    Ok(Err(e)) => panic!(
                        "AcceptHandle::join: accept() still parked after \
                         {accept_deadline:?} with no connection to dequeue — \
                         the libsrt accept-queue prune class (GC erased a \
                         broken queued connection). The caller's \
                         connect either never happened or its socket closed \
                         before the peer thread got scheduled. Woken by the \
                         cancel with: {e}"
                    ),
                    Err(_) => panic!(
                        "listener thread panicked while its accept() was \
                         still pending after {accept_deadline:?}"
                    ),
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        handle
            .join()
            .expect("listener thread panicked")
            .unwrap_or_else(|e| panic!("accept failed: {e}"))
    }
}

impl<R> Drop for AcceptHandle<R> {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        // Only a still-pending accept needs the wake; once the accept has
        // completed the closure phase owns the thread, exactly as before
        // this guard existed, and closing the listener would not end it.
        if !self.accepted.load(Ordering::SeqCst) {
            self.cancel.cancel();
        }
        // Reap if the thread finishes inside the accept budget (a woken
        // accept returns within one libsrt GC tick); otherwise detach —
        // a long closure phase must not turn a test failure into a hang.
        let deadline = Instant::now() + ACCEPT_DEADLINE;
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
    }
}

// ------------------------------------------------------------------------
// Listener-mode URL fixtures.
// ------------------------------------------------------------------------

/// Reserve an ephemeral UDP port and release it again. This exists only
/// because a listener-mode URL (and `accept_one_cancellable`, which owns its
/// listener and never exposes a `local_addr`) must name its port before
/// `accept_one` binds, so a connecting peer can know it in advance. Every
/// other test binds port 0 and reads the address back; the bind-then-release
/// race is accepted only here.
pub fn reserve_port() -> u16 {
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve an ephemeral port");
    let port = probe.local_addr().expect("local_addr").port();
    drop(probe);
    port
}

/// Spawn the peer a happy-path listener-mode accept needs: retry `connect`
/// until the listener under test has bound (its first attempt can run
/// before the bind), then hold the connected socket open until the caller
/// sends on the returned channel — closing it mid-flight lets libsrt's GC
/// reap the listener-side accepted socket before `srt_accept` resolves it
/// (the libsrt accept-queue prune).
///
/// `slot` is the accept's cancel slot, and giving up fires it BEFORE the
/// panic: the accept runs on the test's own thread with nothing else able
/// to reach it, so a connector that dies while the accept is parked would
/// leave the test in `srt_accept` forever — a hang instead of a failure,
/// and a thread left parked there stalls process exit on top (the
/// `atexit(srt_cleanup)` class).
pub fn spawn_connector(
    port: u16,
    slot: Arc<CancelSlot>,
) -> (mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let attempt = tst_srt::SrtSocketBuilder::new()
                .connect_timeout(Duration::from_millis(500))
                .connect(format!("127.0.0.1:{port}"));
            match attempt {
                Ok(socket) => {
                    let _ = release_rx.recv();
                    drop(socket);
                    return;
                }
                Err(e) => {
                    if Instant::now() >= deadline {
                        // Release the parked accept first, then report.
                        slot.cancel();
                        panic!("connect never succeeded: {e:?}");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    });
    (release_tx, handle)
}

// ------------------------------------------------------------------------
// ParkLatch: proof that a call is parked inside tst-srt.
// ------------------------------------------------------------------------

/// How long the park has to hold before [`ParkLatch::assert_parked`]
/// believes it.
pub fn park_look() -> Duration {
    scaled(Duration::from_millis(200))
}

/// How long [`ParkLatch::assert_parked`] waits for the park to settle.
pub fn park_deadline() -> Duration {
    scaled(Duration::from_secs(8))
}

/// Proof that a worker is parked inside a `tst-srt` call.
///
/// A test of a cancel that wakes a parked call proves nothing if the call
/// was never parked: a call that failed or finished at once passes the
/// same assertions with the cancel removed. `entered` / `returned` bracket
/// the call, but on their own they prove little: a worker descheduled
/// right after it set `entered` looks the same as one inside libsrt. The
/// observation is the library's own count of operations in flight —
/// non-zero means a thread has entered a `tst-srt` operation and has not
/// returned from it. The park is proven when, for the whole of
/// [`park_look`], that count is non-zero, the call has not returned, and
/// `progress` has not moved (a worker that calls in a loop [`bump`]s it
/// after every call that completed).
///
/// The count is process-wide, so the proof holds under nextest's
/// process-per-test model, not beside unrelated tests in one process.
///
/// [`bump`]: Self::bump
#[derive(Clone, Default)]
pub struct ParkLatch {
    entered: Arc<AtomicBool>,
    returned: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
    /// The parked closure's type name: it names the test that parked it,
    /// for the failure messages.
    call: Arc<OnceLock<&'static str>>,
}

impl ParkLatch {
    /// Run `call` on its own thread, bracketed by the latch. The handle
    /// yields the call's result; a caller that only needs the park (the
    /// process exits under it) drops the handle.
    pub fn park<F, R>(&self, call: F) -> std::thread::JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let _ = self.call.set(std::any::type_name::<F>());
        let latch = self.clone();
        std::thread::spawn(move || {
            latch.entered.store(true, Ordering::SeqCst);
            let result = call();
            latch.returned.store(true, Ordering::SeqCst);
            result
        })
    }

    /// Calls the worker completed so far.
    pub fn progress(&self) -> u64 {
        self.progress.load(Ordering::SeqCst)
    }

    /// Record one completed call: a worker that calls in a loop bumps after
    /// every call, so a loop that is still making progress is not a park.
    pub fn bump(&self) {
        self.progress.fetch_add(1, Ordering::SeqCst);
    }

    /// Wait for the thread to reach the call, then require the park to
    /// hold for [`park_look`]. The look starts over whenever the worker is
    /// seen outside an operation or making progress, up to
    /// [`park_deadline`]. Panics, naming the call, if the call returns or
    /// the park never settles.
    pub fn assert_parked(&self) {
        let call = self.call.get().copied().unwrap_or("<no call parked>");
        wait_for_ready(&self.entered);
        let (look, deadline_in) = (park_look(), park_deadline());
        let deadline = Instant::now() + deadline_in;
        let mut seen = self.progress();
        let mut held_since = Instant::now();
        while held_since.elapsed() < look {
            assert!(
                !self.returned.load(Ordering::SeqCst),
                "{call}: the blocking call returned; nothing is parked"
            );
            assert!(
                Instant::now() < deadline,
                "{call}: no thread stayed inside a tst-srt operation for \
                 {look:?} within {deadline_in:?}"
            );
            let now = self.progress();
            if tst_srt::operations_in_flight() == 0 || now != seen {
                seen = now;
                held_since = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Read `SRTO_PBKEYLEN` (the negotiated AES key length in bytes) off a socket.
pub fn pbkeylen(handle: srt_sys::SRTSOCKET) -> i32 {
    let mut value: c_int = 0;
    let mut len = std::mem::size_of::<c_int>() as c_int;
    let rc = unsafe {
        srt_sys::srt_getsockflag(
            handle,
            srt_sys::SRT_SOCKOPT_SRTO_PBKEYLEN,
            (&raw mut value).cast(),
            &raw mut len,
        )
    };
    assert!(rc >= 0, "srt_getsockflag(SRTO_PBKEYLEN) failed");
    value
}

/// Assert a connect attempt was refused by the listener with `expected`.
pub fn assert_rejected(result: Result<SrtSocket, SrtConnectError>, expected: RejectReason) {
    match result {
        Err(SrtConnectError::Rejected { reason, .. }) => assert_eq!(reason, expected),
        Err(e) => panic!("expected Rejected {{ reason: {expected:?} }}, got {e:?}"),
        Ok(_) => panic!("expected Rejected {{ reason: {expected:?} }}, got a connection"),
    }
}
