//! Byte-faithful tag-level patching for ST 0601 UAS Datalink local sets.
//!
//! [`patch`] rewrites ONLY the TLVs named by the edits record; every
//! other TLV — unknown/vendor tags and non-canonical BER length forms
//! included — is copied byte-for-byte in original order, and the tag-1
//! checksum is recomputed. This is the surgical primitive for
//! "fix the corner points, touch nothing else" metadata correction;
//! decode→modify→re-encode normalizes TLV order and re-encodes every
//! IMAPB float, which this avoids.

use alloc::vec::Vec;

use crate::error::{KlvDecodeError, KlvEncodeError, KlvPatchError};
use crate::klv::checksum::checksum_running_sum_16;
use crate::klv::length::{read_ber, read_ber_oid, rebase_offset, write_ber, write_ber_oid};

use super::encode::encode_tag_value;
use super::model::{OutOfRangePolicy, UasDatalinkLs};
use super::tags::TAGS;

/// By-value adapter over [`rebase_offset`] for the `map_err` chains below:
/// rebase a slice-relative decode-error offset to an absolute `raw` offset.
fn rebased(mut e: KlvDecodeError, base: usize) -> KlvDecodeError {
    rebase_offset(&mut e, base);
    e
}

/// Append one canonical `[BER-OID tag][BER length][value]` TLV.
fn emit_tlv(tag: u32, value: &[u8], body: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    let mut tag_buf = [0u8; 8];
    let n = write_ber_oid(tag, &mut tag_buf)?;
    body.extend_from_slice(&tag_buf[..n]);
    let mut len_buf = [0u8; 16];
    let m = write_ber(value.len(), &mut len_buf)?;
    body.extend_from_slice(&len_buf[..m]);
    body.extend_from_slice(value);
    Ok(())
}

/// Patch named tags in a raw ST 0601 local set; every other TLV is
/// copied verbatim. The edits carrier is a partial [`UasDatalinkLs`]
/// (`UasDatalinkLs::default()` + the fields to change): `Some` fields
/// are re-encoded, `None` fields leave the input untouched.
///
/// Semantics:
/// - An edited tag PRESENT in the input is re-encoded in place (every
///   occurrence, if the input carries non-compliant duplicates).
/// - An edited tag ABSENT from the input is inserted before the
///   trailing checksum (typed tags in table order, then
///   `edits.unknown` in given order) — at the end if there is none.
/// - Re-encoding an edited tag emits a CANONICAL tag/length encoding
///   for that TLV — editing a tag whose input encoding was
///   non-canonical is not a byte-level no-op, even when the new value
///   bytes match the old.
/// - The tag-1 checksum is recomputed iff the input has one
///   (mirror-input). Only the LAST tag-1 occurrence is recomputed;
///   earlier (non-compliant duplicate) occurrences are copied
///   verbatim. The input checksum is NOT verified — `patch` is
///   an editor, not a validator; run [`super::decode`] first if you
///   need validation.
/// - The outer BER length bytes are preserved verbatim when the body
///   size is unchanged; re-encoded canonically otherwise.
/// - Bytes after the declared outer length (e.g. capture padding) are
///   copied to the output verbatim — patch accepts everything lenient
///   decode accepts and copies what it does not understand.
/// - The 16-byte UL is copied verbatim; `edits.universal_label` and
///   `edits.declared_version` are ignored.
/// - `edits.unknown` re-encodes those tags with the given value bytes
///   (the escape hatch for tags outside the typed model); naming a
///   typed/reserved tag there is rejected with
///   [`KlvEncodeError::ReservedTagInUnknown`].
/// - Tag 65 is NOT auto-injected (unlike `encode_with`); deleting a
///   tag is not supported.
/// - **Exception:** Tag 115 (Control Command,
///   `UasDatalinkLs::control_commands: Vec<..>`) and Tag 102 (SDCC-FLP,
///   `UasDatalinkLs::sdcc_flps: Vec<..>`) are MULTI-INSTANCE and are not
///   patchable — `encode_tag_value` cannot represent a multi-value field
///   as a single TLV, so `edits.control_commands` / `edits.sdcc_flps`
///   are ignored and any existing Tag 115 / Tag 102 occurrences are
///   copied verbatim.
///
/// # Errors
/// [`KlvPatchError::Decode`] when the input cannot be walked
/// (truncated / malformed tag / malformed length);
/// [`KlvPatchError::Encode`] when an edited value cannot be encoded
/// (out of range, string too long, reserved tag in `unknown`).
pub fn patch(raw: &[u8], edits: &UasDatalinkLs) -> Result<Vec<u8>, KlvPatchError> {
    // Reject typed/reserved tags smuggled through `edits.unknown` up
    // front — same contract as `write_unknown_fields` in encode.
    for f in &edits.unknown {
        if TAGS.iter().any(|s| u32::from(s.id) == f.tag) {
            return Err(KlvPatchError::Encode(
                KlvEncodeError::ReservedTagInUnknown { tag: f.tag },
            ));
        }
    }

    // ---- outer envelope: UL + BER length ----
    if raw.len() < 16 {
        return Err(KlvPatchError::Decode(KlvDecodeError::Truncated {
            offset: 0,
            needed: 16,
            have: raw.len(),
        }));
    }
    let ul = &raw[..16];
    let (declared_len, after_len) = read_ber(&raw[16..]).map_err(|e| rebased(e, 16))?;
    let len_bytes = &raw[16..raw.len() - after_len.len()];
    let body_offset = raw.len() - after_len.len();
    if after_len.len() < declared_len {
        return Err(KlvPatchError::Decode(KlvDecodeError::Truncated {
            offset: body_offset,
            needed: declared_len,
            have: after_len.len(),
        }));
    }
    let body = &after_len[..declared_len];

    // ---- single offset-tracked pass over the body TLVs ----
    // Manual walk (not `pack::Iter`): verbatim copy needs the original
    // byte spans, non-canonical length encodings included.
    let mut new_body: Vec<u8> = Vec::with_capacity(body.len() + 64);
    // Linear-scan Vec: real local sets carry at most a few dozen
    // distinct tags, so the O(n²) worst case only degrades on
    // adversarial (fuzz) inputs — not a hot path.
    let mut seen: Vec<u32> = Vec::new();
    // (offset in new_body, header byte count) of the LAST tag-1 TLV;
    // its 2-byte value is recomputed after assembly.
    let mut checksum_slot: Option<(usize, usize)> = None;
    // Original header bytes of a checksum that is the FINAL body TLV —
    // held out so inserted tags land before it.
    let mut trailing_checksum: Option<&[u8]> = None;

    let mut pos = 0usize;
    while pos < body.len() {
        let rest = &body[pos..];
        let (tag, after_tag) = read_ber_oid(rest).map_err(|e| rebased(e, body_offset + pos))?;
        let consumed_tag = rest.len() - after_tag.len();
        let (vlen, after_vlen) =
            read_ber(after_tag).map_err(|e| rebased(e, body_offset + pos + consumed_tag))?;
        let header_len = rest.len() - after_vlen.len();
        if after_vlen.len() < vlen {
            return Err(KlvPatchError::Decode(KlvDecodeError::Truncated {
                offset: body_offset + pos + header_len,
                needed: vlen,
                have: after_vlen.len(),
            }));
        }
        let tlv = &rest[..header_len + vlen];
        if !seen.contains(&tag) {
            seen.push(tag);
        }

        if tag == 1 {
            // Mirror decode_inner: a checksum value must be 2 bytes.
            if vlen != 2 {
                return Err(KlvPatchError::Decode(KlvDecodeError::Truncated {
                    offset: body_offset + pos + header_len,
                    needed: 2,
                    have: vlen,
                }));
            }
            if pos + header_len + vlen == body.len() {
                trailing_checksum = Some(&tlv[..header_len]);
            } else {
                // Mid-body checksum: non-compliant but tolerated (like
                // lenient decode) — copied verbatim for now; if this
                // turns out to be the LAST tag-1, the recompute below
                // overwrites its value in place. Net contract: every
                // non-last tag-1 is verbatim, only the last recomputed.
                checksum_slot = Some((new_body.len(), header_len));
                new_body.extend_from_slice(&tlv[..header_len]);
                new_body.extend_from_slice(&tlv[header_len..]);
            }
        } else if let Some(spec) = TAGS.iter().find(|s| u32::from(s.id) == tag) {
            // patch keeps strict range behavior; policy is an encode-entry option
            match encode_tag_value(edits, spec, None, OutOfRangePolicy::Error)? {
                Some(value) => emit_tlv(tag, &value, &mut new_body)?,
                None => new_body.extend_from_slice(tlv),
            }
        } else if let Some(f) = edits.unknown.iter().find(|f| f.tag == tag) {
            emit_tlv(tag, &f.value, &mut new_body)?;
        } else {
            new_body.extend_from_slice(tlv);
        }
        pos += header_len + vlen;
    }

    // ---- insert edited-but-absent tags ----
    for spec in TAGS {
        if spec.id == 1 || seen.contains(&u32::from(spec.id)) {
            continue;
        }
        // patch keeps strict range behavior; policy is an encode-entry option
        if let Some(value) = encode_tag_value(edits, spec, None, OutOfRangePolicy::Error)? {
            emit_tlv(u32::from(spec.id), &value, &mut new_body)?;
        }
    }
    for f in &edits.unknown {
        if !seen.contains(&f.tag) {
            emit_tlv(f.tag, &f.value, &mut new_body)?;
        }
    }
    if let Some(header) = trailing_checksum {
        checksum_slot = Some((new_body.len(), header.len()));
        new_body.extend_from_slice(header);
        new_body.extend_from_slice(&[0, 0]);
    }

    // ---- assemble: UL + outer length + body + trailing bytes ----
    let trailing = &after_len[declared_len..];
    let mut out: Vec<u8> =
        Vec::with_capacity(16 + len_bytes.len() + new_body.len() + trailing.len());
    out.extend_from_slice(ul);
    if new_body.len() == declared_len {
        // Body size unchanged: preserve the original outer length
        // bytes verbatim (non-canonical forms included).
        out.extend_from_slice(len_bytes);
    } else {
        let mut len_buf = [0u8; 16];
        let n = write_ber(new_body.len(), &mut len_buf)?;
        out.extend_from_slice(&len_buf[..n]);
    }
    let body_start = out.len();
    out.extend_from_slice(&new_body);
    // Bytes after the declared outer length (capture padding etc.) are
    // preserved verbatim — they sit after the body, outside checksum
    // coverage (the sum only runs through the checksum value offset).
    out.extend_from_slice(trailing);

    // ---- recompute the (last) checksum, if the input had one ----
    if let Some((slot, header_len)) = checksum_slot {
        let value_off = body_start + slot + header_len;
        let sum = checksum_running_sum_16(&out[..value_off]);
        out[value_off] = (sum >> 8) as u8;
        out[value_off + 1] = sum as u8;
    }
    Ok(out)
}
