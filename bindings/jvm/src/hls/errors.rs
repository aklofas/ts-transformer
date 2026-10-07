//! `org.tstrans.hls` Rust→Java error mapping: the per-type plumbing that names
//! `Domain::Hls` and picks the right shared `From` impl.

use jni::JNIEnv;
use tst_hls::HlsError;
use tst_pipeline::MuxPublisherError;
use tst_pipeline::binding::{BindingError, BindingErrorKind};

use crate::error::{Domain, throw_binding};

/// `org.tstrans.HlsException(Kind.<kind.name()>, message)`.
#[allow(dead_code)] // consumed by test_hooks.rs's HlsProbe.nRaise, built only under jni-test-hooks
pub(crate) fn throw_hls(env: &mut JNIEnv, kind: BindingErrorKind, message: &str) {
    throw_binding(
        env,
        Domain::Hls,
        &BindingError {
            kind,
            detail: message.to_owned(),
        },
    );
}

/// Any `tst_hls::HlsError` (incl. an `HlsUrlError` via `Into<HlsError>`)
/// through the shared `From<HlsError> for BindingError`: the error's own kind.
pub(crate) fn hls_error(env: &mut JNIEnv, e: impl Into<HlsError>) {
    throw_binding(env, Domain::Hls, &BindingError::from(e.into()));
}

/// `MuxPublisherError<HlsError>` from a `MuxPublisher` send/cut. The source
/// decides the CLASS (tst-py's `map_mux_publisher_error`): `Mux(e)` keeps the
/// `MuxException` classifier; `Publisher(e)` is the inner HLS kind, `Closed`
/// is `CLOSED`, `LockPoisoned` is `INTERNAL` — all via the shared
/// `From<MuxPublisherError<E>>`.
#[allow(dead_code)] // consumed by the mux_publisher.rs native bodies once MuxPublisher lands
pub(crate) fn mux_publisher_error(env: &mut JNIEnv, e: MuxPublisherError<HlsError>) {
    match e {
        MuxPublisherError::Mux(m) => crate::mpegts::muxer::throw_mux_error(env, &m),
        other => throw_binding(env, Domain::Hls, &BindingError::from(other)),
    }
}
