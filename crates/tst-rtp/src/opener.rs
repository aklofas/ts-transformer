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

use crate::{RtpRecvTransport, RtpStatsHandle, RtpTransport};

/// Open an `rtp://` sender (unicast or multicast). `?pkt_size=` and
/// `?ssrc=` set the datagram size and the synchronization source.
///
/// # Errors
/// `RtpUrl` for a bad URL; the socket errors' kinds otherwise.
pub fn open_sender(url: &str) -> Result<Opened<Box<dyn Transport>>, BindingError> {
    let t = RtpTransport::connect(url)?;
    let cancel = cancel_of(&t);
    Ok(Opened::new(Box::new(t), cancel, Domain::Rtp))
}

/// Open an `rtp://` receiver bound to the URL's address (a multicast group
/// is joined), with the bound address in `Opened::local_addr` (the
/// kernel's port for a port-0 URL). The receive buffer sizes itself to the
/// transport's deliverable ceiling; `?pkt_size=` is rejected.
///
/// # Errors
/// `RtpUrl` for a bad URL; the socket errors' kinds otherwise.
pub fn open_recv(url: &str) -> Result<Opened<Box<dyn RecvTransport>>, BindingError> {
    open_recv_with_stats(url).map(|(opened, _)| opened)
}

/// [`open_recv`], plus a reader of the receiver's RTP counters taken
/// before the transport was boxed: a shell over the boxed transport cannot
/// reach them otherwise.
///
/// # Errors
/// As [`open_recv`].
pub fn open_recv_with_stats(
    url: &str,
) -> Result<(Opened<Box<dyn RecvTransport>>, RtpStatsHandle), BindingError> {
    let t = RtpRecvTransport::listen(url)?;
    let cancel = recv_cancel_of(&t);
    let stats = t.stats_handle();
    let local_addr = t.local_addr();
    let mut opened = Opened::new(Box::new(t) as Box<dyn RecvTransport>, cancel, Domain::Rtp);
    if let Some(addr) = local_addr {
        opened = opened.with_local_addr(addr);
    }
    Ok((opened, stats))
}
