//! Program-specific information (PAT, PMT, descriptors).

use crate::mpegts::common::crc32::crc32_mpeg2;
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
    /// belong to a multi-section table that requires reassembly. Current
    /// demuxer scope handles single-section tables only; multi-section
    /// tables are rejected with this error and the demuxer emits a
    /// [`crate::mpegts::demux::event::NonConformantIssue::PsiMultiSectionUnsupported`]
    /// event. Full §2.4.4.5 reassembly is deferred until a real consumer
    /// needs it.
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

#[cfg(test)]
mod pat_tests {
    use super::*;

    /// Build a minimal valid PAT section bytes for testing.
    /// `programs` is `(program_number, pid)` tuples.
    fn build_pat_section(
        transport_stream_id: u16,
        version: u8,
        programs: &[(u16, u16)],
    ) -> Vec<u8> {
        let mut s = Vec::new();
        s.push(0x00); // table_id = PAT
        // section_syntax_indicator (1) + reserved-zero (1) + reserved (2) + section_length (12)
        let section_length = 5 + 4 * programs.len() + 4; // header tail + program loop + CRC32
        let sl = section_length as u16;
        s.push(0xB0 | ((sl >> 8) as u8 & 0x0F)); // 1011_xxxx
        s.push((sl & 0xFF) as u8);
        s.push((transport_stream_id >> 8) as u8);
        s.push((transport_stream_id & 0xFF) as u8);
        s.push(0xC1 | ((version & 0x1F) << 1)); // reserved(2)=11 + version(5) + current_next(1)=1
        s.push(0x00); // section_number
        s.push(0x00); // last_section_number
        for (pn, pid) in programs {
            s.push((pn >> 8) as u8);
            s.push((pn & 0xFF) as u8);
            s.push(0xE0 | ((pid >> 8) as u8 & 0x1F));
            s.push((pid & 0xFF) as u8);
        }
        let crc = crc32_mpeg2(&s);
        s.push((crc >> 24) as u8);
        s.push((crc >> 16) as u8);
        s.push((crc >> 8) as u8);
        s.push(crc as u8);
        s
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
///      starting at offset 6 is UTF-8 free text. ISO/IEC 13818-1 Annex.
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
    // 1. Component descriptor — UTF-8 free text after the 6-byte header.
    if let Some(d) = descs.iter().find(|d| d.tag == 0x50) {
        if d.data.len() > 6 {
            let raw = &d.data[6..];
            if let Ok(s) = core::str::from_utf8(raw) {
                let trimmed = s.trim_end_matches('\0').trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    // 2. Stream Identifier — single component_tag byte.
    if let Some(d) = descs.iter().find(|d| d.tag == 0x52) {
        if let Some(&t) = d.data.first() {
            return Some(format!("tag={t}"));
        }
    }
    // 3. Metadata descriptor — generic "KLV" label.
    if descs.iter().any(|d| d.tag == 0x26) {
        return Some("KLV".to_string());
    }
    // 4. ISO 639 Language — first 3 bytes.
    if let Some(d) = descs.iter().find(|d| d.tag == 0x0A) {
        if d.data.len() >= 3 {
            if let Ok(s) = core::str::from_utf8(&d.data[..3]) {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    // 5. User-private descriptor (tag 0xFF) — Family B (ARS) corpus
    // shape; reserved per ISO/IEC 13818-1 but used in practice as the
    // de-facto label slot. Best-effort UTF-8.
    if let Some(d) = descs.iter().find(|d| d.tag == 0xFF) {
        if let Ok(s) = core::str::from_utf8(&d.data) {
            let trimmed = s.trim_end_matches('\0').trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Maps PMT `stream_type` byte → typed audio codec. Returns `None` for
/// unrecognized stream_types; caller routes those to `StreamKind::Unknown(_)`
/// or maps them via `DemuxerConfig::treat_as`.
pub(crate) fn classify_audio_stream_type(
    stream_type: u8,
) -> Option<crate::mpegts::demux::event::AudioCodec> {
    use crate::mpegts::demux::event::AudioCodec;
    match stream_type {
        0x03 | 0x04 => Some(AudioCodec::Mp2),
        0x0F => Some(AudioCodec::Aac),
        0x11 => Some(AudioCodec::AacLatm),
        0x81 => Some(AudioCodec::Ac3),
        _ => None,
    }
}

/// Returns the `linked_pid` from a `metadata_descriptor` if present, else
/// `None`. The descriptor's structure is per H.222.0 §2.6.60 Table 2-89;
/// the trailing-PID readout below is **heuristic** and not normatively
/// defined by either H.222.0 §2.6.60 (where trailing bytes are
/// `private_data_byte[N]`) or by ST 1402.2 (which does not specify a
/// linked-PID field at all).
///
/// Best-effort interpretation: if the descriptor body ends with at least
/// 2 bytes whose value falls in the PID range, return that value. Other
/// shapes return `None`; the caller treats that as "no declared link."
///
/// Note: if the trailing 2 bytes coincidentally land in the valid PID
/// range without being a real linked PID, the caller will receive a
/// linkage that doesn't reflect actual encoder intent. The demuxer
/// surfaces this as `LinkSource::Declared`; consumers should treat
/// declared linkages as informational and validate (e.g., by confirming
/// the linked PID appears as an `elementary_PID` in the same PMT) when
/// strict pairing matters.
pub fn extract_metadata_link(descs: &[RawDescriptor]) -> Option<u16> {
    let d = descs.iter().find(|d| d.tag == 0x26)?;
    // metadata_descriptor body:
    //   metadata_application_format (16) + maybe metadata_application_format_identifier (32)
    //   metadata_format (8) + maybe metadata_format_identifier (32)
    //   metadata_service_id (8)
    //   decoder_config_flags (3) + DSM_CC_flag (1) + reserved (4)
    //   if DSM_CC_flag: service_identification_length (8) + service_identification_record(...)
    //   for body remainder: private_data
    //
    // Many real encoders produce a 5-byte body with the linked PID encoded
    // in private_data as the trailing 2 bytes. We accept that lenient
    // shape: if the descriptor body is at least 5 bytes long and the
    // trailing 2 bytes look like a valid PID (0x0010..=0x1FFE), return it.
    if d.data.len() >= 5 {
        let candidate = u16::from_be_bytes([d.data[d.data.len() - 2], d.data[d.data.len() - 1]]);
        if (0x0010..=0x1FFE).contains(&candidate) {
            return Some(candidate);
        }
    }
    None
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

    fn build_pmt_section(
        program_number: u16,
        version: u8,
        pcr_pid: u16,
        program_descs: &[RawDescriptor],
        streams: &[PmtStream],
    ) -> Vec<u8> {
        // section_syntax_indicator(1) | '0'(1) | reserved(2) | section_length(12)
        // Body length is computed below; section_length covers from byte 3 to end including CRC.
        let mut s: Vec<u8> = vec![
            0x02, // table_id = PMT
            0xB0, // placeholder for top nibble of section_length
            0x00, // placeholder low byte
            (program_number >> 8) as u8,
            (program_number & 0xFF) as u8,
            0xC1 | ((version & 0x1F) << 1),
            0x00, // section_number
            0x00, // last_section_number
            0xE0 | ((pcr_pid >> 8) as u8 & 0x1F),
            (pcr_pid & 0xFF) as u8,
        ];
        // program_info_length (12 bits, top 4 reserved)
        let mut prog_desc_buf = Vec::new();
        for d in program_descs {
            prog_desc_buf.push(d.tag);
            prog_desc_buf.push(d.data.len() as u8);
            prog_desc_buf.extend_from_slice(&d.data);
        }
        s.push(0xF0 | ((prog_desc_buf.len() >> 8) as u8 & 0x0F));
        s.push((prog_desc_buf.len() & 0xFF) as u8);
        s.extend_from_slice(&prog_desc_buf);
        // streams loop
        for st in streams {
            s.push(st.stream_type);
            s.push(0xE0 | ((st.elementary_pid >> 8) as u8 & 0x1F));
            s.push((st.elementary_pid & 0xFF) as u8);
            let mut sd_buf = Vec::new();
            for d in &st.descriptors {
                sd_buf.push(d.tag);
                sd_buf.push(d.data.len() as u8);
                sd_buf.extend_from_slice(&d.data);
            }
            s.push(0xF0 | ((sd_buf.len() >> 8) as u8 & 0x0F));
            s.push((sd_buf.len() & 0xFF) as u8);
            s.extend_from_slice(&sd_buf);
        }
        // Backfill section_length (from byte 3 forward, including CRC).
        let section_length = (s.len() - 3 + 4) as u16;
        s[1] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
        s[2] = (section_length & 0xFF) as u8;
        // CRC.
        let crc = crc32_mpeg2(&s);
        s.push((crc >> 24) as u8);
        s.push((crc >> 16) as u8);
        s.push((crc >> 8) as u8);
        s.push(crc as u8);
        s
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
        use crate::mpegts::demux::event::AudioCodec;
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
