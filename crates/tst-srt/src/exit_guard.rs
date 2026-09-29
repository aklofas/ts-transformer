//! Crate-private process-exit guard: unpark every thread that is still
//! inside libsrt before `srt_cleanup()` runs.
//!
//! `srt_cleanup()` (registered with `atexit` in [`crate::init`]) joins
//! libsrt's `SRT:GC` thread. With a thread parked in `srt_accept` that join
//! never returns: the GC thread destroys the condition variable the accept
//! is waiting on, and `pthread_cond_destroy` blocks until the waiter is
//! gone — which it never is, because nothing wakes it. The process hangs in
//! its exit handlers. A program cannot always avoid this by closing first:
//! the first accept inside a listener-mode URL open parks before the caller
//! holds anything to close.
//!
//! Two pieces, both walked by [`release_parked_calls`] from the exit
//! handler:
//!
//! - a registry of every open libsrt socket, holding a clone of the
//!   [`SrtCancelHandle`] its `Socket` / `Listener` owns. The handle's
//!   closer removes the entry, so a socket that was closed by any path
//!   (explicit close, cancel, `Drop`) is not in the registry and is never
//!   closed a second time.
//! - a count of threads currently inside a blocking libsrt call
//!   ([`enter_blocking_call`]), so the handler can wait for the woken calls
//!   to actually return instead of sleeping for a guessed interval.
//!
//! A program that closed everything pays nothing: the registry is empty,
//! the count is zero, and the handler goes straight to `srt_cleanup()`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tst_core::SrtCancelHandle;

/// Hard ceiling on the wait for parked calls to return. A cancelled call
/// wakes within one libsrt I/O cycle (~3-10 ms); the ceiling only bounds
/// the case where a call cannot be woken, and `srt_cleanup()` runs
/// regardless once it expires.
const RELEASE_CEILING: Duration = Duration::from_secs(2);

/// Every open socket, keyed by registration id (NOT by `SRTSOCKET`, which
/// libsrt is free to hand out again after a close).
static OPEN_SOCKETS: Mutex<Option<HashMap<u64, SrtCancelHandle>>> = Mutex::new(None);

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Threads currently inside a blocking libsrt call.
static BLOCKING_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Set once the exit handler has started. A socket opened after that point
/// would be missed by the registry walk, so it is closed at registration.
static EXITING: AtomicBool = AtomicBool::new(false);

/// Poison is recovered, never propagated: every critical section is a
/// single map operation, so a panic cannot leave the map torn, and an exit
/// handler that gave up after an unrelated panic would be the hang this
/// module exists to prevent.
fn open_sockets() -> std::sync::MutexGuard<'static, Option<HashMap<u64, SrtCancelHandle>>> {
    OPEN_SOCKETS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Build the close-once handle for `handle` and register it.
///
/// The closer deregisters before it closes, so the registry only ever
/// holds sockets whose `srt_close` has not run.
pub(crate) fn tracked_cancel_handle(handle: srt_sys::SRTSOCKET) -> SrtCancelHandle {
    register(handle).1
}

/// [`tracked_cancel_handle`] plus the registration id, for the tests.
fn register(handle: srt_sys::SRTSOCKET) -> (u64, SrtCancelHandle) {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let cancel = SrtCancelHandle::new(handle as i64, move |h| {
        open_sockets().as_mut().map(|m| m.remove(&id));
        // SAFETY: h was the same SRTSOCKET we stored; libsrt accepts
        // srt_close from any thread; the atomic-swap in SrtCancelHandle
        // guarantees this runs at most once.
        let _ = unsafe { srt_sys::srt_close(h as srt_sys::SRTSOCKET) };
    });
    let exiting = {
        let mut reg = open_sockets();
        // Read under the lock: the exit handler sets the flag before it
        // takes the lock to drain, so either this entry is drained or the
        // flag is seen here.
        let exiting = EXITING.load(Ordering::SeqCst);
        if !exiting {
            reg.get_or_insert_with(HashMap::new)
                .insert(id, cancel.clone());
        }
        exiting
    };
    if exiting {
        // Outside the lock — the closer takes it.
        cancel.cancel();
    }
    (id, cancel)
}

/// RAII marker for one thread inside a blocking libsrt call.
pub(crate) struct BlockingCall(());

/// Bracket a blocking libsrt call (`srt_accept`, `srt_epoll_wait`,
/// `srt_connect`, `srt_send`, `srt_recv`). Hold the returned guard across
/// the call only.
pub(crate) fn enter_blocking_call() -> BlockingCall {
    BLOCKING_CALLS.fetch_add(1, Ordering::SeqCst);
    BlockingCall(())
}

impl Drop for BlockingCall {
    fn drop(&mut self) {
        BLOCKING_CALLS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Close every socket that is still open, then wait (bounded by
/// [`RELEASE_CEILING`]) until no thread is inside a blocking libsrt call.
///
/// Called from the `extern "C"` exit handler, immediately before
/// `srt_cleanup()`: nothing here may panic.
pub(crate) fn release_parked_calls() {
    EXITING.store(true, Ordering::SeqCst);
    // Drain under the lock, fire outside it: each closer takes the lock to
    // remove its own entry.
    let open = open_sockets().take();
    if let Some(open) = open {
        for cancel in open.into_values() {
            // `cancel`, not `close_without_cancel`: the parked call then
            // reports a caller-side close, which ends a reconnecting
            // transport instead of sending it into another attempt.
            cancel.cancel();
        }
    }
    let deadline = Instant::now() + RELEASE_CEILING;
    while BLOCKING_CALLS.load(Ordering::SeqCst) != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered(id: u64) -> bool {
        open_sockets().as_ref().is_some_and(|m| m.contains_key(&id))
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
