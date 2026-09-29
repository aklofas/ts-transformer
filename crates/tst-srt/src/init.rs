//! Crate-private libsrt initialization.
//!
//! `srt_startup()` must be called once before any libsrt API use. Lazy via
//! `OnceLock` from any constructor that needs libsrt (`Socket::connect_with`,
//! `Listener::bind_with`, etc.).
//!
//! `srt_cleanup()` is never tied to value drops — see the design doc for
//! rationale (drop-order ambiguity vs. negligible OS-reclaimed leaks) — but
//! it IS registered via `atexit` to run at process exit; see
//! `register_exit_cleanup` below.
//!
//! The exit handler does one thing before `srt_cleanup()`: it closes every
//! socket that is still open and waits, bounded, for the threads parked in
//! libsrt to return (`crate::exit_guard`). Without that, a thread left in
//! `accept()` makes `srt_cleanup()` — and with it process exit — hang.
//! Closing and joining before exit remains the recommended shape; the guard
//! is what keeps a program that did not from hanging.

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
/// [`crate::exit_guard::release_parked_calls`]. With nothing open and no
/// thread in a call that costs one uncontended lock and two atomic loads.
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

fn install_log_handler() {
    use std::ffi::{CStr, c_char, c_int, c_void};

    extern "C" fn forward(
        _opaque: *mut c_void,
        level: c_int,
        _file: *const c_char,
        _line: c_int,
        _area: *const c_char,
        message: *const c_char,
    ) {
        if message.is_null() {
            return;
        }
        let msg = unsafe { CStr::from_ptr(message) }.to_string_lossy();
        // libsrt log levels (LOG_DEBUG=7, INFO=6, NOTICE=5, WARNING=4,
        // ERROR=3, CRITICAL=2, ALERT=1, EMERGENCY=0). Map roughly to
        // tracing levels. `tracing::event!` requires a const-known level,
        // so dispatch via the per-level macros rather than a runtime value.
        match level {
            7 => tracing::trace!(target: "srt", "{}", msg),
            6 => tracing::debug!(target: "srt", "{}", msg),
            5 => tracing::info!(target: "srt", "{}", msg),
            4 => tracing::warn!(target: "srt", "{}", msg),
            _ => tracing::error!(target: "srt", "{}", msg),
        }
    }

    unsafe {
        srt_sys::srt_setloghandler(std::ptr::null_mut(), Some(forward));
    }
}
