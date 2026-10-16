//! [`Opened<T>`]: what a transport crate's URL opener hands a binding — the
//! transport (type-erased by the binding's choice of `T`), the transport's
//! own cancel handle, taken before it was boxed, and the error [`Domain`]
//! the binding raises the shell's failures in.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

use alloc::sync::Arc;
use tst_core::transport::TransportCancel;

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
