//! ST 0605 decode — parse a Precision Time Stamp Pack from a KLV buffer.

use crate::error::KlvDecodeError;
use crate::klv::length::read_ber;
use crate::klv::st0605::model::{PrecisionTimeStampPack, TimeStatus};
use crate::klv::universal_label::UniversalLabel;

/// Decode a Precision Time Stamp Pack from a buffer that starts with
/// the 16-byte UL. Returns the typed view; verifies the UL and body
/// length match MISB ST 0605 §7. Does **not** validate the reserved
/// bits in the status byte — call `time_status.reserved_bits_valid()`
/// on the returned struct if you need that check.
pub fn decode(buf: &[u8]) -> Result<PrecisionTimeStampPack, KlvDecodeError> {
    if buf.len() < 16 {
        return Err(KlvDecodeError::Truncated {
            offset: 0,
            needed: 16,
            have: buf.len(),
        });
    }
    let mut ul = [0u8; 16];
    ul.copy_from_slice(&buf[..16]);
    let label = UniversalLabel::new(ul);
    if label != UniversalLabel::PRECISION_TIMESTAMP_PACK_UL {
        return Err(KlvDecodeError::UnexpectedUniversalLabel {
            expected: UniversalLabel::PRECISION_TIMESTAMP_PACK_UL,
            found: label,
        });
    }
    // Read from `buf[16..]`, so the outer length's own errors come back
    // slice-relative — rebase by the UL length to keep every offset this
    // decoder reports indexed against the caller's own buffer.
    let (declared_len, after_len) = read_ber(&buf[16..]).map_err(|mut e| {
        crate::klv::length::rebase_offset(&mut e, 16);
        e
    })?;
    if declared_len != 9 {
        return Err(KlvDecodeError::BadTimeStampPackLength { got: declared_len });
    }
    if after_len.len() < 9 {
        return Err(KlvDecodeError::Truncated {
            offset: buf.len() - after_len.len(),
            needed: 9,
            have: after_len.len(),
        });
    }
    let body = &after_len[..9];
    let mut ts_bytes = [0u8; 8];
    ts_bytes.copy_from_slice(&body[1..9]);
    Ok(PrecisionTimeStampPack {
        time_status: TimeStatus(body[0]),
        timestamp_us: u64::from_be_bytes(ts_bytes),
    })
}
