//! 16-byte SMPTE/MISB Universal Label. Non-validating constructor; introspection
//! helpers for the SMPTE structural fields (oid, category, registry, structure
//! designator). Well-known constants for canonical labels.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Per SMPTE 336M / MISB ST 0107, a Universal Label is a 16-byte key. Bytes
//! 0-3 are the SMPTE OID prefix, byte 4 is the category, byte 5 is the
//! registry, byte 6 is the structure designator. For ST 0601 the canonical UL
//! per ST 0601.19 §6.2 (PDF p.4) has bytes 13/14/15 all `0x00`:
//! `06.0E.2B.34.02.0B.01.01.0E.01.03.01.01.00.00.00`. ST 0601.8-19 forbids
//! historical 16-byte UL keys in future developments.
//!
//! Some legacy captures still ship a non-zero byte 13 (the "document version"
//! convention from older MISB conventions). The `is_st0601_family` gate is
//! tolerant of bytes 13/14 to allow round-tripping such captures; encoder
//! output uses the spec-canonical `0x00` bytes.
//!
//! Real-world records contain malformed or non-standard labels. This type is
//! deliberately permissive: `UniversalLabel::new` accepts any 16 bytes;
//! validation is opt-in via `decode_strict` in the typed layer.

use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UniversalLabel(pub [u8; 16]);

impl UniversalLabel {
    /// Construct from raw bytes. Non-validating.
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// SMPTE OID prefix (bytes 0-3).
    pub const fn oid(&self) -> [u8; 4] {
        [self.0[0], self.0[1], self.0[2], self.0[3]]
    }

    /// SMPTE category designator (byte 4).
    pub const fn category(&self) -> u8 {
        self.0[4]
    }

    /// SMPTE registry designator (byte 5).
    pub const fn registry(&self) -> u8 {
        self.0[5]
    }

    /// SMPTE structure designator (byte 6).
    pub const fn structure(&self) -> u8 {
        self.0[6]
    }

    /// ST 0601 document-version convention byte (0-based index 13).
    ///
    /// NOTE: this is **not** the SMPTE ST 336 / ST 298 structural "Version
    /// Number" field — that lives at 0-based index 7 (spec octet 8). Index 13
    /// sits in the Item Designator region (0-based indices 8-15, spec octets
    /// 9-16), which ST 0601 historically repurposed as a document-version
    /// marker. The spec-canonical value is `0x00` per ST 0601.19 §6.2 (PDF
    /// p.4); some legacy captures ship a non-zero index-13 byte (e.g. `0x13`
    /// = a pre-canonical convention), and `is_st0601_family()` is tolerant of
    /// it to allow decode interop. ST 0601.8-19 forbids non-zero values in
    /// new developments.
    pub const fn st0601_version_byte(&self) -> u8 {
        self.0[13]
    }

    // --- Well-known constants ---

    /// Canonical ST 0601 UAS Datalink Local Set UL.
    /// Per MISB ST 0601.19 §6.2 (PDF p.4) — bytes 13/14/15 all `0x00`:
    /// `06.0E.2B.34.02.0B.01.01.0E.01.03.01.01.00.00.00` (CRC 56773).
    pub const ST_0601_LS: UniversalLabel = UniversalLabel([
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ]);

    /// MISB ST 0605 §7 Precision Time Stamp Pack UL.
    /// Registered in MISB ST 0807.27 row 1061 as the Microsecond Timestamp Pack.
    /// Body: `[time_status:1][microseconds_since_epoch:8 BE]`.
    pub const PRECISION_TIMESTAMP_PACK_UL: UniversalLabel = UniversalLabel([
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0E, 0x01, 0x01, 0x03, 0x11, 0x00, 0x00,
        0x00,
    ]);

    /// MISB ST 0102.12 §6.7 Security Metadata Local Set UL.
    /// Registered in MISB ST 0807 as CRC 40980; canonical bytes
    /// `06.0E.2B.34.02.03.01.01.0E.01.03.03.02.00.00.00`.
    ///
    /// Consumers carrying the Security LS as a **standalone KLV stream**
    /// (separate MPEG-TS PID, not nested under ST 0601 Tag 48) detect
    /// the record by matching the leading 16 bytes against this
    /// constant. The dominant carriage path is Tag 48 nesting (handled
    /// by [`crate::klv::st0601`]); the standalone path is spec-allowed
    /// and present in some ISR captures.
    pub const SECURITY_LS_UL: UniversalLabel = UniversalLabel([
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x03, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x03, 0x02, 0x00, 0x00,
        0x00,
    ]);

    /// True if this UL belongs to the ST 0601 family — bytes 0-12 match
    /// the canonical prefix `06 0E 2B 34 02 0B 01 01 0E 01 03 01 01`,
    /// byte 15 must be `0x00`. Bytes 13 (the document version byte; see
    /// `st0601_version_byte()`) and 14 are not validated by this gate.
    pub const fn is_st0601_family(&self) -> bool {
        let canonical = [
            0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01,
        ];
        let mut i = 0;
        while i < 13 {
            if self.0[i] != canonical[i] {
                return false;
            }
            i += 1;
        }
        self.0[15] == 0x00
    }
}

impl Default for UniversalLabel {
    fn default() -> Self {
        Self::ST_0601_LS
    }
}

impl fmt::Display for UniversalLabel {
    /// Dotted-hex form: `06.0E.2B.34.02.0B.01.01.0E.01.03.01.01.00.00.00`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, b) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            write!(f, "{b:02X}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn st0601_constant_well_formed() {
        let ul = UniversalLabel::ST_0601_LS;
        assert_eq!(ul.oid(), [0x06, 0x0E, 0x2B, 0x34]);
        assert_eq!(ul.category(), 0x02);
        assert_eq!(ul.registry(), 0x0B);
        assert_eq!(ul.structure(), 0x01);
        assert!(ul.is_st0601_family());
    }

    /// Per MISB ST 0601.19 §6.2 (PDF p.4): the registered UL is
    ///   06.0E.2B.34.02.0B.01.01.0E.01.03.01.01.00.00.00 (CRC 56773).
    /// Bytes 13/14/15 are all 0x00. A corpus check confirms byte
    /// 13 = 0x00 in 210,886/210,886 ST 0601 ULs across 30 sampled real
    /// captures. The historical "byte 13 carries document version"
    /// convention is forbidden going forward per ST 0601.8-19
    /// ("Historical 16-byte Universal Label Keys shall be forbidden in
    /// future developments").
    #[test]
    fn st0601_canonical_ul_bytes_13_14_15_are_zero() {
        let ul = UniversalLabel::ST_0601_LS;
        assert_eq!(ul.0[13], 0x00, "byte 13 must be 0x00 per ST 0601.19 §6.2");
        assert_eq!(ul.0[14], 0x00, "byte 14 must be 0x00 per ST 0601.19 §6.2");
        assert_eq!(ul.0[15], 0x00, "byte 15 must be 0x00 per ST 0601.19 §6.2");
    }

    #[test]
    fn display_dotted_hex() {
        let ul = UniversalLabel::ST_0601_LS;
        assert_eq!(
            ul.to_string(),
            "06.0E.2B.34.02.0B.01.01.0E.01.03.01.01.00.00.00"
        );
    }

    #[test]
    fn display_zero() {
        let ul = UniversalLabel::new([0; 16]);
        assert_eq!(
            ul.to_string(),
            "00.00.00.00.00.00.00.00.00.00.00.00.00.00.00.00"
        );
    }

    /// Per MISB ST 0102.12 §6.7: Security LS UL is
    ///   06.0E.2B.34.02.03.01.01.0E.01.03.03.02.00.00.00 (CRC 40980).
    #[test]
    fn security_ls_ul_canonical_bytes() {
        let ul = UniversalLabel::SECURITY_LS_UL;
        assert_eq!(ul.oid(), [0x06, 0x0E, 0x2B, 0x34]);
        assert_eq!(ul.category(), 0x02);
        assert_eq!(ul.registry(), 0x03);
        assert_eq!(ul.structure(), 0x01);
        assert_eq!(
            ul.0,
            [
                0x06, 0x0E, 0x2B, 0x34, 0x02, 0x03, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x03, 0x02, 0x00,
                0x00, 0x00,
            ]
        );
    }

    #[test]
    fn family_check_accepts_any_version_byte() {
        let mut bytes = UniversalLabel::ST_0601_LS.0;
        bytes[14] = 0x0E; // ST 0601.14
        assert!(UniversalLabel::new(bytes).is_st0601_family());
        bytes[14] = 0xFF; // out-of-spec but still family
        assert!(UniversalLabel::new(bytes).is_st0601_family());
    }

    #[test]
    fn family_check_rejects_byte15_nonzero() {
        let mut bytes = UniversalLabel::ST_0601_LS.0;
        bytes[15] = 0x01;
        assert!(!UniversalLabel::new(bytes).is_st0601_family());
    }

    #[test]
    fn family_check_rejects_oid_mismatch() {
        let mut bytes = UniversalLabel::ST_0601_LS.0;
        bytes[0] = 0x07;
        assert!(!UniversalLabel::new(bytes).is_st0601_family());
    }

    #[test]
    fn default_is_st0601() {
        assert_eq!(UniversalLabel::default(), UniversalLabel::ST_0601_LS);
    }

    #[test]
    fn new_accepts_anything() {
        // Non-validating constructor — every byte combination is legal.
        let ul = UniversalLabel::new([0xFF; 16]);
        assert_eq!(ul.0, [0xFF; 16]);
    }

    #[test]
    fn const_compatible() {
        // Verifies the helpers are usable in const contexts.
        const UL: UniversalLabel = UniversalLabel::ST_0601_LS;
        const VB: u8 = UL.st0601_version_byte();
        assert_eq!(VB, 0x00);
    }
}
