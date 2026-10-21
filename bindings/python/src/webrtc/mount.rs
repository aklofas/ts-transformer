//! `tstrans.webrtc.MountHandle` and `SessionStats`.
//!
//! The handle is a clone of the mount's `tst_webrtc::MountHandle` (one
//! `Arc`) plus a closed latch. `close()` frees only this wrapper; the mount
//! stays on the server. A push that panics closes the wrapper, so a later
//! call raises `WebrtcError(CLOSED)` instead of using a mount whose push
//! state may be torn.
//!
//! GIL release: the pushes and `sessions` run through
//! `allow_threads_parking` (`sessions` asks the media thread and can wait
//! up to 2 s; the pushes never block, but share the exit-park contract);
//! `stats` reads atomics and `mount_path` a field, so they keep the GIL.

use std::sync::atomic::{AtomicBool, Ordering};

use pyo3::prelude::*;
use tst_pipeline::binding::HandleState;
use tst_pipeline::binding::panic::catch;
use tst_webrtc::{IceState, MountHandle, PushError, SessionStats};

use crate::generated::stats_webrtc::MountStats;
use crate::mux::py_pts90khz;

use super::{closed, webrtc_err};

/// The push side of one mount, returned by `WhepServer.add_mount`.
#[pyclass(name = "MountHandle", module = "tstrans.webrtc", frozen)]
pub struct PyMountHandle {
    inner: MountHandle,
    closed: AtomicBool,
}

impl PyMountHandle {
    pub(super) fn new(inner: MountHandle) -> Self {
        Self {
            inner,
            closed: AtomicBool::new(false),
        }
    }

    /// The mount, or `WebrtcError(CLOSED)` after `close()`.
    fn mount(&self, py: Python<'_>) -> PyResult<&MountHandle> {
        if self.closed.load(Ordering::Acquire) {
            return Err(closed(py));
        }
        Ok(&self.inner)
    }

    /// Runs one push with the GIL released. A panic closes the wrapper and
    /// raises `PanicException`; a refused push raises `WebrtcError`.
    fn push(
        &self,
        py: Python<'_>,
        f: impl Send + FnOnce(&MountHandle) -> Result<(), PushError>,
    ) -> PyResult<()> {
        let mount = self.mount(py)?;
        match crate::util::allow_threads_parking(py, || catch(|| f(mount))) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(webrtc_err(py, e)),
            Err(detail) => {
                self.closed.store(true, Ordering::Release);
                Err(webrtc_err(py, HandleState::Panicked { detail }))
            }
        }
    }
}

#[pymethods]
impl PyMountHandle {
    /// Pushes one Annex-B access unit of an elementary mount. `pts` is in
    /// 90 kHz ticks; `key_frame` is advisory (the NAL units decide).
    #[pyo3(signature = (au, *, pts, key_frame))]
    pub fn push_video(
        &self,
        py: Python<'_>,
        au: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
        key_frame: bool,
    ) -> PyResult<()> {
        let pts = py_pts90khz(pts)?;
        let bytes = crate::util::coerce_bytes_like(py, au)?;
        let au = bytes.as_bytes();
        self.push(py, |m| m.push_video(au, pts, key_frame))
    }

    /// Pushes one KLV record of an elementary mount (a whole local set, as
    /// `encode_uas_datalink` returns it); it rides the first frame pushed at
    /// or after `pts`. A record over 256 KiB, the data-channel limit, raises
    /// `WebrtcError(PUSH)`. `metadata_service_id` (0..=255, else
    /// `ValueError`) is accepted for parity with the RTSP mount and ignored:
    /// a WebRTC mount carries one KLV stream.
    #[pyo3(signature = (record, *, pts, metadata_service_id = 0))]
    pub fn push_klv(
        &self,
        py: Python<'_>,
        record: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
        metadata_service_id: i64,
    ) -> PyResult<()> {
        let service_id = u8::try_from(metadata_service_id).map_err(|_| {
            pyo3::exceptions::PyValueError::new_err(format!(
                "metadata_service_id must be in 0..=255; got {metadata_service_id}"
            ))
        })?;
        let pts = py_pts90khz(pts)?;
        let bytes = crate::util::coerce_bytes_like(py, record)?;
        let record = bytes.as_bytes();
        self.push(py, |m| m.push_klv(record, pts, service_id))
    }

    /// Pushes one Opus packet of an elementary mount; counted and dropped
    /// under `AudioPolicy.DROP`.
    #[pyo3(signature = (frames, *, pts))]
    pub fn push_audio(
        &self,
        py: Python<'_>,
        frames: &Bound<'_, PyAny>,
        pts: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        let pts = py_pts90khz(pts)?;
        let bytes = crate::util::coerce_bytes_like(py, frames)?;
        let frames = bytes.as_bytes();
        self.push(py, |m| m.push_audio(frames, pts))
    }

    /// Pushes MPEG-TS bytes into an MP2T mount, any number of whole or
    /// partial packets.
    pub fn push_ts(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        let bytes = crate::util::coerce_bytes_like(py, data)?;
        let data = bytes.as_bytes();
        self.push(py, |m| m.push_ts(data))
    }

    /// The mount's counters; never waits.
    pub fn stats(&self, py: Python<'_>) -> PyResult<MountStats> {
        Ok(MountStats::from(&self.mount(py)?.stats()))
    }

    /// One `SessionStats` per viewer session. Asks the media thread and can
    /// wait up to 2 s for its reply when it is busy.
    pub fn sessions(&self, py: Python<'_>) -> PyResult<Vec<PySessionStats>> {
        let mount = self.mount(py)?;
        let sessions = crate::util::allow_threads_parking(py, || mount.sessions());
        Ok(sessions.iter().map(PySessionStats::from).collect())
    }

    /// The mount's path.
    pub fn mount_path(&self, py: Python<'_>) -> PyResult<String> {
        Ok(self.mount(py)?.mount_path().to_owned())
    }

    /// Frees this handle; the mount keeps serving until
    /// `WhepServer.remove_mount`, `stop()` or `close()`. Idempotent.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn __repr__(&self) -> String {
        let closed = if self.closed.load(Ordering::Acquire) {
            ", closed"
        } else {
            ""
        };
        format!("MountHandle(path={:?}{closed})", self.inner.mount_path())
    }
}

/// Statistics of one viewer session.
#[pyclass(name = "SessionStats", module = "tstrans.webrtc", frozen, get_all)]
pub struct PySessionStats {
    /// The session's identifier, the last segment of its resource URL.
    id: String,
    /// The path of the mount the session plays.
    mount_path: String,
    /// The viewer's address as `"ip:port"` once ICE has selected one.
    remote_addr: Option<String>,
    /// The ICE connection state: `new`, `checking`, `connected`,
    /// `disconnected`, `failed` or `closed`.
    ice_state: String,
    /// Milliseconds since the session connected; `None` before it does.
    connected_for_ms: Option<u64>,
    /// RTP packets sent to the viewer.
    rtp_packets: u64,
    /// RTP bytes sent to the viewer.
    rtp_bytes: u64,
    /// NACKs received from the viewer.
    nacks: u64,
    /// Picture loss indications received from the viewer.
    plis: u64,
    /// KLV messages sent on the viewer's data channel.
    dc_messages: u64,
    /// RTP payloads dropped because the viewer's queue was full.
    packets_dropped_queue_full: u64,
    /// KLV messages dropped for the viewer: its queue was full, or its data
    /// channel refused or failed the write.
    dc_messages_dropped: u64,
}

/// The ICE state as the lower-case word `SessionStats.ice_state` documents.
fn ice_state_name(s: IceState) -> &'static str {
    match s {
        IceState::New => "new",
        IceState::Checking => "checking",
        IceState::Connected => "connected",
        IceState::Disconnected => "disconnected",
        IceState::Failed => "failed",
        IceState::Closed => "closed",
        _ => "unknown",
    }
}

impl From<&SessionStats> for PySessionStats {
    fn from(s: &SessionStats) -> Self {
        Self {
            id: s.id.clone(),
            mount_path: s.mount_path.clone(),
            remote_addr: s.remote_addr.map(|a| a.to_string()),
            ice_state: ice_state_name(s.ice_state).to_string(),
            connected_for_ms: s
                .connected_for
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            rtp_packets: s.rtp_packets,
            rtp_bytes: s.rtp_bytes,
            nacks: s.nacks,
            plis: s.plis,
            dc_messages: s.dc_messages,
            packets_dropped_queue_full: s.packets_dropped_queue_full,
            dc_messages_dropped: s.dc_messages_dropped,
        }
    }
}

#[pymethods]
impl PySessionStats {
    fn __repr__(&self) -> String {
        format!(
            "SessionStats(id={:?}, mount_path={:?}, remote_addr={:?}, ice_state={:?}, \
             connected_for_ms={:?}, rtp_packets={}, rtp_bytes={}, nacks={}, plis={}, \
             dc_messages={}, packets_dropped_queue_full={}, dc_messages_dropped={})",
            self.id,
            self.mount_path,
            self.remote_addr,
            self.ice_state,
            self.connected_for_ms,
            self.rtp_packets,
            self.rtp_bytes,
            self.nacks,
            self.plis,
            self.dc_messages,
            self.packets_dropped_queue_full,
            self.dc_messages_dropped,
        )
    }
}
