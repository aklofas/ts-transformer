//! JNI exports for `org.tstrans.rtp.MountRules`, plus the marshalling of a
//! `MountRule` (two policies, each flattened to a kind, a status code and two
//! parallel `String[]`s) shared with `RtspServer.nStart`.
//!
//! Each `MountRules` handle owns a clone of the server's table, so it keeps
//! answering after the server closes; an edit then reports `Closed`.

use std::sync::LazyLock;

use jni::Env;
use jni::jni_str;
use jni::objects::{JObjectArray, JString};
use jni::strings::JNIString;
use jni::sys::{jboolean, jint, jlong};
use secrecy::SecretString;
use tst_rtp::{AuthScheme, MountCredential, MountPolicy, MountRule, MountRules, RefuseStatus};

use super::errors::mount_rule_error_to_jvm;
use crate::handle::HandleRegistry;
use crate::jutil::FindClass;
use crate::panic::jni_native;

/// Per-type leased-handle registry for `org.tstrans.rtp.MountRules`. No cancel
/// hook: no native here parks.
static REGISTRY_MOUNT_RULES: LazyLock<HandleRegistry<MountRules>> =
    LazyLock::new(HandleRegistry::new);

/// Register a table handle and return its Java key.
pub(super) fn register(rules: MountRules) -> jlong {
    REGISTRY_MOUNT_RULES.insert(rules) as jlong
}

fn throw_illegal_argument(env: &mut Env, msg: &str) {
    let _ = env.throw_new(
        FindClass(jni_str!("java/lang/IllegalArgumentException")),
        JNIString::from(msg),
    );
}

/// Every element of a nullable `String[]`, or `None` with an exception pending.
fn read_strings(env: &mut Env, arr: &JObjectArray) -> Option<Vec<String>> {
    if arr.is_null() {
        return Some(Vec::new());
    }
    let len = match arr.len(env) {
        Ok(n) => n,
        Err(e) => {
            let _ = env.throw_new(
                FindClass(jni_str!("java/lang/RuntimeException")),
                JNIString::from(e.to_string()),
            );
            return None;
        }
    };
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        // One frame per element so the String locals do not pile up.
        let s = env.with_local_frame(4, |env| -> jni::errors::Result<String> {
            let obj = arr.get_element(env, i)?;
            let js = env.cast_local::<JString>(obj)?;
            js.try_to_string(env)
        });
        match s {
            Ok(s) => out.push(s),
            Err(e) => {
                let _ = env.throw_new(
                    FindClass(jni_str!("java/lang/RuntimeException")),
                    JNIString::from(e.to_string()),
                );
                return None;
            }
        }
    }
    Some(out)
}

/// One `MountPolicy` from its Java flattening (`kind` = `MountPolicy.KIND_*`), or
/// `None` with an exception pending. A password becomes a `SecretString` here.
fn read_policy(
    env: &mut Env,
    kind: jint,
    status: jint,
    users: &JObjectArray,
    passwords: &JObjectArray,
) -> Option<MountPolicy> {
    match kind {
        0 => Some(MountPolicy::Inherit),
        1 => Some(MountPolicy::Open),
        2 => {
            let users = read_strings(env, users)?;
            let passwords = read_strings(env, passwords)?;
            if users.len() != passwords.len() {
                throw_illegal_argument(env, "usernames and passwords differ in length");
                return None;
            }
            let creds = users
                .iter()
                .zip(passwords)
                .map(|(u, p)| MountCredential::new(u, SecretString::from(p)))
                .collect();
            Some(MountPolicy::Require(creds))
        }
        3 => {
            let s = match status {
                403 => RefuseStatus::Forbidden,
                404 => RefuseStatus::NotFound,
                453 => RefuseStatus::NotEnoughBandwidth,
                503 => RefuseStatus::ServiceUnavailable,
                other => {
                    throw_illegal_argument(env, &format!("unknown refuse status {other}"));
                    return None;
                }
            };
            Some(MountPolicy::Refuse(s))
        }
        other => {
            throw_illegal_argument(env, &format!("unknown MountPolicy kind {other}"));
            None
        }
    }
}

/// A `MountRule` from the ten flattened parameters, or `None` with an exception pending.
#[allow(clippy::too_many_arguments)]
pub(super) fn read_rule(
    env: &mut Env,
    pub_kind: jint,
    pub_status: jint,
    pub_users: &JObjectArray,
    pub_passwords: &JObjectArray,
    read_kind: jint,
    read_status: jint,
    read_users: &JObjectArray,
    read_passwords: &JObjectArray,
) -> Option<MountRule> {
    let publish = read_policy(env, pub_kind, pub_status, pub_users, pub_passwords)?;
    let read = read_policy(env, read_kind, read_status, read_users, read_passwords)?;
    Some(MountRule::new(publish, read))
}

/// Run `f` on the table for `handle`, or throw `IllegalStateException` when the
/// Java object was closed.
fn with_rules<R>(env: &mut Env, handle: jlong, f: impl FnOnce(&MountRules) -> R) -> Option<R> {
    match REGISTRY_MOUNT_RULES.with(handle as u64, |r| f(r)) {
        Some(r) => Some(r),
        None => {
            crate::error::throw_closed(env, "MountRules");
            None
        }
    }
}

jni_native! {
    /// `MountRules.nSet(rules, path, ...)` — a null `path` sets the default rule.
    #[allow(clippy::too_many_arguments)]
    fn Java_org_tstrans_rtp_MountRules_nSet<'local>(
        env,
        _class,
        handle: jlong,
        path: JString<'local>,
        pub_kind: jint,
        pub_status: jint,
        pub_users: JObjectArray<'local>,
        pub_passwords: JObjectArray<'local>,
        read_kind: jint,
        read_status: jint,
        read_users: JObjectArray<'local>,
        read_passwords: JObjectArray<'local>,
    ) = () => {
        let path = if path.is_null() {
            None
        } else {
            match path.try_to_string(env) {
                Ok(p) => Some(p),
                Err(_) => return,
            }
        };
        let Some(rule) = read_rule(
            env, pub_kind, pub_status, &pub_users, &pub_passwords,
            read_kind, read_status, &read_users, &read_passwords,
        ) else {
            return;
        };
        let result = with_rules(env, handle, |rules| match &path {
            Some(p) => rules.set(p, rule),
            None => rules.set_default(rule),
        });
        if let Some(Err(e)) = result {
            mount_rule_error_to_jvm(env, e);
        }
    }
}

jni_native! {
    fn Java_org_tstrans_rtp_MountRules_nRemove<'local>(
        env,
        _class,
        handle: jlong,
        path: JString<'local>,
    ) -> jboolean = false => {
        let Ok(path) = path.try_to_string(env) else { return false };
        match with_rules(env, handle, |rules| rules.remove(&path)) {
            Some(Ok(removed)) => removed,
            Some(Err(e)) => {
                mount_rule_error_to_jvm(env, e);
                false
            }
            None => false,
        }
    }
}

jni_native! {
    fn Java_org_tstrans_rtp_MountRules_nClear(env, _class, handle: jlong) = () => {
        if let Some(Err(e)) = with_rules(env, handle, |rules| rules.clear()) {
            mount_rule_error_to_jvm(env, e);
        }
    }
}

jni_native! {
    fn Java_org_tstrans_rtp_MountRules_nSetChallenge<'local>(
        env,
        _class,
        handle: jlong,
        scheme: jint,
        realm: JString<'local>,
    ) = () => {
        let Ok(realm) = realm.try_to_string(env) else { return };
        let scheme = match scheme {
            0 => AuthScheme::Basic,
            1 => AuthScheme::DigestMd5,
            2 => AuthScheme::DigestSha256,
            other => {
                throw_illegal_argument(env, &format!("unknown AuthScheme code {other}"));
                return;
            }
        };
        if let Some(Err(e)) = with_rules(env, handle, |rules| rules.set_challenge(scheme, &realm)) {
            mount_rule_error_to_jvm(env, e);
        }
    }
}

jni_native! {
    fn Java_org_tstrans_rtp_MountRules_nSize(env, _class, handle: jlong) -> jint = 0 => {
        with_rules(env, handle, |rules| i32::try_from(rules.len()).unwrap_or(i32::MAX))
            .unwrap_or(0)
    }
}

jni_native! {
    fn Java_org_tstrans_rtp_MountRules_nIsClosed(env, _class, handle: jlong) -> jboolean = false => {
        with_rules(env, handle, |rules| rules.is_closed()).unwrap_or(false)
    }
}

jni_native! {
    /// Free the handle; the server's table lives on while the server holds it.
    fn Java_org_tstrans_rtp_MountRules_nClose(_env, _class, handle: jlong) = () => {
        let _ = REGISTRY_MOUNT_RULES.close(handle as u64);
    }
}
