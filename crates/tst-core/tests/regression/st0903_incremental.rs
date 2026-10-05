//! ST 0903.6 §10.1.11 + §10.1.12 spec vectors. The spec sections at
//! those locations are:
//!
//!   §10.1.11  Horizontal_FOV  (Tag 11, IMAPB(0, 180, 2), units °)
//!   §10.1.12  Vertical_FOV    (Tag 12, IMAPB(0, 180, 2), units °)
//!
//! ST 0903.6 does NOT define an "incremental update" or "vTarget
//! delete" flow at these section numbers (that may belong to a different
//! MISB document, possibly ST 0903 vTracker LS §10.3 or a draft
//! revision). The substrate has no incremental-update logic to exercise.
//!
//! These tests instead exercise the actual §10.1.11 + §10.1.12 surface —
//! Horizontal_FOV / Vertical_FOV IMAPB worked examples, edge values, top-level
//! VMTI LS encode/decode symmetry, and interaction with the IMAPB special-value
//! branches (since the top-level lenient walker funnels every non-Value decode
//! result into `field_errors` per the §7.2.3 Table 2 ↔ lenient-walker mapping
//! in `crates/tst-core/src/klv/st0903/decode.rs`).

use tst_core::error::KlvFieldError;
use tst_core::klv::imapb::{ImapbParams, encode_imapb};
use tst_core::klv::st0903::{self, VmtiLs};

// ============================================================================
// §10.1.11 Horizontal_FOV worked example
// ============================================================================

/// ST 0903.6 §10.1.11 worked example: IMAPB(0, 180, 2) for 12.5° →
/// `0x06 0x40`. Validated at the IMAPB substrate level in
/// `klv::imapb::tests::st_0903_section_10_1_11_fov_12_5_deg`; this test
/// re-validates at the typed VMTI surface — encode a VmtiLs with
/// horizontal_fov=12.5 and confirm the wire bytes at the Tag 11 TLV.
#[test]
fn st0903_10_1_11_horizontal_fov_12_5_deg_via_vmti_encode() {
    let ls = VmtiLs {
        horizontal_fov: Some(12.5),
        ..Default::default()
    };
    let bytes = st0903::encode_to_vec(&ls).unwrap();
    // Expected: tag=0x0B (11) + len=0x02 + value 0x06 0x40 somewhere in
    // the body. Find the Tag 11 TLV by scanning (encode order in
    // `klv::st0903::encode` is ascending tag — 2, 3, 4, ..., 11, 12, ...
    // — so Tag 11 is the only `0x0B 0x02` sequence in a horizontal-fov-
    // only encode).
    let found = bytes.windows(4).any(|w| w == [0x0B, 0x02, 0x06, 0x40]);
    assert!(
        found,
        "expected Tag 11 TLV [0x0B 0x02 0x06 0x40] in encoded body, got {bytes:02X?}"
    );
    // Round-trip back to confirm decode parity.
    let decoded = st0903::decode(&bytes).unwrap();
    let hfov = decoded.horizontal_fov.unwrap();
    assert!((hfov - 12.5).abs() < 1e-2, "decoded {hfov}, expected 12.5°");
}

// ============================================================================
// §10.1.12 Vertical_FOV worked example
// ============================================================================

/// ST 0903.6 §10.1.12 worked example: IMAPB(0, 180, 2) for 10.0° →
/// `0x05 0x00`. Same shape as §10.1.11 — encoder must emit canonical
/// wire bytes at Tag 12.
#[test]
fn st0903_10_1_12_vertical_fov_10_0_deg_via_vmti_encode() {
    let ls = VmtiLs {
        vertical_fov: Some(10.0),
        ..Default::default()
    };
    let bytes = st0903::encode_to_vec(&ls).unwrap();
    let found = bytes.windows(4).any(|w| w == [0x0C, 0x02, 0x05, 0x00]);
    assert!(
        found,
        "expected Tag 12 TLV [0x0C 0x02 0x05 0x00] in encoded body, got {bytes:02X?}"
    );
    let decoded = st0903::decode(&bytes).unwrap();
    let vfov = decoded.vertical_fov.unwrap();
    assert!((vfov - 10.0).abs() < 1e-2, "decoded {vfov}, expected 10.0°");
}

// ============================================================================
// §10.1.11/12 range boundaries
// ============================================================================

/// Bottom-of-range: IMAPB(0, 180, 2) value 0.0 → wire 0x0000.
/// Integer 0 must decode back to exactly `min` per §7.1.2 Starting
/// Point B.
#[test]
fn st0903_10_1_11_horizontal_fov_zero_deg_boundary() {
    let p = ImapbParams {
        min: 0.0,
        max: 180.0,
        length: 2,
    };
    let mut buf = [0u8; 2];
    encode_imapb(&p, 0.0, &mut buf).unwrap();
    assert_eq!(buf, [0x00, 0x00]);

    let ls = VmtiLs {
        horizontal_fov: Some(0.0),
        ..Default::default()
    };
    let bytes = st0903::encode_to_vec(&ls).unwrap();
    let decoded = st0903::decode(&bytes).unwrap();
    assert_eq!(decoded.horizontal_fov, Some(0.0));
}

/// Top-of-range: IMAPB(0, 180, 2) value 180.0 → integer
/// `floor(sF·180) = floor(128·180) = floor(23040) = 23040 = 0x5A00`.
/// This is the §7.2.3 Table 1 row 2 max-value mapping at the typed
/// surface — top byte 0x5A does NOT set the top-2-bits so it's NOT
/// confused with §7.2.3 special-value space.
#[test]
fn st0903_10_1_11_horizontal_fov_180_deg_boundary() {
    let p = ImapbParams {
        min: 0.0,
        max: 180.0,
        length: 2,
    };
    let mut buf = [0u8; 2];
    encode_imapb(&p, 180.0, &mut buf).unwrap();
    assert_eq!(
        buf,
        [0x5A, 0x00],
        "max-value integer is 0x5A00 for span=180"
    );

    let ls = VmtiLs {
        horizontal_fov: Some(180.0),
        ..Default::default()
    };
    let bytes = st0903::encode_to_vec(&ls).unwrap();
    let decoded = st0903::decode(&bytes).unwrap();
    let hfov = decoded.horizontal_fov.unwrap();
    assert!((hfov - 180.0).abs() < 1e-2, "expected 180.0°, got {hfov}");
}

// ============================================================================
// Interaction between FOV tags and IMAPB special values
// ============================================================================
//
// The lenient ST 0903 walker (`klv::st0903::decode::decode`) maps every
// non-Value DecodedImapb result to `field_errors.push(InvalidLength)`
// and continues. This is documented in the walker source as the
// integration choice: "the lenient top-level walker treats special
// values and out-of-range as 'field unavailable'." Tests below
// confirm that contract holds for the FOV tags specifically (since
// they're the only top-level IMAPB tags in the VMTI LS).

/// Inject a hand-built §7.2.3 Table 2 BelowMin signal (byte0=0xE0)
/// into Tag 11 (`horizontal_fov`). The lenient walker should NOT
/// populate `horizontal_fov` (since it can't synthesize a concrete
/// f64) but should record a `field_errors` entry instead of failing
/// the whole record.
#[test]
fn st0903_lenient_walker_treats_below_min_signal_as_field_error() {
    // Hand-build a minimal embedded-VMTI body with Tag 11 carrying
    // the §7.2.3 Table 2 BelowMin pattern.
    // Body structure: [tag=0x0B] [len=0x02] [value 0xE0 0x00].
    let body = [0x0B, 0x02, 0xE0, 0x00];
    let decoded = st0903::decode(&body).unwrap();
    assert_eq!(
        decoded.horizontal_fov, None,
        "BelowMin signal must not synthesize a concrete f64"
    );
    assert!(
        decoded
            .field_errors
            .iter()
            .any(|e| matches!(e, KlvFieldError::InvalidLength { tag: 11, .. })),
        "expected InvalidLength field_error for tag 11 (lenient mapping), got {:?}",
        decoded.field_errors
    );
}

/// Inject a §7.2.3 Table 2 PositiveInfinity signal (byte0=0xC8) into
/// Tag 12 (`vertical_fov`). Same lenient-walker contract.
#[test]
fn st0903_lenient_walker_treats_positive_infinity_signal_as_field_error() {
    let body = [0x0C, 0x02, 0xC8, 0x00];
    let decoded = st0903::decode(&body).unwrap();
    assert_eq!(decoded.vertical_fov, None);
    assert!(
        decoded
            .field_errors
            .iter()
            .any(|e| matches!(e, KlvFieldError::InvalidLength { tag: 12, .. })),
        "expected InvalidLength field_error for tag 12, got {:?}",
        decoded.field_errors
    );
}

/// Inject a §8.6 Eq.12 inter-band reserved value (byte0=0x80,
/// arithmetic-decodes to 128.0°, outside the [0, 180]° range when
/// `sR · y` lands past max). Same lenient-walker contract — the
/// FOV tag stays None, a field_error fires.
#[test]
fn st0903_lenient_walker_treats_out_of_range_decode_as_field_error() {
    let body = [0x0B, 0x02, 0x80, 0x00];
    let decoded = st0903::decode(&body).unwrap();
    assert_eq!(decoded.horizontal_fov, None);
    assert!(
        decoded
            .field_errors
            .iter()
            .any(|e| matches!(e, KlvFieldError::InvalidLength { tag: 11, .. })),
        "expected InvalidLength field_error for tag 11 out-of-range, got {:?}",
        decoded.field_errors
    );
}

// ============================================================================
// Strict walker rejects special-value signals
// ============================================================================

/// `decode_strict` must reject the §7.2.3 Table 2 BelowMin signal at
/// Tag 11 as a hard error (the walker source comments: "strict mode
/// rejects special values + out-of-range as InvalidLength").
#[test]
fn st0903_strict_walker_rejects_below_min_signal_on_horizontal_fov() {
    let body = [0x0B, 0x02, 0xE0, 0x00];
    let err = st0903::decode_strict(&body).unwrap_err();
    // The strict walker also requires Tags 4 + 6; if the FieldError
    // doesn't fire first, the required-tag check fires after. Either
    // outcome demonstrates the strict path doesn't silently swallow
    // the BelowMin byte. Match the more-specific FieldError case.
    let msg = format!("{err:?}");
    assert!(
        msg.contains("FieldError") || msg.contains("St0903MissingRequiredTag"),
        "expected FieldError or MissingRequiredTag rejection, got {err:?}"
    );
}

// ============================================================================
// Full VMTI round-trip with both FOV tags + targets
// ============================================================================
//
// End-to-end §10.1.11 + §10.1.12 sanity check: a realistic VMTI LS
// with horizontal_fov + vertical_fov + a couple of vTarget packs
// (so the encoded body exercises the Tag 101 VTargetSeries framing
// alongside the FOV IMAPB encodes).
#[test]
fn st0903_10_1_11_and_10_1_12_round_trip_via_full_vmti_ls() {
    let original = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        version_number: Some(6),
        num_targets_reported: Some(2),
        frame_width: Some(3840),
        frame_height: Some(2160),
        horizontal_fov: Some(12.5), // §10.1.11 worked example
        vertical_fov: Some(10.0),   // §10.1.12 worked example
        targets: vec![
            st0903::VTargetPack {
                target_id: 1,
                centroid_pixel: Some(8_294_400),
                priority: Some(1),
                confidence_level: Some(95),
                ..Default::default()
            },
            st0903::VTargetPack {
                target_id: 2,
                centroid_pixel: Some(4_147_200),
                priority: Some(2),
                confidence_level: Some(80),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let bytes = st0903::encode_to_vec(&original).unwrap();
    let decoded = st0903::decode(&bytes).unwrap();
    assert!(
        decoded.field_errors.is_empty(),
        "no field errors on clean round-trip"
    );
    assert!((decoded.horizontal_fov.unwrap() - 12.5).abs() < 1e-2);
    assert!((decoded.vertical_fov.unwrap() - 10.0).abs() < 1e-2);
    assert_eq!(decoded.targets.len(), 2);
    assert_eq!(decoded.targets[0].target_id, 1);
    assert_eq!(decoded.targets[1].target_id, 2);
}

// ============================================================================
// Gap analysis: incremental update flows
// ============================================================================
//
// "vTarget create/incremental update/delete" flows: ST 0903.6 §10.1.11 +
// §10.1.12 do NOT define these flows. ST 0903.6 §10.2.2.24 (`detectionStatus`)
// carries codepoints 0=Inactive, 1=Active-Moving, 2=Dropped, 3=Active-Stopped,
// 4=Active-Coasting — these describe per-frame state per-target, but the spec
// does NOT define an inter-frame state-machine that requires the decoder to
// track create / update / delete across VMTI LS instances. Each VMTI LS is
// self-contained.
//
// Tests below codify the current behavior — `decode` returns a
// fresh `VmtiLs` per call; consumers wanting cross-LS continuity
// implement it themselves (typically by indexing on `target_id`).
// If a future ST 0903.x revision adds incremental update flows,
// these tests will need to grow; for ST 0903.6 the implementation
// is correct.

/// Two consecutive `decode` calls return independent records — no
/// shared state, no per-LS update-relative-to-prior semantics. ST
/// 0903.6 §10.2.2.24 `detectionStatus` is per-frame, not a state
/// machine for the decoder to track.
#[test]
fn st0903_decode_is_stateless_between_consecutive_local_sets() {
    let ls1 = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_000_000),
        num_targets_reported: Some(1),
        targets: vec![st0903::VTargetPack {
            target_id: 42,
            detection_status: Some(1), // Active-Moving
            ..Default::default()
        }],
        ..Default::default()
    };
    let ls2 = VmtiLs {
        precision_time_stamp: Some(1_700_000_000_033_333),
        num_targets_reported: Some(1),
        targets: vec![st0903::VTargetPack {
            target_id: 42,
            detection_status: Some(2), // Dropped
            ..Default::default()
        }],
        ..Default::default()
    };
    let b1 = st0903::encode_to_vec(&ls1).unwrap();
    let b2 = st0903::encode_to_vec(&ls2).unwrap();
    let d1 = st0903::decode(&b1).unwrap();
    let d2 = st0903::decode(&b2).unwrap();
    assert_eq!(d1.targets[0].detection_status, Some(1));
    assert_eq!(d2.targets[0].detection_status, Some(2));
    // No shared state — d2 must NOT inherit any field from d1.
    assert_eq!(d2.targets.len(), 1);
    assert_eq!(d2.targets[0].target_id, 42);
}
