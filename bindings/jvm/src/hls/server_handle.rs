//! JNI surface for `org.tstrans.hls.HlsServerHandle` — wraps
//! `tst_hls::HlsServerHandle`, the HTTP server kept up after
//! `HlsPublisher.finishServing()`. `nShutdown` takes + consumes (idempotent via
//! the registry); `nClose` is its quiet alias.

use std::sync::LazyLock;

use jni::JNIEnv;
use jni::objects::JClass;
use jni::sys::{jlong, jstring};

use tst_hls::HlsServerHandle;

use crate::error::throw_closed;
use crate::handle::HandleRegistry;

static REGISTRY: LazyLock<HandleRegistry<HlsServerHandle>> = LazyLock::new(HandleRegistry::new);

pub(crate) fn register(h: HlsServerHandle) -> jlong {
    REGISTRY.insert(h) as jlong
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_HlsServerHandle_nLocalAddr<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jstring {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        match REGISTRY.with(handle as u64, |h| h.local_addr().to_string()) {
            Some(addr) => env
                .new_string(addr)
                .map(|s| s.into_raw())
                .unwrap_or(std::ptr::null_mut()),
            None => {
                throw_closed(env, "HlsServerHandle");
                std::ptr::null_mut()
            }
        }
    })
}

/// `nShutdown(handle)` — stop serving, drain the runtime. A miss is a no-op
/// (the Java side already made a second call unreachable).
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_hls_HlsServerHandle_nShutdown<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    crate::panic::jni_catch(&mut env, (), |_env| {
        if let Some(h) = REGISTRY.close(handle as u64) {
            h.shutdown();
        }
    })
}
