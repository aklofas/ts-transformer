//! URL → opened transport, for every shell. The scheme picks the transport
//! crate's opener; all six schemes are always built (the JVM binding has no
//! transport features).

use crate::jutil::FindClass;
use jni::Env;
use jni::jni_str;
use jni::objects::{JObject, JString};
use jni::strings::JNIString;
use std::sync::Arc;
use tst_core::cancel::CancelSlot;
use tst_core::transport::{RecvTransport, Transport};
use tst_core::url::redact_url;
use tst_pipeline::binding::{
    BindingError, BindingErrorKind, Domain, OpenSlot, Opened, SHELL_SCHEMES,
};
use tst_rtp::RtpStatsHandle;

use super::open_cancel::{after_begin, begin, closed, finish, object_of, refuse_config};
use crate::error::throw_binding;

/// The domain of `url`'s scheme, or `None` for a scheme no shell opens.
fn domain_of(url: &str) -> Option<Domain> {
    Domain::of_scheme(url.split_once("://")?.0)
}

/// Read the Java `String`, throwing `RuntimeException` on a JNI failure.
pub(crate) fn read_url(env: &mut Env, url: &JString) -> Option<String> {
    match url.try_to_string(env) {
        Ok(s) => Some(s),
        Err(e) => {
            let _ = env.throw_new(
                FindClass(jni_str!("java/lang/RuntimeException")),
                JNIString::from(e.to_string()),
            );
            None
        }
    }
}

/// `IllegalArgumentException` for a scheme no shell opens, naming every shell
/// scheme (the JVM binding builds them all).
fn unsupported(env: &mut Env, url: &str) {
    let schemes: Vec<&str> = SHELL_SCHEMES.iter().map(|(s, _)| *s).collect();
    let _ = env.throw_new(
        FindClass(jni_str!("java/lang/IllegalArgumentException")),
        JNIString::from(format!(
            "unsupported URL scheme in {:?}; expected one of: {}",
            redact_url(url),
            schemes.join(", ")
        )),
    );
}

/// The match arms cover every domain [`domain_of`] returns; this is the
/// arm no caller reaches.
fn unreachable_scheme<T>() -> Result<T, BindingError> {
    Err(BindingError::new(
        BindingErrorKind::Internal,
        "scheme checked above",
    ))
}

/// The checks every shell open runs before it opens anything, in this
/// order: the scheme (`IllegalArgumentException`), a closed `OpenCancel`
/// object (`IllegalStateException`), a cancelled one (the domain's
/// `CLOSED`), then `config` (the shell's configuration; `Err` leaves its
/// exception pending). The URL is parsed and the transport opened after.
/// `None` with an exception pending when a check fails.
fn check<'local, C>(
    env: &mut Env<'local>,
    url: &str,
    cancel: &JObject,
    config: impl FnOnce(&mut Env<'local>) -> Result<C, ()>,
) -> Option<(Domain, Option<OpenSlot>, C)> {
    let Some(domain) = domain_of(url) else {
        unsupported(env, url);
        return None;
    };
    let oc = object_of(env, cancel).ok()?;
    let slot = begin(env, domain, oc.as_ref()).ok()?;
    after_begin(oc.as_ref());
    match config(env) {
        Ok(c) => Some((domain, slot, c)),
        Err(()) => {
            refuse_config(env, domain, slot);
            None
        }
    }
}

/// Open a send transport for a shell, after [`check`] (which runs `config`).
/// `None` with an exception pending on a failed check or open. `cancel` is
/// the caller's `OpenCancel`: a sender dials and takes no slot, so only a
/// cancel before or during the connect is seen, as `CLOSED` once the connect
/// ends.
pub(super) fn open_send<'local, C>(
    env: &mut Env<'local>,
    url: &str,
    cancel: &JObject,
    config: impl FnOnce(&mut Env<'local>) -> Result<C, ()>,
) -> Option<(Opened<Box<dyn Transport>>, C)> {
    let (domain, slot, config) = check(env, url, cancel, config)?;
    let opened = match domain {
        Domain::Srt => tst_srt::opener::open_sender(url),
        Domain::Rtp => tst_rtp::opener::open_sender(url),
        Domain::Udp => tst_udp::opener::open_sender(url),
        Domain::Tcp => tst_tcp::opener::open_sender(url),
        Domain::Rist => tst_rist::opener::open_sender(url),
        _ => unreachable_scheme(),
    };
    match finish(slot, opened, closed()) {
        Ok(o) => Some((o, config)),
        Err(e) => {
            throw_binding(env, domain, &e);
            None
        }
    }
}

/// An opened receive transport, with the reader of its RTP counters for an
/// `rtp://` URL.
pub(super) type OpenedRecv = (Opened<Box<dyn RecvTransport>>, Option<RtpStatsHandle>);

/// Open a receive transport for a shell, after [`check`], with the reader of
/// its RTP counters for an `rtp://` URL (taken before the transport was
/// boxed). An `srt://` open hands its first accept the slot the caller's
/// `OpenCancel` gave it, so the object's `cancel()` ends that accept; without
/// an object the slot is a fresh one nothing fires, and the accept waits for
/// a peer.
pub(super) fn open_recv(env: &mut Env, url: &str, cancel: &JObject) -> Option<OpenedRecv> {
    let (domain, slot, ()) = check(env, url, cancel, |_| Ok(()))?;
    let opened = match domain {
        Domain::Srt => match &slot {
            Some(slot) => tst_srt::opener::open_recv(url, slot),
            None => tst_srt::opener::open_recv(url, &Arc::new(CancelSlot::new())),
        }
        .map(|o| (o, None)),
        Domain::Rtp => tst_rtp::opener::open_recv_with_stats(url).map(|(o, h)| (o, Some(h))),
        Domain::Udp => tst_udp::opener::open_recv(url).map(|o| (o, None)),
        Domain::Tcp => tst_tcp::opener::open_recv(url).map(|o| (o, None)),
        Domain::Rist => tst_rist::opener::open_recv(url).map(|o| (o, None)),
        _ => unreachable_scheme(),
    };
    match finish(slot, opened, closed()) {
        Ok(o) => Some(o),
        Err(e) => {
            throw_binding(env, domain, &e);
            None
        }
    }
}
