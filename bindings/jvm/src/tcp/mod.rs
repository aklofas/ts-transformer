//! JNI exports for `org.tstrans.tcp.Listener` and the connection it accepts,
//! `org.tstrans.tcp.Transport`.
//!
//! The listener is an [`OwnedRegistry`] entry: `nAccept` holds its slot while
//! it is parked, so `nLocalPort` reads the port captured at open and
//! `nCancelHandle` the listener's own cancel handle, neither of which waits
//! for the slot. `nClose` cancels first, then takes the listener.
//!
//! An accepted connection has no parked call of its own, so it sits in a plain
//! [`HandleRegistry`] with no cancel hook: a native that turns it into a shell
//! takes it out with `REGISTRY_TRANSPORT.close` without cancelling it.

use crate::panic::jni_native;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock};

use jni::objects::JString;
use jni::sys::{jint, jlong, jstring};
use tst_core::transport::TransportCancel;
use tst_pipeline::binding::view::BlockingReason;
use tst_pipeline::binding::{BindingError, BindingErrorKind, Domain, Owned};
use tst_tcp::error::TcpError;
use tst_tcp::{TcpListener, TcpListenerBuilder, TcpTransport};

use crate::error::{throw_binding, throw_closed};
use crate::handle::{HandleRegistry, OwnedRegistry};
use crate::shells::open::read_url;
use crate::shells::throw_state;

/// What `Listener` reads without the slot a parked accept holds: the bound
/// port, `None` when the address could not be read at open.
struct ListenerSnapshot {
    local_port: Option<u16>,
}

static REGISTRY_LISTENER: LazyLock<OwnedRegistry<TcpListener, ListenerSnapshot>> =
    LazyLock::new(OwnedRegistry::new);

/// An accepted connection and its peer. The peer sits beside the connection so
/// `peerAddr()` answers from it alone.
pub(crate) struct Accepted {
    // Held for its socket; only a conversion into a shell reads it.
    #[expect(dead_code, reason = "no native converts an accepted connection yet")]
    pub(crate) conn: TcpTransport,
    pub(crate) peer: SocketAddr,
}

pub(crate) static REGISTRY_TRANSPORT: LazyLock<HandleRegistry<Accepted>> =
    LazyLock::new(HandleRegistry::new);

jni_native! {
    fn Java_org_tstrans_tcp_Listener_nOpen(env, _class, url: JString<'_>) -> jlong = 0 => {
        let Some(url) = read_url(env, &url) else {
            return 0;
        };
        let built = TcpListenerBuilder::from_url(&url)
            .map_err(TcpError::Url)
            .and_then(|b| b.build());
        match built {
            Ok(listener) => {
                let cancel: Arc<dyn TransportCancel> = Arc::new(listener.cancel_handle());
                let snapshot = ListenerSnapshot {
                    local_port: listener.local_addr().ok().map(|a| a.port()),
                };
                REGISTRY_LISTENER.insert(Owned::new(listener, cancel, snapshot)) as jlong
            }
            Err(e) => {
                throw_binding(env, Domain::Tcp, &BindingError::from(e));
                0
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_tcp_Listener_nAccept(env, _class, handle: jlong) -> jlong = 0 => {
        // `accept_blocking` polls the listener's alive flag, so the cancel that
        // `close` fires first wakes it and releases the slot for the take.
        let result = REGISTRY_LISTENER.with_ref_blocking(
            handle as u64,
            BlockingReason::IsItselfBlocking,
            TcpListener::accept_blocking,
        );
        match result {
            Ok(Ok(conn)) => {
                let peer = conn.peer();
                REGISTRY_TRANSPORT.insert(Accepted { conn, peer }) as jlong
            }
            Ok(Err(e)) => {
                throw_binding(env, Domain::Tcp, &BindingError::from(e));
                0
            }
            Err(state) => {
                throw_state(env, Domain::Tcp, "Listener", &state);
                0
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_tcp_Listener_nLocalPort(env, _class, handle: jlong) -> jint = -1 => {
        match REGISTRY_LISTENER.snapshot(handle as u64, |s| s.local_port) {
            Some(Some(port)) => jint::from(port),
            Some(None) => {
                throw_binding(
                    env,
                    Domain::Tcp,
                    &BindingError::new(
                        BindingErrorKind::TcpIo,
                        "local port unavailable: the bound address could not be read at open",
                    ),
                );
                -1
            }
            None => {
                throw_closed(env, "Listener");
                -1
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_tcp_Listener_nCancelHandle(_env, _class, handle: jlong) -> jlong = 0 => {
        REGISTRY_LISTENER
            .cancel_view(handle as u64)
            .map_or(0, crate::shells::cancel::cancel_view_handle)
    }
}

jni_native! {
    fn Java_org_tstrans_tcp_Listener_nClose(_env, _class, handle: jlong) = () => {
        // Dropping the listener closes its socket, so a connection still in the
        // kernel's queue is dropped instead of left waiting.
        if let Some(listener) = REGISTRY_LISTENER.close(handle as u64) {
            listener.close();
        }
    }
}

jni_native! {
    fn Java_org_tstrans_tcp_Transport_nPeerAddr(
        env,
        _class,
        handle: jlong,
    ) -> jstring = std::ptr::null_mut() => {
        let Some(peer) = REGISTRY_TRANSPORT.with(handle as u64, |a| a.peer) else {
            throw_closed(env, "Transport");
            return std::ptr::null_mut();
        };
        match env.new_string(peer.to_string()) {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    }
}

jni_native! {
    fn Java_org_tstrans_tcp_Transport_nClose(_env, _class, handle: jlong) = () => {
        // Dropping the connection closes its socket.
        drop(REGISTRY_TRANSPORT.close(handle as u64));
    }
}
