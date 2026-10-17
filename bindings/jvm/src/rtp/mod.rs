//! `org.tstrans.rtp` — RTP transport JNI surface.

pub(crate) mod errors;
mod client;
pub(crate) mod h264_receiver;
pub(crate) mod publish;
mod server;
mod transport;

use std::sync::Arc;

use jni::sys::jlong;
use tst_core::transport::{RecvTransport, TransportCancel};
use tst_pipeline::binding::{BindingError, BindingErrorKind, Domain, Opened};
use tst_rtp::RtpRecvTransport;

/// The rtp transports expose their cancel only through the trait's `Option`
/// (no inherent non-`Option` accessor exists on `RtpTransport` /
/// `RtpRecvTransport`, unlike `SrtTransport::srt_cancel_handle`). A `None`
/// would be a tst-rtp bug, reported as `Internal` at open — never an `expect`.
pub(crate) fn rtp_cancel(
    c: Option<Arc<dyn TransportCancel + Send + Sync>>,
    what: &str,
) -> Result<Arc<dyn TransportCancel>, BindingError> {
    match c {
        Some(c) => Ok(c),
        None => Err(BindingError::new(
            BindingErrorKind::Internal,
            format!("{what} exposes no cancel handle"),
        )),
    }
}

/// Wrap the SETUP-time receive transport an RTSP session hands over in a
/// `DemuxReceiver` shell (default or explicit demux options) and register it.
/// A transport with no cancel handle is a tst-rtp bug, reported as `Internal`.
pub(crate) fn demux_receiver_handle_from_transport(
    transport: RtpRecvTransport,
    opts: Option<tst_core::mpegts::demux::DemuxerConfig>,
) -> Result<jlong, BindingError> {
    let cancel = rtp_cancel(RecvTransport::cancel_handle(&transport), "RtpRecvTransport")?;
    Ok(crate::shells::demux_receiver::register(
        Opened::new(Box::new(transport), cancel, Domain::Rtp),
        opts,
    ))
}
