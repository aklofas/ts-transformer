//! `CancelHandle`, the value class every shell hands out, plus the helper that
//! turns a transport's cancel handle into the shell's shared [`CancelSource`].

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;

use pyo3::prelude::*;

use tst_core::transport::TransportCancel;

use crate::util::CancelSource;

/// Python-side cancel handle. Wraps the shell's shared
/// [`CancelSource`]: every clone obtained from the same shell, and the
/// shell's own `close()`, forwards into one cancel and flips one flag, so
/// `is_cancelled()` reports the shell's state, not this wrapper's history.
#[pyclass(frozen, name = "CancelHandle", module = "tstrans.shells")]
pub(crate) struct PyCancelHandle {
    src: Arc<CancelSource>,
    /// What `cancel()` fires. For a shell this is the `Owned`'s own handle,
    /// which latches the data-plane gate and then fires `src`; otherwise
    /// `src` itself.
    fire: Option<Arc<dyn TransportCancel>>,
}

#[pymethods]
impl PyCancelHandle {
    /// Signal cancellation. Idempotent. Wakes a thread parked in
    /// `send_bytes` / `recv_bytes` / `__next__`; that call raises the
    /// transport's `CLOSED` kind.
    ///
    /// The transport's cancel can wait (libsrt's close waits out the
    /// socket's linger), so it runs without the GIL.
    fn cancel(&self, py: Python<'_>) {
        crate::util::allow_threads_parking(py, || match &self.fire {
            Some(fire) => fire.cancel(),
            None => TransportCancel::cancel(&*self.src),
        });
    }

    /// `True` once the shell was cancelled or closed through ANY handle
    /// or its own `close()` (shared state).
    fn is_cancelled(&self) -> bool {
        self.src.is_cancelled()
    }

    fn __repr__(&self) -> String {
        format!("CancelHandle(cancelled={})", self.is_cancelled())
    }
}

impl PyCancelHandle {
    /// Clone the shell's shared cancel state so this handle observes, and
    /// contributes to, the same cancel.
    pub(crate) fn from_source(src: &Arc<CancelSource>) -> Self {
        Self {
            src: Arc::clone(src),
            fire: None,
        }
    }

    /// A shell's handle: `cancel()` fires `owned_cancel` (the shell's
    /// `Owned::cancel_arc`), whose latch the data-plane entries refuse on,
    /// and which in turn fires `src`. `is_cancelled()` reads `src`, as
    /// [`Self::from_source`] does.
    pub(crate) fn for_shell(
        src: &Arc<CancelSource>,
        owned_cancel: Arc<dyn TransportCancel>,
    ) -> Self {
        Self {
            src: Arc::clone(src),
            fire: Some(owned_cancel),
        }
    }
}

/// The shell's shared cancel state over the transport's own cancel handle
/// (taken from `Opened` before the transport was boxed into the shell).
pub(crate) fn cancel_source(cancel: &Arc<dyn TransportCancel>) -> Arc<CancelSource> {
    CancelSource::new(Arc::clone(cancel))
}

/// A transport cancel that takes `delay_ms` to return, like `srt_close`
/// waiting out a linger. Test-only.
#[cfg(feature = "py-test-hooks")]
struct SlowCancel {
    delay: std::time::Duration,
    latched: std::sync::atomic::AtomicBool,
}

#[cfg(feature = "py-test-hooks")]
impl TransportCancel for SlowCancel {
    fn cancel(&self) {
        std::thread::sleep(self.delay);
        self.latched
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    fn is_cancelled(&self) -> bool {
        self.latched.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// `_slow_cancel_handle_for_test(delay_ms)` -> a `CancelHandle` whose
/// `cancel()` blocks for `delay_ms` before latching.
#[cfg(feature = "py-test-hooks")]
#[pyfunction]
#[pyo3(name = "_slow_cancel_handle_for_test")]
pub(crate) fn slow_cancel_handle_for_test(delay_ms: u64) -> PyCancelHandle {
    let inner: Arc<dyn TransportCancel> = Arc::new(SlowCancel {
        delay: std::time::Duration::from_millis(delay_ms),
        latched: std::sync::atomic::AtomicBool::new(false),
    });
    PyCancelHandle::from_source(&cancel_source(&inner))
}

/// A class whose implicit drop takes `delay`, like a native teardown
/// waiting out a linger or a join. Test-only.
#[cfg(feature = "py-test-hooks")]
#[pyclass(name = "_SlowDropForTest", module = "tstrans.shells")]
pub(crate) struct PySlowDropForTest {
    delay: std::time::Duration,
}

#[cfg(feature = "py-test-hooks")]
impl Drop for PySlowDropForTest {
    fn drop(&mut self) {
        let delay = self.delay;
        crate::util::drop_blocking(|| std::thread::sleep(delay));
    }
}

/// `_slow_drop_for_test(delay_ms)` -> an object whose implicit drop
/// blocks for `delay_ms`.
#[cfg(feature = "py-test-hooks")]
#[pyfunction]
#[pyo3(name = "_slow_drop_for_test")]
pub(crate) fn slow_drop_for_test(delay_ms: u64) -> PySlowDropForTest {
    PySlowDropForTest {
        delay: std::time::Duration::from_millis(delay_ms),
    }
}
