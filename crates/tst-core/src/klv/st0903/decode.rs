//! ST 0903 decode entry points (`decode`, `decode_strict`) and private
//! helpers (`decode_vtarget_series`, `decode_vtarget_series_strict`).

use crate::error::{KlvDecodeError, KlvFieldError};
use crate::klv::pack::OwnedRawField;
use crate::klv::st0903::model::VmtiLs;
use crate::klv::st0903::tags::{Encoding, TAGS, lookup};
use crate::klv::st0903::{rules, vtarget_pack};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::ops::Range;

/// Lenient decode of a VMTI Local Set body per ST 0903.6 §10.1.
///
/// Tolerates structurally recoverable malformations: per-field byte-length
/// mismatches, malformed UTF-8, malformed IMAPB ranges, and truncated value
/// bytes are accumulated in [`VmtiLs::field_errors`] without aborting the
/// walk. Tags not in the §10.1 schema (unknown / deprecated, e.g. Tag 7)
/// are preserved in [`VmtiLs::unknown`] per ST 0107.5 §6 future-proof skip
/// rule. A zero-length item means "unknown" (ST 0107.4-17): its field
/// is left unset, with no field error; one NUL is the empty string
/// (ST 0107.5 §6.3.3.2). Inside a VTarget pack this is not so yet: a
/// zero-length item of fixed length there is a field error of the pack.
///
/// Tags are walked as BER-OID per ST 0107.5 §6.3.1 — defined tags 1..=103
/// fit in one byte (`write_ber_oid(N) == [N]` for `N ≤ 127`), but a future
/// ST 0903.7+ tag ≥ 128 would encode as multi-byte BER-OID. Such
/// forward-compat tags land in [`VmtiLs::unknown`] keyed by their decoded
/// `u32` tag value.
///
/// VTargetSeries (Tag 101) inner packs are dispatched to
/// `vtarget_pack::read_pack`; pack-level errors land in `field_errors`
/// as [`KlvDecodeError::St0903InvalidVTargetPack`] reasons too — but
/// surfaced via the parent-level [`KlvFieldError`] channel (see
/// `decode_vtarget_series`).
///
/// Use [`decode_strict`] for spec-validation use cases that
/// reject any of the above.
///
/// # Errors
/// Returns `Err(KlvDecodeError)` only for unrecoverable framing
/// failures inside [`VTargetSeries`][VmtiLs::targets] (Tag 101) — a
/// VTargetPack header that cannot be parsed surfaces as
/// [`KlvDecodeError::St0903InvalidVTargetPack`]. Tag-level
/// malformations on the parent LS are non-fatal in lenient mode and
/// are captured in [`VmtiLs::field_errors`] instead.
pub fn decode(bytes: &[u8]) -> Result<VmtiLs, KlvDecodeError> {
    use crate::klv::imapb::{DecodedImapb, ImapbParams, decode_imapb};
    use crate::klv::pack::Iter;

    let mut ls = VmtiLs::default();

    for r in Iter::local_set(bytes) {
        // Framing failure (truncated tag / length / value): surface
        // the partial state to the caller as a final field_error and
        // stop. `Iter` already advances `finished` so subsequent
        // calls return None — but matching `break` is clearer.
        let f = match r {
            Ok(f) => f,
            Err(_) => {
                ls.field_errors
                    .push(KlvFieldError::TruncatedField { tag: 0 });
                break;
            }
        };
        let value = f.value;

        // BER-OID-decoded tag arrives as u32. ST 0903.6 §10.1 typed
        // tags all fit in u8 (highest is 103); narrow only for the
        // table lookup. Tags > 0xFF (future-spec multi-byte BER-OID)
        // are preserved as forward-compat unknowns per ST 0107.5 §6,
        // matching the sibling ST 0601 / ST 0102 lenient pattern.
        let Ok(tag) = u8::try_from(f.tag) else {
            ls.unknown.push(OwnedRawField {
                tag: f.tag,
                value: value.to_vec(),
            });
            continue;
        };

        let Some(spec) = lookup(tag) else {
            // Unknown / deprecated tag — preserve per ST 0107.5 §6.
            ls.unknown.push(OwnedRawField {
                tag: tag as u32,
                value: value.to_vec(),
            });
            continue;
        };

        // ST 0107.4-17: a zero-length item means "value unknown". It is
        // an occurrence of its tag like any other, so as the last one it
        // leaves the field unset.
        if value.is_empty() {
            clear_item(&mut ls, tag);
            continue;
        }

        match spec.encoding {
            Encoding::U16Be => {
                if value.len() != 2 {
                    ls.field_errors.push(KlvFieldError::InvalidLength {
                        tag: tag as u32,
                        expected: 2,
                        got: value.len(),
                    });
                    continue;
                }
                let v = u16::from_be_bytes([value[0], value[1]]);
                debug_assert_eq!(tag, 1, "U16Be reserved for tag 1 (Checksum)");
                ls.checksum = Some(v);
            }
            Encoding::U64Be => {
                if value.len() != 8 {
                    ls.field_errors.push(KlvFieldError::InvalidLength {
                        tag: tag as u32,
                        expected: 8,
                        got: value.len(),
                    });
                    continue;
                }
                // Defense-in-depth: the upstream length check above
                // makes this `try_into` infallible today, but a future
                // tag-table refactor that decouples encoding from
                // length could reach this with a wrong-sized slice.
                // Surface as TruncatedField via field_errors rather
                // than panic.
                let arr: [u8; 8] = match value.try_into() {
                    Ok(a) => a,
                    Err(_) => {
                        ls.field_errors
                            .push(KlvFieldError::TruncatedField { tag: tag as u32 });
                        continue;
                    }
                };
                debug_assert_eq!(tag, 2, "U64Be reserved for tag 2 (Precision Time Stamp)");
                ls.precision_time_stamp = Some(u64::from_be_bytes(arr));
            }
            Encoding::VarUint { max_bytes } => {
                if value.is_empty() || value.len() > max_bytes as usize {
                    ls.field_errors.push(KlvFieldError::InvalidLength {
                        tag: tag as u32,
                        expected: max_bytes as usize,
                        got: value.len(),
                    });
                    continue;
                }
                let v = match crate::klv::length::read_var_uint(
                    value,
                    max_bytes as usize,
                    tag as u32,
                ) {
                    Ok(v) => v as u32,
                    Err(_) => {
                        ls.field_errors
                            .push(KlvFieldError::TruncatedField { tag: tag as u32 });
                        continue;
                    }
                };
                match tag {
                    4 => ls.version_number = Some(v as u16), // V2 caps at u16
                    5 => ls.total_targets_in_frame = Some(v),
                    6 => ls.num_targets_reported = Some(v),
                    8 => ls.frame_width = Some(v),
                    9 => ls.frame_height = Some(v),
                    _ => unreachable!("VarUint dispatch missing tag {tag}"),
                }
            }
            Encoding::Utf8 { max_chars } => {
                let s = match core::str::from_utf8(value) {
                    Ok(s) => s,
                    Err(_) => {
                        ls.field_errors
                            .push(KlvFieldError::InvalidUtf8 { tag: tag as u32 });
                        continue;
                    }
                };
                // §10.1.3 / §10.1.10 char caps (V32 / V128 — characters,
                // not bytes; UTF-8 expansion may exceed the byte count).
                // Lenient: surface a field_error if too long, but keep
                // the value (strict mode rejects).
                if s.chars().count() > max_chars {
                    ls.field_errors.push(KlvFieldError::InvalidLength {
                        tag: tag as u32,
                        expected: max_chars,
                        got: s.chars().count(),
                    });
                }
                let owned = string_item(s);
                match tag {
                    3 => ls.vmti_system_name = Some(owned),
                    10 => ls.source_sensor = Some(owned),
                    _ => unreachable!("Utf8 dispatch missing tag {tag}"),
                }
            }
            Encoding::ImapbF64 { min, max } => {
                // Top-level Tags 11 + 12 (Horizontal/Vertical FOV) are
                // IMAPB(0, 180, 2) per ST 0903.6 §10.1.11 + §10.1.12 —
                // both fixed length 2. Hardcode length=2 to match the
                // spec's wire shape: a value of any other length is an
                // `InvalidLength` field error here (`decode_imapb` itself
                // refuses a length outside 1..=8 and a byte count that
                // differs from it). Mirrors the per-tag hardcoded-length
                // pattern in `vtarget_pack::decode_field`.
                let expected_len = 2;
                if value.len() != expected_len {
                    ls.field_errors.push(KlvFieldError::InvalidLength {
                        tag: tag as u32,
                        expected: expected_len,
                        got: value.len(),
                    });
                    continue;
                }
                let params = ImapbParams {
                    min,
                    max,
                    length: expected_len,
                };
                // ST 0903.6 §9.1.1: an ST 1201 special (an out-of-range
                // signal, "Invalid") is sent in place of the measurement:
                // a legal value that leaves the field unset. A reserved
                // pattern or an out-of-range result is a field error.
                let v = match decode_imapb(&params, value) {
                    Ok(DecodedImapb::Value(v)) => Some(v),
                    Ok(DecodedImapb::Special(_)) => None,
                    _ => {
                        ls.field_errors.push(KlvFieldError::InvalidLength {
                            tag: tag as u32,
                            expected: expected_len,
                            got: value.len(),
                        });
                        continue;
                    }
                };
                match tag {
                    11 => ls.horizontal_fov = v,
                    12 => ls.vertical_fov = v,
                    _ => unreachable!("ImapbF64 dispatch missing tag {tag}"),
                }
            }
            Encoding::RawBytes => match tag {
                13 => ls.miis_id = Some(value.to_vec()),
                101 => {
                    ls.targets = decode_vtarget_series(value, &mut ls.field_errors);
                }
                102 => ls.algorithm_series = Some(value.to_vec()),
                103 => ls.ontology_series = Some(value.to_vec()),
                _ => unreachable!("RawBytes dispatch missing tag {tag}"),
            },
        }
    }

    Ok(ls)
}

/// The value of a string item: one NUL is the empty string (ST 0107.5
/// §6.3.3.2).
fn string_item(s: &str) -> String {
    if s == "\0" {
        String::new()
    } else {
        s.to_string()
    }
}

/// Unset the field of the typed top-level item `tag`.
fn clear_item(ls: &mut VmtiLs, tag: u8) {
    match tag {
        1 => ls.checksum = None,
        2 => ls.precision_time_stamp = None,
        3 => ls.vmti_system_name = None,
        4 => ls.version_number = None,
        5 => ls.total_targets_in_frame = None,
        6 => ls.num_targets_reported = None,
        8 => ls.frame_width = None,
        9 => ls.frame_height = None,
        10 => ls.source_sensor = None,
        11 => ls.horizontal_fov = None,
        12 => ls.vertical_fov = None,
        13 => ls.miis_id = None,
        101 => ls.targets.clear(),
        102 => ls.algorithm_series = None,
        103 => ls.ontology_series = None,
        _ => unreachable!("clear_item missing typed tag {tag}"),
    }
}

/// Walk a VTargetSeries (Tag 101) payload, dispatching each
/// BER-length-prefixed pack to [`vtarget_pack::read_pack`]. Pack-level
/// failures are recorded in `field_errors` as `OutOfRange` placeholders
/// keyed to tag 101 (until/unless a future variant carrying the typed
/// `VTargetPackError` lands in `KlvFieldError`).
///
/// Series framing failures (truncated outer BER length, value-length
/// overrun) abort the walk and emit a single `TruncatedField { tag: 101 }`.
fn decode_vtarget_series(
    series_bytes: &[u8],
    field_errors: &mut Vec<KlvFieldError>,
) -> Vec<vtarget_pack::VTargetPack> {
    use crate::klv::length::read_ber;

    let mut targets = Vec::new();
    let mut cursor = series_bytes;
    while !cursor.is_empty() {
        let (pack_len, after_len) = match read_ber(cursor) {
            Ok(x) => x,
            Err(_) => {
                field_errors.push(KlvFieldError::TruncatedField { tag: 101 });
                break;
            }
        };
        cursor = after_len;
        if cursor.len() < pack_len {
            field_errors.push(KlvFieldError::TruncatedField { tag: 101 });
            break;
        }
        let pack_bytes = &cursor[..pack_len];
        cursor = &cursor[pack_len..];

        match vtarget_pack::read_pack(pack_bytes) {
            Ok((pack, _)) => targets.push(pack),
            Err(_) => {
                // `read_pack` is itself lenient: a
                // per-field value error lands in the pack's own
                // `field_errors` and this arm never sees it. Only a
                // pack-level FRAMING failure (truncated target ID,
                // length overrun) reaches here. We don't have a typed
                // `KlvFieldError` arm carrying the `VTargetPackError`
                // shape, so signal via a generic field error keyed to
                // tag 101. Strict-mode will route the typed error via
                // `KlvDecodeError::St0903InvalidVTargetPack`.
                field_errors.push(KlvFieldError::TruncatedField { tag: 101 });
            }
        }
    }
    targets
}

/// Strict decode of a VMTI Local Set body per ST 0903.6 §10.1.
///
/// Rejects spec-violating input rather than recovering: missing
/// unconditionally-required tags (Tags 4 and 6 per ST 0903.5-99 +
/// ST 0903.4-19), duplicate tags, malformed UTF-8, IMAPB length
/// mismatches, VarUint overflow, U16Be / U64Be wrong width, BER framing failures, and pack-level malformations
/// (routed via [`KlvDecodeError::St0903InvalidVTargetPack`]).
///
/// A zero-length item means "unknown" (ST 0107.4-17): accepted for an
/// optional item, and a required item sent so is missing. Inside a
/// VTarget pack a zero-length item of fixed length is refused instead.
///
/// **Record rules** (as the strict encoders apply them): no zero where an
/// item's range starts at one, a pixel number needs the frame width, a
/// row needs its column and the reverse, an Active-Moving or
/// Active-Stopped target needs a position; each refusal is
/// [`KlvDecodeError::ComplianceViolation`] at the pack's or the item's
/// offset. The strict encoders refuse the same records, but for a record
/// with several defects the two may name different ones.
///
/// An IMAP special decodes to an unset field, so the strict encoders can
/// refuse a record this function accepted, when a special was the only
/// value of a mandatory item or of its pack (`MissingMandatoryItem`,
/// `VTargetPackEmpty`): a relay forwards the original bytes.
///
/// Unknown tags are still preserved in [`VmtiLs::unknown`] per
/// ST 0107.5 §6 future-proof skip rule (strict mode is about
/// codepoint legality, not future-spec rejection).
///
/// **Conditional requirements not enforced.** ST 0903.6 marks several
/// tags as required only for specific carriage paths (Tag 1 Checksum
/// required for standalone-VMTI per ST 0903.6-119, prohibited for
/// embedded per -120; Tag 2 PTS required for standalone per -117;
/// Tags 11/12/13 conditional). `decode_strict` does NOT enforce these
/// rules: [`decode_standalone_strict`] does, for a standalone packet.
///
/// **UTF-8 char-cap.** Strict rejects strings exceeding the spec's
/// `max_chars` (Tag 3 V32, Tag 10 V128). Lenient accepts but surfaces
/// a `field_error`.
///
/// # Errors
/// - [`KlvDecodeError::DuplicateTag`] if any tag appears more than
///   once.
/// - [`KlvDecodeError::Truncated`] if a TLV's declared length runs
///   past the end of the buffer.
/// - [`KlvDecodeError::St0903MissingRequiredTag`] if a spec-required
///   tag (4 or 6 per ST 0903.6) is absent.
/// - [`KlvDecodeError::St0903InvalidVTargetPack`] for unrecoverable
///   pack-internal malformations under VTargetSeries (Tag 101).
/// - [`KlvDecodeError::FieldError`] wrapping a [`KlvFieldError`] for
///   any per-tag value validation failure (length / range / UTF-8 /
///   IMAPB / codepoint).
/// - [`KlvDecodeError::NonCanonicalLength`] if a TLV uses a non-
///   canonical BER length encoding.
pub fn decode_strict(bytes: &[u8]) -> Result<VmtiLs, KlvDecodeError> {
    decode_strict_at(bytes, 0).map(|body| body.ls)
}

/// A body strict decode accepted, with what the standalone rules read.
struct StrictBody {
    ls: VmtiLs,
    /// The top-level items on the wire with a value.
    present: [bool; 256],
    /// Where the top-level items a record rule can name start.
    at: [usize; 14],
    /// Where each pack of the series lies.
    packs: Vec<Range<usize>>,
}

/// [`decode_strict`] of `bytes`, which start at `base` in the caller's
/// buffer: every offset it reports is the caller's.
fn decode_strict_at(bytes: &[u8], base: usize) -> Result<StrictBody, KlvDecodeError> {
    use crate::klv::imapb::{DecodedImapb, ImapbParams, decode_imapb};
    use crate::klv::length::read_strict_tlv;

    let mut ls = VmtiLs::default();
    // BER-OID dedup must run on u32 (future ST 0903.7+ tags ≥ 128 will
    // encode as multi-byte BER-OID; using `[bool; 256]` would silently
    // collide tag 128 with tag 0). Mirrors the ST 0601 strict walker
    // which gates dedup on `u8::try_from(tag)` against the typed table
    // but tracks all tag IDs in a `Vec<u32>`. ST 0903's strict dispatch
    // is single-pass so we keep both checks inline.
    let mut seen_u8 = [false; 256];
    // An item is present when it is on the wire with a value; `seen_u8`
    // also counts a zero-length one, for the duplicate check.
    let mut present = [false; 256];
    let mut seen_other: hashbrown::HashSet<u32> = hashbrown::HashSet::new();
    // Where the top-level items a record rule can name start (4, 8, 9,
    // 13), and where each pack of the series starts.
    let mut at = [0usize; 14];
    let mut packs = Vec::new();
    let mut cursor = bytes;
    // `offset` tracks the start of the current item in `bytes` so
    // framing errors and duplicate-tag errors surface a useful
    // `offset` field (the ST 0601 strict walker uses the same scheme).
    let mut offset = base;

    while !cursor.is_empty() {
        let item_start = offset;
        let (tag, declared_len, consumed, after_len) = read_strict_tlv(cursor, item_start)?;

        // Duplicate-tag check on the BER-OID-decoded u32 — covers both
        // the typed u8 universe (tags 1..=103) and future multi-byte
        // BER-OID extensions per ST 0107.5 §6. Runs after the framing
        // read (matching the ST 0601 walker) — a duplicate tag whose
        // length field is also malformed surfaces the framing error.
        let is_dup = if let Ok(tag_u8) = u8::try_from(tag) {
            let was = seen_u8[tag_u8 as usize];
            seen_u8[tag_u8 as usize] = true;
            was
        } else {
            !seen_other.insert(tag)
        };
        if is_dup {
            return Err(KlvDecodeError::DuplicateTag {
                tag,
                offset: item_start,
            });
        }

        if after_len.len() < declared_len {
            return Err(KlvDecodeError::Truncated {
                offset: item_start + consumed,
                needed: declared_len,
                have: after_len.len(),
            });
        }
        let value = &after_len[..declared_len];
        cursor = &after_len[declared_len..];
        offset = item_start + consumed + declared_len;

        // Tags > 0xFF are preserved as forward-compat unknowns (ST
        // 0107.5 §6); the typed table only covers the u8 universe.
        let Ok(tag) = u8::try_from(tag) else {
            ls.unknown.push(OwnedRawField {
                tag,
                value: value.to_vec(),
            });
            continue;
        };

        let Some(spec) = lookup(tag) else {
            // ST 0107.5 §6 skip rule — preserve unknown tags.
            ls.unknown.push(OwnedRawField {
                tag: tag as u32,
                value: value.to_vec(),
            });
            continue;
        };

        // ST 0107.4-17: a zero-length item means "value unknown": the
        // field stays unset and the item is not present for the
        // required-item check.
        if value.is_empty() {
            continue;
        }
        present[tag as usize] = true;
        if let Some(slot) = at.get_mut(tag as usize) {
            *slot = item_start;
        }

        match spec.encoding {
            Encoding::U16Be => {
                if value.len() != 2 {
                    return Err(KlvDecodeError::FieldError(KlvFieldError::InvalidLength {
                        tag: tag as u32,
                        expected: 2,
                        got: value.len(),
                    }));
                }
                debug_assert_eq!(tag, 1, "U16Be reserved for tag 1 (Checksum)");
                ls.checksum = Some(u16::from_be_bytes([value[0], value[1]]));
            }
            Encoding::U64Be => {
                if value.len() != 8 {
                    return Err(KlvDecodeError::FieldError(KlvFieldError::InvalidLength {
                        tag: tag as u32,
                        expected: 8,
                        got: value.len(),
                    }));
                }
                debug_assert_eq!(tag, 2, "U64Be reserved for tag 2 (Precision Time Stamp)");
                // Defense-in-depth: the upstream length check above
                // makes this `try_into` infallible today, but a future
                // tag-table refactor that decouples encoding from
                // length could reach this with a wrong-sized slice.
                // Surface as TruncatedField rather than panic.
                let arr: [u8; 8] = value.try_into().map_err(|_| {
                    KlvDecodeError::FieldError(KlvFieldError::TruncatedField { tag: tag as u32 })
                })?;
                ls.precision_time_stamp = Some(u64::from_be_bytes(arr));
            }
            Encoding::VarUint { max_bytes } => {
                if value.is_empty() || value.len() > max_bytes as usize {
                    return Err(KlvDecodeError::FieldError(KlvFieldError::InvalidLength {
                        tag: tag as u32,
                        expected: max_bytes as usize,
                        got: value.len(),
                    }));
                }
                let v = crate::klv::length::read_var_uint(value, max_bytes as usize, tag as u32)
                    .map(|v| v as u32)
                    .map_err(KlvDecodeError::FieldError)?;
                match tag {
                    4 => ls.version_number = Some(v as u16), // V2 caps at u16
                    5 => ls.total_targets_in_frame = Some(v),
                    6 => ls.num_targets_reported = Some(v),
                    8 => ls.frame_width = Some(v),
                    9 => ls.frame_height = Some(v),
                    _ => unreachable!("VarUint dispatch missing tag {tag}"),
                }
            }
            Encoding::Utf8 { max_chars } => {
                let s = core::str::from_utf8(value).map_err(|_| {
                    KlvDecodeError::FieldError(KlvFieldError::InvalidUtf8 { tag: tag as u32 })
                })?;
                if s.chars().count() > max_chars {
                    return Err(KlvDecodeError::FieldError(KlvFieldError::InvalidLength {
                        tag: tag as u32,
                        expected: max_chars,
                        got: s.chars().count(),
                    }));
                }
                let owned = string_item(s);
                match tag {
                    3 => ls.vmti_system_name = Some(owned),
                    10 => ls.source_sensor = Some(owned),
                    _ => unreachable!("Utf8 dispatch missing tag {tag}"),
                }
            }
            Encoding::ImapbF64 { min, max } => {
                // Top-level Tags 11 + 12 are IMAPB(0, 180, 2) per
                // ST 0903.6 §10.1.11 + §10.1.12 — both fixed length 2.
                let expected_len = 2;
                if value.len() != expected_len {
                    return Err(KlvDecodeError::FieldError(KlvFieldError::InvalidLength {
                        tag: tag as u32,
                        expected: expected_len,
                        got: value.len(),
                    }));
                }
                let params = ImapbParams {
                    min,
                    max,
                    length: expected_len,
                };
                // ST 0903.6 §9.1.1: an ST 1201 special is a legal value
                // that leaves the field unset. A reserved pattern or an
                // out-of-range result is refused as `InvalidLength` (the
                // nearest existing error).
                let v = match decode_imapb(&params, value).map_err(KlvDecodeError::FieldError)? {
                    DecodedImapb::Value(v) => Some(v),
                    DecodedImapb::Special(_) => None,
                    _ => {
                        return Err(KlvDecodeError::FieldError(KlvFieldError::InvalidLength {
                            tag: tag as u32,
                            expected: expected_len,
                            got: value.len(),
                        }));
                    }
                };
                match tag {
                    11 => ls.horizontal_fov = v,
                    12 => ls.vertical_fov = v,
                    _ => unreachable!("ImapbF64 dispatch missing tag {tag}"),
                }
            }
            Encoding::RawBytes => match tag {
                13 => ls.miis_id = Some(value.to_vec()),
                101 => {
                    // `item_start + consumed` = buffer offset of `value[0]`,
                    // so series-internal errors report buffer-absolute
                    // offsets.
                    (ls.targets, packs) =
                        decode_vtarget_series_strict(value, item_start + consumed)?;
                }
                102 => ls.algorithm_series = Some(value.to_vec()),
                103 => ls.ontology_series = Some(value.to_vec()),
                _ => unreachable!("RawBytes dispatch missing tag {tag}"),
            },
        }
    }

    // The record rules, after every item's own checks and before the
    // required items.
    if let Some(v) = rules::first_violation(&ls) {
        return Err(compliance_violation(v, &at, &packs));
    }

    // Required-tag validation: required = {4, 6}.
    for spec in TAGS {
        if spec.required && !present[spec.id as usize] {
            return Err(KlvDecodeError::St0903MissingRequiredTag { tag: spec.id });
        }
    }

    Ok(StrictBody {
        ls,
        present,
        at,
        packs,
    })
}

/// The strict decoders' error for a record rule, at the start of the pack
/// or the top-level item it names.
fn compliance_violation(
    v: rules::Violation,
    at: &[usize; 14],
    packs: &[Range<usize>],
) -> KlvDecodeError {
    let offset = match v.target {
        Some(i) => packs[i].start,
        None => at[v.tag as usize],
    };
    let reason = match v.rule {
        rules::Rule::Zero { .. } => rules::ZERO,
        rules::Rule::Relation(reason) => reason,
    };
    KlvDecodeError::ComplianceViolation {
        tag: v.tag,
        offset,
        reason,
    }
}

/// Strict variant of [`decode_vtarget_series`]: framing failures and
/// pack-level malformations abort with an `Err`. The pack-level error
/// is routed via the typed [`KlvDecodeError::St0903InvalidVTargetPack`]
/// arm carrying the underlying [`VTargetPackError`]. `base` is the
/// buffer offset of `series_bytes[0]`; every offset this function
/// surfaces is buffer-absolute. Returns the packs and where each lies.
fn decode_vtarget_series_strict(
    series_bytes: &[u8],
    base: usize,
) -> Result<(Vec<vtarget_pack::VTargetPack>, Vec<Range<usize>>), KlvDecodeError> {
    use crate::klv::length::{read_ber_strict, rebase_offset};

    let mut targets = Vec::new();
    let mut offsets = Vec::new();
    // ST 0903.6-126: a targetId once per series. A set, so a long series
    // costs no scan per pack.
    let mut ids = alloc::collections::BTreeSet::new();
    let mut cursor = series_bytes;
    let mut offset = base;
    while !cursor.is_empty() {
        let before_len = cursor.len();
        let (pack_len, after_len) = read_ber_strict(cursor).map_err(|mut e| {
            // `read_ber_strict` reports 0 / 1 relative to `cursor` — rebase
            // onto the pack's buffer position.
            rebase_offset(&mut e, offset);
            e
        })?;
        let len_consumed = before_len - after_len.len();
        cursor = after_len;
        offset += len_consumed;
        if cursor.len() < pack_len {
            return Err(KlvDecodeError::Truncated {
                offset,
                needed: pack_len,
                have: cursor.len(),
            });
        }
        let pack_bytes = &cursor[..pack_len];
        cursor = &cursor[pack_len..];

        let (pack, _) = vtarget_pack::read_pack_strict(pack_bytes)
            .map_err(|reason| KlvDecodeError::St0903InvalidVTargetPack { offset, reason })?;
        if !ids.insert(pack.target_id) {
            return Err(KlvDecodeError::St0903InvalidVTargetPack {
                offset,
                reason: vtarget_pack::VTargetPackError::DuplicateTargetId {
                    target_id: pack.target_id,
                },
            });
        }
        targets.push(pack);
        offsets.push(offset..offset + pack_len);
        offset += pack_len;
    }
    Ok((targets, offsets))
}

/// Decode a standalone VMTI packet, `[VMTI_LS_UL][BER length][body]`, and
/// verify its checksum (ST 0903.4-15, -17, ST 0903.6-119): the body must
/// hold exactly one Tag 1, as its last item, two bytes wide, equal to the
/// running 16-bit sum of every byte from the label up to that value. Then
/// the body is decoded as [`decode`] does. Bytes after the declared
/// length are not read.
///
/// This is the decode twin of [`super::encode_to_vec_standalone`]; for an
/// embedded set (no label, no checksum) use [`decode`].
///
/// # Errors
/// - [`KlvDecodeError::Truncated`] if the packet is shorter than its label
///   or than its declared length, or an item runs past the body.
/// - [`KlvDecodeError::UnexpectedUniversalLabel`] if the label is not
///   [`super::VMTI_LS_UL`].
/// - [`KlvDecodeError::MissingChecksum`] (the body's offset),
///   [`KlvDecodeError::MalformedChecksum`] (a Tag 1 not two bytes wide, at
///   its value), [`KlvDecodeError::DuplicateTag`] with tag 1 (a second
///   one, at its start), [`KlvDecodeError::Tag1NotLast`] and
///   [`KlvDecodeError::ChecksumMismatch`], judged in that order.
/// - [`KlvDecodeError::NonCanonicalTag`] at the item's start for a Tag 1
///   written with a padding group (`80 01`): a BER-OID never starts with
///   `0x80` (ST 0107.5 §6.3.1), so it is no checksum item.
pub fn decode_standalone(bytes: &[u8]) -> Result<VmtiLs, KlvDecodeError> {
    let (body, _) = standalone_body(bytes, false)?;
    decode(body)
}

/// [`decode_standalone`] with every rule of [`decode_strict`] on the body,
/// then the rules of standalone carriage: Items 2, 11, 12 and 13 present
/// (ST 0903.6-117, -122, -123, -125; present means on the wire with a
/// value, so an IMAP special in Item 11 or 12 counts), no
/// parent-relative offset item in a pack (Items 10, 11, 13 to 16,
/// ST 0903.6-116, whatever value they hold), a target that is active or
/// coasting has a location (ST 0903.6-130), and the MIIS Core Identifier
/// is one ST 1204 accepts (§10.1.13). These mirror
/// [`super::encode_standalone_strict_compliance`]. The outer length must
/// take its fewest bytes (ST 0107.5 §6.3.2; [`KlvDecodeError::NonCanonicalLength`]),
/// as `st0601::decode_strict_compliance` reads it.
///
/// # Errors
/// Those of [`decode_standalone`] and [`decode_strict`], plus
/// [`KlvDecodeError::St0903MissingRequiredTag`] for a missing standalone
/// item and [`KlvDecodeError::ComplianceViolation`] for the other rules.
pub fn decode_standalone_strict(bytes: &[u8]) -> Result<VmtiLs, KlvDecodeError> {
    let (body, base) = standalone_body(bytes, true)?;
    let strict = decode_strict_at(body, base)?;
    for tag in [2u8, 11, 12, 13] {
        if !strict.present[usize::from(tag)] {
            return Err(KlvDecodeError::St0903MissingRequiredTag { tag });
        }
    }
    for pack in &strict.packs {
        if let Some(tag) = vtarget_pack::first_standalone_offset_item(&bytes[pack.clone()]) {
            return Err(KlvDecodeError::ComplianceViolation {
                tag,
                offset: pack.start,
                reason: "standalone carriage omits a pack's parent-relative offsets (ST 0903.6-116)",
            });
        }
    }
    if let Some(v) = rules::first_standalone_violation(&strict.ls) {
        return Err(compliance_violation(v, &strict.at, &strict.packs));
    }
    Ok(strict.ls)
}

/// The label, the body the outer length bounds, and the checksum of a
/// standalone packet (see [`decode_standalone`]). Returns the body and
/// where it starts in `bytes`.
fn standalone_body(bytes: &[u8], strict: bool) -> Result<(&[u8], usize), KlvDecodeError> {
    use crate::klv::UniversalLabel;
    use crate::klv::checksum::checksum_running_sum_16;
    use crate::klv::length::{read_ber, read_ber_strict, rebase_offset};

    let Some((label, rest)) = bytes.split_first_chunk::<16>() else {
        return Err(KlvDecodeError::Truncated {
            offset: 0,
            needed: 16,
            have: bytes.len(),
        });
    };
    if *label != super::VMTI_LS_UL {
        return Err(KlvDecodeError::UnexpectedUniversalLabel {
            expected: UniversalLabel::new(super::VMTI_LS_UL),
            found: UniversalLabel::new(*label),
        });
    }
    // Strict: the outer length in its fewest bytes (ST 0107.5 §6.3.2), as
    // `st0601::decode_strict_compliance` reads it.
    let outer = if strict {
        read_ber_strict(rest)
    } else {
        read_ber(rest)
    };
    let (declared, after_len) = outer.map_err(|mut e| {
        rebase_offset(&mut e, 16);
        e
    })?;
    let base = bytes.len() - after_len.len();
    let Some(body) = after_len.get(..declared) else {
        return Err(KlvDecodeError::Truncated {
            offset: base,
            needed: declared,
            have: after_len.len(),
        });
    };

    // Every Tag 1 is found before one is judged. The walk keeps where each
    // item starts (the iterator does not), and reads a tag of any width: a
    // wide one is no checksum.
    let mut first = None; // where the first one's value starts
    let mut malformed = None; // the first one not two bytes wide: value start, width
    let mut second = None; // where the second item starts
    let mut item_start = 0;
    while item_start < body.len() {
        let item = &body[item_start..];
        let Some(tag_len) = item.iter().position(|b| b & 0x80 == 0).map(|i| i + 1) else {
            return Err(KlvDecodeError::Truncated {
                offset: base + body.len(),
                needed: 1,
                have: 0,
            });
        };
        let tag = item[..tag_len]
            .iter()
            .fold(0u64, |t, b| t.saturating_mul(128) | u64::from(b & 0x7F));
        // A BER-OID never starts with 0x80 (ST 0107.5 §6.3.1): a padded
        // Tag 1 is no checksum item, and the decoders would not read it as
        // one alike (the iterator stops at a tag over five bytes).
        if tag == 1 && tag_len > 1 {
            return Err(KlvDecodeError::NonCanonicalTag {
                offset: base + item_start,
            });
        }
        let (len, value) = read_ber(&item[tag_len..]).map_err(|mut e| {
            rebase_offset(&mut e, base + item_start + tag_len);
            e
        })?;
        let value_at = body.len() - value.len();
        if value.len() < len {
            return Err(KlvDecodeError::Truncated {
                offset: base + value_at,
                needed: len,
                have: value.len(),
            });
        }
        if tag == 1 {
            if len != 2 && malformed.is_none() {
                malformed = Some((value_at, len));
            }
            if first.is_none() {
                first = Some(value_at);
            } else if second.is_none() {
                second = Some(item_start);
            }
        }
        item_start = value_at + len;
    }
    let Some(value_at) = first else {
        return Err(KlvDecodeError::MissingChecksum { offset: base });
    };
    if let Some((at, got)) = malformed {
        return Err(KlvDecodeError::MalformedChecksum {
            offset: base + at,
            got,
        });
    }
    if let Some(at) = second {
        return Err(KlvDecodeError::DuplicateTag {
            tag: 1,
            offset: base + at,
        });
    }
    if value_at + 2 != body.len() {
        return Err(KlvDecodeError::Tag1NotLast);
    }
    let expected = u16::from_be_bytes([body[value_at], body[value_at + 1]]);
    let found = checksum_running_sum_16(&bytes[..base + value_at]);
    if found != expected {
        return Err(KlvDecodeError::ChecksumMismatch { expected, found });
    }
    Ok((body, base))
}
