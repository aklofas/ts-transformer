//! `tstrans.srt` — SRT transport bindings.
//!
//! Mirrors the `tstrans.rtp` pattern — same GIL-release boundaries,
//! same bytes-like extraction, same error mapping shape.
//!
//! Submodules:
//! - `generated::stats_srt`: SrtStats (the shell classes live in `tstrans.shells`)
//! - `lowlevel`:             Socket, Listener, Builder
//! - `policy`:               ReconnectPolicy, BackoffStrategy, OverflowPolicy
//! - `managed_basic`:        ManagedSender, ManagedReceiver
//! - `managed_convenience`:  ManagedMuxSender, ManagedDemuxReceiver
//!
//! Error mapping goes through `crate::raise`: every Rust error
//! type in this surface has a `From<…> for tst_pipeline::binding::BindingError`
//! next to its own definition, and `raise` resolves the kind's `name()` on
//! `tstrans.exceptions.SrtErrorKind` — checked at `import tstrans`.

mod lowlevel;
pub(crate) mod managed_basic;
pub(crate) mod managed_convenience;
pub(crate) mod policy;

use pyo3::prelude::*;

pub(crate) fn register(parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(parent.py(), "srt")?;
    crate::generated::stats_srt::register(&m)?;
    lowlevel::register(&m)?;
    policy::register(&m)?;
    managed_basic::register(&m)?;
    managed_convenience::register(&m)?;
    parent.add_submodule(&m)?;
    Ok(())
}
