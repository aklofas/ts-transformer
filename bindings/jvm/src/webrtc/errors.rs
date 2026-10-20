//! `org.tstrans.WebrtcException` raise paths. The kinds come from the shared
//! `From<WhepServerError>` / `From<PushError>` impls in
//! `crates/tst-webrtc/src/binding_kind.rs`; this file holds no mapping table.

use jni::Env;
use tst_pipeline::binding::{BindingError, BindingErrorKind};
use tst_webrtc::{PushError, WhepServerError};

use crate::error::{Domain, throw_binding};

/// `WebrtcException(Kind.<kind.name()>, message)`.
pub(super) fn throw_webrtc(env: &mut Env, kind: BindingErrorKind, message: &str) {
    throw_binding(env, Domain::Webrtc, &BindingError::new(kind, message));
}

/// Bind and mount-management errors.
pub(super) fn server_error(env: &mut Env, e: WhepServerError) {
    throw_binding(env, Domain::Webrtc, &BindingError::from(e));
}

/// Push errors: `CLOSED` for a closed mount, `PUSH` for a refused input.
pub(super) fn push_error(env: &mut Env, e: PushError) {
    throw_binding(env, Domain::Webrtc, &BindingError::from(e));
}
