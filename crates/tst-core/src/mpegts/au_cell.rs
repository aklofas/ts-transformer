//! Synchronous metadata Access Unit cell wrapper.
//!
//! Per ITU-T H.222.0 V9 §2.12.4.2 Tables 2-155+2-156 (08/2023, PDF p.209),
//! synchronous metadata streams (`stream_type = 0x15`) carry a sequence of
//! `Metadata_AU_cell` records inside each PES packet. Each cell prefixes its
//! payload with a 5-byte fixed header:
//!
//! ```text
//! metadata_service_id          u8        // typically 0x00 per ST 1402.2 App. B
//! sequence_number              u8        // increments mod 256 per cell
//! cell_fragment_indication     u(2)   \
//! decoder_config_flag          u(1)    \  packed into 1 byte
//! random_access_indicator      u(1)    /  (2+1+1+4 bits)
//! reserved                     u(4)   /
//! AU_cell_data_length          u(16) BE  // payload byte count
//! AU_cell_data_byte[N]                   // N = AU_cell_data_length
//! ```
//!
//! For typical MISB sync KLV (one PES = one complete KLV record), the cell is
//! single-and-complete: `cell_fragment_indication = '11'` (binary 3),
//! `decoder_config_flag = 0`, `random_access_indicator = 1` (every cell is an
//! entry point — the meaning of "random access" is metadata-format-defined).
//!
//! PTS lives in the PES header (per H.222.0 §2.12.4.1), not inside the AU cell.
//! The muxer's PES writer carries it on the `Muxer::push_klv_to`'s `pts_90khz`
//! arg.
//!
//! ST 1402.2 §9.4.1 + Appendix B Table 2 specializes this generic AU cell for
//! KLV by mandating `metadata_format_identifier = "KLVA"` in the PMT
//! metadata_descriptor; the wrapper itself is H.222.0's.

use crate::error::KlvDecodeError;
use alloc::vec::Vec;

/// Cell fragment indication — H.222.0 V9 Table 2-157.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellFragmentIndication {
    /// `'00'` (0): middle cell of a multi-cell AU.
    Middle = 0,
    /// `'01'` (1): last cell of a multi-cell AU.
    Last = 1,
    /// `'10'` (2): first cell of a multi-cell AU.
    First = 2,
    /// `'11'` (3): single cell carrying a complete AU. Typical for MISB sync KLV.
    Complete = 3,
}

/// Header fields for a `Metadata_AU_cell`. The `AU_cell_data_length` is
/// computed at write time from the supplied payload slice — it is not a
/// header field the caller sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuCellHeader {
    /// `metadata_service_id` u8. ST 1402.2 App. B Table 2: `0x00` typical.
    pub metadata_service_id: u8,
    /// `sequence_number` u8. Increments mod 256 per cell, independent of
    /// `metadata_service_id` (per Table 2-156 semantics).
    pub sequence_number: u8,
    /// `cell_fragment_indication` u(2). For single-cell AUs use `Complete`.
    pub cell_fragment_indication: CellFragmentIndication,
    /// `decoder_config_flag` u(1). Set to `true` if the AU carries decoder
    /// configuration; we do not currently emit decoder config.
    pub decoder_config_flag: bool,
    /// `random_access_indicator` u(1). Set to `true` if the AU is an entry
    /// point. The meaning of "entry point" is metadata-format-defined.
    pub random_access_indicator: bool,
}

/// Maximum payload size for a single AU cell. `AU_cell_data_length` is a
/// 16-bit field, so payloads cap at `u16::MAX = 65535` bytes.
pub const MAX_AU_CELL_PAYLOAD: usize = u16::MAX as usize;

/// Serialize a `Metadata_AU_cell` into `out`. Appends 5 header bytes followed
/// by `payload`. Returns the total bytes written (`5 + payload.len()`).
///
/// # Errors
/// Returns [`AuCellError::PayloadTooLarge`] if `payload.len() > MAX_AU_CELL_PAYLOAD`.
pub fn write_metadata_au_cell(
    out: &mut Vec<u8>,
    header: AuCellHeader,
    payload: &[u8],
) -> Result<usize, AuCellError> {
    if payload.len() > MAX_AU_CELL_PAYLOAD {
        return Err(AuCellError::PayloadTooLarge {
            size: payload.len(),
            max: MAX_AU_CELL_PAYLOAD,
        });
    }
    let len = payload.len() as u16;

    // Pack the 1-byte flags field: cfi(2b) | dcf(1b) | rai(1b) | reserved(4b).
    // Reserved bits emitted as all-1s per common MPEG-TS reserved-bit convention.
    let cfi = (header.cell_fragment_indication as u8) & 0b11;
    let dcf = u8::from(header.decoder_config_flag);
    let rai = u8::from(header.random_access_indicator);
    let flags = (cfi << 6) | (dcf << 5) | (rai << 4) | 0b0000_1111;

    out.push(header.metadata_service_id);
    out.push(header.sequence_number);
    out.push(flags);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);

    Ok(5 + payload.len())
}

/// Errors raised by `write_metadata_au_cell`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AuCellError {
    /// Payload exceeds the 16-bit `AU_cell_data_length` field cap.
    #[error("AU cell payload {size} B exceeds 16-bit length field cap {max} B")]
    PayloadTooLarge { size: usize, max: usize },
}

/// Read a `Metadata_AU_cell` from the start of `buf`. Returns the parsed
/// header and a slice into the payload region of `buf` (zero-copy).
///
/// Does not validate the surrounding stream context. Caller is responsible
/// for ensuring `buf` came from a sync-metadata PES (`stream_type = 0x15`).
///
/// # Errors
/// Returns [`KlvDecodeError::Truncated`] if `buf` is shorter than the 5-byte
/// header or if the declared `AU_cell_data_length` exceeds `buf.len() - 5`.
/// Returns [`KlvDecodeError::ReservedBitsInvalid`] if the flags byte's
/// reserved low nibble is not `1111`.
pub fn read_metadata_au_cell(buf: &[u8]) -> Result<(AuCellHeader, &[u8]), KlvDecodeError> {
    if buf.len() < 5 {
        return Err(KlvDecodeError::Truncated {
            offset: 0,
            needed: 5,
            have: buf.len(),
        });
    }
    let metadata_service_id = buf[0];
    let sequence_number = buf[1];
    let flags = buf[2];
    // H.222.0 V9 Table 2-156: the low nibble of the flags byte is
    // `reserved`, and MPEG-2 systems reserved bits read as all ones (our
    // writer emits 0b1111). Anything else is not an AU cell header — most
    // likely the fifth byte of a bare SMPTE UL (review 9, int R9-01).
    if flags & 0x0F != 0x0F {
        return Err(KlvDecodeError::ReservedBitsInvalid { got: flags & 0x0F });
    }
    let cfi = (flags >> 6) & 0b11;
    let cell_fragment_indication = match cfi {
        0 => CellFragmentIndication::Middle,
        1 => CellFragmentIndication::Last,
        2 => CellFragmentIndication::First,
        3 => CellFragmentIndication::Complete,
        _ => unreachable!("2-bit value masked above"),
    };
    let decoder_config_flag = (flags & 0b0010_0000) != 0;
    let random_access_indicator = (flags & 0b0001_0000) != 0;
    let len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
    if buf.len() < 5 + len {
        return Err(KlvDecodeError::Truncated {
            offset: 5,
            needed: 5 + len,
            have: buf.len(),
        });
    }
    Ok((
        AuCellHeader {
            metadata_service_id,
            sequence_number,
            cell_fragment_indication,
            decoder_config_flag,
            random_access_indicator,
        },
        &buf[5..5 + len],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_header() -> AuCellHeader {
        AuCellHeader {
            metadata_service_id: 0x00,
            sequence_number: 0x42,
            cell_fragment_indication: CellFragmentIndication::Complete,
            decoder_config_flag: false,
            random_access_indicator: true,
        }
    }

    #[test]
    fn write_emits_exact_5_byte_header_per_h222_table_2_156() {
        let mut out = Vec::new();
        let payload = vec![0xAA, 0xBB, 0xCC];
        let written = write_metadata_au_cell(&mut out, sample_header(), &payload).unwrap();
        assert_eq!(written, 5 + 3);
        assert_eq!(out.len(), 8);
        // metadata_service_id=0x00, sequence_number=0x42.
        assert_eq!(out[0], 0x00);
        assert_eq!(out[1], 0x42);
        // flags byte: cfi=11 (complete), dcf=0, rai=1, reserved=1111 →
        // 0b1101_1111 = 0xDF.
        assert_eq!(out[2], 0b1101_1111);
        // AU_cell_data_length = 3, big-endian.
        assert_eq!(out[3..5], [0x00, 0x03]);
        assert_eq!(&out[5..], &payload[..]);
    }

    #[test]
    fn round_trip_recovers_header_and_payload() {
        let mut out = Vec::new();
        let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
        write_metadata_au_cell(&mut out, sample_header(), &payload).unwrap();
        let (hdr, recovered) = read_metadata_au_cell(&out).unwrap();
        assert_eq!(hdr, sample_header());
        assert_eq!(recovered, &payload[..]);
    }

    #[test]
    fn read_rejects_truncated_header() {
        // Only 4 bytes — missing the second length byte.
        let buf = [0x00, 0x42, 0xDF, 0x00];
        let res = read_metadata_au_cell(&buf);
        assert!(matches!(res, Err(KlvDecodeError::Truncated { .. })));
    }

    #[test]
    fn read_rejects_truncated_payload() {
        // Header declares 100-byte payload; only 50 bytes follow.
        let mut buf = vec![0x00, 0x00, 0xDF, 0x00, 0x64];
        buf.extend_from_slice(&[0xAA; 50]);
        let res = read_metadata_au_cell(&buf);
        assert!(matches!(res, Err(KlvDecodeError::Truncated { .. })));
    }

    /// H.222.0 V9 Table 2-156: the flags byte's low nibble is reserved and
    /// reads as all ones (our writer emits 0b1111). Anything else is not a
    /// cell header — most likely the fifth byte of a bare SMPTE UL, which is
    /// how a 13 319+ byte async KLV packet used to pass as a Middle cell
    /// (review 9, int R9-01).
    #[test]
    fn read_rejects_a_reserved_nibble_that_is_not_all_ones() {
        // cfi = 11, dcf = 0, rai = 0, reserved = 0000; length 0.
        let buf = [0x00, 0x42, 0xC0, 0x00, 0x00];
        assert!(matches!(
            read_metadata_au_cell(&buf),
            Err(KlvDecodeError::ReservedBitsInvalid { got: 0x0 })
        ));
        // The UL case the demuxer mis-took for a cell: flags byte 0x2B.
        let buf = [0x06, 0x0E, 0x2B, 0x00, 0x00];
        assert!(matches!(
            read_metadata_au_cell(&buf),
            Err(KlvDecodeError::ReservedBitsInvalid { got: 0xB })
        ));
    }

    #[test]
    fn write_rejects_payload_larger_than_u16() {
        let mut out = Vec::new();
        let payload = vec![0xAA; (u16::MAX as usize) + 1];
        let res = write_metadata_au_cell(&mut out, sample_header(), &payload);
        assert!(matches!(res, Err(AuCellError::PayloadTooLarge { .. })));
    }

    #[test]
    fn cell_fragment_indication_round_trip_all_4_values() {
        for cfi in [
            CellFragmentIndication::Middle,
            CellFragmentIndication::Last,
            CellFragmentIndication::First,
            CellFragmentIndication::Complete,
        ] {
            let hdr = AuCellHeader {
                metadata_service_id: 0,
                sequence_number: 0,
                cell_fragment_indication: cfi,
                decoder_config_flag: false,
                random_access_indicator: false,
            };
            let mut out = Vec::new();
            write_metadata_au_cell(&mut out, hdr, &[]).unwrap();
            let (parsed, _) = read_metadata_au_cell(&out).unwrap();
            assert_eq!(parsed.cell_fragment_indication, cfi);
        }
    }

    #[test]
    fn au_cell_error_display_unchanged() {
        assert_eq!(
            AuCellError::PayloadTooLarge {
                size: 65536,
                max: 65535,
            }
            .to_string(),
            "AU cell payload 65536 B exceeds 16-bit length field cap 65535 B"
        );
    }

    #[test]
    fn flag_bits_round_trip_independently() {
        for &dcf in &[false, true] {
            for &rai in &[false, true] {
                let hdr = AuCellHeader {
                    metadata_service_id: 0,
                    sequence_number: 0,
                    cell_fragment_indication: CellFragmentIndication::Complete,
                    decoder_config_flag: dcf,
                    random_access_indicator: rai,
                };
                let mut out = Vec::new();
                write_metadata_au_cell(&mut out, hdr, &[]).unwrap();
                let (parsed, _) = read_metadata_au_cell(&out).unwrap();
                assert_eq!(parsed.decoder_config_flag, dcf);
                assert_eq!(parsed.random_access_indicator, rai);
            }
        }
    }
}
