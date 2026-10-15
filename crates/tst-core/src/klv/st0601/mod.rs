//! ST 0601 UAS Datalink Local Set typed layer.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! `UasDatalinkLs` is a flat plain struct that mirrors the wire format
//! directly. Composite types (`GeoPoint`, `Attitude`, `FieldOfView`,
//! `Corners`) are derived read-only views layered on top.
//!
//! Three decode entry points:
//! - [`decode`] — verifies checksum, accepts any UL.
//! - [`decode_unchecked`] — skips checksum verification (useful for known-
//!   broken-checksum captures), accepts any UL.
//! - [`decode_strict`] — verifies checksum + requires the ST 0601 family UL
//!   pattern.
//! - [`decode_strict_compliance`] — adds full ST 0107.5 conformance checks
//!   on top of `decode_strict`.
//!
//! - [`patch`] — byte-faithful tag-level patching: re-encode only the
//!   edited tags, copy every other TLV verbatim, recompute the checksum.
//!
//! ## Spec coverage
//!
//! **Standard:** MISB ST 0601.x UAS Datalink Local Set (versions
//! 13 + 17 + 19 covered; declared version preserved for caller
//! introspection).
//!
//! **All 142 active ST 0601.19 items are typed or structured** (Item 66
//! deprecated: unknown-passthrough). Grouped by [`UasDatalinkLs`] field
//! section:
//!
//! - **Identity / time:** tags 1–4, 10–12, 59, 65 (checksum, UTF-8
//!   identity strings, version, timestamp).
//! - **Platform state:** tags 5–9, 50, 90–91 (heading/pitch/roll/
//!   airspeed, angle of attack, full-range pitch/roll twins of 6/7).
//! - **Sensor pose & position:** tags 13–20, 75 (lat/lon/altitude,
//!   ellipsoid height, FOV, relative azimuth/elevation/roll).
//! - **Ranging & frame center:** tags 21–25, 78.
//! - **Image corners:** tags 26–33 (offsets from frame center), 82–89
//!   (full lat/lon).
//! - **Target location & tracking:** tags 40–46 (location, track-gate
//!   width/height, CE90/LE90 error estimates).
//! - **Weather / atmospheric:** tags 35–38, 49, 53–55.
//! - **Extended platform state:** tags 51–52, 56–58, 64, 92–93
//!   (vertical speed, sideslip, ground speed/range, fuel remaining,
//!   magnetic heading, full-range AoA/sideslip twins of 50/52).
//! - **Alternate platform:** tags 67–69, 71, 76.
//! - **Sensor velocity:** tags 79–80.
//! - **Coded enums:** tag 34 ([`IcingDetected`]), tag 63
//!   ([`SensorFovName`]), tag 77 ([`OperationalMode`]) — each keeps an
//!   `Other(code)` fallback that round-trips unrecognized wire
//!   codepoints byte-exact.
//! - **Raw scalar & string items:** tags 39, 60–62, 70, 72, 106–108,
//!   129, 135 (I8/U16/U64 raw values + 6 UTF-8 strings; 129 and 135
//!   are the first typed tags whose own tag number is 2-byte BER-OID
//!   encoded).
//! - **Extended-range items (ST 1201.5 IMAPB):** tags 96,
//!   103–105, 109, 112–114, 117–120, 132, 134 (14 fields: four are
//!   wider-range twins of existing restricted items 22, 38, 75, 76;
//!   the remaining ten are new standalone items — see the field docs
//!   for the ST 0601.19 restricted-vs-extended precedence rule).
//!   Out-of-range values under [`OutOfRangePolicy::Indicator`] and
//!   any producer-supplied special value (`+/-Infinity`, NaN families)
//!   round-trip through [`UasDatalinkLs::imapb_specials`], the IMAPB
//!   counterpart of [`UasDatalinkLs::sentinel_tags`].
//! - **Var-length int/enum items:** tags 110–111, 123–126, 131,
//!   133, 136–137, 139 (11 fields: navigation/propulsion/positioning
//!   counters, two new coded enums [`PlatformStatus`] (tag 125) and
//!   [`SensorControlMode`] (tag 126), take-off time, on-board MI
//!   storage capacity, leap-seconds, GPS/UTC correction offset, and
//!   the `active_payloads` bitmask).
//! - **Misc:** tag 47 (generic flag bitfield).
//! - **Pack & list items:** tag 102
//!   ([`SdccFlpField`], MULTI-INSTANCE, "Refined Source List" positional
//!   capture — wire-format parser at [`crate::klv::st1010`]), tags 81
//!   ([`ImageHorizonPixels`]), 115 ([`ControlCommand`],
//!   MULTI-INSTANCE), 116, 121 (BER-OID id lists), 122
//!   ([`CountryCodes`]), 127 ([`SensorFrameRate`]), 128
//!   ([`WavelengthRecord`] list), 130 ([`AirbaseLocations`], sharing
//!   [`Location`] with 141), 138 ([`PayloadList`]), 140
//!   ([`WeaponsStore`] list), 141 ([`Waypoint`] list), 142
//!   ([`ViewDomain`]), 143 ([`MetadataSubstreamId`]).
//!
//! **Sibling-decoded nested local sets** — the tag's payload bytes are
//! further parsed by a dedicated typed module, not just stored: tag 48
//! (Security LS, via [`crate::klv::st0102`]), tag 73 (RVT LS, via
//! [`crate::klv::st0806`]), tag 74 (VMTI LS, via [`crate::klv::st0903`]),
//! tag 94 (MIIS Core Identifier, via [`crate::klv::st1204`]).
//!
//! **Named nested-set byte fields** — a dedicated, named struct field
//! (not folded into `unknown`), but the interior bytes are pass-through
//! and not yet decoded: tag 95 (`sar_mi_local_set`, ST 1206), tag 97
//! (`range_image_local_set`, ST 1002), tag 98
//! (`geo_registration_local_set`, ST 1601), tag 99
//! (`composite_imaging_local_set`, ST 1602), tags 100–101
//! (`segment_local_set` / `amend_local_set`, ST 1607).
//!
//! **Tags preserved as `unknown` (`OwnedRawField`):** any tag not in
//! the groups above — only 1 of the 143 spec items remains untyped:
//! tag 66 (deprecated placeholder, permanently unknown-passthrough by
//! design). Full payload bytes preserved per ST 0107.5
//! §6 future-proof skip rule. Consumers reading `record.unknown` can
//! apply downstream-specific typed parsers.
//!
//! **Decode modes:**
//! - [`decode`] — verifies running-sum checksum; accepts any UL.
//! - [`decode_unchecked`] — skips checksum verification (useful for
//!   known-broken-checksum captures from older encoders).
//! - [`decode_strict`] — checksum + ST 0601 family UL pattern
//!   requirement.
//! - [`decode_strict_compliance`] — adds full ST 0107.5 conformance
//!   checks (BER canonicality, no duplicate tags, etc.).
//!
//! **Deferred:** interior typing of the six remaining named-but-opaque
//! nested local sets above (tags 95, 97–101). Tag 73 (RVT / ST 0806) is
//! now typed via [`crate::klv::st0806`] and no longer on this list. Each
//! of the six has a tracked entry in `docs/project/deferred-features.md`.
//! Otherwise none — ST 0601 typed model is the most-complete of the
//! typed sets.

pub(crate) mod decode;
pub(crate) mod encode;
pub(crate) mod mapping;
pub(crate) mod mismms;
pub(crate) mod model;
pub(crate) mod packs;
pub(crate) mod patch;
pub(crate) mod tags;

#[cfg(test)]
mod tests;

pub use decode::{decode, decode_strict, decode_strict_compliance, decode_unchecked};
pub use encode::{
    encode, encode_strict_compliance, encode_to_vec, encode_to_vec_with, encode_with, encoded_len,
    encoded_len_with,
};
pub use mapping::{St0601SentinelMeaning, st0601_sentinel_meaning};
pub use mismms::{MismmsViolation, validate_mismms};
pub use model::{
    Attitude, Corners, EncodeConfig, FieldOfView, GeoPoint, IcingDetected, OperationalMode,
    OutOfRangePolicy, PlatformStatus, SdccFlpField, SensorControlMode, SensorFovName,
    UasDatalinkLs,
};
pub use packs::{
    AirbaseLocations, ControlCommand, CountryCodes, ImageHorizonPixels, Location,
    MetadataSubstreamId, PayloadList, PayloadRecord, PayloadType, SensorFrameRate, ViewDomain,
    ViewDomainPair, WavelengthRecord, Waypoint, WeaponsStore,
};
pub use patch::patch;
