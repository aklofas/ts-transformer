//! JVM (JNI) bindings.
//!
//! Exports the bootstrap `org.tstrans.Version.versionString()` plus the
//! mpegts-demux keystone (`org.tstrans.mpegts.Demuxer` + `DemuxEvent`); see
//! `mod mpegts`. The `srt` module backs `org.tstrans.srt` (Socket/Listener/
//! Sender/Receiver/CancelHandle/Stats). The remaining `org.tstrans.*` modules
//! mirror tst-py's surface.

// A binding never aborts its host process on a panic it could have avoided:
// no `.expect(` and no `unreachable!` outside tests.
#![cfg_attr(not(test), deny(clippy::expect_used, clippy::unreachable))]

#[cfg(tst_lint_fixture)]
#[path = "../../../scripts/ratchets/tests/lint_fixtures/jvm.rs"]
mod lint_fixture;

mod codec;
mod custom;
mod error;
mod generated;
mod handle;
mod hls;
mod jutil;
mod klv;
mod mpegts;
mod panic;
mod pipeline;
mod rtp;
mod shells;
mod srt;
mod webrtc;

#[cfg(feature = "jni-test-hooks")]
mod test_hooks;

use crate::panic::jni_native;
use jni::sys::{JNI_VERSION_1_8, jint, jstring};
use std::os::raw::c_void;

/// `TSTRANS_LOG` bridge — the Rust core (`tst-core`/`tst-rtp`/
/// `tst-pipeline`) emits `tracing` events, but embedding them in a JVM
/// process installs no subscriber, so every `info!`/`warn!`/`debug!` call
/// is silently discarded — a field integrator lost a diagnosis day to
/// exactly this (the Python binding has the same bridge for the same
/// reason). Iff `TSTRANS_LOG` is set at load time, install a
/// stderr subscriber filtered by its value (the same syntax as
/// `RUST_LOG`/`EnvFilter`); unset ⇒ install nothing, so a bare
/// `System.load` has zero subscriber overhead beyond the env lookup.
/// `try_init` (not `init`) so a host process that already installed its
/// own subscriber keeps it — this bridge never displaces one. ANSI color
/// codes are gated on `stderr` actually being a terminal
/// (`std::io::IsTerminal`, stable since 1.85) — a piped or redirected
/// stderr (log files, a Gradle test's captured output) gets plain text,
/// no stray escape sequences.
fn init_tracing_bridge() {
    use std::io::IsTerminal;

    if let Ok(filter) = std::env::var("TSTRANS_LOG") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_writer(std::io::stderr)
            .with_ansi(std::io::stderr().is_terminal())
            .try_init();
    }
}

/// `JNI_OnLoad` — the JVM invokes this once when `System.load`ing
/// `libtstjni`, before any `Java_org_tstrans_*` native method is
/// reachable. This is the JVM-binding equivalent of the Python
/// extension's module-init hook (`_native()` in the Python binding):
/// the one guaranteed one-time entry point, so it is where the
/// `TSTRANS_LOG` bridge installs itself.
#[unsafe(no_mangle)]
pub extern "system" fn JNI_OnLoad(vm: *mut jni::sys::JavaVM, _reserved: *mut c_void) -> jint {
    init_tracing_bridge();
    // No panic may unwind into the JVM; a failed warm-up only leaves a cache cold.
    let _ = std::panic::catch_unwind(|| warm_jni_type_caches(vm));
    JNI_VERSION_1_8
}

/// Fill jni 0.22's per-type class caches once, through the system class loader.
/// jni fills each cache on first use through the calling thread's context class
/// loader, and a context loader that throws on every lookup sends that path into
/// recursion until the JVM dies with SIGSEGV; a filled cache is never looked up again.
fn warm_jni_type_caches(vm: *mut jni::sys::JavaVM) {
    use jni::exceptions::*;
    use jni::objects::*;
    use jni::refs::{LoaderContext, Reference};
    macro_rules! warm {
        ($env:ident, $ctx:ident; $($t:ty),* $(,)?) => {
            $( let _ = <$t as Reference>::lookup_class($env, &$ctx); )*
        };
    }
    // SAFETY: the JVM passes JNI_OnLoad a valid JavaVM pointer.
    let vm = unsafe { jni::JavaVM::from_raw(vm) };
    let _ = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
        // ClassLoader resolves itself with FindClass, never a context loader.
        let system = JClassLoader::get_system_class_loader(env)?;
        let ctx = LoaderContext::Loader(&system);
        // The list mirrors the types jni 0.22.4 caches: the 18 exceptions bound
        // by `bind_exception!` in its `src/exceptions.rs` (lines 62-171); the
        // 11 types bound by `bind_java_type!` under `src/objects/` (`JThread`,
        // `JClass` and `JClassLoader` are left out, their lookups use
        // `FindClass`) plus the hand-written `JObject`; and the eight
        // primitive arrays of `impl_ref_for_jprimitive_array!` in
        // `src/objects/jprimitive_array.rs`. A type missing here stays cold
        // without any warning, so re-check the list against those files on
        // every jni bump, patch releases included.
        warm!(env, ctx; JThrowable, JObject, JString, JCharSequence, JStackTraceElement,
            JByteBuffer, JCollection, JList, JSet, JIterator, JMap, JMapEntry,
            JBooleanArray, JByteArray, JCharArray, JShortArray, JIntArray, JLongArray,
            JFloatArray, JDoubleArray, JArrayIndexOutOfBoundsException, JArrayStoreException,
            JClassCircularityError, JClassFormatError, JExceptionInInitializerError,
            JClassNotFoundException, JIllegalArgumentException, JIllegalMonitorStateException,
            JInstantiationException, JLinkageError, JNoClassDefFoundError, JNoSuchFieldError,
            JNoSuchMethodError, JNumberFormatException, JOutOfMemoryError, JRuntimeException,
            JSecurityException, JStringIndexOutOfBoundsException);
        Ok(())
    });
}

jni_native! {
    /// `org.tstrans.NativeLoader.nVerifyKinds()` — see
    /// `crate::error::verify_kind_tables`. Resolves every error kind this
    /// library can raise against the `*Exception.Kind` enums of the JAR that
    /// just loaded it, so a JAR/native mismatch fails at `System.load` naming
    /// both sides instead of at the first failing call.
    ///
    /// Not done in `JNI_OnLoad`: an exception pending when `JNI_OnLoad` returns
    /// reaches Java as `UnsatisfiedLinkError("exception occurred in JNI_OnLoad")`
    /// with the detail dropped; a native called right after `System.load`
    /// surfaces the exact mismatch.
    fn Java_org_tstrans_NativeLoader_nVerifyKinds(env, _class) = () => {
        crate::error::verify_kind_tables(env)
    }
}

jni_native! {
    /// `org.tstrans.Version.versionString()` — returns the Rust workspace crate
    /// version (e.g. "0.2.0") as a Java string, proving a value crosses the JNI
    /// boundary from Rust to the JVM.
    ///
    /// `jni_native!` emits `#[unsafe(no_mangle)]` (edition 2024 spelling), which
    /// keeps the symbol name exactly `Java_org_tstrans_Version_versionString` so
    /// the JVM's native-method linker resolves it.
    fn Java_org_tstrans_Version_versionString<'local>(
        env,
        _class,
    ) -> jstring = std::ptr::null_mut() => {
        // A failed JNI allocation has already left an OutOfMemoryError pending
        // on this thread; returning null lets the JVM raise it instead of
        // aborting the process from Rust.
        match env.new_string(env!("CARGO_PKG_VERSION")) {
            Ok(js) => js.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `init_tracing_bridge` touches two pieces of process-global state:
    /// the `TSTRANS_LOG` env var and `tracing`'s process-global dispatcher
    /// (which can only ever be installed once — `try_init` is a
    /// deliberate no-op on every call after the first, per the bridge's
    /// own doc comment above). A forked-JVM integration test (the honest
    /// end-to-end check) is impractical in this harness — see the PR
    /// report for the manual Gradle verification that covers the real
    /// stderr-output path. This is ONE test covering both the unset and
    /// the set branch in a single serialized run, rather than two
    /// `#[test]` fns that could interleave under cargo's parallel-by-default
    /// test runner and race the same env var / dispatcher.
    #[test]
    fn init_tracing_bridge_installs_only_when_set() {
        // SAFETY: env mutation is process-wide, not memory-unsafe; this
        // is the sole test in the crate touching TSTRANS_LOG.
        unsafe {
            std::env::remove_var("TSTRANS_LOG");
        }
        assert!(
            !tracing::dispatcher::has_been_set(),
            "no subscriber should be installed before the bridge ever runs"
        );

        init_tracing_bridge();
        assert!(
            !tracing::dispatcher::has_been_set(),
            "TSTRANS_LOG unset must install nothing"
        );

        // SAFETY: see above.
        unsafe {
            std::env::set_var("TSTRANS_LOG", "info");
        }
        init_tracing_bridge();
        assert!(
            tracing::dispatcher::has_been_set(),
            "TSTRANS_LOG set must install a subscriber"
        );

        // A second call (e.g. a second `System.load` of the same process)
        // must not panic even though a subscriber is already installed —
        // this is the "host process already installed its own subscriber"
        // production case that `try_init` (not `init`) exists to handle.
        init_tracing_bridge();

        // SAFETY: see above.
        unsafe {
            std::env::remove_var("TSTRANS_LOG");
        }
    }
}
