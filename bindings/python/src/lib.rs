//! PyO3 bindings for the ts-transformer Rust workspace.
//!
//! The compiled cdylib is imported from Python as `tstrans._native`;
//! the top-level `tstrans/__init__.py` re-exports the public surface
//! into submodules so users `from tstrans.mpegts import Muxer`.
//!
//! Exports `__version__`, the exception handles, and the PyClass
//! wrappers for the `mpegts`, `klv`, and `codec` submodules.

mod codec;
mod errors;
mod klv;
mod mpegts;
mod mux;
mod raise;
mod raw_bytes;
mod util;
#[cfg(feature = "rtp")]
mod rtp;
#[cfg(feature = "srt")]
mod srt;
// udp / tcp / hls / rist transport bindings (default-on).
#[cfg(feature = "udp")]
mod udp;
#[cfg(feature = "tcp")]
mod tcp;
#[cfg(feature = "hls")]
mod hls;
#[cfg(feature = "rist")]
mod rist;
#[cfg(feature = "pipeline")]
mod pipeline;

use pyo3::prelude::*;

/// `TSTRANS_LOG` bridge (Task C7) — the Rust core (`tst-core`/`tst-rtp`/
/// `tst-pipeline`) emits `tracing` events, but embedding them in a
/// Python process installs no subscriber, so every `info!`/`warn!`/
/// `debug!` call is silently discarded — a field integrator lost a
/// diagnosis day to exactly this. Iff `TSTRANS_LOG` is set at import
/// time, install a stderr subscriber filtered by its value (the same
/// syntax as `RUST_LOG`/`EnvFilter`); unset ⇒ install nothing, so a
/// bare `import tstrans` has zero subscriber overhead beyond the env
/// lookup. `try_init` (not `init`) so an embedding process that already
/// installed its own subscriber keeps it — this bridge never displaces
/// one. ANSI color codes are gated on `stderr` actually being a
/// terminal (`std::io::IsTerminal`, stable since 1.85) — a piped or
/// redirected stderr (log files, `subprocess.run(capture_output=True)`)
/// gets plain text, no stray escape sequences.
fn init_tracing_bridge() {
    use std::io::IsTerminal;

    if let Ok(filter) = std::env::var("TSTRANS_LOG") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_writer(std::io::stderr)
            .with_ansi(std::io::stderr().is_terminal())
            .try_init();
    }
}

// `_py` is prefixed because it is not used directly in this
// registration shell. CI runs `clippy -D warnings` so an unprefixed
// unused parameter would fail the workspace.
#[pymodule]
fn _native(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    init_tracing_bridge();
    // Spec Arc 2 §3.3: a kind member the Python enum lacks fails `import tstrans`.
    raise::check_error_kinds(_py)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_function(wrap_pyfunction!(raise::check_error_kinds_py, m)?)?;
    m.add_function(wrap_pyfunction!(errors::raise_mux_error_for_test, m)?)?;
    #[cfg(feature = "srt")]
    {
        m.add_function(wrap_pyfunction!(errors::raise_srt_error_for_test, m)?)?;
    }
    #[cfg(feature = "udp")]
    {
        m.add_function(wrap_pyfunction!(errors::raise_udp_error_for_test, m)?)?;
    }
    #[cfg(feature = "tcp")]
    {
        m.add_function(wrap_pyfunction!(errors::raise_tcp_error_for_test, m)?)?;
    }
    #[cfg(feature = "hls")]
    {
        m.add_function(wrap_pyfunction!(errors::raise_hls_error_for_test, m)?)?;
    }
    #[cfg(feature = "rist")]
    {
        m.add_function(wrap_pyfunction!(errors::raise_rist_error_for_test, m)?)?;
    }
    mpegts::register(m)?;
    klv::register(m)?;
    // Lazy `.raw` holder backing DemuxEvent.Video/.Audio.
    m.add_class::<crate::raw_bytes::RawBytes>()?;
    // Stream handle newtypes. Registered here (not in mux::register)
    // because they were the first mux surface to land in src/mux.rs.
    m.add_class::<crate::mux::PyVideoStreamHandle>()?;
    m.add_class::<crate::mux::PyAudioStreamHandle>()?;
    m.add_class::<crate::mux::PyKlvStreamHandle>()?;
    m.add_class::<crate::mux::PySubtitleStreamHandle>()?;
    m.add_class::<crate::mux::PyDataStreamHandle>()?;
    // Program-level config + builder.
    m.add_class::<crate::mux::PyMuxerProgramConfig>()?;
    m.add_class::<crate::mux::PyMuxerProgramConfigBuilder>()?;
    // Top-level muxer config + builder.
    m.add_class::<crate::mux::PyMuxerConfig>()?;
    m.add_class::<crate::mux::PyMuxerConfigBuilder>()?;
    // Muxer base (init + pull + pending + capacity).
    m.add_class::<crate::mux::PyMuxer>()?;
    // MuxerStats snapshot (StreamCodecStats is pure Python —
    // constructed by `Muxer.stream_codec_stats` per call).
    m.add_class::<crate::mux::PyMuxerStats>()?;
    // codec submodule — shared types, NalUnit, Obu, and per-codec
    // PyClasses.
    codec::register(m)?;
    // rtp submodule — RTP + RTSP bindings.
    #[cfg(feature = "rtp")]
    rtp::register(m)?;
    // srt submodule — SRT transport bindings.
    #[cfg(feature = "srt")]
    srt::register(m)?;
    // udp / tcp / hls / rist submodules.
    #[cfg(feature = "udp")]
    udp::register(m)?;
    #[cfg(feature = "tcp")]
    tcp::register(m)?;
    #[cfg(feature = "hls")]
    hls::register(m)?;
    #[cfg(feature = "rist")]
    rist::register(m)?;
    // pipeline submodule — the ext::pairing PairingDemuxer composite,
    // exposed as tstrans.pipeline.Pairer.
    #[cfg(feature = "pipeline")]
    pipeline::register(m)?;
    Ok(())
}
