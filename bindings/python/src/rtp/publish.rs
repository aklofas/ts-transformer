//! `PublishMount`, `PublishMountStats`, `PublisherInfo` — the RTSP
//! publisher role (ANNOUNCE / RECORD ingest on `RtspServer`).
//!
//! Bindings for `tst_rtp::rtsp::server::publish::{PublishMountHandle,
//! PublishMountStats, PublisherInfo}`. A `PublishMount` comes from
//! `RtspServer.add_publish_mount(path)` or, for mounts an ANNOUNCE created
//! on demand, from `RtspServer.next_publisher()`; both live in
//! `server.rs`.
//!
//! `PublishShape` and `ClockAlignment` only ever flow Rust → Python as
//! return values, so they are pure-Python `IntEnum`s in `tstrans/rtp.py`
//! (the `EndReason` convention); the getters here look the member up
//! by name.
//!
//! GIL release boundaries: `stats`, `publisher`, `cancel` and
//! `into_demux_receiver` take a lock inside the mount and release the GIL
//! first. `mount_path`, `peer_count` and `generation` read immutable or
//! atomic state and keep it.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::time::UNIX_EPOCH;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use tst_rtp::rtsp::server::publish::{
    ClockAlignment as RustClockAlignment, PublishMountHandle as RustPublishMountHandle,
    PublishShape as RustPublishShape, PublisherInfo as RustPublisherInfo,
};

use crate::generated::stats_rtp::PublishMountStats;
use crate::raise::{RTSP, raise};
use crate::shells::demux_receiver::PyDemuxReceiver;
use tst_pipeline::binding::BindingError;

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyPublishMount>()?;
    m.add_class::<PyPublisherInfo>()?;
    Ok(())
}

/// Look up `tstrans.rtp.<enum_name>.<member>` (a pure-Python `IntEnum`).
fn rtp_enum_member(py: Python<'_>, enum_name: &str, member: &str) -> PyResult<Py<PyAny>> {
    let rtp = py.import("tstrans.rtp")?;
    Ok(rtp.getattr(enum_name)?.getattr(member)?.into())
}

/// A publish mount's clock alignment as the host reads it: the
/// `tstrans.rtp.ClockAlignment` member of the same name. A variant this build
/// does not know raises `RuntimeError`, as the Java builder throws.
#[derive(Debug, Clone, Copy)]
pub struct ClockAlignmentPy(RustClockAlignment);

impl TryFrom<RustClockAlignment> for ClockAlignmentPy {
    type Error = PyErr;

    fn try_from(a: RustClockAlignment) -> PyResult<Self> {
        match a {
            RustClockAlignment::NotApplicable
            | RustClockAlignment::Pending
            | RustClockAlignment::Provisional
            | RustClockAlignment::SenderReport => Ok(Self(a)),
            _ => Err(PyRuntimeError::new_err(
                "ClockAlignment variant unknown to this build of tstrans",
            )),
        }
    }
}

impl ClockAlignmentPy {
    /// The member name (`UNKNOWN` cannot occur: construction refuses a
    /// variant this build does not know).
    pub fn name(self) -> &'static str {
        match self.0 {
            RustClockAlignment::NotApplicable => "NOT_APPLICABLE",
            RustClockAlignment::Pending => "PENDING",
            RustClockAlignment::Provisional => "PROVISIONAL",
            RustClockAlignment::SenderReport => "SENDER_REPORT",
            _ => "UNKNOWN",
        }
    }
}

impl<'py> IntoPyObject<'py> for ClockAlignmentPy {
    type Target = PyAny;
    type Output = Bound<'py, PyAny>;
    type Error = std::convert::Infallible;

    fn into_pyobject(self, py: Python<'py>) -> Result<Self::Output, Self::Error> {
        Ok(rtp_enum_member(py, "ClockAlignment", self.name())
            .unwrap_or_else(|_| py.None())
            .into_bound(py))
    }
}

// ---------------------------------------------------------------------------
// PyPublisherInfo.
// ---------------------------------------------------------------------------

/// Which publisher holds a publish mount's publisher slot. Frozen
/// snapshot returned by [`PublishMount.publisher()`][PyPublishMount::publisher].
#[pyclass(name = "PublisherInfo", module = "tstrans.rtp", frozen)]
pub struct PyPublisherInfo {
    peer: String,
    /// `"MP2T"` or `"ELEMENTARY"` — the `PublishShape` member name.
    shape: &'static str,
    klv: bool,
    since_unix_ms: u64,
    generation: u64,
}

impl PyPublisherInfo {
    fn from_rust(info: &RustPublisherInfo) -> PyResult<Self> {
        let (shape, klv) = match info.shape {
            RustPublishShape::Mp2t => ("MP2T", false),
            RustPublishShape::Elementary { klv } => ("ELEMENTARY", klv),
            _ => {
                return Err(PyRuntimeError::new_err(
                    "PublishShape variant unknown to this build of tstrans",
                ));
            }
        };
        // A clock set before 1970 reads 0 rather than failing the snapshot.
        let since_unix_ms = info
            .since
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        Ok(Self {
            peer: info.peer.to_string(),
            shape,
            klv,
            since_unix_ms,
            generation: info.generation,
        })
    }
}

#[pymethods]
impl PyPublisherInfo {
    /// Address (`"ip:port"`) of the publisher's RTSP control connection.
    #[getter]
    pub fn peer(&self) -> String {
        self.peer.clone()
    }

    /// Wire shape the publisher's ANNOUNCE declared (`PublishShape`).
    #[getter]
    pub fn shape(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        rtp_enum_member(py, "PublishShape", self.shape)
    }

    /// `True` when an elementary announce carries a KLV track beside the
    /// video; always `False` for `PublishShape.MP2T`.
    #[getter]
    pub fn klv(&self) -> bool {
        self.klv
    }

    /// When the ANNOUNCE claimed the mount, in milliseconds since the Unix
    /// epoch.
    #[getter]
    pub fn since_unix_ms(&self) -> u64 {
        self.since_unix_ms
    }

    /// The mount's publisher generation while this publisher holds it (the
    /// count of publishers that ended on the mount before it).
    #[getter]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn __repr__(&self) -> String {
        format!(
            "PublisherInfo(peer={:?}, shape={}, klv={}, since_unix_ms={}, generation={})",
            self.peer,
            self.shape,
            if self.klv { "True" } else { "False" },
            self.since_unix_ms,
            self.generation,
        )
    }
}

// ---------------------------------------------------------------------------
// PyPublishMount.
// ---------------------------------------------------------------------------

/// A publish mount: the server accepts ANNOUNCE / RECORD on its path and
/// hands the received MPEG-TS to the application through
/// [`into_demux_receiver`][PyPublishMount::into_demux_receiver]; PLAY
/// readers on the same path are re-served from the same TS bytes.
///
/// Returned by `RtspServer.add_publish_mount(path)` and
/// `RtspServer.next_publisher()`. The mount object stays usable for
/// `stats()` / `publisher()` / `generation()` after its transport is
/// taken, after `RtspServer.remove_mount`, and after the server stops.
#[pyclass(name = "PublishMount", module = "tstrans.rtp", frozen)]
#[derive(Clone)]
pub struct PyPublishMount {
    inner: RustPublishMountHandle,
}

impl PyPublishMount {
    pub(crate) fn new(inner: RustPublishMountHandle) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyPublishMount {
    /// The mount path (`"/path"`).
    pub fn mount_path(&self) -> String {
        self.inner.mount_path().to_string()
    }

    /// Live PLAY readers subscribed to the mount's fan-out.
    pub fn peer_count(&self) -> usize {
        self.inner.peer_count()
    }

    /// Publishers that have ended on this mount.
    pub fn generation(&self) -> u64 {
        self.inner.generation()
    }

    /// The current publisher, or `None` when no publisher holds the mount.
    #[expect(
        clippy::disallowed_methods,
        reason = "clones PublisherInfo under a short in-memory mutex"
    )]
    pub fn publisher(&self, py: Python<'_>) -> PyResult<Option<PyPublisherInfo>> {
        let info = py.detach(|| self.inner.publisher());
        info.as_ref().map(PyPublisherInfo::from_rust).transpose()
    }

    /// Snapshot of the mount's cumulative and live stats.
    #[expect(
        clippy::disallowed_methods,
        reason = "snapshots counters under a short in-memory mutex"
    )]
    pub fn stats(&self, py: Python<'_>) -> PyResult<PublishMountStats> {
        PublishMountStats::try_from(&py.detach(|| self.inner.stats()))
    }

    /// End the application side only: a `DemuxReceiver` taken from this
    /// mount (now or later) raises `RtpError(CLOSED)` from its next read.
    /// PLAY readers and the publisher are unaffected. Idempotent.
    #[expect(
        clippy::disallowed_methods,
        reason = "sets the shared cancel flag; no lock held across I/O"
    )]
    pub fn cancel(&self, py: Python<'_>) {
        py.detach(|| self.inner.cancel());
    }

    /// Take the mount's received MPEG-TS as a `tstrans.shells.DemuxReceiver`.
    ///
    /// Take-once across every handle to the mount (including the one
    /// `next_publisher()` returned for the same mount): a second call raises
    /// `RtspError(CLOSED)`. The receiver outlives publisher churn — it goes
    /// quiet between publishers. It stops iterating (`StopIteration`, the
    /// end-of-stream shape) after `RtspServer.remove_mount` or
    /// `RtspServer.stop()`; after `cancel()` its next read raises
    /// `RtpError(CLOSED)`.
    ///
    /// `demux_config` accepts the `tstrans.mpegts.DemuxerConfig` the
    /// `Demuxer(config=...)` constructor takes; `None` uses the defaults.
    #[pyo3(signature = (demux_config = None))]
    #[allow(clippy::wrong_self_convention)]
    pub(crate) fn into_demux_receiver(
        &self,
        py: Python<'_>,
        demux_config: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PyDemuxReceiver> {
        // Validate the config BEFORE the take, so a bad config does not
        // spend the take-once transport.
        let opts = match demux_config {
            None => None,
            Some(cfg) => Some(crate::mpegts::build_demuxer_config(py, cfg)?),
        };
        let handle = self.inner.clone();
        let transport =
            crate::util::allow_threads_parking(py, move || handle.into_recv_transport())
                .map_err(|e| raise(py, &RTSP, BindingError::from(e)))?;
        PyDemuxReceiver::from_opened(py, crate::rtp::transport::opened_recv(transport), opts)
    }

    fn __repr__(&self) -> String {
        format!(
            "PublishMount(path={:?}, generation={}, peer_count={})",
            self.inner.mount_path(),
            self.inner.generation(),
            self.inner.peer_count(),
        )
    }
}
