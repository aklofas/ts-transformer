//! Round-trip property tests for **typed** KLV Local Sets and Packs in
//! `tst_core::klv::st*`.
//!
//! ## Convention
//!
//! **Every typed set in `klv::*` MUST have a round-trip proptest in this
//! file.** When adding a new typed set (e.g. ST 0806 RVT), add a section
//! here using the TSDuck-style shape:
//!
//! ```text
//! 1. Define a `proptest::Strategy` that generates a typed record with
//!    field values sampled from spec-realistic ranges (one field at a
//!    time is fine — see `st0601_per_tag_roundtrip` below).
//! 2. Encode → decode → assert_eq!(original, decoded).
//! 3. For f64-bearing fields routed through IMAPB, allow a tolerance
//!    computed per `klv_proptest.rs::imapb_roundtrip` (scale_factor
//!    plus f64-precision floor).
//! ```
//!
//! The existing in-module example-based `round_trip_*` tests cover
//! fixed-value records; these proptests add **value-space exploration**
//! and catch range-boundary + f64-precision corners.
//!
//! ## Why this is its own file (not `klv_proptest.rs`)
//!
//! `klv_proptest.rs` covers the **substrate** (BER, BER-OID, IMAPB).
//! This file covers the **typed sets that sit on top**. Splitting by
//! layer keeps each file focused and makes the "where do I add the
//! proptest for my new typed set?" answer mechanical: this file.
//!
//! ## Failure-mode discipline
//!
//! A property failure here means value-space exploration surfaced a
//! real bug in the encoder/decoder. Capture the regression seed
//! (proptest does this automatically in `.proptest-regressions`) and
//! file an issue. Do NOT mask with `prop_assume!` without
//! recording the bug — silent suppression defeats the point of the
//! property test.

use crate::common::imapb_tol;
use proptest::prelude::*;

// ----------------------------------------------------------------------------
// ST 0605 PrecisionTimeStampPack
// ----------------------------------------------------------------------------

use tst_core::klv::st0605::{self, PrecisionTimeStampPack, TimeStatus};

proptest! {
    /// ST 0605 PrecisionTimeStampPack round-trip: build a typed pack
    /// with random TimeStatus byte + random u64 microseconds, encode to
    /// the 26-byte canonical layout (UL + BER length + body), decode,
    /// assert_eq.
    ///
    /// `time_status_byte`: full u8 domain (0..=255). `TimeStatus` is a
    /// transparent wrapper; encode/decode are byte-passthrough. The
    /// reserved-bits validity check (`reserved_bits_valid`) is a
    /// downstream consumer concern, not an encode/decode invariant.
    ///
    /// `timestamp_us`: full u64 domain. POSIX microseconds since 1970
    /// can in principle hit any u64 value; the codec uses big-endian
    /// `to_be_bytes` / `from_be_bytes` so the property is byte-identity.
    #[test]
    fn st0605_precision_time_stamp_pack_roundtrip(
        time_status_byte in any::<u8>(),
        timestamp_us in any::<u64>(),
    ) {
        let original = PrecisionTimeStampPack {
            time_status: TimeStatus(time_status_byte),
            timestamp_us,
        };
        let bytes = st0605::encode(&original);
        // encode returns a fixed-size [u8; 26]; sanity-check the layout
        // boundary (UL + 1-byte BER length=9 + 9-byte body).
        prop_assert_eq!(bytes.len(), 26);
        let decoded = st0605::decode(&bytes).expect("ST 0605 decode of valid pack must succeed");
        prop_assert_eq!(decoded, original);
    }
}

// ----------------------------------------------------------------------------
// ST 0102 SecurityLs
// ----------------------------------------------------------------------------

use tst_core::klv::st0102::{
    self, ClassifyingCountryCodingMethod, ObjectCountryCodingMethod, SecurityClassification,
    SecurityLs,
};

/// One-tag-at-a-time strategy: returns a `SecurityLs` with default
/// (all-None) fields except for ONE tag set to a random value.
///
/// Why one-at-a-time: combinatorial explosion. SecurityLs has 17
/// optional fields; sampling all of them independently produces 2^17
/// distinct presence permutations before considering value spaces. The
/// existing in-module `round_trip_full_record` test (st0102/mod.rs:842)
/// already covers the "all populated" case with fixed values — this
/// proptest's job is value-space exploration per tag.
///
/// Why the enum sub-strategies restrict `Unknown(byte)` to non-named
/// byte ranges: `decode` maps named codepoints back to their named
/// variants, so constructing e.g. `SecurityClassification::Unknown(0x01)`
/// and encoding it would round-trip back as `Unclassified` (the named
/// variant at codepoint 0x01) — breaking `assert_eq`. Each enum's safe
/// Unknown range is the set of bytes `b` where `from_u8(b) == Unknown(b)`,
/// i.e. the bytes outside the spec's named codepoint set. Confirmed
/// against `crates/tst-core/src/klv/st0102/enums.rs` codepoint comments.
fn st0102_one_tag_record_strategy() -> impl Strategy<Value = SecurityLs> {
    // ASCII-only string for fields whose lenient-mode UTF-8 / UTF-16
    // decode would otherwise normalize away non-roundtrippable input.
    // Tag 13 is the explicit UTF-16 case; all other text fields are
    // straight UTF-8 but ASCII keeps the strategy small and noise-free.
    // Length 1..=64 to exercise the BER short-form length boundary (≤127).
    let ascii_string = "[ -~]{1,64}";

    prop_oneof![
        // Tag 1: SecurityClassification enum. Named codepoints per
        // klv/st0102/enums.rs: 0x01..=0x05 (Unclassified..=TopSecret).
        // Safe Unknown bytes: 0x00 plus 0x06..=0xFF.
        prop_oneof![
            Just(SecurityClassification::Unclassified),
            Just(SecurityClassification::Restricted),
            Just(SecurityClassification::Confidential),
            Just(SecurityClassification::Secret),
            Just(SecurityClassification::TopSecret),
            Just(SecurityClassification::Unknown(0x00)),
            (0x06u8..=0xFFu8).prop_map(SecurityClassification::Unknown),
        ]
        .prop_map(|v| SecurityLs {
            security_classification: Some(v),
            ..Default::default()
        }),
        // Tag 2: ClassifyingCountryCodingMethod enum. Named codepoints
        // per klv/st0102/enums.rs: 0x01..=0x10 (contiguous, includes
        // OmittedValue08/09 reserved slots which are still recognized
        // by `from_u8`). Safe Unknown bytes: 0x00 plus 0x11..=0xFF.
        // Strategy samples all 16 named variants plus Unknown(byte) over
        // the safe range — the named-variant encode path (`to_u8` match
        // arm by name) is a different code path from the Unknown encode
        // path (`Unknown(b) => b`), so each named arm needs coverage.
        prop_oneof![
            Just(ClassifyingCountryCodingMethod::Iso3166TwoLetter),
            Just(ClassifyingCountryCodingMethod::Iso3166ThreeLetter),
            Just(ClassifyingCountryCodingMethod::Fips104TwoLetter),
            Just(ClassifyingCountryCodingMethod::Fips104FourLetter),
            Just(ClassifyingCountryCodingMethod::Iso3166Numeric),
            Just(ClassifyingCountryCodingMethod::Stanag1059TwoLetter),
            Just(ClassifyingCountryCodingMethod::Stanag1059ThreeLetter),
            Just(ClassifyingCountryCodingMethod::OmittedValue08),
            Just(ClassifyingCountryCodingMethod::OmittedValue09),
            Just(ClassifyingCountryCodingMethod::Fips104Mixed),
            Just(ClassifyingCountryCodingMethod::Iso3166Mixed),
            Just(ClassifyingCountryCodingMethod::Stanag1059Mixed),
            Just(ClassifyingCountryCodingMethod::GencTwoLetter),
            Just(ClassifyingCountryCodingMethod::GencThreeLetter),
            Just(ClassifyingCountryCodingMethod::GencNumeric),
            Just(ClassifyingCountryCodingMethod::GencMixed),
            Just(ClassifyingCountryCodingMethod::Unknown(0x00)),
            (0x11u8..=0xFFu8).prop_map(ClassifyingCountryCodingMethod::Unknown),
        ]
        .prop_map(|v| SecurityLs {
            classifying_country_coding_method: Some(v),
            ..Default::default()
        }),
        // Tag 3: classifying_country (UTF-8).
        ascii_string.prop_map(|s| SecurityLs {
            classifying_country: Some(s),
            ..Default::default()
        }),
        // Tag 12: ObjectCountryCodingMethod enum. Note: this is a
        // distinct enum from Tag 2's even though some variant names
        // overlap — the numeric codepoints differ (e.g. Iso3166Numeric
        // is 0x03 here but 0x05 in Tag 2's enum). Named codepoints per
        // klv/st0102/enums.rs: 0x01..=0x0F contiguous, then a jump to
        // 0x40 for GencAdminSub. Safe Unknown bytes: 0x00, 0x10..=0x3F,
        // and 0x41..=0xFF (excluding the 0x40 island).
        // Strategy samples all 16 named variants plus Unknown(byte) over
        // the safe ranges — the named-variant encode path (`to_u8` match
        // arm by name) is a different code path from the Unknown encode
        // path (`Unknown(b) => b`), so each named arm needs coverage.
        prop_oneof![
            Just(ObjectCountryCodingMethod::Iso3166TwoLetter),
            Just(ObjectCountryCodingMethod::Iso3166ThreeLetter),
            Just(ObjectCountryCodingMethod::Iso3166Numeric),
            Just(ObjectCountryCodingMethod::Fips104TwoLetter),
            Just(ObjectCountryCodingMethod::Fips104FourLetter),
            Just(ObjectCountryCodingMethod::Stanag1059TwoLetter),
            Just(ObjectCountryCodingMethod::Stanag1059ThreeLetter),
            Just(ObjectCountryCodingMethod::OmittedValue08),
            Just(ObjectCountryCodingMethod::OmittedValue09),
            Just(ObjectCountryCodingMethod::OmittedValue0A),
            Just(ObjectCountryCodingMethod::OmittedValue0B),
            Just(ObjectCountryCodingMethod::OmittedValue0C),
            Just(ObjectCountryCodingMethod::GencTwoLetter),
            Just(ObjectCountryCodingMethod::GencThreeLetter),
            Just(ObjectCountryCodingMethod::GencNumeric),
            Just(ObjectCountryCodingMethod::GencAdminSub),
            Just(ObjectCountryCodingMethod::Unknown(0x00)),
            (0x10u8..=0x3Fu8).prop_map(ObjectCountryCodingMethod::Unknown),
            (0x41u8..=0xFFu8).prop_map(ObjectCountryCodingMethod::Unknown),
        ]
        .prop_map(|v| SecurityLs {
            object_country_coding_method: Some(v),
            ..Default::default()
        }),
        // Tag 13: object_country_codes (UTF-16 on the wire, BE-normalized).
        // Strategy uses ASCII so the value is byte-identical pre- and
        // post-normalization. UTF-16 BOM stripping + LE→BE normalization
        // is exercised by the in-module test `round_trip_utf16_normalizes_to_be`.
        ascii_string.prop_map(|s| SecurityLs {
            object_country_codes: Some(s),
            ..Default::default()
        }),
        // Tag 22: version u16 (2-byte BE).
        any::<u16>().prop_map(|v| SecurityLs {
            version: Some(v),
            ..Default::default()
        }),
        // Tags 4-11, 14, 23, 24: free-text UTF-8 fields.
        ascii_string.prop_map(|s| SecurityLs {
            sci_shi_info: Some(s),
            ..Default::default()
        }),
        ascii_string.prop_map(|s| SecurityLs {
            caveats: Some(s),
            ..Default::default()
        }),
        ascii_string.prop_map(|s| SecurityLs {
            releasing_instructions: Some(s),
            ..Default::default()
        }),
        ascii_string.prop_map(|s| SecurityLs {
            classified_by: Some(s),
            ..Default::default()
        }),
        ascii_string.prop_map(|s| SecurityLs {
            derived_from: Some(s),
            ..Default::default()
        }),
        ascii_string.prop_map(|s| SecurityLs {
            classification_reason: Some(s),
            ..Default::default()
        }),
        ascii_string.prop_map(|s| SecurityLs {
            declassification_date: Some(s),
            ..Default::default()
        }),
        ascii_string.prop_map(|s| SecurityLs {
            classification_marking_system: Some(s),
            ..Default::default()
        }),
        ascii_string.prop_map(|s| SecurityLs {
            classification_comments: Some(s),
            ..Default::default()
        }),
        ascii_string.prop_map(|s| SecurityLs {
            classifying_country_coding_method_version_date: Some(s),
            ..Default::default()
        }),
        ascii_string.prop_map(|s| SecurityLs {
            object_country_coding_method_version_date: Some(s),
            ..Default::default()
        }),
    ]
}

proptest! {
    /// ST 0102 SecurityLs per-tag round-trip via lenient `decode`.
    ///
    /// Uses lenient decode because the strategy produces records with
    /// only one optional field set, which `decode_strict` would reject
    /// for missing required tags. Lenient decode is the symmetric
    /// inverse of `encode` for any valid in-memory record.
    #[test]
    fn st0102_per_tag_roundtrip(record in st0102_one_tag_record_strategy()) {
        let bytes = st0102::encode_to_vec(&record).expect("ST 0102 encode of valid record must succeed");
        let decoded = st0102::decode(&bytes).expect("ST 0102 decode of self-encoded bytes must succeed");
        prop_assert_eq!(decoded, record);
    }
}

// ----------------------------------------------------------------------------
// ST 0601 UasDatalinkLs
// ----------------------------------------------------------------------------

use tst_core::klv::st0601::{self, UasDatalinkLs};

// Three categories of tag for the per-tag strategy. Each round-trips
// through `encode_to_vec` + `decode` but with a different comparison
// shape, so we keep them as distinct proptests for readable failures.

proptest! {
    /// Exact-equality tags (strings, opaque bytes, integer types).
    ///
    /// Strategy: pick one tag uniformly at random, populate just that
    /// field in an otherwise-default record, round-trip. All these
    /// tags use byte-passthrough or integer encoding; the comparison
    /// is `assert_eq!`.
    ///
    /// **Tag 65 quirk:** the ST 0601 encoder unconditionally emits Tag 65
    /// ("UAS LS Version Number") — if `record.uas_ls_version` is None it
    /// auto-emits the default `EncodeConfig::version` byte (19, per ST
    /// 0601.19). The decoder reads that back into `uas_ls_version =
    /// Some(19)`, breaking `assert_eq` if the input had None. We pre-set
    /// `uas_ls_version = Some(19)` on the input so both sides match;
    /// the `which == 6` arm overwrites this with the random `v_u8` to
    /// actually exercise the tag's value space.
    #[test]
    fn st0601_exact_equality_per_tag_roundtrip(
        // Discriminator selecting which tag to populate. Range covers
        // the 10 exact-equality tag arms in the match below.
        which in 0u8..10,
        // Generic value space large enough to feed any single field.
        // Bytes: any opaque payload. String: ASCII subset 0x20..=0x7E
        // (printable ASCII; trivially valid UTF-8, so the strict-UTF-8
        // decode at st0601/mod.rs:923 round-trips byte-identical).
        // Range 1..=32 keeps things BER-short-form (length ≤ 127) and
        // proptest case fast.
        bytes in proptest::collection::vec(any::<u8>(), 1..=32),
        s in "[ -~]{1,32}",
        v_u8 in any::<u8>(),
        v_u64 in any::<u64>(),
    ) {
        // Pre-populate the auto-emitted Tag 65 to match what the decoder
        // will produce; see method-level docstring for the rationale.
        let mut record = UasDatalinkLs {
            uas_ls_version: Some(19),
            ..Default::default()
        };
        match which {
            0 => record.mission_id = Some(s),
            1 => record.platform_tail_number = Some(s),
            2 => record.platform_designation = Some(s),
            3 => record.image_source_sensor = Some(s),
            // Tag 12 image_coordinate_system: decode accepts any valid
            // UTF-8 but spec values are short ("WGS84" etc).
            // ASCII strategy is in-spec for the values encoders produce.
            4 => record.image_coordinate_system = Some(s),
            5 => record.platform_call_sign = Some(s),
            // Overwrite the default-version pre-set with a random byte
            // to actually exercise Tag 65's value space.
            6 => record.uas_ls_version = Some(v_u8),
            7 => record.timestamp_us = Some(v_u64),
            8 => record.generic_flag_data = Some(v_u8),
            // Tag 48 security_local_set: opaque pass-through to ST 0102 sibling
            // layer. Round-trip is byte-identical at the ST 0601 layer.
            9 => record.security_local_set = Some(bytes.clone()),
            _ => unreachable!(),
        }
        let buf = st0601::encode_to_vec(&record)
            .expect("ST 0601 encode of valid record must succeed");
        let decoded = st0601::decode(&buf)
            .expect("ST 0601 decode of self-encoded bytes must succeed");
        // Compare just the field we set — `field_errors` and `unknown`
        // are decode-side diagnostics, empty on a clean self-encoded
        // record; the default fields stay None on both sides.
        prop_assert_eq!(decoded, record);
    }

    /// Tag 74 vmti opaque pass-through (separate so failure messages
    /// localize when the ST 0903 sibling layering breaks the parent's
    /// byte-passthrough contract).
    #[test]
    fn st0601_vmti_passthrough_roundtrip(
        bytes in proptest::collection::vec(any::<u8>(), 1..=64),
    ) {
        let record = UasDatalinkLs { vmti: Some(bytes.clone()), ..Default::default() };
        let buf = st0601::encode_to_vec(&record).expect("encode");
        let decoded = st0601::decode(&buf).expect("decode");
        prop_assert_eq!(decoded.vmti.as_deref(), Some(bytes.as_slice()));
    }

    /// Tag 94 miis_core_id opaque pass-through — mirrors the Tag 74 vmti
    /// pattern; the ST 1204 binary payload is carried verbatim.
    #[test]
    fn st0601_miis_core_id_passthrough_roundtrip(
        bytes in proptest::collection::vec(any::<u8>(), 1..=64),
    ) {
        let record = UasDatalinkLs { miis_core_id: Some(bytes.clone()), ..Default::default() };
        let buf = st0601::encode_to_vec(&record).expect("encode");
        let decoded = st0601::decode(&buf).expect("decode");
        prop_assert_eq!(decoded.miis_core_id.as_deref(), Some(bytes.as_slice()));
    }

    /// IMAPB f64 tags: pick one ranged tag uniformly, encode value at
    /// random `t ∈ [0,1]` lerped into the tag's [min, max] range,
    /// decode, assert within IMAPB tolerance.
    ///
    /// Tolerance derivation mirrors `klv_proptest.rs::imapb_roundtrip`:
    /// max of (quantization scale, f64 ULP × span/magnitude × safety
    /// factor). See that file lines 73-94 for the rationale.
    ///
    /// `which` indexes into the ranged-tag table built below. The
    /// table is a hand-curated subset covering both IMAPB encodings
    /// commonly used (signed +/- ranges and unsigned 0..N ranges) and
    /// the three integer widths in play (i8, i16, i32, u8, u16, u32).
    /// Full coverage of every tag is the job of `every_typed_tag_round_trips`
    /// (st0601/mod.rs:1498); this proptest's job is value-space.
    #[test]
    fn st0601_imapb_tag_value_space_roundtrip(
        which in 0usize..8,
        t in 0.0f64..=1.0,
    ) {
        // (tag_id, min, max, field-getter). Eight representative tags
        // covering signed/unsigned and the integer-width spectrum.
        struct ImapbTag {
            id: u8,
            min: f64,
            max: f64,
            set: fn(&mut UasDatalinkLs, f64),
            get: fn(&UasDatalinkLs) -> Option<f64>,
        }
        let tags: [ImapbTag; 8] = [
            // Tag 5: platform_heading_angle 0..360 (u16 → 2 bytes, ST 0601 §item5)
            ImapbTag { id: 5, min: 0.0, max: 360.0,
                set: |r, v| r.platform_heading_deg = Some(v),
                get: |r| r.platform_heading_deg },
            // Tag 6: platform_pitch_angle ±20° (i16 → 2 bytes)
            ImapbTag { id: 6, min: -20.0, max: 20.0,
                set: |r, v| r.platform_pitch_deg = Some(v),
                get: |r| r.platform_pitch_deg },
            // Tag 13: sensor_latitude ±90° (i32 → 4 bytes)
            ImapbTag { id: 13, min: -90.0, max: 90.0,
                set: |r, v| r.sensor_lat_deg = Some(v),
                get: |r| r.sensor_lat_deg },
            // Tag 14: sensor_longitude ±180° (i32 → 4 bytes)
            ImapbTag { id: 14, min: -180.0, max: 180.0,
                set: |r, v| r.sensor_lon_deg = Some(v),
                get: |r| r.sensor_lon_deg },
            // Tag 15: sensor_altitude_m -900..19000 (u16 → 2 bytes, asymmetric range)
            ImapbTag { id: 15, min: -900.0, max: 19000.0,
                set: |r, v| r.sensor_alt_m = Some(v),
                get: |r| r.sensor_alt_m },
            // Tag 21: slant_range 0..5,000,000 m (u32 → 4 bytes, large unsigned range)
            ImapbTag { id: 21, min: 0.0, max: 5_000_000.0,
                set: |r, v| r.slant_range_m = Some(v),
                get: |r| r.slant_range_m },
            // Tag 90: platform_pitch_full ±90° (i32 → 4 bytes)
            ImapbTag { id: 90, min: -90.0, max: 90.0,
                set: |r, v| r.platform_pitch_full_deg = Some(v),
                get: |r| r.platform_pitch_full_deg },
            // Tag 50: platform_angle_of_attack ±20° (i16 → 2 bytes)
            ImapbTag { id: 50, min: -20.0, max: 20.0,
                set: |r, v| r.platform_angle_of_attack_deg = Some(v),
                get: |r| r.platform_angle_of_attack_deg },
        ];
        let tag = &tags[which];
        let value = tag.min + t * (tag.max - tag.min);

        let mut record = UasDatalinkLs::default();
        (tag.set)(&mut record, value);

        let buf = st0601::encode_to_vec(&record).expect("encode");
        let decoded = st0601::decode(&buf).expect("decode");
        let got = (tag.get)(&decoded).expect("field must be present after round-trip");

        // IMAPB tolerance derivation: see the `imapb_tol` helper at the
        // bottom of this file (formula mirrored from
        // klv_proptest.rs::imapb_roundtrip lines 73-94). Conservative
        // `length=2` keeps the bound safe for 4-byte tags too.
        let length = 2usize;
        let tol = imapb_tol(tag.min, tag.max, length).tol;

        prop_assert!(
            (got - value).abs() <= tol,
            "ST 0601 tag {} value {} round-tripped to {} (delta {}, tol {})",
            tag.id, value, got, (got - value).abs(), tol
        );
    }
}

// ----------------------------------------------------------------------------
// ST 0903 VmtiLs + VTargetPack
// ----------------------------------------------------------------------------

use tst_core::klv::st0903::{self, VTargetPack, VmtiLs};

/// Strategy generating a VmtiLs with `checksum: None` and ONE typed
/// field populated. Same "one-at-a-time" pattern as ST 0102 — avoids
/// 2^N permutation explosion across ~15 optional fields.
///
/// `checksum` is held at None to sidestep the encode-shape asymmetry
/// (per VmtiLs.checksum rustdoc): embedded `encode` always drops Tag 1,
/// standalone `encode_standalone` always recomputes it. Each variant
/// has its own proptest below that asserts the correct post-decode
/// shape for `checksum` specifically.
///
/// Numeric ranges follow the spec caps in `klv::st0903::tags::TAGS`:
/// - Tags 5/6/8/9 are V3 (`VarUint { max_bytes: 3 }`), so values are
///   capped at `0..=0xFF_FFFF` (2^24 − 1). The encoder side
///   (`emit_var`) writes 4 bytes for values above that and the decoder
///   then rejects via `InvalidLength` (silently dropping the value
///   into `field_errors`) — out-of-spec values are not codec bugs, so
///   restrict the strategy.
/// - Tag 4 (`version_number`) is V2 (`max_bytes: 2`); full u16 fits.
fn st0903_one_field_vmti_strategy() -> impl Strategy<Value = VmtiLs> {
    let ascii_string = "[ -~]{1,32}";
    // V3 cap: 2^24 − 1. See module comment above.
    const V3_MAX: u32 = 0x00FF_FFFF;

    prop_oneof![
        any::<u64>().prop_map(|v| VmtiLs {
            precision_time_stamp: Some(v),
            ..Default::default()
        }),
        ascii_string.prop_map(|s| VmtiLs {
            vmti_system_name: Some(s),
            ..Default::default()
        }),
        // Tag 4 version_number: u16 per ST 0903.6 §10.1.4 (VarUint 1..=2 wire bytes).
        any::<u16>().prop_map(|v| VmtiLs {
            version_number: Some(v),
            ..Default::default()
        }),
        // Tags 5/6/8/9 target counts and frame dims: V3 = max_bytes 3 = 0..=2^24-1.
        (0u32..=V3_MAX).prop_map(|v| VmtiLs {
            total_targets_in_frame: Some(v),
            ..Default::default()
        }),
        (0u32..=V3_MAX).prop_map(|v| VmtiLs {
            num_targets_reported: Some(v),
            ..Default::default()
        }),
        (0u32..=V3_MAX).prop_map(|v| VmtiLs {
            frame_width: Some(v),
            ..Default::default()
        }),
        (0u32..=V3_MAX).prop_map(|v| VmtiLs {
            frame_height: Some(v),
            ..Default::default()
        }),
        ascii_string.prop_map(|s| VmtiLs {
            source_sensor: Some(s),
            ..Default::default()
        }),
        // Tags 11/12 FOV: IMAPB f64 0..180 deg, 2-byte wire. Use the
        // klv_proptest.rs tolerance derivation locally below.
        (0.0f64..=1.0).prop_map(|t| VmtiLs {
            horizontal_fov: Some(t * 180.0),
            ..Default::default()
        }),
        (0.0f64..=1.0).prop_map(|t| VmtiLs {
            vertical_fov: Some(t * 180.0),
            ..Default::default()
        }),
        proptest::collection::vec(any::<u8>(), 1..=32).prop_map(|b| VmtiLs {
            miis_id: Some(b),
            ..Default::default()
        }),
        proptest::collection::vec(any::<u8>(), 1..=32).prop_map(|b| VmtiLs {
            algorithm_series: Some(b),
            ..Default::default()
        }),
        proptest::collection::vec(any::<u8>(), 1..=32).prop_map(|b| VmtiLs {
            ontology_series: Some(b),
            ..Default::default()
        }),
    ]
}

proptest! {
    /// VmtiLs embedded-shape round-trip via `encode_to_vec` + `decode`.
    ///
    /// Per the VmtiLs.checksum rustdoc contract, `encode` drops Tag 1.
    /// Input `checksum` is None (from the strategy); decoded `checksum`
    /// must also be None (no Tag 1 was emitted, so decode finds none).
    /// The FOV fields are f64 IMAPB — we tolerate IMAPB quantization
    /// error for those by skipping exact f64 comparison via PartialEq
    /// when those fields are set; instead, assert each non-FOV field
    /// exactly and assert FOV fields are within IMAPB tolerance.
    #[test]
    fn st0903_vmti_embedded_roundtrip(record in st0903_one_field_vmti_strategy()) {
        let bytes = st0903::encode_to_vec(&record).expect("encode_to_vec");
        let decoded = st0903::decode(&bytes).expect("decode");
        prop_assert_eq!(decoded.checksum, None, "embedded encode must not emit Tag 1");

        // Compare via a lossy-comparison helper that uses IMAPB tolerance
        // for FOV fields and exact equality elsewhere. Keeping the helper
        // inline (not a shared fn) so the comparison shape is visible at
        // failure-report sites.
        if let Some(hfov) = record.horizontal_fov {
            let got = decoded.horizontal_fov.expect("hfov present");
            // ST 0903.6 §10.1.11: 0..180 deg, 2-byte IMAPB. Compute
            // tolerance per klv_proptest.rs lines 73-94.
            let tol = imapb_tol(0.0, 180.0, 2).tol;
            prop_assert!((got - hfov).abs() <= tol, "hfov delta {} > tol {}", (got - hfov).abs(), tol);
        } else {
            prop_assert_eq!(decoded.horizontal_fov, None);
        }
        if let Some(vfov) = record.vertical_fov {
            let got = decoded.vertical_fov.expect("vfov present");
            let tol = imapb_tol(0.0, 180.0, 2).tol;
            prop_assert!((got - vfov).abs() <= tol, "vfov delta {} > tol {}", (got - vfov).abs(), tol);
        } else {
            prop_assert_eq!(decoded.vertical_fov, None);
        }
        // All other fields: exact equality. Build a "normalized" copy
        // of the input with FOV fields equal to the decoded values
        // (already asserted within tolerance above) so PartialEq on
        // the full struct works for the remaining fields.
        let normalized = VmtiLs {
            horizontal_fov: decoded.horizontal_fov,
            vertical_fov: decoded.vertical_fov,
            ..record
        };
        prop_assert_eq!(decoded, normalized);
    }

    /// VmtiLs standalone-shape round-trip via `encode_to_vec_standalone`
    /// + `decode`. Per VmtiLs.checksum rustdoc, `encode_standalone`
    /// emits a fresh substrate-computed Tag 1 last per ST 0903.4-17 /
    /// ST 0903.6-119. The strategy provides `checksum: None` (input is
    /// ignored by `encode_standalone` anyway); the decoded record must
    /// carry `checksum: Some(_)` populated from the wire bytes.
    ///
    /// `encode_to_vec_standalone` returns the full wire record:
    /// `[UL (16 bytes)] [outer BER length] [body] [Tag 1 TLV]`. The
    /// top-level `decode` consumes only the LS body, so we peel the UL
    /// + outer BER length first — mirrors the in-module test at
    /// `klv/st0903/mod.rs::encode_standalone_round_trips_via_decode`.
    #[test]
    fn st0903_vmti_standalone_roundtrip(record in st0903_one_field_vmti_strategy()) {
        let bytes = st0903::encode_to_vec_standalone(&record).expect("encode_to_vec_standalone");
        // Peel UL + outer BER length to get the LS body that `decode` expects.
        prop_assert_eq!(&bytes[..16], &st0903::VMTI_LS_UL[..]);
        let (outer_len, body) = tst_core::klv::length::read_ber(&bytes[16..]).expect("outer BER");
        prop_assert_eq!(outer_len, body.len(), "outer BER length covers full body");

        let decoded = st0903::decode(body).expect("decode standalone");
        prop_assert!(decoded.checksum.is_some(), "standalone encode must emit Tag 1");

        // FOV tolerance + exact-equality-for-everything-else, same shape
        // as the embedded test but with `checksum` normalized to the
        // decoded value (it's substrate-computed, not caller-supplied).
        if let Some(hfov) = record.horizontal_fov {
            let got = decoded.horizontal_fov.expect("hfov present");
            let tol = imapb_tol(0.0, 180.0, 2).tol;
            prop_assert!((got - hfov).abs() <= tol, "hfov delta {} > tol {}", (got - hfov).abs(), tol);
        }
        if let Some(vfov) = record.vertical_fov {
            let got = decoded.vertical_fov.expect("vfov present");
            let tol = imapb_tol(0.0, 180.0, 2).tol;
            prop_assert!((got - vfov).abs() <= tol, "vfov delta {} > tol {}", (got - vfov).abs(), tol);
        }
        let normalized = VmtiLs {
            checksum: decoded.checksum,
            horizontal_fov: decoded.horizontal_fov,
            vertical_fov: decoded.vertical_fov,
            ..record
        };
        prop_assert_eq!(decoded, normalized);
    }

    /// VTargetPack inner-codec round-trip: exercises
    /// `vtarget_pack::{write_pack, read_pack}` indirectly via the
    /// `VmtiLs.targets` Vec round-trip through the top-level
    /// `encode_to_vec` / `decode`.
    ///
    /// Covers all non-IMAPB VTargetPack fields per ST 0903.6 §10.2.2.
    /// Range constraints follow the spec valid-value ranges (priority
    /// 1..=255 per §10.2.2.5, confidence_level 0..=100 per §10.2.2.6,
    /// percentage_of_target_pixels 1..=100 per §10.2.2.8) and the V3
    /// VarUint cap (0..=0xFFFFFF for target_intensity per §10.2.2.10
    /// and algorithm_id per §10.2.2.23). u32 fields without spec caps
    /// use the full domain (Tags 1/2/3 are V6 max_bytes=6 and Tags 19/
    /// 20 are V4 max_bytes=4 per vtarget_pack.rs:60-197; minimal-encoded
    /// u32 fits in ≤4 bytes so all u32 values are within max_bytes).
    ///
    /// IMAPB f64 fields (centroid_lat_offset, centroid_lon_offset,
    /// centroid_hae, bbox_*_lat/lon_offset) are excluded — covered by
    /// the IMAPB-tolerance proptests in the VmtiLs section; including
    /// them here would force tolerance-based comparison and lose the
    /// exact-equality assertion that catches packing bugs.
    ///
    /// The existing in-module `populated_pack_round_trips` test
    /// (vtarget_pack.rs:854) covers fixed-value full-pack records;
    /// this proptest covers value-space across the exact-encoded fields.
    #[test]
    fn st0903_vtarget_pack_roundtrip(
        // proptest's Strategy is implemented for tuples up to size 12,
        // so the 14 fields are split into two nested tuples:
        //   pixels   = (target_id, centroid_pixel, bbox_tl_pixel,
        //               bbox_br_pixel, centroid_pix_row, centroid_pix_col)
        //   scalars  = (priority, confidence_level, history,
        //               pct_target_pixels, target_color,
        //               target_intensity, algorithm_id, detection_status)
        targets in proptest::collection::vec(
            (
                (
                    // target_id: u64 BER-OID (spec §10.2.2.1: up to 9 bytes ≈ 63-bit)
                    any::<u64>(),
                    // V6 pixel fields: up to 6 wire bytes (max 2^48-1)
                    proptest::option::of(0u64..=(1u64 << 48) - 1),
                    proptest::option::of(0u64..=(1u64 << 48) - 1),
                    proptest::option::of(0u64..=(1u64 << 48) - 1),
                    // V4 row/col fields: up to 4 wire bytes (max u32::MAX)
                    proptest::option::of(0u64..=u32::MAX as u64),
                    proptest::option::of(0u64..=u32::MAX as u64),
                ),
                (
                    proptest::option::of(1u8..=255),
                    proptest::option::of(0u8..=100),
                    proptest::option::of(any::<u16>()),
                    proptest::option::of(1u8..=100),
                    proptest::option::of(any::<[u8; 3]>()),
                    proptest::option::of(0u32..=0x00FF_FFFF),
                    proptest::option::of(0u32..=0x00FF_FFFF),
                    proptest::option::of(any::<u8>()),
                ),
            ),
            1..=4,
        ),
    ) {
        let targets: Vec<VTargetPack> = targets
            .into_iter()
            // pixels = (target_id, centroid_pixel, bbox_top_left, bbox_bottom_right, centroid_pix_row, centroid_pix_col);
            // scalars = (priority, confidence_level, history, percentage_of_target_pixels, target_color, target_intensity, algorithm_id, detection_status)
            .map(|(pixels, scalars)| {
                let (target_id, centroid_pixel, bbox_top_left_pixel,
                     bbox_bottom_right_pixel, centroid_pix_row,
                     centroid_pix_col) = pixels;
                let (priority, confidence_level, history,
                     percentage_of_target_pixels, target_color,
                     target_intensity, algorithm_id, detection_status) =
                    scalars;
                VTargetPack {
                    target_id,
                    centroid_pixel,
                    bbox_top_left_pixel,
                    bbox_bottom_right_pixel,
                    priority,
                    confidence_level,
                    history,
                    percentage_of_target_pixels,
                    target_color,
                    target_intensity,
                    centroid_pix_row,
                    centroid_pix_col,
                    algorithm_id,
                    detection_status,
                    ..Default::default()
                }
            })
            .collect();
        let record = VmtiLs { targets: targets.clone(), ..Default::default() };
        let bytes = st0903::encode_to_vec(&record).expect("encode");
        let decoded = st0903::decode(&bytes).expect("decode");
        prop_assert_eq!(decoded.targets, targets);
    }
}
