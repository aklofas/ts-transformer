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
//! (the `StreamEndReason` convention); the getters here look the member up
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
    PublishMountStats as RustPublishMountStats, PublishShape as RustPublishShape,
    PublisherInfo as RustPublisherInfo,
};

use crate::raise::{RTSP, raise};
use crate::rtp::demux_receiver::PyDemuxReceiver;
use tst_pipeline::binding::BindingError;

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyPublishMount>()?;
    m.add_class::<PyPublishMountStats>()?;
    m.add_class::<PyPublisherInfo>()?;
    Ok(())
}

/// Look up `tstrans.rtp.<enum_name>.<member>` (a pure-Python `IntEnum`).
fn rtp_enum_member(py: Python<'_>, enum_name: &str, member: &str) -> PyResult<PyObject> {
    let rtp = py.import_bound("tstrans.rtp")?;
    Ok(rtp.getattr(enum_name)?.getattr(member)?.into())
}

fn alignment_name(a: RustClockAlignment) -> PyResult<&'static str> {
    Ok(match a {
        RustClockAlignment::NotApplicable => "NOT_APPLICABLE",
        RustClockAlignment::Pending => "PENDING",
        RustClockAlignment::Provisional => "PROVISIONAL",
        RustClockAlignment::SenderReport => "SENDER_REPORT",
        _ => {
            return Err(PyRuntimeError::new_err(
                "ClockAlignment variant unknown to this build of tstrans",
            ));
        }
    })
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
    pub fn shape(&self, py: Python<'_>) -> PyResult<PyObject> {
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
// PyPublishMountStats.
// ---------------------------------------------------------------------------

/// Frozen snapshot of a publish mount's stats. Returned by
/// [`PublishMount.stats()`][PyPublishMount::stats]. Counters are
/// cumulative over the mount's life, across publishers.
#[pyclass(name = "PublishMountStats", module = "tstrans.rtp", frozen)]
pub struct PyPublishMountStats {
    inner: RustPublishMountStats,
}

#[pymethods]
impl PyPublishMountStats {
    /// RTP packets received from publishers, counted before validation.
    #[getter]
    pub fn rtp_packets_received(&self) -> u64 {
        self.inner.rtp_packets_received
    }

    /// Bytes of those RTP packets, headers included.
    #[getter]
    pub fn bytes_received(&self) -> u64 {
        self.inner.bytes_received
    }

    /// Packets dropped as unusable: not RTP, the wrong payload type, an
    /// invalid MP2T payload, or an interleaved frame on an unknown channel.
    #[getter]
    pub fn malformed_packets(&self) -> u64 {
        self.inner.malformed_packets
    }

    /// UDP datagrams dropped because they came from an IP other than the
    /// publisher's control connection.
    #[getter]
    pub fn source_rejected(&self) -> u64 {
        self.inner.source_rejected
    }

    /// Frames emitted to the mount's sinks (PLAY readers and the
    /// application transport).
    #[getter]
    pub fn frames_emitted(&self) -> u64 {
        self.inner.frames_emitted
    }

    /// Frames dropped because the application transport's queue was full
    /// (the application stopped reading its `DemuxReceiver`).
    #[getter]
    pub fn frames_dropped_app(&self) -> u64 {
        self.inner.frames_dropped_app
    }

    /// Frames dropped across PLAY readers that lagged behind the fan-out.
    #[getter]
    pub fn frames_dropped_readers(&self) -> u64 {
        self.inner.frames_dropped_readers
    }

    /// Access units emitted by an elementary-shape adapter.
    #[getter]
    pub fn aus_emitted(&self) -> u64 {
        self.inner.aus_emitted
    }

    /// Access units an elementary-shape adapter dropped.
    #[getter]
    pub fn aus_dropped(&self) -> u64 {
        self.inner.aus_dropped
    }

    /// Access units muxed with a PTS below one already muxed: nonzero means
    /// the publisher sends B-frames.
    #[getter]
    pub fn aus_reordered(&self) -> u64 {
        self.inner.aus_reordered
    }

    /// KLV units emitted by an elementary-shape adapter.
    #[getter]
    pub fn klv_units_emitted(&self) -> u64 {
        self.inner.klv_units_emitted
    }

    /// KLV units an elementary-shape adapter dropped.
    #[getter]
    pub fn klv_units_dropped(&self) -> u64 {
        self.inner.klv_units_dropped
    }

    /// How the current publisher's tracks are aligned to one clock
    /// (`ClockAlignment`).
    #[getter]
    pub fn alignment(&self, py: Python<'_>) -> PyResult<PyObject> {
        rtp_enum_member(py, "ClockAlignment", alignment_name(self.inner.alignment)?)
    }

    /// Times a new clock mapping replaced the previous one.
    #[getter]
    pub fn alignment_steps(&self) -> u64 {
        self.inner.alignment_steps
    }

    /// Source restarts (RTP SSRC changes) on an elementary publisher's
    /// tracks, counted once per change per track.
    #[getter]
    pub fn ssrc_changes(&self) -> u64 {
        self.inner.ssrc_changes
    }

    /// Publishers that have ended on this mount.
    #[getter]
    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    /// Live PLAY readers subscribed to the mount's fan-out.
    #[getter]
    pub fn peer_count(&self) -> usize {
        self.inner.peer_count
    }

    fn __repr__(&self) -> PyResult<String> {
        let s = &self.inner;
        Ok(format!(
            "PublishMountStats(rtp_packets_received={}, bytes_received={}, malformed_packets={}, \
             source_rejected={}, frames_emitted={}, frames_dropped_app={}, \
             frames_dropped_readers={}, aus_emitted={}, aus_dropped={}, aus_reordered={}, \
             klv_units_emitted={}, klv_units_dropped={}, alignment={}, alignment_steps={}, \
             ssrc_changes={}, generation={}, peer_count={})",
            s.rtp_packets_received,
            s.bytes_received,
            s.malformed_packets,
            s.source_rejected,
            s.frames_emitted,
            s.frames_dropped_app,
            s.frames_dropped_readers,
            s.aus_emitted,
            s.aus_dropped,
            s.aus_reordered,
            s.klv_units_emitted,
            s.klv_units_dropped,
            alignment_name(s.alignment)?,
            s.alignment_steps,
            s.ssrc_changes,
            s.generation,
            s.peer_count,
        ))
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
    pub fn publisher(&self, py: Python<'_>) -> PyResult<Option<PyPublisherInfo>> {
        let info = py.allow_threads(|| self.inner.publisher());
        info.as_ref().map(PyPublisherInfo::from_rust).transpose()
    }

    /// Snapshot of the mount's cumulative and live stats.
    pub fn stats(&self, py: Python<'_>) -> PyPublishMountStats {
        PyPublishMountStats {
            inner: py.allow_threads(|| self.inner.stats()),
        }
    }

    /// End the application side only: a `DemuxReceiver` taken from this
    /// mount (now or later) raises `RtpError(CLOSED)` from its next read.
    /// PLAY readers and the publisher are unaffected. Idempotent.
    pub fn cancel(&self, py: Python<'_>) {
        py.allow_threads(|| self.inner.cancel());
    }

    /// Take the mount's received MPEG-TS as a `tstrans.rtp.DemuxReceiver`.
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
    pub fn into_demux_receiver(
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
        PyDemuxReceiver::from_recv_transport(py, transport, opts)
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
