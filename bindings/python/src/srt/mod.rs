//! `tstrans.srt` — SRT transport bindings.
//!
//! See `docs/specs/2026-05-27-tst-py-srt-design.md` for the surface
//! shape. Mirrors the `tstrans.rtp` pattern — same GIL-release boundaries,
//! same bytes-like extraction, same error mapping shape.
//!
//! Submodules:
//! - `transport`:      Sender, Receiver, SocketStats, SrtStats, CancelHandle
//! - `lowlevel`:       Socket, Listener, Builder
//! - `mux_sender`:     MuxSender convenience wrapper
//! - `demux_receiver`: DemuxReceiver convenience wrapper
//! - `policy`:         ReconnectPolicy, BackoffStrategy, OverflowPolicy
//! - `managed`:        ManagedSender, ManagedReceiver, ManagedMuxSender, ManagedDemuxReceiver
//! - `end_reason`:     RecvEndReason conversion for ManagedDemuxReceiver.end_reason()
//!
//! Error mapping lives in `crate::srt::errors` — typed `*_to_pyerr` helpers
//! consolidate every Rust enum that flows through the surface (UrlError /
//! ConnectError / BindError / AcceptError / IoError / TransportError) into
//! the 8-variant `SrtErrorKind`.

pub(crate) mod demux_receiver;
pub(crate) mod end_reason;
pub(crate) mod errors;
mod lowlevel;
pub(crate) mod managed_basic;
pub(crate) mod managed_convenience;
pub(crate) mod mux_sender;
pub(crate) mod policy;
mod transport;

use pyo3::prelude::*;

pub(crate) fn register(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new_bound(parent.py(), "srt")?;
    transport::register(&m)?;
    lowlevel::register(&m)?;
    mux_sender::register(&m)?;
    demux_receiver::register(&m)?;
    policy::register(&m)?;
    managed_basic::register(&m)?;
    managed_convenience::register(&m)?;
    parent.add_submodule(&m)?;
    Ok(())
}
