//! ST 0902 Minimum Metadata Set (MISMMS) record-level validator.
//!
//! # Semantics
//!
//! [`validate_mismms`] performs a record-level check of ST 0902.8 Table 1
//! (the 23-item Minimum Metadata Set). All violations found are returned;
//! an empty `Vec` means the record satisfies the MISMMS requirements.
//!
//! **Presence** is satisfied when a typed field is `Some(_)` with non-empty
//! content OR when `record.unknown` contains an entry with that tag number
//! and a non-empty value. Zero-length wire values do NOT satisfy presence
//! and additionally produce a [`MismmsViolation::ZeroLengthItem`] (ST 0902.8-05).
//! This applies to both typed fields and unknown entries.
//!
//! **Alternation groups** (Note 1):
//! - Tags 6 | 90 — inclusive-or (either satisfies the requirement).
//! - Tags 7 | 91 — inclusive-or.
//! - Tags 22 | 96 — inclusive-or.
//! - Tags 25 | 78 — inclusive-or.
//! - Tags 15 | 75 | 104 — any one satisfies presence; however Tags 75 and
//!   104 are **exclusive-or**: if both are present
//!   [`MismmsViolation::AlternationConflict`]`{ tag_a: 75, tag_b: 104 }` is
//!   reported even if the requirement is otherwise satisfied.
//!
//! **Tag 48 (Security Local Set)** must decode via
//! [`crate::klv::st0102::decode`] and contain all nine MISMMS-required
//! sub-items (security classification, classifying country coding method,
//! classifying country, SCI/SHI info, caveats, releasing instructions,
//! object country coding method, object country codes, version). A ST 0102
//! decode failure reports [`MismmsViolation::MissingItem`]`{ tag: 48, … }`
//! rather than a per-sub-item violation.
//!
//! **Tags 1 (Checksum) and 65 (UAS Datalink LS Version)** are NOT checked:
//! the `encode*` entry points auto-emit both, and `decode` verifies the
//! checksum on ingest. Checking them here would either always pass (on
//! encode-produced records) or duplicate the decode contract.
//!
//! **Out-of-scope:** the 30-second reporting-cadence rule (ST 0902.3-04) and
//! the stream-level requirement ST 1204.1-34 are stream-level checks that
//! require a sequence of records; they are documented here for completeness
//! but are NOT enforced by this function.

use crate::klv::st0102;
use crate::klv::st0601::encode::each_typed_field;
use crate::klv::st0601::model::{EncodeConfig, UasDatalinkLs};
use alloc::vec::Vec;

// ============================================================================
// Public types
// ============================================================================

/// A violation of the ST 0902.8 Minimum Metadata Set requirements.
///
/// See [`validate_mismms`] for full semantics.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MismmsViolation {
    /// A required MISMMS item (Tag `tag`, human-readable `name`) is absent
    /// from the record. For Tag 48, this also covers the case where the
    /// Security Local Set bytes are present but fail to decode via ST 0102.
    #[error("MISMMS missing required item: Tag {tag} ({name})")]
    MissingItem { tag: u8, name: &'static str },

    /// A required sub-item of the ST 0102 Security Local Set (Tag 48) is
    /// absent. `tag` is the ST 0102 item number; `name` is its label.
    #[error("MISMMS Security LS (Tag 48) missing required sub-item: Tag {tag} ({name})")]
    MissingSecurityItem { tag: u8, name: &'static str },

    /// Tag `tag` has a zero-length wire value (from either a typed field or a
    /// `record.unknown` entry), which does NOT satisfy MISMMS presence (ST 0902.8-05).
    #[error("MISMMS Tag {tag} has zero-length wire value (does not satisfy presence)")]
    ZeroLengthItem { tag: u8 },

    /// Tags `tag_a` (75) and `tag_b` (104) are both present. Within the
    /// `15 | 75 | 104` group, Tags 75 and 104 are exclusive-or: only one
    /// may be used at a time.
    #[error(
        "MISMMS alternation conflict: Tags {tag_a} and {tag_b} are mutually exclusive \
         within the 15|75|104 group"
    )]
    AlternationConflict { tag_a: u8, tag_b: u8 },
}

// ============================================================================
// MISMMS table
// ============================================================================

struct MismmsReq {
    /// Any one of these tags satisfies the requirement.
    tags: &'static [u8],
    name: &'static str,
}

const MISMMS: &[MismmsReq] = &[
    MismmsReq {
        tags: &[2],
        name: "Precision Time Stamp",
    },
    MismmsReq {
        tags: &[3],
        name: "Mission ID",
    },
    MismmsReq {
        tags: &[5],
        name: "Platform Heading Angle",
    },
    MismmsReq {
        tags: &[6, 90],
        name: "Platform Pitch Angle",
    },
    MismmsReq {
        tags: &[7, 91],
        name: "Platform Roll Angle",
    },
    MismmsReq {
        tags: &[10],
        name: "Platform Designation",
    },
    MismmsReq {
        tags: &[11],
        name: "Image Source Sensor",
    },
    MismmsReq {
        tags: &[12],
        name: "Image Coordinate System",
    },
    MismmsReq {
        tags: &[13],
        name: "Sensor Latitude",
    },
    MismmsReq {
        tags: &[14],
        name: "Sensor Longitude",
    },
    MismmsReq {
        tags: &[15, 75, 104],
        name: "Sensor Altitude / Ellipsoid Height",
    },
    MismmsReq {
        tags: &[16],
        name: "Sensor Horizontal FoV",
    },
    MismmsReq {
        tags: &[17],
        name: "Sensor Vertical FoV",
    },
    MismmsReq {
        tags: &[18],
        name: "Sensor Relative Azimuth Angle",
    },
    MismmsReq {
        tags: &[19],
        name: "Sensor Relative Elevation Angle",
    },
    MismmsReq {
        tags: &[20],
        name: "Sensor Relative Roll Angle",
    },
    MismmsReq {
        tags: &[21],
        name: "Slant Range",
    },
    MismmsReq {
        tags: &[22, 96],
        name: "Target Width",
    },
    MismmsReq {
        tags: &[23],
        name: "Frame Center Latitude",
    },
    MismmsReq {
        tags: &[24],
        name: "Frame Center Longitude",
    },
    MismmsReq {
        tags: &[25, 78],
        name: "Frame Center Elevation / Height Above Ellipsoid",
    },
    MismmsReq {
        tags: &[48],
        name: "Security Local Set",
    },
    MismmsReq {
        tags: &[94],
        name: "MIIS Core Identifier",
    },
];

// ST 0102 sub-items required by MISMMS (tag, name).
const SECURITY_REQUIRED: &[(u8, &str)] = &[
    (1, "security_classification"),
    (2, "classifying_country_coding_method"),
    (3, "classifying_country"),
    (4, "sci_shi_info"),
    (5, "caveats"),
    (6, "releasing_instructions"),
    (12, "object_country_coding_method"),
    (13, "object_country_codes"),
    (22, "version"),
];

// ============================================================================
// Validator
// ============================================================================

/// Validate a [`UasDatalinkLs`] record against the ST 0902.8 Minimum
/// Metadata Set (Table 1).
///
/// Returns a `Vec` of all violations found; an empty `Vec` means the record
/// satisfies every MISMMS requirement at the record level.
///
/// See the module-level documentation for full semantics, including
/// alternation-group rules, the Tag 48 sub-item check, and out-of-scope
/// stream-level requirements.
#[must_use]
pub fn validate_mismms(record: &UasDatalinkLs) -> Vec<MismmsViolation> {
    let mut violations = Vec::new();

    // ----------------------------------------------------------------
    // Step 1: collect present typed tags via the encode visitor.
    // EncodeConfig::default() is fine — _opts is unused in the visitor.
    // Zero-length typed values do NOT satisfy presence.
    // ----------------------------------------------------------------
    let opts = EncodeConfig::default();
    let mut typed_present: [bool; 256] = [false; 256];
    each_typed_field(record, &opts, |tag, len| {
        if len == 0 {
            // Zero-length typed value — record violation but do not set presence.
            violations.push(MismmsViolation::ZeroLengthItem { tag });
        } else {
            // Non-empty typed value satisfies presence.
            typed_present[tag as usize] = true;
        }
    });
    // Tag 65 is auto-emitted by the visitor (auto_version branch); exclude it
    // from the presence map so it cannot contaminate MISMMS checks.
    typed_present[65] = false;

    // ----------------------------------------------------------------
    // Step 2: collect unknown tags — track presence and zero-length.
    // ----------------------------------------------------------------
    let mut unknown_present: [bool; 256] = [false; 256];
    for f in &record.unknown {
        let Ok(tag) = u8::try_from(f.tag) else {
            continue;
        };
        if f.value.is_empty() {
            violations.push(MismmsViolation::ZeroLengthItem { tag });
            // Zero-length does NOT satisfy presence — do not set the flag.
        } else {
            unknown_present[tag as usize] = true;
        }
    }

    // ----------------------------------------------------------------
    // Helper: is tag `t` considered present?
    // ----------------------------------------------------------------
    let present = |t: u8| typed_present[t as usize] || unknown_present[t as usize];

    // ----------------------------------------------------------------
    // Step 3: walk the MISMMS requirement table.
    // ----------------------------------------------------------------
    for req in MISMMS {
        let satisfied = req.tags.iter().any(|&t| present(t));
        if !satisfied {
            // Report the first/primary tag as the canonical violation tag.
            violations.push(MismmsViolation::MissingItem {
                tag: req.tags[0],
                name: req.name,
            });
        }
    }

    // ----------------------------------------------------------------
    // Step 4: Tag 75 / Tag 104 exclusive-or within the 15|75|104 group.
    // ----------------------------------------------------------------
    if present(75) && present(104) {
        violations.push(MismmsViolation::AlternationConflict {
            tag_a: 75,
            tag_b: 104,
        });
    }

    // ----------------------------------------------------------------
    // Step 5: Tag 48 Security Local Set sub-item check.
    // ----------------------------------------------------------------
    // Only run the sub-item check if Tag 48 is present (typed or unknown).
    // If absent, the MissingItem{48} violation was already added in Step 3.
    if present(48) {
        // Prefer the typed field only when non-empty; otherwise use the first
        // non-empty unknown Tag 48 entry.  A typed Some(vec![]) does not satisfy
        // presence and must not shadow a non-empty unknown entry.
        let security_bytes: Option<&[u8]> = match &record.security_local_set {
            Some(v) if !v.is_empty() => Some(v.as_slice()),
            _ => record
                .unknown
                .iter()
                .find(|f| f.tag == 48 && !f.value.is_empty())
                .map(|f| f.value.as_slice()),
        };

        match security_bytes {
            None => {
                // present() returned true but we found no bytes — shouldn't happen,
                // but guard conservatively.
                violations.push(MismmsViolation::MissingItem {
                    tag: 48,
                    name: "Security Local Set",
                });
            }
            Some(bytes) => match st0102::decode(bytes) {
                Err(_) => {
                    violations.push(MismmsViolation::MissingItem {
                        tag: 48,
                        name: "Security Local Set",
                    });
                }
                Ok(sec) => {
                    for &(sub_tag, sub_name) in SECURITY_REQUIRED {
                        let sub_present = match sub_tag {
                            1 => sec.security_classification.is_some(),
                            2 => sec.classifying_country_coding_method.is_some(),
                            3 => sec.classifying_country.is_some(),
                            4 => sec.sci_shi_info.is_some(),
                            5 => sec.caveats.is_some(),
                            6 => sec.releasing_instructions.is_some(),
                            12 => sec.object_country_coding_method.is_some(),
                            13 => sec.object_country_codes.is_some(),
                            22 => sec.version.is_some(),
                            _ => false,
                        };
                        if !sub_present {
                            violations.push(MismmsViolation::MissingSecurityItem {
                                tag: sub_tag,
                                name: sub_name,
                            });
                        }
                    }
                }
            },
        }
    }

    violations
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::klv::pack::OwnedRawField;
    use crate::klv::st0102::{
        ClassifyingCountryCodingMethod, ObjectCountryCodingMethod, SecurityClassification,
        SecurityLs,
    };

    /// Build a Security Local Set with all 9 MISMMS-required sub-items.
    fn full_security_ls_bytes() -> Vec<u8> {
        let sec = SecurityLs {
            security_classification: Some(SecurityClassification::Unclassified),
            classifying_country_coding_method: Some(
                ClassifyingCountryCodingMethod::Iso3166ThreeLetter,
            ),
            classifying_country: Some("//USA".to_string()),
            sci_shi_info: Some("SCI".to_string()),
            caveats: Some("FOUO".to_string()),
            releasing_instructions: Some("USA".to_string()),
            object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166ThreeLetter),
            object_country_codes: Some("USA".to_string()),
            version: Some(12),
            ..Default::default()
        };
        crate::klv::st0102::encode_to_vec(&sec).expect("valid security LS")
    }

    /// Build a record that satisfies all 23 MISMMS requirements.
    fn full_mismms_record() -> UasDatalinkLs {
        UasDatalinkLs {
            timestamp_us: Some(1_700_000_000_000_000),          // Tag 2
            mission_id: Some("MISSION-1".to_string()),          // Tag 3
            platform_heading_deg: Some(45.0),                   // Tag 5
            platform_pitch_deg: Some(5.0),                      // Tag 6  (6|90)
            platform_roll_deg: Some(2.0),                       // Tag 7  (7|91)
            platform_designation: Some("UAV-1".to_string()),    // Tag 10
            image_source_sensor: Some("EO".to_string()),        // Tag 11
            image_coordinate_system: Some("WGS84".to_string()), // Tag 12
            sensor_lat_deg: Some(47.0),                         // Tag 13
            sensor_lon_deg: Some(-122.0),                       // Tag 14
            sensor_alt_m: Some(1500.0),                         // Tag 15  (15|75|104)
            sensor_hfov_deg: Some(5.0),                         // Tag 16
            sensor_vfov_deg: Some(3.75),                        // Tag 17
            sensor_rel_az_deg: Some(180.0),                     // Tag 18
            sensor_rel_el_deg: Some(-30.0),                     // Tag 19
            sensor_rel_roll_deg: Some(0.5),                     // Tag 20
            slant_range_m: Some(5000.0),                        // Tag 21
            target_width_m: Some(100.0),                        // Tag 22 (22|96)
            frame_center_lat_deg: Some(46.9),                   // Tag 23
            frame_center_lon_deg: Some(-122.1),                 // Tag 24
            frame_center_elev_m: Some(50.0),                    // Tag 25 (25|78)
            security_local_set: Some(full_security_ls_bytes()), // Tag 48
            miis_core_id: Some(vec![
                // Tag 94 — 16-byte MIIS Core ID
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00,
            ]),
            ..UasDatalinkLs::default()
        }
    }

    #[test]
    fn full_record_no_violations() {
        let record = full_mismms_record();
        let v = validate_mismms(&record);
        assert!(
            v.is_empty(),
            "full MISMMS record should produce no violations; got: {v:?}"
        );
    }

    #[test]
    fn missing_mission_id_single_violation() {
        let mut record = full_mismms_record();
        record.mission_id = None;
        let v = validate_mismms(&record);
        assert_eq!(
            v,
            [MismmsViolation::MissingItem {
                tag: 3,
                name: "Mission ID"
            }],
            "dropping mission_id should yield exactly MissingItem{{tag:3}}"
        );
    }

    #[test]
    fn alternation_90_without_6_no_violation() {
        // Tag 90 (Platform Pitch Full) alone satisfies the 6|90 group.
        let mut record = full_mismms_record();
        record.platform_pitch_deg = None; // Tag 6 absent
        record.platform_pitch_full_deg = Some(20.0); // Tag 90 present
        let v = validate_mismms(&record);
        assert!(
            !v.iter().any(|vi| matches!(
                vi,
                MismmsViolation::MissingItem { tag: 6, .. }
                    | MismmsViolation::MissingItem { tag: 90, .. }
            )),
            "Tag 90 alone should satisfy the 6|90 group; violations: {v:?}"
        );
    }

    #[test]
    fn alternation_conflict_75_and_104() {
        // Tags 75 (typed) and 104 (via unknown, non-empty) both present.
        let mut record = full_mismms_record();
        record.sensor_alt_m = None; // Remove Tag 15 to keep test focused
        record.sensor_ellipsoid_height_m = Some(1500.0); // Tag 75 typed
        record.unknown.push(OwnedRawField {
            tag: 104,
            value: vec![0x01, 0x02], // non-empty → satisfies presence
        });
        let v = validate_mismms(&record);
        assert!(
            v.contains(&MismmsViolation::AlternationConflict {
                tag_a: 75,
                tag_b: 104,
            }),
            "both Tag 75 and Tag 104 present should produce AlternationConflict; got: {v:?}"
        );
        // The 15|75|104 requirement is satisfied (Tag 75 is present).
        assert!(
            !v.iter()
                .any(|vi| matches!(vi, MismmsViolation::MissingItem { tag: 15, .. })),
            "requirement should be satisfied despite conflict; violations: {v:?}"
        );
    }

    #[test]
    fn wpb_mismms_typed_96_104() {
        // The extended-range IMAPB items (96/104) are wired into presence via
        // `each_typed_field`'s generic Imapb match arm. This pins that:
        // typed-only 96/104 satisfy the 22|96 and 15|75|104 groups, and setting
        // both 75 and 104 reaches the pre-existing exclusive-or conflict.
        let mut rec = full_mismms_record();
        rec.target_width_m = None;
        rec.target_width_extended_m = Some(100.0); // 22|96 satisfied via typed 96
        rec.sensor_alt_m = None;
        rec.sensor_ellipsoid_height_m = None;
        rec.sensor_ellipsoid_height_extended_m = Some(1500.0); // 15|75|104 via typed 104
        assert!(
            validate_mismms(&rec).is_empty(),
            "typed 96/104 alone should satisfy MISMMS presence; got: {:?}",
            validate_mismms(&rec)
        );

        // 75 XOR 104 conflict now reachable with BOTH typed.
        rec.sensor_ellipsoid_height_m = Some(1500.0);
        let v = validate_mismms(&rec);
        assert!(
            v.iter().any(|vi| matches!(
                vi,
                MismmsViolation::AlternationConflict {
                    tag_a: 75,
                    tag_b: 104,
                }
            )),
            "both Tag 75 and Tag 104 typed-present should produce AlternationConflict; got: {v:?}"
        );
    }

    #[test]
    fn security_ls_missing_caveats() {
        // Build a Security LS without caveats (Tag 5).
        let sec = SecurityLs {
            security_classification: Some(SecurityClassification::Unclassified),
            classifying_country_coding_method: Some(
                ClassifyingCountryCodingMethod::Iso3166ThreeLetter,
            ),
            classifying_country: Some("//USA".to_string()),
            sci_shi_info: Some("SCI".to_string()),
            caveats: None, // OMITTED
            releasing_instructions: Some("USA".to_string()),
            object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166ThreeLetter),
            object_country_codes: Some("USA".to_string()),
            version: Some(12),
            ..Default::default()
        };
        let sec_bytes = crate::klv::st0102::encode_to_vec(&sec).unwrap();
        let mut record = full_mismms_record();
        record.security_local_set = Some(sec_bytes);
        let v = validate_mismms(&record);
        assert!(
            v.contains(&MismmsViolation::MissingSecurityItem {
                tag: 5,
                name: "caveats",
            }),
            "missing caveats should produce MissingSecurityItem{{tag:5}}; got: {v:?}"
        );
    }

    #[test]
    fn zero_length_unknown_tag_96_and_missing_group() {
        // Tag 96 present but zero-length; Tag 22 absent.
        // Expect: ZeroLengthItem{96} AND MissingItem{22} (for the 22|96 group).
        let mut record = full_mismms_record();
        record.target_width_m = None; // Remove Tag 22
        record.unknown.push(OwnedRawField {
            tag: 96,
            value: vec![], // zero-length
        });
        let v = validate_mismms(&record);
        assert!(
            v.contains(&MismmsViolation::ZeroLengthItem { tag: 96 }),
            "zero-length Tag 96 should produce ZeroLengthItem{{96}}; got: {v:?}"
        );
        assert!(
            v.contains(&MismmsViolation::MissingItem {
                tag: 22,
                name: "Target Width",
            }),
            "absent Tag 22 with zero-length Tag 96 should yield MissingItem{{22}}; got: {v:?}"
        );
    }

    #[test]
    fn zero_length_typed_vmti() {
        // vmti = Some(vec![]) — empty typed bytes vector.
        // Expect: ZeroLengthItem{74} AND MissingItem{20} (Tag 74 is not in MISMMS,
        // but to get a required tag with zero-length, use miis_core_id at Tag 94).
        // For a simpler test, use unknown with a MISMMS tag instead.
        // Actually, let's test that a typed-field zero-length still triggers
        // the violation even if it's not in MISMMS — use Tag 74 (VMTI).
        let mut record = full_mismms_record();
        record.vmti = Some(vec![]); // empty, not None
        let v = validate_mismms(&record);

        let zero_length_count = v
            .iter()
            .filter(|vi| matches!(vi, MismmsViolation::ZeroLengthItem { tag: 74 }))
            .count();
        assert_eq!(
            zero_length_count, 1,
            "should produce exactly one ZeroLengthItem{{tag:74}}; got: {v:?}"
        );
        // Tag 74 is not in the MISMMS requirement table, so there should be
        // no MissingItem for it — just the ZeroLengthItem.
    }

    #[test]
    fn zero_length_typed_security_local_set() {
        // security_local_set = Some(vec![]) — empty typed bytes.
        // Expect: ZeroLengthItem{48} AND MissingItem{48} (empty doesn't satisfy presence).
        let mut record = full_mismms_record();
        record.security_local_set = Some(vec![]); // empty, not None
        let v = validate_mismms(&record);

        let zero_length_count = v
            .iter()
            .filter(|vi| matches!(vi, MismmsViolation::ZeroLengthItem { tag: 48 }))
            .count();
        assert_eq!(
            zero_length_count, 1,
            "should produce exactly one ZeroLengthItem{{tag:48}}; got: {v:?}"
        );

        let missing_count = v
            .iter()
            .filter(|vi| {
                matches!(
                    vi,
                    MismmsViolation::MissingItem {
                        tag: 48,
                        name: "Security Local Set"
                    }
                )
            })
            .count();
        assert_eq!(
            missing_count, 1,
            "should produce exactly one MissingItem{{tag:48, name:\"Security Local Set\"}}; got: {v:?}"
        );

        assert_eq!(
            v.len(),
            2,
            "should produce exactly 2 violations for empty security_local_set; got: {v:?}"
        );
    }

    #[test]
    fn empty_typed_security_ls_with_non_empty_unknown_tag48() {
        // security_local_set = Some(vec![]) (typed, empty) PLUS a non-empty valid
        // unknown Tag 48 entry.  The source-selection fix must prefer the unknown
        // non-empty bytes over the typed empty vec so the sub-item check runs.
        let mut record = full_mismms_record();
        record.security_local_set = Some(vec![]); // typed but empty → ZeroLengthItem{48}
        record.unknown.push(OwnedRawField {
            tag: 48,
            value: full_security_ls_bytes(), // non-empty valid ST 0102 bytes
        });

        let v = validate_mismms(&record);

        // The typed empty still fires ZeroLengthItem{48}.
        assert!(
            v.contains(&MismmsViolation::ZeroLengthItem { tag: 48 }),
            "typed empty security_local_set should still produce ZeroLengthItem{{48}}; got: {v:?}"
        );

        // But the sub-item check must run against the unknown non-empty bytes →
        // no MissingItem{48} and no MissingSecurityItem.
        assert!(
            !v.iter()
                .any(|vi| matches!(vi, MismmsViolation::MissingItem { tag: 48, .. })),
            "non-empty unknown Tag 48 must satisfy presence; got: {v:?}"
        );
        assert!(
            !v.iter()
                .any(|vi| matches!(vi, MismmsViolation::MissingSecurityItem { .. })),
            "sub-item check should pass against the non-empty unknown bytes; got: {v:?}"
        );
    }
}
