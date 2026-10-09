//! Shared utilities for the Python bindings.

use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use tst_core::mpegts::mux::{
    AudioStreamHandle, DataStreamHandle, KlvStreamHandle, SubtitleStreamHandle, VideoStreamHandle,
};
use tst_core::transport::{Transport, TransportCancel};
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

// ---------------------------------------------------------------------------
// Shared cancel state + the two `Owned` helpers every class uses
// ---------------------------------------------------------------------------

/// The one cancel state a Python shell and every `CancelHandle` it hands
/// out share. Handed to `Owned::new` as the shell's
/// `Arc<dyn TransportCancel>` AND cloned into each Python `CancelHandle`,
/// so `close()` (which goes through `Owned::cancel`) and `handle.cancel()`
/// flip the same flag, and `is_cancelled()` is observable from any clone.
///
/// `inner` is always the transport's own handle — `SrtCancelHandle`,
/// `RtpCancelHandle`, `TcpCancelHandle`, `UdpCancelHandle`,
/// `RistCancelHandle` or a
/// `ManagedHandles.cancel`. For udp/rist the `cancelled` flag is also the
/// stop flag their polled `recv()` loop checks between slices.
///
/// `Owned` wraps whatever it is given in its own latching `OwnedCancel`, so
/// `Owned::cancel` → `CancelSource::cancel` → flag; a `CancelHandle` fires
/// `CancelSource` directly. Both paths therefore set THIS flag, which is the
/// one Python reads.
///
/// `is_cancelled()` reports THIS source's latch — "was this shell cancelled",
/// the question Python's `CancelHandle.is_cancelled()` asks. `CancelSource` is
/// a composing wrapper, not an alias of `inner`, so the two latches are
/// composed exactly once, by `Owned::is_cancelled`
/// (`self.cancel.cancelled || self.cancel.transport.is_cancelled()`). See the
/// "Handle aliases vs composing wrappers" section on `TransportCancel`.
pub(crate) struct CancelSource {
    inner: Arc<dyn TransportCancel + Send + Sync>,
    cancelled: AtomicBool,
}

/// How long [`fire_cancel_sources_at_exit`] waits for woken threads to
/// leave their Python frames. Long enough for libsrt's ~3-10 ms cancel
/// wake plus the GIL hand-off; short enough to be invisible at exit.
const EXIT_SETTLE_MS: u64 = 250;

/// Set by [`fire_cancel_sources_at_exit`] — Python's `atexit`, which runs
/// after every non-daemon thread was joined and before interpreter
/// finalisation. Read by [`allow_threads_parking`] and [`exiting`] on
/// worker threads: once set, a thread that would re-take the GIL parks
/// forever instead. CPython 3.12 `pthread_exit`s a thread re-acquiring
/// the GIL during finalisation; that forced unwind crosses PyO3's
/// `catch_unwind` trampoline and glibc aborts the process ("FATAL:
/// exception not rethrown", exit 134). Parking is what CPython 3.14 does
/// to daemon threads itself; `exit_group` reaps the parked thread.
static INTERPRETER_EXITING: AtomicBool = AtomicBool::new(false);
/// The thread that ran the exit hook (the main thread): it is finalising
/// the interpreter and must keep running.
static EXIT_HOOK_THREAD: OnceLock<std::thread::ThreadId> = OnceLock::new();

/// `true` once the exit hook has run on another thread. Byte-sink
/// callbacks check it before `Python::with_gil`.
#[allow(dead_code)] // byte-sink callers are srt/rtp-feature-gated.
pub(crate) fn exiting() -> bool {
    INTERPRETER_EXITING.load(Ordering::Acquire)
        && EXIT_HOOK_THREAD.get() != Some(&std::thread::current().id())
}

/// `py.allow_threads(f)` for a native call that can park (network I/O, a
/// wait, a join, or a lock another thread can hold across one). If the
/// interpreter began exiting while `f` ran, this thread parks forever
/// instead of re-taking the GIL — see [`INTERPRETER_EXITING`]. The value
/// `f` returned is never used on that path; the process reclaims it at
/// exit.
#[allow(dead_code)] // every transport surface calls this; dead only in a
// transport-less `--no-default-features` build.
pub(crate) fn allow_threads_parking<T, F>(py: Python<'_>, f: F) -> T
where
    // `Send`, not `pyo3::marker::Ungil`: without PyO3's `nightly` feature
    // `Ungil` is exactly a blanket over `Send`, and a closure capturing a
    // generic `F: Ungil` is not provably `Ungil` itself.
    F: Send + FnOnce() -> T,
    T: Send,
{
    py.allow_threads(move || {
        let r = f();
        if exiting() {
            // `park()` may return spuriously; never fall through.
            loop {
                std::thread::park();
            }
        }
        r
    })
}

/// Every live [`CancelSource`], weakly. Walked once at interpreter exit by
/// [`fire_cancel_sources_at_exit`] — see its doc for why.
static LIVE_CANCEL_SOURCES: Mutex<Vec<Weak<CancelSource>>> = Mutex::new(Vec::new());

impl CancelSource {
    pub(crate) fn new(inner: Arc<dyn TransportCancel + Send + Sync>) -> Arc<Self> {
        let me = Arc::new(Self {
            inner,
            cancelled: AtomicBool::new(false),
        });
        // Poison is RECOVERED, not skipped: the registry holds only
        // `Weak`s and every critical section below is a single `push` /
        // `retain`, so a panic cannot leave it half-updated. Bailing out
        // instead would stop registering shells after any unrelated panic
        // and reintroduce the exit hang this registry exists to prevent.
        let mut reg = LIVE_CANCEL_SOURCES
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Amortised cleanup: drop the dead weaks whenever the registry
        // doubles, so a long-lived process that opens and closes many
        // shells does not grow the vector without bound.
        if reg.len() == reg.capacity() {
            reg.retain(|w| w.strong_count() > 0);
        }
        reg.push(Arc::downgrade(&me));
        drop(reg);
        me
    }

    /// `true` once THIS source has been cancelled — its own latch only.
    ///
    /// Deliberately does NOT OR in `inner.is_cancelled()`: `Owned` already
    /// ORs the transport's latch for the shell's own reporting, and pushing
    /// it in here too would widen what the user-visible
    /// `tstrans.*.CancelHandle.is_cancelled()` answers with no caller asking
    /// for it.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// The trait-object view `Owned::new` takes. Only the transport
    /// surfaces construct an `Owned`, so this is cfg'd rather than
    /// `allow(dead_code)`d — an orphaned helper would still warn.
    #[cfg(any(
        feature = "srt",
        feature = "rtp",
        feature = "udp",
        feature = "tcp",
        feature = "rist"
    ))]
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
    fn is_cancelled(&self) -> bool {
        CancelSource::is_cancelled(self)
    }
}

/// `close()` for every converted class: `Owned::close` = cancel → lock
/// (recover) → take → `T::close`, outside the GIL. A transport whose own
/// close fails (`Listener::close` → `IoError`) is logged, not raised —
/// `close()` is documented infallible and idempotent; a panic inside the
/// inner close is re-raised as `PanicException`.
#[allow(dead_code)] // every transport surface calls this; dead only in a
// transport-less `--no-default-features` build.
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
    match allow_threads_parking(py, || owned.close()) {
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
/// `false` once it is empty. Never waits behind a parked call.
#[allow(dead_code)] // every transport surface calls this; dead only in a
// transport-less `--no-default-features` build.
pub(crate) fn alive_probe<T, S>(owned: &Owned<T, S>, alive: impl FnOnce(&T) -> bool) -> bool {
    match owned.try_with_ref(alive) {
        None => true,
        Some(Ok(b)) => b,
        Some(Err(_)) => false,
    }
}

/// The first configured stream handle of each kind — the `Owned` snapshot
/// of every `MuxSender`-shaped shell.
///
/// The stream set is fixed by the `MuxerProgramConfig` the sender is built
/// from (the muxer has no way to add a stream afterwards), so the handles
/// are read once, before the sender moves into its slot. The `*_handle()`
/// getters then never wait behind a `send_*` parked on a full send buffer
/// or in a Blocking reconnect.
#[allow(dead_code)] // dead only in a transport-less `--no-default-features` build.
#[derive(Clone, Copy)]
pub(crate) struct StreamHandles {
    pub video: Option<VideoStreamHandle>,
    pub klv: Option<KlvStreamHandle>,
    pub audio: Option<AudioStreamHandle>,
    pub subtitle: Option<SubtitleStreamHandle>,
    pub data: Option<DataStreamHandle>,
}

#[allow(dead_code)] // as `StreamHandles`.
impl StreamHandles {
    /// Read the handles off a freshly built sender.
    pub(crate) fn of<T: Transport>(sender: &tst_pipeline::MuxSender<T>) -> Self {
        Self {
            video: sender.video_handles().into_iter().next(),
            klv: sender.klv_handles().into_iter().next(),
            audio: sender.audio_handles().into_iter().next(),
            subtitle: sender.subtitle_handles().into_iter().next(),
            data: sender.data_handles().into_iter().next(),
        }
    }
}

/// The construction-time snapshot while the shell is open, `None` once it
/// is closed. Never takes the slot: `is_closed()` answers "open" without
/// waiting when a parked call holds it.
#[allow(dead_code)] // as `alive_probe`.
pub(crate) fn open_snapshot<T, S>(owned: &Owned<T, S>) -> Option<&S> {
    (!owned.is_closed()).then(|| owned.snapshot())
}

/// Cancel every still-live shell at interpreter exit.
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
///
/// **A clean exit costs nothing.** A shell that was `close()`d (or whose
/// handle was cancelled) has already latched its `CancelSource` — every
/// path into `Owned::close` fires `OwnedCancel::cancel` → `CancelSource`
/// first — so those sources are filtered out here. Only shells left OPEN
/// are fired, and only then is the settle window paid. Returns how many
/// sources it fired, which is the observable
/// `test_exit_with_parked_shell.py` asserts is 0 after a clean close;
/// `atexit` discards it. Note the shell object itself usually outlives
/// the walk (a module-level name keeps the `Arc` alive), so "still
/// registered" is NOT the same question as "still open" — the latch is.
#[pyfunction]
#[pyo3(name = "_fire_cancel_sources_at_exit")]
pub(crate) fn fire_cancel_sources_at_exit(py: Python<'_>) -> usize {
    // First: from here on a worker thread waking from a native call parks
    // instead of re-taking the GIL of an interpreter about to finalise.
    let _ = EXIT_HOOK_THREAD.set(std::thread::current().id());
    INTERPRETER_EXITING.store(true, Ordering::Release);
    // Poison recovered for the same reason as in `CancelSource::new`: a
    // hook that no-ops after an unrelated panic is exactly the hang this
    // exists to prevent, and `drain` on a `Vec<Weak<_>>` cannot observe a
    // torn state.
    let live: Vec<Arc<CancelSource>> = LIVE_CANCEL_SOURCES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
        .filter_map(|w| w.upgrade())
        .filter(|s| !s.is_cancelled())
        .collect();
    // The cancels themselves are native and may block briefly (libsrt's
    // `srt_close` on the paired socket), so drop the GIL for the walk.
    if live.is_empty() {
        return 0;
    }
    let fired = live.len();
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
    fired
}

/// Adapter so a `tst_core::cancel::CancelSlot` can be registered as a
/// [`CancelSource`] — the slot has inherent `cancel()` but no
/// `TransportCancel` impl.
///
/// This exists for ONE purpose: the first accept inside a listener-mode
/// constructor parks before any Python handle exists (it stays
/// uncancellable BY THE CALLER, and that is deliberate), so without this
/// the exit hook has nothing to fire and the process hangs. Wrapping the
/// slot for the duration of the accept keeps it user-uncancellable while
/// making it reachable at interpreter exit.
pub(crate) struct SlotCancel(pub Arc<tst_core::cancel::CancelSlot>);

impl TransportCancel for SlotCancel {
    fn cancel(&self) {
        self.0.cancel();
    }
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

/// Register `slot` for the duration of a blocking first accept. The
/// returned guard must stay alive across the accept: the exit registry
/// holds only a `Weak`, so dropping it de-registers the slot.
#[allow(dead_code)] // srt-feature-gated callers
pub(crate) fn register_accept_slot(slot: &Arc<tst_core::cancel::CancelSlot>) -> Arc<CancelSource> {
    CancelSource::new(Arc::new(SlotCancel(Arc::clone(slot))))
}
