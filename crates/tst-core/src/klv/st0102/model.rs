//! ST 0102 typed model — the `SecurityLs` flat struct, its manual
//! `PartialEq` impl, and the UTF-16 BOM helpers shared by decode/encode.

use crate::error::KlvFieldError;
use crate::klv::pack::OwnedRawField;
use crate::klv::st0102::enums::{
    ClassifyingCountryCodingMethod, ObjectCountryCodingMethod, SecurityClassification,
};
use alloc::string::String;
use alloc::vec::Vec;

crate::binding_record! {
    /// MISB ST 0102.12 Security Metadata Local Set typed view.
    ///
    /// Required tags per ST 0102.12 §6.7 Table 1: 1, 2, 3, 12, 13 and 22.
    /// Lenient decode tolerates missing required tags, unknown enum codepoints
    /// and a malformed Tag 13 UTF-16 value (surfaced in `field_errors`); strict
    /// decode rejects all of them.
    ///
    /// The three enum-typed fields (`security_classification`,
    /// `classifying_country_coding_method` and `object_country_coding_method`)
    /// cross as their code point, so a forward-compat code point round-trips; a
    /// host with an enum type for the known code points may surface the member.
    #[must_use]
    #[derive(Debug, Clone, Default)]
    pub struct SecurityLs {
        // Required per spec (still Option<T> at decode time so lenient
        // mode tolerates broken input; decode_strict rejects a record
        // missing any of 1, 2, 3, 12, 13, 22).
        @[custom]
        @[java("securityClassificationCode")]
        pub security_classification: Option<SecurityClassification>, // Tag 1
        @[custom]
        @[java("classifyingCountryCodingMethodCode")]
        pub classifying_country_coding_method: Option<ClassifyingCountryCodingMethod>, // Tag 2
        pub classifying_country: Option<String>,                     // Tag 3
        @[custom]
        @[java("objectCountryCodingMethodCode")]
        pub object_country_coding_method: Option<ObjectCountryCodingMethod>, // Tag 12
        pub object_country_codes: Option<String>,                    // Tag 13 (UTF-16)
        pub version: Option<u16>,                                    // Tag 22

        // Context (per-spec semantics: present only when applicable).
        pub sci_shi_info: Option<String>,                  // Tag 4
        pub caveats: Option<String>,                       // Tag 5
        pub releasing_instructions: Option<String>,        // Tag 6
        pub classified_by: Option<String>,                 // Tag 7
        pub derived_from: Option<String>,                  // Tag 8
        pub classification_reason: Option<String>,         // Tag 9
        pub declassification_date: Option<String>,         // Tag 10 ("YYYYMMDD")
        pub classification_marking_system: Option<String>, // Tag 11

        // Optional.
        pub classification_comments: Option<String>, // Tag 14
        pub classifying_country_coding_method_version_date: Option<String>, // Tag 23 ("YYYY-MM-DD")
        pub object_country_coding_method_version_date: Option<String>, // Tag 24 ("YYYY-MM-DD")

        /// Forward-compat: tags outside the LS table preserved verbatim.
        /// Both the lenient and the strict decode populate this per ST 0107.5 §6.
        /// A malformed Tag 13 value is not kept here (re-emitting malformed
        /// UTF-16 would help no consumer); it is recorded in `field_errors`.
        @[custom]
        pub unknown: Vec<OwnedRawField>,

        /// Lenient-mode diagnosis: known tags whose value validation
        /// failed (e.g. Tag 13 UTF-16 decode failure, Tag 22 wrong
        /// length). Strict mode raises these as an error instead of
        /// populating this field. Encode does not consume this field.
        @[custom]
        pub field_errors: Vec<KlvFieldError>,
    }
}

/// Manual `PartialEq` excluding [`SecurityLs::field_errors`]. The
/// `field_errors` vec is a decode-side diagnostic — two LSes that
/// produced identical field values are semantically equal regardless
/// of which fields failed strict-decode validation along the way.
/// Used by round-trip fuzz (decode → encode → decode → assert_eq);
/// `field_errors` is empty on the second decode since encode never
/// emits malformed bytes (e.g. broken UTF-16 on Tag 13).
impl PartialEq for SecurityLs {
    fn eq(&self, other: &Self) -> bool {
        self.security_classification == other.security_classification
            && self.classifying_country_coding_method == other.classifying_country_coding_method
            && self.classifying_country == other.classifying_country
            && self.object_country_coding_method == other.object_country_coding_method
            && self.object_country_codes == other.object_country_codes
            && self.version == other.version
            && self.sci_shi_info == other.sci_shi_info
            && self.caveats == other.caveats
            && self.releasing_instructions == other.releasing_instructions
            && self.classified_by == other.classified_by
            && self.derived_from == other.derived_from
            && self.classification_reason == other.classification_reason
            && self.declassification_date == other.declassification_date
            && self.classification_marking_system == other.classification_marking_system
            && self.classification_comments == other.classification_comments
            && self.classifying_country_coding_method_version_date
                == other.classifying_country_coding_method_version_date
            && self.object_country_coding_method_version_date
                == other.object_country_coding_method_version_date
            && self.unknown == other.unknown
    }
}

// ============================================================================
// UTF-16 BOM helpers (shared by decode.rs and encode.rs)
// ============================================================================

/// Decode RFC 2781 UTF-16 with optional BOM. Default endianness is BE
/// per RFC 2781 §4.3 ("if there is no BOM, the text SHOULD be
/// interpreted as UTF-16BE").
pub(super) fn decode_utf16_bom(bytes: &[u8]) -> Result<String, ()> {
    let (units, big_endian): (Vec<u16>, bool) = if bytes.starts_with(&[0xFE, 0xFF]) {
        (parse_u16_pairs(&bytes[2..], true)?, true)
    } else if bytes.starts_with(&[0xFF, 0xFE]) {
        (parse_u16_pairs(&bytes[2..], false)?, false)
    } else {
        (parse_u16_pairs(bytes, true)?, true) // default BE per RFC 2781 §4.3
    };
    let _ = big_endian; // captured for potential future re-emit; not currently used
    String::from_utf16(&units).map_err(|_| ())
}

fn parse_u16_pairs(bytes: &[u8], big_endian: bool) -> Result<Vec<u16>, ()> {
    if bytes.len() % 2 != 0 {
        return Err(());
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let unit = if big_endian {
            u16::from_be_bytes([chunk[0], chunk[1]])
        } else {
            u16::from_le_bytes([chunk[0], chunk[1]])
        };
        out.push(unit);
    }
    Ok(out)
}

/// Encode a string as RFC 2781 UTF-16 with BE BOM.
///
/// Per spec §3.5 the encoder normalizes to BE; round-trip from any
/// LE-encoded input through decode → encode emits BE.
pub(super) fn encode_utf16_bom(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + s.encode_utf16().count() * 2);
    out.extend_from_slice(&[0xFE, 0xFF]); // BE BOM
    for unit in s.encode_utf16() {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    out
}
