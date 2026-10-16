//! `tst_st0601_*` — C-callable MISB ST 0601 KLV decode surface.
//!
//! Wraps `tst_core::klv::st0601::decode` (lenient decode) behind an
//! opaque `CHandle<UasDatalinkLs>`. Before this module, the C ABI only
//! delivered raw KLV bytes (via `tst_demuxer_*` / `tst_muxer_push_klv*`)
//! — Python and JVM already had typed ST 0601 decode, so this closes the
//! gap for C/Apple-PoC consumers that want geolocation/attitude fields
//! without re-implementing the KLV parser.
//!
//! Unconditional module (no `srt`/`rtp`/... feature gate) — `tst-core`
//! is a non-optional dependency of `tst-c`, matching the existing
//! offline `tst_demuxer_*` / `tst_muxer_*` surfaces.
//!
//! ## Three-way `None` preservation
//!
//! A `UasDatalinkLs` field being absent is NOT one uniform "not present"
//! condition — MISB ST 0601 (via the underlying VLC/IMAPB substrate)
//! distinguishes:
//!
//! - the tag was simply never on the wire (`Absent`),
//! - the tag was on the wire carrying the spec's `INT_MIN` sentinel
//!   (`Sentinel` — recorded in `UasDatalinkLs::sentinel_tags`),
//! - the tag was on the wire as an IMAPB special (±inf/NaN/BelowMin/
//!   AboveMax — recorded in `UasDatalinkLs::imapb_specials`).
//!
//! [`TstSt0601FieldState`] preserves this distinction end-to-end; see
//! `compute_state` (private — see the source).
//!
//! ## Corner geometry fallback
//!
//! [`tst_st0601_geometry`]'s 4 corner points prefer the full-range tag
//! family (82-89) and fall back to offsets-from-frame-center (26-33)
//! when only that family is populated, reusing
//! `UasDatalinkLs::corners()` — see that method's doc for the
//! all-or-nothing precondition on each family. When neither family
//! resolves a complete set of 4 points, all 8 corner fields in the
//! curated struct report `Absent` uniformly (even if some individual
//! raw tags in the 82-89 family are actually `Sentinel` /
//! `ImapbSpecial` on their own — query [`tst_st0601_state`] on the
//! specific tag for that finer-grained diagnostic).

use alloc::boxed::Box;
use alloc::format;

use tst_core::UasDatalinkLs;

use crate::error::{
    TstError, record_klv_decode_error, record_not_found, record_wrong_type, set_last_error,
};
use crate::handle::CHandle;

// ---------------------------------------------------------------------------
// Field-state enum (three-way `None` preserved)
// ---------------------------------------------------------------------------

/// State of one ST 0601 tag on a decoded record. See the module docs for
/// why "not present" needs more than a bool.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TstSt0601FieldState {
    /// The mapped field carries a typed value — read it via
    /// [`tst_st0601_get_f64`] / [`tst_st0601_get_u64`] /
    /// [`tst_st0601_geometry`].
    Present = 0,
    /// The tag was not present on the wire (and is not a tag this
    /// module maps — see `field_kind`, the private tag-to-type table in
    /// the source).
    Absent = 1,
    /// The tag was present on the wire carrying the MISB spec's
    /// `INT_MIN` absent-value sentinel. See
    /// `UasDatalinkLs::sentinel_tags`.
    Sentinel = 2,
    /// The tag was present on the wire as an ST 1201.5 IMAPB special
    /// (±infinity / NaN / `BelowMin` / `AboveMax`). See
    /// `UasDatalinkLs::imapb_specials`.
    ImapbSpecial = 3,
    /// The caller's accessor requested a native type that does not
    /// match the mapped field's actual Rust type (e.g.
    /// [`tst_st0601_get_f64`] on tag 2, which is `u64`-typed). Only
    /// ever returned by the getters themselves via
    /// `TST_E_WRONG_TYPE`, never by [`tst_st0601_state`] (which is
    /// type-agnostic).
    WrongType = 4,
}

// ---------------------------------------------------------------------------
// Named tag constants (ST 0601.19 tag numbers for the curated contract)
// ---------------------------------------------------------------------------

pub const TST_ST0601_TAG_PRECISION_TIMESTAMP: u32 = 2;
pub const TST_ST0601_TAG_PLATFORM_HEADING: u32 = 5;
pub const TST_ST0601_TAG_PLATFORM_PITCH: u32 = 6;
pub const TST_ST0601_TAG_PLATFORM_ROLL: u32 = 7;
pub const TST_ST0601_TAG_SENSOR_LATITUDE: u32 = 13;
pub const TST_ST0601_TAG_SENSOR_LONGITUDE: u32 = 14;
pub const TST_ST0601_TAG_SENSOR_TRUE_ALTITUDE: u32 = 15;
pub const TST_ST0601_TAG_SENSOR_HORIZONTAL_FOV: u32 = 16;
pub const TST_ST0601_TAG_SENSOR_VERTICAL_FOV: u32 = 17;
pub const TST_ST0601_TAG_SENSOR_REL_AZIMUTH: u32 = 18;
pub const TST_ST0601_TAG_SENSOR_REL_ELEVATION: u32 = 19;
pub const TST_ST0601_TAG_SENSOR_REL_ROLL: u32 = 20;
pub const TST_ST0601_TAG_FRAME_CENTER_LATITUDE: u32 = 23;
pub const TST_ST0601_TAG_FRAME_CENTER_LONGITUDE: u32 = 24;
pub const TST_ST0601_TAG_FRAME_CENTER_ELEVATION: u32 = 25;
pub const TST_ST0601_TAG_CORNER_LAT_P1: u32 = 82;
pub const TST_ST0601_TAG_CORNER_LON_P1: u32 = 83;
pub const TST_ST0601_TAG_CORNER_LAT_P2: u32 = 84;
pub const TST_ST0601_TAG_CORNER_LON_P2: u32 = 85;
pub const TST_ST0601_TAG_CORNER_LAT_P3: u32 = 86;
pub const TST_ST0601_TAG_CORNER_LON_P3: u32 = 87;
pub const TST_ST0601_TAG_CORNER_LAT_P4: u32 = 88;
pub const TST_ST0601_TAG_CORNER_LON_P4: u32 = 89;

// ---------------------------------------------------------------------------
// Tag -> field mapping (single point of truth for get_f64/get_u64/state)
// ---------------------------------------------------------------------------

enum FieldKind {
    F64,
    U64,
}

/// Which native type (if any) the C contract table maps `tag` to. `None`
/// means the tag is not one of the curated contract fields — every
/// accessor treats that identically to a structurally-absent field
/// (state `Absent`), per the module contract.
fn field_kind(tag: u32) -> Option<FieldKind> {
    match tag {
        TST_ST0601_TAG_PRECISION_TIMESTAMP => Some(FieldKind::U64),
        TST_ST0601_TAG_PLATFORM_HEADING
        | TST_ST0601_TAG_PLATFORM_PITCH
        | TST_ST0601_TAG_PLATFORM_ROLL
        | TST_ST0601_TAG_SENSOR_LATITUDE
        | TST_ST0601_TAG_SENSOR_LONGITUDE
        | TST_ST0601_TAG_SENSOR_TRUE_ALTITUDE
        | TST_ST0601_TAG_SENSOR_HORIZONTAL_FOV
        | TST_ST0601_TAG_SENSOR_VERTICAL_FOV
        | TST_ST0601_TAG_SENSOR_REL_AZIMUTH
        | TST_ST0601_TAG_SENSOR_REL_ELEVATION
        | TST_ST0601_TAG_SENSOR_REL_ROLL
        | TST_ST0601_TAG_FRAME_CENTER_LATITUDE
        | TST_ST0601_TAG_FRAME_CENTER_LONGITUDE
        | TST_ST0601_TAG_FRAME_CENTER_ELEVATION
        | TST_ST0601_TAG_CORNER_LAT_P1
        | TST_ST0601_TAG_CORNER_LON_P1
        | TST_ST0601_TAG_CORNER_LAT_P2
        | TST_ST0601_TAG_CORNER_LON_P2
        | TST_ST0601_TAG_CORNER_LAT_P3
        | TST_ST0601_TAG_CORNER_LON_P3
        | TST_ST0601_TAG_CORNER_LAT_P4
        | TST_ST0601_TAG_CORNER_LON_P4 => Some(FieldKind::F64),
        _ => None,
    }
}

/// Read the `f64`-typed field for `tag`. Only meaningful when
/// `field_kind(tag) == Some(FieldKind::F64)`; returns `None` for any
/// other tag (including the `u64` timestamp) without panicking.
fn f64_field(ls: &UasDatalinkLs, tag: u32) -> Option<f64> {
    match tag {
        TST_ST0601_TAG_PLATFORM_HEADING => ls.platform_heading_deg,
        TST_ST0601_TAG_PLATFORM_PITCH => ls.platform_pitch_deg,
        TST_ST0601_TAG_PLATFORM_ROLL => ls.platform_roll_deg,
        TST_ST0601_TAG_SENSOR_LATITUDE => ls.sensor_lat_deg,
        TST_ST0601_TAG_SENSOR_LONGITUDE => ls.sensor_lon_deg,
        TST_ST0601_TAG_SENSOR_TRUE_ALTITUDE => ls.sensor_alt_m,
        TST_ST0601_TAG_SENSOR_HORIZONTAL_FOV => ls.sensor_hfov_deg,
        TST_ST0601_TAG_SENSOR_VERTICAL_FOV => ls.sensor_vfov_deg,
        TST_ST0601_TAG_SENSOR_REL_AZIMUTH => ls.sensor_rel_az_deg,
        TST_ST0601_TAG_SENSOR_REL_ELEVATION => ls.sensor_rel_el_deg,
        TST_ST0601_TAG_SENSOR_REL_ROLL => ls.sensor_rel_roll_deg,
        TST_ST0601_TAG_FRAME_CENTER_LATITUDE => ls.frame_center_lat_deg,
        TST_ST0601_TAG_FRAME_CENTER_LONGITUDE => ls.frame_center_lon_deg,
        TST_ST0601_TAG_FRAME_CENTER_ELEVATION => ls.frame_center_elev_m,
        TST_ST0601_TAG_CORNER_LAT_P1 => ls.corner_lat_p1_deg,
        TST_ST0601_TAG_CORNER_LON_P1 => ls.corner_lon_p1_deg,
        TST_ST0601_TAG_CORNER_LAT_P2 => ls.corner_lat_p2_deg,
        TST_ST0601_TAG_CORNER_LON_P2 => ls.corner_lon_p2_deg,
        TST_ST0601_TAG_CORNER_LAT_P3 => ls.corner_lat_p3_deg,
        TST_ST0601_TAG_CORNER_LON_P3 => ls.corner_lon_p3_deg,
        TST_ST0601_TAG_CORNER_LAT_P4 => ls.corner_lat_p4_deg,
        TST_ST0601_TAG_CORNER_LON_P4 => ls.corner_lon_p4_deg,
        _ => None,
    }
}

/// Read the `u64`-typed field for `tag`. Only tag 2 (precision
/// timestamp) is `u64`-typed in the contract table.
fn u64_field(ls: &UasDatalinkLs, tag: u32) -> Option<u64> {
    match tag {
        TST_ST0601_TAG_PRECISION_TIMESTAMP => ls.timestamp_us,
        _ => None,
    }
}

/// Compute the three-way state for `tag` on `ls`. Type-agnostic — does
/// not distinguish `f64` vs `u64` tags, since a bare tag number carries
/// no getter-type context (see [`TstSt0601FieldState::WrongType`]'s
/// doc).
fn compute_state(ls: &UasDatalinkLs, tag: u32) -> TstSt0601FieldState {
    // A tag outside the C contract table is unconditionally `Absent` —
    // `sentinel_tags`/`imapb_specials` are only meaningful for tags this
    // module maps (see `TstSt0601FieldState::Absent`'s doc and the module
    // doc's "Three-way None preservation" section). Gating here, before
    // either side-channel is consulted, is what keeps an unmapped tag
    // Absent even when `ls` happens to carry it in one of those Vecs.
    let kind = match field_kind(tag) {
        Some(k) => k,
        None => return TstSt0601FieldState::Absent,
    };
    let present = match kind {
        FieldKind::U64 => u64_field(ls, tag).is_some(),
        FieldKind::F64 => f64_field(ls, tag).is_some(),
    };
    if present {
        return TstSt0601FieldState::Present;
    }
    if ls.sentinel_tags.contains(&tag) {
        return TstSt0601FieldState::Sentinel;
    }
    if ls.imapb_specials.iter().any(|(t, _)| *t == tag) {
        return TstSt0601FieldState::ImapbSpecial;
    }
    TstSt0601FieldState::Absent
}

// ---------------------------------------------------------------------------
// Curated geometry struct
// ---------------------------------------------------------------------------

/// Curated summary of the ST 0601 geometry/attitude fields the
/// Apple-PoC consumer contract needs — one call instead of 23
/// individual [`tst_st0601_get_f64`] / [`tst_st0601_get_u64`] round
/// trips. Every value field is paired with a `uint8_t ..._state`
/// carrying a [`TstSt0601FieldState`] discriminant (0-4); read the
/// value only when its state is `Present` (0) — other states leave the
/// value at `0`/`0.0`, not an undefined bit pattern.
///
/// Deviation from the original sketch: each corner point gets its own
/// named `..._state` field (`corner_lat_p1_state`, ...) rather than a
/// packed `uint8_t corner_state[8]` array, matching the (value, state)
/// pairing used by every other field in this struct — this keeps every
/// corner state addressable by name instead of a positional index.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TstSt0601Geometry {
    pub timestamp_us: u64,
    pub timestamp_state: u8,
    pub platform_heading_deg: f64,
    pub platform_heading_state: u8,
    pub platform_pitch_deg: f64,
    pub platform_pitch_state: u8,
    pub platform_roll_deg: f64,
    pub platform_roll_state: u8,
    pub sensor_lat_deg: f64,
    pub sensor_lat_state: u8,
    pub sensor_lon_deg: f64,
    pub sensor_lon_state: u8,
    pub sensor_alt_m: f64,
    pub sensor_alt_state: u8,
    pub sensor_hfov_deg: f64,
    pub sensor_hfov_state: u8,
    pub sensor_vfov_deg: f64,
    pub sensor_vfov_state: u8,
    pub sensor_rel_az_deg: f64,
    pub sensor_rel_az_state: u8,
    pub sensor_rel_el_deg: f64,
    pub sensor_rel_el_state: u8,
    pub sensor_rel_roll_deg: f64,
    pub sensor_rel_roll_state: u8,
    pub frame_center_lat_deg: f64,
    pub frame_center_lat_state: u8,
    pub frame_center_lon_deg: f64,
    pub frame_center_lon_state: u8,
    pub frame_center_elev_m: f64,
    pub frame_center_elev_state: u8,
    pub corner_lat_p1_deg: f64,
    pub corner_lat_p1_state: u8,
    pub corner_lon_p1_deg: f64,
    pub corner_lon_p1_state: u8,
    pub corner_lat_p2_deg: f64,
    pub corner_lat_p2_state: u8,
    pub corner_lon_p2_deg: f64,
    pub corner_lon_p2_state: u8,
    pub corner_lat_p3_deg: f64,
    pub corner_lat_p3_state: u8,
    pub corner_lon_p3_deg: f64,
    pub corner_lon_p3_state: u8,
    pub corner_lat_p4_deg: f64,
    pub corner_lat_p4_state: u8,
    pub corner_lon_p4_deg: f64,
    pub corner_lon_p4_state: u8,
}

/// Fill one `(value, state)` pair for an `f64`-typed contract tag.
fn geometry_f64(ls: &UasDatalinkLs, tag: u32) -> (f64, u8) {
    let state = compute_state(ls, tag);
    let value = if state == TstSt0601FieldState::Present {
        f64_field(ls, tag).unwrap_or(0.0)
    } else {
        0.0
    };
    (value, state as u8)
}

fn build_geometry(ls: &UasDatalinkLs) -> TstSt0601Geometry {
    let timestamp_state = compute_state(ls, TST_ST0601_TAG_PRECISION_TIMESTAMP);
    let timestamp_us = if timestamp_state == TstSt0601FieldState::Present {
        ls.timestamp_us.unwrap_or(0)
    } else {
        0
    };

    let (platform_heading_deg, platform_heading_state) =
        geometry_f64(ls, TST_ST0601_TAG_PLATFORM_HEADING);
    let (platform_pitch_deg, platform_pitch_state) =
        geometry_f64(ls, TST_ST0601_TAG_PLATFORM_PITCH);
    let (platform_roll_deg, platform_roll_state) = geometry_f64(ls, TST_ST0601_TAG_PLATFORM_ROLL);
    let (sensor_lat_deg, sensor_lat_state) = geometry_f64(ls, TST_ST0601_TAG_SENSOR_LATITUDE);
    let (sensor_lon_deg, sensor_lon_state) = geometry_f64(ls, TST_ST0601_TAG_SENSOR_LONGITUDE);
    let (sensor_alt_m, sensor_alt_state) = geometry_f64(ls, TST_ST0601_TAG_SENSOR_TRUE_ALTITUDE);
    let (sensor_hfov_deg, sensor_hfov_state) =
        geometry_f64(ls, TST_ST0601_TAG_SENSOR_HORIZONTAL_FOV);
    let (sensor_vfov_deg, sensor_vfov_state) = geometry_f64(ls, TST_ST0601_TAG_SENSOR_VERTICAL_FOV);
    let (sensor_rel_az_deg, sensor_rel_az_state) =
        geometry_f64(ls, TST_ST0601_TAG_SENSOR_REL_AZIMUTH);
    let (sensor_rel_el_deg, sensor_rel_el_state) =
        geometry_f64(ls, TST_ST0601_TAG_SENSOR_REL_ELEVATION);
    let (sensor_rel_roll_deg, sensor_rel_roll_state) =
        geometry_f64(ls, TST_ST0601_TAG_SENSOR_REL_ROLL);
    let (frame_center_lat_deg, frame_center_lat_state) =
        geometry_f64(ls, TST_ST0601_TAG_FRAME_CENTER_LATITUDE);
    let (frame_center_lon_deg, frame_center_lon_state) =
        geometry_f64(ls, TST_ST0601_TAG_FRAME_CENTER_LONGITUDE);
    let (frame_center_elev_m, frame_center_elev_state) =
        geometry_f64(ls, TST_ST0601_TAG_FRAME_CENTER_ELEVATION);

    // Corners: prefer the full tag family (82-89); `UasDatalinkLs::corners()`
    // already implements the fall-back-to-offsets-plus-frame-center logic
    // (all-or-nothing per family). When neither family resolves a complete
    // set of 4 points, all 8 corner fields report Absent uniformly — see
    // the module doc for why a partial-family case isn't split out further.
    let (corners, corner_state) = match ls.corners() {
        Some(c) => (
            [
                c.p1.0, c.p1.1, c.p2.0, c.p2.1, c.p3.0, c.p3.1, c.p4.0, c.p4.1,
            ],
            TstSt0601FieldState::Present,
        ),
        None => ([0.0; 8], TstSt0601FieldState::Absent),
    };
    let corner_state = corner_state as u8;

    TstSt0601Geometry {
        timestamp_us,
        timestamp_state: timestamp_state as u8,
        platform_heading_deg,
        platform_heading_state,
        platform_pitch_deg,
        platform_pitch_state,
        platform_roll_deg,
        platform_roll_state,
        sensor_lat_deg,
        sensor_lat_state,
        sensor_lon_deg,
        sensor_lon_state,
        sensor_alt_m,
        sensor_alt_state,
        sensor_hfov_deg,
        sensor_hfov_state,
        sensor_vfov_deg,
        sensor_vfov_state,
        sensor_rel_az_deg,
        sensor_rel_az_state,
        sensor_rel_el_deg,
        sensor_rel_el_state,
        sensor_rel_roll_deg,
        sensor_rel_roll_state,
        frame_center_lat_deg,
        frame_center_lat_state,
        frame_center_lon_deg,
        frame_center_lon_state,
        frame_center_elev_m,
        frame_center_elev_state,
        corner_lat_p1_deg: corners[0],
        corner_lat_p1_state: corner_state,
        corner_lon_p1_deg: corners[1],
        corner_lon_p1_state: corner_state,
        corner_lat_p2_deg: corners[2],
        corner_lat_p2_state: corner_state,
        corner_lon_p2_deg: corners[3],
        corner_lon_p2_state: corner_state,
        corner_lat_p3_deg: corners[4],
        corner_lat_p3_state: corner_state,
        corner_lon_p3_deg: corners[5],
        corner_lon_p3_state: corner_state,
        corner_lat_p4_deg: corners[6],
        corner_lat_p4_state: corner_state,
        corner_lon_p4_deg: corners[7],
        corner_lon_p4_state: corner_state,
    }
}

// ---------------------------------------------------------------------------
// Opaque handle + entry points
// ---------------------------------------------------------------------------

/// Opaque handle wrapping a decoded [`UasDatalinkLs`]. Obtained from
/// [`tst_st0601_decode`]; freed via [`tst_st0601_free`].
pub struct TstSt0601 {
    inner: CHandle<UasDatalinkLs>,
}

/// Decode `bytes[0..len)` as a MISB ST 0601 UAS Datalink Local Set
/// (lenient decode — see `tst_core::klv::st0601::decode`: unknown tags
/// are preserved, per-tag value-validation failures are collected
/// rather than failing the whole record). `bytes` must be the raw KLV
/// Local Set bytes (as pulled from a `tst_demuxer_*` metadata event or
/// equivalent) — not TS-framed, not PES-wrapped.
///
/// Returns NULL on a hard structural decode failure (truncated buffer,
/// malformed BER length/tag, checksum mismatch, unexpected universal
/// label) with `TST_E_KLV_DECODE` (-48) recorded on the last-error
/// channel; the message carries the specific failure.
///
/// # Safety
///
/// `bytes` must be valid for reads of `len` bytes (or NULL with
/// `len == 0`).
///
/// # C ABI
///
/// `tst_st0601_decode` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_st0601_decode(bytes: *const u8, len: usize) -> *mut TstSt0601 {
    crate::panic::ffi_catch(core::ptr::null_mut(), || {
        let slice = match unsafe { crate::ffi_slice::ffi_slice(bytes, len, "bytes") } {
            Ok(s) => s,
            Err(_) => return core::ptr::null_mut(),
        };
        match tst_core::klv::st0601::decode(slice) {
            Ok(ls) => Box::into_raw(Box::new(TstSt0601 {
                inner: CHandle::offline(ls),
            })),
            Err(e) => {
                record_klv_decode_error(&e);
                core::ptr::null_mut()
            }
        }
    })
}

/// Fill `*out` with the curated geometry/attitude summary — see
/// [`TstSt0601Geometry`]. Returns 0 on success, or a negative `TST_E_*`
/// code if `p` or `out` is null, or `TST_E_PANIC_CAUGHT` if the read
/// panicked (see `CHandle::with_inner_ref` in `crate::handle` — a caught
/// panic in a read-only accessor keeps the decoded state, so later calls
/// on the same pointer still work). This type has no
/// separate `_close` — only
/// [`tst_st0601_free`] — so freeing invalidates the pointer itself;
/// calling this (or any `tst_st0601_*` function) with an already-freed
/// pointer is undefined behavior, not a clean `TST_E_CLOSED` return.
///
/// **Corners:** unlike [`tst_st0601_get_f64`]/[`tst_st0601_state`] on
/// tags 82-89 (which read the raw full-range tag only), the 4 corner
/// points here prefer the full tag family and fall back to
/// offsets-from-frame-center when only that family is populated — see
/// the module doc and [`tst_st0601_get_f64`]'s doc for the exact split.
///
/// # Safety
///
/// `p` must be a valid non-freed `*const TstSt0601` from
/// [`tst_st0601_decode`]. `out` must point to a writable
/// [`TstSt0601Geometry`].
///
/// # C ABI
///
/// `tst_st0601_geometry` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_st0601_geometry(
    p: *const TstSt0601,
    out: *mut TstSt0601Geometry,
) -> crate::c_types::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null st0601 pointer");
            return TstError::InvalidConfig as i32;
        };
        if out.is_null() {
            set_last_error(TstError::InvalidConfig, "null out pointer");
            return TstError::InvalidConfig as i32;
        }
        handle.inner.with_inner_ref(|ls| {
            let geometry = build_geometry(ls);
            // SAFETY: out non-null per guard above.
            unsafe { *out = geometry };
            0
        })
    })
}

/// Read the `f64`-typed field mapped to `tag` (see the
/// `TST_ST0601_TAG_*` constants). Writes `*out` and returns 0 only when
/// the field's state is `Present`; query [`tst_st0601_state`]
/// separately to distinguish `Absent` / `Sentinel` / `ImapbSpecial` —
/// this getter's return code just says "value available or not".
///
/// Returns `TST_E_WRONG_TYPE` (-47) without writing `*out` if `tag`
/// maps to a `u64`-typed field (only tag 2, the precision timestamp —
/// use [`tst_st0601_get_u64`]). Returns `TST_E_NOT_FOUND` (-14) without
/// writing `*out` if `tag` is not in the contract table, or is in the
/// table but absent/sentinel/IMAPB-special on this record.
///
/// **Corner tags (82-89) do NOT apply the offsets-from-frame-center
/// fallback [`tst_st0601_geometry`] applies.** This getter (and
/// [`tst_st0601_state`]) reads the raw full-range tag directly — on a
/// record carrying only the offset family (26-33), a corner tag here
/// reports `Absent` even though `tst_st0601_geometry` would compute and
/// report that same corner as `Present`. This getter family is a raw
/// per-tag reader; `tst_st0601_geometry` is the curated/derived view —
/// use whichever contract you need.
///
/// # Safety
///
/// `p` must be a valid non-freed `*const TstSt0601`. `out` must point
/// to a writable `double`.
///
/// # C ABI
///
/// `tst_st0601_get_f64` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_st0601_get_f64(
    p: *const TstSt0601,
    tag: u32,
    out: *mut f64,
) -> crate::c_types::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null st0601 pointer");
            return TstError::InvalidConfig as i32;
        };
        if out.is_null() {
            set_last_error(TstError::InvalidConfig, "null out pointer");
            return TstError::InvalidConfig as i32;
        }
        handle.inner.with_inner_ref(|ls| match field_kind(tag) {
            Some(FieldKind::U64) => record_wrong_type(&format!(
                "tag {tag} is u64-typed (call tst_st0601_get_u64 instead)"
            )),
            Some(FieldKind::F64) => match f64_field(ls, tag) {
                Some(v) => {
                    // SAFETY: out non-null per guard above.
                    unsafe { *out = v };
                    0
                }
                None => record_not_found(&format!("tag {tag} not present on this record")),
            },
            None => record_not_found(&format!("tag {tag} is not in the ST 0601 C contract table")),
        })
    })
}

/// Read the `u64`-typed field mapped to `tag` (only
/// `TST_ST0601_TAG_PRECISION_TIMESTAMP` = 2 today). Same contract as
/// [`tst_st0601_get_f64`] with the type check inverted: any `f64`-typed
/// tag returns `TST_E_WRONG_TYPE` (-47).
///
/// # Safety
///
/// `p` must be a valid non-freed `*const TstSt0601`. `out` must point
/// to a writable `uint64_t`.
///
/// # C ABI
///
/// `tst_st0601_get_u64` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_st0601_get_u64(
    p: *const TstSt0601,
    tag: u32,
    out: *mut u64,
) -> crate::c_types::c_int {
    crate::panic::ffi_catch(TstError::Internal as i32, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            set_last_error(TstError::InvalidConfig, "null st0601 pointer");
            return TstError::InvalidConfig as i32;
        };
        if out.is_null() {
            set_last_error(TstError::InvalidConfig, "null out pointer");
            return TstError::InvalidConfig as i32;
        }
        handle.inner.with_inner_ref(|ls| match field_kind(tag) {
            Some(FieldKind::F64) => record_wrong_type(&format!(
                "tag {tag} is f64-typed (call tst_st0601_get_f64 instead)"
            )),
            Some(FieldKind::U64) => match u64_field(ls, tag) {
                Some(v) => {
                    // SAFETY: out non-null per guard above.
                    unsafe { *out = v };
                    0
                }
                None => record_not_found(&format!("tag {tag} not present on this record")),
            },
            None => record_not_found(&format!("tag {tag} is not in the ST 0601 C contract table")),
        })
    })
}

/// Query the three-way state of `tag` on this record — see
/// [`TstSt0601FieldState`]. Type-agnostic (never returns `WrongType`;
/// that's only ever produced by the getters).
///
/// A null `p`, a closed handle, or an unmapped `tag` all report
/// `Absent` — this is a side-channel query with no separate rc, so
/// there is no failure signal beyond the returned state; it never
/// touches the last-error channel.
///
/// # Safety
///
/// `p` must be a valid non-freed `*const TstSt0601`, or NULL.
///
/// # C ABI
///
/// `tst_st0601_state` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_st0601_state(p: *const TstSt0601, tag: u32) -> TstSt0601FieldState {
    crate::panic::ffi_catch(TstSt0601FieldState::Absent, || {
        let Some(handle) = (unsafe { p.as_ref() }) else {
            return TstSt0601FieldState::Absent;
        };
        // `with_inner_ref_silent` (unlike `with_inner_ref`) never calls
        // `set_last_error`/`record_panic_caught` on its closed/panic
        // paths — required for this function's "never touches the
        // last-error channel" contract above to hold even if the reader
        // panics.
        handle
            .inner
            .with_inner_ref_silent(TstSt0601FieldState::Absent, |ls| compute_state(ls, tag))
    })
}

/// Free a handle obtained from [`tst_st0601_decode`]. Idempotent-safe
/// with NULL; freeing twice is undefined behavior (matches every other
/// `tst_*_free`/`_close` in this crate).
///
/// # Safety
///
/// `p` must be a valid `*mut TstSt0601` from [`tst_st0601_decode`], or
/// NULL. Must not be called more than once on the same pointer.
///
/// # C ABI
///
/// `tst_st0601_free` — see `bindings/c/include/tstrans.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tst_st0601_free(p: *mut TstSt0601) {
    crate::panic::ffi_catch((), || {
        if p.is_null() {
            return;
        }
        let boxed = unsafe { Box::from_raw(p) };
        boxed.inner.release();
        drop(boxed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{clear_last_error_for_test, tst_get_last_error};
    use crate::test_support::null_entry_tests;

    null_entry_tests! {
        free_is_null_safe => no_crash { tst_st0601_free(core::ptr::null_mut()) };
    }

    fn fixture_bytes(name: &str) -> alloc::vec::Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../crates/tst-core/tests/fixtures/st0601")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e))
    }

    #[test]
    fn decode_full_fixture_geometry_matches_known_values() {
        clear_last_error_for_test();
        let bytes = fixture_bytes("synthetic_full.klv");
        let p = unsafe { tst_st0601_decode(bytes.as_ptr(), bytes.len()) };
        assert!(!p.is_null(), "decode failed: {}", unsafe {
            core::ffi::CStr::from_ptr(crate::error::tst_get_last_error_str())
                .to_str()
                .unwrap()
        });

        let mut geo = core::mem::MaybeUninit::<TstSt0601Geometry>::uninit();
        let rc = unsafe { tst_st0601_geometry(p, geo.as_mut_ptr()) };
        assert_eq!(rc, 0);
        let geo = unsafe { geo.assume_init() };

        assert_eq!(geo.timestamp_state, TstSt0601FieldState::Present as u8);
        assert_eq!(geo.timestamp_us, 1_700_123_456_789_000);

        assert_eq!(geo.sensor_lat_state, TstSt0601FieldState::Present as u8);
        assert!((geo.sensor_lat_deg - 38.123456).abs() < 1e-3);

        assert_eq!(
            geo.platform_heading_state,
            TstSt0601FieldState::Present as u8
        );
        assert!((geo.platform_heading_deg - 123.45).abs() < 0.01);

        // synthetic_full.klv carries the full corner family (82-89).
        assert_eq!(geo.corner_lat_p1_state, TstSt0601FieldState::Present as u8);
        assert!((geo.corner_lat_p1_deg - 38.001).abs() < 1e-3);

        unsafe { tst_st0601_free(p) };
    }

    #[test]
    fn get_f64_matches_geometry_value() {
        clear_last_error_for_test();
        let bytes = fixture_bytes("synthetic_full.klv");
        let p = unsafe { tst_st0601_decode(bytes.as_ptr(), bytes.len()) };
        assert!(!p.is_null());

        let mut heading = 0.0f64;
        let rc = unsafe { tst_st0601_get_f64(p, TST_ST0601_TAG_PLATFORM_HEADING, &mut heading) };
        assert_eq!(rc, 0);
        assert!((heading - 123.45).abs() < 0.01);

        unsafe { tst_st0601_free(p) };
    }

    #[test]
    fn get_u64_reads_timestamp() {
        clear_last_error_for_test();
        let bytes = fixture_bytes("synthetic_full.klv");
        let p = unsafe { tst_st0601_decode(bytes.as_ptr(), bytes.len()) };
        assert!(!p.is_null());

        let mut ts = 0u64;
        let rc = unsafe { tst_st0601_get_u64(p, TST_ST0601_TAG_PRECISION_TIMESTAMP, &mut ts) };
        assert_eq!(rc, 0);
        assert_eq!(ts, 1_700_123_456_789_000);

        unsafe { tst_st0601_free(p) };
    }

    #[test]
    fn get_f64_on_u64_tag_returns_wrong_type() {
        clear_last_error_for_test();
        let bytes = fixture_bytes("synthetic_full.klv");
        let p = unsafe { tst_st0601_decode(bytes.as_ptr(), bytes.len()) };
        assert!(!p.is_null());

        let mut out = 0.0f64;
        let rc = unsafe { tst_st0601_get_f64(p, TST_ST0601_TAG_PRECISION_TIMESTAMP, &mut out) };
        assert_eq!(rc, TstError::WrongType as i32);
        assert_eq!(unsafe { tst_get_last_error() }, TstError::WrongType as i32);

        unsafe { tst_st0601_free(p) };
    }

    #[test]
    fn get_u64_on_f64_tag_returns_wrong_type() {
        clear_last_error_for_test();
        let bytes = fixture_bytes("synthetic_full.klv");
        let p = unsafe { tst_st0601_decode(bytes.as_ptr(), bytes.len()) };
        assert!(!p.is_null());

        let mut out = 0u64;
        let rc = unsafe { tst_st0601_get_u64(p, TST_ST0601_TAG_PLATFORM_HEADING, &mut out) };
        assert_eq!(rc, TstError::WrongType as i32);

        unsafe { tst_st0601_free(p) };
    }

    #[test]
    fn state_absent_for_unmapped_tag() {
        clear_last_error_for_test();
        let bytes = fixture_bytes("synthetic_full.klv");
        let p = unsafe { tst_st0601_decode(bytes.as_ptr(), bytes.len()) };
        assert!(!p.is_null());

        // Tag 1 (checksum) is not in the C contract table.
        let state = unsafe { tst_st0601_state(p, 1) };
        assert_eq!(state, TstSt0601FieldState::Absent);

        unsafe { tst_st0601_free(p) };
    }

    /// Regression for the compute_state gate order: a tag NOT in the C
    /// contract table (`field_kind` returns `None`) must report `Absent`
    /// even when the underlying `UasDatalinkLs` happens to carry it in
    /// `sentinel_tags` / `imapb_specials` — those side-channels are only
    /// meaningful for tags this module maps. Hand-built rather than via a
    /// real decode: no fixture naturally carries an unmapped tag in
    /// either Vec (a real ST 0601 decode only ever populates them for
    /// tags the model type knows about), so this constructs the
    /// otherwise-impossible-via-decode case directly to exercise the
    /// gate.
    #[test]
    fn state_absent_for_unmapped_tag_even_when_in_sentinel_or_imapb() {
        let mut ls = tst_core::UasDatalinkLs::default();
        // Tag 1 (checksum) is not in the C contract table (see
        // `field_kind`); tag 200 is not a real ST 0601 tag at all,
        // covering both "unmapped but real" and "unmapped and bogus".
        ls.sentinel_tags.push(1);
        ls.imapb_specials
            .push((200, tst_core::klv::imapb::ImapbSpecial::PositiveInfinity));

        assert_eq!(compute_state(&ls, 1), TstSt0601FieldState::Absent);
        assert_eq!(compute_state(&ls, 200), TstSt0601FieldState::Absent);
    }

    #[test]
    fn state_present_for_populated_tag() {
        clear_last_error_for_test();
        let bytes = fixture_bytes("synthetic_full.klv");
        let p = unsafe { tst_st0601_decode(bytes.as_ptr(), bytes.len()) };
        assert!(!p.is_null());

        let state = unsafe { tst_st0601_state(p, TST_ST0601_TAG_SENSOR_LATITUDE) };
        assert_eq!(state, TstSt0601FieldState::Present);

        unsafe { tst_st0601_free(p) };
    }

    #[test]
    fn decode_garbage_returns_null_and_records_klv_decode_error() {
        clear_last_error_for_test();
        let garbage = [0xFFu8; 4];
        let p = unsafe { tst_st0601_decode(garbage.as_ptr(), garbage.len()) };
        assert!(p.is_null());
        assert_eq!(unsafe { tst_get_last_error() }, TstError::KlvDecode as i32);
    }

    #[test]
    fn decode_null_bytes_with_zero_len_is_a_hard_error_not_a_panic() {
        // Empty input is not a valid ST 0601 Local Set (no tag-1
        // checksum, no universal label) — decode must fail cleanly.
        clear_last_error_for_test();
        let p = unsafe { tst_st0601_decode(core::ptr::null(), 0) };
        assert!(p.is_null());
    }

    #[test]
    fn geometry_null_out_pointer_returns_invalid_config() {
        clear_last_error_for_test();
        let bytes = fixture_bytes("synthetic_full.klv");
        let p = unsafe { tst_st0601_decode(bytes.as_ptr(), bytes.len()) };
        assert!(!p.is_null());

        let rc = unsafe { tst_st0601_geometry(p, core::ptr::null_mut()) };
        assert_eq!(rc, TstError::InvalidConfig as i32);

        unsafe { tst_st0601_free(p) };
    }
}
