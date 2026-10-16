//! URL → opened transport, for every shell. The scheme picks the
//! transport crate's opener; the GIL is released around the open.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use tst_core::transport::{RecvTransport, Transport};
use tst_pipeline::binding::{BindingError, BindingErrorKind, Domain, Opened};

use crate::raise::raise;

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
    PyValueError::new_err(format!(
        "unsupported URL scheme in {url:?}; expected one of: srt, rtp, udp, tcp, tcps, rist (the schemes this build includes)"
    ))
}

/// `domain_of` already matched a built transport, so this is never reached.
fn unreachable_scheme<T>() -> Result<T, BindingError> {
    Err(BindingError::new(
        BindingErrorKind::Internal,
        "scheme checked above",
    ))
}

/// Open a send transport for a shell, with the GIL released.
pub(crate) fn open_send(py: Python<'_>, url: &str) -> PyResult<Opened<Box<dyn Transport>>> {
    let d = domain_of(url).ok_or_else(|| unsupported(url))?;
    let r = crate::util::allow_threads_parking(py, || match d {
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
    });
    r.map_err(|e| raise(py, &d, e))
}

/// Open a receive transport for a shell, with the GIL released. An `srt://`
/// open registers its accept slot so the interpreter-exit hook can unpark a
/// first accept that no caller holds a handle to yet.
pub(crate) fn open_recv(py: Python<'_>, url: &str) -> PyResult<Opened<Box<dyn RecvTransport>>> {
    let d = domain_of(url).ok_or_else(|| unsupported(url))?;
    let r = crate::util::allow_threads_parking(py, || match d {
        #[cfg(feature = "srt")]
        Domain::Srt => {
            let slot = std::sync::Arc::new(tst_core::cancel::CancelSlot::new());
            let _accept_guard = crate::util::register_accept_slot(&slot);
            tst_srt::opener::open_recv(url, &slot)
        }
        #[cfg(feature = "rtp")]
        Domain::Rtp => tst_rtp::opener::open_recv(url),
        #[cfg(feature = "udp")]
        Domain::Udp => tst_udp::opener::open_recv(url),
        #[cfg(feature = "tcp")]
        Domain::Tcp => tst_tcp::opener::open_recv(url),
        #[cfg(feature = "rist")]
        Domain::Rist => tst_rist::opener::open_recv(url),
        _ => unreachable_scheme(),
    });
    r.map_err(|e| raise(py, &d, e))
}
