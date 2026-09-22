//! Shared utilities for the Python bindings.

use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use tst_core::transport::TransportCancel;
use tst_pipeline::binding::{BindingError, BindingErrorKind, Close, CloseFailure, Owned};

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

// ---------------------------------------------------------------------------
// Arc 2 WP-B2 — shared cancel state + the two `Owned` helpers every class uses
// ---------------------------------------------------------------------------

/// The one cancel state a Python shell and every `CancelHandle` it hands
/// out share (Arc 2 WP-B2). Handed to `Owned::new` as the shell's
/// `Arc<dyn TransportCancel>` AND cloned into each Python `CancelHandle`,
/// so `close()` (which goes through `Owned::cancel`) and `handle.cancel()`
/// flip the same flag, and `is_cancelled()` is observable from any clone.
///
/// `inner` is the transport's own handle (`SrtCancelHandle`,
/// `RtpCancelHandle`, `TcpCancelHandle`, a `ManagedHandles.cancel`) or
/// `tst_pipeline::binding::FlagCancel` for udp/rist until WP-D gives them
/// one; for those two the `cancelled` flag is also the stop flag their
/// polled `recv()` loop checks (the former per-class `stop: Arc<AtomicBool>`
/// fields are deleted).
///
/// `Owned` wraps whatever it is given in its own latching `OwnedCancel`, so
/// `Owned::cancel` → `CancelSource::cancel` → flag; a `CancelHandle` fires
/// `CancelSource` directly. Both paths therefore set THIS flag, which is the
/// one Python reads. WP-C1 follow-up: once `TransportCancel` has
/// `is_cancelled`, `is_cancelled()` also ORs in `inner.is_cancelled()`.
pub(crate) struct CancelSource {
    inner: Arc<dyn TransportCancel + Send + Sync>,
    cancelled: AtomicBool,
}

impl CancelSource {
    pub(crate) fn new(inner: Arc<dyn TransportCancel + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            cancelled: AtomicBool::new(false),
        })
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// The trait-object view `Owned::new` takes.
    pub(crate) fn as_dyn(self: &Arc<Self>) -> Arc<dyn TransportCancel + Send + Sync> {
        Arc::clone(self) as Arc<dyn TransportCancel + Send + Sync>
    }
}

impl TransportCancel for CancelSource {
    fn cancel(&self) {
        // Flag first so a poll loop that wakes because of the forwarded
        // cancel already sees the state.
        self.cancelled.store(true, Ordering::Release);
        self.inner.cancel();
    }
}

/// `close()` for every converted class: `Owned::close` = cancel → lock
/// (recover) → take → `T::close`, outside the GIL. A transport whose own
/// close fails (`Listener::close` → `IoError`) is logged, not raised —
/// `close()` is documented infallible and idempotent; a panic inside the
/// inner close is re-raised as `PanicException` exactly as before Arc 2.
#[allow(dead_code)] // transport-feature-gated callers; unused in minimal builds
pub(crate) fn close_owned<T, S>(
    py: Python<'_>,
    d: &crate::raise::Domain,
    owned: &Owned<T, S>,
) -> PyResult<()>
where
    T: Close + Send,
    // `CloseFailure<T::Error>` is carried back across `allow_threads`.
    T::Error: Send,
    S: Send + Sync,
{
    match py.allow_threads(|| owned.close()) {
        Ok(()) => Ok(()),
        Err(CloseFailure::Inner(e)) => {
            tracing::warn!(error = %e, "close() failed on the underlying transport; handle released");
            Ok(())
        }
        Err(CloseFailure::Panicked { detail }) => Err(crate::raise::raise(
            py,
            d,
            BindingError {
                kind: BindingErrorKind::PanicCaught,
                detail,
            },
        )),
    }
}

/// Non-blocking liveness: `alive(&T)` when the slot can be inspected now,
/// `true` while another thread holds it (a parked call means open),
/// `false` once it is empty. Never waits behind a parked call (the
/// PR #234 class).
#[allow(dead_code)] // transport-feature-gated callers; unused in minimal builds
pub(crate) fn alive_probe<T, S>(owned: &Owned<T, S>, alive: impl FnOnce(&T) -> bool) -> bool {
    match owned.try_with_ref(alive) {
        None => true,
        Some(Ok(b)) => b,
        Some(Err(_)) => false,
    }
}
