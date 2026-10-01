//! Crate-private process-exit guard: get every thread out of libsrt, and
//! keep new ones from entering, before `srt_cleanup()` runs.
//!
//! `srt_cleanup()` (registered with `atexit` in [`crate::init`]) joins
//! libsrt's `SRT:GC` thread, and libsrt's C++ static destructors run after
//! it. Two things go wrong when another thread is still using libsrt at
//! that point:
//!
//! - a thread PARKED in `srt_accept` keeps the GC thread from finishing (it
//!   destroys the condition variable the accept is waiting on), so the join
//!   never returns and the process hangs in its exit handlers. A program
//!   cannot always avoid this by closing first: the first accept inside a
//!   listener-mode URL open parks before the caller holds anything to
//!   close;
//! - a thread that makes ANY libsrt call after the teardown — reading the
//!   reject reason of a connect that just failed, an option read, a close —
//!   takes a lock in libsrt's global state that no longer exists.
//!
//! So the guard tracks whole operations, not blocking calls:
//!
//! - every entry point of this crate that calls into libsrt holds an
//!   [`Operation`] from before its first libsrt call until after its last
//!   ([`enter`]). Once the exit handler has started, [`enter`] refuses and
//!   the entry point answers with its caller-close error without touching
//!   libsrt;
//! - a registry of every open libsrt socket lets the handler close them,
//!   which is what wakes the parked calls;
//! - [`release_parked_calls`] then waits, bounded, for the operation count
//!   to reach zero.
//!
//! A program that closed everything pays nothing at exit: the registry is
//! empty, the count is zero, and the handler goes straight to
//! `srt_cleanup()`. On the data path an operation costs one atomic
//! increment and decrement and two atomic loads; no lock is taken.
//!
//! Nothing after `EXITING` is set may use `tracing`: the handler runs after
//! glibc has destroyed the exiting thread's thread-locals, and a fmt
//! subscriber formats into one (see [`exit_note`]).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tst_core::SrtCancelHandle;

/// Hard ceiling on the wait for the operations in flight to return, counted
/// from the moment every open socket has been closed. A cancelled call
/// wakes within one libsrt I/O cycle (~3-10 ms) and the rest of its entry
/// point is a handful of non-blocking calls; the ceiling only bounds the
/// case where an operation cannot be woken.
///
/// It is NOT a bound on how long exit takes: the handler closes the open
/// sockets first, one after the other, and `srt_close` on a connected
/// socket that still has unsent data waits for up to that socket's
/// `SRTO_LINGER` before it returns.
const RELEASE_CEILING: Duration = Duration::from_secs(2);

/// What a refused entry point says when its error type carries a message.
pub(crate) const REFUSED: &str = "the process is exiting";

/// Every open socket, keyed by registration id (NOT by `SRTSOCKET`, which
/// libsrt is free to hand out again after a close). The entry is the right
/// to call `srt_close`: whoever removes it, under the lock, closes.
static OPEN_SOCKETS: Mutex<Option<HashMap<u64, OpenSocket>>> = Mutex::new(None);

struct OpenSocket {
    handle: srt_sys::SRTSOCKET,
    /// Clone of the handle the `Socket` / `Listener` owns, so the exit
    /// handler can latch `is_cancelled()` before it closes.
    cancel: SrtCancelHandle,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Operations that have been admitted and have not returned, plus closes
/// in progress on threads other than the exit handler's.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Set once the exit handler has started, and never cleared.
static EXITING: AtomicBool = AtomicBool::new(false);

/// Poison is recovered, never propagated: every critical section is a
/// single map operation, so a panic cannot leave the map torn, and an exit
/// handler that gave up after an unrelated panic would be the hang this
/// module exists to prevent.
fn open_sockets() -> std::sync::MutexGuard<'static, Option<HashMap<u64, OpenSocket>>> {
    OPEN_SOCKETS.lock().unwrap_or_else(|e| e.into_inner())
}

/// RAII marker for one operation inside this crate's libsrt-calling code.
pub(crate) struct Operation(());

/// Admit one operation, or refuse it because the process is exiting.
///
/// Call this before the entry point's first libsrt call — before
/// [`crate::init::ensure_initialized`] too — and hold the result until
/// after its last, error paths included. On `None` the entry point must
/// return its caller-close error and make no libsrt call.
///
/// Entry points nest (`accept_timeout` calls `accept`); each level takes
/// its own `Operation`. It is a count, so nesting cannot deadlock, and the
/// handler only ever asks whether it is zero.
///
/// # Why zero means nobody is inside
///
/// This thread increments [`IN_FLIGHT`] and THEN reads [`EXITING`]; the
/// exit handler stores `EXITING` and THEN reads `IN_FLIGHT`. All four are
/// `SeqCst`, so they have one total order, and in that order either the
/// increment comes before the handler's read — the handler sees a non-zero
/// count and waits for this operation — or the handler's store comes before
/// this thread's read — this thread sees the flag, takes its increment back
/// and never enters libsrt. There is no order in which the handler reads
/// zero and this operation is admitted. (With anything weaker than `SeqCst`
/// both sides may read the other's old value; this is the store-then-load
/// pattern on both sides.)
///
/// That covers initialization as well. The exit handler is registered by
/// the first admitted operation, after `srt_startup()` returned, so it
/// cannot run while an operation that started libsrt up is still inside
/// its entry point without waiting for it; and a first use that arrives
/// after the handler started is refused before it reaches `srt_startup()`.
pub(crate) fn enter() -> Option<Operation> {
    // Not needed for correctness. It keeps a thread that calls in a loop
    // after exit started from flickering the count the handler polls.
    if EXITING.load(Ordering::SeqCst) {
        return None;
    }
    IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
    if EXITING.load(Ordering::SeqCst) {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        return None;
    }
    Some(Operation(()))
}

impl Drop for Operation {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether the exit handler has started. For the entry points that walk or
/// retry: once this is true they stop instead of trying the next address or
/// binding again.
pub(crate) fn is_exiting() -> bool {
    EXITING.load(Ordering::SeqCst)
}

/// Operations currently in flight. For the tests.
#[doc(hidden)]
pub fn operations_in_flight() -> u64 {
    IN_FLIGHT.load(Ordering::SeqCst) as u64
}

/// Build the close-once handle for `handle` and register it.
///
/// Call from inside an [`Operation`]: if the exit handler has already
/// started, the socket is closed here, on the calling thread.
pub(crate) fn tracked_cancel_handle(handle: srt_sys::SRTSOCKET) -> SrtCancelHandle {
    register(handle).1
}

/// [`tracked_cancel_handle`] plus the registration id, for the tests.
fn register(handle: srt_sys::SRTSOCKET) -> (u64, SrtCancelHandle) {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let cancel = SrtCancelHandle::new(handle as i64, move |h| {
        close_registered(id, h as srt_sys::SRTSOCKET);
    });
    let exiting = {
        let mut reg = open_sockets();
        // Read under the lock: the exit handler sets the flag before it
        // takes the lock to drain, so either this entry is drained or the
        // flag is seen here.
        let exiting = EXITING.load(Ordering::SeqCst);
        if !exiting {
            reg.get_or_insert_with(HashMap::new).insert(
                id,
                OpenSocket {
                    handle,
                    cancel: cancel.clone(),
                },
            );
        }
        exiting
    };
    if exiting {
        // Never registered, so the closer will find no entry and leave the
        // close to us. The caller's `Operation` covers it. Latch first, as
        // the exit handler does: what the caller sees next is a close.
        cancel.cancel();
        // SAFETY: `handle` is the live socket the caller just created or
        // accepted; nothing else can close it.
        let _ = unsafe { srt_sys::srt_close(handle) };
    }
    (id, cancel)
}

/// The closer behind every tracked [`SrtCancelHandle`]: runs at most once
/// per socket, on whichever thread fired the handle — an explicit close, a
/// cancel from another thread, or `Drop`.
///
/// A close has to happen whether or not the process is exiting, so it does
/// not go through [`enter`]. What admits it is the registry entry: the
/// thread that removes the entry closes the socket, and it adds itself to
/// [`IN_FLIGHT`] under the same lock. The exit handler drains the registry
/// under that lock, so every close on another thread is either already
/// counted when the handler looks (it removed its entry first) or never
/// happens there (the handler took the entry and closes the socket
/// itself). The handler's own closes run on the handler's thread before it
/// starts waiting, so they need no count — it cannot overtake itself.
fn close_registered(id: u64, handle: srt_sys::SRTSOCKET) {
    let closing = {
        let mut reg = open_sockets();
        let ours = reg.as_mut().and_then(|m| m.remove(&id)).is_some();
        ours.then(|| {
            IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
            Operation(())
        })
    };
    // No entry: closed at registration, or taken over by the exit handler.
    let Some(_closing) = closing else { return };
    checkpoint(Checkpoint::InClose);
    // SAFETY: `handle` is the SRTSOCKET this closer was built for; libsrt
    // accepts srt_close from any thread; removing the entry under the lock
    // guarantees this runs at most once.
    let _ = unsafe { srt_sys::srt_close(handle) };
}

/// Points a test can pause a thread at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Checkpoint {
    /// A blocking libsrt call has returned; its entry point has not.
    AfterBlockingCall,
    /// Inside the closer, immediately before `srt_close`.
    InClose,
}

#[cfg(test)]
type CheckpointHook = Box<dyn Fn(Checkpoint)>;

#[cfg(test)]
thread_local! {
    /// Per thread, so a hook installed by one test can never pause a
    /// thread that belongs to another.
    static CHECKPOINT_HOOK: std::cell::RefCell<Option<CheckpointHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Install `hook` for the calling thread.
#[cfg(test)]
pub(crate) fn set_checkpoint_hook(hook: impl Fn(Checkpoint) + 'static) {
    CHECKPOINT_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
}

/// Test checkpoint. Compiles to nothing outside the crate's unit tests.
#[cfg(test)]
pub(crate) fn checkpoint(at: Checkpoint) {
    CHECKPOINT_HOOK.with(|h| {
        if let Some(hook) = h.borrow().as_ref() {
            hook(at);
        }
    });
}

#[cfg(not(test))]
#[inline(always)]
pub(crate) fn checkpoint(_at: Checkpoint) {}

/// The one message the exit handler can emit. Written with a raw
/// `write(2)` on unix (`std::io::stderr` on windows): `tracing` is off
/// limits here — a fmt subscriber formats into a thread-local whose
/// destructor glibc has already run by the time `atexit` handlers execute,
/// and `LocalKey::with` on a destroyed key panics, which aborts an
/// `extern "C"` function. libsrt's own `CUDTUnited::cleanup` carries the
/// same rule ("NO LOGGING AT ALL").
fn exit_note(in_flight: usize) {
    use std::io::Write as _;
    let mut buf = [0u8; 160];
    let mut cur = std::io::Cursor::new(&mut buf[..]);
    let _ = writeln!(
        cur,
        "tst-srt: process exit: {in_flight} operation(s) still inside libsrt after {RELEASE_CEILING:?}; running srt_cleanup() anyway"
    );
    let n = cur.position() as usize;
    #[cfg(unix)]
    {
        // SAFETY: fd 2 is stderr for the process lifetime; `buf[..n]` is
        // initialised. A failed write is ignored — there is nowhere to
        // report it.
        let _ = unsafe { libc::write(2, buf.as_ptr().cast(), n) };
    }
    #[cfg(not(unix))]
    {
        let _ = std::io::stderr().write_all(&buf[..n]);
    }
}

/// Refuse new operations, close every socket that is still open, then wait
/// (bounded by [`RELEASE_CEILING`]) until no operation is in flight.
///
/// The ceiling covers the wait only. The closes before it are not bounded
/// here: each `srt_close` can take up to its socket's `SRTO_LINGER` when
/// unsent data is queued (see [`RELEASE_CEILING`]).
///
/// The handler cannot shorten that with `srt_setsockopt(SRTO_LINGER)`:
/// libsrt's `CUDT::setOpt` takes `m_ConnectionLock`, `m_SendLock` and
/// `m_RecvLock` (core.cpp:547-549), and a parked blocking connect or send
/// holds the first or second for the whole call (core.cpp:3695, ~6986)
/// until the close wakes it. With a setsockopt before each close,
/// `exit_with_parked_call::exit_is_clean_with_a_caller_parked_in_connect`
/// hung for its full 12 s. So each serial close keeps its socket's
/// configured linger.
///
/// If the ceiling expires with operations still in flight, `srt_cleanup()`
/// runs anyway. That is what happened before this guard existed, with the
/// same exposure: a thread still inside libsrt may crash or hang the
/// exiting process. Skipping `srt_cleanup()` instead is not an option — it
/// is a known crash after `main` under libsrt >= 1.5.6 (see
/// [`crate::init`]).
///
/// Called from the `extern "C"` exit handler, immediately before
/// `srt_cleanup()`: nothing here may panic.
pub(crate) fn release_parked_calls() {
    EXITING.store(true, Ordering::SeqCst);
    // Drain under the lock, close outside it: a closer on another thread
    // takes the lock to look for its entry.
    let open = open_sockets().take();
    if let Some(open) = open {
        for OpenSocket { handle, cancel } in open.into_values() {
            // `cancel`, not `close_without_cancel`: the parked call then
            // reports a caller-side close, which ends a reconnecting
            // transport instead of sending it into another attempt. The
            // closer it may run finds no entry and returns; the close is
            // the next line.
            cancel.cancel();
            // SAFETY: the entry was still registered, so no other thread
            // has closed `handle` or ever will.
            let _ = unsafe { srt_sys::srt_close(handle) };
        }
    }
    let deadline = Instant::now() + RELEASE_CEILING;
    loop {
        let in_flight = IN_FLIGHT.load(Ordering::SeqCst);
        if in_flight == 0 {
            return;
        }
        if Instant::now() >= deadline {
            exit_note(in_flight);
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Put the guard in its exiting state without running the exit handler.
/// Process-global and never reset: only for a test that owns its process.
#[cfg(test)]
pub(crate) fn set_exiting_for_test() {
    EXITING.store(true, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered(id: u64) -> bool {
        open_sockets().as_ref().is_some_and(|m| m.contains_key(&id))
    }

    use crate::config::{ListenerConfig, SocketConfig};
    use crate::error::{AcceptError, BindError, ConnectError, RecvError, SendError};
    use crate::{Listener, Socket, SrtTransport};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use tst_core::transport::{RecvTransport, Transport, TransportError};

    /// Set (to any value) in the child's environment.
    const CHILD_ENV: &str = "TST_SRT_EXIT_GUARD_CHILD";

    /// How long a parent lets its child run, and how long a test waits for
    /// one of its own threads. Under nextest's 20 s kill.
    const DEADLINE: Duration = Duration::from_secs(12);

    /// The guard's state is process-global, so a test that sets the
    /// exiting flag or reads the operation count needs a process of its
    /// own. Returns `true` in the child; in the parent it re-executes this
    /// test binary on `test_name` and requires a clean exit.
    fn in_own_process(test_name: &str) -> bool {
        if std::env::var_os(CHILD_ENV).is_some() {
            return true;
        }
        let exe = std::env::current_exe().expect("current_exe");
        let mut child = Command::new(exe)
            .args([test_name, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn the child test process");
        let deadline = Instant::now() + DEADLINE;
        let status = loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{test_name}: the child did not exit within {DEADLINE:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(status.code(), Some(0), "{test_name}: the child failed");
        false
    }

    fn loopback_available() -> bool {
        std::env::var_os("SKIP_LOOPBACK").is_none()
            && std::net::TcpListener::bind("127.0.0.1:0").is_ok()
    }

    /// Run `body` on a worker that pauses at `at`, and return the operation
    /// count read while it is paused there. The worker is released when
    /// this returns (or when its own deadline passes) and joined.
    fn count_while_paused_at<R: Send + 'static>(
        at: Checkpoint,
        body: impl FnOnce() -> R + Send + 'static,
    ) -> u64 {
        let (paused_tx, paused_rx) = mpsc::channel::<()>();
        let (resume_tx, resume_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            set_checkpoint_hook(move |reached| {
                if reached == at {
                    let _ = paused_tx.send(());
                    // Rescue: never stay paused past the deadline.
                    let _ = resume_rx.recv_timeout(DEADLINE);
                }
            });
            let _ = body();
            let _ = done_tx.send(());
        });
        paused_rx
            .recv_timeout(DEADLINE)
            .expect("the worker must reach the checkpoint");
        let count = operations_in_flight();
        let _ = resume_tx.send(());
        done_rx
            .recv_timeout(DEADLINE)
            .expect("the worker must finish once released");
        worker.join().expect("join the worker");
        count
    }

    /// After `srt_connect` returns, `connect_with` still reads the reject
    /// reason and closes the socket. The exit handler must see the thread
    /// for that whole tail, not only for the blocking call.
    #[test]
    fn a_connect_is_counted_until_its_entry_point_returns_on_loopback() {
        if !in_own_process(
            "exit_guard::tests::a_connect_is_counted_until_its_entry_point_returns_on_loopback",
        ) {
            return;
        }
        if !loopback_available() {
            eprintln!("SKIP: loopback unavailable");
            return;
        }
        // A plain UDP socket never answers the handshake, so the connect
        // fails at its timeout and takes the error tail.
        let silent = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind the silent peer");
        let port = silent.local_addr().expect("local_addr").port();
        let config = SocketConfig {
            connect_timeout: Some(Duration::from_millis(300)),
            ..SocketConfig::default()
        };
        let count = count_while_paused_at(Checkpoint::AfterBlockingCall, move || {
            Socket::connect_with(&config, format!("127.0.0.1:{port}"))
        });
        assert_eq!(
            count, 1,
            "a thread between the return of srt_connect and the end of \
             connect_with must be counted as in flight"
        );
        assert_eq!(operations_in_flight(), 0, "nothing is left in flight");
    }

    /// `srt_close` takes libsrt's global lock like any other call, so a
    /// close on another thread must hold the exit handler back too.
    #[test]
    fn a_close_in_progress_is_counted() {
        if !in_own_process("exit_guard::tests::a_close_in_progress_is_counted") {
            return;
        }
        crate::init::ensure_initialized();
        let h = unsafe { srt_sys::srt_create_socket() };
        assert_ne!(h, -1);
        let (_id, cancel) = register(h);
        let count = count_while_paused_at(Checkpoint::InClose, move || cancel.cancel());
        assert_eq!(count, 1, "a close in progress must be counted as in flight");
        assert_eq!(operations_in_flight(), 0, "nothing is left in flight");
    }

    /// Once the exit handler has started, nothing new enters libsrt: every
    /// entry point answers with its caller-close error instead.
    #[test]
    fn nothing_is_admitted_once_exit_has_started_on_loopback() {
        if !in_own_process(
            "exit_guard::tests::nothing_is_admitted_once_exit_has_started_on_loopback",
        ) {
            return;
        }
        if !loopback_available() {
            eprintln!("SKIP: loopback unavailable");
            return;
        }
        // A live connection, built before the flag is set.
        let mut listener =
            Listener::bind_with(&ListenerConfig::default(), "127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        let caller = std::thread::spawn(move || {
            Socket::connect_with(&SocketConfig::default(), format!("127.0.0.1:{port}"))
        });
        let (mut accepted, _peer) = listener.accept().expect("accept");
        let mut socket = caller.join().expect("join the caller").expect("connect");

        set_exiting_for_test();

        // Each of these would park (accept, recv) or succeed (send) if it
        // reached libsrt: the connection is up and idle.
        assert!(matches!(
            listener.accept(),
            Err(AcceptError::ListenerClosed)
        ));
        assert!(matches!(
            listener.accept_timeout(Duration::from_secs(300)),
            Err(AcceptError::ListenerClosed)
        ));
        let mut buf = [0u8; 1500];
        assert!(matches!(
            accepted.recv(&mut buf),
            Err(RecvError::ConnectionBroken)
        ));
        assert!(matches!(
            socket.send(&[0x47; 188]),
            Err(SendError::ConnectionBroken)
        ));
        assert!(socket.stats().is_err());
        assert!(socket.peer_addr().is_err());
        assert!(listener.local_addr().is_err());

        let mut transport = SrtTransport::new(socket);
        assert!(matches!(
            transport.send_bytes(&[0x47; 188]),
            Err(TransportError::ExplicitClose)
        ));
        assert!(matches!(
            transport.recv_bytes(&mut buf),
            Err(TransportError::ExplicitClose)
        ));
        let slot = tst_core::cancel::CancelSlot::new();
        assert!(matches!(
            Listener::accept_one_cancellable(&ListenerConfig::default(), "127.0.0.1:0", &slot),
            Err(TransportError::ExplicitClose)
        ));
        assert_eq!(
            operations_in_flight(),
            0,
            "a refusal leaves nothing in flight"
        );
    }

    /// A first use that arrives after exit has started must not start
    /// libsrt up at all.
    #[test]
    fn a_first_use_after_exit_has_started_does_not_start_libsrt() {
        if !in_own_process(
            "exit_guard::tests::a_first_use_after_exit_has_started_does_not_start_libsrt",
        ) {
            return;
        }
        set_exiting_for_test();
        assert!(matches!(
            Socket::connect_with(&SocketConfig::default(), "127.0.0.1:9"),
            Err(ConnectError::Other { .. })
        ));
        assert!(matches!(
            Listener::bind_with(&ListenerConfig::default(), "127.0.0.1:0"),
            Err(BindError::Other { .. })
        ));
        assert!(
            !crate::init::is_initialized(),
            "a refused open must not call srt_startup"
        );
    }

    /// R7-01 (review #7, internal): the ceiling warning used to go through
    /// `tracing`. A `tracing_subscriber::fmt` layer formats into a
    /// destructor-bearing thread-local; glibc runs thread-local
    /// destructors BEFORE `atexit` handlers, so the first event emitted
    /// from the exit handler on a thread that had logged before panicked
    /// inside an `extern "C"` function and aborted the process. The
    /// child must print the note and exit 0.
    #[test]
    fn ceiling_warning_does_not_abort_the_exiting_process() {
        if !in_own_process("exit_guard::tests::ceiling_warning_does_not_abort_the_exiting_process")
        {
            return;
        }
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .init();
        tracing::info!("one event on the exiting thread, so the fmt layer's buffer exists");
        crate::init::ensure_initialized();
        // An operation that is never released: the handler waits out the
        // ceiling and takes the warning path.
        let _op = std::mem::ManuallyDrop::new(enter().expect("admitted"));
        std::process::exit(0);
    }

    /// A closed socket leaves the registry, so the exit walk can neither
    /// close it twice nor grow without bound over a long-lived process.
    #[test]
    fn closing_by_any_path_deregisters() {
        crate::init::ensure_initialized();
        for close in [
            SrtCancelHandle::cancel as fn(&SrtCancelHandle),
            SrtCancelHandle::close_without_cancel,
        ] {
            let h = unsafe { srt_sys::srt_create_socket() };
            assert_ne!(h, -1);
            let (id, cancel) = register(h);
            assert!(registered(id), "an open socket must be registered");
            close(&cancel);
            assert!(!registered(id), "a closed socket must leave the registry");
        }
    }
}
