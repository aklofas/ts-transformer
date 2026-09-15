//! Shared utilities for the Python bindings.

use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::sync::{Arc, Mutex, TryLockError};

/// Coerce a Python bytes-like argument (`bytes`, `bytearray`, `memoryview`,
/// NumPy `uint8`) to an owned `Bound<'py, PyBytes>` strong reference.
///
/// Fast path for `bytes`: zero-copy clone of the bound reference.
/// Fallback: calls Python's `bytes()` built-in to coerce the argument,
/// which copies the data once into an immutable `bytes` object.
///
/// The `bytes()` fallback accepts strictly more than the stubs'
/// `_BytesLike` promise: an `int` yields that many zero bytes and any
/// iterable of ints is materialized (`bytes(5)`, `bytes([1, 2, 3])`).
/// That widening is deliberate — every transport sender has coerced this
/// way since the pattern was introduced, and narrowing here would make
/// `Muxer.push_*` reject inputs its wrapping senders accept. The stubs'
/// `_BytesLike` annotation is the static-analysis guard against misuse.
///
/// PyO3 0.22 abi3-py310 cannot extract `&[u8]` from `bytearray` or
/// `memoryview` — only from `bytes`. This helper bridges that gap by
/// accepting any bytes-like and returning a `PyBytes` whose `.as_bytes()`
/// borrow lives for as long as the returned value is on the stack. That
/// makes it safe to pass the resulting `&[u8]` across a subsequent
/// `py.allow_threads()` call.
///
/// Raises `TypeError` if `arg` cannot be passed to `bytes()`.
pub(crate) fn coerce_bytes_like<'py>(
    py: Python<'py>,
    arg: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyBytes>> {
    if let Ok(b) = arg.downcast::<PyBytes>() {
        return Ok(b.clone());
    }
    py.import_bound("builtins")?
        .getattr(intern!(py, "bytes"))?
        .call1((arg,))?
        .downcast_into::<PyBytes>()
        .map_err(|e| e.into())
}

/// Format a `host:port` string, bracketing IPv6 literals so the result
/// parses through `SocketAddr` / `ToSocketAddrs`.
///
/// `host` must be the plain hostname or IP literal (without brackets or
/// port). The function adds `[…]` iff `host` contains a colon and does
/// not already start with `[`.
#[allow(dead_code)] // transport-feature-gated callers; unused in minimal builds
pub(crate) fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Run `f` against the value held in a shared slot with the GIL released,
/// holding the slot's lock for the whole call. `None` when the slot is
/// empty (the wrapper was closed). A `close()` on another thread fires the
/// wrapper's cancel BEFORE taking this lock, so a parked call returns
/// promptly and releases the slot.
///
/// Poison is recovered with `into_inner`: the slot only ever holds an
/// `Option`, which a panic cannot leave half-updated (same policy as
/// `srt::Receiver`).
#[allow(dead_code)] // transport-feature-gated callers; unused in minimal builds
pub(crate) fn with_slot<T, R>(
    py: Python<'_>,
    slot: &Arc<Mutex<Option<T>>>,
    f: impl FnOnce(&mut T) -> R + Send,
) -> Option<R>
where
    T: Send,
    R: Send,
{
    let slot = Arc::clone(slot);
    py.allow_threads(move || {
        let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_mut().map(f)
    })
}

/// Take the value out of a shared slot (GIL released while waiting for a
/// parked call to release it) and run `close` on it OUTSIDE the lock — an
/// inner's close may block (libsrt lingers) and must never hold the slot
/// while it does. No-op when the slot is already empty. Callers fire the
/// wrapper's cancel handle before calling this.
#[allow(dead_code)] // transport-feature-gated callers; unused in minimal builds
pub(crate) fn close_slot<T: Send>(
    py: Python<'_>,
    slot: &Arc<Mutex<Option<T>>>,
    close: impl FnOnce(T) + Send,
) {
    let slot = Arc::clone(slot);
    py.allow_threads(move || {
        let taken = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(t) = taken {
            close(t);
        }
    })
}

/// Non-blocking liveness read of a shared slot: `alive(&T)` when the slot
/// can be inspected, `true` while another thread holds it (a parked call
/// means the wrapper is still open), `false` once it is empty.
#[allow(dead_code)] // transport-feature-gated callers; unused in minimal builds
pub(crate) fn slot_alive<T>(slot: &Arc<Mutex<Option<T>>>, alive: impl FnOnce(&T) -> bool) -> bool {
    match slot.try_lock() {
        Ok(g) => g.as_ref().is_some_and(alive),
        Err(TryLockError::Poisoned(p)) => p.into_inner().as_ref().is_some_and(alive),
        Err(TryLockError::WouldBlock) => true,
    }
}
