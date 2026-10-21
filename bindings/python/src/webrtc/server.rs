//! `tstrans.webrtc.WhepServer` — bindings for `tst_webrtc::WhepServer`.
//!
//! The server is an `Arc` plus a closed latch (see the module doc in
//! `mod.rs`). `close()` sets the latch and stops the server; a call already
//! in flight keeps its clone of the `Arc` and finishes on the stopped server.
//!
//! GIL release: `bind`, `add_mount`, `remove_mount`, `stats` and `stop`
//! run with the GIL released through `allow_threads_parking` (`stats` asks
//! the media thread and can wait up to 2 s, `stop` waits up to 2 s per
//! thread). The address getters read fields fixed at bind and keep it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pyo3::intern;
use pyo3::prelude::*;
use tst_pipeline::binding::{BindingError, BindingErrorKind};
use tst_webrtc::{AudioPolicy, KlvCarriage, MountConfig, MountKind, WhepServer};

use crate::generated::stats_webrtc::ServerStats;
use crate::mux::py_video_codec;

use super::mount::PyMountHandle;
use super::{closed, webrtc_err};

/// A WHEP server: browsers POST an SDP offer to a mount's path and play it
/// over WebRTC. Construct with `WhepServer.bind(url)`.
#[pyclass(name = "WhepServer", module = "tstrans.webrtc", frozen)]
pub struct PyWhepServer {
    inner: Arc<WhepServer>,
    closed: AtomicBool,
}

impl PyWhepServer {
    /// The server, or `WebrtcError(CLOSED)` after `close()`.
    fn server(&self, py: Python<'_>) -> PyResult<Arc<WhepServer>> {
        if self.closed.load(Ordering::Acquire) {
            return Err(closed(py));
        }
        Ok(Arc::clone(&self.inner))
    }
}

#[pymethods]
impl PyWhepServer {
    /// Binds the signalling (HTTP) and media (UDP) sockets of a
    /// `whep://` / `wheps://` URL and starts the server.
    ///
    /// Raises `WebrtcError(URL)` for an unparsable URL (the host must be a
    /// literal IP), `WebrtcError(TLS)` for an unreadable certificate or key,
    /// and `WebrtcError(IO)` for a port that cannot be bound.
    #[staticmethod]
    pub fn bind(py: Python<'_>, url: &str) -> PyResult<Self> {
        let server = crate::util::allow_threads_parking(py, || WhepServer::bind(url))
            .map_err(|e| webrtc_err(py, e))?;
        Ok(Self {
            inner: Arc::new(server),
            closed: AtomicBool::new(false),
        })
    }

    /// Adds a mount at `path` and returns its push handle. `config` is a
    /// `tstrans.webrtc.MountConfig`; `None` uses the defaults.
    ///
    /// Raises `WebrtcError(MOUNT)` for an invalid or duplicate path or a
    /// configuration `MountConfig` validation refuses, and
    /// `WebrtcError(CLOSED)` once the server is stopped.
    #[pyo3(signature = (path, config = None))]
    pub fn add_mount(
        &self,
        py: Python<'_>,
        path: &str,
        config: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PyMountHandle> {
        let server = self.server(py)?;
        let cfg = match config {
            Some(c) => mount_config(c)?,
            None => MountConfig::default(),
        };
        let mount = crate::util::allow_threads_parking(py, || server.add_mount(path, cfg))
            .map_err(|e| webrtc_err(py, e))?;
        Ok(PyMountHandle::new(mount))
    }

    /// Removes the mount at `path`: its viewers' sessions end and its
    /// handles' pushes raise `WebrtcError(CLOSED)`. Raises
    /// `WebrtcError(MOUNT)` when nothing is mounted there.
    pub fn remove_mount(&self, py: Python<'_>, path: &str) -> PyResult<()> {
        let server = self.server(py)?;
        crate::util::allow_threads_parking(py, || server.remove_mount(path))
            .map_err(|e| webrtc_err(py, e))
    }

    /// The server's counters. Asks the media thread and can wait up to 2 s
    /// for its reply when it is busy.
    pub fn stats(&self, py: Python<'_>) -> PyResult<ServerStats> {
        let server = self.server(py)?;
        let s = crate::util::allow_threads_parking(py, || server.stats());
        Ok(ServerStats::from(&s))
    }

    /// The signalling (HTTP) address as `"ip:port"`, with the kernel's port
    /// when the URL asked for port 0.
    pub fn local_addr(&self, py: Python<'_>) -> PyResult<String> {
        Ok(self.server(py)?.local_addr().to_string())
    }

    /// The media (UDP) address as `"ip:port"`, shared by every viewer.
    pub fn media_addr(&self, py: Python<'_>) -> PyResult<String> {
        Ok(self.server(py)?.media_addr().to_string())
    }

    /// Stops the server: every mount's pushes then raise
    /// `WebrtcError(CLOSED)` and the sessions end. Idempotent and bounded
    /// (at most 2 s per server thread).
    pub fn stop(&self, py: Python<'_>) -> PyResult<()> {
        let server = self.server(py)?;
        crate::util::allow_threads_parking(py, || server.stop());
        Ok(())
    }

    /// `stop()`, then releases the server: every later call raises
    /// `WebrtcError(CLOSED)`. Idempotent.
    pub fn close(&self, py: Python<'_>) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let server = Arc::clone(&self.inner);
        crate::util::allow_threads_parking(py, || server.stop());
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// `close()`; never suppresses the block's exception.
    #[pyo3(signature = (exc_type=None, exc_value=None, traceback=None))]
    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: Option<&Bound<'_, PyAny>>,
        exc_value: Option<&Bound<'_, PyAny>>,
        traceback: Option<&Bound<'_, PyAny>>,
    ) -> bool {
        let _ = (exc_type, exc_value, traceback);
        self.close(py);
        false
    }

    fn __repr__(&self) -> String {
        if self.closed.load(Ordering::Acquire) {
            return "WhepServer(closed)".to_string();
        }
        format!(
            "WhepServer(local_addr={:?}, media_addr={:?})",
            self.inner.local_addr().to_string(),
            self.inner.media_addr().to_string(),
        )
    }
}

/// Reads a `tstrans.webrtc.MountConfig` by attribute name. The enums are
/// `IntEnum`s whose values follow the Rust declaration order; a value
/// outside an enum, or a count outside `u32`, is refused as `MOUNT`, the
/// invalid-configuration kind (an out-of-range count becomes 0, which
/// `MountConfig::validate` refuses with the range in its message).
fn mount_config(obj: &Bound<'_, PyAny>) -> PyResult<MountConfig> {
    let py = obj.py();
    let refuse = |what: &str, v: i64| {
        webrtc_err(
            py,
            BindingError::new(
                BindingErrorKind::WebrtcMount,
                format!("unknown {what} value {v}"),
            ),
        )
    };
    let int =
        |name: &Bound<'_, pyo3::types::PyString>| -> PyResult<i64> { obj.getattr(name)?.extract() };
    let mut cfg = MountConfig::default();
    cfg.kind = match int(intern!(py, "kind"))? {
        0 => MountKind::Elementary,
        1 => MountKind::Mp2t,
        v => return Err(refuse("MountKind", v)),
    };
    cfg.video = py_video_codec(&obj.getattr(intern!(py, "video"))?)?;
    cfg.audio = match int(intern!(py, "audio"))? {
        0 => AudioPolicy::Drop,
        1 => AudioPolicy::PassthroughOpus,
        v => return Err(refuse("AudioPolicy", v)),
    };
    cfg.klv = match int(intern!(py, "klv"))? {
        0 => KlvCarriage::SeiAndDataChannel,
        1 => KlvCarriage::SeiOnly,
        2 => KlvCarriage::DataChannelOnly,
        v => return Err(refuse("KlvCarriage", v)),
    };
    cfg.sei_budget_bytes = u32::try_from(int(intern!(py, "sei_budget_bytes"))?).unwrap_or(0);
    cfg.viewer_queue_packets =
        u32::try_from(int(intern!(py, "viewer_queue_packets"))?).unwrap_or(0);
    Ok(cfg)
}

// ---------------------------------------------------------------------------
// Test-only viewer probe (feature = "py-test-hooks"): the str0m test viewer
// of tst-test-helpers' `whep-viewer` feature, which tst-webrtc's own tests
// use too, plays a mount that Python feeds, so the pytest suite proves media
// reaches a real WebRTC peer. Never compiled into the wheels.
// ---------------------------------------------------------------------------

/// `_loopback_viewer_probe(server, mount)` → the frames one viewer received:
/// it joins `mount`, waits up to 5 s to connect and up to 5 s for a first
/// frame, then returns. A viewer that does not connect, or a refused join,
/// raises `RuntimeError` with the viewer's trace.
#[cfg(feature = "py-test-hooks")]
#[pyfunction]
#[pyo3(name = "_loopback_viewer_probe")]
pub(super) fn loopback_viewer_probe(
    py: Python<'_>,
    server: PyRef<'_, PyWhepServer>,
    mount: &str,
) -> PyResult<usize> {
    use std::time::Duration;
    use tst_test_helpers::whep_viewer;

    let addr = server.server(py)?.local_addr();
    let joined = crate::util::allow_threads_parking(py, || {
        tst_pipeline::binding::panic::catch(|| {
            let mut v = whep_viewer::TestViewer::new();
            whep_viewer::join(addr, mount, &mut v, false);
            if !v.pump(|v| v.connected, Duration::from_secs(5)) {
                return Err(format!("the viewer did not connect: {}", v.trace()));
            }
            v.pump(|v| !v.frames.is_empty(), Duration::from_secs(5));
            Ok(v.frames.len())
        })
    });
    match joined {
        Ok(Ok(frames)) => Ok(frames),
        Ok(Err(msg)) => Err(pyo3::exceptions::PyRuntimeError::new_err(msg)),
        Err(panic) => Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
            "the viewer could not join: {panic}"
        ))),
    }
}
