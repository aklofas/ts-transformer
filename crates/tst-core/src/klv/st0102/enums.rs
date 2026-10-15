//! Typed value enums for ST 0102.12 LS tags 1, 2, and 12.
//!
//! Three distinct enums because Tags 2 and 12 use different uint8
//! codepoints for the same logical coding method (e.g. ISO-3166
//! Numeric is 0x05 in Tag 2 but 0x03 in Tag 12 per ST 0102.12 §6.7
//! Table 2). Sharing one enum would require two encode tables anyway
//! for no consumer-side benefit.

/// Tag 1 — Security Classification per ST 0102.12 §6.1.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityClassification {
    Unclassified, // 0x01
    Restricted,   // 0x02
    Confidential, // 0x03
    Secret,       // 0x04
    TopSecret,    // 0x05
    Unknown(u8),
}

/// Tag 2 — Classifying Country and Releasing Instructions Country
/// Coding Method per ST 0102.12 §6.1.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassifyingCountryCodingMethod {
    Iso3166TwoLetter,      // 0x01
    Iso3166ThreeLetter,    // 0x02
    Fips104TwoLetter,      // 0x03
    Fips104FourLetter,     // 0x04
    Iso3166Numeric,        // 0x05
    Stanag1059TwoLetter,   // 0x06
    Stanag1059ThreeLetter, // 0x07
    OmittedValue08,        // 0x08 (reserved/omitted by spec)
    OmittedValue09,        // 0x09 (reserved/omitted by spec)
    Fips104Mixed,          // 0x0A
    Iso3166Mixed,          // 0x0B
    Stanag1059Mixed,       // 0x0C
    GencTwoLetter,         // 0x0D
    GencThreeLetter,       // 0x0E
    GencNumeric,           // 0x0F
    GencMixed,             // 0x10
    Unknown(u8),
}

/// Tag 12 — Object Country Coding Method per ST 0102.12 §6.1.12.
/// Note: codepoints differ from Tag 2; the spec is non-contiguous
/// (jumps to 0x40 for `GencAdminSub`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectCountryCodingMethod {
    Iso3166TwoLetter,      // 0x01
    Iso3166ThreeLetter,    // 0x02
    Iso3166Numeric,        // 0x03 (≠ Tag 2's 0x05)
    Fips104TwoLetter,      // 0x04 (≠ Tag 2's 0x03)
    Fips104FourLetter,     // 0x05 (≠ Tag 2's 0x04)
    Stanag1059TwoLetter,   // 0x06
    Stanag1059ThreeLetter, // 0x07
    OmittedValue08,        // 0x08
    OmittedValue09,        // 0x09
    OmittedValue0A,        // 0x0A
    OmittedValue0B,        // 0x0B
    OmittedValue0C,        // 0x0C
    GencTwoLetter,         // 0x0D
    GencThreeLetter,       // 0x0E
    GencNumeric,           // 0x0F
    GencAdminSub,          // 0x40 (jumps; spec is non-contiguous)
    Unknown(u8),
}

impl SecurityClassification {
    crate::klv::wire_codepoints! {
        from_wire, to_wire, Unknown, "ST 0102.12 §6.1.1";
        0x01 => Unclassified,
        0x02 => Restricted,
        0x03 => Confidential,
        0x04 => Secret,
        0x05 => TopSecret,
    }

    /// True if this codepoint is in the spec's enumerated range
    /// (excluding `Unknown`). Used by strict-mode validation.
    pub fn is_known_codepoint(self) -> bool {
        !matches!(self, Self::Unknown(_))
    }
}

impl ClassifyingCountryCodingMethod {
    crate::klv::wire_codepoints! {
        from_wire, to_wire, Unknown, "ST 0102.12 §6.1.2";
        0x01 => Iso3166TwoLetter,
        0x02 => Iso3166ThreeLetter,
        0x03 => Fips104TwoLetter,
        0x04 => Fips104FourLetter,
        0x05 => Iso3166Numeric,
        0x06 => Stanag1059TwoLetter,
        0x07 => Stanag1059ThreeLetter,
        0x08 => OmittedValue08,
        0x09 => OmittedValue09,
        0x0A => Fips104Mixed,
        0x0B => Iso3166Mixed,
        0x0C => Stanag1059Mixed,
        0x0D => GencTwoLetter,
        0x0E => GencThreeLetter,
        0x0F => GencNumeric,
        0x10 => GencMixed,
    }

    /// True if codepoint is in the enumerated range AND not an
    /// `OmittedValueXX` reserved slot. Strict-mode rejects everything
    /// this returns false for.
    pub fn is_known_codepoint(self) -> bool {
        !matches!(
            self,
            Self::Unknown(_) | Self::OmittedValue08 | Self::OmittedValue09
        )
    }
}

impl ObjectCountryCodingMethod {
    crate::klv::wire_codepoints! {
        from_wire, to_wire, Unknown, "ST 0102.12 §6.1.12";
        0x01 => Iso3166TwoLetter,
        0x02 => Iso3166ThreeLetter,
        0x03 => Iso3166Numeric,
        0x04 => Fips104TwoLetter,
        0x05 => Fips104FourLetter,
        0x06 => Stanag1059TwoLetter,
        0x07 => Stanag1059ThreeLetter,
        0x08 => OmittedValue08,
        0x09 => OmittedValue09,
        0x0A => OmittedValue0A,
        0x0B => OmittedValue0B,
        0x0C => OmittedValue0C,
        0x0D => GencTwoLetter,
        0x0E => GencThreeLetter,
        0x0F => GencNumeric,
        0x40 => GencAdminSub,
    }

    pub fn is_known_codepoint(self) -> bool {
        !matches!(
            self,
            Self::Unknown(_)
                | Self::OmittedValue08
                | Self::OmittedValue09
                | Self::OmittedValue0A
                | Self::OmittedValue0B
                | Self::OmittedValue0C
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_classification_round_trip_all_known() {
        for b in 0x01..=0x05 {
            let v = SecurityClassification::from_wire(b);
            assert_eq!(v.to_wire(), b);
            assert!(v.is_known_codepoint());
        }
    }

    #[test]
    fn security_classification_unknown_preserved() {
        let v = SecurityClassification::from_wire(0xFE);
        assert_eq!(v, SecurityClassification::Unknown(0xFE));
        assert_eq!(v.to_wire(), 0xFE);
        assert!(!v.is_known_codepoint());
    }

    #[test]
    fn classifying_country_coding_method_round_trip_all_codepoints() {
        for b in 0x01..=0x10 {
            let v = ClassifyingCountryCodingMethod::from_wire(b);
            assert_eq!(v.to_wire(), b);
        }
    }

    #[test]
    fn classifying_country_coding_method_omitted_known_false() {
        // 0x08 and 0x09 are spec-reserved "Omitted Value" slots —
        // they exist on the wire but strict-mode rejects them.
        assert!(!ClassifyingCountryCodingMethod::OmittedValue08.is_known_codepoint());
        assert!(!ClassifyingCountryCodingMethod::OmittedValue09.is_known_codepoint());
        // Adjacent codepoints stay valid.
        assert!(ClassifyingCountryCodingMethod::Stanag1059ThreeLetter.is_known_codepoint());
        assert!(ClassifyingCountryCodingMethod::Fips104Mixed.is_known_codepoint());
    }

    #[test]
    fn classifying_country_coding_method_unknown_preserved() {
        let v = ClassifyingCountryCodingMethod::from_wire(0x7F);
        assert_eq!(v, ClassifyingCountryCodingMethod::Unknown(0x7F));
        assert_eq!(v.to_wire(), 0x7F);
    }

    #[test]
    fn object_country_coding_method_round_trip_low_range() {
        for b in 0x01..=0x0F {
            let v = ObjectCountryCodingMethod::from_wire(b);
            assert_eq!(v.to_wire(), b);
        }
    }

    #[test]
    fn object_country_coding_method_round_trip_admin_sub_jump() {
        // The spec is non-contiguous — 0x10..=0x3F are unknown,
        // 0x40 is `GencAdminSub`.
        let v = ObjectCountryCodingMethod::from_wire(0x40);
        assert_eq!(v, ObjectCountryCodingMethod::GencAdminSub);
        assert_eq!(v.to_wire(), 0x40);
        assert!(v.is_known_codepoint());

        // The gap between 0x0F and 0x40 is unknown.
        let v = ObjectCountryCodingMethod::from_wire(0x10);
        assert_eq!(v, ObjectCountryCodingMethod::Unknown(0x10));
        assert!(!v.is_known_codepoint());
    }

    #[test]
    fn object_country_coding_method_omitted_known_false() {
        for omitted in [
            ObjectCountryCodingMethod::OmittedValue08,
            ObjectCountryCodingMethod::OmittedValue09,
            ObjectCountryCodingMethod::OmittedValue0A,
            ObjectCountryCodingMethod::OmittedValue0B,
            ObjectCountryCodingMethod::OmittedValue0C,
        ] {
            assert!(!omitted.is_known_codepoint());
        }
    }

    #[test]
    fn object_country_coding_method_iso3166_numeric_codepoint_differs_from_tag2() {
        // ST 0102.12 §6.7 Table 2: Tag 2 ISO-3166 Numeric is 0x05;
        // Tag 12 ISO-3166 Numeric is 0x03. Verify both encodings.
        assert_eq!(
            ClassifyingCountryCodingMethod::Iso3166Numeric.to_wire(),
            0x05
        );
        assert_eq!(ObjectCountryCodingMethod::Iso3166Numeric.to_wire(), 0x03);
    }
}
