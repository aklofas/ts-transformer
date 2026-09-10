//! `tst-c-core` — embeddable C-ABI core for the ts-transformer pipeline.
//!
//! This is an rlib (no_std-capable) that holds all the `extern "C"` logic.
//! The `tst-c` leaf crate re-exports it to produce `libtstrans.so` / `.a`
//! plus the cbindgen-generated `tstrans.h`; bare-metal firmware depends on
//! this core directly. Rust callers should consume `tst-pipeline` and
//! `tst-srt` directly rather than going through the C ABI.
//!
//! Sender side complete; receiver side complete — raw byte, TS-aligned,
//! and typed demux-event surfaces all ship today (`tst_raw_receiver_*` /
//! `tst_ts_receiver_*` / `tst_receiver_*` / `tst_demux_receiver_*`),
//! along with the reconnecting `tst_managed_*` variants. RTP and RTSP
//! transport surfaces are gated on the `rtp` cargo feature. The
//! offline byte-feeding `tst_demuxer_*` surface is unconditional (no
//! feature gate), as is the offline `tst_muxer_*` surface (un-gated from
//! `srt` in ABI 0.9). ABI minor is `0.21` (see [`TST_ABI_VERSION_MINOR`]).

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::missing_safety_doc)] // every extern "C" fn has a /// header documenting the contract

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

// ---------------------------------------------------------------------------
// no_std Mutex seam
//
// Under std: callers import `std::sync::Mutex` directly.
// Under no_std (bare-metal): a spin::Mutex newtype that surfaces the same
// `new() + lock() -> Result<Guard, _>` interface so call sites compile
// verbatim in both modes.
// (The `critical-section` impl a glue crate registers serves the no_std
// last-error slot in error.rs — NOT this seam, which is plain spin.)
//
// No priority inheritance / interrupt masking: the documented embedding
// contract is one-handle-per-task (see handle.rs module docs) — the lock
// exists to satisfy Sync, not to arbitrate cross-task sharing.
// ---------------------------------------------------------------------------
// `spin` (this seam) and `critical-section` (the no_std last-error slot in
// error.rs) are referenced only under no_std, and Cargo cannot express
// "dependency only when a feature is OFF" — so on std builds they look unused
// to cargo's (nightly) `unused_dependencies` lint. The conventional
// `use … as _` marks them as deliberately present.
#[cfg(feature = "std")]
use critical_section as _;
#[cfg(feature = "std")]
use spin as _;
#[cfg(not(feature = "std"))]
pub(crate) mod nostd_mutex {
    //! Single-core no_std lock seam mirroring `std::sync::Mutex`'s surface
    //! (`new` + `lock() -> Result<Guard, _>`) so call sites compile verbatim.
    pub(crate) struct Mutex<T>(spin::Mutex<T>);
    impl<T> Mutex<T> {
        pub(crate) const fn new(v: T) -> Self {
            Self(spin::Mutex::new(v))
        }
        #[allow(clippy::result_unit_err)]
        pub(crate) fn lock(&self) -> Result<spin::MutexGuard<'_, T>, ()> {
            Ok(self.0.lock())
        }
    }
}

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
// Shared C-string -> &str bridge, consumed only by the four transport
// families whose url.rs modules delegate to it (rtp's URL parsing needs
// more than a borrowed &str, so it stays separate — see c_str.rs docs).
// Gated on the union of those features for the same reason transport_impls
// is: with all of udp/tcp/rist/hls off, nothing calls it and -D warnings
// clippy rejects the dead code in the default (transport-less) build.
#[cfg(any(feature = "udp", feature = "tcp", feature = "rist", feature = "hls"))]
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
#[cfg(feature = "srt")]
mod srt_addr;

// RTP/RTSP transport surface:
#[cfg(feature = "rtp")]
pub mod rtp;
#[cfg(feature = "rtp")]
mod rtsp;

// Plan A5a — udp / tcp / hls / rist transport surfaces (all default-off):
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
pub use error::{
    test_clear_last_error, test_last_error_code, test_last_error_msg, test_record_shell_error,
};

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
pub const TST_VERSION_MINOR: crate::c_types::c_int = 6;
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
/// Bumped on:
///
/// - Removing or renaming any `tst_*` symbol.
/// - Changing a struct layout / size in a way that breaks
///   `_Static_assert(sizeof(...) == N)` lines in the header trailer.
/// - Changing the signature of an existing `tst_*` function.
/// - Changing the semantic contract of an existing function in a way
///   that would silently miscompile or misbehave in pre-existing
///   consumers (e.g., flipping return-code polarity, changing
///   buffer-ownership semantics).
///
/// **Initial value:** `0.1` (pre-1.0). The minor field bumps freely
/// during pre-1.0 development on any breaking ABI change. Settles to
/// `1.0` at the first stable release.
///
/// Cbindgen emits this as `#define TST_ABI_VERSION_MAJOR 0` in the
/// generated header. Runtime accessor: [`tst_get_abi_version_major`].
pub const TST_ABI_VERSION_MAJOR: crate::c_types::c_int = 0;

/// Minor version of the C ABI contract. See [`TST_ABI_VERSION_MAJOR`]
/// for the bump policy.
///
/// Cbindgen emits this as `#define TST_ABI_VERSION_MINOR 21` in the
/// generated header. Runtime accessor: [`tst_get_abi_version_minor`].
///
/// History (additive bumps only — major stays at 0 pre-1.0):
/// - `1` (plan #62): receiver-surface initial drop.
/// - `2` (raw-receiver hardening, `d711ecb`): TS-bytes raw-receiver
///   pull-loop hardening + F2 C-ABI shape additions.
/// - `3` (AU cell reassembly, `5527a9e`): `TstMultiCellAuReason` +
///   `multi_cell_au_reason` field on `TstEventNonConformant`.
/// - `4` (AU cell CFI tolerance): `TstNonConformantCode::CfiTolerated`
///   (= 32) + `TstCellFragmentIndication` enum + `tst_demux_config_set_cfi_tolerance`
///   setter. The new variant reuses the existing `cc_expected` + `cc_observed`
///   field carriers to surface `observed_cfi` + `treated_as` without growing
///   the struct.
/// - `5` (demuxer-config parity, 2026-05-25):
///   `TstAv1CarriageMode` enum (mux side already had a mirror;
///   demux side reuses it) + three new C entry points —
///   `tst_demux_config_set_av1_carriage`,
///   `tst_demux_config_set_au_cell_cap_per_pid`, and
///   `tst_demux_config_set_lenient_psi_reassembly`. Bridges
///   Rust-only demux knobs through the C builder.
/// - `6` (tst-rtp C binding exposure, 2026-05-26):
///   Introduces `srt` + `rtp` cargo features in `tst-c` with
///   cbindgen `TST_HAS_SRT` / `TST_HAS_RTP` conditional emission.
///   Existing SRT surface now gated on `feature = "srt"` (default-on
///   through 2026-06-06; opt-in / default-off thereafter, like every
///   other transport).
///   New RTP/RTSP entry points land behind `feature = "rtp"`.
/// - `7` (network-protocol-stack expansion, 2026-05-27): UDP + TCP + HLS +
///   RIST entry points plus cargo feature flags `udp`/`tcp`/`hls`/`rist`
///   (all default-OFF — embedded `libtstrans.so` size stays unchanged for
///   existing consumers). Adds 4 new `TST_HAS_*` defines and ~95 new entry
///   points.
/// - `8` — added the offline `tst_demuxer_*` byte-feeding demuxer surface.
///   Wraps `tst_core::Demuxer` directly (no transport URL); callers feed
///   raw TS bytes and pull typed `TstEvent`s. Unconditional (no feature
///   gate — `tst-core` is a non-optional dep).
/// - `9` — the offline `tst_muxer_*` surface is now unconditional (no
///   feature gate), matching `tst_demuxer_*`. Previously gated on the `srt`
///   cargo feature; now lives in the top-level `muxer` module. Additive —
///   no symbol removed, no signature changed; SRT builds are unaffected,
///   non-SRT / no_std builds gain the offline muxer.
/// - `10` — two appended `TstMultiCellAuReason` values: `OverflowTotal`
///   (= 4, aggregate AU-cell byte cap exceeded) and `TooManyPids`
///   (= 5, too many in-flight AU PIDs). Both previously fell through to
///   `Orphan` (0) via the forward-compat default. Additive — existing
///   discriminants 0..=3 are unchanged, no symbol/signature change; a
///   consumer now observes the distinct value instead of a misleading
///   `Orphan` for these two memory-limit rejections.
/// - `11` — `pmt_pid` field added to `TstEventProgramMap` (immediately after
///   `pcr_pid`; `_pad` shrunk from 4 to 2 bytes to preserve total size).
///   Exposes the PID carrying the PMT (from the PAT) so C callers can
///   reconstruct a muxer config from a ProgramMap event. Additive — no
///   symbol/signature changed; struct total size and pointer-field offsets
///   are unchanged.
/// - `12` — opaque private-data (`StreamSpec::Data`) stream surface:
///   `tst_data_stream_handle_t` typedef plus seven new entry points —
///   `tst_mux_config_add_data_stream`,
///   `tst_mux_config_set_stream_descriptors_for_data`,
///   `tst_mux_config_add_data_descriptor`, the offline muxer pair
///   `tst_muxer_push_data` / `tst_muxer_push_data_to`, and the
///   srt-gated sender pair `tst_mux_sender_send_data` /
///   `tst_mux_sender_send_data_to`. Lets C callers carry opaque
///   private payloads (PES `stream_id 0xBD`, arbitrary `stream_type`)
///   alongside video/KLV, mirroring the Rust `push_data` family.
///   Additive — no symbol removed, no signature or struct layout
///   changed.
/// - `13` — private-data push through the managed-sender and RTSP-mount
///   pipeline shells: the srt-gated pair `tst_managed_mux_sender_send_data`
///   / `tst_managed_mux_sender_send_data_to` (behind `TST_HAS_SRT`) and the
///   rtp-gated pair `tst_rtsp_mount_push_data` / `tst_rtsp_mount_push_data_to`
///   (behind `TST_HAS_RTP`). Completes the data-stream surface parity with the
///   video/klv/audio/subtitle push families on both shells. Additive — no
///   symbol removed, no signature or struct layout changed.
/// - `14` — AV1 carriage work (WP-B): `TstError::InvalidAv1Obu` (-44) B0
///   guard error code; `av1_carriage` provenance byte on `TstEventSample`
///   (repurposed pad byte — 0=`MPEG2_TS_BINDING`, 1=`INTEROP_RAW_OBU`,
///   0xFF=N/A for non-AV1); `tst_muxer_push_video_wire` /
///   `tst_muxer_push_video_wire_to` pass-through push for byte-faithful
///   transmux; `tst_mux_config_set_av1_carriage` mux-side carriage setter.
/// - `15` — REF-PSI-01: `TstNonConformantCode::PmtProgramNumberMismatch`
///   (= 33). PMT body `program_number` mismatch vs PAT assignment. Surfaces
///   on `TstEventNonConformant`; `pid` is the PMT PID; `programs[0]` =
///   `pat_program`, `programs[1]` = `pmt_program` (reuses the two-element
///   `programs_buf` carrier, same layout as `PidReusedAcrossPrograms`).
///   The mislabeled topology is NOT adopted. No struct layout change.
/// - `16` — WP-D demux trust-boundary diagnostics. New
///   `TstNonConformantCode` values surfaced on `TstEventNonConformant`
///   (no struct layout change — all reuse existing carriers):
///   `UnsupportedScrambling` (= 34, REF-TS-01; `pid` = scrambled PID,
///   `table_id` = 2-bit transport_scrambling_control).
///   `AdaptationFieldMalformed` (= 35, REF-TS-02; `table_id` = kind
///   discriminator: 0=ReservedControl, 1=BadLengthForControl, 2=ShortPcr,
///   0xFF=unknown).
///   `ZeroLengthPesNonVideo` (= 36, REF-PES-01; `table_id` = PES stream_id).
///   `PsiSyntax` (= 37, REF-PSI-03; table_id = PSI table_id, obu_type = kind,
///   cc_observed = section_number for SectionNumberNonZero).
/// - `17` — BIND-01 (WP-I): DTS-aware video push through the C ABI.
///   `tst_muxer_push_video_to_with_dts` and
///   `tst_muxer_push_video_wire_to_with_dts` add a `dts_90khz` parameter to
///   the targeted video push, emitting PES with `PTS_DTS_flags = '11'`
///   (ISO/IEC 13818-1 §2.4.3.6) for B-frame-reordered streams. Additive —
///   no symbol removed, no signature or struct layout changed. (AV1 mux
///   carriage and the targeted `*_to` push family already shipped in ABI 14.)
/// - `18` (tst-hls promotion, 2026-07-11): HLS publisher hardening surface
///   (all gated `TST_HAS_HLS`) — `tst_hls_publisher_finish_serving` +
///   the opaque `TstHlsServerHandle` (`tst_hls_server_handle_local_addr` /
///   `_shutdown` / `_free`) that keeps the built-in HTTP server serving a
///   finished VOD/EVENT stream; builder
///   `tst_hls_publisher_builder_max_segment_duration_ms` (wall-clock
///   force-cut cap; `0` leaves the default); and
///   `tst_hls_publisher_get_forced_cuts` (reads `HlsStats::forced_cuts` via
///   a getter — `TstHlsStats` layout unchanged). Additive — no existing
///   symbol or struct changed. The HLS surface moved from `tst-tcp` to the
///   new `tst-hls` crate (link-level only; no C ABI effect).
/// - `19` (ST 0604 MISP timestamps, 2026-07-11): `tst_muxer_push_video_misp_to`,
///   `tst_muxer_push_video_misp_to_with_dts`, `tst_misp_time_extract`,
///   `TST_E_MISP_TIME` (-45), `TST_E_MISP_TIME_MALFORMED` (-46).
///   Additive — no struct growth, no signature changes.
/// - `20` (bindings-parity bundle, items 6-9, 2026-08-20): four addition
///   groups, all additive — no existing symbol, signature, or struct
///   layout changed:
///   - **Background reconnect (item 7):** `TstReconnectMode` enum
///     (`Blocking = 0`, `Background = 1`) + `tst_reconnect_policy_set_mode`
///     setter on the opaque `tst_reconnect_policy_t` builder — both
///     **unconditional**, like the rest of the reconnect-policy family
///     (the builder module carries no feature gate; only the `Managed*`
///     consumers are SRT-only). Also unconditional: the 48-byte
///     `tst_managed_transport_stats_t` struct (5×`uint64_t` + `bool` +
///     explicit 7-byte pad, `_Static_assert`-pinned). `TST_HAS_SRT` gates
///     only the three getters that read it —
///     `tst_managed_sender_get_reconnect_stats`,
///     `tst_managed_mux_sender_get_reconnect_stats`,
///     `tst_managed_raw_sender_get_reconnect_stats` (send-side only,
///     matching Rust's `ManagedTransport::stats_handle()` — the recv-side
///     managed handles get no getter).
///   - **Stream-end reason (item 8, RTP half):** `TstStreamEndReason` enum
///     (`None = 0`, `CleanTeardown = 1`, `SessionExpired = 2`,
///     `KeepaliveFailed = 3`, `TransportFailed = 4`, `ProtocolError = 5`,
///     `Cancelled = 6`) is defined outside the `rtp` module — also
///     **unconditional**, same reasoning as `TstReconnectMode` above (and
///     avoids cbindgen re-emitting a per-variant `#if` guard, which a
///     feature-gated enum definition would). `TST_HAS_RTP` gates only the
///     two getters that read it — `tst_rtp_receiver_end_reason` /
///     `tst_rtp_demux_receiver_end_reason`.
///     **Last-error detail contract:** on every call that reports an
///     *actually-recorded* reason, the getter resets the thread-local
///     last-error channel to `TST_E_SUCCESS` (0) with either the real
///     detail message (`KeepaliveFailed` / `TransportFailed` /
///     `ProtocolError`) or an empty string (every other recorded reason) —
///     this OVERWRITES any pending failure from an earlier call, so a
///     caller reading `tst_get_last_error[_str]` after one of these
///     getters is reading the getter's own outcome, not a stale error. A
///     `None` result (session hasn't ended) touches last-error not at all.
///     See `tst_get_last_error`'s doc for the same note. **Forward-compat
///     wildcard:** a future Rust `StreamEndReason` variant this C binding
///     doesn't yet know about maps to `TstStreamEndReason::None` until the
///     C mapping is updated in a later release — same as "ended through a
///     path this arc doesn't instrument" already means for the Rust side's
///     own `None`.
///   - **Per-stream last-seen gauges (item 9):** `tst_*_get_stream_last_seen_micros`
///     added to all six demux-receiver handle families —
///     `tst_demux_receiver_*` / `tst_managed_demux_receiver_*` (`TST_HAS_SRT`),
///     `tst_rist_demux_receiver_*` (`TST_HAS_RIST`),
///     `tst_rtp_demux_receiver_*` (`TST_HAS_RTP`),
///     `tst_tcp_demux_receiver_*` (`TST_HAS_TCP`),
///     `tst_udp_demux_receiver_*` (`TST_HAS_UDP`). Returns a `uint64_t`
///     Unix epoch microsecond timestamp of the last time the given PID
///     carried an item through the demuxer; `0` if the PID has never been
///     observed (mirrors `StreamStats.last_seen: Option<SystemTime>`,
///     `None` and a pre-epoch clock both collapsing to `0`).
///   - **RTP receive-deadline parity (item 6, C half — no new symbols):**
///     the `?recv_timeout=<ms>` URL key (already Rust-core-only prior to
///     this release) is now honored by `tst_rtp_recv_open` and
///     `tst_rtp_demux_receiver_open` — deadline expiry surfaces as the
///     existing `TST_E_BUFFER_FULL` (-4), retryable, exactly like SRT's
///     recv-deadline expiry already does. Documented on
///     `tst_rtp_receiver_recv_ts` / `tst_rtp_demux_receiver_next_event`.
/// - `21` (Apple-PoC C ABI arc, Task 7): C-callable MISB ST 0601 KLV
///   decode surface — the first PR-2 addition, unconditional (no
///   feature gate; `tst-core` is a non-optional dependency). New
///   opaque handle `tst_st0601_t` (`tst_st0601_decode` /
///   `tst_st0601_free`), the `tst_st0601_field_state` enum
///   (`TST_ST0601_FIELD_STATE_PRESENT` = 0 / `_ABSENT` = 1 /
///   `_SENTINEL` = 2 / `_IMAPB_SPECIAL` = 3 / `_WRONG_TYPE` = 4,
///   preserving the MISB three-way-`None` distinction end-to-end), a
///   23-tag curated `tst_st0601_geometry_t` struct + one-call getter
///   (`tst_st0601_geometry`), and per-tag typed accessors
///   (`tst_st0601_get_f64` / `_get_u64` / `_state`). Two new error
///   codes: `TST_E_WRONG_TYPE` (-47, a getter's native type doesn't
///   match the mapped field) and `TST_E_KLV_DECODE` (-48, hard
///   structural ST 0601 decode failure). See
///   `bindings/c/core/src/klv_st0601.rs` for the tag table and the
///   corner-geometry fallback contract.
///
///   Task 8 (same arc, additive within `21` — no further bump):
///   `tst_annexb_to_length_prefixed` (Annex B → AVCC/HVCC length-prefixed
///   NAL conversion, two-call sizing idiom) and the parameter-set
///   extraction trio `tst_param_sets_extract` / `_count` / `_get` behind
///   a new opaque handle `tst_param_sets_t` (freed via
///   `tst_param_sets_free`). Both wrap
///   `tst_core::codec::nal_framing::{annexb_to_length_prefixed,
///   extract_parameter_sets}`. No new error codes — reuses
///   `TST_E_BUFFER_FULL` / `TST_E_INVALID_CONFIG` / `TST_E_TOO_LARGE` /
///   `TST_E_NOT_FOUND`. Unconditional module, matching Task 7. See
///   `bindings/c/core/src/codec_framing.rs`.
///
///   Task 9 (same arc, additive within `21` — no further bump): managed
///   SRT demux-receiver lifecycle parity, both `TST_HAS_SRT`-gated —
///   `tst_managed_demux_receiver_end_reason` reuses the existing (ABI
///   20) `TstStreamEndReason` enum via a new
///   `tst-pipeline`-side `RecvEndReason` type (the recv analogue of
///   `tst_rtp::StreamEndReason`, mapped onto the shared C enum by
///   `convert_recv_end_reason`) rather than adding a new one, and
///   `tst_managed_demux_receiver_get_reconnect_stats` reuses the
///   existing `tst_managed_transport_stats_t` struct from ABI 20,
///   closing the send/recv asymmetry both left open. Also
///   `tst_demux_config_set_unwrap_timestamps` (unconditional, no
///   feature gate), a new demuxer-config setter. No new C types or
///   error codes. See
///   `bindings/c/core/src/receiver/demux_receiver/managed.rs` and
///   `bindings/c/core/src/demux_config.rs`.
pub const TST_ABI_VERSION_MINOR: crate::c_types::c_int = 21;

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
