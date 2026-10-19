//! Test-only JNI hooks, compiled solely under `feature = "jni-test-hooks"`.
//! The published cdylib is built WITHOUT this feature (CI rebuilds probe-free
//! before staging the shipped lib — see `.github/workflows/jvm-jar.yml`), so
//! these symbols never reach Maven Central.

use crate::panic::jni_native;
use jni::jni_str;
use jni::strings::JNIString;
use jni::sys::jlong;

jni_native! {
    /// `org.tstrans.internal.PanicProbe.nForcePanic()` — deliberately panics inside
    /// [`crate::panic::jni_catch`] to prove the panic becomes a thrown
    /// `RuntimeException` rather than unwinding across the JNI boundary.
    fn Java_org_tstrans_internal_PanicProbe_nForcePanic<'local>(_env, _class) -> jlong = 0 => {
        panic!("intentional panic from PanicProbe.nForcePanic (jni-test-hooks)");
    }
}

// ---------------------------------------------------------------------------
// Handle-aware probe: exercises `HandleRegistry::with_poisoning` end-to-end
// through a real JNI call. A registry-backed `u64` counter acts as the
// resource; the mutating native panics mid-mutation to prove the handle is
// poisoned and later ops throw `IllegalStateException`.
// ---------------------------------------------------------------------------

mod handle_probe {
    use crate::handle::HandleRegistry;
    use crate::panic::jni_native;

    use jni::jni_str;
    use jni::sys::{jboolean, jlong};
    use std::sync::LazyLock;

    static PROBE_REGISTRY: LazyLock<HandleRegistry<u64>> = LazyLock::new(HandleRegistry::new);

    jni_native! {
        /// `org.tstrans.internal.PanicProbe.nOpenHandle()` — open a registry-backed
        /// probe handle; returns the opaque non-zero key.
        fn Java_org_tstrans_internal_PanicProbe_nOpenHandle<'local>(_env, _class) -> jlong = 0 => {
            PROBE_REGISTRY.insert(0) as jlong
        }
    }

    jni_native! {
        /// `org.tstrans.internal.PanicProbe.nMutateMaybePanic(long, boolean)` — run
        /// a mutation through [`HandleRegistry::with_poisoning`]. When `doPanic` is
        /// `true`, panics mid-mutation; the outer `jni_catch` surfaces the panic as a
        /// `RuntimeException`. A subsequent call on the same (now-poisoned) handle
        /// leases `None` and throws `IllegalStateException`.
        fn Java_org_tstrans_internal_PanicProbe_nMutateMaybePanic<'local>(
            env,
            _class,
            handle: jlong,
            do_panic: jboolean,
        ) = () => {
            let panic = do_panic;
            let outcome = PROBE_REGISTRY.with_poisoning(handle as u64, |v| {
                *v = v.wrapping_add(1); // begin a mutation
                if panic {
                    panic!("probe: torn mutation");
                }
            });
            if outcome.is_none() {
                let _ = env.throw_new(
                    jni_str!("java/lang/IllegalStateException"),
                    jni_str!("probe handle is closed or poisoned"),
                );
            }
        }
    }

    jni_native! {
        /// `org.tstrans.internal.PanicProbe.nCloseHandle(long)` — close the probe
        /// handle; idempotent, safe even on a poisoned handle.
        fn Java_org_tstrans_internal_PanicProbe_nCloseHandle<'local>(
            _env,
            _class,
            handle: jlong,
        ) = () => {
            PROBE_REGISTRY.close(handle as u64);
        }
    }
}

jni_native! {
    /// `org.tstrans.internal.HlsProbe.nRaise(kind, message)` — raise an
    /// `HlsException` of the named Java member through the real throw path, so
    /// `HlsErrorModelTest.everyKindRoundTripsFromRust` pins the mapping.
    fn Java_org_tstrans_internal_HlsProbe_nRaise<'local>(
        env,
        _class,
        kind: jni::objects::JString<'local>,
        message: jni::objects::JString<'local>,
    ) = () => {
        use tst_pipeline::binding::BindingErrorKind as K;

        let kind: String = match kind.try_to_string(env) {
            Ok(s) => s,
            Err(e) => {
                let _ = env.throw_new(jni_str!("java/lang/RuntimeException"), JNIString::from(e.to_string()));
                return;
            }
        };
        let message: String = match message.try_to_string(env) {
            Ok(s) => s,
            Err(e) => {
                let _ = env.throw_new(jni_str!("java/lang/RuntimeException"), JNIString::from(e.to_string()));
                return;
            }
        };
        let k = match kind.as_str() {
            "IO" => K::HlsIo,
            "INVALID_CONFIG" => K::HlsInvalidConfig,
            "FINISHED" => K::HlsFinished,
            "TLS" => K::HlsTls,
            "URL" => K::HlsUrl,
            "BIND_FAILED" => K::HlsBindFailed,
            "UNALIGNED_PUSH_TS" => K::HlsUnalignedPushTs,
            "TLS_DISABLED" => K::HlsTlsDisabled,
            "CLOSED" => K::Closed,
            "INTERNAL" => K::Internal,
            other => {
                let _ = env.throw_new(
                    jni_str!("java/lang/IllegalArgumentException"),
                    JNIString::from(format!("HlsProbe: unknown kind {other}")),
                );
                return;
            }
        };
        crate::hls::errors::throw_hls(env, k, &message);
    }
}
