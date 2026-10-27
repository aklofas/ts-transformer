//! Crate-private libsrt initialization.
//!
//! `srt_startup()` must be called once before any libsrt API use. Lazy via
//! `OnceLock` from any constructor that needs libsrt (`SrtSocket::connect_with`,
//! `SrtListener::bind_with`, etc.). Each of them is admitted by the exit guard
//! first, so the startup — and the registration of the exit handler at its
//! end — happens inside an operation the handler waits for.
//!
//! `srt_cleanup()` is never tied to value drops — see the design doc for
//! rationale (drop-order ambiguity vs. negligible OS-reclaimed leaks) — but
//! it IS registered via `atexit` to run at process exit; see
//! `register_exit_cleanup` below.
//!
//! Before `srt_cleanup()` the exit handler gets every other thread out of
//! libsrt (`crate::exit_guard`): it refuses operations that have not
//! started, closes every socket that is still open, and waits, bounded, for
//! the operations in flight to return — each one whole, from its first
//! libsrt call to its last. Without that, a thread left in `accept()` makes
//! `srt_cleanup()` — and with it process exit — hang, and a thread that
//! calls into libsrt after the teardown uses state that is gone. If the
//! bound expires, `srt_cleanup()` runs anyway. The bound does not cover the
//! closes: a connected socket with unsent data takes up to its
//! `SRTO_LINGER` to close. Closing and joining before exit remains the
//! recommended shape; the guard is for the program that did not.

use std::sync::OnceLock;

static SRT_INITIALIZED: OnceLock<()> = OnceLock::new();

/// Idempotent libsrt initialization.
///
/// Panics if `srt_startup` returns < 0 — that's a process-fatal condition.
pub(crate) fn ensure_initialized() {
    SRT_INITIALIZED.get_or_init(|| {
        let rc = unsafe { srt_sys::srt_startup() };
        if rc < 0 {
            panic!("srt_startup() failed with rc={rc}; libsrt cannot be used");
        }
        install_log_handler();
        register_exit_cleanup();
    });
}

/// Whether `srt_startup()` has run in this process. For the tests.
#[cfg(test)]
pub(crate) fn is_initialized() -> bool {
    SRT_INITIALIZED.get().is_some()
}

/// Register `srt_cleanup()` to run at process exit.
///
/// Required since libsrt 1.5.6: commit 1e4c908c (Haivision/srt#3327)
/// changed the RcvQueue worker to keep running when the UDP channel
/// reports an error mid-teardown ("the worker thread must run until all
/// sockets are removed from the multiplexer"). Exiting the process
/// WITHOUT `srt_cleanup()` now lets a live worker race libsrt's C++
/// static destructors and dereference the destroyed receive list — a
/// deterministic post-main SIGSEGV whenever any socket existed in the
/// process (on 1.5.5 the worker exited on that same channel error, which
/// is why skipping cleanup used to be benign).
///
/// Ordering: `atexit`/`__cxa_atexit` handlers run in REVERSE registration
/// order. libsrt's global-state destructor is registered during
/// `srt_startup()`, and this handler is registered after `srt_startup()`
/// returns, so `srt_cleanup()` — which stops the GC and queue workers —
/// always runs before those destructors.
///
/// The design-doc decision to keep `srt_cleanup()` away from `Drop` impls
/// (drop-order ambiguity between sockets/listeners) is unchanged; this is
/// process-exit only.
///
/// `srt_cleanup()` joins libsrt's GC thread, which cannot finish while a
/// thread is parked in `srt_accept`, so the handler first runs
/// `crate::exit_guard::release_parked_calls`. With nothing open and no
/// operation in flight that costs one uncontended lock, one atomic store
/// and one atomic load.
fn register_exit_cleanup() {
    extern "C" fn srt_exit_cleanup() {
        // Must not unwind out of an `extern "C"` function:
        // `release_parked_calls` recovers poisoned locks and has no
        // panicking path.
        crate::exit_guard::release_parked_calls();
        unsafe {
            srt_sys::srt_cleanup();
        }
    }
    // `atexit` can fail (POSIX: unspecified; in practice allocation
    // failure, plus implementation-defined handler limits). Since libsrt
    // 1.5.6 this registration is load-bearing: without the handler, any
    // process that opened an SRT socket segfaults after `main` returns.
    // Panicking here — at first libsrt use, where the failure is
    // attributable — beats a mystery SIGSEGV at process exit, and matches
    // the process-fatal handling of `srt_startup` failure above.
    let rc = unsafe { libc::atexit(srt_exit_cleanup) };
    if rc != 0 {
        panic!(
            "atexit(srt_cleanup) registration failed (rc={rc}); \
             refusing to continue — exiting without srt_cleanup() \
             segfaults under libsrt >= 1.5.6"
        );
    }
}

/// libsrt's ERROR-level line for an empty non-blocking `srt_accept`
/// (`CUDTUnited::accept`). `SrtListener::try_accept_nonblocking`
/// probes this way, so the line is routine and is logged at debug.
const ACCEPT_PROBE_EMPTY: &str = "no pending connection available";

/// libsrt log callback. A panic (a host `tracing` subscriber may panic) must
/// not unwind into libsrt's C++ frames, so the body is caught here.
extern "C" fn forward(
    _opaque: *mut std::ffi::c_void,
    level: std::ffi::c_int,
    _file: *const std::ffi::c_char,
    _line: std::ffi::c_int,
    _area: *const std::ffi::c_char,
    message: *const std::ffi::c_char,
) {
    // libsrt logs from inside `srt_cleanup()`; the exit handler's rule
    // (no `tracing` once exit started) applies here too.
    if crate::exit_guard::is_exiting() {
        return;
    }
    if message.is_null() {
        return;
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let msg = unsafe { std::ffi::CStr::from_ptr(message) }.to_string_lossy();
        // libsrt log levels (LOG_DEBUG=7, INFO=6, NOTICE=5, WARNING=4,
        // ERROR=3, CRITICAL=2, ALERT=1, EMERGENCY=0). Map roughly to
        // tracing levels. `tracing::event!` requires a const-known level,
        // so dispatch via the per-level macros rather than a runtime value.
        match level {
            7 => tracing::trace!(target: "srt", "{}", msg),
            6 => tracing::debug!(target: "srt", "{}", msg),
            5 => tracing::info!(target: "srt", "{}", msg),
            4 => tracing::warn!(target: "srt", "{}", msg),
            3 if msg.contains(ACCEPT_PROBE_EMPTY) => {
                tracing::debug!(target: "srt", "{}", msg)
            }
            _ => tracing::error!(target: "srt", "{}", msg),
        }
    }));
    if let Err(payload) = result {
        // A payload whose `Drop` panics would unwind out of this frame.
        std::mem::forget(payload);
    }
}

fn install_log_handler() {
    unsafe {
        srt_sys::srt_setloghandler(std::ptr::null_mut(), Some(forward));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PanickingSubscriber;
    impl tracing::Subscriber for PanickingSubscriber {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {
            panic!("subscriber panic")
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// A host subscriber that panics must not abort the process from inside
    /// the extern "C" callback libsrt calls.
    #[test]
    fn a_panicking_subscriber_does_not_abort_the_log_callback() {
        tracing::subscriber::with_default(PanickingSubscriber, || {
            forward(
                std::ptr::null_mut(),
                3,
                std::ptr::null(),
                0,
                std::ptr::null(),
                c"boom".as_ptr(),
            );
        });
    }
}
