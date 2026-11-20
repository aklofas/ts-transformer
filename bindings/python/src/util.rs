//! Shared utilities for the Python bindings.

use pyo3::intern;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use tst_core::transport::TransportCancel;
use tst_pipeline::binding::view::CancelLatch;
use tst_pipeline::binding::{BindingError, BindingErrorKind, Close, CloseFailure, Owned};

/// Coerce a Python bytes-like argument (`bytes`, `bytearray`, `memoryview`,
/// NumPy `uint8`) to an owned `Bound<'py, PyBytes>` strong reference.
///
/// Fast path for `bytes`: zero-copy clone of the bound reference.
/// Fallback: calls Python's `bytes()` built-in to coerce the argument,
/// which copies the data once into an immutable `bytes` object; it also
/// materializes an iterable of ints (`bytes([1, 2, 3])`).
///
/// An object whose `__index__` succeeds (an `int`, a `bool`, a NumPy
/// integer scalar or 0-d integer array) is refused with `TypeError`:
/// `bytes()` reads it as a length, `bytes(5)` being five zero bytes. This
/// runtime refusal is the guard: the stubs' `_BytesLike` includes `Any`
/// (a NumPy array has no stub type to name), so no type checker catches
/// an integer.
///
/// PyO3 cannot extract `&[u8]` from `bytearray` or `memoryview` — only
/// from `bytes`; `PyBuffer`, in the stable ABI from the 3.11 floor, is the
/// zero-copy alternative this helper predates. This helper bridges that gap by
/// accepting any bytes-like and returning a `PyBytes` whose `.as_bytes()`
/// borrow lives for as long as the returned value is on the stack. That
/// makes it safe to pass the resulting `&[u8]` across a subsequent
/// `py.detach()` call.
///
/// Raises `TypeError` for an integer and if `arg` cannot be passed to
/// `bytes()`.
pub(crate) fn coerce_bytes_like<'py>(
    py: Python<'py>,
    arg: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyBytes>> {
    if let Ok(b) = arg.cast::<PyBytes>() {
        return Ok(b.clone());
    }
    if reads_as_a_length(py, arg)? {
        return Err(pyo3::exceptions::PyTypeError::new_err(format!(
            "a bytes-like object is required, not {} (an integer would be read as \
             that many zero bytes)",
            arg.get_type().name()?
        )));
    }
    py.import("builtins")?
        .getattr(intern!(py, "bytes"))?
        .call1((arg,))?
        .cast_into::<PyBytes>()
        .map_err(|e| e.into())
}

/// Whether `bytes(arg)` would take `arg` as a length: its `__index__`
/// succeeds (CPython's `bytes()` tries it before the buffer protocol, so a
/// NumPy integer scalar, which has a buffer too, is a length).
fn reads_as_a_length(py: Python<'_>, arg: &Bound<'_, PyAny>) -> PyResult<bool> {
    if arg.is_instance_of::<pyo3::types::PyInt>() {
        return Ok(true);
    }
    if !arg.hasattr(intern!(py, "__index__"))? {
        return Ok(false);
    }
    match py
        .import("operator")?
        .getattr(intern!(py, "index"))?
        .call1((arg,))
    {
        Ok(_) => Ok(true),
        // A NumPy array that is not a 0-d integer array has `__index__`
        // and refuses it: `bytes()` then reads its buffer.
        Err(e) if e.is_instance_of::<pyo3::exceptions::PyTypeError>(py) => Ok(false),
        Err(e) => Err(e),
    }
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
    latch: CancelLatch,
}

/// How long [`fire_cancel_sources_at_exit`] waits for woken threads to
/// leave their Python frames. Long enough for libsrt's ~3-10 ms cancel
/// wake plus the GIL hand-off; short enough to be invisible at exit.
const EXIT_SETTLE_MS: u64 = 250;

/// Set by [`fire_cancel_sources_at_exit`] — Python's `atexit`, which runs
/// after every non-daemon thread was joined and before interpreter
/// finalisation. Read by [`allow_threads_parking`] and [`exiting`] on
/// worker threads: once set, a thread that would re-take the GIL parks
/// instead (bounded by [`EXIT_PARK_MAX`]). CPython 3.12 `pthread_exit`s a thread re-acquiring
/// the GIL during finalisation; that forced unwind crosses PyO3's
/// `catch_unwind` trampoline and glibc aborts the process ("FATAL:
/// exception not rethrown", exit 134). Parking is what CPython 3.14 does
/// to daemon threads itself; `exit_group` reaps the parked thread.
static INTERPRETER_EXITING: AtomicBool = AtomicBool::new(false);
/// The thread that ran the exit hook (the main thread): it is finalising
/// the interpreter and must keep running.
static EXIT_HOOK_THREAD: OnceLock<std::thread::ThreadId> = OnceLock::new();

/// `true` once the exit hook has run on another thread. Read by
/// [`allow_threads_parking`].
#[allow(dead_code)] // dead only in a transport-less build, like its caller
pub(crate) fn exiting() -> bool {
    INTERPRETER_EXITING.load(Ordering::Acquire)
        && EXIT_HOOK_THREAD.get() != Some(&std::thread::current().id())
}

/// How long [`allow_threads_parking`] holds a thread whose native call
/// returned after the exit hook ran, before letting it re-take the GIL.
///
/// - If finalisation already began, the process is gone long before this
///   (`exit_group` reaps the parked thread), so the abort race is closed in
///   practice.
/// - If the process is still alive after it, something is waiting for this
///   worker — an `atexit` handler registered before `import tstrans` (it
///   runs after the hook) joining it — so the thread returns, the call
///   raises as any cancelled call does, and the join completes. Re-taking
///   the GIL is safe then: `atexit` handlers are still running.
/// - The residual window is a finalisation still in progress at exactly the
///   deadline: milliseconds against seconds.
const EXIT_PARK_MAX: std::time::Duration = std::time::Duration::from_secs(3);

/// `py.detach(f)` for a native call that can park (network I/O, a
/// wait, a join, or a lock another thread can hold across one). If the
/// interpreter began exiting while `f` ran, this thread parks for up to
/// [`EXIT_PARK_MAX`] instead of re-taking the GIL at once — see
/// [`INTERPRETER_EXITING`]; normally the process ends during that park.
/// Only if it is still alive afterwards does the thread return `f`'s value.
// every transport surface calls this; dead only in a
// transport-less `--no-default-features` build.
#[allow(dead_code)]
#[expect(clippy::disallowed_methods, reason = "the parking wrapper itself")]
pub(crate) fn allow_threads_parking<T, F>(py: Python<'_>, f: F) -> T
where
    // `Send`, not `pyo3::marker::Ungil`: without PyO3's `nightly` feature
    // `Ungil` is exactly a blanket over `Send`, and a closure capturing a
    // generic `F: Ungil` is not provably `Ungil` itself.
    F: Send + FnOnce() -> T,
    T: Send,
{
    py.detach(move || {
        let r = f();
        if exiting() {
            // `park_timeout` may return early (spuriously or on an
            // `unpark`); keep parking until the deadline itself.
            let deadline = std::time::Instant::now() + EXIT_PARK_MAX;
            loop {
                let now = std::time::Instant::now();
                if now >= deadline {
                    break;
                }
                std::thread::park_timeout(deadline - now);
            }
        }
        r
    })
}

/// Every live [`CancelSource`], weakly. Walked once at interpreter exit by
/// [`fire_cancel_sources_at_exit`] — see its doc for why.
static LIVE_CANCEL_SOURCES: Mutex<Vec<Weak<CancelSource>>> = Mutex::new(Vec::new());

impl CancelSource {
    /// Register a source for a shell. Every caller holds the GIL: shells
    /// build their source in the method body after the parking open
    /// returned.
    pub(crate) fn new(inner: Arc<dyn TransportCancel>) -> Arc<Self> {
        let me = Self::registered(inner);
        if INTERPRETER_EXITING.load(Ordering::Acquire) {
            // Born after the hook's drain (see `registered`). The cancel can
            // reach native teardown (libsrt's `srt_close`), so release the
            // GIL for it as the hook's own walk does. `attach` is re-entrant
            // on a thread that already holds the GIL and acquires nothing.
            Python::attach(|py| allow_threads_parking(py, || me.latch.cancel()));
        }
        me
    }

    /// Add a source to the exit registry. A caller that reads
    /// `INTERPRETER_EXITING` as set after this returns must cancel the
    /// source itself.
    ///
    /// The exit hook drains the registry once. It stores the flag before it
    /// locks and drains, so a push that lands after the drain sees the flag
    /// afterwards, and a push before it is fired by the hook's walk. A
    /// source caught by both is cancelled twice, which is a no-op.
    fn registered(inner: Arc<dyn TransportCancel>) -> Arc<Self> {
        let me = Arc::new(Self {
            latch: CancelLatch::new(inner),
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
        self.latch.is_cancelled()
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
        // Latch first, then the inner cancel: `CancelLatch` orders them.
        self.latch.cancel();
    }
    fn is_cancelled(&self) -> bool {
        CancelSource::is_cancelled(self)
    }
}

/// `close()` for every class holding an `Owned`: `Owned::close` = cancel → lock
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
    // `CloseFailure<T::Error>` is carried back across `detach`.
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
            BindingError::new(BindingErrorKind::PanicCaught, detail),
        )),
    }
}

/// The implicit drop of every class holding an `Owned`: the same
/// cancel-first close as `close()`. `Owned`'s own `Drop` already cancels a
/// slot that still holds a value, which keeps a `Sender` flushing into a
/// dead peer's reconnect loop from stalling, but the value then closes and
/// drops with the GIL held. Routing the drop through `close_owned` runs that
/// close with the GIL released, under the same exit park as `close()`.
/// `close_owned` takes the slot, so `Owned`'s own `Drop` then finds it empty
/// and fires nothing.
///
/// `tp_dealloc` already holds the GIL, so `attach` hands back the current
/// token. A `PanicException` from the inner close is dropped: there is no
/// caller to raise it to.
#[allow(dead_code)] // dead only in a transport-less build, as `close_owned`.
pub(crate) fn drop_owned<T, S>(d: &crate::raise::Domain, owned: &Owned<T, S>)
where
    T: Close + Send,
    T::Error: Send,
    S: Send + Sync,
{
    Python::attach(|py| {
        let _ = close_owned(py, d, owned);
    });
}

/// The native teardown of a class's implicit drop: `tp_dealloc` holds the
/// GIL, so `attach` hands back the current token and the closure runs
/// detached, under the same exit park as `close()`. Nothing is raised:
/// there is no caller.
#[allow(dead_code)] // dead only in a transport-less build, as `close_owned`.
pub(crate) fn drop_blocking<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    Python::attach(|py| allow_threads_parking(py, f))
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
#[expect(
    clippy::disallowed_methods,
    reason = "the exit hook: cancelling only sets flags, then a bounded settle window"
)]
pub(crate) fn fire_cancel_sources_at_exit(py: Python<'_>) -> usize {
    // First: from here on a worker thread waking from a native call parks
    // instead of re-taking the GIL of an interpreter about to finalise.
    let _ = EXIT_HOOK_THREAD.set(std::thread::current().id());
    INTERPRETER_EXITING.store(true, Ordering::Release);
    // Poison recovered for the same reason as in `CancelSource::registered`: a
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
    py.detach(move || {
        for src in live {
            TransportCancel::cancel(&*src);
        }
        // Settle window. Cancelling only WAKES the parked call. A woken
        // thread does not re-take the GIL at once: the exit flag is already
        // set, so `allow_threads_parking` parks it for up to
        // `EXIT_PARK_MAX` (normally the process ends during that park).
        // This window covers threads that were between their native return
        // and `exiting()` when the flag flipped: they re-take the GIL, and
        // holding here with the GIL released lets them raise and leave
        // their Python frames while the interpreter is still alive, instead
        // of finalisation aborting them (exit 134). Bounded and paid once,
        // at exit, only when a shell was actually left open.
        std::thread::sleep(std::time::Duration::from_millis(EXIT_SETTLE_MS));
    });
    fired
}

/// Adapter so a `tst_core::cancel::CancelSlot` can be registered as a
/// [`CancelSource`] — the slot has inherent `cancel()` but no
/// `TransportCancel` impl.
///
/// This exists for ONE purpose: the first accept inside a listener-mode
/// constructor parks before any Python handle exists (only an
/// `OpenCancel` the caller passed can reach it, and most callers pass
/// none), so without this the exit hook has nothing to fire and the
/// process hangs. Wrapping the slot for the duration of the accept makes
/// it reachable at interpreter exit whether or not an `OpenCancel` was
/// given.
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
    let me = CancelSource::registered(Arc::new(SlotCancel(Arc::clone(slot))));
    if INTERPRETER_EXITING.load(Ordering::Acquire) {
        // Born after the hook's drain. This runs inside a parking closure
        // without the GIL, and must not re-acquire it at exit; a slot cancel
        // only sets a flag and wakes the accept, so it runs in place.
        me.latch.cancel();
    }
    me
}

/// Bytes a generated native class holds. A getter on the class hands out a
/// Python `bytes`. Under PyO3 0.23 and later a plain `Vec<u8>` field would
/// convert to `bytes` too; the newtype stays because the binding
/// generator's type map names it and it builds the `PyBytes` explicitly,
/// so the getter's Python type does not rest on PyO3's `u8` sequence
/// specialisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostBytes(pub Vec<u8>);

impl<'py> IntoPyObject<'py> for &HostBytes {
    type Target = PyBytes;
    type Output = Bound<'py, PyBytes>;
    type Error = std::convert::Infallible;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        Ok(PyBytes::new(py, &self.0))
    }
}

impl<'py> IntoPyObject<'py> for HostBytes {
    type Target = PyBytes;
    type Output = Bound<'py, PyBytes>;
    type Error = std::convert::Infallible;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        (&self).into_pyobject(py)
    }
}

/// `tst_core::url::redact_url` for the pure-Python classes whose repr
/// formats a URL (`tstrans.rtp.RtspServerConfig`). Private to the package.
#[pyfunction]
#[pyo3(name = "_redact_url")]
pub(crate) fn redact_url_py(url: &str) -> String {
    tst_core::url::redact_url(url)
}

/// A socket address as Python's `(host, port)`: the IP as text, unbracketed,
/// a 2-tuple for IPv6 too; the shape `tstrans.srt.Socket.peer_addr()` returns.
/// A scoped IPv6 host carries `%<scope id>` (the form `ipaddress` parses and
/// `socket.sendto` accepts); an IPv4-mapped IPv6 address is reported as the
/// IPv4 address, as the source records report one.
pub(crate) type PyAddr = (String, u16);

/// `addr` as a [`PyAddr`].
pub(crate) fn addr_tuple(addr: std::net::SocketAddr) -> PyAddr {
    match addr {
        std::net::SocketAddr::V4(v4) => (v4.ip().to_string(), v4.port()),
        std::net::SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => (v4.to_string(), v6.port()),
            None if v6.scope_id() != 0 => (format!("{}%{}", v6.ip(), v6.scope_id()), v6.port()),
            None => (v6.ip().to_string(), v6.port()),
        },
    }
}

/// `_addr_tuple_for_test(host, port, scope_id)`: [`addr_tuple`] of the
/// address built from an IP literal, a port and an IPv6 scope id, so the
/// tests reach the scoped form without a link-local peer.
#[cfg(feature = "py-test-hooks")]
#[pyfunction]
#[pyo3(name = "_addr_tuple_for_test")]
pub(crate) fn addr_tuple_for_test(host: &str, port: u16, scope_id: u32) -> PyResult<PyAddr> {
    use std::net::{IpAddr, SocketAddr, SocketAddrV6};
    let ip: IpAddr = host.parse().map_err(|_| {
        pyo3::exceptions::PyValueError::new_err(format!("not an IP literal: {host:?}"))
    })?;
    let addr = match ip {
        IpAddr::V4(v4) if scope_id == 0 => SocketAddr::from((v4, port)),
        IpAddr::V4(_) => {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "an IPv4 address has no scope id",
            ));
        }
        IpAddr::V6(v6) => SocketAddr::V6(SocketAddrV6::new(v6, port, 0, scope_id)),
    };
    Ok(addr_tuple(addr))
}

/// Signed microseconds from the Unix epoch to `t` (negative for a clock set
/// before 1970), truncated toward the epoch.
fn unix_micros(t: std::time::SystemTime) -> i128 {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_micros() as i128,
        Err(e) => -(e.duration().as_micros() as i128),
    }
}

/// `t` as a UTC-aware `datetime.datetime`: the epoch plus a `timedelta` of
/// whole microseconds, so no float rounding moves the instant.
pub(crate) fn utc_datetime(py: Python<'_>, t: std::time::SystemTime) -> PyResult<Py<PyAny>> {
    let dt = py.import(intern!(py, "datetime"))?;
    let utc = dt
        .getattr(intern!(py, "timezone"))?
        .getattr(intern!(py, "utc"))?;
    let epoch = dt
        .getattr(intern!(py, "datetime"))?
        .call((1970, 1, 1, 0, 0, 0, 0, utc), None)?;
    let kwargs = pyo3::types::PyDict::new(py);
    kwargs.set_item(intern!(py, "microseconds"), unix_micros(t))?;
    let delta = dt
        .getattr(intern!(py, "timedelta"))?
        .call((), Some(&kwargs))?;
    Ok(epoch.add(delta)?.unbind())
}

/// The last delivering sender of a transport's source record, if it keeps one.
pub(crate) fn peer_addr_tuple(
    source: Option<&tst_core::transport::SourceHandle>,
) -> Option<PyAddr> {
    source.and_then(|s| s.peer_addr()).map(addr_tuple)
}
