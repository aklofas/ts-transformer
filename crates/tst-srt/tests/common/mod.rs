//! Shared scaffolding for tst-srt integration tests.
//! Loopback only; no external network.

#![allow(dead_code)]

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Small wall-clock pause to give a listener thread time to enter accept().
pub fn settle() {
    std::thread::sleep(Duration::from_millis(100));
}

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
// See plan: docs/plans/2026-05-15-srt-loopback-test-helper.md
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
/// use tst_srt::SocketBuilder;
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
///     let mut socket = SocketBuilder::new()
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
///   listener BEFORE spawn for use in `SocketBuilder::connect`.
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
    pub listener: tst_srt::Listener,
    pub port: u16,
}

impl Loopback {
    /// Bind a listener to `127.0.0.1:0` with 5-second recv/send timeouts.
    /// Panics if bind fails — the test should have called `require_loopback!()`
    /// first.
    pub fn bind() -> Self {
        let listener = tst_srt::ListenerBuilder::new()
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
    pub fn bind_with(builder: tst_srt::ListenerBuilder) -> Self {
        Self::bind_at(builder, "127.0.0.1:0")
    }

    /// [`bind_with`](Self::bind_with) at a caller-supplied address — for the
    /// IPv6 loopback test (`[::1]:0`). Read `listener.local_addr()` off the
    /// returned fixture to assert the family it actually bound.
    pub fn bind_at(builder: tst_srt::ListenerBuilder, addr: &str) -> Self {
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
        F: FnOnce(tst_srt::Socket) -> R + Send + 'static,
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
/// hangs to nextest's kill (tst-c's managed demux receiver test, twice, PR
/// #231). Two guards, both driven by the listener's pre-obtained cancel
/// handle (`cancel()` closes the listening socket, which wakes a parked
/// `accept()` with an error):
///
/// - [`join`](Self::join) bounds the ACCEPT phase by [`ACCEPT_DEADLINE`]:
///   on expiry it fires the cancel, reaps the woken thread, and panics
///   naming this class. It never fires the cancel while the accept can
///   still succeed, and the closure phase after a successful accept is
///   not bounded at all — long recv loops stay legal.
/// - `Drop` fires the cancel and joins when the handle is dropped without
///   `join` — i.e. a test unwinding on an `expect` before its join. A
///   leaked thread parked in `srt_accept` does worse than leak: it holds
///   the port AND stalls process exit (`srt_cleanup` runs at exit). The
///   drop path swallows a peer panic (a panic is already in flight); the
///   happy path's explicit `join` is what reports one.
pub struct AcceptHandle<R> {
    handle: Option<std::thread::JoinHandle<Result<R, tst_srt::AcceptError>>>,
    ready: Arc<AtomicBool>,
    /// Set by the thread the moment `accept()` returns, Ok or Err.
    accepted: Arc<AtomicBool>,
    cancel: tst_srt::SrtCancelHandle,
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
                let outcome = handle.join();
                // The accept can still have won the race against the cancel
                // and delivered a socket; only a lost one is the failure.
                if let Ok(Ok(value)) = outcome {
                    return value;
                }
                panic!(
                    "AcceptHandle::join: accept() still parked after \
                     {accept_deadline:?} with no connection to dequeue — the \
                     libsrt accept-queue prune class (GC erased a broken \
                     queued connection; PR #231). The caller's connect \
                     either never happened or its socket closed before the \
                     peer thread got scheduled."
                );
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
        if let Some(handle) = self.handle.take() {
            self.cancel.cancel();
            // Bounded: after the cancel the accept returns at once, and the
            // closure phase is bounded by the fixture's socket timeouts.
            let _ = handle.join();
        }
    }
}
