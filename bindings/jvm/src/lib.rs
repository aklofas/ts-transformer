//! JVM (JNI) bindings.
//!
//! Exports the bootstrap `org.tstrans.Version.versionString()` plus the Wave 1
//! mpegts-demux keystone (`org.tstrans.mpegts.Demuxer` + `DemuxEvent`); see
//! `mod mpegts`. The `srt` module backs `org.tstrans.srt` (Socket/Listener/
//! Sender/Receiver/CancelHandle/Stats). The remaining `org.tstrans.*` modules
//! (mirroring tst-py) land in the follow-on surface-port waves.

mod codec;
mod error;
mod handle;
mod jutil;
mod klv;
mod mpegts;
mod panic;
mod pipeline;
mod rtp;
mod srt;

#[cfg(feature = "jni-test-hooks")]
mod test_hooks;

use jni::JNIEnv;
use jni::JavaVM;
use jni::objects::JClass;
use jni::sys::{JNI_VERSION_1_8, jint, jstring};
use std::os::raw::c_void;

/// `TSTRANS_LOG` bridge (Task D8) — the Rust core (`tst-core`/`tst-rtp`/
/// `tst-pipeline`) emits `tracing` events, but embedding them in a JVM
/// process installs no subscriber, so every `info!`/`warn!`/`debug!` call
/// is silently discarded — a field integrator lost a diagnosis day to
/// exactly this (the Python bridge, Task C7, is the same fix for the
/// Python binding). Iff `TSTRANS_LOG` is set at load time, install a
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
pub extern "system" fn JNI_OnLoad(_vm: JavaVM, _reserved: *mut c_void) -> jint {
    init_tracing_bridge();
    JNI_VERSION_1_8
}

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
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_NativeLoader_nVerifyKinds(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) {
    crate::panic::jni_catch(&mut env, (), |env| crate::error::verify_kind_tables(env));
}

/// `org.tstrans.Version.versionString()` — returns the Rust workspace crate
/// version (e.g. "0.2.0") as a Java string, proving a value crosses the JNI
/// boundary from Rust to the JVM.
///
/// `#[unsafe(no_mangle)]` (edition 2024 spelling) keeps the symbol name exactly
/// `Java_org_tstrans_Version_versionString` so the JVM's native-method linker
/// resolves it.
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_tstrans_Version_versionString<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    crate::panic::jni_catch(&mut env, std::ptr::null_mut(), |env| {
        env.new_string(env!("CARGO_PKG_VERSION"))
            .expect("failed to allocate Java string")
            .into_raw()
    })
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
