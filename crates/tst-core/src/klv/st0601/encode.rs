//! ST 0601 encode entry points — 5 public functions plus private helpers.

use crate::error::KlvEncodeError;
use crate::klv::checksum::checksum_running_sum_16;
use crate::klv::imapb::ImapbSpecial;
use crate::klv::length::{ber_len, ber_oid_len, write_ber};
use crate::klv::pack::{emit_ber_oid_tlv, is_typed_tag};
use alloc::borrow::Cow;
use alloc::vec::Vec;

use super::mapping::encode_fixed_range;
use super::model::{EncodeConfig, OutOfRangePolicy, UasDatalinkLs};
use super::packs;
use super::tags::{Encoding, FieldValue, LinearRange, TAGS, TagSpec, lookup as lookup_tag};

/// Encode a UAS Datalink Local Set into the caller-provided buffer per
/// MISB ST 0601. Returns the number of bytes written.
///
/// Use [`encode_to_vec`] to allocate a fresh `Vec<u8>` instead;
/// [`encoded_len`] sizes a buffer in advance.
///
/// # Errors
/// - [`KlvEncodeError::BufferTooSmall`] if `out.len()` is less than the
///   required encoded length (call [`encoded_len`] first).
/// - [`KlvEncodeError::OutOfRange`] if any IMAPB-encoded float field
///   value falls outside its declared range (e.g. `platform_heading_deg`
///   outside `0.0..=360.0`).
/// - [`KlvEncodeError::StringTooLong`] if a UTF-8 string field exceeds
///   the spec-declared byte cap.
/// - [`KlvEncodeError::RecordTooLarge`] if the encoded body would
///   overflow the BER-encodable length limit.
/// - [`KlvEncodeError::ReservedTagInUnknown`] if `record.unknown` carries
///   a tag the typed encoder would emit (any tag in `tags::TAGS`) or a
///   reserved structural tag (`{1, 2, 65}`). The `unknown` vec is only
///   for forward-compat pass-through of tags this encoder does not model.
pub fn encode(record: &UasDatalinkLs, out: &mut [u8]) -> Result<usize, KlvEncodeError> {
    encode_with(record, &EncodeConfig::default(), out)
}

/// Encode a UAS Datalink Local Set into the caller-provided buffer with
/// explicit [`EncodeConfig`] (universal label, version byte, out-of-range
/// policy).
///
/// # Errors
/// - [`KlvEncodeError::BufferTooSmall`] if `out.len()` is less than the
///   required encoded length.
/// - [`KlvEncodeError::OutOfRange`] if any ranged float field falls outside
///   its ST 0601 mapped range. Under [`OutOfRangePolicy::Indicator`],
///   `OutOfRange` is still returned for tags without a spec-defined Out-of-Range
///   special (i.e., tags whose INT_MIN sentinel means `Reserved` or
///   `NotAvailable`) and for any non-finite input value.
/// - [`KlvEncodeError::StringTooLong`] if a UTF-8 string exceeds the spec cap.
/// - [`KlvEncodeError::RecordTooLarge`] on BER length overflow.
/// - [`KlvEncodeError::ReservedTagInUnknown`] for typed/reserved tags in
///   `record.unknown`.
pub fn encode_with(
    record: &UasDatalinkLs,
    opts: &EncodeConfig,
    out: &mut [u8],
) -> Result<usize, KlvEncodeError> {
    // Build the inner body into a temporary Vec, then assemble UL + BER length + body + checksum.
    let mut body: Vec<u8> = Vec::with_capacity(256);
    write_typed_fields(record, opts, &mut body)?;
    write_unknown_fields(record, &mut body)?;

    // Reserve room for Tag 1 (checksum) — 4 bytes (tag=1, len-byte=1, value=2).
    let body_len_with_checksum = body.len() + 4;
    let outer_len_bytes = ber_len(body_len_with_checksum);
    let total = 16 + outer_len_bytes + body_len_with_checksum;

    if out.len() < total {
        return Err(KlvEncodeError::BufferTooSmall {
            needed: total,
            got: out.len(),
        });
    }

    // 1) UL
    out[..16].copy_from_slice(&opts.universal_label.0);
    // 2) Outer BER length
    let written = write_ber(body_len_with_checksum, &mut out[16..])?;
    let body_offset = 16 + written;
    // 3) Body
    out[body_offset..body_offset + body.len()].copy_from_slice(&body);
    // 4) Tag 1 (checksum) tag + len
    let cksum_tag_offset = body_offset + body.len();
    out[cksum_tag_offset] = 0x01; // tag 1
    out[cksum_tag_offset + 1] = 0x02; // len 2
    // 5) Compute checksum across [UL .. start of checksum value]
    let cksum_value_offset = cksum_tag_offset + 2;
    let cksum = checksum_running_sum_16(&out[..cksum_value_offset]);
    out[cksum_value_offset] = (cksum >> 8) as u8;
    out[cksum_value_offset + 1] = cksum as u8;
    Ok(total)
}

/// Encode a UAS Datalink Local Set into a fresh `Vec<u8>`. Convenience
/// over [`encode`] when the caller has no pre-sized buffer.
///
/// # Errors
/// Returns the same [`KlvEncodeError`] variants as [`encode`] —
/// [`KlvEncodeError::OutOfRange`] for IMAPB ranges,
/// [`KlvEncodeError::StringTooLong`] for over-cap UTF-8 fields,
/// [`KlvEncodeError::RecordTooLarge`] for BER-overflow records, and
/// [`KlvEncodeError::ReservedTagInUnknown`] if `record.unknown` carries
/// a reserved or typed tag.
/// (`KlvEncodeError::BufferTooSmall` cannot fire on this path — it
/// delegates to [`encode_to_vec_with`], which pre-sizes the buffer via
/// [`encoded_len_with`].)
pub fn encode_to_vec(record: &UasDatalinkLs) -> Result<Vec<u8>, KlvEncodeError> {
    encode_to_vec_with(record, &EncodeConfig::default())
}

/// [`encode_to_vec`] with explicit [`EncodeConfig`] (universal label,
/// version byte, out-of-range policy).
///
/// # Errors
/// Same as [`encode_to_vec`]; additionally, under
/// [`OutOfRangePolicy::Indicator`], [`KlvEncodeError::OutOfRange`] is still
/// returned for tags without a spec-defined Out-of-Range special and for
/// non-finite input values.
/// (`KlvEncodeError::BufferTooSmall` cannot fire — the buffer is pre-sized
/// via [`encoded_len_with`].)
pub fn encode_to_vec_with(
    record: &UasDatalinkLs,
    opts: &EncodeConfig,
) -> Result<Vec<u8>, KlvEncodeError> {
    let n = encoded_len_with(record, opts);
    let mut buf = vec![0u8; n];
    let written = encode_with(record, opts, &mut buf)?;
    buf.truncate(written);
    Ok(buf)
}

/// Encode a UAS Datalink Local Set, rejecting records that omit any
/// item the spec marks mandatory. Symmetric with
/// [`super::decode_strict_compliance`], which already rejects on-the-wire
/// instances missing the same mandatory items.
///
/// Per MISB ST 0601.13 §6 (and re-affirmed by ST 0601.24 §6 and
/// ST 0107.5 §6.2 — Local Set conformance rules), every conformant ST
/// 0601 instance must carry Tag 2 (Precision Time Stamp). Tag 1
/// (Checksum) and Tag 65 (LS Version Number) are also mandatory but are
/// auto-emitted by this encoder, so the only caller-supplied mandatory
/// item this function gates is Tag 2.
///
/// # Errors
///
/// - [`KlvEncodeError::MissingMandatoryItem`] if `record.timestamp_us`
///   is `None`. `tag` will be `2` and `name` will be
///   `"Precision Time Stamp"`.
/// - [`KlvEncodeError::InvalidFieldValue`] with the item's tag when the
///   record would write an item that
///   [`super::decode_strict_compliance`] refuses by its item profile or
///   coded-value domains (for example wavelength id 0 beside another id,
///   a root Metadata Substream ID, a Mode 1 SDCC-FLP, a zero Positioning
///   Method Source, a reserved Generic Flag Data bit, a Platform Status
///   code above 12 or an Image Horizon coordinate above 100 %).
/// - All [`KlvEncodeError`] variants that [`encode_to_vec`] can return,
///   surfaced verbatim from the underlying encode path once the strict
///   precondition gate passes.
pub fn encode_strict_compliance(record: &UasDatalinkLs) -> Result<Vec<u8>, KlvEncodeError> {
    // The only caller-supplied mandatory item under ST 0601.13-22 +
    // ST 0107.5 §6.2 — Tags 1 + 65 auto-emit (see encode.rs:45-73 +
    // 119-166), so they cannot be missing here.
    if record.timestamp_us.is_none() {
        return Err(KlvEncodeError::MissingMandatoryItem {
            tag: 2,
            name: "Precision Time Stamp",
        });
    }
    // ST 0107.5 §6.3.3: sanitize all string fields before encoding — remove
    // banned control chars everywhere (ST 0107.3-13) and trim leading/trailing
    // null/tab/LF/CR/space (ST 0107.3-12). Sanitization runs before the
    // ST 0107.5 §6.3.3.2 empty-string mapping, so a field that sanitizes to
    // "" encodes as [0x00].
    let mut r = record.clone();
    sanitize_strings_st0601(&mut r);
    let out = encode_to_vec(&r)?;
    // The per-item rules are the compliance decode's, judged on the bytes
    // this encoder wrote, so the pair agrees by construction.
    let (len, after_len) =
        crate::klv::length::read_ber(&out[16..]).expect("the encoder writes a valid outer length");
    for f in crate::klv::pack::Iter::local_set(&after_len[..len]) {
        let f = f.expect("the encoder writes a valid body");
        if let Some(reason) = super::decode::profile_violation(f.tag, f.value) {
            return Err(KlvEncodeError::InvalidFieldValue { tag: f.tag, reason });
        }
    }
    Ok(out)
}

pub fn encoded_len(record: &UasDatalinkLs) -> usize {
    encoded_len_with(record, &EncodeConfig::default())
}

pub fn encoded_len_with(record: &UasDatalinkLs, _opts: &EncodeConfig) -> usize {
    let mut body_len = 0usize;
    // No setting of the configuration changes a length: the label is 16
    // bytes, the version one byte, and a value is sized at its row's length
    // under every out-of-range policy.
    each_typed_field(record, |tag, value_len| {
        body_len += ber_oid_len(tag as u32) + ber_len(value_len) + value_len;
    });
    body_len += replayed_signals_len(record);
    for f in &record.unknown {
        body_len += ber_oid_len(f.tag) + ber_len(f.value.len()) + f.value.len();
    }
    let body_len_with_checksum = body_len + 4; // tag 1 (1 byte) + len byte (1) + value (2 bytes)
    16 + ber_len(body_len_with_checksum) + body_len_with_checksum
}

/// Where an item's bytes come from: its typed field, or a signal the
/// record replays for it from `sentinel_tags` or `imapb_specials`.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ItemSource {
    /// The typed field, written as its value.
    Value(FieldValue),
    /// The row's INT_MIN sentinel.
    Sentinel,
    /// An ST 1201 special value.
    Special(ImapbSpecial),
}

/// A replayed signal: the row's sentinel or one IMAP special.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Signal {
    Sentinel,
    Special(ImapbSpecial),
}

/// Which bytes the item `spec` gets from `record`: the one rule sizing,
/// writing and every other writer of ST 0601 items follow.
///
/// A populated typed field is written as its value: the per-row paths of
/// sizing and writing (`each_typed_field`, `encode_tag_value`) take that
/// half of the rule straight from the row's getter, and look at the signal
/// lists only through [`each_replayed_signal`].
/// An unset one replays the record's entry of the row's kind, if it has
/// one: the INT_MIN sentinel for a signed linear row listed in
/// `sentinel_tags`, the LAST special listed for an IMAPB row in
/// `imapb_specials`. An entry of another kind (a special for a linear row,
/// a sentinel for an IMAPB or unsigned row, either for a pack row or an
/// unknown tag) is never written.
pub(super) fn resolve(record: &UasDatalinkLs, spec: &TagSpec) -> Option<ItemSource> {
    if let Some(v) = spec.get.and_then(|get| get(record))
        && !replays_overflow_signal(record, spec, &v)
    {
        return Some(ItemSource::Value(v));
    }
    let tag = u32::from(spec.id);
    match spec.encoding {
        Encoding::Imapb { .. } => record
            .imapb_specials
            .iter()
            .rev()
            .find(|e| e.0 == tag)
            .map(|&(_, special)| ItemSource::Special(special)),
        _ if spec.range.as_ref().is_some_and(|r| r.signed) => record
            .sentinel_tags
            .contains(&tag)
            .then_some(ItemSource::Sentinel),
        _ => None,
    }
}

/// `true` when the populated field `v` of an IMAPB row is written as the
/// overflow signal the record carries for it: `v` is the range's minimum
/// and the tag's last special is `BelowMin`, or its maximum and the last is
/// `AboveMax` (the state a decode of such a signal leaves). A value the
/// caller changed is written as the value.
#[inline]
fn replays_overflow_signal(record: &UasDatalinkLs, spec: &TagSpec, v: &FieldValue) -> bool {
    let (&FieldValue::F64(x), Encoding::Imapb { min, max, .. }) = (v, spec.encoding) else {
        return false;
    };
    if x != min && x != max {
        return false;
    }
    let tag = u32::from(spec.id);
    match record.imapb_specials.iter().rev().find(|e| e.0 == tag) {
        Some(&(_, ImapbSpecial::BelowMin)) => x == min,
        Some(&(_, ImapbSpecial::AboveMax)) => x == max,
        _ => false,
    }
}

/// `true` when [`resolve`] writes a sentinel or an IMAP special for the
/// item `tag`: the item is on the wire, sent as no value.
pub(crate) fn is_signalled(record: &UasDatalinkLs, tag: u8) -> bool {
    lookup_tag(tag).is_some_and(|spec| {
        matches!(
            resolve(record, spec),
            Some(ItemSource::Sentinel | ItemSource::Special(_))
        )
    })
}

/// The INT_MIN sentinel of a signed linear row, at the row's width.
pub(super) fn sentinel_value_bytes(spec: &TagSpec) -> Vec<u8> {
    let width = linear_range(spec).byte_length;
    let mut bytes = vec![0u8; width];
    bytes[0] = 0x80;
    bytes
}

/// The special's bytes for an IMAPB row at the shortest length, from the
/// row's default length up to its maximum, that holds the special's
/// payload.
///
/// # Errors
/// [`KlvEncodeError::OutOfRange`] with the row's tag when no length of the
/// row holds the payload.
pub(super) fn imapb_special_value_bytes(
    spec: &TagSpec,
    special: ImapbSpecial,
) -> Result<Vec<u8>, KlvEncodeError> {
    let Encoding::Imapb {
        default_len,
        max_len,
        ..
    } = spec.encoding
    else {
        unreachable!("only an IMAPB row has IMAP specials")
    };
    for len in default_len..max_len {
        let mut buf = vec![0u8; len];
        if crate::klv::imapb::encode_imapb_special(special, len, &mut buf).is_ok() {
            return Ok(buf);
        }
    }
    let mut buf = vec![0u8; max_len];
    crate::klv::imapb::encode_imapb_special(special, max_len, &mut buf).map_err(|e| match e {
        KlvEncodeError::OutOfRange {
            value,
            min,
            max,
            hint,
            ..
        } => KlvEncodeError::OutOfRange {
            tag: u32::from(spec.id),
            value,
            min,
            max,
            hint,
        },
        e => e,
    })?;
    Ok(buf)
}

/// The value length of a replayed signal, as [`signal_bytes`] writes it; a
/// special no length holds is sized at the row's default and fails when
/// written.
fn signal_len(spec: &TagSpec, signal: Signal) -> usize {
    match signal {
        Signal::Sentinel => linear_range(spec).byte_length,
        Signal::Special(special) => match imapb_special_value_bytes(spec, special) {
            Ok(bytes) => bytes.len(),
            Err(_) => match spec.encoding {
                Encoding::Imapb { default_len, .. } => default_len,
                _ => unreachable!("only an IMAPB row has IMAP specials"),
            },
        },
    }
}

/// The value bytes of a replayed signal.
fn signal_bytes(spec: &TagSpec, signal: Signal) -> Result<Vec<u8>, KlvEncodeError> {
    match signal {
        Signal::Sentinel => Ok(sentinel_value_bytes(spec)),
        Signal::Special(special) => imapb_special_value_bytes(spec, special),
    }
}

/// Visit every signal the record replays, once per tag, in the order the
/// encoder writes them: the sentinels in `sentinel_tags` order, then the
/// specials in `imapb_specials` order, each at its tag's LAST entry (the
/// one a decode of a set that carried the tag twice keeps). An entry
/// [`resolve`] does not replay (its field holds a value that wins, or the
/// entry is of the wrong kind for its row) is skipped.
fn each_replayed_signal(record: &UasDatalinkLs, mut visit: impl FnMut(&TagSpec, Signal)) {
    for &tag in last_per_tag(&record.sentinel_tags, |&t| t).iter() {
        if let Some(spec) = u8::try_from(tag).ok().and_then(lookup_tag)
            && resolve(record, spec) == Some(ItemSource::Sentinel)
        {
            visit(spec, Signal::Sentinel);
        }
    }
    for &(tag, _) in last_per_tag(&record.imapb_specials, |e| e.0).iter() {
        if let Some(spec) = u8::try_from(tag).ok().and_then(lookup_tag)
            && let Some(ItemSource::Special(special)) = resolve(record, spec)
        {
            visit(spec, Signal::Special(special));
        }
    }
}

/// The entries of `entries` that are the last for their tag, in list
/// order. A tag above 255 names no typed row, so it is left out. Linear in
/// the list: one pass from the end with a bit per tag.
fn last_per_tag<T: Copy>(entries: &[T], tag_of: impl Fn(&T) -> u32) -> Cow<'_, [T]> {
    if entries.len() < 2 {
        return Cow::Borrowed(entries);
    }
    let mut seen = [0u64; 4];
    let mut kept: Vec<T> = Vec::new();
    for e in entries.iter().rev() {
        let Ok(t) = u8::try_from(tag_of(e)) else {
            continue;
        };
        let (word, bit) = (usize::from(t >> 6), 1u64 << (t & 63));
        if seen[word] & bit == 0 {
            seen[word] |= bit;
            kept.push(*e);
        }
    }
    kept.reverse();
    Cow::Owned(kept)
}

/// Visit each typed field that will be emitted, calling `visit(tag_id, value_len)`.
/// Used by both `encoded_len_with` (for sizing) and `write_typed_fields` (for emission).
///
/// NOTE: the arm-65 auto-version below sizes Tag 65 even when
/// `uas_ls_version` is `None`, matching the `encode*` entry points'
/// fallback emission. Do NOT reuse this visitor for `patch()` sizing —
/// `patch()` never auto-injects Tag 65 (it builds output `Vec`s
/// directly and never calls this).
pub(super) fn each_typed_field<F: FnMut(u8, usize)>(record: &UasDatalinkLs, mut visit: F) {
    for spec in TAGS {
        let len = if spec.encoding == Encoding::Pack {
            // Tags 102 and 115 are multi-instance: their loops follow.
            pack_len(record, spec.id)
        } else {
            // The checksum row has no accessor; the caller appends it.
            let Some(get) = spec.get else { continue };
            // A populated field is written as its value (`resolve`).
            match get(record) {
                Some(v) if !replays_overflow_signal(record, spec, &v) => Some(scalar_len(spec, &v)),
                Some(_) => None,
                // Tag 65 is auto-emitted when the record leaves it unset.
                None if spec.id == 65 => Some(1),
                None => None,
            }
        };
        if let Some(len) = len {
            visit(spec.id, len);
        }
    }
    // Tag 102 (SDCC-FLP) is also MULTI-INSTANCE (ST 0601.19 Table 1
    // "Multiples Allowed" = Yes) — one TLV per `sdcc_flps` entry, sized
    // by its already-captured raw bytes (no re-derivation).
    for field in &record.sdcc_flps {
        visit(102, field.bytes.len());
    }
    // Tag 115 (Control Command) is MULTI-INSTANCE (ST 0601.19 Table 1
    // "Multiples Allowed" = Yes) — one TLV per `control_commands` entry,
    // which the once-per-tag TAGS loop above cannot express.
    for cmd in &record.control_commands {
        visit(115, packs::control_command_len(cmd));
    }
}

/// The wire length of one scalar row's value. A string or an integer is
/// sized without building its bytes. Ranged and IMAPB rows are sized at
/// their fixed length whether the eventual bytes are a normal-range
/// encode or (under `OutOfRangePolicy::Indicator`) a special; an
/// Error-policy out-of-range value still sizes here, and `encode_scalar`
/// is what surfaces the actual `OutOfRange` error during emission.
fn scalar_len(spec: &TagSpec, v: &FieldValue) -> usize {
    match spec.encoding {
        Encoding::U8 | Encoding::I8 => 1,
        Encoding::U16 => 2,
        Encoding::U64 => 8,
        Encoding::U8Range
        | Encoding::U16Range
        | Encoding::I16Range
        | Encoding::U32Range
        | Encoding::I32Range => linear_range(spec).byte_length,
        Encoding::Imapb { default_len, .. } => default_len,
        Encoding::Utf8 { .. } => match v {
            FieldValue::Str(s) => str_wire_len(s),
            _ => wrong_variant(spec),
        },
        Encoding::RawBytes => match v {
            FieldValue::Bytes(b) => b.len(),
            _ => wrong_variant(spec),
        },
        Encoding::VarUint { .. } => crate::klv::length::var_uint_min_len(wire_u64(spec, v)),
        Encoding::VarInt { .. } => crate::klv::length::var_int_min_len(wire_i64(spec, v)),
        Encoding::Pack => wrong_variant(spec),
    }
}

/// The value bytes of one scalar row. Calls `encode_fixed_range`,
/// `encode_imapb_field` and the shortest-form varint writers exactly as
/// the table's float and integer semantics need; no expression here is
/// reordered from the wire definitions.
fn encode_scalar(
    spec: &TagSpec,
    v: &FieldValue,
    policy: OutOfRangePolicy,
) -> Result<Vec<u8>, KlvEncodeError> {
    Ok(match spec.encoding {
        Encoding::U8 => vec![wire_u64(spec, v) as u8],
        Encoding::I8 => vec![wire_i64(spec, v) as u8],
        Encoding::U16 => (wire_u64(spec, v) as u16).to_be_bytes().to_vec(),
        Encoding::U64 => wire_u64(spec, v).to_be_bytes().to_vec(),
        Encoding::U8Range
        | Encoding::U16Range
        | Encoding::I16Range
        | Encoding::U32Range
        | Encoding::I32Range => encode_ranged(wire_f64(spec, v), spec, policy)?,
        Encoding::Imapb {
            min,
            max,
            default_len,
            ..
        } => encode_imapb_field(
            wire_f64(spec, v),
            spec.id as u32,
            min,
            max,
            default_len,
            policy,
        )?,
        Encoding::Utf8 { .. } => match v {
            FieldValue::Str(s) => {
                check_string(spec.id as u32, s, &spec.encoding)?;
                str_to_bytes(s)
            }
            _ => wrong_variant(spec),
        },
        Encoding::RawBytes => match v {
            FieldValue::Bytes(b)
                if spec.id == 94 && b.len() > super::decode::MIIS_CORE_ID_MAX_LEN =>
            {
                return Err(KlvEncodeError::ValueTooLong {
                    tag: 94,
                    max: super::decode::MIIS_CORE_ID_MAX_LEN,
                });
            }
            FieldValue::Bytes(b) => b.clone(),
            _ => wrong_variant(spec),
        },
        Encoding::VarUint { .. } => crate::klv::length::write_var_uint_min(wire_u64(spec, v)),
        Encoding::VarInt { .. } => crate::klv::length::write_var_int_min(wire_i64(spec, v)),
        Encoding::Pack => wrong_variant(spec),
    })
}

/// A row's accessor yields the variant its encoding names (a coded enum
/// travels as its wire code), so these are unreachable by construction.
fn wrong_variant(spec: &TagSpec) -> ! {
    unreachable!(
        "tag {} ({}) accessor yielded the wrong value variant",
        spec.id, spec.name
    )
}

fn wire_u64(spec: &TagSpec, v: &FieldValue) -> u64 {
    match v {
        FieldValue::U64(n) => *n,
        FieldValue::Code(c) => u64::from(*c),
        _ => wrong_variant(spec),
    }
}

fn wire_i64(spec: &TagSpec, v: &FieldValue) -> i64 {
    match v {
        FieldValue::I64(n) => *n,
        _ => wrong_variant(spec),
    }
}

fn wire_f64(spec: &TagSpec, v: &FieldValue) -> f64 {
    match v {
        FieldValue::F64(n) => *n,
        _ => wrong_variant(spec),
    }
}

fn linear_range(spec: &TagSpec) -> &LinearRange {
    spec.range
        .as_ref()
        .expect("ranged tag must have LinearRange")
}

/// The wire length of a single-instance pack row's value, `None` when the
/// record leaves the pack unset. Tags 102 and 115 are multi-instance and
/// are sized by `each_typed_field`'s trailing loops.
fn pack_len(record: &UasDatalinkLs, tag: u8) -> Option<usize> {
    match tag {
        81 => record.image_horizon.map(|h| packs::image_horizon_len(&h)),
        116 => record
            .control_command_verification
            .as_ref()
            .map(|ids| packs::id_list_len(ids)),
        121 => record
            .active_wavelengths
            .as_ref()
            .map(|ids| packs::id_list_len(ids)),
        122 => record.country_codes.as_ref().map(packs::country_codes_len),
        127 => record
            .sensor_frame_rate
            .map(|fr| packs::sensor_frame_rate_len(&fr)),
        128 => record
            .wavelengths_list
            .as_ref()
            .map(|l| packs::wavelengths_list_len(l)),
        130 => record
            .airbase_locations
            .map(|al| packs::airbase_locations_len(&al)),
        138 => record.payload_list.as_ref().map(packs::payload_list_len),
        140 => record
            .weapons_stores
            .as_ref()
            .map(|l| packs::weapons_stores_len(l)),
        141 => record
            .waypoint_list
            .as_ref()
            .map(|l| packs::waypoints_len(l)),
        142 => record.view_domain.map(|vd| packs::view_domain_len(&vd)),
        143 => record
            .metadata_substream_id
            .map(|ms| packs::metadata_substream_id_len(&ms)),
        _ => None,
    }
}

/// Build one pack's value bytes into a buffer sized for `len`.
fn pack_buf<T>(
    len: usize,
    emit: impl FnOnce(&mut Vec<u8>) -> Result<T, KlvEncodeError>,
) -> Result<Vec<u8>, KlvEncodeError> {
    let mut buf = Vec::with_capacity(len);
    emit(&mut buf)?;
    Ok(buf)
}

/// The value bytes of a single-instance pack row, `None` when the record
/// leaves the pack unset. Tags 102 and 115 are multi-instance and are
/// emitted by `write_typed_fields`'s trailing loops, so they yield `None`.
fn pack_value(record: &UasDatalinkLs, tag: u8) -> Result<Option<Vec<u8>>, KlvEncodeError> {
    match tag {
        81 => record
            .image_horizon
            .map(|h| {
                pack_buf(packs::image_horizon_len(&h), |b| {
                    packs::emit_image_horizon(&h, b)
                })
            })
            .transpose(),
        116 => record
            .control_command_verification
            .as_ref()
            .map(|ids| pack_buf(packs::id_list_len(ids), |b| packs::emit_id_list(ids, b)))
            .transpose(),
        121 => record
            .active_wavelengths
            .as_ref()
            .map(|ids| pack_buf(packs::id_list_len(ids), |b| packs::emit_id_list(ids, b)))
            .transpose(),
        122 => record
            .country_codes
            .as_ref()
            .map(|cc| {
                pack_buf(packs::country_codes_len(cc), |b| {
                    packs::emit_country_codes(cc, b)
                })
            })
            .transpose(),
        127 => record
            .sensor_frame_rate
            .map(|fr| {
                pack_buf(packs::sensor_frame_rate_len(&fr), |b| {
                    packs::emit_sensor_frame_rate(&fr, b)
                })
            })
            .transpose(),
        128 => record
            .wavelengths_list
            .as_ref()
            .map(|l| {
                pack_buf(packs::wavelengths_list_len(l), |b| {
                    packs::emit_wavelengths_list(l, b)
                })
            })
            .transpose(),
        130 => record
            .airbase_locations
            .map(|al| {
                pack_buf(packs::airbase_locations_len(&al), |b| {
                    packs::emit_airbase_locations(&al, b)
                })
            })
            .transpose(),
        138 => record
            .payload_list
            .as_ref()
            .map(|pl| {
                pack_buf(packs::payload_list_len(pl), |b| {
                    packs::emit_payload_list(pl, b)
                })
            })
            .transpose(),
        140 => record
            .weapons_stores
            .as_ref()
            .map(|l| {
                pack_buf(packs::weapons_stores_len(l), |b| {
                    packs::emit_weapons_stores(l, b)
                })
            })
            .transpose(),
        141 => record
            .waypoint_list
            .as_ref()
            .map(|l| pack_buf(packs::waypoints_len(l), |b| packs::emit_waypoints(l, b)))
            .transpose(),
        142 => record
            .view_domain
            .map(|vd| {
                pack_buf(packs::view_domain_len(&vd), |b| {
                    packs::emit_view_domain(&vd, b)
                })
            })
            .transpose(),
        143 => record
            .metadata_substream_id
            .map(|ms| {
                pack_buf(packs::metadata_substream_id_len(&ms), |b| {
                    packs::emit_metadata_substream_id(&ms, b)
                })
            })
            .transpose(),
        _ => Ok(None),
    }
}

/// Encode the VALUE bytes for one typed tag from `record`, or `None`
/// when the corresponding field is absent. `version_fallback` supplies
/// the Tag 65 auto-version used by the `encode*` entry points; `patch`
/// passes `None` so only explicitly-set fields are encoded.
pub(super) fn encode_tag_value(
    record: &UasDatalinkLs,
    spec: &TagSpec,
    version_fallback: Option<u8>,
    policy: OutOfRangePolicy,
) -> Result<Option<Vec<u8>>, KlvEncodeError> {
    if spec.encoding == Encoding::Pack {
        return pack_value(record, spec.id);
    }
    // The checksum row has no accessor; the caller appends it.
    let Some(get) = spec.get else {
        return Ok(None);
    };
    // A populated field is written as its value (`resolve`).
    match get(record) {
        Some(v) if !replays_overflow_signal(record, spec, &v) => {
            encode_scalar(spec, &v, policy).map(Some)
        }
        Some(_) => Ok(None),
        None if spec.id == 65 => Ok(version_fallback.map(|v| vec![v])),
        None => Ok(None),
    }
}

fn write_typed_fields(
    record: &UasDatalinkLs,
    opts: &EncodeConfig,
    body: &mut Vec<u8>,
) -> Result<(), KlvEncodeError> {
    for spec in TAGS {
        if spec.id == 1 {
            continue;
        }
        if spec.id == 102 {
            continue; // MULTI-INSTANCE — handled below
        }
        if spec.id == 115 {
            continue; // MULTI-INSTANCE — handled below
        }
        if let Some(value) =
            encode_tag_value(record, spec, Some(opts.version), opts.out_of_range_policy)?
        {
            emit_ber_oid_tlv(spec.id as u32, &value, body)?;
        }
    }
    // Tag 102 (SDCC-FLP) is MULTI-INSTANCE — one TLV per
    // `record.sdcc_flps` entry, emitted verbatim (the raw captured
    // bytes, not re-derived from a parsed `SdccFlp`). Ascending-order
    // emission caveat: see `SdccFlpField`'s doc — grouping every
    // occurrence together here does not reproduce the original wire
    // interleaving with the Local Set items named in `preceding_tags`.
    for field in &record.sdcc_flps {
        emit_ber_oid_tlv(102, &field.bytes, body)?;
    }
    // Tag 115 (Control Command) is MULTI-INSTANCE — one TLV per
    // `record.control_commands` entry, in push order. Mirror of the
    // `each_typed_field` special-case above.
    for cmd in &record.control_commands {
        let mut buf = Vec::with_capacity(packs::control_command_len(cmd));
        packs::emit_control_command(cmd, &mut buf)?;
        emit_ber_oid_tlv(115, &buf, body)?;
    }
    write_replayed_signals(record, body)
}

/// The TLV bytes of the replayed signals, as [`write_replayed_signals`]
/// writes them. Out of line: most records replay none, and the per-row
/// sizing loop stays as small as it is without signals.
#[inline(never)]
fn replayed_signals_len(record: &UasDatalinkLs) -> usize {
    let mut len = 0;
    if !record.sentinel_tags.is_empty() || !record.imapb_specials.is_empty() {
        each_replayed_signal(record, |spec, signal| {
            let vlen = signal_len(spec, signal);
            len += ber_oid_len(u32::from(spec.id)) + ber_len(vlen) + vlen;
        });
    }
    len
}

/// Write each signal [`each_replayed_signal`] visits, after the typed
/// fields: one TLV per tag, at the shortest width that holds it.
fn write_replayed_signals(
    record: &UasDatalinkLs,
    body: &mut Vec<u8>,
) -> Result<(), KlvEncodeError> {
    if record.sentinel_tags.is_empty() && record.imapb_specials.is_empty() {
        return Ok(());
    }
    let mut result = Ok(());
    each_replayed_signal(record, |spec, signal| {
        if result.is_ok() {
            result = signal_bytes(spec, signal)
                .and_then(|bytes| emit_ber_oid_tlv(u32::from(spec.id), &bytes, body));
        }
    });
    result
}

fn write_unknown_fields(record: &UasDatalinkLs, body: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    for f in &record.unknown {
        // Reject reserved/typed tags before emitting. Without this guard,
        // a caller-constructed Tag 2 in `unknown` would produce a duplicate
        // Precision Time Stamp; a Tag 1 would produce a bogus mid-stream
        // pseudo-checksum followed by the real final checksum; a Tag 65
        // would produce a duplicate UAS LS Version Number; and any typed
        // tag (5-91 modeled) would produce a non-conformant duplicate. The
        // `unknown` vec is for forward-compat pass-through only.
        if is_typed_tag(f.tag, super::tags::lookup) {
            return Err(KlvEncodeError::ReservedTagInUnknown { tag: f.tag });
        }
        emit_ber_oid_tlv(f.tag, &f.value, body)?;
    }
    Ok(())
}

fn encode_ranged(
    v: f64,
    spec: &TagSpec,
    policy: OutOfRangePolicy,
) -> Result<Vec<u8>, KlvEncodeError> {
    let r = linear_range(spec);
    let mut scratch = [0u8; 8];
    encode_fixed_range(r, spec.id as u32, v, &mut scratch[..r.byte_length], policy)?;
    Ok(scratch[..r.byte_length].to_vec())
}

/// Encode one ST 1201.5 IMAPB extended-range field — the IMAPB
/// sibling of `encode_ranged`. In-range values encode normally at
/// `default_len`, and so does a value in the band the substrate takes just
/// below `min` (the range's bottom bucket at that length, at least a float
/// tolerance wide), which is written as `min` under every policy;
/// out-of-range values either error (default policy) or, under
/// [`OutOfRangePolicy::Indicator`], emit the tag's `BelowMin` / `AboveMax`
/// special at `default_len` instead. Non-finite input always errors
/// regardless of policy.
fn encode_imapb_field(
    v: f64,
    tag: u32,
    min: f64,
    max: f64,
    default_len: usize,
    policy: OutOfRangePolicy,
) -> Result<Vec<u8>, KlvEncodeError> {
    let mut buf = vec![0u8; default_len];
    let p = crate::klv::imapb::ImapbParams {
        min,
        max,
        length: default_len,
    };
    match crate::klv::imapb::encode_imapb(&p, v, &mut buf) {
        Ok(()) => return Ok(buf),
        // Outside what the substrate writes: below its band, above `max`,
        // or not finite.
        Err(KlvEncodeError::OutOfRange { .. }) => {}
        Err(e) => return Err(e),
    }
    if policy == OutOfRangePolicy::Indicator && v.is_finite() {
        let special = if v < min {
            crate::klv::imapb::ImapbSpecial::BelowMin
        } else {
            crate::klv::imapb::ImapbSpecial::AboveMax
        };
        crate::klv::imapb::encode_imapb_special(special, default_len, &mut buf)?;
        return Ok(buf);
    }
    // Extended items have no LinearRange twin of their own to name in a
    // hint (`mapping::range_hint` only covers LinearRange fields).
    Err(KlvEncodeError::OutOfRange {
        tag,
        value: v,
        min,
        max,
        hint: None,
    })
}

/// Wire byte count for a UTF-8 string: empty → 1 (NUL signal), else `s.len()`.
fn str_wire_len(s: &str) -> usize {
    if s.is_empty() { 1 } else { s.len() }
}

/// Encode a UTF-8 string per ST 0107.5 §6.3.3.2:
/// empty string → `[0x00]` (single NUL), non-empty → `s.as_bytes()`.
fn str_to_bytes(s: &str) -> Vec<u8> {
    if s.is_empty() {
        vec![0x00]
    } else {
        s.as_bytes().to_vec()
    }
}

fn check_string(tag: u32, s: &str, enc: &Encoding) -> Result<(), KlvEncodeError> {
    if let Encoding::Utf8 { max_bytes } = enc
        && s.len() > *max_bytes
    {
        return Err(KlvEncodeError::StringTooLong {
            tag,
            max: *max_bytes,
        });
    }
    Ok(())
}

/// Sanitize all string fields of a record per ST 0107.5 §6.3.3 before
/// strict-compliance encode. Runs BEFORE the empty-string mapping (a field
/// that sanitizes to `""` encodes as `[0x00]`).
///
/// NOTE: this list must cover EVERY typed `Encoding::Utf8` field — add a
/// `strip_opt_str` line whenever a new ST 0601 string tag is typed (a
/// review caught six missing entries when tags 70/106-108/129/135 were
/// typed). Pinned by the `strict_encode_sanitizes_all_string_fields` test.
/// The strings inside packs follow (pinned by
/// `strict_encode_sanitises_pack_strings`); a pack string that cleans to
/// `""` keeps the zero-length encoding it then has.
pub(crate) fn sanitize_strings_st0601(r: &mut super::model::UasDatalinkLs) {
    strip_opt_str(&mut r.mission_id);
    strip_opt_str(&mut r.platform_tail_number);
    strip_opt_str(&mut r.platform_designation);
    strip_opt_str(&mut r.image_source_sensor);
    strip_opt_str(&mut r.image_coordinate_system);
    strip_opt_str(&mut r.platform_call_sign);
    strip_opt_str(&mut r.alternate_platform_name);
    strip_opt_str(&mut r.stream_designator);
    strip_opt_str(&mut r.operational_base);
    strip_opt_str(&mut r.broadcast_source);
    strip_opt_str(&mut r.target_id);
    strip_opt_str(&mut r.communications_method);
    for c in &mut r.control_commands {
        c.command = sanitize_st0107_string(&c.command);
    }
    if let Some(cc) = &mut r.country_codes {
        for code in [&mut cc.overflight, &mut cc.operator, &mut cc.manufacture] {
            strip_opt_str(code);
            // A country code is written length-prefixed: "" is length 0,
            // which reads back as an unknown code.
            if code.as_deref() == Some("") {
                *code = None;
            }
        }
    }
    for w in r.wavelengths_list.iter_mut().flatten() {
        w.name = sanitize_st0107_string(&w.name);
    }
    if let Some(list) = &mut r.payload_list {
        for p in &mut list.records {
            p.name = sanitize_st0107_string(&p.name);
        }
    }
    for w in r.weapons_stores.iter_mut().flatten() {
        w.weapon_type = sanitize_st0107_string(&w.weapon_type);
    }
}

fn strip_opt_str(s: &mut Option<alloc::string::String>) {
    if let Some(s) = s {
        *s = sanitize_st0107_string(s);
    }
}

/// Sanitize one string per ST 0107.5 §6.3.3's two "shall"s:
/// - ST 0107.3-13: remove ALL characters in U+0000–U+0008, U+000B, U+000C,
///   U+000E–U+001F, U+007F (at any position);
/// - ST 0107.3-12: remove LEADING and TRAILING null (0x00), tab (0x09),
///   line feed (0x0A), carriage return (0x0D), and space (0x20).
///
/// Embedded tab/LF/CR/space are legitimate content and are kept. The filter
/// runs first so control characters cannot shield end-whitespace from the
/// trim (null is in both sets, so only tab/LF/CR/space remain to trim).
pub(crate) fn sanitize_st0107_string(s: &str) -> alloc::string::String {
    let filtered: alloc::string::String =
        s.chars().filter(|&c| !is_st0107_control_char(c)).collect();
    filtered
        .trim_matches([' ', '\t', '\n', '\r'].as_slice())
        .into()
}

fn is_st0107_control_char(c: char) -> bool {
    matches!(c, '\x00'..='\x08' | '\x0B' | '\x0C' | '\x0E'..='\x1F' | '\x7F')
}
