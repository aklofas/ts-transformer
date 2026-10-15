//! ST 0102.12 LS tag schema as data. Decoder and encoder loop this
//! table.
//!
//! Pinned against MISB ST 0102.12 §6.7 Table 2.

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Encoding {
    /// Raw 1-byte typed enum codepoint (Tags 1, 2, 12).
    U8Enum,
    /// Raw 2-byte big-endian unsigned (Tag 22).
    U16Be,
    /// ISO/IEC 646 string (effectively ASCII-7), variable-length.
    /// Most string tags (3, 4, 5, 6, 7, 8, 9, 11, 14).
    Iso646,
    /// ISO/IEC 646 string with fixed length: "YYYYMMDD" (Tag 10) or
    /// "YYYY-MM-DD" (Tags 23, 24). Decoder accepts any length the
    /// `expected_len` permits; strict-mode rejects mismatches.
    FixedAscii { expected_len: usize },
    /// RFC 2781 UTF-16 with optional BOM (Tag 13). Default endianness
    /// is BE per RFC 2781 §4.3.
    Utf16Bom,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TagSpec {
    pub id: u8,
    pub encoding: Encoding,
    /// True if ST 0102.12 §6.7 Table 2 marks this tag as Required.
    pub required: bool,
}

pub(crate) const TAGS: &[TagSpec] = &[
    TagSpec {
        id: 1,
        encoding: Encoding::U8Enum,
        required: true,
    }, // Security Classification
    TagSpec {
        id: 2,
        encoding: Encoding::U8Enum,
        required: true,
    }, // Classifying Country & Releasing Coding Method
    TagSpec {
        id: 3,
        encoding: Encoding::Iso646,
        required: true,
    }, // Classifying Country
    TagSpec {
        id: 4,
        encoding: Encoding::Iso646,
        required: false,
    }, // SCI/SHI Information
    TagSpec {
        id: 5,
        encoding: Encoding::Iso646,
        required: false,
    }, // Caveats
    TagSpec {
        id: 6,
        encoding: Encoding::Iso646,
        required: false,
    }, // Releasing Instructions
    TagSpec {
        id: 7,
        encoding: Encoding::Iso646,
        required: false,
    }, // Classified By
    TagSpec {
        id: 8,
        encoding: Encoding::Iso646,
        required: false,
    }, // Derived From
    TagSpec {
        id: 9,
        encoding: Encoding::Iso646,
        required: false,
    }, // Classification Reason
    TagSpec {
        id: 10,
        encoding: Encoding::FixedAscii { expected_len: 8 },
        required: false,
    }, // Declassification Date YYYYMMDD
    TagSpec {
        id: 11,
        encoding: Encoding::Iso646,
        required: false,
    }, // Classification & Marking System
    TagSpec {
        id: 12,
        encoding: Encoding::U8Enum,
        required: true,
    }, // Object Country Coding Method
    TagSpec {
        id: 13,
        encoding: Encoding::Utf16Bom,
        required: true,
    }, // Object Country Codes
    TagSpec {
        id: 14,
        encoding: Encoding::Iso646,
        required: false,
    }, // Classification Comments
    TagSpec {
        id: 22,
        encoding: Encoding::U16Be,
        required: true,
    }, // Version
    TagSpec {
        id: 23,
        encoding: Encoding::FixedAscii { expected_len: 10 },
        required: false,
    }, // CCM Version Date YYYY-MM-DD
    TagSpec {
        id: 24,
        encoding: Encoding::FixedAscii { expected_len: 10 },
        required: false,
    }, // Object CCM Version Date YYYY-MM-DD
];

/// Required-tag IDs per ST 0102.12 §6.7 Table 2. Used by
/// `decode_strict` to enforce the "required tags present" rule.
pub(crate) const REQUIRED_TAGS: &[u8] = &[1, 2, 3, 12, 13, 22];

pub(crate) fn lookup(tag: u8) -> Option<&'static TagSpec> {
    TAGS.iter().find(|t| t.id == tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_prelude::*;

    #[test]
    fn lookup_returns_known_tags() {
        for tag in [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 22, 23, 24] {
            let spec = lookup(tag).unwrap_or_else(|| panic!("tag {tag} missing from table"));
            assert_eq!(spec.id, tag);
        }
    }

    #[test]
    fn lookup_returns_none_for_unknown_tags() {
        // Tags 15-21 + 25+ are not in the LS table.
        for tag in [0u8, 15, 16, 19, 20, 21, 25, 99, 255] {
            assert!(lookup(tag).is_none(), "tag {tag} unexpectedly in table");
        }
    }

    #[test]
    fn required_tags_match_spec() {
        // ST 0102.12 §6.7 Table 2 marks tags 1, 2, 3, 12, 13, 22
        // as Required. Cross-check the table flag against the
        // dedicated REQUIRED_TAGS list.
        let from_table: Vec<u8> = TAGS.iter().filter(|t| t.required).map(|t| t.id).collect();
        let mut expected = REQUIRED_TAGS.to_vec();
        expected.sort_unstable();
        let mut actual = from_table.clone();
        actual.sort_unstable();
        assert_eq!(actual, expected);
    }

    #[test]
    fn tag_count_matches_spec() {
        // ST 0102.12 §6.7 Table 2 lists 17 tags in the LS.
        assert_eq!(TAGS.len(), 17);
    }
}
