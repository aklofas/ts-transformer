//! URL openers for the language bindings' shells: parse the URL, open the
//! transport the way the URL says, and return it type-erased with its
//! cancel handle and error domain (`tst_pipeline::binding::Opened`). The
//! binding wraps it in `MuxSender` / `Sender` / `DemuxReceiver` /
//! `Receiver`; the same URL keys as every other entry point apply.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

use std::sync::Arc;

use tst_core::cancel::CancelSlot;
use tst_core::transport::{RecvTransport, Transport};
use tst_pipeline::binding::opened::{cancel_of, recv_cancel_of};
use tst_pipeline::binding::{BindingError, BindingErrorKind, Domain, Opened};

use crate::url::{Mode, SrtUrl};

/// Open an `srt://` sender: `?mode=caller` (the default) connects.
///
/// Dials through [`SrtUrl::connect_recv`]: the URL's keys only, no sender preset,
/// the way the bindings' plain senders always have.
///
/// # Errors
/// `ConfigInvalid` for another mode; the URL and connect errors' kinds otherwise.
pub fn open_sender(url: &str) -> Result<Opened<Box<dyn Transport>>, BindingError> {
    let parsed = SrtUrl::parse(url)?;
    if parsed.mode != Mode::Caller {
        return Err(BindingError::new(
            BindingErrorKind::ConfigInvalid,
            format!(
                "an srt:// sender requires ?mode=caller (default); got mode={:?}",
                parsed.mode
            ),
        ));
    }
    let t = parsed.connect_recv()?;
    let cancel = cancel_of(&t);
    Ok(Opened::new(Box::new(t), cancel, Domain::Srt))
}

/// Open an `srt://` receiver: `?mode=listener` binds and accepts one caller.
/// `accept` lets the caller (and a host's interpreter-exit hook) cancel the
/// accept before the shell exists.
///
/// # Errors
/// `ConfigInvalid` for another mode; the URL, bind and accept errors' kinds otherwise.
pub fn open_recv(
    url: &str,
    accept: &Arc<CancelSlot>,
) -> Result<Opened<Box<dyn RecvTransport>>, BindingError> {
    let parsed = SrtUrl::parse(url)?;
    if parsed.mode != Mode::Listener {
        return Err(BindingError::new(
            BindingErrorKind::ConfigInvalid,
            format!(
                "an srt:// receiver requires ?mode=listener; got mode={:?}",
                parsed.mode
            ),
        ));
    }
    let t = parsed.accept_one(accept)?;
    let cancel = recv_cancel_of(&t);
    Ok(Opened::new(Box::new(t), cancel, Domain::Srt))
}
