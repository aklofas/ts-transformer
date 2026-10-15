//! MISB ST 0605 §7 Precision Time Stamp Pack: PES-emit-time auxiliary
//! KLV record commonly multiplexed alongside an ST 0601 LS in real
//! captures. Body is a 1-byte Time Status (per MISB ST 0603 §7.4) plus
//! an 8-byte big-endian microsecond timestamp (per MISB ST 0603 §7.1).
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Registered in MISB ST 0807.27 row 1061 (UL CRC 23259).
//!
//! ## Spec coverage
//!
//! **Standard:** MISB ST 0605 §7 Precision Time Stamp Pack
//! (registered in MISB ST 0807.27 row 1061; UL CRC 23259).
//!
//! **Fields parsed:** 1-byte Time Status (per MISB ST 0603 §7.4
//! Table 3 — bit 7: lock; bit 6: discontinuity; bit 5: reverse
//! direction; bits 4-0: reserved 0b11111) + 8-byte big-endian
//! microsecond timestamp (per MISB ST 0603 §7.1).
//!
//! **Decode mode:** permissive — [`decode`] accepts any byte
//! pattern with the correct UL prefix and 9-byte fixed-length BER
//! body. Reserved-bit validation is caller-opt-in via
//! [`TimeStatus::reserved_bits_valid`].
//!
//! **Deferred:** the ST 0605.7 Nano Precision Time Stamp Pack
//! (tag 0x06 0x0E…0x0F, nanosecond resolution, 10-byte body) is not
//! yet implemented. The current decoder handles only the standard
//! 9-byte Precision Time Stamp Pack. See
//! `docs/project/deferred-features.md` → "ST 0605 Nano Precision
//! Time Stamp Pack".

pub(crate) mod decode;
pub(crate) mod encode;
pub(crate) mod model;

pub use decode::decode;
pub use encode::encode;
pub use model::{PrecisionTimeStampPack, TimeStatus};

#[cfg(test)]
mod tests {
    use super::decode::decode;
    use super::encode::encode;
    use super::model::{PrecisionTimeStampPack, TimeStatus};
    use crate::error::KlvDecodeError;
    use crate::klv::universal_label::UniversalLabel;
    use crate::test_prelude::*;

    #[test]
    fn time_status_locked_normal() {
        // 0x1F = 0b 0001 1111: locked, normal increment, reserved bits ok
        let s = TimeStatus(0x1F);
        assert!(s.is_locked());
        assert!(!s.has_discontinuity());
        assert!(!s.is_reverse_jump());
        assert!(s.reserved_bits_valid());
    }

    #[test]
    fn time_status_lock_unknown_normal() {
        // 0x9F = 0b 1001 1111: lock unknown, normal increment
        let s = TimeStatus(0x9F);
        assert!(!s.is_locked());
        assert!(!s.has_discontinuity());
        assert!(s.reserved_bits_valid());
    }

    #[test]
    fn time_status_discontinuity_reverse() {
        // 0xFF = 0b 1111 1111: lock unknown, discontinuity, reverse jump
        let s = TimeStatus(0xFF);
        assert!(!s.is_locked());
        assert!(s.has_discontinuity());
        assert!(s.is_reverse_jump());
        assert!(s.reserved_bits_valid());
    }

    #[test]
    fn time_status_invalid_reserved() {
        // Reserved bits must be 11111; 0x10 = 0b 0001 0000 has bits 3-0 = 0
        let s = TimeStatus(0x10);
        assert!(!s.reserved_bits_valid());
    }

    #[test]
    fn decode_locked_pack() {
        // UL + BER 0x09 + status 0x1F + 8-byte BE timestamp
        let mut buf = Vec::new();
        buf.extend_from_slice(&UniversalLabel::PRECISION_TIMESTAMP_PACK_UL.0);
        buf.push(0x09);
        buf.push(0x1F); // locked, normal
        buf.extend_from_slice(&1_753_983_356_565_441u64.to_be_bytes());
        let pack = decode(&buf).unwrap();
        assert!(pack.time_status.is_locked());
        assert!(pack.time_status.reserved_bits_valid());
        assert_eq!(pack.timestamp_us, 1_753_983_356_565_441);
    }

    #[test]
    fn decode_rejects_wrong_ul() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&UniversalLabel::ST_0601_LS.0);
        buf.push(0x09);
        buf.push(0x1F);
        buf.extend_from_slice(&[0u8; 8]);
        let err = decode(&buf).unwrap_err();
        assert!(matches!(
            err,
            KlvDecodeError::UnexpectedUniversalLabel { .. }
        ));
    }

    #[test]
    fn decode_rejects_short_buffer() {
        let buf = [0u8; 8];
        let err = decode(&buf).unwrap_err();
        assert!(matches!(err, KlvDecodeError::Truncated { .. }));
    }

    #[test]
    fn decode_rejects_wrong_body_length() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&UniversalLabel::PRECISION_TIMESTAMP_PACK_UL.0);
        buf.push(0x05); // declared 5, not 9
        buf.extend_from_slice(&[0u8; 5]);
        let err = decode(&buf).unwrap_err();
        assert!(matches!(
            err,
            KlvDecodeError::BadTimeStampPackLength { got: 5 }
        ));
    }

    #[test]
    fn encode_round_trip() {
        let pack = PrecisionTimeStampPack {
            time_status: TimeStatus(0x1F),
            timestamp_us: 1_700_000_000_123_456,
        };
        let buf = encode(&pack);
        assert_eq!(buf.len(), 26); // 16 UL + 1 BER + 9 body
        let back = decode(&buf).unwrap();
        assert_eq!(back, pack);
    }

    #[test]
    fn encode_starts_with_ul_and_length() {
        let pack = PrecisionTimeStampPack {
            time_status: TimeStatus(0x9F),
            timestamp_us: 0,
        };
        let buf = encode(&pack);
        assert_eq!(&buf[..16], &UniversalLabel::PRECISION_TIMESTAMP_PACK_UL.0);
        assert_eq!(buf[16], 0x09);
        assert_eq!(buf[17], 0x9F);
        assert_eq!(&buf[18..26], &[0u8; 8]);
    }
}
