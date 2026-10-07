//! JNI surface for `org.tstrans.hls` — the tst-hls publisher family.
//!
//! Ports tst-py's `bindings/python/src/hls/`. Three leased registries, one per
//! Java handle class (`HlsPublisher`, `HlsServerHandle`, `MuxPublisher`); every
//! native body runs inside `crate::panic::jni_catch`. Errors go through
//! `crate::error::throw_binding` with `Domain::Hls` — no local kind tables.
//!
//! Lock posture: nothing in HLS parks on a peer (every push is bounded disk
//! I/O), so the plain `HandleRegistry` (no cancel hook) is the right entry
//! shape. `close()` on a handle whose push is in flight waits for that push's
//! segment write, then takes the resource — documented on the Java classes.

pub(crate) mod errors;
mod mux_publisher;
mod publisher;
mod server_handle;
