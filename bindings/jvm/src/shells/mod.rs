//! `org.tstrans.shells` — one Java class per pipeline shell (`Sender`,
//! `Receiver`, `MuxSender`, `DemuxReceiver`) over every transport. A shell
//! is opened from a URL (`Class.open(url, …)`); the scheme picks the
//! transport crate's opener (`srt://`, `rtp://`, `udp://`, `tcp://`,
//! `tcps://`, `rist://`), and the shell holds it type-erased
//! (`Box<dyn Transport>`). The transport's error domain travels in the
//! shell's [`Snapshot`]; it is read before each operation, so a `CLOSED` on a
//! `rtp://` shell throws `RtpException` and on a `udp://` shell
//! `UdpException`. After `close()` the Java handle is gone and every call
//! throws `IllegalStateException` from the class's `ensureOpen`.
//!
//! Each class owns one [`OwnedRegistry`]; the Java `long` is the registry
//! key. A key that is absent (the Java handle was closed) is an
//! `IllegalStateException`, thrown by [`throw_state`] below.

pub(crate) mod cancel;
pub(crate) mod demux_receiver;
pub(crate) mod end_reason;
pub(crate) mod mux_sender;
mod open;
pub(crate) mod receiver;
pub(crate) mod sender;

use jni::JNIEnv;
use jni::sys::jlong;
use tst_pipeline::binding::view::Snapshot;
use tst_pipeline::binding::{BindingError, Domain, HandleState};

use crate::error::{throw_binding, throw_handle_state};
use crate::handle::OwnedRegistry;

/// The shell's error domain, read from its construction-time snapshot BEFORE
/// the native op runs: a `close()` on another thread removes the key (then
/// cancels), so reading it afterwards would turn the parked call's `CLOSED`
/// into an `IllegalStateException`. An absent key is a closed Java handle:
/// `IllegalStateException` naming `what`, and `None`.
fn domain_or_closed<T: Send + 'static, H: Send + Sync + 'static>(
    env: &mut JNIEnv,
    registry: &OwnedRegistry<T, Snapshot<H>>,
    handle: jlong,
    what: &str,
) -> Option<Domain> {
    let domain = registry.snapshot(handle as u64, |s| s.domain);
    if domain.is_none() {
        crate::error::throw_closed(env, what);
    }
    domain
}

/// A [`HandleState`] failure of a native that reached the registry. A slot
/// taken by a concurrent `close()` is the transport's own `CLOSED` in the
/// shell's domain, as Python raises it; a poisoned slot and a panic keep the
/// one handle-state mapping.
/// Only the operations that can park (send, receive, iterate, finish) use this;
/// the stats and accessor natives throw `IllegalStateException` for a closed
/// handle through `throw_handle_state`, the established JVM idiom.
fn throw_state(env: &mut JNIEnv, domain: Domain, what: &str, state: &HandleState) {
    if matches!(state, HandleState::Closed) {
        throw_binding(env, domain, &BindingError::from(state.clone()));
    } else {
        throw_handle_state(env, what, state);
    }
}
