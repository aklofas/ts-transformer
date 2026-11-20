//! Test-only JNI hooks, compiled solely under `feature = "jni-test-hooks"`.
//! The published cdylib is built WITHOUT this feature (CI rebuilds probe-free
//! before staging the shipped lib — see `.github/workflows/jvm-jar.yml`), so
//! these symbols never reach Maven Central.

use crate::jutil::FindClass;
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

    use crate::jutil::FindClass;
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
                    FindClass(jni_str!("java/lang/IllegalStateException")),
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
                let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.to_string()));
                return;
            }
        };
        let message: String = match message.try_to_string(env) {
            Ok(s) => s,
            Err(e) => {
                let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.to_string()));
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
                    FindClass(jni_str!("java/lang/IllegalArgumentException")),
                    JNIString::from(format!("HlsProbe: unknown kind {other}")),
                );
                return;
            }
        };
        crate::hls::errors::throw_hls(env, k, &message);
    }
}

// ---------------------------------------------------------------------------
// KLV error-model probes: force a KlvDecodeException / KlvEncodeException of a
// named Kind so the error-mapping path can be tested end to end.
// ---------------------------------------------------------------------------

mod klv_probe {
    use crate::panic::jni_native;
    use jni::objects::JString;

    jni_native! {
        /// `org.tstrans.internal.KlvProbe.nRaiseDecode(kind)` — throws a
        /// `KlvDecodeException` with the given Kind name.
        fn Java_org_tstrans_internal_KlvProbe_nRaiseDecode<'local>(
            env,
            _class,
            kind: JString<'local>,
        ) = () => {
            let k: String = kind.try_to_string(env).unwrap_or_default();
            crate::error::throw_klv_decode_raw(env, &k, "forced decode error for test");
        }
    }

    jni_native! {
        /// `org.tstrans.internal.KlvProbe.nRaiseEncode(kind)` — throws a
        /// `KlvEncodeException` with the given Kind name (no tag).
        fn Java_org_tstrans_internal_KlvProbe_nRaiseEncode<'local>(
            env,
            _class,
            kind: JString<'local>,
        ) = () => {
            let k: String = kind.try_to_string(env).unwrap_or_default();
            crate::error::throw_klv_encode_raw(env, &k, None, "forced encode error for test");
        }
    }
}

// ---------------------------------------------------------------------------
// Cancel-view probe: a `CancelHandle` over a transport cancel that latches and
// then blocks until the test releases it, the way SRT's `cancel()` can sit in
// `srt_close` for the socket's linger. Lets a test call the handle's other
// methods while one thread is inside `cancel()`.
// ---------------------------------------------------------------------------

mod cancel_probe {
    use crate::handle::{CancelView, HandleRegistry};
    use crate::panic::jni_native;
    use jni::sys::{jboolean, jlong};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, LazyLock, Mutex};
    use std::time::{Duration, Instant};
    use tst_core::transport::TransportCancel;

    /// Upper bound on how long `cancel()` stays parked, so a test that never
    /// releases the gate cannot hang the test JVM.
    const MAX_PARK: Duration = Duration::from_secs(60);

    #[derive(Default)]
    struct Gate {
        cancelled: AtomicBool,
        /// (a thread is inside `cancel()`, the test released the gate)
        state: Mutex<(bool, bool)>,
        cv: Condvar,
    }

    impl TransportCancel for Gate {
        fn cancel(&self) {
            self.cancelled.store(true, Ordering::SeqCst);
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.0 = true;
            self.cv.notify_all();
            let _ = self
                .cv
                .wait_timeout_while(s, MAX_PARK, |s| !s.1)
                .unwrap_or_else(|e| e.into_inner());
        }

        fn is_cancelled(&self) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }
    }

    static GATES: LazyLock<HandleRegistry<Arc<Gate>>> = LazyLock::new(HandleRegistry::new);

    fn gate(probe: jlong) -> Option<Arc<Gate>> {
        GATES.with(probe as u64, |g| Arc::clone(g))
    }

    jni_native! {
        /// `org.tstrans.internal.CancelProbe.nOpen()` — a new blocking gate; returns its probe key.
        fn Java_org_tstrans_internal_CancelProbe_nOpen<'local>(_env, _class) -> jlong = 0 => {
            GATES.insert(Arc::new(Gate::default())) as jlong
        }
    }

    jni_native! {
        /// `org.tstrans.internal.CancelProbe.nCancelView(long)` — register a cancel
        /// view over the gate; returns the key an `org.tstrans.shells.CancelHandle` wraps.
        fn Java_org_tstrans_internal_CancelProbe_nCancelView<'local>(
            _env,
            _class,
            probe: jlong,
        ) -> jlong = 0 => {
            match gate(probe) {
                Some(g) => crate::shells::cancel::cancel_view_handle(CancelView(g)),
                None => 0,
            }
        }
    }

    jni_native! {
        /// `org.tstrans.internal.CancelProbe.nHoldOpen(long, OpenCancel)` — register
        /// one open with the `OpenCancel` whose wake is the gate, so the object's
        /// `cancel()` blocks inside it. The open never finishes: its guard is
        /// leaked, and only `cancel()` takes its slot from the object.
        fn Java_org_tstrans_internal_CancelProbe_nHoldOpen<'local>(
            env,
            _class,
            probe: jlong,
            cancel: jni::objects::JObject<'local>,
        ) = () => {
            let Some(g) = gate(probe) else { return };
            let Ok(Some(oc)) = crate::shells::open_cancel::object_of(env, &cancel) else {
                return;
            };
            let slot = oc.slot();
            slot.install(g);
            std::mem::forget(slot);
        }
    }

    jni_native! {
        /// `org.tstrans.internal.CancelProbe.nAwaitEntered(long, long)` — wait up to
        /// `timeoutMs` for a thread to be inside the gate's `cancel()`.
        fn Java_org_tstrans_internal_CancelProbe_nAwaitEntered<'local>(
            _env,
            _class,
            probe: jlong,
            timeout_ms: jlong,
        ) -> jboolean = false => {
            let Some(g) = gate(probe) else { return false };
            let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(0) as u64);
            let mut s = g.state.lock().unwrap_or_else(|e| e.into_inner());
            while !s.0 {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return false;
                }
                s = g.cv.wait_timeout(s, left).unwrap_or_else(|e| e.into_inner()).0;
            }
            true
        }
    }

    jni_native! {
        /// `org.tstrans.internal.CancelProbe.nRelease(long)` — let every parked `cancel()` return.
        fn Java_org_tstrans_internal_CancelProbe_nRelease<'local>(_env, _class, probe: jlong) = () => {
            if let Some(g) = gate(probe) {
                g.state.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
                g.cv.notify_all();
            }
        }
    }

    jni_native! {
        /// `org.tstrans.internal.CancelProbe.nClose(long)` — drop the probe key; idempotent.
        fn Java_org_tstrans_internal_CancelProbe_nClose<'local>(_env, _class, probe: jlong) = () => {
            GATES.close(probe as u64);
        }
    }
}

// ---------------------------------------------------------------------------
// org.tstrans.internal.SlotProbe — puts an `H264Receiver` into the state a
// racing `close()` leaves between taking the receiver and a receive that had
// already leased its id: the slot empty, the id still registered.
// ---------------------------------------------------------------------------

jni_native! {
    /// `org.tstrans.internal.SlotProbe.nEmptyH264ReceiverSlot(long)` — empty the
    /// receiver's slot, keep its id; `true` if a receiver was taken.
    fn Java_org_tstrans_internal_SlotProbe_nEmptyH264ReceiverSlot<'local>(
        _env,
        _class,
        handle: jlong,
    ) -> jni::sys::jboolean = false => {
        crate::rtp::h264_receiver::empty_slot_keep_id(handle)
    }
}

jni_native! {
    /// `org.tstrans.internal.SlotProbe.nHeld(String, long)` — whether a call holds
    /// the slot of the shell `handle` names in the registry of the Java class
    /// `type` (handles are numbered per class); never waits. An unknown class
    /// throws `IllegalArgumentException`.
    fn Java_org_tstrans_internal_SlotProbe_nHeld<'local>(
        env,
        _class,
        type_name: jni::objects::JString<'local>,
        handle: jlong,
    ) -> jni::sys::jboolean = false => {
        let name: String = match type_name.try_to_string(env) {
            Ok(s) => s,
            Err(e) => {
                let _ = env.throw_new(FindClass(jni_str!("java/lang/RuntimeException")), JNIString::from(e.to_string()));
                return false;
            }
        };
        match name.as_str() {
            "org.tstrans.shells.Sender" => crate::shells::sender::held(handle),
            "org.tstrans.shells.Receiver" => crate::shells::receiver::held(handle),
            "org.tstrans.shells.MuxSender" => crate::shells::mux_sender::held(handle),
            "org.tstrans.shells.DemuxReceiver" => crate::shells::demux_receiver::held(handle),
            "org.tstrans.tcp.Listener" => crate::tcp::held(handle),
            other => {
                let _ = env.throw_new(
                    FindClass(jni_str!("java/lang/IllegalArgumentException")),
                    JNIString::from(format!("no slot probe for {other}")),
                );
                false
            }
        }
    }
}
