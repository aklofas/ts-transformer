//! `tstrans.srt` Rust→Python error mapping helpers.
//!
//! Centralized exhaustive mappings from every Rust enum that can flow
//! through a `tstrans.srt` code path to one of the 8 `SrtErrorKind`
//! variants declared in `tstrans.exceptions`. Each public helper builds
//! an `SrtError` via `crate::errors::make_srt_error`.
//!
//! Routing rules (one paragraph each):
//!
//! - **`UrlError`** — every variant collapses to `CONFIG_INVALID`. URL
//!   parse failures are caller-misconfiguration by definition; the
//!   variant detail is preserved in the free-text message via
//!   `Display`.
//! - **`ConnectError`** — `InvalidAddress` / `InvalidOption` →
//!   `CONFIG_INVALID` (caller supplied a bad address or option);
//!   `TimedOut` → `TIMEOUT`; everything else (`Refused`, `BadEncryption`,
//!   `Rejected`, `System`, `Other` + future `#[non_exhaustive]`) →
//!   `CONNECT_FAILED`.
//! - **`BindError`** — `InvalidAddress` / `InvalidOption` →
//!   `CONFIG_INVALID`; everything else (`AddressInUse`,
//!   `PermissionDenied`, `System`, `Other` + future variants) →
//!   `CONNECT_FAILED`. The listener failed to come up, which the
//!   user-facing API treats as a connect-side failure.
//! - **`AcceptError`** — `TimedOut` → `TIMEOUT`; `ListenerClosed` →
//!   `CLOSED`; everything else (`PeerRejected`, `System`, `Other` +
//!   future variants) → `ACCEPT_FAILED`. Kept distinct from
//!   `CONNECT_FAILED` so callers can tell "I bound but the peer broke
//!   things" apart from "I could not bind at all".
//! - **`IoError`** — `SocketClosed` → `CLOSED`; everything else
//!   (`System(io::Error)`, `Other`, future variants) → `IO`.
//! - **`TransportError`** — `Backpressure` → `WOULD_BLOCK`; `Broken` →
//!   `BROKEN`; `Closed` / `ExplicitClose` → `CLOSED`; `TooLarge` →
//!   `CONFIG_INVALID` (the cap is a configurable payload size, so the
//!   caller can tune it); future `#[non_exhaustive]` additions → `IO`.
//!
//! The consolidated `scripts/check/python/error-mapping-coverage.sh`
//! ratchet verifies every `SrtErrorKind` variant has at least one
//! `make_srt_error(py, "<KIND>", ...)` call site under
//! `bindings/python/src/`. The single-line kind literal is required
//! by the line-based grep — multi-line wraps will not match.

use pyo3::prelude::*;

use tst_core::transport::TransportError;
use tst_srt::UrlError;

use crate::errors::make_srt_error;

/// Map a `tst_srt::UrlError` (raised by `SrtUrl::parse` and friends) to
/// a `tstrans.exceptions.SrtError` with kind `CONFIG_INVALID`.
///
/// `UrlError` is `#[non_exhaustive]`; the single arm catches every
/// current variant (`Syntax`, `WrongScheme`, `MissingPort`,
/// `MissingHost`, `UserinfoNotSupported`, `UnsupportedMode`,
/// `UnsupportedKey`, `FfmpegAliasNotExposed`, `UnknownKey`,
/// `InvalidValue`, `OptionValidation`) AND any future addition — they
/// are all caller-misconfiguration by definition.
pub(crate) fn url_error_to_pyerr(py: Python<'_>, e: UrlError) -> PyErr {
    make_srt_error(py, "CONFIG_INVALID", &e.to_string())
}

/// Map a `tst_core::transport::TransportError` (the unified transport
/// failure surface used by `tst_pipeline::Sender` / `Receiver`) to a
/// `tstrans.exceptions.SrtError`.
///
/// `Backpressure` is the only variant a polling caller would
/// reasonably retry; `Broken` requires re-establishing the transport.
/// `Closed` and `ExplicitClose` both surface as `CLOSED` — the
/// distinction (peer-EOS vs caller-initiated close) is not exposed at
/// the SRT Python surface today.
pub(crate) fn transport_error_to_pyerr(py: Python<'_>, e: TransportError) -> PyErr {
    match e {
        TransportError::Backpressure { msg, .. } => make_srt_error(py, "WOULD_BLOCK", &msg),
        TransportError::Broken { msg, .. } => make_srt_error(py, "BROKEN", &msg),
        TransportError::Closed => make_srt_error(py, "CLOSED", "transport closed"),
        TransportError::ExplicitClose => make_srt_error(py, "CLOSED", "transport explicit close"),
        TransportError::TooLarge { len, max } => {
            let msg = format!("payload too large: {len} bytes exceeds {max}-byte cap");
            make_srt_error(py, "CONFIG_INVALID", &msg)
        }
        // Catch-all for future #[non_exhaustive] additions (e.g.
        // `Cancelled` once Plan B lands). Surface as IO so the kind
        // is at least categorized; the message preserves the variant.
        other => make_srt_error(py, "IO", &other.to_string()),
    }
}
