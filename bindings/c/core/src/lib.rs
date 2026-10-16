//! `tst-c-core` — embeddable C-ABI core for the ts-transformer pipeline.
//!
//! This is an rlib (no_std-capable) that holds all the `extern "C"` logic.
//! The `tst-c` leaf crate re-exports it to produce `libtstrans.so` / `.a`
//! plus the cbindgen-generated `tstrans.h`; bare-metal firmware depends on
//! this core directly. Rust callers should consume `tst-pipeline` and
//! `tst-srt` directly rather than going through the C ABI.
//!
//! Both directions are covered: senders, and raw byte, TS-aligned and
//! typed demux-event receivers (`tst_raw_receiver_*` / `tst_receiver_*` /
//! `tst_demux_receiver_*`), along with the reconnecting `tst_managed_*`
//! variants. RTP and RTSP
//! transport surfaces are gated on the `rtp` cargo feature. The
//! offline byte-feeding `tst_demuxer_*` surface is unconditional (no
//! feature gate), as is the offline `tst_muxer_*` surface (un-gated from
//! `srt` in ABI 0.9). ABI minor is `0.24` (see [`TST_ABI_VERSION_MINOR`]).

#![cfg_attr(not(feature = "std"), no_std)]
// every extern "C" fn has a /// header documenting the contract
#![allow(clippy::missing_safety_doc)]
// A binding never aborts its host process on a panic it could have avoided:
// no `.expect(` and no `unreachable!` outside tests.
#![cfg_attr(not(test), deny(clippy::expect_used, clippy::unreachable))]

#[cfg(tst_lint_fixture)]
#[path = "../../../../scripts/ratchets/tests/lint_fixtures/c_core.rs"]
mod lint_fixture;

extern crate alloc;

// The host `cargo test` binary always links std (libtest needs it); this lets
// the unit tests of the no_std feature plane reach `std::fs`/`std::path` for
// fixture reads without touching the library's own no_std proof — the MCU
// builds in CI are plain `cargo build`, where this line is compiled out.
#[cfg(test)]
extern crate std;

// ---------------------------------------------------------------------------
// C primitive type aliases
//
// `libc` does not export `c_int`, `c_char`, or `size_t` on bare-metal
// targets (no OS = no C library). Under std, use `libc::*` for
// cbindgen compatibility. Under no_std, alias from `core::ffi`.
// ---------------------------------------------------------------------------
#[cfg(feature = "std")]
pub(crate) mod c_types {
    pub(crate) use libc::{c_char, c_int};
    #[allow(non_camel_case_types)]
    pub(crate) type size_t = libc::size_t;
}
#[cfg(not(feature = "std"))]
pub(crate) mod c_types {
    pub(crate) use core::ffi::{c_char, c_int};
    #[allow(non_camel_case_types)]
    pub(crate) type size_t = usize;
}

// ---------------------------------------------------------------------------
// no_std lang items
//
// Under no_std, tst-c is consumed as an *rlib* by a downstream glue crate
// (the bare-metal staticlib/firmware binary). That embedding crate — not
// tst-c — supplies the `#[global_allocator]` and `#[panic_handler]`, exactly
// as tst-core and tst-pipeline leave them to their embedder. Defining either
// here would cause a duplicate-lang-item / duplicate-symbol link error when
// the glue crate links its own.
// ---------------------------------------------------------------------------

// `critical-section` (the no_std last-error slot in error.rs) is referenced
// only under no_std, and Cargo cannot express "dependency only when a feature
// is OFF" — so on std builds it looks unused to cargo's (nightly)
// `unused_dependencies` lint. The conventional `use … as _` marks it as
// deliberately present.
#[cfg(feature = "std")]
use critical_section as _;

// Cross-cutting (shared by both sender and receiver):
pub mod codec_framing;
pub mod config;
pub mod demux_config;
pub mod demuxer;
pub mod error;
pub mod event;
pub mod handle;
pub mod klv_st0601;
pub mod misp_time;
pub mod muxer;
pub mod stats;
pub mod stream_end_reason;
mod ffi_slice;
mod panic;
#[cfg(test)]
mod test_support;
// Shared C-string -> &str bridges (`parse_c_str` for the transport URL
// arguments and `rtp://` URLs; `c_str_arg` and `credentials` for the RTSP
// entry points). Gated on the union of the consuming features: with all of
// them off nothing calls it and `-D warnings` clippy rejects the dead code.
#[cfg(any(
    feature = "udp",
    feature = "tcp",
    feature = "rist",
    feature = "hls",
    feature = "rtp"
))]
mod c_str;
// Generic transport body impls shared across the family modules. Gated on the
// UNION of the consuming transport features (not just `std`): with all
// transports off, nothing calls these generic bodies and `-D warnings` clippy
// rejects the dead code in the default (transport-less) build.
#[cfg(any(
    feature = "srt",
    feature = "rtp",
    feature = "udp",
    feature = "tcp",
    feature = "rist"
))]
pub(crate) mod transport_impls;

// Sender-side surface (requires SRT transport):
#[cfg(feature = "srt")]
pub mod sender;

// Receiver-side surface (requires SRT transport):
#[cfg(feature = "srt")]
pub mod receiver;

// RTP/RTSP transport surface:
#[cfg(feature = "rtp")]
pub mod rtp;
#[cfg(feature = "rtp")]
mod rtsp;

// udp / tcp / hls / rist transport surfaces (all default-off):
#[cfg(feature = "udp")]
pub mod udp;
#[cfg(feature = "tcp")]
pub mod tcp;
#[cfg(feature = "hls")]
pub mod hls;
#[cfg(feature = "rist")]
pub mod rist;
/// Re-exports of internal error helpers for integration tests. These are not
/// `extern "C"` and do not appear in `tstrans.h`. Named with `test_` prefix
/// to mark their test-only intent; not gated on `#[cfg(test)]` because
/// integration tests in `tests/` are separate crates that cannot see
/// `pub(crate)` items.
pub use error::{test_clear_last_error, test_last_error_code, test_last_error_msg};
// `record_shell_error` projects `ShellErrorKind` through the std-only
// binding kind table, so its test alias follows the same gate.
#[cfg(feature = "std")]
pub use error::test_record_shell_error;

// Feature-gated re-exports used by the `feature_matrix_compile` integration
// test to verify that each cargo feature exposes the expected entry points.
// These are thin aliases — the real `extern "C"` symbols are in their
// respective submodules; integration tests in `tests/` cannot see
// `pub(crate)` paths, so flat re-exports at crate root are needed.
#[cfg(feature = "rtp")]
pub use rtp::{tst_rtp_mux_sender_open, tst_rtp_sender_open};
#[cfg(feature = "rtp")]
pub use rtsp::client::builder::tst_rtsp_client_builder_new;
#[cfg(feature = "rtp")]
pub use rtsp::server::builder::tst_rtsp_server_builder_new;
// RTSP server lifecycle + publisher role (ABI 23), re-exported flat for the
// `transports::rtsp_publish_smoke` integration test.
#[cfg(feature = "rtp")]
pub use rtsp::server::builder::tst_rtsp_server_builder_accept_unregistered_publishers;
#[cfg(feature = "rtp")]
pub use rtsp::server::publish::{
    tst_rtsp_publish_mount_cancel, tst_rtsp_publish_mount_free, tst_rtsp_publish_mount_generation,
    tst_rtsp_publish_mount_get_stats, tst_rtsp_publish_mount_into_demux_receiver,
    tst_rtsp_publish_mount_path, tst_rtsp_publish_mount_peer_count,
    tst_rtsp_publish_mount_publisher_info, tst_rtsp_server_active_publishers,
    tst_rtsp_server_add_publish_mount, tst_rtsp_server_next_publisher,
    tst_rtsp_server_remove_mount, tst_rtsp_server_total_rtp_bytes_received,
    tst_rtsp_server_total_rtp_packets_received,
};
#[cfg(feature = "rtp")]
pub use rtsp::server::start::tst_rtsp_server_builder_start;
#[cfg(feature = "rtp")]
pub use rtsp::server::stop::{
    tst_rtsp_server_free, tst_rtsp_server_get_stats, tst_rtsp_server_local_addr,
    tst_rtsp_server_stop,
};
#[cfg(feature = "rtp")]
pub use rtsp::server::types::{
    TST_RTSP_PEER_ADDR_LEN, TstRtspClockAlignment, TstRtspPublishMount, TstRtspPublishMountStats,
    TstRtspPublishShape, TstRtspPublisherInfo, TstRtspServer,
};
#[cfg(feature = "srt")]
pub use sender::ts_sender::tst_sender_open;

// Feature-gated transport re-exports. One line per transport; all now shipped
// (udp, tcp, hls, rist). Gated so a minimal `--no-default-features` build
// with no transport feature selected links without any transport symbols.
#[cfg(feature = "hls")]
pub use hls::builder::tst_hls_publisher_builder_new;
#[cfg(feature = "rist")]
pub use rist::tst_rist_sender_open;
#[cfg(feature = "tcp")]
pub use tcp::{tst_tcp_mux_sender_open, tst_tcp_recv_open, tst_tcp_sender_open};
#[cfg(feature = "udp")]
pub use udp::{tst_udp_mux_sender_open, tst_udp_recv_open, tst_udp_sender_open};

/// Major version (compile-time macro in the generated header).
pub const TST_VERSION_MAJOR: crate::c_types::c_int = 0;
/// Minor version.
pub const TST_VERSION_MINOR: crate::c_types::c_int = 8;
/// Patch version.
pub const TST_VERSION_PATCH: crate::c_types::c_int = 0;

/// Major version of the C ABI contract. Bumped only on **breaking
/// C-ABI change** — i.e., a change that would force a consumer to
/// rebuild against a different `tstrans.h`. NOT bumped on:
///
/// - Cargo package version bumps (track `TST_VERSION_*` for that).
/// - Adding new `tst_*` functions or `TST_*` macros (backwards-compatible).
/// - Adding new `TstError::*` codes (backwards-compatible; old codes
///   remain stable per the documented `#[repr(i32)]` policy in
///   `bindings/c/core/src/error.rs`).
///
/// Rule (additive-only): within a major, the minor bumps for ADDITIONS
/// only — new symbols, new struct fields only in NEW structs (existing struct
/// layouts are frozen until a sized/versioned snapshot convention exists), new enum values, new error codes. REMOVING or renaming a `tst_*`
/// symbol, changing an existing function's signature or semantic contract,
/// or changing an existing struct's layout/size (the `_Static_assert`
/// lines in the header trailer) is a MAJOR bump. Pre-1.0 the major stays
/// at 0, so those changes wait for 1.0; until then they are made by adding
/// a new symbol/struct beside the old one.
///
/// **Initial value:** `0.1`. Settles to `1.0` at the first stable release.
///
/// Cbindgen emits this as `#define TST_ABI_VERSION_MAJOR 0` in the
/// generated header. Runtime accessor: [`tst_get_abi_version_major`].
pub const TST_ABI_VERSION_MAJOR: crate::c_types::c_int = 0;

/// Minor version of the C ABI contract. See [`TST_ABI_VERSION_MAJOR`]
/// for the bump policy.
///
/// Cbindgen emits this as `#define TST_ABI_VERSION_MINOR 24` in the
/// generated header. Runtime accessor: [`tst_get_abi_version_minor`].
///
/// What each minor added is recorded once, in the repository's
/// `docs/reference/binding-authors.md` § "Versioning".
pub const TST_ABI_VERSION_MINOR: crate::c_types::c_int = 24;

// =========================================================================
// Runtime version accessors
// =========================================================================
//
// Bindings (tst-jni, tst-uniffi, pure-C consumers) use these to verify
// the loaded shared object matches the header they compiled against:
//
//     assert(tst_get_abi_version_major() == TST_ABI_VERSION_MAJOR);
//     assert(tst_get_abi_version_minor() >= TST_ABI_VERSION_MINOR);
//
// See examples/getting-started/version_check.c for the canonical pattern.

/// Returns the C ABI contract major version at runtime.
///
/// Always returns the value of [`TST_ABI_VERSION_MAJOR`] cast to `u32`.
/// Use the compile-time macro `TST_ABI_VERSION_MAJOR` in `tstrans.h` to
/// learn what the header you compiled against expects; compare against
/// this runtime value to detect SO/header mismatches.
///
/// # Safety
///
/// Sound under any caller invocation — no pointer arguments, no
/// mutating state, no internal locks. The `unsafe extern "C"`
/// annotation matches the convention of every other `tst_*` entry
/// point for consistency.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_get_abi_version_major() -> u32 {
    TST_ABI_VERSION_MAJOR as u32
}

/// Returns the C ABI contract minor version at runtime.
///
/// See [`tst_get_abi_version_major`] for the binding-author usage
/// pattern and the bump policy.
///
/// # Safety
///
/// Sound under any caller invocation; see [`tst_get_abi_version_major`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_get_abi_version_minor() -> u32 {
    TST_ABI_VERSION_MINOR as u32
}

// =========================================================================
// Package version (matches Cargo.toml)
// =========================================================================
//
// Bindings query these at runtime to surface the loaded library's
// version to their consumers (e.g., `tstrans.version` in a Java toString).
// Distinct from the ABI version above — the package version bumps every
// release (patches/minors/majors per SemVer), while the ABI version only
// bumps on breaking C-ABI change.

/// Returns the package major version at runtime — matches
/// `Cargo.toml`'s major field at the time `libtstrans` was built.
///
/// Always equal to [`TST_VERSION_MAJOR`] cast to `u32`. Cross-validate
/// against the compile-time header macro to detect SO/header mismatches:
///
/// ```c
/// if (tst_get_version_major() != TST_VERSION_MAJOR) {
///     fprintf(stderr, "tstrans header/SO version mismatch\n");
///     return 1;
/// }
/// ```
///
/// # Safety
///
/// Sound under any caller invocation; see [`tst_get_abi_version_major`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_get_version_major() -> u32 {
    TST_VERSION_MAJOR as u32
}

/// Returns the package minor version at runtime. See
/// [`tst_get_version_major`] for the usage pattern.
///
/// # Safety
///
/// Sound under any caller invocation; see [`tst_get_version_major`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_get_version_minor() -> u32 {
    TST_VERSION_MINOR as u32
}

/// Returns the package patch version at runtime. See
/// [`tst_get_version_major`] for the usage pattern.
///
/// # Safety
///
/// Sound under any caller invocation; see [`tst_get_version_major`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_get_version_patch() -> u32 {
    TST_VERSION_PATCH as u32
}

/// Returns the package version packed as `(M << 16) | (m << 8) | p`.
///
/// Lets binding authors compare versions as single integers:
///
/// ```c
/// // "at least 0.1.2" check
/// if (tst_get_version_packed() < ((0 << 16) | (1 << 8) | 2)) {
///     fprintf(stderr, "tstrans too old\n");
///     return 1;
/// }
/// ```
///
/// Each field caps at 255 (the encoding uses 8 bits per field with the
/// major field shifted into the upper 8 bits of a 24-bit window). Pre-1.0
/// values fit comfortably; revisit if any field ever exceeds 255.
///
/// Convention matches libsrt's `SRT_VERSION_*` packing.
///
/// # Safety
///
/// Sound under any caller invocation; see [`tst_get_version_major`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_get_version_packed() -> u32 {
    let m = TST_VERSION_MAJOR as u32;
    let n = TST_VERSION_MINOR as u32;
    let p = TST_VERSION_PATCH as u32;
    (m << 16) | (n << 8) | p
}

/// Returns a NUL-terminated `"<major>.<minor>.<patch>"` C string at
/// runtime.
///
/// Pointer is valid for the process lifetime (backed by a `'static`
/// Rust string created at compile time via `concat!` of the
/// `env!("CARGO_PKG_VERSION_*")` variables). Caller must NOT free.
///
/// ```c
/// printf("tstrans version: %s\n", tst_get_version_string());
/// ```
///
/// # Safety
///
/// Sound under any caller invocation; the returned pointer is always
/// non-NULL and process-lifetime stable. Reading past the NUL byte is
/// undefined behavior per usual C string rules.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_get_version_string() -> *const crate::c_types::c_char {
    // Construct the NUL-terminated string at compile time from the
    // Cargo-package env vars. The trailing `\0` extends the &str's
    // backing storage by one byte so `.as_ptr()` yields a valid C
    // string. Single source of truth: Cargo.toml.
    static VERSION: &str = concat!(
        env!("CARGO_PKG_VERSION_MAJOR"),
        ".",
        env!("CARGO_PKG_VERSION_MINOR"),
        ".",
        env!("CARGO_PKG_VERSION_PATCH"),
        "\0",
    );
    VERSION.as_ptr() as *const crate::c_types::c_char
}
