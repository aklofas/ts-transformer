//! ES payload parsers: H.264 / H.265 NAL split, KLV unwrap.

use crate::codec::annexb::start_codes;
use crate::codec::av1::decode::leb128::read_leb128;
use crate::mpegts::demux::event::{
    Av1ObuHeaderKind, NalHeaderKind, NalUnit, NonConformantIssue, Obu, ObuExtension, VideoCodec,
    VideoPayload,
};
use crate::shared::SharedBytes;
use alloc::vec::Vec;

/// Split an Annex-B-framed elementary stream payload into typed NAL units.
///
/// Looks for `0x000001` or `0x00000001` start codes. NAL bytes between
/// start codes are passed through (RBSP with emulation-prevention bytes
/// preserved); the consumer's decoder removes the 0x03 escapes.
///
/// Returns `(nals, issues)`. `issues` carries any NAL-header constraint
/// violations detected per H.264 §7.3.1 / H.265 §7.3.1.2 / H.266 §7.3.1.2
/// (forbidden_zero_bit set, reserved bits non-zero, etc.). Issues use
/// sentinel `codec` carried verbatim; the caller annotates with PID at
/// queue-time. NALs for which the spec mandates discard (H.266 reserved /
/// layer>55) are dropped from the output but the issue is still emitted.
pub fn split_nals(
    es_payload: &SharedBytes,
    codec: VideoCodec,
) -> (Vec<NalUnit>, Vec<NonConformantIssue>) {
    let mut out = Vec::new();
    let mut issues = Vec::new();
    let bytes: &[u8] = es_payload;
    let starts = start_codes(bytes);
    for win in starts.windows(2) {
        // `data_start` is the offset of the first NAL byte after this NAL's
        // start-code prefix; `prefix_start` of the next entry is where the
        // following NAL's start-code begins. Slicing `[data_start..prefix_start]`
        // yields exactly this NAL's bytes with no inter-NAL prefix bleed.
        let data_start = win[0].data_start;
        let nal_end = win[1].prefix_start;
        if let Some(unit) = parse_one_nal(es_payload, data_start, nal_end, codec, &mut issues) {
            out.push(unit);
        }
    }
    if let Some(&last) = starts.last() {
        if let Some(unit) =
            parse_one_nal(es_payload, last.data_start, bytes.len(), codec, &mut issues)
        {
            out.push(unit);
        }
    }
    (out, issues)
}

/// Parse a single NAL unit at `es_payload[nal_start..nal_end]`.
///
/// `nal_start` and `nal_end` are byte offsets into `es_payload` pointing at
/// the first NAL header byte (start code already consumed) through the last
/// body byte. The payload field of the returned unit is a zero-copy
/// `SharedBytes` view sharing `es_payload`'s backing allocation.
fn parse_one_nal(
    es_payload: &SharedBytes,
    nal_start: usize,
    nal_end: usize,
    codec: VideoCodec,
    issues: &mut Vec<NonConformantIssue>,
) -> Option<NalUnit> {
    let nal: &[u8] = &es_payload[nal_start..nal_end];
    match codec {
        VideoCodec::H264 => {
            if nal.is_empty() {
                return None;
            }
            let header = nal[0];
            // forbidden_zero_bit (1) | nal_ref_idc (2) | nal_unit_type (5)
            // Per H.264 §7.3.1: forbidden_zero_bit MUST be 0. H.264 has no
            // reserved bit and no temporal-id field at the NAL header level.
            if (header & 0x80) != 0 {
                issues.push(NonConformantIssue::NalHeader {
                    codec,
                    kind: NalHeaderKind::ForbiddenZeroBit,
                });
            }
            let ref_idc = (header >> 5) & 0x03;
            let nal_type = header & 0x1F;
            // nal[0] is the 1-byte header; body starts at nal_start+1.
            let payload = es_payload.slice((nal_start + 1)..nal_end);
            Some(NalUnit::H264 {
                nal_type,
                ref_idc,
                payload,
            })
        }
        VideoCodec::H265 => {
            if nal.len() < 2 {
                return None;
            }
            // forbidden_zero_bit (1) | nal_unit_type (6) | nuh_layer_id (6) | nuh_temporal_id_plus1 (3)
            // Per H.265 §7.3.1.2: forbidden_zero_bit MUST be 0;
            // nuh_temporal_id_plus1 MUST be != 0 (plus-1 encoding reserves
            // 0 as forbidden so decoders can distinguish missing/sync).
            let h0 = nal[0];
            let h1 = nal[1];
            if (h0 & 0x80) != 0 {
                issues.push(NonConformantIssue::NalHeader {
                    codec,
                    kind: NalHeaderKind::ForbiddenZeroBit,
                });
            }
            let nal_type = (h0 >> 1) & 0x3F;
            let layer_id = ((h0 & 0x01) << 5) | (h1 >> 3);
            let temporal_id_plus1 = h1 & 0x07;
            if temporal_id_plus1 == 0 {
                issues.push(NonConformantIssue::NalHeader {
                    codec,
                    kind: NalHeaderKind::ZeroTemporalIdPlus1,
                });
            }
            // nal[0..2] are the 2-byte header; body starts at nal_start+2.
            let payload = es_payload.slice((nal_start + 2)..nal_end);
            Some(NalUnit::H265 {
                nal_type,
                layer_id,
                temporal_id_plus1,
                payload,
            })
        }
        VideoCodec::H266 => {
            if nal.len() < 2 {
                return None;
            }
            // Per H.266 V4 §7.3.1.2:
            //   byte 0: forbidden_zero_bit(1) | nuh_reserved_zero_bit(1) | nuh_layer_id(6)
            //   byte 1: nal_unit_type(5)      | nuh_temporal_id_plus1(3)
            //
            // Note nal_type lives in byte 1 (top 5 bits); H.265 has it in
            // byte 0 — different layout, not just renamed fields.
            //
            // Spec constraints:
            // - forbidden_zero_bit MUST be 0.
            // - nuh_reserved_zero_bit MUST be 0; receivers MUST discard
            //   NALs that violate this (H.266 §7.3.1.2). Drop the NAL.
            // - nuh_layer_id MUST be in 0..=55 (§7.4.2.2); values 56..=63
            //   are reserved and receivers MUST discard such NALs.
            // - nuh_temporal_id_plus1 MUST be != 0.
            let h0 = nal[0];
            let h1 = nal[1];
            let forbidden = (h0 & 0x80) != 0;
            let reserved = (h0 & 0x40) != 0;
            let layer_id = h0 & 0x3F;
            let nal_type = (h1 >> 3) & 0x1F;
            let temporal_id_plus1 = h1 & 0x07;
            if forbidden {
                issues.push(NonConformantIssue::NalHeader {
                    codec,
                    kind: NalHeaderKind::ForbiddenZeroBit,
                });
            }
            if reserved {
                issues.push(NonConformantIssue::NalHeader {
                    codec,
                    kind: NalHeaderKind::ReservedBit,
                });
                // H.266 §7.3.1.2 requires receivers to discard NALs with
                // nuh_reserved_zero_bit set. Emit issue + drop.
                return None;
            }
            if layer_id > 55 {
                issues.push(NonConformantIssue::NalHeader {
                    codec,
                    kind: NalHeaderKind::LayerIdOutOfRange { id: layer_id },
                });
                // H.266 §7.4.2.2: layer_id in 56..=63 is reserved; receivers
                // MUST discard. Emit issue + drop.
                return None;
            }
            if temporal_id_plus1 == 0 {
                issues.push(NonConformantIssue::NalHeader {
                    codec,
                    kind: NalHeaderKind::ZeroTemporalIdPlus1,
                });
            }
            // nal[0..2] are the 2-byte header; body starts at nal_start+2.
            let payload = es_payload.slice((nal_start + 2)..nal_end);
            Some(NalUnit::H266 {
                nal_type,
                layer_id,
                temporal_id_plus1,
                payload,
            })
        }
        VideoCodec::Av1 => {
            // AV1 is OBU-shaped, not NAL-shaped. `split_video` dispatches AV1
            // to `split_obus` before this function is ever called; reaching
            // this arm means the splitter was mis-dispatched. Defense-in-depth:
            // don't panic — return None and emit a debug_assert so test runs
            // catch any regression that routes AV1 here.
            //
            // Typed-error promotion (`parse_one_nal -> Result<Option<_>, _>`)
            // is deferred to Phase 1's SemVer ratchet — would cascade through
            // `split_nals`, its call sites, and existing tests.
            debug_assert!(
                false,
                "internal: AV1 reached NAL splitter; split dispatch is inconsistent"
            );
            None
        }
    }
}

/// Split an AV1 PES payload into typed OBUs. Per AV1 spec §5.3
/// "low overhead bitstream format" framing.
///
/// Returns `(obus, issues)`. `issues` contains any non-conformance
/// issues raised during the walk (missing obu_size field, forbidden
/// Tile List OBU); the caller surfaces these to its consumer. `pid` on
/// each issue is left as a sentinel `0` — the opt-in `split_video` parse
/// path that calls this has no PID context.
///
/// On a malformed buffer (truncated header, truncated LEB128, length
/// runs past buffer end) the splitter stops and returns what it has
/// accumulated, mirroring `split_nals`'s lenient stance.
pub fn split_obus(es_payload: &SharedBytes) -> (Vec<Obu>, Vec<NonConformantIssue>) {
    let mut out = Vec::new();
    let mut issues = Vec::new();
    let bytes: &[u8] = es_payload;
    let mut i = 0usize;
    while i < bytes.len() {
        // OBU header byte (AV1 §5.3.2):
        //   obu_forbidden_bit f(1)  — must be 0
        //   obu_type           f(4)
        //   obu_extension_flag f(1)
        //   obu_has_size_field f(1)
        //   obu_reserved_1bit  f(1)
        let header = bytes[i];
        // Validate the spec-mandated forbidden + reserved bits before
        // peeling off the field-level decode. `pid` is sentinel 0 — the
        // opt-in `split_video` parse path has no PID context (same pattern
        // as the other AV1 issues this splitter emits).
        if (header & 0x80) != 0 {
            issues.push(NonConformantIssue::Av1ObuHeader {
                pid: 0,
                kind: Av1ObuHeaderKind::ForbiddenBit,
            });
        }
        if (header & 0x01) != 0 {
            issues.push(NonConformantIssue::Av1ObuHeader {
                pid: 0,
                kind: Av1ObuHeaderKind::ReservedBit,
            });
        }
        let obu_type = (header >> 3) & 0x0F;
        let extension_flag = (header >> 2) & 0x01 != 0;
        let has_size_field = (header >> 1) & 0x01 != 0;
        i += 1;

        let extension = if extension_flag {
            if i >= bytes.len() {
                break; // truncated extension — stop
            }
            let ext = bytes[i];
            i += 1;
            // temporal_id(3) | spatial_id(2) | reserved(3)
            // Per AV1 §5.3.3 the low 3 reserved bits MUST be 0.
            if (ext & 0x07) != 0 {
                issues.push(NonConformantIssue::Av1ObuHeader {
                    pid: 0,
                    kind: Av1ObuHeaderKind::ExtensionReservedBits,
                });
            }
            Some(ObuExtension {
                temporal_id: (ext >> 5) & 0x07,
                spatial_id: (ext >> 3) & 0x03,
            })
        } else {
            None
        };

        if !has_size_field {
            issues.push(NonConformantIssue::Av1ObuMissingSizeField {
                pid: 0, // sentinel — opt-in split_video path has no PID context
                obu_type,
            });
            // Zero-copy view: body runs from i to end of es_payload.
            let payload = es_payload.slice(i..bytes.len());
            out.push(Obu {
                obu_type,
                extension,
                payload,
            });
            break;
        }

        let (obu_size, consumed) = match read_leb128(bytes, i) {
            Ok(t) => t,
            Err(_) => break, // truncated LEB128 — stop
        };
        i += consumed;

        // AV1 §4.10.5 guarantees obu_size <= u32::MAX (read_leb128 enforces
        // it), so try_from is infallible on 64-bit and safe on 32-bit; the
        // checked_add prevents `i + obu_size` from wrapping near usize::MAX.
        let obu_size = match usize::try_from(obu_size) {
            Ok(n) => n,
            Err(_) => break,
        };
        let body_end = match i.checked_add(obu_size) {
            Some(end) if end <= bytes.len() => end,
            _ => break,
        };
        // Zero-copy view: body runs from i to body_end.
        let payload = es_payload.slice(i..body_end);
        i = body_end;

        // Tile List OBU non-conformance issue per binding §3.3.
        if obu_type == 8 {
            issues.push(NonConformantIssue::Av1TileListNotAllowed { pid: 0 });
        }

        out.push(Obu {
            obu_type,
            extension,
            payload,
        });
    }
    (out, issues)
}

/// Opt-in video ES parse. Splits the encoded access unit `raw` into NAL units
/// (H.26x) or OBUs (AV1), returning the parsed payload plus any ES-layer
/// non-conformance issues observed (lenient — issues are reported, not fatal).
///
/// The returned NAL/OBU payloads are zero-copy `SharedBytes` views into `raw`.
///
/// For AV1 the `av1_carriage` parameter governs framing expectations (H.26x
/// ignores it):
///
/// - **`Mpeg2TsBinding`** ([`crate::mpegts::mux::Av1CarriageMode::Mpeg2TsBinding`]):
///   reverses `ts_open_bitstream_unit()` binding framing (§3.2) when present;
///   if framing is absent the OBUs are split directly and
///   [`NonConformantIssue::Av1MissingTsObuFraming`] is included (binding
///   REQUIRES framing, so its absence is non-conformant).
/// - **`InteropRawObu`** ([`crate::mpegts::mux::Av1CarriageMode::InteropRawObu`]):
///   splits raw OBUs directly and does NOT surface `Av1MissingTsObuFraming`
///   — absent framing is correct for interop carriage (AV1-03).
///
/// Pass the `av1_carriage` field from the demux
/// [`crate::mpegts::demux::SamplePayload::Video`] to get the right behavior.
pub fn split_video(
    raw: &SharedBytes,
    codec: VideoCodec,
    av1_carriage: crate::mpegts::mux::Av1CarriageMode,
) -> (VideoPayload, Vec<NonConformantIssue>) {
    use crate::mpegts::mux::Av1CarriageMode;
    match codec {
        VideoCodec::H264 | VideoCodec::H265 | VideoCodec::H266 => {
            let (nals, issues) = split_nals(raw, codec);
            (VideoPayload::Nals(nals), issues)
        }
        VideoCodec::Av1 => match av1_carriage {
            Av1CarriageMode::Mpeg2TsBinding => match unwrap_av1_binding(raw) {
                Av1BindingUnwrap::Conformant(unwrapped) => {
                    // Binding-framed: OBU views point into the unwrapped buffer
                    // (the documented AV1 ~2× allocation case).
                    let (obus, issues) = split_obus(&SharedBytes::from_vec(unwrapped));
                    (VideoPayload::Obus(obus), issues)
                }
                Av1BindingUnwrap::MissingFraming => {
                    // Binding REQUIRES ts_open_bitstream_unit framing; its
                    // absence is genuinely non-conformant here.
                    // `pid: 0` sentinel — this opt-in path has no PID context
                    // (matching how `split_obus` sentinel-pids its own issues).
                    let (obus, mut issues) = split_obus(raw);
                    issues.insert(0, NonConformantIssue::Av1MissingTsObuFraming { pid: 0 });
                    (VideoPayload::Obus(obus), issues)
                }
            },
            Av1CarriageMode::InteropRawObu => {
                // Interop carriage carries raw OBUs by design — split directly
                // and do NOT flag missing framing (AV1-03).
                let (obus, issues) = split_obus(raw);
                (VideoPayload::Obus(obus), issues)
            }
        },
    }
}

/// Strict variant: returns `Err(issue)` on the first ES-conformance issue,
/// mirroring `klv::st0601::decode_strict`. Use when the caller wants
/// malformed-ES rejection (the responsibility the demuxer's `StrictMode` no
/// longer carries for ES content).
///
/// The `av1_carriage` parameter is passed through to [`split_video`]; see its
/// doc for carriage semantics. Pass the `av1_carriage` field from the demux
/// [`crate::mpegts::demux::SamplePayload::Video`] so that interop samples
/// parse cleanly under interop carriage (AV1-03).
pub fn split_video_strict(
    raw: &SharedBytes,
    codec: VideoCodec,
    av1_carriage: crate::mpegts::mux::Av1CarriageMode,
) -> Result<VideoPayload, NonConformantIssue> {
    let (payload, mut issues) = split_video(raw, codec, av1_carriage);
    if !issues.is_empty() {
        return Err(issues.swap_remove(0));
    }
    Ok(payload)
}

/// Outcome of the AV1-binding `ts_open_bitstream_unit()` unwrap step.
///
/// Used by the opt-in [`split_video`] AV1 parse to detect binding-framed vs.
/// raw-OBU (interop) carriage: binding-framed input is unwrapped before the
/// OBU walk, while a missing start code falls back to raw-OBU parsing with a
/// [`NonConformantIssue::Av1MissingTsObuFraming`] issue surfaced.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Av1BindingUnwrap {
    /// PES payload was binding-framed: 3-byte start code `0x00 0x00 0x01`
    /// observed at offset 0, OBU bytes unwrapped (emulation-prevention `0x03`
    /// bytes stripped). Returned vector is ready for [`split_obus`].
    Conformant(Vec<u8>),
    /// PES payload did not start with the `ts_open_bitstream_unit` start
    /// code. Caller should surface
    /// [`NonConformantIssue::Av1MissingTsObuFraming`] and fall through to
    /// raw-OBU parsing.
    MissingFraming,
}

/// Unwrap AV1-binding `ts_open_bitstream_unit()` framing from a PES payload.
///
/// Per AV1-in-MPEG-2-TS binding §3.2 the on-wire PES payload is a
/// sequence of `ts_open_bitstream_unit()` invocations, one per OBU:
///
/// ```text
/// [0x00 0x00 0x01] [escape(OBU1)] [0x00 0x00 0x01] [escape(OBU2)] …
/// ```
///
/// - Each OBU is prefixed with the 3-byte sequence `0x00 0x00 0x01`
///   (`obu_start_code` is `uimsbf(24)` with value `0x000001`).
/// - Inside each unit body, any sequence `0x00 0x00 0x0X` with
///   `X ∈ {0x00, 0x01, 0x02, 0x03}` has had a `0x03` emulation-prevention
///   byte inserted between the second `0x00` and the `0x0X`; the unwrap
///   strips those. (Including the `X == 0x03` case is required: the
///   decoder consumes every `0x00 0x00 0x03` triple as an escape, so an
///   OBU body byte `0x03` after `0x00 0x00` was wired as
///   `0x00 0x00 0x03 0x03` by the encoder.)
/// - The emulation-prevention rule makes start codes uniquely detectable
///   on the wire: a real OBU body byte `0x01` following `0x00 0x00`
///   would have been escaped to `0x00 0x00 0x03 0x01` by the encoder,
///   so when we see an UN-escaped `0x00 0x00 0x01` past the first start
///   code it can only be a NEW OBU boundary.
///
/// Returns the concatenated unescaped low-overhead OBU bytestream — ready
/// to feed to [`split_obus`]. Each `escape(OBU_n)` is reversed independently
/// (zero-run state resets at each start code boundary) and the recovered
/// OBU bytes are appended back-to-back.
///
/// On a malformed input (start code missing) the unwrap returns
/// [`Av1BindingUnwrap::MissingFraming`]; [`split_video`] treats that as a
/// non-conformance signal and falls back to raw-OBU parsing. A truncated
/// escape near end-of-payload is tolerated by emitting the trailing bytes
/// verbatim (lenient stance — matches `split_obus`).
pub(crate) fn unwrap_av1_binding(payload: &[u8]) -> Av1BindingUnwrap {
    // Binding §3.2 start code is 3 bytes; anything shorter can't carry it.
    if payload.len() < 3 || payload[0..3] != [0x00, 0x00, 0x01] {
        return Av1BindingUnwrap::MissingFraming;
    }
    // Walk the body byte-by-byte, tracking the trailing-zero count to
    // disambiguate three on-wire patterns:
    //   (1) `0x00 0x00 0x01` — a NEW start code (next OBU). Pop the two
    //       trailing zeros we just appended to `out` (they were the
    //       start-code prefix, not body bytes) and continue into the
    //       next unit's body.
    //   (2) `0x00 0x00 0x03 X` with `X ≤ 0x03` — emulation-prevention
    //       escape. Drop the 0x03; the 0x00 0x00 are real body bytes;
    //       reset the zero-run so a subsequent 0x00 starts a fresh run.
    //   (3) anything else — body byte; push verbatim.
    //
    // The mux guarantees (1) and (2) are mutually exclusive on
    // conformant input: a literal body 0x01 after 0x00 0x00 is wired as
    // (2) with X=0x01; a literal body 0x03 after 0x00 0x00 is wired as
    // (2) with X=0x03. So an un-escaped 0x00 0x00 0x01 is ALWAYS a start
    // code, never a body sequence.
    let mut out = Vec::with_capacity(payload.len().saturating_sub(3));
    let mut zero_run = 0u8;
    let mut i = 3usize;
    while i < payload.len() {
        let b = payload[i];
        if zero_run >= 2 && b == 0x01 {
            // New start code. The two 0x00 bytes immediately preceding
            // this 0x01 in `out` were the start-code prefix, not body
            // bytes — truncate them off. (`out.len() >= 2` is guaranteed
            // because zero_run >= 2 implies we pushed at least two
            // 0x00s into `out` since the last reset.)
            let new_len = out.len() - 2;
            out.truncate(new_len);
            zero_run = 0;
            i += 1;
            continue;
        }
        if zero_run >= 2 && b == 0x03 && i + 1 < payload.len() && payload[i + 1] <= 0x03 {
            // Drop the emulation-prevention byte; reset zero_run so the
            // next 0x00 starts a fresh run.
            zero_run = 0;
            i += 1;
            continue;
        }
        out.push(b);
        if b == 0x00 {
            zero_run = zero_run.saturating_add(1);
        } else {
            zero_run = 0;
        }
        i += 1;
    }
    Av1BindingUnwrap::Conformant(out)
}

/// KLV payload classification result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KlvShape {
    /// Sync metadata — first cell parses as a valid H.222.0 AU cell header.
    /// The full cell walk + reassembly is done by the caller via
    /// [`iter_au_cells`] + the reassembler.
    Sync,
    /// Async-shape KLV (bare SMPTE UL at offset 0). `klv` is the unwrapped
    /// LS bytes ready to feed to `klv::st0601::decode`.
    Async { klv: Vec<u8> },
    /// Payload is something else; pass-through as `Unknown`.
    Other,
}

/// Sniff a KLV PES payload shape: sync (first 5 bytes look like a valid AU
/// cell header) vs async (first 4 bytes are the SMPTE UL `06 0E 2B 34`)
/// vs other.
///
/// Used by both lenient and strict modes. The KlvSync branch of the
/// demuxer's emit path uses [`iter_au_cells`] for the actual cell walk
/// + reassembly; this function only decides which branch to take.
pub fn classify_klv(payload: &[u8]) -> KlvShape {
    use crate::mpegts::au_cell::read_metadata_au_cell;

    if payload.len() >= 5 && read_metadata_au_cell(payload).is_ok() {
        return KlvShape::Sync;
    }

    if payload.len() >= 16 && payload[0..4] == [0x06, 0x0E, 0x2B, 0x34] {
        return KlvShape::Async {
            klv: payload.to_vec(),
        };
    }

    KlvShape::Other
}

/// Walk back-to-back `Metadata_AU_cell` records inside a PES payload.
///
/// Yields one `Result<(AuCellHeader, &[u8]), KlvDecodeError>` per cell,
/// stopping when the payload is fully consumed or a parse error is
/// reached. The iterator is lazy and zero-copy: each emitted `&[u8]`
/// borrows from the input slice.
///
/// Per H.222.0 V9 §2.12.4, a sync-metadata PES MAY carry multiple AU
/// cells back-to-back. The previous `classify_klv` path only inspected
/// the first cell; this iterator gives the caller every cell so the
/// reassembler can thread fragments across PES boundaries.
pub fn iter_au_cells(
    payload: &[u8],
) -> impl Iterator<
    Item = Result<(crate::mpegts::au_cell::AuCellHeader, &[u8]), crate::error::KlvDecodeError>,
> + '_ {
    use crate::mpegts::au_cell::read_metadata_au_cell;
    let mut offset = 0usize;
    core::iter::from_fn(move || {
        if offset >= payload.len() {
            return None;
        }
        let remaining = &payload[offset..];
        match read_metadata_au_cell(remaining) {
            Ok((header, inner)) => {
                offset += 5 + inner.len();
                Some(Ok((header, inner)))
            }
            Err(e) => {
                // Stop at the first parse error.
                offset = payload.len();
                Some(Err(e))
            }
        }
    })
}

/// Outcome of parsing the EN 300 743 §6.2 PES_data_field envelope on a
/// DVB-subtitle PES payload.
///
/// The envelope wire format is:
///
/// ```text
///   data_identifier(1) + subtitle_stream_id(1) + segments(N) + end_marker(0xFF)
/// ```
///
/// Three terminal shapes per the spec + permissive carriage handling:
///
/// * `Conformant` — `data_identifier == 0x20`, `subtitle_stream_id == 0x00`,
///   end marker present. Strip and emit.
/// * `NonConformantDataId` — envelope is well-formed (stream_id + marker
///   match) but `data_identifier` falls in the legacy permissive carriage
///   range (`0x20..=0x3F | 0x70..=0x7F` per EN 300 743 §7.1) other than the
///   exact `0x20` required by §6.2 Table 3. Stripping still produces
///   sensible segment bytes; caller decides whether to emit a sample.
/// * `Malformed` — envelope shape doesn't match (too short, bad stream_id,
///   missing end marker, or data_identifier completely outside the
///   permissive range). Caller falls through to passthrough so strict-mode
///   consumers can still observe the unexpected payload shape.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DvbSubStripResult<'a> {
    /// Conformant envelope per EN 300 743 §6.2 Table 3.
    Conformant(&'a [u8]),
    /// Envelope well-formed but `data_identifier != 0x20`. The legacy
    /// permissive range (`0x20..=0x3F | 0x70..=0x7F`) is accepted here so
    /// the caller can choose strict reject vs. lenient pass-through.
    NonConformantDataId { observed: u8, stripped: &'a [u8] },
    /// Envelope shape invalid — caller should fall back to passthrough.
    Malformed,
}

/// Parse the EN 300 743 §6.2 PES_data_field envelope from a DVB-subtitle
/// PES payload.
///
/// See [`DvbSubStripResult`] for the per-outcome contract. The
/// `subtitle_stream_id` is fixed at `0x00` per §6.2. The end-marker byte
/// `0xFF` per §6.2.
pub(crate) fn strip_dvb_sub_envelope(bytes: &[u8]) -> DvbSubStripResult<'_> {
    if bytes.len() < 3 {
        return DvbSubStripResult::Malformed;
    }
    let data_id = bytes[0];
    let in_permissive_range = matches!(data_id, 0x20..=0x3F | 0x70..=0x7F);
    if !in_permissive_range || bytes[1] != 0x00 || *bytes.last().unwrap() != 0xFF {
        return DvbSubStripResult::Malformed;
    }
    let stripped = &bytes[2..bytes.len() - 1];
    if data_id == 0x20 {
        DvbSubStripResult::Conformant(stripped)
    } else {
        DvbSubStripResult::NonConformantDataId {
            observed: data_id,
            stripped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_two_nals() {
        // 0x09 access_unit_delimiter then 0x05 IDR.
        let buf = SharedBytes::from_vec(vec![
            0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB,
        ]);
        let (nals, issues) = split_nals(&buf, VideoCodec::H264);
        assert_eq!(nals.len(), 2);
        assert!(issues.is_empty(), "conformant input emits no issues");
        match &nals[0] {
            NalUnit::H264 {
                nal_type, payload, ..
            } => {
                assert_eq!(*nal_type, 9);
                assert_eq!(payload.as_slice(), &[0x10]);
            }
            _ => panic!("wrong codec"),
        }
        match &nals[1] {
            NalUnit::H264 { nal_type, .. } => assert_eq!(*nal_type, 5),
            _ => panic!("wrong codec"),
        }
    }

    #[test]
    fn h265_two_nals() {
        // VPS (32) + IDR_W_RADL (19) with layer_id=0, temporal_id_plus1=1.
        let buf = SharedBytes::from_vec(vec![
            0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0xAA, 0x00, 0x00, 0x01, 0x26, 0x01, 0xBB,
        ]);
        let (nals, issues) = split_nals(&buf, VideoCodec::H265);
        assert_eq!(nals.len(), 2);
        assert!(issues.is_empty(), "conformant input emits no issues");
        match &nals[0] {
            NalUnit::H265 {
                nal_type,
                layer_id,
                temporal_id_plus1,
                ..
            } => {
                assert_eq!(*nal_type, 32);
                assert_eq!(*layer_id, 0);
                assert_eq!(*temporal_id_plus1, 1);
            }
            _ => panic!("wrong codec"),
        }
        match &nals[1] {
            NalUnit::H265 { nal_type, .. } => assert_eq!(*nal_type, 19),
            _ => panic!("wrong codec"),
        }
    }

    #[test]
    fn classifies_async_klv_by_ul_prefix() {
        let mut buf = vec![0x06, 0x0E, 0x2B, 0x34];
        buf.extend(core::iter::repeat_n(0xAA, 30));
        assert_eq!(classify_klv(&buf), KlvShape::Async { klv: buf });
    }

    #[test]
    fn classifies_sync_klv_via_h222_au_cell_header() {
        // 5-byte H.222.0 §2.12.4.2 Metadata_AU_cell header
        // [metadata_service_id][sequence_number][flags][AU_cell_data_length BE]
        // followed by 16-byte SMPTE UL + 10 filler bytes.
        // AU_cell_data_length = 26 bytes (16 UL + 10 body). This is the
        // spec-conformant sync KLV wire form per ST 1402.2 §9.4.1.
        //
        // classify_klv now only returns the SHAPE (Sync / Async / Other);
        // the cell walk + header surface lives in `iter_au_cells`.
        let inner: Vec<u8> = [0x06, 0x0E, 0x2B, 0x34]
            .into_iter()
            .chain(core::iter::repeat_n(0xAA, 12))
            .chain(core::iter::repeat_n(0x55, 10))
            .collect();
        // flags byte 0xCF: cfi=11 (Complete), dcf=0, rai=0, reserved=1111.
        // Complete = 0b11 in bits [7:6]; 0b11_0_0_1111 = 0xCF.
        let mut buf = vec![0x00, 0xB7, 0xCF, 0x00, 0x1A];
        buf.extend_from_slice(&inner);
        assert_eq!(classify_klv(&buf), KlvShape::Sync);

        // Pull the AU cell header out via iter_au_cells to confirm the
        // wrapper fields are surfaced for downstream consumers.
        let cells: Vec<_> = iter_au_cells(&buf).collect();
        assert_eq!(cells.len(), 1);
        let (header, inner_recovered) = cells[0].as_ref().unwrap();
        assert_eq!(*inner_recovered, &inner[..]);
        assert_eq!(header.metadata_service_id, 0x00);
        assert_eq!(header.sequence_number, 0xB7);
    }

    #[test]
    fn classifies_unknown_payload() {
        assert_eq!(classify_klv(&[0xDE, 0xAD, 0xBE, 0xEF]), KlvShape::Other);
    }

    #[test]
    fn h264_single_nal_3byte_start() {
        // Single NAL preceded by the 3-byte start code — exercises the
        // trailing-NAL branch of split_nals where the inner-NAL loop
        // doesn't fire. Locks in the fix for the start_codes
        // slice-boundary bug (NAL bytes must NOT include any next-NAL
        // prefix bytes — there's no next NAL here).
        let buf = SharedBytes::from_vec(vec![0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB]);
        let (nals, issues) = split_nals(&buf, VideoCodec::H264);
        assert_eq!(nals.len(), 1);
        assert!(issues.is_empty());
        match &nals[0] {
            NalUnit::H264 {
                nal_type, payload, ..
            } => {
                assert_eq!(*nal_type, 7); // SPS
                assert_eq!(payload.as_slice(), &[0xAA, 0xBB]);
            }
            _ => panic!("wrong codec"),
        }
    }

    #[test]
    fn split_nals_empty_input() {
        // Empty input produces no NALs. start_codes returns vec![],
        // both the inner-window loop and the trailing-NAL branch no-op.
        let empty = SharedBytes::from_vec(vec![]);
        let (nals_264, issues_264) = split_nals(&empty, VideoCodec::H264);
        assert!(nals_264.is_empty() && issues_264.is_empty());
        let (nals_265, issues_265) = split_nals(&empty, VideoCodec::H265);
        assert!(nals_265.is_empty() && issues_265.is_empty());
    }

    #[test]
    fn h266_two_nals() {
        // VPS_NUT (14) + IDR_W_RADL (7) with layer_id=0, temporal_id_plus1=1.
        // H.266 NAL header (per H.266 V4 §7.3.1.2):
        //   byte 0: forbidden(1) | reserved(1) | nuh_layer_id(6)
        //   byte 1: nal_unit_type(5) | nuh_temporal_id_plus1(3)
        //
        // VPS: layer_id=0, nal_type=14(0b01110), temporal_id_plus1=1 →
        //   byte0 = 0x00 (forbidden=0, reserved=0, layer_id=0)
        //   byte1 = (14 << 3) | 1 = 0x71
        //
        // IDR_W_RADL: layer_id=0, nal_type=7(0b00111), temporal_id_plus1=1 →
        //   byte0 = 0x00
        //   byte1 = (7 << 3) | 1 = 0x39
        let buf = SharedBytes::from_vec(vec![
            0x00, 0x00, 0x00, 0x01, 0x00, 0x71, 0xAA, 0x00, 0x00, 0x01, 0x00, 0x39, 0xBB,
        ]);
        let (nals, issues) = split_nals(&buf, VideoCodec::H266);
        assert_eq!(nals.len(), 2);
        assert!(issues.is_empty(), "conformant H.266 emits no issues");
        match &nals[0] {
            NalUnit::H266 {
                nal_type,
                layer_id,
                temporal_id_plus1,
                payload,
            } => {
                assert_eq!(*nal_type, 14);
                assert_eq!(*layer_id, 0);
                assert_eq!(*temporal_id_plus1, 1);
                assert_eq!(payload.as_slice(), &[0xAA]);
            }
            _ => panic!("wrong codec variant"),
        }
        match &nals[1] {
            NalUnit::H266 { nal_type, .. } => assert_eq!(*nal_type, 7),
            _ => panic!("wrong codec variant"),
        }
    }

    #[test]
    fn split_nals_no_start_codes() {
        // Bytes with no start code at all produce no NALs. Garbage
        // before/between expected boundaries is silently discarded —
        // sync recovery is the demuxer state machine's job (Task 7),
        // not the leaf NAL splitter's.
        let buf = SharedBytes::from_vec(vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE]);
        let (nals, issues) = split_nals(&buf, VideoCodec::H264);
        assert!(nals.is_empty() && issues.is_empty());
    }

    fn build_obu_with_size(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        // Header: obu_type(4) | ext_flag(1)=0 | has_size(1)=1 | reserved(1)=0
        // → byte = (obu_type << 3) | 0b010 = (obu_type << 3) | 0x02
        let header = (obu_type << 3) | 0x02;
        let mut v = vec![header];
        // LEB128 size — for sizes < 128, single byte equal to size.
        let size = payload.len();
        assert!(size < 128, "test helper supports only small payloads");
        v.push(size as u8);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn split_obus_two_obus() {
        let mut raw = Vec::new();
        raw.extend(build_obu_with_size(2, &[])); // Temporal Delimiter (empty)
        raw.extend(build_obu_with_size(1, &[0xAA, 0xBB])); // Sequence Header (placeholder)
        let buf = SharedBytes::from_vec(raw);
        let (obus, issues) = split_obus(&buf);
        assert_eq!(obus.len(), 2);
        assert_eq!(obus[0].obu_type, 2);
        assert_eq!(obus[1].obu_type, 1);
        assert_eq!(obus[1].payload.as_slice(), &[0xAA, 0xBB]);
        assert!(issues.is_empty());
    }

    #[test]
    fn split_obus_missing_size_field_reports_issue() {
        // obu_type=1 (Seq Header), ext_flag=0, has_size=0
        let header = 1 << 3;
        let buf = SharedBytes::from_vec(vec![header, 0xAA, 0xBB, 0xCC]);
        let (obus, issues) = split_obus(&buf);
        assert_eq!(obus.len(), 1);
        assert_eq!(obus[0].payload.as_slice(), &[0xAA, 0xBB, 0xCC]);
        assert!(matches!(
            issues.first(),
            Some(NonConformantIssue::Av1ObuMissingSizeField { .. })
        ));
    }

    #[test]
    fn split_obus_tile_list_reports_issue() {
        let buf = SharedBytes::from_vec(build_obu_with_size(8, &[0x00])); // Tile List (forbidden in TS)
        let (obus, issues) = split_obus(&buf);
        assert_eq!(obus.len(), 1);
        assert!(matches!(
            issues.first(),
            Some(NonConformantIssue::Av1TileListNotAllowed { .. })
        ));
    }

    #[test]
    fn split_obus_truncated_leb128_stops_walking() {
        // Header byte then a continuation byte with no terminator, and one
        // more (header for a hypothetical second OBU we should never reach).
        let buf = SharedBytes::from_vec(vec![0x12, 0x80]); // single OBU header + truncated LEB128
        let (obus, _) = split_obus(&buf);
        assert!(obus.is_empty(), "truncated LEB128 should abort the walk");
    }

    #[test]
    fn split_obus_empty_input_returns_empty() {
        let buf = SharedBytes::from_vec(vec![]);
        let (obus, issues) = split_obus(&buf);
        assert!(obus.is_empty());
        assert!(issues.is_empty());
    }

    #[test]
    fn classify_klv_recognizes_h222_metadata_au_cell_with_header_fields() {
        // Build an AU cell with non-default header field values, carrying a
        // synthetic ST 0601 LS payload. classify_klv now returns the SHAPE
        // (Sync); the per-cell header fields are recovered via
        // `iter_au_cells` (verbatim per H.222.0 §2.12.4.2 Table 2-156).
        use crate::mpegts::au_cell::{
            AuCellHeader, CellFragmentIndication, write_metadata_au_cell,
        };
        let mut inner_klv = Vec::new();
        inner_klv.extend_from_slice(&[
            0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00,
            0x00, 0x00,
        ]);
        inner_klv.push(0x04);
        inner_klv.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

        let hdr_in = AuCellHeader {
            metadata_service_id: 0x42, // non-default to detect drift
            sequence_number: 0x07,
            cell_fragment_indication: CellFragmentIndication::Complete,
            decoder_config_flag: true,
            random_access_indicator: false,
        };
        let mut wrapped = Vec::new();
        write_metadata_au_cell(&mut wrapped, hdr_in, &inner_klv).unwrap();

        assert_eq!(classify_klv(&wrapped), KlvShape::Sync);

        let cells: Vec<_> = iter_au_cells(&wrapped).collect();
        assert_eq!(cells.len(), 1);
        let (header, klv_recovered) = cells[0].as_ref().unwrap();
        assert_eq!(*klv_recovered, &inner_klv[..]);
        assert_eq!(header.metadata_service_id, 0x42);
        assert_eq!(header.sequence_number, 0x07);
        assert_eq!(
            header.cell_fragment_indication,
            CellFragmentIndication::Complete
        );
        assert!(header.decoder_config_flag);
        assert!(!header.random_access_indicator);
    }

    #[test]
    fn classify_klv_async_arm_unchanged_for_bare_ls() {
        let bare_klv = [
            0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00,
            0x00, 0x00, 0x04, 0xDE, 0xAD, 0xBE, 0xEF,
        ];
        match classify_klv(&bare_klv) {
            KlvShape::Async { klv } => assert_eq!(klv, &bare_klv[..]),
            other => panic!("expected Async, got {other:?}"),
        }
    }

    #[test]
    fn strip_dvb_sub_envelope_round_trip() {
        let segs = [0x0F, 0x10, 0xAB, 0xCD];
        let wrapped = [&[0x20u8, 0x00][..], &segs[..], &[0xFF][..]].concat();
        assert_eq!(
            strip_dvb_sub_envelope(&wrapped),
            DvbSubStripResult::Conformant(&segs[..])
        );
    }

    #[test]
    fn strip_dvb_sub_envelope_hd_range_is_non_conformant_data_id() {
        // 0x70..=0x7F is the legacy permissive HD-subtitle range per
        // EN 300 743 §7.1, but §6.2 Table 3 binds DVB-subtitle to exactly
        // 0x20. Surface as NonConformantDataId so caller (strict vs lenient)
        // decides whether to emit the sample.
        let wrapped = [0x70, 0x00, 0x0F, 0x10, 0xFF];
        assert_eq!(
            strip_dvb_sub_envelope(&wrapped),
            DvbSubStripResult::NonConformantDataId {
                observed: 0x70,
                stripped: &[0x0F, 0x10][..],
            }
        );
    }

    #[test]
    fn strip_dvb_sub_envelope_permissive_range_above_0x20_is_non_conformant() {
        // 0x21 sits in 0x20..=0x3F but isn't the §6.2 binding (which is 0x20
        // exact). Surface as NonConformantDataId.
        let wrapped = [0x21, 0x00, 0x0F, 0xAB, 0xFF];
        assert_eq!(
            strip_dvb_sub_envelope(&wrapped),
            DvbSubStripResult::NonConformantDataId {
                observed: 0x21,
                stripped: &[0x0F, 0xAB][..],
            }
        );
    }

    #[test]
    fn strip_dvb_sub_envelope_rejects_missing_marker() {
        assert_eq!(
            strip_dvb_sub_envelope(&[0x20, 0x00, 0xAB, 0xCD]),
            DvbSubStripResult::Malformed
        );
    }

    #[test]
    fn strip_dvb_sub_envelope_rejects_bad_data_identifier() {
        // 0x40 is outside both 0x20..=0x3F and 0x70..=0x7F — Malformed (not
        // even in the legacy permissive range).
        assert_eq!(
            strip_dvb_sub_envelope(&[0x40, 0x00, 0xAB, 0xFF]),
            DvbSubStripResult::Malformed
        );
    }

    #[test]
    fn strip_dvb_sub_envelope_rejects_bad_stream_id() {
        // subtitle_stream_id must be 0x00 per §6.2.
        assert_eq!(
            strip_dvb_sub_envelope(&[0x20, 0x01, 0xAB, 0xFF]),
            DvbSubStripResult::Malformed
        );
    }

    #[test]
    fn strip_dvb_sub_envelope_too_short() {
        assert_eq!(
            strip_dvb_sub_envelope(&[0x20]),
            DvbSubStripResult::Malformed
        );
        assert_eq!(strip_dvb_sub_envelope(&[]), DvbSubStripResult::Malformed);
    }

    // The AV1 arm of `parse_one_nal` is unreachable in normal demuxer flow
    // (AV1 routes to `split_obus`). Pre-Phase-0 it called `unimplemented!`,
    // which abort-panicked the host on hostile or buggy upstream. The fix:
    // `debug_assert!` + `None` — debug builds still detect regressions via
    // assert; release builds gracefully yield no NALs. The two tests below
    // pin both behaviors.
    //
    // Typed-error promotion (`parse_one_nal -> Result<Option<NalUnit>, _>`)
    // is deferred to Phase 1 where signature SemVer ratchets are in scope.

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "AV1 reached NAL splitter")]
    fn split_nals_av1_debug_asserts_no_unimplemented_panic() {
        // Pre-fix: panicked with `unimplemented!("AV1 uses OBU framing...")`.
        // Post-fix in debug: panics with the debug_assert message instead,
        // which is the regression detector.
        let buf = SharedBytes::from_vec(vec![0x00, 0x00, 0x01, 0xAA, 0xBB]);
        let _ = split_nals(&buf, VideoCodec::Av1);
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn split_nals_av1_no_panic_release() {
        // Release builds (no debug_assertions): the defense-in-depth arm
        // returns None from parse_one_nal, so split_nals yields an empty
        // Vec without panicking.
        let buf = SharedBytes::from_vec(vec![0x00, 0x00, 0x01, 0xAA, 0xBB]);
        let (nals, _issues) = split_nals(&buf, VideoCodec::Av1);
        assert!(nals.is_empty(), "AV1 must yield no NALs from NAL splitter");
    }

    // -----------------------------------------------------------------
    // B9 — NAL-header constraint enforcement tests
    // (validate-1 Phase 2 Wave B Task 9)
    // -----------------------------------------------------------------

    #[test]
    fn h264_forbidden_zero_bit_set_emits_nal_header_issue() {
        // forbidden_zero_bit=1 → header high bit set. Byte 0x80 alone:
        // forbidden=1, ref_idc=00, nal_type=0 (unspecified). The NAL is
        // still emitted (no spec-mandated discard for H.264) but the
        // issue is surfaced.
        let buf = SharedBytes::from_vec(vec![0x00, 0x00, 0x01, 0x80, 0xAA]);
        let (nals, issues) = split_nals(&buf, VideoCodec::H264);
        assert_eq!(nals.len(), 1, "H.264 forbidden-bit NAL still emitted");
        assert_eq!(issues.len(), 1, "exactly one NalHeader issue raised");
        match &issues[0] {
            NonConformantIssue::NalHeader {
                codec: VideoCodec::H264,
                kind: NalHeaderKind::ForbiddenZeroBit,
            } => {}
            other => panic!("expected H.264 ForbiddenZeroBit, got {other:?}"),
        }
    }

    #[test]
    fn h265_forbidden_zero_bit_set_emits_nal_header_issue() {
        // H.265 byte 0: forbidden(1) | nal_type(6) | top-bit-of-layer_id(1).
        // 0x80 sets forbidden=1, leaves nal_type=0, layer_id top=0.
        // Byte 1: layer_id_low(5) | temporal_id_plus1(3); use 0x01 →
        // temporal_id_plus1=1 (valid) so we isolate the forbidden-bit issue.
        let buf = SharedBytes::from_vec(vec![0x00, 0x00, 0x01, 0x80, 0x01, 0xAA]);
        let (nals, issues) = split_nals(&buf, VideoCodec::H265);
        assert_eq!(nals.len(), 1, "H.265 forbidden-bit NAL still emitted");
        assert!(
            issues.iter().any(|i| matches!(
                i,
                NonConformantIssue::NalHeader {
                    codec: VideoCodec::H265,
                    kind: NalHeaderKind::ForbiddenZeroBit
                }
            )),
            "expected H.265 ForbiddenZeroBit issue, got {issues:?}"
        );
    }

    #[test]
    fn h265_zero_temporal_id_plus1_emits_issue() {
        // H.265: temporal_id_plus1=0 (forbidden per §7.3.1.2). Header
        // bytes: byte0=0x40 (forbidden=0, nal_type=32 VPS, layer_id_top=0),
        // byte1=0x00 (layer_id_low=0, temporal_id_plus1=0).
        let buf = SharedBytes::from_vec(vec![0x00, 0x00, 0x01, 0x40, 0x00, 0xAA]);
        let (nals, issues) = split_nals(&buf, VideoCodec::H265);
        assert_eq!(nals.len(), 1);
        assert!(
            issues.iter().any(|i| matches!(
                i,
                NonConformantIssue::NalHeader {
                    codec: VideoCodec::H265,
                    kind: NalHeaderKind::ZeroTemporalIdPlus1
                }
            )),
            "expected H.265 ZeroTemporalIdPlus1 issue, got {issues:?}"
        );
    }

    #[test]
    fn h266_reserved_bit_set_drops_nal_and_emits_issue() {
        // H.266: byte0 forbidden(1) | reserved(1) | layer_id(6).
        // 0x40 → forbidden=0, reserved=1, layer_id=0. byte1=0x71 →
        // nal_type=14 (VPS), temporal_id_plus1=1. Per H.266 §7.3.1.2
        // receivers MUST discard NALs with nuh_reserved_zero_bit set;
        // the NAL is dropped from `nals` but the issue is still emitted.
        let buf = SharedBytes::from_vec(vec![0x00, 0x00, 0x01, 0x40, 0x71, 0xAA]);
        let (nals, issues) = split_nals(&buf, VideoCodec::H266);
        assert!(
            nals.is_empty(),
            "H.266 spec mandates discard for reserved-bit NALs"
        );
        assert!(
            issues.iter().any(|i| matches!(
                i,
                NonConformantIssue::NalHeader {
                    codec: VideoCodec::H266,
                    kind: NalHeaderKind::ReservedBit
                }
            )),
            "expected H.266 ReservedBit issue, got {issues:?}"
        );
    }

    #[test]
    fn h266_layer_id_out_of_range_drops_nal_and_emits_issue() {
        // H.266: layer_id=56 violates §7.4.2.2 (allowed range 0..=55).
        // byte0 forbidden(1) | reserved(1) | layer_id(6) = 0x38 →
        // forbidden=0, reserved=0, layer_id=56 (0x38 = 0b00111000;
        // low 6 bits = 0b111000 = 56). byte1=0x71 → nal_type=14, t+1=1.
        // Receivers MUST discard.
        let buf = SharedBytes::from_vec(vec![0x00, 0x00, 0x01, 0x38, 0x71, 0xAA]);
        let (nals, issues) = split_nals(&buf, VideoCodec::H266);
        assert!(
            nals.is_empty(),
            "H.266 spec mandates discard for layer_id > 55"
        );
        assert!(
            issues.iter().any(|i| matches!(
                i,
                NonConformantIssue::NalHeader {
                    codec: VideoCodec::H266,
                    kind: NalHeaderKind::LayerIdOutOfRange { id: 56 }
                }
            )),
            "expected H.266 LayerIdOutOfRange{{id=56}}, got {issues:?}"
        );
    }

    #[test]
    fn h266_forbidden_zero_bit_set_emits_issue_keeps_nal() {
        // H.266 forbidden_zero_bit set, otherwise valid (reserved=0,
        // layer_id=0, t+1=1). byte0=0x80, byte1=0x71. forbidden-bit
        // violation alone is NOT spec-mandated discard for H.266 (only
        // reserved + layer>55 are); the NAL stays in the output.
        let buf = SharedBytes::from_vec(vec![0x00, 0x00, 0x01, 0x80, 0x71, 0xAA]);
        let (nals, issues) = split_nals(&buf, VideoCodec::H266);
        assert_eq!(
            nals.len(),
            1,
            "H.266 forbidden-bit alone does not mandate discard"
        );
        assert!(
            issues.iter().any(|i| matches!(
                i,
                NonConformantIssue::NalHeader {
                    codec: VideoCodec::H266,
                    kind: NalHeaderKind::ForbiddenZeroBit
                }
            )),
            "expected H.266 ForbiddenZeroBit issue, got {issues:?}"
        );
    }

    // -----------------------------------------------------------------
    // B10 — AV1 OBU header bit validation tests
    // (validate-1 Phase 2 Wave B Task 10)
    // -----------------------------------------------------------------

    #[test]
    fn split_obus_forbidden_bit_set_emits_issue() {
        // obu_type=1 (Seq Header), ext_flag=0, has_size=1, reserved_1bit=0,
        // forbidden_bit=1. Header byte = 0x80 | (1<<3) | 0x02 = 0x8A.
        let buf = SharedBytes::from_vec(vec![0x8A, 0x00]);
        let (_obus, issues) = split_obus(&buf);
        assert!(
            issues.iter().any(|i| matches!(
                i,
                NonConformantIssue::Av1ObuHeader {
                    pid: 0,
                    kind: Av1ObuHeaderKind::ForbiddenBit
                }
            )),
            "expected ForbiddenBit issue, got {issues:?}"
        );
    }

    #[test]
    fn split_obus_reserved_bit_set_emits_issue() {
        // obu_type=1, ext_flag=0, has_size=1, reserved_1bit=1 → low bit set.
        // Header = (1<<3) | 0b011 = 0x0B.
        let buf = SharedBytes::from_vec(vec![0x0B, 0x00]);
        let (_obus, issues) = split_obus(&buf);
        assert!(
            issues.iter().any(|i| matches!(
                i,
                NonConformantIssue::Av1ObuHeader {
                    pid: 0,
                    kind: Av1ObuHeaderKind::ReservedBit
                }
            )),
            "expected ReservedBit issue, got {issues:?}"
        );
    }

    #[test]
    fn split_video_h264_returns_nal_views_and_no_issues_for_clean_au() {
        // Two H.264 NALs: SPS (nal_ref_idc=3, type=7) then a slice (type=5), 4-byte start codes.
        let au = SharedBytes::from_vec(vec![0, 0, 0, 1, 0x67, 0xAA, 0xBB, 0, 0, 0, 1, 0x65, 0xCC]);
        let (payload, issues) = split_video(
            &au,
            VideoCodec::H264,
            crate::mpegts::mux::Av1CarriageMode::Mpeg2TsBinding,
        );
        assert!(issues.is_empty());
        match payload {
            VideoPayload::Nals(nals) => assert_eq!(nals.len(), 2),
            _ => panic!("expected NALs"),
        }
    }

    #[test]
    fn split_video_strict_errs_on_malformed_nal_header() {
        // forbidden_zero_bit set (0x80) → a NAL-header issue.
        let au = SharedBytes::from_vec(vec![0, 0, 0, 1, 0x80, 0x00]);
        let res = split_video_strict(
            &au,
            VideoCodec::H264,
            crate::mpegts::mux::Av1CarriageMode::Mpeg2TsBinding,
        );
        assert!(res.is_err());
    }

    #[test]
    fn split_obus_extension_reserved_bits_set_emits_issue() {
        // obu_type=1, ext_flag=1, has_size=1 → header = (1<<3) | 0b110 = 0x0E.
        // Extension byte: temporal_id(3) | spatial_id(2) | reserved(3).
        // Set the low 3 reserved bits: ext = 0x07. Then size LEB128 = 0x00.
        let buf = SharedBytes::from_vec(vec![0x0E, 0x07, 0x00]);
        let (_obus, issues) = split_obus(&buf);
        assert!(
            issues.iter().any(|i| matches!(
                i,
                NonConformantIssue::Av1ObuHeader {
                    pid: 0,
                    kind: Av1ObuHeaderKind::ExtensionReservedBits
                }
            )),
            "expected ExtensionReservedBits issue, got {issues:?}"
        );
    }
}
