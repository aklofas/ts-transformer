//! URL → opened transport, for every shell. The scheme picks the
//! transport crate's opener; the GIL is released around the open.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use tst_core::transport::{RecvTransport, Transport};
use tst_core::url::redact_url;
use tst_pipeline::binding::{
    BindingError, BindingErrorKind, Domain, OpenCancel, Opened, SHELL_SCHEMES,
};

use crate::raise::raise;
use crate::shells::open_cancel::{begin, closed, finish, refuse_config};

/// The transports this build includes.
const BUILT: &[Domain] = &[
    #[cfg(feature = "srt")]
    Domain::Srt,
    #[cfg(feature = "rtp")]
    Domain::Rtp,
    #[cfg(feature = "udp")]
    Domain::Udp,
    #[cfg(feature = "tcp")]
    Domain::Tcp,
    #[cfg(feature = "rist")]
    Domain::Rist,
];

/// The domain of `url`'s scheme, when this build includes its transport.
fn domain_of(url: &str) -> Option<Domain> {
    Domain::of_scheme(url.split_once("://")?.0).filter(|d| BUILT.contains(d))
}

fn unsupported(url: &str) -> PyErr {
    let built: Vec<&str> = SHELL_SCHEMES
        .iter()
        .filter(|(_, d)| BUILT.contains(d))
        .map(|(s, _)| *s)
        .collect();
    PyValueError::new_err(format!(
        "unsupported URL scheme in {:?}; expected one of: {} (the schemes this build includes)",
        redact_url(url),
        built.join(", ")
    ))
}

/// `domain_of` already matched a built transport, so this is never reached.
fn unreachable_scheme<T>() -> Result<T, BindingError> {
    Err(BindingError::new(
        BindingErrorKind::Internal,
        "scheme checked above",
    ))
}

/// Open a send transport for a shell, with the GIL released. `oc` is the
/// open's cancel object (see [`crate::shells::open_cancel`]): a sender
/// takes no slot, so only a cancel before or during the connect is seen,
/// as `CLOSED` once it ends.
///
/// The checks run in one order for every open: the scheme (`ValueError`),
/// the cancelled object (`CLOSED`), then `config` (the shell's
/// configuration, built with the GIL held), then the URL, then the connect.
pub(crate) fn open_send<C>(
    py: Python<'_>,
    url: &str,
    oc: &OpenCancel,
    config: impl FnOnce() -> PyResult<C>,
) -> PyResult<(Opened<Box<dyn Transport>>, C)> {
    let d = domain_of(url).ok_or_else(|| unsupported(url))?;
    let slot = begin(oc).ok_or_else(|| raise(py, &d, closed()))?;
    let config = match config() {
        Ok(c) => c,
        Err(e) => return Err(refuse_config(py, &d, slot, e)),
    };
    let r = crate::util::allow_threads_parking(py, || {
        let r = match d {
            #[cfg(feature = "srt")]
            Domain::Srt => tst_srt::opener::open_sender(url),
            #[cfg(feature = "rtp")]
            Domain::Rtp => tst_rtp::opener::open_sender(url),
            #[cfg(feature = "udp")]
            Domain::Udp => tst_udp::opener::open_sender(url),
            #[cfg(feature = "tcp")]
            Domain::Tcp => tst_tcp::opener::open_sender(url),
            #[cfg(feature = "rist")]
            Domain::Rist => tst_rist::opener::open_sender(url),
            _ => unreachable_scheme(),
        };
        finish(slot, r)
    });
    r.map(|o| (o, config)).map_err(|e| raise(py, &d, e))
}

/// The RTP counters' handle of a receive transport, which a shell reads
/// without its slot.
#[cfg(feature = "rtp")]
pub(crate) type RtpStatsSource = Option<tst_rtp::RtpStatsHandle>;
/// No build without `rtp` has RTP counters.
#[cfg(not(feature = "rtp"))]
pub(crate) type RtpStatsSource = Option<std::convert::Infallible>;

/// A receive open and the RTP counters of an `rtp://` transport, taken
/// before the transport is boxed (a boxed transport cannot be asked).
pub(crate) struct OpenedRecv {
    pub(crate) opened: Opened<Box<dyn RecvTransport>>,
    pub(crate) rtp_stats: RtpStatsSource,
}

impl OpenedRecv {
    /// A transport with no RTP counters.
    pub(crate) fn plain(opened: Opened<Box<dyn RecvTransport>>) -> Self {
        Self {
            opened,
            rtp_stats: None,
        }
    }
}

/// Open a receive transport for a shell, with the GIL released. An `srt://`
/// open hands the slot `oc` gave it to its first accept and registers it,
/// so both `oc.cancel()` and the interpreter-exit hook end a first accept
/// that no caller holds a handle to yet. The checks run in
/// [`open_send`]'s order, `config` before the URL.
pub(crate) fn open_recv<C>(
    py: Python<'_>,
    url: &str,
    oc: &OpenCancel,
    config: impl FnOnce() -> PyResult<C>,
) -> PyResult<(OpenedRecv, C)> {
    let d = domain_of(url).ok_or_else(|| unsupported(url))?;
    let slot = begin(oc).ok_or_else(|| raise(py, &d, closed()))?;
    let config = match config() {
        Ok(c) => c,
        Err(e) => return Err(refuse_config(py, &d, slot, e)),
    };
    let r = crate::util::allow_threads_parking(py, || {
        let r = match d {
            #[cfg(feature = "srt")]
            Domain::Srt => {
                let _accept_guard = crate::util::register_accept_slot(&slot);
                tst_srt::opener::open_recv(url, &slot).map(OpenedRecv::plain)
            }
            #[cfg(feature = "rtp")]
            Domain::Rtp => {
                tst_rtp::opener::open_recv_with_stats(url).map(|(opened, h)| OpenedRecv {
                    opened,
                    rtp_stats: Some(h),
                })
            }
            #[cfg(feature = "udp")]
            Domain::Udp => tst_udp::opener::open_recv(url).map(OpenedRecv::plain),
            #[cfg(feature = "tcp")]
            Domain::Tcp => tst_tcp::opener::open_recv(url).map(OpenedRecv::plain),
            #[cfg(feature = "rist")]
            Domain::Rist => tst_rist::opener::open_recv(url).map(OpenedRecv::plain),
            _ => unreachable_scheme(),
        };
        finish(slot, r)
    });
    r.map(|o| (o, config)).map_err(|e| raise(py, &d, e))
}
