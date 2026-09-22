//! [`Close`] for the pipeline shells, and for raw transports via
//! [`SendHalf`] / [`RecvHalf`].
//!
//! Every shell's own `close()` is infallible today (returns `()`), so each
//! impl has `Error = Infallible`; the associated type exists for a shell
//! that grows a fallible drain-on-close later. Raw transports implement
//! both transport traits (SRT does), so there is no blanket impl — a
//! binding wraps the transport as the half its object exposes.

use core::convert::Infallible;

use tst_core::transport::{RecvTransport, Transport};

use super::owned::Close;
use crate::{
    DemuxReceiver, ManagedDemuxReceiver, MuxSender, RawReceiver, RawSender, Receiver, Sender,
};

impl<T: Transport> Close for Sender<T> {
    type Error = Infallible;
    fn close(&mut self) -> Result<(), Infallible> {
        Sender::close(self);
        Ok(())
    }
}

impl<T: Transport> Close for RawSender<T> {
    type Error = Infallible;
    fn close(&mut self) -> Result<(), Infallible> {
        RawSender::close(self);
        Ok(())
    }
}

impl<T: Transport> Close for MuxSender<T> {
    type Error = Infallible;
    fn close(&mut self) -> Result<(), Infallible> {
        // `MuxSender::close` takes `&self` (it cancels first through its own
        // slot); the `&mut self` here simply reborrows.
        MuxSender::close(self);
        Ok(())
    }
}

impl<R: RecvTransport> Close for Receiver<R> {
    type Error = Infallible;
    fn close(&mut self) -> Result<(), Infallible> {
        Receiver::close(self);
        Ok(())
    }
}

impl<R: RecvTransport> Close for RawReceiver<R> {
    type Error = Infallible;
    fn close(&mut self) -> Result<(), Infallible> {
        RawReceiver::close(self);
        Ok(())
    }
}

impl<R: RecvTransport> Close for DemuxReceiver<R> {
    type Error = Infallible;
    fn close(&mut self) -> Result<(), Infallible> {
        DemuxReceiver::close(self);
        Ok(())
    }
}

impl<R: RecvTransport> Close for ManagedDemuxReceiver<R> {
    type Error = Infallible;
    fn close(&mut self) -> Result<(), Infallible> {
        ManagedDemuxReceiver::close(self);
        Ok(())
    }
}

/// A raw transport held by a binding object that exposes its SEND side.
/// `Close::close` is [`Transport::close`]. The field is `pub` so the
/// binding reaches the transport with one deref inside `with_mut`.
///
/// Two reasons this exists rather than a blanket impl or a binding-side
/// one. The binding crates cannot write `impl Close for SrtTransport` at
/// all — trait and type are both foreign to them, so the orphan rule
/// forbids it, which is why the impls live in this crate. And a blanket
/// `impl<T: Transport> Close for T` here would overlap one over
/// `RecvTransport`, because a transport may implement both (`SrtTransport`
/// does). So the binding wraps the transport as the half its object
/// exposes: `Owned<SendHalf<SrtTransport>>`.
pub struct SendHalf<T>(pub T);

impl<T: Transport> Close for SendHalf<T> {
    type Error = Infallible;
    fn close(&mut self) -> Result<(), Infallible> {
        self.0.close();
        Ok(())
    }
}

/// A raw transport held by a binding object that exposes its RECEIVE
/// side. `Close::close` is [`RecvTransport::close`] — which has a default
/// empty body, so this is a no-op for a transport that does not override
/// it. See [`SendHalf`] for why the halves exist at all (orphan rule +
/// overlapping blankets).
pub struct RecvHalf<R>(pub R);

impl<R: RecvTransport> Close for RecvHalf<R> {
    type Error = Infallible;
    fn close(&mut self) -> Result<(), Infallible> {
        self.0.close();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tst_core::transport::{TransportCancel, TransportError};

    use crate::binding::{FlagCancel, Owned};
    use crate::{MuxSender, ReceiverConfig, SenderConfig};

    /// Send-side mock that records its close.
    struct Sink(Arc<AtomicBool>);
    impl Transport for Sink {
        fn send_bytes(&mut self, _b: &[u8]) -> Result<(), TransportError> {
            Ok(())
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn close(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
        fn is_alive(&self) -> bool {
            !self.0.load(Ordering::SeqCst)
        }
    }

    /// Recv-side mock that records its close.
    struct Source(Arc<AtomicBool>);
    impl RecvTransport for Source {
        fn recv_bytes(&mut self, _b: &mut [u8]) -> Result<usize, TransportError> {
            Err(TransportError::Closed)
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn close(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
        fn is_alive(&self) -> bool {
            !self.0.load(Ordering::SeqCst)
        }
    }

    fn cancel() -> Arc<dyn TransportCancel> {
        Arc::new(FlagCancel::new())
    }

    #[test]
    fn owned_close_reaches_the_sender_shell_and_its_transport() {
        let closed = Arc::new(AtomicBool::new(false));
        let owned = Owned::new(
            Sender::new(Sink(Arc::clone(&closed)), SenderConfig::default()),
            cancel(),
            (),
        );
        assert!(owned.close().is_ok());
        assert!(
            closed.load(Ordering::SeqCst),
            "Sender::close closed its transport (sender/mod.rs:426)"
        );
        assert!(owned.is_closed());
    }

    #[test]
    fn owned_close_reaches_the_receiver_shell_and_its_transport() {
        let closed = Arc::new(AtomicBool::new(false));
        let owned = Owned::new(
            Receiver::new(Source(Arc::clone(&closed)), ReceiverConfig::default()),
            cancel(),
            (),
        );
        assert!(owned.close().is_ok());
        assert!(
            closed.load(Ordering::SeqCst),
            "Receiver::close closed its transport (receiver/mod.rs:385)"
        );
        assert!(owned.is_closed());
    }

    #[test]
    fn owned_close_reaches_the_mux_sender_shell_and_its_transport() {
        // `MuxSender::close` takes `&self`, unlike the other six shells'
        // `&mut self` — this pins that the reborrow in its `Close` impl
        // still reaches the transport.
        use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};

        let closed = Arc::new(AtomicBool::new(false));
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x1011, VideoCodec::H264);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().expect("valid single-program config")
        };
        let owned = Owned::new(
            MuxSender::new(Sink(Arc::clone(&closed)), cfg).expect("MuxSender opens"),
            cancel(),
            (),
        );
        assert!(owned.close().is_ok());
        assert!(
            closed.load(Ordering::SeqCst),
            "MuxSender::close closed its transport (mux_sender.rs:1017)"
        );
        assert!(owned.is_closed());
    }

    #[test]
    fn halves_close_the_raw_transport_and_expose_it() {
        let s = Arc::new(AtomicBool::new(false));
        let r = Arc::new(AtomicBool::new(false));
        let send = Owned::new(SendHalf(Sink(Arc::clone(&s))), cancel(), ());
        let recv = Owned::new(RecvHalf(Source(Arc::clone(&r))), cancel(), ());
        assert_eq!(
            send.with_mut(|h| h.0.send_bytes(b"x").is_ok()),
            Ok(true),
            "the half is one deref from the transport"
        );
        assert!(send.close().is_ok() && recv.close().is_ok());
        assert!(s.load(Ordering::SeqCst) && r.load(Ordering::SeqCst));
    }
}
