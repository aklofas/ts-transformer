//! [`Opened<T>`]: what a transport crate's URL opener hands a binding — the
//! transport (type-erased by the binding's choice of `T`), the transport's
//! own cancel handle, taken before it was boxed, and the error [`Domain`]
//! the binding raises the shell's failures in.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

use alloc::sync::Arc;
use tst_core::cancel::FlagCancel;
use tst_core::transport::{RecvTransport, Transport, TransportCancel};

use super::Domain;

/// An opened transport, ready to be wrapped in a shell.
#[non_exhaustive]
pub struct Opened<T> {
    /// The transport (`Box<dyn Transport>` / `Box<dyn RecvTransport>` from
    /// the openers).
    pub transport: T,
    /// The transport's cancel handle, lock-free; what `Owned::new` takes.
    pub cancel: Arc<dyn TransportCancel>,
    /// The error domain of the transport: which host exception class a
    /// shell over it raises for a cross-domain kind (`Closed`, `Broken`, …).
    pub domain: Domain,
}

impl<T> Opened<T> {
    /// Bundle an opened transport.
    pub fn new(transport: T, cancel: Arc<dyn TransportCancel>, domain: Domain) -> Self {
        Self {
            transport,
            cancel,
            domain,
        }
    }
}

/// Resolve an optional cancel handle (what `cancel_handle()` returns on a
/// transport or a shell): `None` becomes a [`FlagCancel`], a plain latch
/// that records the request and wakes nothing.
#[must_use]
pub fn cancel_or_latch(
    c: Option<Arc<dyn TransportCancel + Send + Sync>>,
) -> Arc<dyn TransportCancel> {
    match c {
        Some(c) => c,
        None => Arc::new(FlagCancel::new()),
    }
}

/// A send transport's cancel handle, read before the transport is boxed;
/// [`cancel_or_latch`] when it has none.
#[must_use]
pub fn cancel_of<T: Transport + ?Sized>(t: &T) -> Arc<dyn TransportCancel> {
    cancel_or_latch(Transport::cancel_handle(t))
}

/// A receive transport's cancel handle, read before the transport is
/// boxed; [`cancel_or_latch`] when it has none.
#[must_use]
pub fn recv_cancel_of<T: RecvTransport + ?Sized>(t: &T) -> Arc<dyn TransportCancel> {
    cancel_or_latch(RecvTransport::cancel_handle(t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tst_core::transport::TransportError;

    /// A transport with no cancel handle of its own.
    struct NoCancel;

    impl Transport for NoCancel {
        fn send_bytes(&mut self, _msg: &[u8]) -> Result<(), TransportError> {
            Ok(())
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn is_alive(&self) -> bool {
            true
        }
        fn close(&mut self) {}
    }

    #[test]
    fn a_missing_handle_becomes_a_clear_latch() {
        let c = cancel_of(&NoCancel);
        assert!(!c.is_cancelled());
        c.cancel();
        assert!(c.is_cancelled());
    }

    #[test]
    fn a_real_handle_passes_through() {
        let real: Arc<dyn TransportCancel + Send + Sync> = Arc::new(FlagCancel::new());
        let c = cancel_or_latch(Some(real.clone()));
        c.cancel();
        assert!(
            real.is_cancelled(),
            "the resolved handle is the transport's own"
        );
    }
}
