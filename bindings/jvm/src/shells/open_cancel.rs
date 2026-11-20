//! `org.tstrans.shells.OpenCancel` natives, and the steps every open takes
//! with the object (`begin` before the configuration and the URL are looked
//! at, `finish` after the open).
//!
//! The registry holds the [`OpenCancel`]; every native clones it out of the
//! entry and calls it after the entry lock is released, so a `cancel()`
//! whose slot cancel blocks (an SRT listener close) never holds up
//! `isCancelled()` or `close()`. An open clones the object out the same way
//! before it blocks, so a `close()` of the Java object frees only the
//! registry's clone and does not end a parked open.

use crate::panic::jni_native;
use std::sync::LazyLock;

use jni::Env;
use jni::objects::JObject;
use jni::sys::{jboolean, jlong};
use jni::{jni_sig, jni_str};
use tst_core::transport::TransportError;
use tst_pipeline::binding::{BindingError, Domain, OpenCancel, OpenSlot};

use crate::error::throw_binding;
use crate::handle::HandleRegistry;

static REGISTRY: LazyLock<HandleRegistry<OpenCancel>> = LazyLock::new(HandleRegistry::new);

/// The object behind the Java `OpenCancel` an open was given; `Ok(None)`
/// for a Java `null` (the open runs as it does without the argument).
/// `Err(())` with `IllegalStateException` pending when the object is closed.
pub(crate) fn object_of(env: &mut Env, cancel: &JObject) -> Result<Option<OpenCancel>, ()> {
    if cancel.is_null() {
        return Ok(None);
    }
    // `openKey()` throws `IllegalStateException` for a closed object.
    let key = env
        .call_method(cancel, jni_str!("openKey"), jni_sig!("()J"), &[])
        .and_then(|v| v.j())
        .map_err(|_| ())?;
    match REGISTRY.with(key as u64, |oc| oc.clone()) {
        Some(oc) => Ok(Some(oc)),
        None => {
            crate::error::throw_closed(env, "OpenCancel");
            Err(())
        }
    }
}

/// Take one open's slot. `Ok(None)` when the open was given no object;
/// `Err(())` with the domain's `CLOSED` pending when the object is already
/// cancelled (the open then parses nothing, binds nothing, dials nothing).
pub(crate) fn begin(
    env: &mut Env,
    domain: Domain,
    oc: Option<&OpenCancel>,
) -> Result<Option<OpenSlot>, ()> {
    let Some(oc) = oc else { return Ok(None) };
    let slot = oc.slot();
    if slot.is_cancelled() {
        throw_binding(env, domain, &closed());
        return Err(());
    }
    Ok(Some(slot))
}

/// What an open reports when its object is cancelled: kind `CLOSED`.
pub(crate) fn closed() -> BindingError {
    BindingError::from(TransportError::ExplicitClose)
}

/// End an open: when the object was cancelled by now, `r` is dropped (a
/// shell that had just opened is closed again) and the open reports
/// `closed`; otherwise `r` is returned whole and the object never touches
/// it again.
pub(crate) fn finish<T, E>(slot: Option<OpenSlot>, r: Result<T, E>, closed: E) -> Result<T, E> {
    if slot.is_some_and(OpenSlot::finish) {
        drop(r);
        return Err(closed);
    }
    r
}

/// End an open whose configuration was refused, with that refusal's
/// exception pending: when the object was cancelled by now the domain's
/// `CLOSED` replaces it, as [`finish`] replaces a failed open's error.
pub(crate) fn refuse_config(env: &mut Env, domain: Domain, slot: Option<OpenSlot>) {
    if slot.is_some_and(OpenSlot::finish) {
        env.exception_clear();
        throw_binding(env, domain, &closed());
    }
}

jni_native! {
    fn Java_org_tstrans_shells_OpenCancel_nNew(_env, _class) -> jlong = 0 => {
        REGISTRY.insert(OpenCancel::new()) as jlong
    }
}

jni_native! {
    fn Java_org_tstrans_shells_OpenCancel_nCancel(env, _class, handle: jlong) = () => {
        match REGISTRY.with(handle as u64, |oc| oc.clone()) {
            Some(oc) => oc.cancel(),
            None => crate::error::throw_closed(env, "OpenCancel"),
        }
    }
}

jni_native! {
    fn Java_org_tstrans_shells_OpenCancel_nIsCancelled(
        env,
        _class,
        handle: jlong,
    ) -> jboolean = false => {
        match REGISTRY.with(handle as u64, |oc| oc.clone()) {
            Some(oc) => oc.is_cancelled(),
            None => {
                crate::error::throw_closed(env, "OpenCancel");
                false
            }
        }
    }
}

jni_native! {
    fn Java_org_tstrans_shells_OpenCancel_nClose(_env, _class, handle: jlong) = () => {
        // Frees the registry's clone only; an open in progress keeps its own.
        let _ = REGISTRY.close(handle as u64);
    }
}
