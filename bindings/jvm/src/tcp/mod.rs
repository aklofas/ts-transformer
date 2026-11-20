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
//! takes it out with `REGISTRY_TRANSPORT.close` without cancelling it, and the
//! shell's cancel handle is the connection's own, read before it is boxed.

use crate::panic::jni_native;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock};

use jni::objects::JString;
use jni::sys::{jboolean, jbyteArray, jint, jlong};
use tst_core::transport::{RecvTransport, Transport, TransportCancel};
use tst_pipeline::binding::opened::{cancel_of, recv_cancel_of};
use tst_pipeline::binding::view::BlockingReason;
use tst_pipeline::binding::{BindingError, BindingErrorKind, Domain, Opened, Owned};
use tst_tcp::error::TcpError;
use tst_tcp::{TcpListener, TcpListenerBuilder, TcpTransport};

use crate::error::{throw_binding, throw_closed};
use crate::handle::{HandleRegistry, OwnedRegistry};
use crate::jutil::socket_addr_array;
use crate::mpegts::build_demux_config_from_args;
use crate::shells;
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
        // A key already gone is a listener closed before this call, the shells'
        // rule; only a close or cancel that lands while the accept holds the
        // slot is the transport's `CLOSED` (`throw_state` below).
        if REGISTRY_LISTENER.snapshot(handle as u64, |_| ()).is_none() {
            throw_closed(env, "Listener");
            return 0;
        }
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
    fn Java_org_tstrans_tcp_Listener_nCancelHandle(env, _class, handle: jlong) -> jlong = 0 => {
        match REGISTRY_LISTENER.cancel_view(handle as u64) {
            Some(view) => crate::shells::cancel::cancel_view_handle(view),
            None => {
                throw_closed(env, "Listener");
                0
            }
        }
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
    ) -> jbyteArray = std::ptr::null_mut() => {
        let Some(peer) = REGISTRY_TRANSPORT.with(handle as u64, |a| a.peer) else {
            throw_closed(env, "Transport");
            return std::ptr::null_mut();
        };
        socket_addr_array(env, peer)
    }
}

jni_native! {
    fn Java_org_tstrans_tcp_Transport_nClose(_env, _class, handle: jlong) = () => {
        // Dropping the connection closes its socket.
        drop(REGISTRY_TRANSPORT.close(handle as u64));
    }
}

/// Take an accepted connection out of the registry; `None` (and an
/// `IllegalStateException` pending) when it is closed or already consumed.
fn take_conn(env: &mut jni::Env, handle: jlong) -> Option<TcpTransport> {
    let taken = REGISTRY_TRANSPORT.close(handle as u64).map(|a| a.conn);
    if taken.is_none() {
        throw_closed(env, "Transport");
    }
    taken
}

/// The connection as an opened send transport, its cancel read before it is boxed.
fn opened_send(conn: TcpTransport) -> Opened<Box<dyn Transport>> {
    let cancel = cancel_of(&conn);
    Opened::new(Box::new(conn), cancel, Domain::Tcp)
}

/// The connection as an opened receive transport; see [`opened_send`].
fn opened_recv(conn: TcpTransport) -> Opened<Box<dyn RecvTransport>> {
    let cancel = recv_cancel_of(&conn);
    Opened::new(Box::new(conn), cancel, Domain::Tcp)
}

jni_native! {
    fn Java_org_tstrans_tcp_Transport_nIntoSender(env, _class, handle: jlong) -> jlong = 0 => {
        let Some(conn) = take_conn(env, handle) else {
            return 0;
        };
        shells::sender::register(opened_send(conn))
    }
}

jni_native! {
    fn Java_org_tstrans_tcp_Transport_nIntoReceiver(env, _class, handle: jlong) -> jlong = 0 => {
        let Some(conn) = take_conn(env, handle) else {
            return 0;
        };
        shells::receiver::register(opened_recv(conn))
    }
}

jni_native! {
    fn Java_org_tstrans_tcp_Transport_nIntoDemuxReceiver(env, _class, handle: jlong) -> jlong = 0 => {
        let Some(conn) = take_conn(env, handle) else {
            return 0;
        };
        shells::demux_receiver::register(opened_recv(conn), None)
    }
}

jni_native! {
    fn Java_org_tstrans_tcp_Transport_nIntoDemuxReceiverWithConfig(
        env,
        _class,
        handle: jlong,
        strict: jint,
        pes_cap_per_pid: jlong,
        pes_cap_total: jlong,
        cfi: jboolean,
        av1: jint,
        au_cell_cap: jlong,
        lenient_psi: jboolean,
        sync_buf_cap: jlong,
        unwrap_timestamps: jboolean,
    ) -> jlong = 0 => {
        let Some(opts) = build_demux_config_from_args(
            env,
            strict,
            pes_cap_per_pid,
            pes_cap_total,
            cfi,
            av1,
            au_cell_cap,
            lenient_psi,
            sync_buf_cap,
            unwrap_timestamps,
        ) else {
            // Refused before the take: the connection stays registered with its
            // caller, which claims its handle only after a successful return.
            return 0;
        };
        let Some(conn) = take_conn(env, handle) else {
            return 0;
        };
        shells::demux_receiver::register(opened_recv(conn), Some(opts))
    }
}
