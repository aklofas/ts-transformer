//! JNI exports for `org.tstrans.srt.ListenRules` and `RejectReason.nTable`.
//!
//! Every native reads the listener's `ListenRules` handle from the registry
//! snapshot, never the slot, so an edit answers while `accept` is parked. A
//! closed listener has no registry entry: that is `SrtException(CLOSED)`, the
//! same outcome the Rust handle reports once its listener closes.

use jni::Env;
use jni::jni_str;
use jni::objects::{JObject, JString};
use jni::sys::{jboolean, jint, jlong, jobjectArray};
use tst_pipeline::binding::BindingErrorKind;
use tst_srt::error::RejectReason;
use tst_srt::{AccessControl, ListenRule, ListenRuleError, ListenRules, Passphrase};

use super::errors::throw_srt;
use super::lowlevel::listener_rules;
use crate::jutil::FindClass;
use crate::panic::jni_native;

/// The typed predefined reasons and their `org.tstrans.srt.RejectReason`
/// constant names; `RejectReasonTableTest` holds the Java side to this list.
const PREDEFINED: [(RejectReason, &str); 21] = [
    (RejectReason::Fallback, "FALLBACK"),
    (RejectReason::KeyNotSupported, "KEY_NOT_SUPPORTED"),
    (RejectReason::Filepath, "FILEPATH"),
    (RejectReason::HostNotFound, "HOST_NOT_FOUND"),
    (RejectReason::BadRequest, "BAD_REQUEST"),
    (RejectReason::Unauthorized, "UNAUTHORIZED"),
    (RejectReason::Overload, "OVERLOAD"),
    (RejectReason::Forbidden, "FORBIDDEN"),
    (RejectReason::NotFound, "NOT_FOUND"),
    (RejectReason::BadMode, "BAD_MODE"),
    (RejectReason::Unacceptable, "UNACCEPTABLE"),
    (RejectReason::Conflict, "CONFLICT"),
    (RejectReason::NotSupportedMedia, "NOT_SUPPORTED_MEDIA"),
    (RejectReason::Locked, "LOCKED"),
    (RejectReason::FailedDependency, "FAILED_DEPENDENCY"),
    (RejectReason::InternalServerError, "INTERNAL_SERVER_ERROR"),
    (RejectReason::Unimplemented, "UNIMPLEMENTED"),
    (RejectReason::Gateway, "GATEWAY"),
    (RejectReason::Down, "DOWN"),
    (RejectReason::VersionUnsupported, "VERSION_UNSUPPORTED"),
    (RejectReason::NoRoom, "NO_ROOM"),
];

/// The typed variant for a predefined code, `Other(code)` for any other.
fn reason_from_code(code: i32) -> RejectReason {
    PREDEFINED
        .iter()
        .map(|(r, _)| *r)
        .find(|r| r.raw() == code)
        .unwrap_or(RejectReason::Other(code))
}

/// The listener's rules, or a pending `SrtException(CLOSED)`.
fn rules_or_throw(env: &mut Env, handle: jlong) -> Option<ListenRules> {
    let rules = listener_rules(handle as u64);
    if rules.is_none() {
        throw_srt(env, BindingErrorKind::Closed, "Listener is closed");
    }
    rules
}

/// `ListenRule` from the Java marshalling (`kind` = `ListenRule.KIND_*`), or a
/// pending `SrtException(CONFIG_INVALID)`.
fn build_rule(
    env: &mut Env,
    kind: jint,
    passphrase: &JString,
    reject_code: jint,
) -> Option<ListenRule> {
    match kind {
        0 => Some(ListenRule::Accept),
        1 => {
            if passphrase.is_null() {
                throw_srt(env, BindingErrorKind::ConfigInvalid, "passphrase is null");
                return None;
            }
            let s: String = passphrase.try_to_string(env).ok()?;
            match Passphrase::new(s) {
                Ok(passphrase) => Some(ListenRule::AcceptWith { passphrase }),
                Err(e) => {
                    throw_srt(env, BindingErrorKind::ConfigInvalid, &e.to_string());
                    None
                }
            }
        }
        2 => Some(ListenRule::Reject(reason_from_code(reject_code))),
        other => {
            throw_srt(
                env,
                BindingErrorKind::ConfigInvalid,
                &format!("unknown ListenRule kind {other}"),
            );
            None
        }
    }
}

fn throw_rule_error(env: &mut Env, e: ListenRuleError) {
    let kind = match e {
        ListenRuleError::Closed => BindingErrorKind::Closed,
        // Java refuses codes below 1000 before the native runs.
        _ => BindingErrorKind::ConfigInvalid,
    };
    throw_srt(env, kind, &e.to_string());
}

jni_native! {
    fn Java_org_tstrans_srt_ListenRules_nSet<'local>(
        env,
        _class,
        handle: jlong,
        stream_id: JString<'local>,
        kind: jint,
        passphrase: JString<'local>,
        reject_code: jint,
    ) = () => {
        let Ok(stream_id) = stream_id.try_to_string(env) else { return };
        let Some(rule) = build_rule(env, kind, &passphrase, reject_code) else { return };
        let Some(rules) = rules_or_throw(env, handle) else { return };
        if let Err(e) = rules.set(&stream_id, rule) {
            throw_rule_error(env, e);
        }
    }
}

jni_native! {
    fn Java_org_tstrans_srt_ListenRules_nSetDefault<'local>(
        env,
        _class,
        handle: jlong,
        kind: jint,
        passphrase: JString<'local>,
        reject_code: jint,
    ) = () => {
        let Some(rule) = build_rule(env, kind, &passphrase, reject_code) else { return };
        let Some(rules) = rules_or_throw(env, handle) else { return };
        if let Err(e) = rules.set_default(rule) {
            throw_rule_error(env, e);
        }
    }
}

jni_native! {
    fn Java_org_tstrans_srt_ListenRules_nRemove<'local>(
        env,
        _class,
        handle: jlong,
        stream_id: JString<'local>,
    ) -> jboolean = false => {
        let Ok(stream_id) = stream_id.try_to_string(env) else { return false };
        let Some(rules) = rules_or_throw(env, handle) else { return false };
        rules.remove(&stream_id)
    }
}

jni_native! {
    fn Java_org_tstrans_srt_ListenRules_nClear(env, _class, handle: jlong) = () => {
        if let Some(rules) = rules_or_throw(env, handle) {
            rules.clear();
        }
    }
}

jni_native! {
    fn Java_org_tstrans_srt_ListenRules_nSize(env, _class, handle: jlong) -> jint = 0 => {
        match rules_or_throw(env, handle) {
            Some(rules) => i32::try_from(rules.len()).unwrap_or(i32::MAX),
            None => 0,
        }
    }
}

jni_native! {
    fn Java_org_tstrans_srt_RejectReason_nTable(
        env,
        _class,
    ) -> jobjectArray = std::ptr::null_mut() => {
        let build = |env: &mut Env| -> jni::errors::Result<jobjectArray> {
            let rows = env.new_object_array(
                PREDEFINED.len() as i32,
                FindClass(jni_str!("java/lang/String")),
                JObject::null(),
            )?;
            for (i, (reason, name)) in PREDEFINED.iter().enumerate() {
                // One frame per row so the String locals do not pile up.
                env.with_local_frame(4, |inner| -> jni::errors::Result<()> {
                    let row = inner.new_string(format!("{} {name}", reason.raw()))?;
                    rows.set_element(inner, i, &row)
                })?;
            }
            Ok(rows.into_raw())
        };
        build(env).unwrap_or(std::ptr::null_mut())
    }
}

jni_native! {
    fn Java_org_tstrans_srt_AccessControl_nParse<'local>(
        env,
        _class,
        stream_id: JString<'local>,
    ) -> jobjectArray = std::ptr::null_mut() => {
        let Ok(stream_id) = stream_id.try_to_string(env) else { return std::ptr::null_mut() };
        let ac = match AccessControl::parse(&stream_id) {
            Ok(ac) => ac,
            Err(e) => {
                let _ = env.throw_new(
                    FindClass(jni_str!("java/lang/IllegalArgumentException")),
                    jni::strings::JNIString::from(e.to_string()),
                );
                return std::ptr::null_mut();
            }
        };
        let pairs: Vec<(&str, &str)> = ac.pairs().collect();
        let build = |env: &mut Env| -> jni::errors::Result<jobjectArray> {
            let out = env.new_object_array(
                (pairs.len() * 2) as i32,
                FindClass(jni_str!("java/lang/String")),
                JObject::null(),
            )?;
            for (i, (k, v)) in pairs.iter().enumerate() {
                // One frame per pair so the String locals do not pile up.
                env.with_local_frame(4, |inner| -> jni::errors::Result<()> {
                    let ks = inner.new_string(*k)?;
                    out.set_element(inner, i * 2, &ks)?;
                    let vs = inner.new_string(*v)?;
                    out.set_element(inner, i * 2 + 1, &vs)
                })?;
            }
            Ok(out.into_raw())
        };
        build(env).unwrap_or(std::ptr::null_mut())
    }
}
