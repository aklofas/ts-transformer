//! ST 0601 encode entry points — 5 public functions plus private helpers.

use crate::error::KlvEncodeError;
use crate::klv::checksum::checksum_running_sum_16;
use crate::klv::length::{ber_len, ber_oid_len, write_ber};
use crate::klv::pack::{emit_ber_oid_tlv, is_typed_tag};
use alloc::vec::Vec;

use super::mapping::encode_fixed_range;
use super::model::{EncodeConfig, OutOfRangePolicy, UasDatalinkLs};
use super::packs;
use super::tags::{Encoding, TAGS, TagSpec, lookup as lookup_tag};

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
/// The set of enforced tags is exposed (hidden from doc) as
/// [`_mandatory_tags`] for future proptest hookup; production callers
/// should not depend on it.
///
/// # Errors
///
/// - [`KlvEncodeError::MissingMandatoryItem`] if `record.timestamp_us`
///   is `None`. `tag` will be `2` and `name` will be
///   `"Precision Time Stamp"`.
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
    encode_to_vec(&r)
}

/// Caller-supplied mandatory ST 0601 tag IDs enforced by
/// [`encode_strict_compliance`]. Tag 1 (Checksum) and Tag 65 (LS Version
/// Number) are deliberately excluded — both are auto-emitted by the
/// encoder. Validate-2 proptests may pivot on this list to fuzz strict
/// vs. lenient encode paths.
#[doc(hidden)]
#[must_use]
pub fn _mandatory_tags() -> &'static [u16] {
    &[2]
}

pub fn encoded_len(record: &UasDatalinkLs) -> usize {
    encoded_len_with(record, &EncodeConfig::default())
}

pub fn encoded_len_with(record: &UasDatalinkLs, opts: &EncodeConfig) -> usize {
    let mut body_len = 0usize;
    each_typed_field(record, opts, |tag, value_len| {
        body_len += ber_oid_len(tag as u32) + ber_len(value_len) + value_len;
    });
    // Sentinel tags: each tag in `sentinel_tags` whose typed field is None
    // re-emits INT_MIN bytes. Value wins: a populated field is already
    // counted by `each_typed_field` above.
    for &st in &record.sentinel_tags {
        body_len += sentinel_field_len(record, st);
    }
    // IMAPB specials: each (tag, special) in `imapb_specials` whose typed
    // field is None re-emits the special's bytes. Value wins: a populated
    // field is already counted by `each_typed_field` above.
    for &(tag, _) in &record.imapb_specials {
        body_len += imapb_special_field_len(record, tag);
    }
    for f in &record.unknown {
        body_len += ber_oid_len(f.tag) + ber_len(f.value.len()) + f.value.len();
    }
    let body_len_with_checksum = body_len + 4; // tag 1 (1 byte) + len byte (1) + value (2 bytes)
    16 + ber_len(body_len_with_checksum) + body_len_with_checksum
}

/// Return the TLV byte size that a sentinel entry for `tag` contributes, or
/// 0 if the tag is not a signed ranged field, is unrecognized, or its typed
/// field is already populated (value wins over the sentinel).
fn sentinel_field_len(record: &UasDatalinkLs, tag: u32) -> usize {
    let Ok(tag_u8) = u8::try_from(tag) else {
        return 0;
    };
    let Some(spec) = lookup_tag(tag_u8) else {
        return 0;
    };
    let Some(ref range) = spec.range else {
        return 0;
    };
    if !range.signed {
        return 0;
    }
    // Value wins: if the typed field is populated, it is already counted.
    if super::decode::ranged_entry(tag_u8)
        .and_then(|e| (e.get)(record))
        .is_some()
    {
        return 0;
    }
    let vlen = range.byte_length;
    ber_oid_len(tag) + ber_len(vlen) + vlen
}

/// Return the TLV byte size that an IMAPB special entry for `tag`
/// contributes, or 0 if the tag is not IMAPB-encoded, is unrecognized, or
/// its typed field is already populated (value wins over the special).
/// Sibling of `sentinel_field_len` for the IMAPB fields.
fn imapb_special_field_len(record: &UasDatalinkLs, tag: u32) -> usize {
    let Ok(tag_u8) = u8::try_from(tag) else {
        return 0;
    };
    let Some(spec) = lookup_tag(tag_u8) else {
        return 0;
    };
    let Encoding::Imapb { default_len, .. } = spec.encoding else {
        return 0;
    };
    if super::decode::ranged_entry(tag_u8)
        .and_then(|e| (e.get)(record))
        .is_some()
    {
        return 0;
    }
    ber_oid_len(tag) + ber_len(default_len) + default_len
}

/// Visit each typed field that will be emitted, calling `visit(tag_id, value_len)`.
/// Used by both `encoded_len_with` (for sizing) and `write_typed_fields` (for emission).
///
/// NOTE: the arm-65 auto-version below sizes Tag 65 even when
/// `uas_ls_version` is `None`, matching the `encode*` entry points'
/// fallback emission. Do NOT reuse this visitor for `patch()` sizing —
/// `patch()` never auto-injects Tag 65 (it builds output `Vec`s
/// directly and never calls this).
pub(super) fn each_typed_field<F: FnMut(u8, usize)>(
    record: &UasDatalinkLs,
    _opts: &EncodeConfig,
    mut visit: F,
) {
    // Tag 65 auto-emit if not explicitly set.
    let auto_version = record.uas_ls_version.is_none();

    for spec in TAGS {
        if spec.id == 1 {
            continue; // checksum is appended after
        }
        if spec.id == 102 {
            continue; // MULTI-INSTANCE — handled below, one visit() call per Vec entry
        }
        if spec.id == 115 {
            continue; // MULTI-INSTANCE — handled below, one visit() call per Vec entry
        }
        let len = match spec.id {
            2 => record.timestamp_us.map(|_| 8),
            3 => record.mission_id.as_ref().map(|s| str_wire_len(s)),
            4 => record
                .platform_tail_number
                .as_ref()
                .map(|s| str_wire_len(s)),
            10 => record
                .platform_designation
                .as_ref()
                .map(|s| str_wire_len(s)),
            11 => record.image_source_sensor.as_ref().map(|s| str_wire_len(s)),
            12 => record
                .image_coordinate_system
                .as_ref()
                .map(|s| str_wire_len(s)),
            34 => record.icing_detected.map(|_| 1),
            39 => record.outside_air_temp_c.map(|_| 1),
            47 => record.generic_flag_data.map(|_| 1),
            48 => record.security_local_set.as_ref().map(|v| v.len()),
            59 => record.platform_call_sign.as_ref().map(|s| str_wire_len(s)),
            60 => record.weapon_load.map(|_| 2),
            61 => record.weapon_fired.map(|_| 1),
            62 => record.laser_prf_code.map(|_| 2),
            63 => record.sensor_fov_name.map(|_| 1),
            65 => record
                .uas_ls_version
                .map(|_| 1)
                .or(if auto_version { Some(1) } else { None }),
            70 => record
                .alternate_platform_name
                .as_ref()
                .map(|s| str_wire_len(s)),
            72 => record.event_start_time_us.map(|_| 8),
            73 => record.rvt.as_ref().map(|v| v.len()),
            74 => record.vmti.as_ref().map(|v| v.len()),
            77 => record.operational_mode.map(|_| 1),
            94 => record.miis_core_id.as_ref().map(|v| v.len()),
            95 => record.sar_mi_local_set.as_ref().map(|v| v.len()),
            97 => record.range_image_local_set.as_ref().map(|v| v.len()),
            98 => record.geo_registration_local_set.as_ref().map(|v| v.len()),
            99 => record.composite_imaging_local_set.as_ref().map(|v| v.len()),
            100 => record.segment_local_set.as_ref().map(|v| v.len()),
            101 => record.amend_local_set.as_ref().map(|v| v.len()),
            106 => record.stream_designator.as_ref().map(|s| str_wire_len(s)),
            107 => record.operational_base.as_ref().map(|s| str_wire_len(s)),
            108 => record.broadcast_source.as_ref().map(|s| str_wire_len(s)),
            129 => record.target_id.as_ref().map(|s| str_wire_len(s)),
            135 => record
                .communications_method
                .as_ref()
                .map(|s| str_wire_len(s)),
            // Var-length int/enum fields — shortest-form
            // length, computed without allocating the value bytes.
            110 => record
                .time_airborne_s
                .map(|v| crate::klv::length::var_uint_min_len(v as u64)),
            111 => record
                .propulsion_unit_speed_rpm
                .map(|v| crate::klv::length::var_uint_min_len(v as u64)),
            123 => record
                .navsats_in_view
                .map(|v| crate::klv::length::var_uint_min_len(v as u64)),
            124 => record
                .positioning_method_source
                .map(|v| crate::klv::length::var_uint_min_len(v as u64)),
            125 => record
                .platform_status
                .map(|v| crate::klv::length::var_uint_min_len(v.to_wire() as u64)),
            126 => record
                .sensor_control_mode
                .map(|v| crate::klv::length::var_uint_min_len(v.to_wire() as u64)),
            131 => record
                .take_off_time_us
                .map(crate::klv::length::var_uint_min_len),
            133 => record
                .mi_storage_capacity_gb
                .map(|v| crate::klv::length::var_uint_min_len(v as u64)),
            136 => record
                .leap_seconds
                .map(|v| crate::klv::length::var_int_min_len(v as i64)),
            137 => record
                .correction_offset_us
                .map(crate::klv::length::var_int_min_len),
            139 => record.active_payloads.as_ref().map(|v| v.len()),
            // Pack/list items (single-instance; Tags 102
            // and 115 are multi-instance and are handled after this loop).
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
            // All 69 LinearRange Option<f64> fields — driven from RANGED_FIELDS
            // so that `byte_length` comes from the single `tags::TAGS` source.
            _ if spec.range.is_some() => super::decode::ranged_entry(spec.id)
                .and_then(|e| (e.get)(record).map(|_| spec.range.as_ref().unwrap().byte_length)),
            // The 14 IMAPB extended-range fields — always sized at
            // default_len whether the eventual bytes are a normal-range
            // encode or (under OutOfRangePolicy::Indicator) a special;
            // both are default_len bytes. An Error-policy out-of-range
            // value still sizes here — `encode_tag_value` is what surfaces
            // the actual OutOfRange error during emission.
            _ if matches!(spec.encoding, Encoding::Imapb { .. }) => {
                super::decode::ranged_entry(spec.id).and_then(|e| {
                    let Encoding::Imapb { default_len, .. } = spec.encoding else {
                        unreachable!()
                    };
                    (e.get)(record).map(|_| default_len)
                })
            }
            _ => None,
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
    let mut scratch = [0u8; 8];
    Ok(match spec.id {
        2 => record.timestamp_us.map(|t| t.to_be_bytes().to_vec()),
        3 => record
            .mission_id
            .as_ref()
            .map(|s| check_string(3, s, &spec.encoding).map(|_| str_to_bytes(s)))
            .transpose()?,
        4 => record
            .platform_tail_number
            .as_ref()
            .map(|s| check_string(4, s, &spec.encoding).map(|_| str_to_bytes(s)))
            .transpose()?,
        10 => record
            .platform_designation
            .as_ref()
            .map(|s| check_string(10, s, &spec.encoding).map(|_| str_to_bytes(s)))
            .transpose()?,
        11 => record
            .image_source_sensor
            .as_ref()
            .map(|s| check_string(11, s, &spec.encoding).map(|_| str_to_bytes(s)))
            .transpose()?,
        12 => record
            .image_coordinate_system
            .as_ref()
            .map(|s| check_string(12, s, &spec.encoding).map(|_| str_to_bytes(s)))
            .transpose()?,
        34 => record.icing_detected.map(|v| vec![v.to_wire()]),
        39 => record.outside_air_temp_c.map(|v| vec![v as u8]),
        47 => record.generic_flag_data.map(|b| vec![b]),
        48 => record.security_local_set.clone(),
        59 => record
            .platform_call_sign
            .as_ref()
            .map(|s| check_string(59, s, &spec.encoding).map(|_| str_to_bytes(s)))
            .transpose()?,
        60 => record.weapon_load.map(|v| v.to_be_bytes().to_vec()),
        61 => record.weapon_fired.map(|b| vec![b]),
        62 => record.laser_prf_code.map(|v| v.to_be_bytes().to_vec()),
        63 => record.sensor_fov_name.map(|v| vec![v.to_wire()]),
        65 => match (record.uas_ls_version, version_fallback) {
            (Some(v), _) => Some(vec![v]),
            (None, Some(fallback)) => Some(vec![fallback]),
            (None, None) => None,
        },
        70 => record
            .alternate_platform_name
            .as_ref()
            .map(|s| check_string(70, s, &spec.encoding).map(|_| str_to_bytes(s)))
            .transpose()?,
        72 => record.event_start_time_us.map(|t| t.to_be_bytes().to_vec()),
        73 => record.rvt.clone(),
        74 => record.vmti.clone(),
        77 => record.operational_mode.map(|v| vec![v.to_wire()]),
        94 => record.miis_core_id.clone(),
        95 => record.sar_mi_local_set.clone(),
        97 => record.range_image_local_set.clone(),
        98 => record.geo_registration_local_set.clone(),
        99 => record.composite_imaging_local_set.clone(),
        100 => record.segment_local_set.clone(),
        101 => record.amend_local_set.clone(),
        106 => record
            .stream_designator
            .as_ref()
            .map(|s| check_string(106, s, &spec.encoding).map(|_| str_to_bytes(s)))
            .transpose()?,
        107 => record
            .operational_base
            .as_ref()
            .map(|s| check_string(107, s, &spec.encoding).map(|_| str_to_bytes(s)))
            .transpose()?,
        108 => record
            .broadcast_source
            .as_ref()
            .map(|s| check_string(108, s, &spec.encoding).map(|_| str_to_bytes(s)))
            .transpose()?,
        129 => record
            .target_id
            .as_ref()
            .map(|s| check_string(129, s, &spec.encoding).map(|_| str_to_bytes(s)))
            .transpose()?,
        135 => record
            .communications_method
            .as_ref()
            .map(|s| check_string(135, s, &spec.encoding).map(|_| str_to_bytes(s)))
            .transpose()?,
        // Var-length int/enum fields — always shortest-form.
        110 => record
            .time_airborne_s
            .map(|v| crate::klv::length::write_var_uint_min(v as u64)),
        111 => record
            .propulsion_unit_speed_rpm
            .map(|v| crate::klv::length::write_var_uint_min(v as u64)),
        123 => record
            .navsats_in_view
            .map(|v| crate::klv::length::write_var_uint_min(v as u64)),
        124 => record
            .positioning_method_source
            .map(|v| crate::klv::length::write_var_uint_min(v as u64)),
        125 => record
            .platform_status
            .map(|v| crate::klv::length::write_var_uint_min(v.to_wire() as u64)),
        126 => record
            .sensor_control_mode
            .map(|v| crate::klv::length::write_var_uint_min(v.to_wire() as u64)),
        131 => record
            .take_off_time_us
            .map(crate::klv::length::write_var_uint_min),
        133 => record
            .mi_storage_capacity_gb
            .map(|v| crate::klv::length::write_var_uint_min(v as u64)),
        136 => record
            .leap_seconds
            .map(|v| crate::klv::length::write_var_int_min(v as i64)),
        137 => record
            .correction_offset_us
            .map(crate::klv::length::write_var_int_min),
        139 => record.active_payloads.clone(),
        // Pack/list items (single-instance). Tags 102 and
        // 115 are MULTI-INSTANCE and fall to the `_ => None` arm below —
        // their TLVs are emitted directly by `write_typed_fields`'s
        // inline loops over `record.sdcc_flps` / `record.control_commands`,
        // bypassing this once-per-tag function entirely.
        81 => record
            .image_horizon
            .map(|h| {
                let mut buf = Vec::with_capacity(packs::image_horizon_len(&h));
                packs::emit_image_horizon(&h, &mut buf).map(|_| buf)
            })
            .transpose()?,
        116 => record
            .control_command_verification
            .as_ref()
            .map(|ids| {
                let mut buf = Vec::with_capacity(packs::id_list_len(ids));
                packs::emit_id_list(ids, &mut buf).map(|_| buf)
            })
            .transpose()?,
        121 => record
            .active_wavelengths
            .as_ref()
            .map(|ids| {
                let mut buf = Vec::with_capacity(packs::id_list_len(ids));
                packs::emit_id_list(ids, &mut buf).map(|_| buf)
            })
            .transpose()?,
        122 => record
            .country_codes
            .as_ref()
            .map(|cc| {
                let mut buf = Vec::with_capacity(packs::country_codes_len(cc));
                packs::emit_country_codes(cc, &mut buf).map(|_| buf)
            })
            .transpose()?,
        127 => record
            .sensor_frame_rate
            .map(|fr| {
                let mut buf = Vec::with_capacity(packs::sensor_frame_rate_len(&fr));
                packs::emit_sensor_frame_rate(&fr, &mut buf).map(|_| buf)
            })
            .transpose()?,
        128 => record
            .wavelengths_list
            .as_ref()
            .map(|l| {
                let mut buf = Vec::with_capacity(packs::wavelengths_list_len(l));
                packs::emit_wavelengths_list(l, &mut buf).map(|_| buf)
            })
            .transpose()?,
        130 => record
            .airbase_locations
            .map(|al| {
                let mut buf = Vec::with_capacity(packs::airbase_locations_len(&al));
                packs::emit_airbase_locations(&al, &mut buf).map(|_| buf)
            })
            .transpose()?,
        138 => record
            .payload_list
            .as_ref()
            .map(|pl| {
                let mut buf = Vec::with_capacity(packs::payload_list_len(pl));
                packs::emit_payload_list(pl, &mut buf).map(|_| buf)
            })
            .transpose()?,
        140 => record
            .weapons_stores
            .as_ref()
            .map(|l| {
                let mut buf = Vec::with_capacity(packs::weapons_stores_len(l));
                packs::emit_weapons_stores(l, &mut buf).map(|_| buf)
            })
            .transpose()?,
        141 => record
            .waypoint_list
            .as_ref()
            .map(|l| {
                let mut buf = Vec::with_capacity(packs::waypoints_len(l));
                packs::emit_waypoints(l, &mut buf).map(|_| buf)
            })
            .transpose()?,
        142 => record
            .view_domain
            .map(|vd| {
                let mut buf = Vec::with_capacity(packs::view_domain_len(&vd));
                packs::emit_view_domain(&vd, &mut buf).map(|_| buf)
            })
            .transpose()?,
        143 => record
            .metadata_substream_id
            .map(|ms| {
                let mut buf = Vec::with_capacity(packs::metadata_substream_id_len(&ms));
                packs::emit_metadata_substream_id(&ms, &mut buf).map(|_| buf)
            })
            .transpose()?,
        // All 69 LinearRange Option<f64> fields — driven from RANGED_FIELDS so
        // the tag→field mapping is the single source of truth across decode + encode.
        _ if spec.range.is_some() => {
            if let Some(entry) = super::decode::ranged_entry(spec.id) {
                encode_ranged((entry.get)(record), spec, &mut scratch, policy)?
            } else {
                None
            }
        }
        // The 14 IMAPB extended-range fields — share the
        // RANGED_FIELDS accessor table with the LinearRange fields above;
        // the wire format is what differs (encode_imapb_field vs encode_ranged).
        _ if matches!(spec.encoding, Encoding::Imapb { .. }) => {
            let Encoding::Imapb {
                min,
                max,
                default_len,
                ..
            } = spec.encoding
            else {
                unreachable!()
            };
            match super::decode::ranged_entry(spec.id) {
                Some(entry) => encode_imapb_field(
                    (entry.get)(record),
                    spec.id as u32,
                    min,
                    max,
                    default_len,
                    policy,
                )?,
                None => None,
            }
        }
        _ => None,
    })
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
    write_sentinel_tags(record, body)?;
    write_imapb_specials(record, body)?;
    Ok(())
}

/// Emit INT_MIN bytes for each tag in `record.sentinel_tags` whose typed
/// field is currently `None`. If the typed field is `Some(v)`, `encode_tag_value`
/// has already emitted it above (value wins over the sentinel entry).
fn write_sentinel_tags(record: &UasDatalinkLs, body: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    for &st in &record.sentinel_tags {
        let Ok(tag_u8) = u8::try_from(st) else {
            continue;
        };
        let Some(spec) = lookup_tag(tag_u8) else {
            continue;
        };
        let Some(ref range) = spec.range else {
            continue;
        };
        if !range.signed {
            continue;
        }
        // Value wins: only re-emit the sentinel when the typed field is absent.
        if super::decode::ranged_entry(tag_u8)
            .and_then(|e| (e.get)(record))
            .is_some()
        {
            continue;
        }
        // INT_MIN for this field width: 2-byte → 0x8000, 4-byte → 0x80000000.
        let int_min_value: i64 = match range.byte_length {
            2 => i64::from(i16::MIN),
            4 => i64::from(i32::MIN),
            _ => continue,
        };
        let all_bytes = int_min_value.to_be_bytes();
        let sentinel_bytes = &all_bytes[8 - range.byte_length..];
        emit_ber_oid_tlv(st, sentinel_bytes, body)?;
    }
    Ok(())
}

/// Emit the ST 1201.5 special-value bytes for each `(tag, special)` in
/// `record.imapb_specials` whose typed field is currently `None`. If the
/// typed field is `Some(v)`, `encode_tag_value` has already emitted it
/// above (value wins over the special entry) — mirror of
/// `write_sentinel_tags` for the IMAPB fields.
fn write_imapb_specials(record: &UasDatalinkLs, body: &mut Vec<u8>) -> Result<(), KlvEncodeError> {
    for &(tag, special) in &record.imapb_specials {
        let Ok(tag_u8) = u8::try_from(tag) else {
            continue;
        };
        let Some(spec) = lookup_tag(tag_u8) else {
            continue;
        };
        let Encoding::Imapb { default_len, .. } = spec.encoding else {
            continue;
        };
        // Value wins: only re-emit the special when the typed field is absent.
        if super::decode::ranged_entry(tag_u8)
            .and_then(|e| (e.get)(record))
            .is_some()
        {
            continue;
        }
        let mut buf = vec![0u8; default_len];
        crate::klv::imapb::encode_imapb_special(special, default_len, &mut buf)?;
        emit_ber_oid_tlv(tag, &buf, body)?;
    }
    Ok(())
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
    value: Option<f64>,
    spec: &super::tags::TagSpec,
    scratch: &mut [u8; 8],
    policy: OutOfRangePolicy,
) -> Result<Option<Vec<u8>>, KlvEncodeError> {
    let Some(v) = value else { return Ok(None) };
    let r = spec
        .range
        .as_ref()
        .expect("ranged tag must have LinearRange");
    encode_fixed_range(r, spec.id as u32, v, &mut scratch[..r.byte_length], policy)?;
    Ok(Some(scratch[..r.byte_length].to_vec()))
}

/// Encode one ST 1201.5 IMAPB extended-range field — the IMAPB
/// sibling of `encode_ranged`. In-range values encode normally at
/// `default_len`; out-of-range values either error (default policy) or,
/// under [`OutOfRangePolicy::Indicator`], emit the tag's `BelowMin` /
/// `AboveMax` special at `default_len` instead. Non-finite input always
/// errors regardless of policy.
fn encode_imapb_field(
    value: Option<f64>,
    tag: u32,
    min: f64,
    max: f64,
    default_len: usize,
    policy: OutOfRangePolicy,
) -> Result<Option<Vec<u8>>, KlvEncodeError> {
    let Some(v) = value else { return Ok(None) };
    let mut buf = vec![0u8; default_len];
    if v.is_finite() && v >= min && v <= max {
        let p = crate::klv::imapb::ImapbParams {
            min,
            max,
            length: default_len,
        };
        crate::klv::imapb::encode_imapb(&p, v, &mut buf)?;
        return Ok(Some(buf));
    }
    if policy == OutOfRangePolicy::Indicator && v.is_finite() {
        let special = if v < min {
            crate::klv::imapb::ImapbSpecial::BelowMin
        } else {
            crate::klv::imapb::ImapbSpecial::AboveMax
        };
        crate::klv::imapb::encode_imapb_special(special, default_len, &mut buf)?;
        return Ok(Some(buf));
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
    if let Encoding::Utf8 { max_bytes } = enc {
        if s.len() > *max_bytes {
            return Err(KlvEncodeError::StringTooLong {
                tag,
                max: *max_bytes,
            });
        }
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
