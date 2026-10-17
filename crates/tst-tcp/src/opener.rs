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

use crate::TcpTransportBuilder;
use crate::error::TcpError;

/// Open a `tcp://` or `tcps://` sender: connect as a caller (TLS for
/// `tcps://`).
///
/// # Errors
/// The URL, connect and TLS errors' kinds.
pub fn open_sender(url: &str) -> Result<Opened<Box<dyn Transport>>, BindingError> {
    let t = TcpTransportBuilder::from_url(url)
        .map_err(TcpError::Url)?
        .build()?;
    let cancel = cancel_of(&t);
    Ok(Opened::new(Box::new(t), cancel, Domain::Tcp))
}

/// Open a `tcp://` or `tcps://` receiver: connect as a caller and read.
/// The listening side stays on [`TcpListener`](crate::TcpListener).
///
/// # Errors
/// The URL, connect and TLS errors' kinds.
pub fn open_recv(url: &str) -> Result<Opened<Box<dyn RecvTransport>>, BindingError> {
    let t = TcpTransportBuilder::from_url(url)
        .map_err(TcpError::Url)?
        .build()?;
    let cancel = recv_cancel_of(&t);
    Ok(Opened::new(Box::new(t), cancel, Domain::Tcp))
}
