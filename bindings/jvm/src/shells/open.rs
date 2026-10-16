//! URL → opened transport, for every shell. The scheme picks the transport
//! crate's opener; all six schemes are always built (the JVM binding has no
//! transport features).

use jni::JNIEnv;
use jni::objects::JString;
use tst_core::transport::{RecvTransport, Transport};
use tst_pipeline::binding::{BindingError, BindingErrorKind, Domain, Opened};

use crate::error::throw_binding;

fn scheme(url: &str) -> &str {
    url.split_once("://").map_or("", |(s, _)| s)
}

fn domain_of(scheme: &str) -> Option<Domain> {
    match scheme {
        "srt" => Some(Domain::Srt),
        "rtp" => Some(Domain::Rtp),
        "udp" => Some(Domain::Udp),
        "tcp" | "tcps" => Some(Domain::Tcp),
        "rist" => Some(Domain::Rist),
        _ => None,
    }
}

/// Read the Java `String`, throwing `RuntimeException` on a JNI failure.
pub(super) fn read_url(env: &mut JNIEnv, url: &JString) -> Option<String> {
    match env.get_string(url) {
        Ok(s) => Some(s.into()),
        Err(e) => {
            let _ = env.throw_new("java/lang/RuntimeException", e.to_string());
            None
        }
    }
}

/// `IllegalArgumentException` for a scheme no shell opens.
fn unsupported(env: &mut JNIEnv, url: &str) {
    let _ = env.throw_new(
        "java/lang/IllegalArgumentException",
        format!(
            "unsupported URL scheme in {url:?}; expected one of: srt, rtp, udp, tcp, tcps, rist"
        ),
    );
}

/// The match arms are exhaustive over [`domain_of`]'s schemes; this is the
/// arm no caller reaches.
fn unreachable_scheme<T>() -> Result<T, BindingError> {
    Err(BindingError::new(
        BindingErrorKind::Internal,
        "scheme checked above",
    ))
}

/// Open a send transport for a shell. `None` with an exception pending on a
/// bad scheme or a failed open.
pub(super) fn open_send(env: &mut JNIEnv, url: &str) -> Option<Opened<Box<dyn Transport>>> {
    let s = scheme(url);
    let Some(domain) = domain_of(s) else {
        unsupported(env, url);
        return None;
    };
    let opened = match s {
        "srt" => tst_srt::opener::open_sender(url),
        "rtp" => tst_rtp::opener::open_sender(url),
        "udp" => tst_udp::opener::open_sender(url),
        "tcp" | "tcps" => tst_tcp::opener::open_sender(url),
        "rist" => tst_rist::opener::open_sender(url),
        _ => unreachable_scheme(),
    };
    match opened {
        Ok(o) => Some(o),
        Err(e) => {
            throw_binding(env, domain, &e);
            None
        }
    }
}

/// Open a receive transport for a shell. An `srt://` open takes a fresh,
/// never-fired accept slot: no cancel handle exists until the shell does, so
/// the first accept inside a blocking open stays uncancellable.
pub(super) fn open_recv(env: &mut JNIEnv, url: &str) -> Option<Opened<Box<dyn RecvTransport>>> {
    let s = scheme(url);
    let Some(domain) = domain_of(s) else {
        unsupported(env, url);
        return None;
    };
    let opened = match s {
        "srt" => {
            let slot = std::sync::Arc::new(tst_core::cancel::CancelSlot::new());
            tst_srt::opener::open_recv(url, &slot)
        }
        "rtp" => tst_rtp::opener::open_recv(url),
        "udp" => tst_udp::opener::open_recv(url),
        "tcp" | "tcps" => tst_tcp::opener::open_recv(url),
        "rist" => tst_rist::opener::open_recv(url),
        _ => unreachable_scheme(),
    };
    match opened {
        Ok(o) => Some(o),
        Err(e) => {
            throw_binding(env, domain, &e);
            None
        }
    }
}
