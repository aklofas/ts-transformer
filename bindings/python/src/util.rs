//! Shared utilities for the Python bindings.

use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError, Weak};

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

/// Every live [`CancelSource`], weakly. Walked once at interpreter exit by
/// [`fire_cancel_sources_at_exit`] — see its doc for why.
/// How long [`fire_cancel_sources_at_exit`] waits for woken threads to
/// leave their Python frames. Long enough for libsrt's ~3-10 ms cancel
/// wake plus the GIL hand-off; short enough to be invisible at exit.
const EXIT_SETTLE_MS: u64 = 250;

static LIVE_CANCEL_SOURCES: Mutex<Vec<Weak<CancelSource>>> = Mutex::new(Vec::new());

impl CancelSource {
    pub(crate) fn new(inner: Arc<dyn TransportCancel + Send + Sync>) -> Arc<Self> {
        let me = Arc::new(Self {
            inner,
            cancelled: AtomicBool::new(false),
        });
        if let Ok(mut reg) = LIVE_CANCEL_SOURCES.lock() {
            // Amortised cleanup: drop the dead weaks whenever the registry
            // doubles, so a long-lived process that opens and closes many
            // shells does not grow the vector without bound.
            if reg.len() == reg.capacity() {
                reg.retain(|w| w.strong_count() > 0);
            }
            reg.push(Arc::downgrade(&me));
        }
        me
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

/// Cancel every still-live shell at interpreter exit (Arc 2 rider R-EXIT).
///
/// A thread parked inside libsrt when the process exits deadlocks teardown:
/// `tst_srt` registers `srt_cleanup` with C `atexit`, and `srt_cleanup`
/// joins libsrt's `SRT:GC` thread, which cannot finish while a socket is
/// still parked in `accept()` / `recv()`. Python's `atexit` callbacks run
/// during interpreter finalisation — strictly before the C-level handlers —
/// so firing every live cancel here unparks those calls in time for
/// `srt_cleanup` to join. Without it a script that simply forgets to
/// `close()` a receiver hangs forever at exit instead of terminating.
///
/// Registered once from `_native`'s init (`lib.rs`). Idempotent and
/// best-effort: a poisoned registry or an already-closed shell is skipped,
/// and cancelling an already-cancelled source is a no-op by the
/// `TransportCancel` contract.
#[pyfunction]
#[pyo3(name = "_fire_cancel_sources_at_exit")]
pub(crate) fn fire_cancel_sources_at_exit(py: Python<'_>) {
    let live: Vec<Arc<CancelSource>> = match LIVE_CANCEL_SOURCES.lock() {
        Ok(mut reg) => reg.drain(..).filter_map(|w| w.upgrade()).collect(),
        Err(_) => return,
    };
    // The cancels themselves are native and may block briefly (libsrt's
    // `srt_close` on the paired socket), so drop the GIL for the walk.
    if live.is_empty() {
        return;
    }
    py.allow_threads(move || {
        for src in live {
            TransportCancel::cancel(&*src);
        }
        // Settle window. Cancelling only WAKES the parked call; the thread
        // then needs the GIL to raise its exception and leave its Python
        // frame. If interpreter finalisation gets there first, CPython
        // aborts that thread (exit 134) instead of hanging (exit 124) —
        // both are bad. Holding here with the GIL released lets those
        // threads finish while the interpreter is still alive. Bounded and
        // paid once, at exit, only when a shell was actually left open.
        std::thread::sleep(std::time::Duration::from_millis(EXIT_SETTLE_MS));
    });
}

/// Adapter so a `tst_core::cancel::CancelSlot` can be registered as a
/// [`CancelSource`] — the slot has inherent `cancel()` but no
/// `TransportCancel` impl.
///
/// This exists for ONE purpose: the first accept inside a listener-mode
/// constructor parks before any Python handle exists (DEBT-16 — it stays
/// uncancellable BY THE CALLER, and that is deliberate), so without this
/// the exit hook has nothing to fire and the process hangs. Wrapping the
/// slot for the duration of the accept keeps it user-uncancellable while
/// making it reachable at interpreter exit.
pub(crate) struct SlotCancel(pub Arc<tst_core::cancel::CancelSlot>);

impl TransportCancel for SlotCancel {
    fn cancel(&self) {
        self.0.cancel();
    }
}

/// Register `slot` for the duration of a blocking first accept. The
/// returned guard must stay alive across the accept: the exit registry
/// holds only a `Weak`, so dropping it de-registers the slot.
#[allow(dead_code)] // srt-feature-gated callers
pub(crate) fn register_accept_slot(slot: &Arc<tst_core::cancel::CancelSlot>) -> Arc<CancelSource> {
    CancelSource::new(Arc::new(SlotCancel(Arc::clone(slot))))
}
