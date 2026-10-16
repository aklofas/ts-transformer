//! URL openers for the language bindings' shells: parse the URL, open the
//! transport the way the URL says, and return it type-erased with its
//! cancel handle and error domain (`tst_pipeline::binding::Opened`). The
//! binding wraps it in `MuxSender` / `Sender` / `DemuxReceiver` /
//! `Receiver`; the same URL keys as every other entry point apply.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

use tst_core::transport::{RecvTransport, Transport};
use tst_pipeline::binding::opened::{cancel_of, recv_cancel_of};
use tst_pipeline::binding::{BindingError, Domain, Opened};

use crate::{RistRecvTransportBuilder, RistTransportBuilder};

/// Open a `rist://` sender.
///
/// # Errors
/// The URL and librist session errors' kinds.
pub fn open_sender(url: &str) -> Result<Opened<Box<dyn Transport>>, BindingError> {
    let t = RistTransportBuilder::new(url)?.connect()?;
    let cancel = cancel_of(&t);
    Ok(Opened::new(Box::new(t), cancel, Domain::Rist))
}

/// Open a `rist://@host:port` receiver.
///
/// # Errors
/// The URL and librist session errors' kinds.
pub fn open_recv(url: &str) -> Result<Opened<Box<dyn RecvTransport>>, BindingError> {
    let t = RistRecvTransportBuilder::new(url)?.listen()?;
    let cancel = recv_cancel_of(&t);
    Ok(Opened::new(Box::new(t), cancel, Domain::Rist))
}
