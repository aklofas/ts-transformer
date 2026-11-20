//! JNI surface for `org.tstrans.hls.HlsServerHandle` — wraps
//! `tst_hls::HlsServerHandle`, the HTTP server kept up after
//! `HlsPublisher.finishServing()`. `nShutdown` takes + consumes (idempotent via
//! the registry); `nClose` is its quiet alias.

use crate::panic::jni_native;
use std::sync::LazyLock;

use jni::sys::{jbyteArray, jlong};

use tst_hls::HlsServerHandle;

use crate::error::throw_closed;
use crate::handle::HandleRegistry;
use crate::jutil::socket_addr_array;

static REGISTRY: LazyLock<HandleRegistry<HlsServerHandle>> = LazyLock::new(HandleRegistry::new);

pub(crate) fn register(h: HlsServerHandle) -> jlong {
    REGISTRY.insert(h) as jlong
}

jni_native! {
    fn Java_org_tstrans_hls_HlsServerHandle_nLocalAddr<'local>(
        env,
        _class,
        handle: jlong,
    ) -> jbyteArray = std::ptr::null_mut() => {
        match REGISTRY.with(handle as u64, |h| h.local_addr()) {
            Some(addr) => socket_addr_array(env, addr),
            None => {
                throw_closed(env, "HlsServerHandle");
                std::ptr::null_mut()
            }
        }
    }
}

jni_native! {
    /// `nShutdown(handle)` — stop serving and wait up to 2 s for the `tst-hls` thread. A miss is a no-op
    /// (the Java side already made a second call unreachable).
    fn Java_org_tstrans_hls_HlsServerHandle_nShutdown<'local>(_env, _class, handle: jlong) = () => {
        if let Some(h) = REGISTRY.close(handle as u64) {
            h.shutdown();
        }
    }
}
