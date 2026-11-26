//! Program-specific information (PAT, PMT, descriptors).

use crate::mpegts::common::crc32::crc32_mpeg2;
use crate::mpegts::demux::event::PsiSyntaxKind;
use crate::mpegts::descriptors::RawDescriptor;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;
use thiserror::Error;

/// Decoded Program Association Table (ISO/IEC 13818-1 §2.4.4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pat {
    pub transport_stream_id: u16,
    pub version: u8,
    pub current_next_indicator: bool,
    pub programs: Vec<PatEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatEntry {
    pub program_number: u16,
    /// `0x0000` is the network PID; everything else is a PMT PID.
    pub pid: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PsiParseError {
    #[error("section too short ({have} bytes, need {need})")]
    Truncated { have: usize, need: usize },
    #[error("table_id mismatch (got 0x{got:02X}, expected 0x{expected:02X})")]
    TableIdMismatch { got: u8, expected: u8 },
    #[error("CRC32 mismatch (computed 0x{computed:08X}, declared 0x{declared:08X})")]
    CrcMismatch { computed: u32, declared: u32 },
    #[error("section_length {0} exceeds 1021 (the H.222.0 §2.4.4.6 cap)")]
    SectionTooLong(u16),
    #[error("malformed program loop entry at offset {offset}")]
    MalformedProgramEntry { offset: usize },
    #[error("descriptor loop length {declared} exceeds remaining {remaining}")]
    DescriptorLoopOverflow { declared: usize, remaining: usize },
    /// Per ISO/IEC 13818-1 §2.4.4.4, a section with `current_next_indicator=0`
    /// describes a future-staged table not yet in effect. Callers should drop
    /// these silently — processing them would bump version state and dedupe
    /// the real current section that follows. ffmpeg drops them too
    /// (mpegts.c:1759, 2610-2611, 2832, 2969-2970, 2974).
    #[error("section is future-staged (current_next_indicator=0); not yet in effect")]
    FuturePsiSection,
    /// `section_length` is below the structural minimum for the table's
    /// fixed fields + 4-byte CRC trailer. Surfaced via OSS-Fuzz on
    /// inputs with `section_length` 0..=3 (which underflow the CRC slice
    /// at byte index `total_len - 4`). For PAT the minimum is 9, for
    /// PMT the minimum is 13.
    #[error(
        "section_length {section_length} below minimum {min_required} for table_id 0x{table_id:02X}"
    )]
    SectionTooShort {
        table_id: u8,
        section_length: u16,
        min_required: u16,
    },
    /// Per ISO/IEC 13818-1 §2.4.4.5, sections with `last_section_number > 0`
    /// belong to a multi-section table that requires reassembly. The
    /// single-section parsers (`parse_pat`, `parse_pmt`) reject such a
    /// section with this error. The demuxer reassembles a multi-section PAT
    /// itself; a multi-section PMT is dropped and reported as
    /// [`crate::mpegts::demux::event::NonConformantIssue::PsiMultiSectionUnsupported`].
    #[error(
        "multi-section PSI table not supported (table_id=0x{table_id:02X}, last_section_number={last_section_number})"
    )]
    MultiSectionUnsupported {
        table_id: u8,
        last_section_number: u8,
    },
}

/// A single parsed PAT section (ISO/IEC 13818-1 §2.4.4.3), including its
/// program loop. Unlike [`parse_pat`], this does NOT reject multi-section
/// tables — the caller reassembles them.
#[derive(Debug)]
pub(super) struct PatSection {
    pub(super) transport_stream_id: u16,
    pub(super) version: u8,
    pub(super) current_next_indicator: bool,
    pub(super) section_number: u8,
    pub(super) last_section_number: u8,
    pub(super) programs: Vec<PatEntry>,
}

/// Parse one PAT section including its program loop. Performs the full
/// ISO/IEC 13818-1 §2.4.4.3 validation; the multi-section rejection lives
/// in [`parse_pat`].
pub(super) fn parse_pat_section(section: &[u8]) -> Result<PatSection, PsiParseError> {
    // Minimum: 8-byte fixed header + 4-byte CRC32 = 12 bytes; add 4 per program.
    if section.len() < 12 {
        return Err(PsiParseError::Truncated {
            have: section.len(),
            need: 12,
        });
    }
    if section[0] != 0x00 {
        return Err(PsiParseError::TableIdMismatch {
            got: section[0],
            expected: 0x00,
        });
    }
    let section_length = (((section[1] & 0x0F) as u16) << 8) | section[2] as u16;
    if section_length as usize > 1021 {
        return Err(PsiParseError::SectionTooLong(section_length));
    }
    // PAT minimum: 5 fixed bytes (transport_stream_id 2 + flags 1 +
    // section_number 1 + last_section_number 1) + 4 CRC = 9. Without
    // this guard, section_length < 4 underflows `total_len - 4` on
    // the CRC slice below (found by OSS-Fuzz; see oss-fuzz/VERIFICATION.md).
    const PAT_MIN_SECTION_LENGTH: u16 = 9;
    if section_length < PAT_MIN_SECTION_LENGTH {
        return Err(PsiParseError::SectionTooShort {
            table_id: 0x00,
            section_length,
            min_required: PAT_MIN_SECTION_LENGTH,
        });
    }
    let total_len = 3 + section_length as usize;
    if section.len() < total_len {
        return Err(PsiParseError::Truncated {
            have: section.len(),
            need: total_len,
        });
    }
    // CRC over bytes [0 .. total_len - 4] should equal the trailer.
    let computed = crc32_mpeg2(&section[..total_len - 4]);
    let declared = u32::from_be_bytes([
        section[total_len - 4],
        section[total_len - 3],
        section[total_len - 2],
        section[total_len - 1],
    ]);
    if computed != declared {
        return Err(PsiParseError::CrcMismatch { computed, declared });
    }
    let transport_stream_id = u16::from_be_bytes([section[3], section[4]]);
    let version = (section[5] >> 1) & 0x1F;
    let current_next_indicator = (section[5] & 0x01) != 0;
    // Per ISO/IEC 13818-1 §2.4.4.4, sections with current_next=0 are
    // future-staged tables not yet in effect. ffmpeg drops these
    // (mpegts.c:1759). Silently dropping prevents version_number from being
    // bumped and de-duping the real current section that follows.
    if !current_next_indicator {
        return Err(PsiParseError::FuturePsiSection);
    }
    let section_number = section[6];
    let last_section_number = section[7];
    // Program loop runs from byte 8 to byte total_len - 4 (exclusive — that's the CRC).
    let mut programs = Vec::new();
    let mut off = 8;
    while off + 4 <= total_len - 4 {
        let pn = u16::from_be_bytes([section[off], section[off + 1]]);
        let pid = u16::from_be_bytes([section[off + 2] & 0x1F, section[off + 3]]);
        programs.push(PatEntry {
            program_number: pn,
            pid,
        });
        off += 4;
    }
    if off != total_len - 4 {
        return Err(PsiParseError::MalformedProgramEntry { offset: off });
    }
    Ok(PatSection {
        transport_stream_id,
        version,
        current_next_indicator,
        section_number,
        last_section_number,
        programs,
    })
}

/// Parse a fully-assembled PAT section (ISO/IEC 13818-1 §2.4.4.3).
///
/// Bytes must include the 4-byte CRC-32/MPEG-2 trailer; the function
/// verifies the CRC over the section header + payload.
///
/// This function rejects multi-section tables (`last_section_number != 0`);
/// multi-section reassembly is handled internally by the `Demuxer`.
pub fn parse_pat(section: &[u8]) -> Result<Pat, PsiParseError> {
    let s = parse_pat_section(section)?;
    if s.last_section_number != 0 {
        return Err(PsiParseError::MultiSectionUnsupported {
            table_id: 0x00,
            last_section_number: s.last_section_number,
        });
    }
    Ok(Pat {
        transport_stream_id: s.transport_stream_id,
        version: s.version,
        current_next_indicator: s.current_next_indicator,
        programs: s.programs,
    })
}

/// Fixed and reserved PSI syntax fields a PAT or PMT section violates
/// (H.222.0 §2.4.4.3, §2.4.4.8). The demuxer runs the same checks on every
/// CRC-valid section it accepts.
///
/// `section_syntax_indicator` and a non-zero `section_number` on a
/// single-section table are always checked; reserved bits only when
/// `check_reserved_bits` is set. A section whose `table_id` is `0x02` also
/// has its PMT-only reserved bits checked. The reserved bits inside the PAT
/// program loop and the PMT elementary-stream loop entries are not checked
/// by this helper. A section too short for a field
/// skips that field's check, so this never panics.
pub fn psi_section_syntax_issues(section: &[u8], check_reserved_bits: bool) -> Vec<PsiSyntaxKind> {
    let mut issues = Vec::new();
    let byte = |i: usize| section.get(i).copied();
    // section_syntax_indicator (byte 1 bit 0x80) must be 1.
    if byte(1).is_some_and(|b| b & 0x80 == 0) {
        issues.push(PsiSyntaxKind::SectionSyntaxIndicatorUnset);
    }
    // section_number (byte 6) must be 0 only on a single-section table
    // (last_section_number, byte 7, == 0). Multi-section PATs legitimately
    // carry section_number 1..=last_section_number.
    if let (Some(number), Some(0)) = (byte(6), byte(7))
        && number != 0
    {
        issues.push(PsiSyntaxKind::SectionNumberNonZero { observed: number });
    }
    if check_reserved_bits {
        // byte 1 bit 0x40 = reserved-zero (must be 0);
        // byte 1 bits 0x30 = reserved (must be 1);
        // byte 5 bits 0xC0 = reserved (must be 1).
        let mut bad = byte(1).is_some_and(|b| b & 0x40 != 0 || b & 0x30 != 0x30)
            || byte(5).is_some_and(|b| b & 0xC0 != 0xC0);
        if byte(0) == Some(0x02) {
            // PMT additionally: byte 8 bits 0xE0 = reserved (must be 1);
            // byte 10 bits 0xF0 = reserved (must be 1), Table 2-33.
            bad = bad
                || byte(8).is_some_and(|b| b & 0xE0 != 0xE0)
                || byte(10).is_some_and(|b| b & 0xF0 != 0xF0);
        }
        if bad {
            issues.push(PsiSyntaxKind::ReservedBits);
        }
    }
    issues
}

#[cfg(test)]
mod pat_tests {
    use super::*;
    use crate::mpegts::demux::test_support::{build_pat_section, build_pmt_section, recrc};

    #[test]
    fn public_syntax_helper_matches_the_demuxer_checks() {
        let mut s = build_pat_section(1, 0, &[(1, 0x1000)]);
        s[1] &= 0x7F;
        recrc(&mut s);
        assert_eq!(
            psi_section_syntax_issues(&s, false),
            vec![PsiSyntaxKind::SectionSyntaxIndicatorUnset]
        );
    }

    #[test]
    fn pmt_reserved_bits_before_program_info_length_are_all_checked() {
        // Table 2-33: byte 10 = reserved(4) + program_info_length(12) high nibble.
        let mut s = build_pmt_section(1, 0x100, 0, &[], &[(0x1B, 0x101, &[])]);
        s[10] &= !0x80; // clear the top reserved bit; bits 0x30 stay set
        recrc(&mut s);
        assert_eq!(
            psi_section_syntax_issues(&s, true),
            vec![PsiSyntaxKind::ReservedBits]
        );
        assert!(
            psi_section_syntax_issues(&s, false).is_empty(),
            "lenient mode ignores reserved bits"
        );
    }

    #[test]
    fn parses_minimal_pat() {
        let bytes = build_pat_section(0x1234, 7, &[(1, 0x100)]);
        let pat = parse_pat(&bytes).unwrap();
        assert_eq!(pat.transport_stream_id, 0x1234);
        assert_eq!(pat.version, 7);
        assert!(pat.current_next_indicator);
        assert_eq!(
            pat.programs,
            vec![PatEntry {
                program_number: 1,
                pid: 0x100
            }]
        );
    }

    #[test]
    fn parses_two_programs() {
        let bytes = build_pat_section(1, 0, &[(1, 0x100), (2, 0x200)]);
        let pat = parse_pat(&bytes).unwrap();
        assert_eq!(pat.programs.len(), 2);
        assert_eq!(pat.programs[1].pid, 0x200);
    }

    #[test]
    fn rejects_wrong_table_id() {
        let mut bytes = build_pat_section(1, 0, &[(1, 0x100)]);
        bytes[0] = 0x02; // PMT table_id
        let err = parse_pat(&bytes).unwrap_err();
        assert!(matches!(err, PsiParseError::TableIdMismatch { .. }));
    }

    #[test]
    fn rejects_bad_crc() {
        let mut bytes = build_pat_section(1, 0, &[(1, 0x100)]);
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let err = parse_pat(&bytes).unwrap_err();
        assert!(matches!(err, PsiParseError::CrcMismatch { .. }));
    }

    #[test]
    fn rejects_truncated() {
        let bytes = build_pat_section(1, 0, &[(1, 0x100)]);
        let err = parse_pat(&bytes[..5]).unwrap_err();
        assert!(matches!(err, PsiParseError::Truncated { .. }));
    }

    #[test]
    fn parse_pat_rejects_multi_section() {
        // Build a single-program PAT with last_section_number=1 (declaring
        // a 2-section table). Per H.222.0 §2.4.4.5 this requires
        // reassembly; the current demuxer rejects it.
        let mut s = build_pat_section(0x0001, 0, &[(1, 0x100)]);
        // Bytes [6,7] are section_number / last_section_number — fix the
        // last_section_number byte to 1 and recompute the CRC.
        s[7] = 0x01;
        let n = s.len();
        let crc = crc32_mpeg2(&s[..n - 4]);
        s[n - 4..].copy_from_slice(&crc.to_be_bytes());

        let err = parse_pat(&s).unwrap_err();
        match err {
            PsiParseError::MultiSectionUnsupported {
                table_id,
                last_section_number,
            } => {
                assert_eq!(table_id, 0x00);
                assert_eq!(last_section_number, 1);
            }
            other => panic!("expected MultiSectionUnsupported, got {other:?}"),
        }
    }

    /// Bug surfaced by an OSS-Fuzz local smoke run.
    /// section_length = 0 makes total_len = 3, so total_len - 4 underflows
    /// in usize when computing the CRC slice. The fuzz harness panicked
    /// at psi.rs:90 on `&section[..total_len - 4]`. Guard should catch
    /// section_length below the structural minimum (5 fixed bytes + 4 CRC = 9).
    #[test]
    fn parse_pat_rejects_section_length_below_minimum() {
        // Build a 12-byte section with section_length = 0 (the buggy input).
        // section[0] = 0x00 (PAT table_id), section[1..3] encodes section_length=0.
        // section[1] bits: section_syntax_indicator=1, '0', reserved=11, section_length_hi=0000 → 0xB0
        // section[2] = section_length_lo = 0x00
        let mut section = vec![0u8; 12];
        section[0] = 0x00;
        section[1] = 0xB0;
        section[2] = 0x00;
        let err = parse_pat(&section).expect_err("must reject section_length=0");
        assert!(
            matches!(
                err,
                PsiParseError::SectionTooShort {
                    table_id: 0x00,
                    section_length: 0,
                    min_required: 9
                }
            ),
            "expected SectionTooShort, got {err:?}"
        );
    }

    /// Adjacent boundary: section_length = 8 is still below the minimum (9).
    #[test]
    fn parse_pat_rejects_section_length_8() {
        let mut section = vec![0u8; 12];
        section[0] = 0x00;
        section[1] = 0xB0;
        section[2] = 0x08;
        let err = parse_pat(&section).expect_err("must reject section_length=8");
        assert!(matches!(
            err,
            PsiParseError::SectionTooShort {
                table_id: 0x00,
                section_length: 8,
                min_required: 9
            }
        ));
    }
}

/// Decoded Program Map Table (ISO/IEC 13818-1 §2.4.4.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pmt {
    pub program_number: u16,
    pub version: u8,
    pub current_next_indicator: bool,
    pub pcr_pid: u16,
    pub program_descriptors: Vec<RawDescriptor>,
    pub streams: Vec<PmtStream>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmtStream {
    pub stream_type: u8,
    pub elementary_pid: u16,
    pub descriptors: Vec<RawDescriptor>,
}

/// Returns true if the descriptor list contains `registration_descriptor`
/// (tag 0x05) with format identifier `KLVA`.
pub fn has_klva_registration(descs: &[RawDescriptor]) -> bool {
    descs
        .iter()
        .any(|d| d.tag == 0x05 && d.data.starts_with(b"KLVA"))
}

/// Extract a human-readable label from a PMT-stream descriptor list.
///
/// First match wins, in priority order:
///   1. Component descriptor (tag 0x50, ETSI EN 300 468 §6.2.8) — body
///      starting at offset 6 is free text. A leading character-table
///      selector (EN 300 468 Annex A.2, Table A.3) is consumed: 0x15
///      selects UTF-8, and the ISO 8859 selectors (0x01..=0x07,
///      0x09..=0x0B, and 0x10 0x00 with a third byte of 0x01..=0x0B or
///      0x0D..=0x0F, Table A.4) are accepted only when the text is ASCII
///      (no transcoding). The reserved selectors (0x08, the other 0x10
///      values), the multi-byte selectors 0x11..=0x14, any other selector,
///      and text the selector cannot be decoded as fall through to the next
///      source.
///      Text starting at 0x20 or above is in the default table 00 (Figure
///      A.1: ISO/IEC 6937 with the euro sign at 0xA4) and is decoded by it:
///      a non-spacing accent (0xC1..=0xCF) and the letter after it are one
///      precomposed character; a control code (0x80..=0x9F), a code the
///      table leaves empty, or an accent that is last or not followed by a
///      letter it has a precomposed character with makes the text
///      undecodable. One deviation from Annex A: default-table text that is
///      well-formed UTF-8 as a whole is read as UTF-8, so a label written as
///      UTF-8 without the 0x15 selector reads as written. Table 00 text that
///      is also well-formed UTF-8 is misread by that rule: a character coded
///      0xD0..=0xDF followed by one coded 0xA0..=0xBF, one coded 0xE0..=0xEF
///      (not 0xED) followed by two, or one coded 0xF0..=0xF3 followed by
///      three, is read as the UTF-8 character those bytes form (`D3 A0`, "©"
///      and a no-break space, reads as "Ӡ"); a text is misread when every
///      byte of it over 0x7F belongs to such a run.
///   2. Stream Identifier descriptor (tag 0x52, ETSI EN 300 468 §6.2.40) —
///      single `component_tag` byte rendered as `"tag=NN"`.
///   3. Metadata descriptor (tag 0x26) — when present, label is `"KLV"`
///      (the descriptor itself signals metadata streams without carrying
///      free-text labels in the common shapes we see).
///   4. ISO 639 Language descriptor (tag 0x0A) — first 3 bytes are an
///      ISO 639-2 language code.
///
/// Returns `None` if no usable descriptor is found. Truncated /
/// malformed bodies fall through to the next descriptor.
pub fn extract_user_label(descs: &[RawDescriptor]) -> Option<String> {
    // 1. Component descriptor — DVB text after the 6-byte header.
    if let Some(d) = descs.iter().find(|d| d.tag == 0x50)
        && d.data.len() > 6
        && let Some(s) = dvb_text(&d.data[6..])
    {
        let trimmed = s.trim_end_matches('\0').trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    // 2. Stream Identifier — single component_tag byte.
    if let Some(d) = descs.iter().find(|d| d.tag == 0x52)
        && let Some(&t) = d.data.first()
    {
        return Some(format!("tag={t}"));
    }
    // 3. Metadata descriptor — generic "KLV" label.
    if descs.iter().any(|d| d.tag == 0x26) {
        return Some("KLV".to_string());
    }
    // 4. ISO 639 Language — first 3 bytes.
    if let Some(d) = descs.iter().find(|d| d.tag == 0x0A)
        && d.data.len() >= 3
        && let Ok(s) = core::str::from_utf8(&d.data[..3])
    {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    // 5. User-private descriptor (tag 0xFF) — Family B (ARS) corpus
    // shape; reserved per ISO/IEC 13818-1 but used in practice as the
    // de-facto label slot. Best-effort UTF-8.
    if let Some(d) = descs.iter().find(|d| d.tag == 0xFF)
        && let Ok(s) = core::str::from_utf8(&d.data)
    {
        let trimmed = s.trim_end_matches('\0').trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Decodes DVB text per the EN 300 468 Annex A.2 character-table selector:
/// the default table by table 00 (as UTF-8 when the whole text is
/// well-formed UTF-8), 0x15 as UTF-8, the ISO 8859 selectors (Tables A.3
/// and A.4, reserved values excluded) when the text is ASCII. `None` for a
/// reserved selector, the multi-byte selectors 0x11..=0x14, any other
/// selector, or text that cannot be decoded without transcoding.
fn dvb_text(bytes: &[u8]) -> Option<String> {
    let (&first, rest) = bytes.split_first()?;
    let text = match first {
        0x20.. => return default_table_text(bytes),
        0x15 => rest,
        // Table A.4: 0x00 then the ISO/IEC 8859 part; 0x0C and every other
        // value are reserved.
        0x10 => {
            let text = match bytes.get(1..)? {
                [0x00, 0x01..=0x0B | 0x0D..=0x0F, text @ ..] => text,
                _ => return None,
            };
            return text.is_ascii().then(|| ascii_string(text));
        }
        // ISO/IEC 8859-5..15: an ASCII remainder reads the same in each.
        // 0x08 is reserved (Table A.3).
        0x01..=0x07 | 0x09..=0x0B => {
            return rest.is_ascii().then(|| ascii_string(rest));
        }
        // Table A.3: 0x11 ISO/IEC 10646 (two bytes per character), 0x12
        // KS X 1001, 0x13 GB-2312, 0x14 Big5. Multi-byte text is not
        // transcoded; the label comes from the next source.
        0x11..=0x14 => return None,
        _ => return None,
    };
    core::str::from_utf8(text).ok().map(str::to_string)
}

/// A default-table text: UTF-8 when the whole text is well-formed UTF-8 (the
/// deviation [`extract_user_label`] documents), else table 00.
fn default_table_text(bytes: &[u8]) -> Option<String> {
    match core::str::from_utf8(bytes) {
        Ok(text) => Some(text.to_string()),
        Err(_) => table_00_text(bytes),
    }
}

/// Decodes `bytes` by EN 300 468 table 00 (Figure A.1): a byte below 0x80 as
/// ASCII, a non-spacing accent and the letter after it as the precomposed
/// character, any other byte by `TABLE_00_HIGH`. `None` for a control code
/// (0x80..=0x9F), a code the table leaves empty, or an accent that is last or
/// is not followed by a letter `TABLE_00_ACCENTED` pairs it with.
fn table_00_text(bytes: &[u8]) -> Option<String> {
    let mut text = String::with_capacity(bytes.len());
    let mut bytes = bytes.iter().copied();
    while let Some(b) = bytes.next() {
        let c = match b {
            0x00..=0x7F => char::from(b),
            0x80..=0x9F => return None,
            // The accent comes first and marks the letter after it.
            0xC1..=0xC8 | 0xCA | 0xCB | 0xCD..=0xCF => {
                let letter = bytes.next()?;
                let &(_, letters, marked) = TABLE_00_ACCENTED.iter().find(|row| row.0 == b)?;
                let i = letters.bytes().position(|l| l == letter)?;
                marked.chars().nth(i)?
            }
            0xA0..=0xFF => match TABLE_00_HIGH[usize::from(b - 0xA0)] {
                '\0' => return None,
                c => c,
            },
        };
        text.push(c);
    }
    Some(text)
}

/// EN 300 468 Figure A.1, table 00 (ISO/IEC 6937 with the euro sign at
/// 0xA4): the character of each byte 0xA0..=0xFF, `'\0'` where the figure
/// has none. Row 0xC0 holds the non-spacing accents, which never stand alone
/// (see `TABLE_00_ACCENTED`).
const TABLE_00_HIGH: [char; 96] = [
    // 0xA0
    '\u{00A0}', '\u{00A1}', '\u{00A2}', '\u{00A3}', '\u{20AC}', '\u{00A5}', '\0', '\u{00A7}',
    '\u{00A4}', '\u{2018}', '\u{201C}', '\u{00AB}', '\u{2190}', '\u{2191}', '\u{2192}', '\u{2193}',
    // 0xB0
    '\u{00B0}', '\u{00B1}', '\u{00B2}', '\u{00B3}', '\u{00D7}', '\u{00B5}', '\u{00B6}', '\u{00B7}',
    '\u{00F7}', '\u{2019}', '\u{201D}', '\u{00BB}', '\u{00BC}', '\u{00BD}', '\u{00BE}', '\u{00BF}',
    // 0xC0
    '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0', '\0',
    // 0xD0
    '\u{2015}', '\u{00B9}', '\u{00AE}', '\u{00A9}', '\u{2122}', '\u{266A}', '\u{00AC}', '\u{00A6}',
    '\0', '\0', '\0', '\0', '\u{215B}', '\u{215C}', '\u{215D}', '\u{215E}',
    // 0xE0
    '\u{2126}', '\u{00C6}', '\u{0110}', '\u{00AA}', '\u{0126}', '\0', '\u{0132}', '\u{013F}',
    '\u{0141}', '\u{00D8}', '\u{0152}', '\u{00BA}', '\u{00DE}', '\u{0166}', '\u{014A}', '\u{0149}',
    // 0xF0
    '\u{0138}', '\u{00E6}', '\u{0111}', '\u{00F0}', '\u{0127}', '\u{0131}', '\u{0133}', '\u{0140}',
    '\u{0142}', '\u{00F8}', '\u{0153}', '\u{00DF}', '\u{00FE}', '\u{0167}', '\u{014B}', '\u{00AD}',
];

/// Table 00's non-spacing accents (row 0xC0 of Figure A.1): each accent byte
/// with the ASCII letters that have a precomposed Unicode character for that
/// accent, and those characters in the same order.
const TABLE_00_ACCENTED: [(u8, &str, &str); 13] = [
    // grave accent
    (0xC1, "AEINOUWYaeinouwy", "ÀÈÌǸÒÙẀỲàèìǹòùẁỳ"),
    // acute accent
    (
        0xC2,
        "ACEGIKLMNOPRSUWYZacegiklmnoprsuwyz",
        "ÁĆÉǴÍḰĹḾŃÓṔŔŚÚẂÝŹáćéǵíḱĺḿńóṕŕśúẃýź",
    ),
    // circumflex accent
    (
        0xC3,
        "ACEGHIJOSUWYZaceghijosuwyz",
        "ÂĈÊĜĤÎĴÔŜÛŴŶẐâĉêĝĥîĵôŝûŵŷẑ",
    ),
    // tilde
    (0xC4, "AEINOUVYaeinouvy", "ÃẼĨÑÕŨṼỸãẽĩñõũṽỹ"),
    // macron
    (0xC5, "AEGIOUYaegiouy", "ĀĒḠĪŌŪȲāēḡīōūȳ"),
    // breve
    (0xC6, "AEGIOUaegiou", "ĂĔĞĬŎŬăĕğĭŏŭ"),
    // dot above
    (
        0xC7,
        "ABCDEFGHIMNOPRSTWXYZabcdefghmnoprstwxyz",
        "ȦḂĊḊĖḞĠḢİṀṄȮṖṘṠṪẆẊẎŻȧḃċḋėḟġḣṁṅȯṗṙṡṫẇẋẏż",
    ),
    // diaeresis
    (0xC8, "AEHIOUWXYaehiotuwxy", "ÄËḦÏÖÜẄẌŸäëḧïöẗüẅẍÿ"),
    // ring above
    (0xCA, "AUauwy", "ÅŮåůẘẙ"),
    // cedilla
    (0xCB, "CDEGHKLNRSTcdeghklnrst", "ÇḐȨĢḨĶĻŅŖŞŢçḑȩģḩķļņŗşţ"),
    // double acute accent
    (0xCD, "OUou", "ŐŰőű"),
    // ogonek
    (0xCE, "AEIOUaeiou", "ĄĘĮǪŲąęįǫų"),
    // caron
    (
        0xCF,
        "ACDEGHIKLNORSTUZacdeghijklnorstuz",
        "ǍČĎĚǦȞǏǨĽŇǑŘŠŤǓŽǎčďěǧȟǐǰǩľňǒřšťǔž",
    ),
];

fn ascii_string(ascii: &[u8]) -> String {
    ascii.iter().map(|&b| char::from(b)).collect()
}

/// Maps PMT `stream_type` byte → typed audio codec. Returns `None` for
/// unrecognized stream_types; caller routes those to `StreamKind::Unknown(_)`
/// or maps them via `DemuxerConfig::treat_as`.
pub(crate) fn classify_audio_stream_type(stream_type: u8) -> Option<crate::codec::AudioCodec> {
    use crate::codec::AudioCodec;
    match stream_type {
        0x03 | 0x04 => Some(AudioCodec::Mp2),
        0x0F => Some(AudioCodec::Aac),
        0x11 => Some(AudioCodec::AacLatm),
        0x81 => Some(AudioCodec::Ac3),
        _ => None,
    }
}

/// The fixed fields of a `metadata_descriptor` body (H.222.0 §2.6.60,
/// Table 2-89) that classification and link readout need.
pub(super) struct MetadataDescriptorFields {
    /// `metadata_format`.
    pub(super) format: u8,
    /// `metadata_format_identifier`, present when `metadata_format` is 0xFF.
    pub(super) format_identifier: Option<[u8; 4]>,
    /// Bytes before the first `private_data_byte`.
    pub(super) fixed_len: usize,
    /// The flags byte's four reserved bits read `1111`.
    pub(super) flags_reserved_ok: bool,
}

/// Parse a `metadata_descriptor` body (the bytes after tag + length) up to
/// its `private_data_byte` loop, per H.222.0 §2.6.60, Table 2-89. `None`
/// when the body is shorter than the fixed fields it declares.
pub(super) fn metadata_descriptor_fields(body: &[u8]) -> Option<MetadataDescriptorFields> {
    // metadata_application_format (16), + identifier (32) when 0xFFFF.
    let app_format = u16::from_be_bytes([*body.first()?, *body.get(1)?]);
    let mut pos = if app_format == 0xFFFF { 6 } else { 2 };
    // metadata_format (8), + metadata_format_identifier (32) when 0xFF.
    let format = *body.get(pos)?;
    pos += 1;
    let format_identifier = if format == 0xFF {
        let id: [u8; 4] = body.get(pos..pos + 4)?.try_into().ok()?;
        pos += 4;
        Some(id)
    } else {
        None
    };
    // metadata_service_id (8).
    pos += 1;
    // decoder_config_flags (3) + DSM-CC_flag (1) + reserved (4).
    let flags = *body.get(pos)?;
    pos += 1;
    if flags & 0x10 != 0 {
        // service_identification_length (8) + service_identification_record.
        pos += 1 + usize::from(*body.get(pos)?);
    }
    match flags >> 5 {
        // decoder_config_length (8) + decoder_config_byte, or
        // dec_config_identification_record_length (8) + its record.
        0b001 | 0b011 => pos += 1 + usize::from(*body.get(pos)?),
        // decoder_config_metadata_service_id (8).
        0b100 => pos += 1,
        // reserved_data_length (8) + reserved bytes.
        0b101 | 0b110 => pos += 1 + usize::from(*body.get(pos)?),
        _ => {}
    }
    if body.len() < pos {
        return None;
    }
    Some(MetadataDescriptorFields {
        format,
        format_identifier,
        fixed_len: pos,
        flags_reserved_ok: flags & 0x0F == 0x0F,
    })
}

/// Returns the linked video PID a `metadata_descriptor` (tag 0x26) declares,
/// else `None`. H.222.0 §2.6.60 defines no linked-PID field (the trailing
/// bytes are `private_data_byte`), and neither does ST 1402, so any readout
/// is a convention. Two paths:
///
/// - **Well-formed descriptor**: the body parses through its fixed fields
///   (Table 2-89) and the flags byte's reserved bits read `1111`. A link is
///   read only from the first two `private_data_byte`s; a descriptor with no
///   private data declares no link. The muxer's own KLVA descriptor ends
///   with `metadata_service_id` + flags, which this path never reads as a
///   PID.
/// - **Anything else** (truncated, or reserved bits not `1111`): the
///   lenient readout many encoders rely on: when the body is at least 5
///   bytes, its trailing 2 bytes are the link if they fall in
///   0x0010..=0x1FFE.
///
/// Accepted limitation: a 5-byte body whose trailing PID has a low nibble
/// of 0xF, with DSM-CC_flag clear and decoder_config_flags 000, 010 or 111
/// (a link to 0x010F, say), parses as `metadata_service_id` + flags and
/// loses its link; it cannot be told apart from a descriptor whose trailing
/// bytes really are those fields.
///
/// The demuxer surfaces a link as `LinkSource::Declared` only when it names
/// a video PID of the same PMT; consumers calling this directly should
/// apply the same check when strict pairing matters.
pub fn extract_metadata_link(descs: &[RawDescriptor]) -> Option<u16> {
    let d = descs.iter().find(|d| d.tag == 0x26)?;
    let pid_in_range = |v: u16| (0x0010..=0x1FFE).contains(&v).then_some(v);
    match metadata_descriptor_fields(&d.data) {
        Some(fields) if fields.flags_reserved_ok => {
            let private = d.data.get(fields.fixed_len..fields.fixed_len + 2)?;
            pid_in_range(u16::from_be_bytes([private[0], private[1]]))
        }
        _ if d.data.len() >= 5 => pid_in_range(u16::from_be_bytes([
            d.data[d.data.len() - 2],
            d.data[d.data.len() - 1],
        ])),
        _ => None,
    }
}

/// Walk a descriptor loop, returning the parsed list. Returns
/// `DescriptorLoopOverflow` if the declared loop length doesn't match
/// what's actually present.
pub fn walk_descriptors(buf: &[u8]) -> Result<Vec<RawDescriptor>, PsiParseError> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        if i + 2 > buf.len() {
            return Err(PsiParseError::DescriptorLoopOverflow {
                declared: i + 2,
                remaining: buf.len() - i,
            });
        }
        let tag = buf[i];
        let len = buf[i + 1] as usize;
        let start = i + 2;
        let end = start + len;
        if end > buf.len() {
            return Err(PsiParseError::DescriptorLoopOverflow {
                declared: end,
                remaining: buf.len(),
            });
        }
        out.push(RawDescriptor {
            tag,
            data: buf[start..end].to_vec(),
        });
        i = end;
    }
    Ok(out)
}

/// Parse a fully-assembled PMT section (ISO/IEC 13818-1 §2.4.4.8).
pub fn parse_pmt(section: &[u8]) -> Result<Pmt, PsiParseError> {
    if section.len() < 16 {
        return Err(PsiParseError::Truncated {
            have: section.len(),
            need: 16,
        });
    }
    if section[0] != 0x02 {
        return Err(PsiParseError::TableIdMismatch {
            got: section[0],
            expected: 0x02,
        });
    }
    let section_length = (((section[1] & 0x0F) as u16) << 8) | section[2] as u16;
    if section_length as usize > 1021 {
        return Err(PsiParseError::SectionTooLong(section_length));
    }
    // PMT minimum: 9 fixed bytes (program_number 2 + flags 1 +
    // section_number 1 + last_section_number 1 + pcr_pid 2 +
    // program_info_length 2) + 4 CRC = 13. Without this guard,
    // section_length < 4 underflows `total_len - 4` on the CRC slice
    // below (the same guard as parse_pat's).
    const PMT_MIN_SECTION_LENGTH: u16 = 13;
    if section_length < PMT_MIN_SECTION_LENGTH {
        return Err(PsiParseError::SectionTooShort {
            table_id: 0x02,
            section_length,
            min_required: PMT_MIN_SECTION_LENGTH,
        });
    }
    let total_len = 3 + section_length as usize;
    if section.len() < total_len {
        return Err(PsiParseError::Truncated {
            have: section.len(),
            need: total_len,
        });
    }
    let computed = crc32_mpeg2(&section[..total_len - 4]);
    let declared = u32::from_be_bytes([
        section[total_len - 4],
        section[total_len - 3],
        section[total_len - 2],
        section[total_len - 1],
    ]);
    if computed != declared {
        return Err(PsiParseError::CrcMismatch { computed, declared });
    }
    let program_number = u16::from_be_bytes([section[3], section[4]]);
    let version = (section[5] >> 1) & 0x1F;
    let current_next_indicator = (section[5] & 0x01) != 0;
    // Per ISO/IEC 13818-1 §2.4.4.4, sections with current_next=0 are
    // future-staged tables not yet in effect. ffmpeg drops these
    // (mpegts.c:1759). Silently dropping prevents version_number from being
    // bumped and de-duping the real current section that follows.
    if !current_next_indicator {
        return Err(PsiParseError::FuturePsiSection);
    }
    // Per ISO/IEC 13818-1 §2.4.4.5 — see parse_pat for rationale.
    let last_section_number = section[7];
    if last_section_number != 0 {
        return Err(PsiParseError::MultiSectionUnsupported {
            table_id: 0x02,
            last_section_number,
        });
    }
    let pcr_pid = u16::from_be_bytes([section[8] & 0x1F, section[9]]);
    let program_info_length = (((section[10] & 0x0F) as usize) << 8) | section[11] as usize;
    let pi_start = 12;
    let pi_end = pi_start + program_info_length;
    if pi_end > total_len - 4 {
        return Err(PsiParseError::DescriptorLoopOverflow {
            declared: pi_end,
            remaining: total_len - 4,
        });
    }
    let program_descriptors = walk_descriptors(&section[pi_start..pi_end])?;
    let mut streams = Vec::new();
    let mut off = pi_end;
    while off + 5 <= total_len - 4 {
        let stream_type = section[off];
        let elementary_pid = u16::from_be_bytes([section[off + 1] & 0x1F, section[off + 2]]);
        let es_info_length =
            (((section[off + 3] & 0x0F) as usize) << 8) | section[off + 4] as usize;
        let desc_start = off + 5;
        let desc_end = desc_start + es_info_length;
        if desc_end > total_len - 4 {
            return Err(PsiParseError::DescriptorLoopOverflow {
                declared: desc_end,
                remaining: total_len - 4,
            });
        }
        let descriptors = walk_descriptors(&section[desc_start..desc_end])?;
        streams.push(PmtStream {
            stream_type,
            elementary_pid,
            descriptors,
        });
        off = desc_end;
    }
    if off != total_len - 4 {
        return Err(PsiParseError::MalformedProgramEntry { offset: off });
    }
    Ok(Pmt {
        program_number,
        version,
        current_next_indicator,
        pcr_pid,
        program_descriptors,
        streams,
    })
}

#[cfg(test)]
mod pmt_tests {
    use super::*;

    use crate::mpegts::demux::test_support::build_pmt_section as build_pmt_section_with;

    /// `descriptors` serialised as `tag, length, data` triples.
    fn descriptor_bytes(descriptors: &[RawDescriptor]) -> Vec<u8> {
        let mut out = Vec::new();
        for d in descriptors {
            out.push(d.tag);
            out.push(d.data.len() as u8);
            out.extend_from_slice(&d.data);
        }
        out
    }

    fn build_pmt_section(
        program_number: u16,
        version: u8,
        pcr_pid: u16,
        program_descs: &[RawDescriptor],
        streams: &[PmtStream],
    ) -> Vec<u8> {
        let es: Vec<(u8, u16, Vec<u8>)> = streams
            .iter()
            .map(|s| {
                (
                    s.stream_type,
                    s.elementary_pid,
                    descriptor_bytes(&s.descriptors),
                )
            })
            .collect();
        let es: Vec<(u8, u16, &[u8])> = es.iter().map(|(t, p, d)| (*t, *p, d.as_slice())).collect();
        build_pmt_section_with(
            program_number,
            pcr_pid,
            version,
            &descriptor_bytes(program_descs),
            &es,
        )
    }

    fn klva_descriptor() -> RawDescriptor {
        RawDescriptor {
            tag: 0x05,
            data: b"KLVA".to_vec(),
        }
    }

    #[test]
    fn parses_minimal_pmt_video_only() {
        let bytes = build_pmt_section(
            1,
            0,
            0x100,
            &[],
            &[PmtStream {
                stream_type: 0x1B,
                elementary_pid: 0x100,
                descriptors: vec![],
            }],
        );
        let pmt = parse_pmt(&bytes).unwrap();
        assert_eq!(pmt.program_number, 1);
        assert_eq!(pmt.pcr_pid, 0x100);
        assert_eq!(pmt.streams.len(), 1);
        assert_eq!(pmt.streams[0].stream_type, 0x1B);
    }

    #[test]
    fn parses_video_plus_klva() {
        let bytes = build_pmt_section(
            1,
            0,
            0x100,
            &[],
            &[
                PmtStream {
                    stream_type: 0x1B,
                    elementary_pid: 0x100,
                    descriptors: vec![],
                },
                PmtStream {
                    stream_type: 0x06,
                    elementary_pid: 0x101,
                    descriptors: vec![klva_descriptor()],
                },
            ],
        );
        let pmt = parse_pmt(&bytes).unwrap();
        assert!(has_klva_registration(&pmt.streams[1].descriptors));
        assert!(!has_klva_registration(&pmt.streams[0].descriptors));
    }

    #[test]
    fn extracts_metadata_link_when_descriptor_present() {
        // metadata_descriptor (tag 0x26) with trailing 2 bytes 0x0100 (PID 256).
        let metadata_desc = RawDescriptor {
            tag: 0x26,
            data: vec![0x01, 0x00, 0xFF, 0x01, 0x00],
        };
        let bytes = build_pmt_section(
            1,
            0,
            0x100,
            &[],
            &[PmtStream {
                stream_type: 0x15,
                elementary_pid: 0x101,
                descriptors: vec![metadata_desc],
            }],
        );
        let pmt = parse_pmt(&bytes).unwrap();
        assert_eq!(
            extract_metadata_link(&pmt.streams[0].descriptors),
            Some(0x0100)
        );
    }

    /// The muxer's own canonical descriptor: its trailing two bytes are
    /// `metadata_service_id` + the flags byte (H.222.0 §2.6.60, Table 2-89),
    /// not a linked PID.
    #[test]
    fn canonical_klva_metadata_descriptor_declares_no_link() {
        use crate::mpegts::descriptors::metadata_klva;
        for service_id in [0u8, 1, 0x1F, 0xFE] {
            let d = metadata_klva(service_id);
            assert_eq!(
                extract_metadata_link(&[RawDescriptor {
                    tag: 0x26,
                    data: d[2..].to_vec(),
                }]),
                None,
                "{service_id}"
            );
        }
    }

    /// decoder_config_flags '101' carries reserved_data_length + reserved
    /// bytes (H.222.0 §2.6.60, Table 2-89); they are fixed fields, not a link.
    #[test]
    fn reserved_decoder_config_bytes_are_not_read_as_a_link() {
        let data = vec![
            0x01, 0x00, 0xFF, b'K', b'L', b'V', b'A', 0x00, 0xAF, 0x01, 0x00,
        ];
        assert_eq!(
            extract_metadata_link(&[RawDescriptor { tag: 0x26, data }]),
            None
        );
        // A missing reserved_data_length byte is a truncated descriptor.
        assert!(
            metadata_descriptor_fields(&[0x01, 0x00, 0xFF, b'K', b'L', b'V', b'A', 0x00, 0xCF])
                .is_none()
        );
    }

    /// private_data_byte after a well-formed descriptor's fixed fields still
    /// carries a declared link.
    #[test]
    fn private_data_after_a_well_formed_descriptor_still_carries_a_link() {
        use crate::mpegts::descriptors::metadata_klva;
        let mut data = metadata_klva(0)[2..].to_vec();
        data.extend_from_slice(&[0x01, 0x00]);
        assert_eq!(
            extract_metadata_link(&[RawDescriptor { tag: 0x26, data }]),
            Some(0x0100)
        );
    }

    #[test]
    fn rejects_wrong_table_id_for_pmt() {
        let mut bytes = build_pmt_section(1, 0, 0x100, &[], &[]);
        bytes[0] = 0x00;
        let err = parse_pmt(&bytes).unwrap_err();
        assert!(matches!(err, PsiParseError::TableIdMismatch { .. }));
    }

    #[test]
    fn walk_descriptors_handles_two_descriptors() {
        let buf = vec![0x05, 0x04, b'K', b'L', b'V', b'A', 0x26, 0x02, 0x00, 0x00];
        let descs = walk_descriptors(&buf).unwrap();
        assert_eq!(descs.len(), 2);
        assert_eq!(descs[0].tag, 0x05);
        assert_eq!(descs[1].data.len(), 2);
    }

    #[test]
    fn walk_descriptors_rejects_overflow() {
        // tag=5, len=10 declared but only 4 bytes follow.
        let buf = vec![0x05, 0x0A, b'K', b'L', b'V', b'A'];
        assert!(walk_descriptors(&buf).is_err());
    }

    #[test]
    fn parse_pmt_rejects_multi_section() {
        // Build a minimal PMT with last_section_number=1. Per H.222.0
        // §2.4.4.5 this requires reassembly; the demuxer rejects it.
        let mut s = build_pmt_section(1, 0, 0x100, &[], &[]);
        s[7] = 0x01;
        let n = s.len();
        let crc = crc32_mpeg2(&s[..n - 4]);
        s[n - 4..].copy_from_slice(&crc.to_be_bytes());

        let err = parse_pmt(&s).unwrap_err();
        match err {
            PsiParseError::MultiSectionUnsupported {
                table_id,
                last_section_number,
            } => {
                assert_eq!(table_id, 0x02);
                assert_eq!(last_section_number, 1);
            }
            other => panic!("expected MultiSectionUnsupported, got {other:?}"),
        }
    }

    /// Mirror of the PAT section_length = 0 test, for parse_pmt. section_length = 0
    /// makes total_len = 3, which underflows the CRC slice at psi.rs's
    /// parse_pmt CRC slice. PMT minimum section_length: 9 fixed bytes
    /// (program_number 2 + flags 1 + section_number 1 + last_section_number 1 +
    /// pcr_pid 2 + program_info_length 2) + 4 CRC = 13.
    #[test]
    fn parse_pmt_rejects_section_length_below_minimum() {
        // 16-byte section (PMT requires section.len() >= 16 to start).
        // section[0] = 0x02 (PMT table_id), section[1..3] encodes section_length=0.
        let mut section = vec![0u8; 16];
        section[0] = 0x02;
        section[1] = 0xB0;
        section[2] = 0x00;
        let err = parse_pmt(&section).expect_err("must reject section_length=0");
        assert!(
            matches!(
                err,
                PsiParseError::SectionTooShort {
                    table_id: 0x02,
                    section_length: 0,
                    min_required: 13
                }
            ),
            "expected SectionTooShort, got {err:?}"
        );
    }

    /// Adjacent boundary: section_length = 12 is still below the minimum (13).
    #[test]
    fn parse_pmt_rejects_section_length_12() {
        let mut section = vec![0u8; 16];
        section[0] = 0x02;
        section[1] = 0xB0;
        section[2] = 0x0C;
        let err = parse_pmt(&section).expect_err("must reject section_length=12");
        assert!(matches!(
            err,
            PsiParseError::SectionTooShort {
                table_id: 0x02,
                section_length: 12,
                min_required: 13
            }
        ));
    }
}

#[cfg(test)]
mod label_tests {
    use super::*;

    #[test]
    fn extract_user_label_picks_component_descriptor() {
        // Component descriptor (tag 0x50): 4 bytes header + UTF-8 text body.
        // Body shape: stream_content(4) + component_type(8) + component_tag(8) + ISO_639(24) + text...
        // We accept the trailing text as the label.
        let descs = vec![RawDescriptor {
            tag: 0x50,
            data: vec![
                0x09, 0x07, // stream_content + component_type
                0x01, // component_tag
                b'e', b'n', b'g', // ISO 639 language
                b'E', b'O', b' ', b'1', b'0', b'8', b'0', b'p',
            ],
        }];
        assert_eq!(extract_user_label(&descs).as_deref(), Some("EO 1080p"));
    }

    #[test]
    fn extract_user_label_falls_back_to_iso639() {
        // ISO 639 Language descriptor (tag 0x0A): 3-byte language code + 1 audio_type
        // We take the language code as the label when nothing better is present.
        let descs = vec![RawDescriptor {
            tag: 0x0A,
            data: vec![b'e', b'n', b'g', 0x00],
        }];
        assert_eq!(extract_user_label(&descs).as_deref(), Some("eng"));
    }

    fn component_with_text(text: &[u8]) -> Vec<RawDescriptor> {
        let mut body = vec![0xF1, 0x01, 0x00, b'e', b'n', b'g'];
        body.extend_from_slice(text);
        vec![RawDescriptor {
            tag: 0x50,
            data: body,
        }]
    }

    /// EN 300 468 Annex A.2: a first byte of 0x15 selects UTF-8 for the rest.
    #[test]
    fn component_text_with_the_utf8_selector_is_decoded_without_it() {
        let mut text = vec![0x15];
        text.extend_from_slice("Kanal ü".as_bytes());
        assert_eq!(
            extract_user_label(&component_with_text(&text)).as_deref(),
            Some("Kanal ü")
        );
    }

    #[test]
    fn component_text_with_an_iso_8859_selector_and_ascii_text_is_decoded() {
        assert_eq!(
            extract_user_label(&component_with_text(&[0x10, 0x00, 0x02, b'a', b'b', b'c']))
                .as_deref(),
            Some("abc")
        );
        assert_eq!(
            extract_user_label(&component_with_text(&[0x05, b'a', b'b', b'c'])).as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn component_text_in_the_default_table_is_unchanged() {
        assert_eq!(
            extract_user_label(&component_with_text(b"EO Camera")).as_deref(),
            Some("EO Camera")
        );
    }

    /// An 8859 remainder that is not ASCII is not transcoded, and a truncated
    /// 0x10 selector or a reserved control value yields no text, so the label
    /// falls through to the next source.
    #[test]
    fn component_text_that_cannot_be_decoded_falls_through() {
        let with_lang = |text: &[u8]| {
            let mut descs = component_with_text(text);
            descs.push(RawDescriptor {
                tag: 0x0A,
                data: vec![b'e', b'n', b'g', 0x00],
            });
            descs
        };
        for text in [
            &[0x05, 0xFC, b'x'][..],
            &[0x10, 0x00][..],
            &[0x10][..],
            &[0x1F, b'a'][..],
            &[0x1E, b'a'][..],
        ] {
            assert_eq!(extract_user_label(&with_lang(text)).as_deref(), Some("eng"));
        }
    }

    /// EN 300 468 Table A.3: 0x11 selects ISO/IEC 10646 (two bytes per
    /// character). Its bytes can all be ASCII without being ASCII text, so
    /// the label comes from the next source rather than as "\0a\0b".
    #[test]
    fn component_text_with_a_two_byte_selector_falls_through() {
        for selector in 0x11..=0x14 {
            let mut descs = component_with_text(&[selector, 0x00, b'a', 0x00, b'b']);
            descs.push(RawDescriptor {
                tag: 0x0A,
                data: vec![b'e', b'n', b'g', 0x00],
            });
            assert_eq!(
                extract_user_label(&descs).as_deref(),
                Some("eng"),
                "selector 0x{selector:02X}"
            );
        }
    }

    /// The label of a component descriptor with `text`, followed by an ISO 639
    /// descriptor: "eng" means the text was not decoded.
    fn label_or_next_source(text: &[u8]) -> Option<String> {
        let mut descs = component_with_text(text);
        descs.push(RawDescriptor {
            tag: 0x0A,
            data: vec![b'e', b'n', b'g', 0x00],
        });
        extract_user_label(&descs)
    }

    /// EN 300 468 Table A.3: 0x08 is reserved (it was ISO/IEC 8859-12, which
    /// was never published).
    #[test]
    fn the_reserved_selector_0x08_is_not_decoded() {
        assert_eq!(
            label_or_next_source(&[0x08, b'A', b'B']).as_deref(),
            Some("eng")
        );
    }

    /// EN 300 468 Table A.4: after 0x10 the second byte is 0x00 and the third
    /// names ISO/IEC 8859-1..=11 or 13..=15; 0x00, 0x0C, 0x10 and above, and
    /// any second byte but 0x00 are reserved.
    #[test]
    fn a_0x10_selector_with_a_reserved_table_is_not_decoded() {
        for (second, third) in [
            (0x00, 0x0C),
            (0x00, 0x00),
            (0x12, 0x34),
            (0x00, 0x10),
            (0x01, 0x01),
            (0xFF, 0x05),
        ] {
            assert_eq!(
                label_or_next_source(&[0x10, second, third, b'A', b'B']).as_deref(),
                Some("eng"),
                "10 {second:02X} {third:02X}"
            );
        }
    }

    #[test]
    fn a_0x10_selector_with_a_listed_table_is_decoded() {
        for third in (0x01..=0x0B).chain(0x0D..=0x0F) {
            assert_eq!(
                label_or_next_source(&[0x10, 0x00, third, b'A', b'B']).as_deref(),
                Some("AB"),
                "10 00 {third:02X}"
            );
        }
    }

    /// A selector with nothing behind it is an empty text: the label comes
    /// from the next source.
    #[test]
    fn a_selector_without_text_is_not_decoded() {
        for text in [
            &[0x15][..],
            &[0x05][..],
            &[0x10, 0x00, 0x01][..],
            &[0x08][..],
        ] {
            assert_eq!(
                label_or_next_source(text).as_deref(),
                Some("eng"),
                "{text:02X?}"
            );
        }
    }

    /// EN 300 468 Annex A.2: a first byte of 0x20 or above selects table 00
    /// (Figure A.1, ISO/IEC 6937 with the euro sign at 0xA4), whose accents
    /// come before the letter they mark.
    #[test]
    fn default_table_text_is_iso_6937() {
        for (text, label) in [
            (&[0x43, 0x61, 0x66, 0xC2, 0x65][..], "Café"),
            (&[0x41, 0xA3][..], "A£"),
            (
                &[0xCA, b'A', b'n', b'g', b's', b't', b'r', 0xC8, b'o', b'm'][..],
                "Ångström",
            ),
            (&[0xA4, b'5'][..], "€5"),
        ] {
            assert_eq!(
                label_or_next_source(text).as_deref(),
                Some(label),
                "{text:02X?}"
            );
        }
    }

    /// Each byte of table 00 alone. Bytes below 0x80 are ASCII, 0x80..=0x9F
    /// are control codes, row 0xC0 holds the accents (never alone), and a
    /// code Figure A.1 leaves empty is refused.
    #[test]
    fn every_default_table_byte_decodes_or_is_refused() {
        // EN 300 468 Figure A.1, rows 0xA0 to 0xF0: the code point printed
        // under each glyph, 0 where the figure has none or holds an accent.
        const FIGURE_A1: [[u32; 16]; 6] = [
            [
                0x00A0, 0x00A1, 0x00A2, 0x00A3, 0x20AC, 0x00A5, 0, 0x00A7, 0x00A4, 0x2018, 0x201C,
                0x00AB, 0x2190, 0x2191, 0x2192, 0x2193,
            ],
            [
                0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x00D7, 0x00B5, 0x00B6, 0x00B7, 0x00F7, 0x2019,
                0x201D, 0x00BB, 0x00BC, 0x00BD, 0x00BE, 0x00BF,
            ],
            [0; 16],
            [
                0x2015, 0x00B9, 0x00AE, 0x00A9, 0x2122, 0x266A, 0x00AC, 0x00A6, 0, 0, 0, 0, 0x215B,
                0x215C, 0x215D, 0x215E,
            ],
            [
                0x2126, 0x00C6, 0x0110, 0x00AA, 0x0126, 0, 0x0132, 0x013F, 0x0141, 0x00D8, 0x0152,
                0x00BA, 0x00DE, 0x0166, 0x014A, 0x0149,
            ],
            [
                0x0138, 0x00E6, 0x0111, 0x00F0, 0x0127, 0x0131, 0x0133, 0x0140, 0x0142, 0x00F8,
                0x0153, 0x00DF, 0x00FE, 0x0167, 0x014B, 0x00AD,
            ],
        ];
        for b in 0x20..=0xFFu8 {
            let expected = match b {
                0x20..=0x7F => Some(char::from(b)),
                0x80..=0x9F => None,
                _ => match FIGURE_A1[usize::from(b - 0xA0) / 16][usize::from(b % 16)] {
                    0 => None,
                    cp => char::from_u32(cp),
                },
            };
            assert_eq!(dvb_text(&[b]), expected.map(String::from), "0x{b:02X}");
        }
        // The euro sign Figure A.1 adds at 0xA4, and the currency sign at 0xA8.
        assert_eq!(dvb_text(&[0xA4]).as_deref(), Some("\u{20AC}"));
        assert_eq!(dvb_text(&[0xA8]).as_deref(), Some("\u{A4}"));
    }

    /// Row 0xC0 of table 00 holds 13 non-spacing accents (0xC0, 0xC9 and
    /// 0xCC are empty). An accent and the letter after it are one character,
    /// the precomposed one; anything else after an accent is refused.
    #[test]
    fn an_accent_combines_with_the_letter_that_follows() {
        for (bytes, label) in [
            ([0xC1, b'a'], "à"),
            ([0xC2, b'e'], "é"),
            ([0xC3, b'o'], "ô"),
            ([0xC4, b'n'], "ñ"),
            ([0xC5, b'a'], "ā"),
            ([0xC6, b'g'], "ğ"),
            ([0xC7, b'z'], "ż"),
            ([0xC8, b'u'], "ü"),
            ([0xCA, b'A'], "Å"),
            ([0xCB, b'c'], "ç"),
            ([0xCD, b'o'], "ő"),
            ([0xCE, b'a'], "ą"),
            ([0xCF, b'c'], "č"),
        ] {
            assert_eq!(dvb_text(&bytes).as_deref(), Some(label), "{bytes:02X?}");
        }
        // Every byte of row 0xC0 before every ASCII letter: 251 pairs have a
        // precomposed character.
        let mut combined = 0;
        for accent in 0xC0..=0xCFu8 {
            for letter in (b'A'..=b'Z').chain(b'a'..=b'z') {
                if let Some(text) = dvb_text(&[accent, letter]) {
                    assert_eq!(text.chars().count(), 1, "{accent:02X} {letter:02X}");
                    combined += 1;
                }
            }
        }
        assert_eq!(combined, 251);
        for bytes in [
            &[b'a', 0xC2][..],       // an accent as the last byte
            &[0xC2][..],             // an accent alone
            &[0xC2, 0xC2, b'e'][..], // two accents in a row
            &[0xC2, b' ', b'e'][..], // an accent before a space
            &[0xC2, b'1'][..],       // an accent before a digit
            &[0xC2, 0xE1][..],       // an accent before a letter outside ASCII
            &[0xC2, b'b'][..],       // no precomposed b with acute
            &[0xC8, b'q'][..],       // no precomposed q with diaeresis
            &[0xC0, b'a'][..],       // the empty codes of row 0xC0
            &[0xC9, b'a'][..],
            &[0xCC, b'a'][..],
        ] {
            assert_eq!(dvb_text(bytes), None, "{bytes:02X?}");
        }
    }

    /// Labels and their UTF-8 bytes, written out.
    const UTF8_LABELS: [(&[u8], &str); 5] = [
        (&[0x43, 0x61, 0x66, 0xC3, 0xA9], "Café"),
        (&[0x43, 0x61, 0x66, 0xC3, 0xA9, 0x20, 0x45, 0x4F], "Café EO"),
        (&[0xC3, 0x89, 0x63, 0x6F, 0x6C, 0x65], "École"),
        (
            &[0xE6, 0x97, 0xA5, 0xE6, 0x9C, 0xAC, 0xE8, 0xAA, 0x9E],
            "日本語",
        ),
        (
            &[0x45, 0x4F, 0x2F, 0x49, 0x52, 0x20, 0xF0, 0x9F, 0x9B, 0xB0],
            "EO/IR 🛰",
        ),
    ];

    /// A default-table text that is well-formed UTF-8 as a whole is read as
    /// UTF-8: the bytes a writer puts in the default table when it writes
    /// UTF-8 with no selector, as this crate's `descriptors::component` did
    /// up to 0.10.0.
    #[test]
    fn default_table_text_that_is_well_formed_utf8_is_read_as_utf8() {
        for (text, label) in UTF8_LABELS {
            assert_eq!(text, label.as_bytes());
            assert_eq!(label_or_next_source(text).as_deref(), Some(label));
        }
    }

    /// The same texts behind the UTF-8 selector 0x15, as
    /// `descriptors::component` writes text that is not ASCII.
    #[test]
    fn a_label_written_with_the_utf8_selector_reads_the_same() {
        for (text, label) in UTF8_LABELS {
            let mut selected = vec![0x15];
            selected.extend_from_slice(text);
            assert_eq!(label_or_next_source(&selected).as_deref(), Some(label));
        }
    }

    #[test]
    fn a_label_written_by_the_component_helper_reads_back() {
        let bytes =
            crate::mpegts::descriptors::component(0xF, 0x05, 0x01, 0x42, *b"eng", "Café EO")
                .expect("within cap");
        assert_eq!(
            &bytes[8..],
            &[0x15, 0x43, 0x61, 0x66, 0xC3, 0xA9, 0x20, 0x45, 0x4F]
        );
        let descs = crate::mpegts::demux::low_level::walk_descriptors(&bytes)
            .expect("one whole descriptor");
        assert_eq!(extract_user_label(&descs).as_deref(), Some("Café EO"));
    }

    /// What the UTF-8 rule costs, enumerated: every UTF-8 character of two,
    /// three or four bytes whose bytes are also table 00 text. A UTF-8 text is
    /// ASCII and such characters, and a character's table 00 reading does not
    /// depend on its neighbours (its last byte is no accent, and an accent as
    /// its first byte would need a letter after it), so these are every text
    /// the rule reads differently from table 00.
    #[test]
    fn the_documented_misreads_are_exactly_these() {
        // The runs `extract_user_label` names: a character coded 0xD0..=0xDF,
        // 0xE0..=0xEF (not 0xED) or 0xF0..=0xF3, then one, two or three coded
        // 0xA0..=0xBF; 0xA6, 0xD8..=0xDB and 0xE5 code none.
        let coded = |b: u8| !matches!(b, 0xA6 | 0xD8..=0xDB | 0xE5);
        let documented = |run: &[u8]| {
            let (&lead, tail) = run.split_first().expect("two bytes or more");
            let lead_ok = match tail.len() {
                1 => (0xD0..=0xDF).contains(&lead),
                2 => (0xE0..=0xEF).contains(&lead) && lead != 0xED,
                _ => (0xF0..=0xF3).contains(&lead),
            };
            lead_ok && coded(lead) && tail.iter().all(|&b| (0xA0..=0xBF).contains(&b) && coded(b))
        };
        let mut misread = [0usize; 3];
        let mut buf = [0u8; 4];
        for (len, leads) in [(2, 0xC0..=0xDFu8), (3, 0xE0..=0xEF), (4, 0xF0..=0xF7)] {
            for lead in leads {
                for tail in 0..(1u32 << (6 * (len - 1))) {
                    buf[0] = lead;
                    for (i, b) in buf[1..len].iter_mut().enumerate() {
                        *b = 0x80 | ((tail >> (6 * i)) & 0x3F) as u8;
                    }
                    let run = &buf[..len];
                    let Ok(utf8) = core::str::from_utf8(run) else {
                        continue;
                    };
                    let table_00 = table_00_text(run).is_some();
                    assert_eq!(table_00, documented(run), "{run:02X?}");
                    if table_00 {
                        assert_eq!(dvb_text(run).as_deref(), Some(utf8), "{run:02X?}");
                        misread[len - 2] += 1;
                    }
                }
            }
        }
        assert_eq!(misread, [372, 13_454, 119_164]);
        // Two runs with ASCII between them: the whole text is UTF-8.
        assert_eq!(
            dvb_text(&[0xD3, 0xA0, b'2', b'0', 0xE1, 0xA0, 0xB0]).as_deref(),
            Some("\u{4E0}20\u{1830}")
        );
        // One stray byte makes the whole text table 00: "Café " written as
        // UTF-8 and then 0xA3 is refused (0xC3 is the circumflex, 0xA9 no
        // letter), and a run is read byte by byte.
        assert_eq!(dvb_text(&[0x43, 0x61, 0x66, 0xC3, 0xA9, 0x20, 0xA3]), None);
        assert_eq!(
            dvb_text(&[0xD3, 0xA0, b'2', 0xA3]).as_deref(),
            Some("©\u{A0}2£")
        );
    }

    #[test]
    fn extract_user_label_returns_none_when_nothing_matches() {
        let descs = vec![RawDescriptor {
            tag: 0x05,
            data: b"KLVA".to_vec(),
        }];
        assert_eq!(extract_user_label(&descs), None);
    }

    #[test]
    fn extract_user_label_empty_descriptors() {
        assert_eq!(extract_user_label(&[]), None);
    }

    #[test]
    fn extract_user_label_picks_user_private_tag_0xff() {
        // Standalone tag 0xFF — should be picked up.
        let descs = vec![RawDescriptor {
            tag: 0xFF,
            data: b"VIDEO-ARS".to_vec(),
        }];
        assert_eq!(extract_user_label(&descs).as_deref(), Some("VIDEO-ARS"));
    }

    #[test]
    fn extract_user_label_prefers_component_over_user_private() {
        // When both Component (0x50) and tag 0xFF are present, Component wins —
        // conformant descriptors take priority.
        let descs = vec![
            RawDescriptor {
                tag: 0x50,
                data: vec![0xF9, 0, 0, b'e', b'n', b'g', b'C', b'O', b'M', b'P'],
            },
            RawDescriptor {
                tag: 0xFF,
                data: b"PRIVATE".to_vec(),
            },
        ];
        assert_eq!(extract_user_label(&descs).as_deref(), Some("COMP"));
    }

    #[test]
    fn classify_audio_stream_types() {
        use crate::codec::AudioCodec;
        assert_eq!(classify_audio_stream_type(0x03), Some(AudioCodec::Mp2));
        assert_eq!(classify_audio_stream_type(0x04), Some(AudioCodec::Mp2));
        assert_eq!(classify_audio_stream_type(0x0F), Some(AudioCodec::Aac));
        assert_eq!(classify_audio_stream_type(0x11), Some(AudioCodec::AacLatm));
        assert_eq!(classify_audio_stream_type(0x81), Some(AudioCodec::Ac3));
        assert_eq!(classify_audio_stream_type(0x87), None); // E-AC-3: not classified
        assert_eq!(classify_audio_stream_type(0xF1), None); // user-private: not classified
        assert_eq!(classify_audio_stream_type(0x1B), None); // H.264: not audio
    }
}
