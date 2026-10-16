//! `SrtStats`, the libsrt-flavoured statistics class of `tstrans.srt`.
//!
//! The shell classes, `CancelHandle` and `SocketStats` live in
//! `tstrans.shells`; this module keeps only what is SRT-specific.

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use pyo3::prelude::*;

// ---------------------------------------------------------------------------
// PySrtStats — frozen mirror of tst_srt::Stats (17 fields)
// ---------------------------------------------------------------------------

/// Mirror of `tst_srt::Stats` — the libsrt-flavored 17-field stats
/// struct. Exposes the SRT-rich fields that don't fit the abstract
/// `SocketStats` shape:
/// - `mbps_estimated_bandwidth` (libsrt's estimate; bps view lives in
///   `SocketStats::link_bandwidth_bps`).
/// - Symmetric send/recv-side byte-loss split
///   (`bytes_lost_send_side` + `bytes_lost_recv_side`).
/// - Symmetric send/recv-side packet drop split.
///
/// `rtt_us` is the `Duration` converted to microseconds, saturating at
/// `u32::MAX` — matches the `SocketStats::rtt_us` projection so callers
/// can pin either accessor and get the same view.
#[pyclass(frozen, get_all, name = "SrtStats", module = "tstrans.srt")]
pub(crate) struct PySrtStats {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub bytes_lost_recv_side: u64,
    pub bytes_lost_send_side: u64,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub packets_lost_recv_side: u64,
    pub packets_lost_send_side: u64,
    pub packets_retransmitted: u64,
    pub packets_dropped_recv_side: u64,
    pub packets_dropped_send_side: u64,
    pub rtt_us: u32,
    pub send_bandwidth_bps: u64,
    pub recv_bandwidth_bps: u64,
    pub mbps_estimated_bandwidth: f64,
    pub send_buffer_packets: u32,
    pub recv_buffer_packets: u32,
}

impl PySrtStats {
    pub(crate) fn from_srt(s: &tst_srt::Stats) -> Self {
        let rtt_us = u32::try_from(s.rtt.as_micros()).unwrap_or(u32::MAX);
        Self {
            bytes_sent: s.bytes_sent,
            bytes_received: s.bytes_received,
            bytes_lost_recv_side: s.bytes_lost_recv_side,
            bytes_lost_send_side: s.bytes_lost_send_side,
            packets_sent: s.packets_sent,
            packets_received: s.packets_received,
            packets_lost_recv_side: s.packets_lost_recv_side,
            packets_lost_send_side: s.packets_lost_send_side,
            packets_retransmitted: s.packets_retransmitted,
            packets_dropped_recv_side: s.packets_dropped_recv_side,
            packets_dropped_send_side: s.packets_dropped_send_side,
            rtt_us,
            send_bandwidth_bps: s.send_bandwidth_bps,
            recv_bandwidth_bps: s.recv_bandwidth_bps,
            mbps_estimated_bandwidth: s.mbps_estimated_bandwidth,
            send_buffer_packets: s.send_buffer_packets,
            recv_buffer_packets: s.recv_buffer_packets,
        }
    }
}

#[pymethods]
impl PySrtStats {
    fn __repr__(&self) -> String {
        format!(
            "SrtStats(bytes_sent={}, packets_sent={}, bytes_received={}, packets_received={}, rtt_us={}, mbps_estimated_bandwidth={:.3})",
            self.bytes_sent,
            self.packets_sent,
            self.bytes_received,
            self.packets_received,
            self.rtt_us,
            self.mbps_estimated_bandwidth,
        )
    }
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PySrtStats>()?;
    Ok(())
}
