//! Shared TLV emit helpers used by `klv::st0903::encode` and
//! `klv::st0903::vtarget_pack::write_pack`.
//!
//! Centralizing keeps the wire-format conventions (1-byte tag, BER
//! length, raw value bytes) in one place. Future nested-LS typed
//! layers (VMask, VObject, VFeature, VTracker, VChip) will reuse the
//! same helpers when they land.

use crate::error::KlvEncodeError;
use crate::klv::imapb::{ImapbParams, encode_imapb};
use crate::klv::length::{write_ber, write_var_uint_min_into};
use alloc::vec::Vec;

/// Emit a `[tag][BER length][value]` TLV to `out`.
pub(super) fn emit_tlv(out: &mut Vec<u8>, tag: u8, value: &[u8]) -> Result<(), KlvEncodeError> {
    out.push(tag);
    let mut len_buf = [0u8; 9];
    let len_n = write_ber(value.len(), &mut len_buf)?;
    out.extend_from_slice(&len_buf[..len_n]);
    out.extend_from_slice(value);
    Ok(())
}

/// Emit a VarUint-encoded value as a TLV.
pub(super) fn emit_var(out: &mut Vec<u8>, tag: u8, value: u32) -> Result<(), KlvEncodeError> {
    let mut tmp = Vec::with_capacity(4);
    write_var_uint_min_into(value as u64, &mut tmp);
    emit_tlv(out, tag, &tmp)
}

/// Emit a VarUint-encoded `u64` value as a TLV (ST 0903.6 V6 pixel numbers).
pub(super) fn emit_var_u64(out: &mut Vec<u8>, tag: u8, value: u64) -> Result<(), KlvEncodeError> {
    let mut tmp = Vec::with_capacity(8);
    write_var_uint_min_into(value, &mut tmp);
    emit_tlv(out, tag, &tmp)
}

/// Emit an IMAPB-encoded value as a TLV with caller-specified wire
/// length (per ST 0903.6 — Tag 12 uses length 2; Tags 10/11/13/14/15/
/// 16 in pack and Tags 11/12 top-level use length 2 or 3 per spec).
pub(super) fn emit_imapb_n(
    out: &mut Vec<u8>,
    tag: u8,
    value: f64,
    min: f64,
    max: f64,
    length: usize,
) -> Result<(), KlvEncodeError> {
    let mut buf = vec![0u8; length];
    // `encode_imapb` knows no tag: a value out of range names this item.
    encode_imapb(&ImapbParams { min, max, length }, value, &mut buf).map_err(|e| match e {
        KlvEncodeError::OutOfRange {
            value,
            min,
            max,
            hint,
            ..
        } => KlvEncodeError::OutOfRange {
            tag: u32::from(tag),
            value,
            min,
            max,
            hint,
        },
        e => e,
    })?;
    emit_tlv(out, tag, &buf)
}
