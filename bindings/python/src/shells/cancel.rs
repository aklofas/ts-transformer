//! `CancelHandle` and `SocketStats`, the two value classes every shell hands
//! out, plus the helper that turns a transport's cancel handle into the
//! shell's shared [`CancelSource`].

#![allow(unsafe_op_in_unsafe_fn, clippy::useless_conversion)]

use std::sync::Arc;

use pyo3::prelude::*;

use tst_core::transport::{SocketStats, TransportCancel};

use crate::util::CancelSource;

/// Mirror of `tst_core::transport::SocketStats` exposed to Python as a
/// frozen, `get_all`-decorated PyClass: the 16 scheme-neutral wire
/// counters, the same on every transport. For SRT-specific extras
/// (`mbps_estimated_bandwidth`, the symmetric send/recv-side byte-loss
/// split) use `srt_stats()` on an `srt://` shell.
#[pyclass(frozen, get_all, name = "SocketStats", module = "tstrans.shells")]
pub(crate) struct PySocketStats {
    pub rtt_us: u32,
    pub send_bandwidth_bps: u64,
    pub recv_bandwidth_bps: u64,
    pub link_bandwidth_bps: u64,
    pub bytes_sent: u64,
    pub packets_sent: u64,
    pub bytes_received: u64,
    pub packets_received: u64,
    pub bytes_lost_recv: u64,
    pub packets_lost_recv: u64,
    pub packets_lost_send: u64,
    pub packets_retransmitted: u64,
    pub packets_dropped_send: u64,
    pub packets_dropped_recv: u64,
    pub send_buffer_packets: u32,
    pub recv_buffer_packets: u32,
}

impl PySocketStats {
    pub(crate) fn from_core(s: SocketStats) -> Self {
        Self {
            rtt_us: s.rtt_us,
            send_bandwidth_bps: s.send_bandwidth_bps,
            recv_bandwidth_bps: s.recv_bandwidth_bps,
            link_bandwidth_bps: s.link_bandwidth_bps,
            bytes_sent: s.bytes_sent,
            packets_sent: s.packets_sent,
            bytes_received: s.bytes_received,
            packets_received: s.packets_received,
            bytes_lost_recv: s.bytes_lost_recv,
            packets_lost_recv: s.packets_lost_recv,
            packets_lost_send: s.packets_lost_send,
            packets_retransmitted: s.packets_retransmitted,
            packets_dropped_send: s.packets_dropped_send,
            packets_dropped_recv: s.packets_dropped_recv,
            send_buffer_packets: s.send_buffer_packets,
            recv_buffer_packets: s.recv_buffer_packets,
        }
    }
}

#[pymethods]
impl PySocketStats {
    fn __repr__(&self) -> String {
        format!(
            "SocketStats(bytes_sent={}, packets_sent={}, bytes_received={}, packets_received={}, rtt_us={})",
            self.bytes_sent,
            self.packets_sent,
            self.bytes_received,
            self.packets_received,
            self.rtt_us,
        )
    }
}

/// Python-side cancel handle. Wraps the shell's shared
/// [`CancelSource`]: every clone obtained from the same shell, and the
/// shell's own `close()`, forwards into one cancel and flips one flag, so
/// `is_cancelled()` reports the shell's state, not this wrapper's history.
#[pyclass(frozen, name = "CancelHandle", module = "tstrans.shells")]
pub(crate) struct PyCancelHandle {
    src: Arc<CancelSource>,
}

#[pymethods]
impl PyCancelHandle {
    /// Signal cancellation. Idempotent. Wakes a thread parked in
    /// `send_bytes` / `recv_bytes` / `__next__`; that call raises the
    /// transport's `CLOSED` kind.
    fn cancel(&self) {
        TransportCancel::cancel(&*self.src);
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
        }
    }
}

/// The shell's shared cancel state over the transport's own cancel handle
/// (taken from `Opened` before the transport was boxed into the shell).
pub(crate) fn cancel_source(cancel: &Arc<dyn TransportCancel>) -> Arc<CancelSource> {
    CancelSource::new(Arc::clone(cancel))
}
