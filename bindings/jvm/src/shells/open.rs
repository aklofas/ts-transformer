//! URL → opened transport, for every shell. The scheme picks the transport
//! crate's opener; all six schemes are always built (the JVM binding has no
//! transport features).

use jni::JNIEnv;
use jni::objects::JString;
use tst_core::transport::{RecvTransport, Transport};
use tst_pipeline::binding::{BindingError, BindingErrorKind, Domain, Opened};

use crate::error::throw_binding;

/// The domain of `url`'s scheme, or `None` for a scheme no shell opens.
fn domain_of(url: &str) -> Option<Domain> {
    Domain::of_scheme(url.split_once("://")?.0)
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

/// The match arms cover every domain [`domain_of`] returns; this is the
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
    let Some(domain) = domain_of(url) else {
        unsupported(env, url);
        return None;
    };
    let opened = match domain {
        Domain::Srt => tst_srt::opener::open_sender(url),
        Domain::Rtp => tst_rtp::opener::open_sender(url),
        Domain::Udp => tst_udp::opener::open_sender(url),
        Domain::Tcp => tst_tcp::opener::open_sender(url),
        Domain::Rist => tst_rist::opener::open_sender(url),
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
    let Some(domain) = domain_of(url) else {
        unsupported(env, url);
        return None;
    };
    let opened = match domain {
        Domain::Srt => {
            let slot = std::sync::Arc::new(tst_core::cancel::CancelSlot::new());
            tst_srt::opener::open_recv(url, &slot)
        }
        Domain::Rtp => tst_rtp::opener::open_recv(url),
        Domain::Udp => tst_udp::opener::open_recv(url),
        Domain::Tcp => tst_tcp::opener::open_recv(url),
        Domain::Rist => tst_rist::opener::open_recv(url),
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
