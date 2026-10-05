//! Per-stream-class state structs cached by `Muxer` at construction
//! time, plus free helper functions used during muxer setup and per-push
//! validation.
//!
//! All items here are `pub(super)` — visible to `mpegts::mux` and its
//! siblings (`push_video`, `push_klv`, `push_audio`, `push_subtitle`,
//! `emit`, `scheduling`, `stats_accounting`) but not outside
//! the parent module.

use super::psi::KLVA_REGISTRATION_DESCRIPTOR;
use super::{AudioCodec, KlvStreamType, MuxerProgramConfig, StreamSpec, SubtitleCodec, VideoCodec};
use crate::error::MuxError;
use crate::mpegts::common::{StreamType, StreamTypeCode};
use crate::mpegts::stats::StreamStats;
use alloc::collections::BTreeMap;
use alloc::string::ToString;
use alloc::vec::Vec;

/// Per-video-stream cached state. Built once at `Muxer::new` time.
pub(super) struct VideoStreamState {
    pub(super) pid: u16,
    pub(super) codec: VideoCodec,
}

/// Per-KLV-stream cached state.
pub(super) struct KlvStreamState {
    pub(super) pid: u16,
    pub(super) stream_type: KlvStreamType,
    pub(super) carries_pts: bool,
    /// For `SynchronousMetadata` streams: incrementing AU cell `sequence_number`,
    /// wraps modulo 256 per H.222.0 §2.12.4.2 Table 2-156 semantics. Unused
    /// for `PrivateData` streams.
    pub(super) au_cell_sequence_number: u8,
}

/// Per-audio-stream cached state.
pub(super) struct AudioStreamState {
    pub(super) pid: u16,
    pub(super) codec: AudioCodec,
}

/// Per-subtitle-stream cached state. `codec` is `Clone` (not `Copy`) so we
/// store it owned per-stream — same shape as `SubtitleCodec` itself.
pub(super) struct SubtitleStreamState {
    pub(super) pid: u16,
    pub(super) codec: SubtitleCodec,
}

/// Per-data-stream cached state.
pub(super) struct DataStreamState {
    pub(super) pid: u16,
    /// Raw caller-chosen PMT stream_type byte (e.g. 0xF0/0xF1, bare 0x06).
    pub(super) stream_type: u8,
    pub(super) carries_pts: bool,
}

/// Structural Annex-B access-unit validator per H.264 / H.265 / H.266
/// byte-stream syntax (ISO/IEC 14496-10 Annex B; ITU-T H.265 Annex B;
/// ITU-T H.266 Annex B).
///
/// Checking only the AU's leading prefix would accept malformed inputs
/// whose first 3-4 bytes happen to be a start code but whose interior
/// contains no real NAL units, or trails off mid-start-code. This walker
/// scans every start code in the buffer and rejects:
///
/// - inputs that do not start with `00 00 01` or `00 00 00 01`,
/// - inputs that contain only start codes with no NAL body byte between
///   them (zero-length NAL unit),
/// - inputs whose final NAL unit is empty (start code at the very tail),
/// - inputs that end mid-start-code (trailing `00 00` or `00` is
///   ambiguous in Annex-B framing).
///
/// Spec note: H.264/H.265/H.266 forbid empty NAL units — a `nal_unit()`
/// element must contain at least a 1-byte (H.264) or 2-byte (H.265/H.266)
/// `nal_unit_header()`. Validating "non-empty NAL" without parsing the
/// header is the minimum compatible check; we don't enforce the
/// codec-specific header size here (codec-specific parsers in
/// `crate::codec` do that downstream).
pub(super) fn validate_annex_b(nal: &[u8]) -> Result<(), MuxError> {
    // Must start with a recognised Annex-B start code.
    let leading_len = if nal.starts_with(&[0x00, 0x00, 0x00, 0x01]) {
        4
    } else if nal.starts_with(&[0x00, 0x00, 0x01]) {
        3
    } else {
        return Err(MuxError::InvalidNal);
    };

    // Position immediately after the current start code (i.e., start of
    // the current NAL unit's body).
    let mut nal_body_start: usize = leading_len;
    let mut i: usize = leading_len;
    while i < nal.len() {
        // Look for the next start code: `00 00 01` or `00 00 00 01`.
        // The byte preceding `00 00 01` may be a `00` (4-byte form) or
        // any other byte (3-byte form); either way the NAL body ends
        // before the leading zero pair.
        if i + 3 <= nal.len() && nal[i] == 0 && nal[i + 1] == 0 && nal[i + 2] == 1 {
            // 3-byte start code — close the current NAL.
            if i <= nal_body_start {
                // Zero-length NAL between two adjacent start codes (no
                // body bytes), e.g., `00 00 01 00 00 01`. Forbidden.
                return Err(MuxError::InvalidNal);
            }
            nal_body_start = i + 3;
            i += 3;
            continue;
        }
        if i + 4 <= nal.len()
            && nal[i] == 0
            && nal[i + 1] == 0
            && nal[i + 2] == 0
            && nal[i + 3] == 1
        {
            // 4-byte start code — close the current NAL.
            if i <= nal_body_start {
                return Err(MuxError::InvalidNal);
            }
            nal_body_start = i + 4;
            i += 4;
            continue;
        }
        i += 1;
    }
    // Reject buffers that end mid-start-code (trailing `00` or `00 00`
    // with no terminating `01` byte). The Annex-B framing makes such a
    // tail ambiguous — receivers could read it as a start-code prefix
    // and expect more data. Allow benign trailing zero-byte padding
    // only if the last closed NAL had a body (caller-supplied
    // emulation-prevention-style tails).
    //
    // We only reject if the *very last* bytes look like an unterminated
    // start code AND no NAL body has been emitted after it. This avoids
    // false positives for legitimate emulation prevention bytes inside
    // a NAL body.
    if nal.len() >= 2 && nal[nal.len() - 1] == 0 && nal[nal.len() - 2] == 0 {
        // Trailing `... 00 00` with no terminating `01` — only a problem
        // if those zeros are not part of the last NAL body. Cheap proxy:
        // when nal_body_start sits more than 2 bytes from EOF, the zeros
        // are inside a NAL body (fine). When they ARE the start of a
        // would-be new start code (i.e., at the boundary of the last NAL),
        // reject.
        if nal.len() <= nal_body_start + 2 && nal.len() > nal_body_start {
            return Err(MuxError::InvalidNal);
        }
    }

    // Close out the final NAL (from `nal_body_start` to EOF).
    if nal.len() <= nal_body_start {
        // No bytes after the last start code — final NAL is empty.
        return Err(MuxError::InvalidNal);
    }

    Ok(())
}

/// AV1-in-MPEG-2-TS binding §3.2 `ts_open_bitstream_unit()` start code.
///
/// Per binding spec the `obu_start_code` field is `uimsbf(24)` —
/// 24 bits = 3 bytes — with value `0x000001`. The on-wire byte
/// sequence is `0x00 0x00 0x01`, identical to the H.264 / H.265
/// Annex-B 3-byte start code. (Stream classification keeps AV1 and
/// H.264 distinguishable via the PMT `stream_type` + `AV01`
/// registration descriptor — the §3.2 start code only marks OBU
/// boundaries within an AV1 PES.)
pub(super) const AV1_TS_OBU_START_CODE: [u8; 3] = [0x00, 0x00, 0x01];

/// Escape-encode a SINGLE AV1 OBU unit body (no start code emitted).
///
/// This is the inner escape loop of the binding §3.2
/// `ts_open_bitstream_unit()` syntax — it implements the per-body-byte
/// emulation-prevention rule for ONE OBU. The zero-run state is local
/// to this call (each OBU body has an independent escape context per
/// the §3.2 syntax: the production `for-each-body-byte { … }` is
/// evaluated inside ONE `ts_open_bitstream_unit()` invocation, and a
/// new invocation reinitializes its own implicit zero-run).
///
/// Per binding §3.2 the on-wire body forbids the three 3-byte sequences
/// `0x000000` / `0x000001` / `0x000002`, AND requires that any 4-byte
/// sequence starting with `0x000003` have a 4th byte in
/// `{0x00, 0x01, 0x02, 0x03}` (because the receiver consumes every
/// `0x00 0x00 0x03` triple as an escape and emits the leading
/// `0x00 0x00` as OBU bytes). So whenever the body input contains
/// `0x00 0x00 X` with `X ∈ {0x00, 0x01, 0x02, 0x03}`, this helper
/// inserts a `0x03` between the second `0x00` and `X`. The decoder
/// reverses this by consuming the `0x03` after any `0x00 0x00` triple.
///
/// Caller is responsible for emitting [`AV1_TS_OBU_START_CODE`] before
/// calling this for each OBU. Appends to `out` (does not clear it).
pub(super) fn escape_obu_unit_body(unit_bytes: &[u8], out: &mut Vec<u8>) {
    let mut zero_run = 0u8;
    for &b in unit_bytes {
        if zero_run >= 2 && b <= 0x03 {
            out.push(0x03);
            zero_run = 0;
        }
        out.push(b);
        if b == 0x00 {
            zero_run = zero_run.saturating_add(1);
        } else {
            zero_run = 0;
        }
    }
}

/// Wrap a raw AV1 low-overhead OBU bytestream in per-OBU
/// `ts_open_bitstream_unit()` framing (AV1-in-MPEG-2-TS binding §3.2).
///
/// Binding §3.2 syntax: `ts_open_bitstream_unit() { obu_start_code;
/// for-each-body-byte { emulation_prevention rule } }`. The production
/// is applied **once per OBU**, NOT once per access unit. This function
/// walks the caller-supplied low-overhead OBU bytestream — each OBU is
/// `header(1) + optional_extension(1) + obu_size(LEB128) + body` per
/// AV1 spec §5.3 with `obu_has_size_field=1` (required by binding §3.1)
/// — and for every OBU emits the 3-byte [`AV1_TS_OBU_START_CODE`]
/// followed by an escape-encoded copy of the WHOLE OBU (header through
/// body). Each escape application is independent — the zero-run state
/// resets fresh at each unit boundary.
///
/// Wire layout for N input OBUs:
///   `[0x00 0x00 0x01] [escape(OBU1)] [0x00 0x00 0x01] [escape(OBU2)] …`
///
/// The wire-side `unwrap_av1_binding` mirrors this: it splits on
/// `0x00 0x00 0x01` boundaries and unescapes each unit body
/// independently, then concatenates the recovered low-overhead OBU
/// bytestream for [`split_obus`](crate::mpegts::demux::payload::split_obus).
///
/// On malformed input (truncated header / extension / LEB128, or
/// `obu_size` running past buffer end, or an OBU without
/// `obu_has_size_field=1`) the walk stops at the last good boundary and
/// returns what's been written so far — mirrors `split_obus`'s lenient
/// stance on the demux side. Does NOT panic.
///
/// Empty input → empty output (no start codes, no escape bytes).
///
/// Appends to `out` (does not clear it; the mux push path derives the
/// wrapped length from `out.len()` directly). Returns `true` iff the wrap
/// consumed the entire `obu_bytes` input cleanly (every OBU carried
/// `obu_has_size_field=1` and fit the buffer); `false` when the walk
/// bailed early — the input is not a well-formed elementary OBU stream.
pub(super) fn wrap_av1_obus_binding(obu_bytes: &[u8], out: &mut Vec<u8>) -> bool {
    let mut i = 0usize;
    while i < obu_bytes.len() {
        let obu_start = i;
        // OBU header byte (AV1 §5.3.2):
        //   obu_forbidden_bit  f(1)
        //   obu_type           f(4)
        //   obu_extension_flag f(1)
        //   obu_has_size_field f(1)
        //   obu_reserved_1bit  f(1)
        let header = obu_bytes[i];
        let extension_flag = (header >> 2) & 0x01 != 0;
        let has_size_field = (header >> 1) & 0x01 != 0;
        i += 1;
        if extension_flag {
            if i >= obu_bytes.len() {
                // Truncated extension — stop. Lenient: emit nothing for
                // this partial OBU and bail out.
                break;
            }
            i += 1;
        }
        if !has_size_field {
            // Binding §3.1 requires every OBU to carry obu_size. Without
            // it we can't find the boundary to the next OBU, so we can't
            // emit a per-OBU start code for what follows. Bail out
            // lenient-stance (same as split_obus on the demux side).
            break;
        }
        let (obu_size, consumed) =
            match crate::codec::av1::decode::leb128::read_leb128(obu_bytes, i) {
                Ok(t) => t,
                Err(_) => break, // truncated LEB128 — stop
            };
        i += consumed;
        let obu_size = match usize::try_from(obu_size) {
            Ok(n) => n,
            Err(_) => break,
        };
        let body_end = match i.checked_add(obu_size) {
            Some(end) if end <= obu_bytes.len() => end,
            _ => break,
        };
        let obu_slice = &obu_bytes[obu_start..body_end];
        i = body_end;
        // Per binding §3.2: emit start code, then escape-encode the full
        // OBU bytes (header + optional extension + LEB128 size + body).
        // Each escape application is independent — zero-run state is
        // local to `escape_obu_unit_body`.
        out.extend_from_slice(&AV1_TS_OBU_START_CODE);
        escape_obu_unit_body(obu_slice, out);
    }
    i == obu_bytes.len()
}

/// True iff `caller_descs` contains any descriptor that the receiver-side
/// subtitle classifier recognizes as a codec marker. Mirrors the demux-side
/// `mpegts::demux::pmt_classify::has_recognized_subtitle_descriptor` predicate
/// but operates on raw TLV bytes (the form held in `prog.stream_descriptors`)
/// rather than on parsed `RawDescriptor`.
///
/// Return `true` when any descriptor in `descs` carries `tag` as its first byte.
pub(super) fn has_descriptor_tag(descs: &[Vec<u8>], tag: u8) -> bool {
    descs.iter().any(|d| !d.is_empty() && d[0] == tag)
}

/// Return `true` when any descriptor in `descs` is a Registration descriptor
/// (tag 0x05, TLV ≥ 6 bytes) whose 4-byte `format_identifier` equals `fid`.
pub(super) fn has_registration_fid(descs: &[Vec<u8>], fid: &[u8; 4]) -> bool {
    descs
        .iter()
        .any(|d| d.len() >= 6 && d[0] == 0x05 && &d[2..6] == fid)
}

/// Used to suppress the subtitle auto-emit when the caller has already
/// supplied one of:
///   * `subtitling_descriptor`   (tag 0x59)
///   * `teletext_descriptor`     (tag 0x56)
///   * `VBI_teletext_descriptor` (tag 0x46)
///   * `registration_descriptor` (tag 0x05) with format_identifier
///     `"VTTC"` or `"GA94"`
///
/// Mirrors the KLV/AV1 caller-supplied-Registration suppression rule so
/// receivers don't see two competing codec markers on the same PID.
pub(super) fn caller_has_recognized_subtitle_descriptor(caller_descs: &[Vec<u8>]) -> bool {
    for tlv in caller_descs {
        if tlv.is_empty() {
            continue;
        }
        let tag = tlv[0];
        if tag == 0x59 || tag == 0x56 || tag == 0x46 {
            return true;
        }
        // registration_descriptor TLV layout: tag(1) + length(1) + body(length).
        // format_identifier is the first 4 body bytes.
        if tag == 0x05 && tlv.len() >= 6 {
            let fid = &tlv[2..6];
            if fid == b"VTTC" || fid == b"GA94" {
                return true;
            }
        }
    }
    false
}

/// Number of 188-byte TS packets needed to carry `payload_size` bytes of
/// PES (header + ES). 184 = 188 - 4 byte TS header. Adaptation field eats
/// further capacity but for sizing purposes the worst case is no AF (gives
/// the smallest packet count). The orchestrator may emit one more packet
/// than this if AF stuffing pushes a byte over; we allow a 1-packet slop
/// in the buffer reservation.
pub(super) fn ts_packets_for(payload_size: usize) -> usize {
    payload_size.div_ceil(184).max(1) + 1
}

/// Collect per-stream-class state vectors for one program. Single-pass over
/// `prog.streams`. Matches the `filter_map` collections that previously lived
/// in `Muxer::new`.
// One Vec per stream class, consumed positionally by `Muxer::new` —
// a named struct would only be destructured straight back into fields.
#[allow(clippy::type_complexity)]
pub(super) fn collect_stream_states(
    prog: &MuxerProgramConfig,
) -> (
    Vec<VideoStreamState>,
    Vec<KlvStreamState>,
    Vec<AudioStreamState>,
    Vec<SubtitleStreamState>,
    Vec<DataStreamState>,
) {
    let video: Vec<VideoStreamState> = prog
        .streams
        .iter()
        .filter_map(|s| match s {
            StreamSpec::Video { pid, codec } => Some(VideoStreamState {
                pid: *pid,
                codec: *codec,
            }),
            _ => None,
        })
        .collect();
    let klv: Vec<KlvStreamState> = prog
        .streams
        .iter()
        .filter_map(|s| match s {
            StreamSpec::Klv {
                pid,
                stream_type,
                carries_pts,
            } => Some(KlvStreamState {
                pid: *pid,
                stream_type: *stream_type,
                carries_pts: *carries_pts,
                au_cell_sequence_number: 0,
            }),
            _ => None,
        })
        .collect();
    let audio: Vec<AudioStreamState> = prog
        .streams
        .iter()
        .filter_map(|s| match s {
            StreamSpec::Audio { pid, codec, .. } => Some(AudioStreamState {
                pid: *pid,
                codec: *codec,
            }),
            _ => None,
        })
        .collect();
    let subtitle: Vec<SubtitleStreamState> = prog
        .streams
        .iter()
        .filter_map(|s| match s {
            StreamSpec::Subtitle { pid, codec } => Some(SubtitleStreamState {
                pid: *pid,
                codec: codec.clone(),
            }),
            _ => None,
        })
        .collect();
    let data: Vec<DataStreamState> = prog
        .streams
        .iter()
        .filter_map(|s| match s {
            StreamSpec::Data {
                pid,
                stream_type,
                carries_pts,
            } => Some(DataStreamState {
                pid: *pid,
                stream_type: *stream_type,
                carries_pts: *carries_pts,
            }),
            _ => None,
        })
        .collect();
    (video, klv, audio, subtitle, data)
}

/// The PCR-PID fallback when no PCR PID is pinned: first video, then first
/// audio. KLV/data/subtitle are NEVER auto-selected — KLV cadence is too
/// sparse for ETSI TR 101 290 §5.6.1's 100 ms ceiling, subtitles must not
/// carry PCR per ETSI EN 300 472 §4.0, and data has no cadence guarantee.
/// Selection (here) and validation (`MuxerConfig::validate`) share this so
/// they can never disagree.
pub(super) fn default_pcr_pid(prog: &MuxerProgramConfig) -> Option<u16> {
    prog.first_video_pid().or_else(|| prog.first_audio_pid())
}

/// Resolve the PCR-carrying PID for a program. Priority order: caller-pinned >
/// first video > first audio. KLV, data, and subtitle streams are deliberately
/// excluded from the fallback chain — see `default_pcr_pid`. `validate()`
/// rejects programs with no PCR-eligible (video / audio) stream, so the
/// `expect()` below cannot panic in well-formed configs.
pub(super) fn resolve_pcr_pid(prog: &MuxerProgramConfig) -> u16 {
    prog.pcr_pid
        .or_else(|| default_pcr_pid(prog))
        .expect("validate() guarantees ≥1 PCR-eligible stream per program")
}

/// Emit tracing warnings for caller-supplied descriptor conflicts on KLV, AV1,
/// and AC-3 streams. Kept out of `build_pmt_descriptor_cache` so that
/// function is pure (no side effects), and called once from `Muxer::new` —
/// construction-time, after `validate()`, at most ≤16 programs × ≤16 streams.
pub(super) fn warn_on_descriptor_conflicts(prog: &MuxerProgramConfig) {
    for (i, spec) in prog.streams.iter().enumerate() {
        let caller_descs = &prog.stream_descriptors[i];

        if matches!(spec, StreamSpec::Klv { .. }) {
            for tlv in caller_descs {
                if tlv.len() >= 6 && tlv[0] == 0x05 && &tlv[2..6] != b"KLVA" {
                    tracing::warn!(
                        "caller-supplied Registration descriptor on KLV PID has \
                         non-KLVA format_identifier ({:?}); receivers may not \
                         recognize the stream as KLV",
                        core::str::from_utf8(&tlv[2..6]).unwrap_or("?")
                    );
                }
            }
        }

        if let StreamSpec::Video {
            codec: VideoCodec::Av1,
            ..
        } = spec
        {
            let caller_has_av01 = has_registration_fid(caller_descs, b"AV01");
            let caller_has_other_registration = caller_descs
                .iter()
                .any(|d| d.len() >= 6 && d[0] == 0x05 && &d[2..6] != b"AV01");
            if caller_has_other_registration && !caller_has_av01 {
                tracing::warn!(
                    "caller-supplied Registration descriptor on AV1 PID has \
                     non-AV01 format_identifier; receivers may not recognize \
                     the stream as AV1"
                );
            }
        }

        if let StreamSpec::Audio {
            codec: AudioCodec::Ac3,
            ..
        } = spec
        {
            let caller_has_ac3 = has_registration_fid(caller_descs, b"AC-3");
            let caller_has_other_registration = caller_descs
                .iter()
                .any(|d| d.len() >= 6 && d[0] == 0x05 && &d[2..6] != b"AC-3");
            if caller_has_other_registration && !caller_has_ac3 {
                tracing::warn!(
                    "caller-supplied Registration descriptor on AC-3 PID has \
                     non-AC-3 format_identifier; receivers may not recognize \
                     the stream as AC-3"
                );
            }
        }
    }
}

/// Build the per-stream PMT descriptor cache for one program. Each entry is
/// the concatenated descriptor bytes for the corresponding `StreamSpec`,
/// composed of (a) any auto-emitted descriptor (KLVA / AV01 / AC-3 / ISO 639 /
/// subtitle disambiguator) followed by (b) the caller-supplied descriptors
/// from `prog.stream_descriptors[i]`. The auto-emit on (a) is suppressed when
/// the caller has already supplied an equivalent descriptor (KLVA / AV01 /
/// AC-3 suppression mirror each other; subtitle suppression matches the
/// demux-side classifier).
pub(super) fn build_pmt_descriptor_cache(prog: &MuxerProgramConfig) -> Vec<Vec<u8>> {
    let mut cache: Vec<Vec<u8>> = Vec::with_capacity(prog.streams.len());
    for (i, spec) in prog.streams.iter().enumerate() {
        let caller_descs = &prog.stream_descriptors[i];
        let caller_has_registration = has_descriptor_tag(caller_descs, 0x05);

        let mut bytes = Vec::new();
        // KLVA Registration auto-emit on KLV streams (both
        // PrivateData=0x06 and SynchronousMetadata=0x15). ffmpeg
        // mpegtsenc.c:817-818 emits KLVA on the metadata
        // stream_type path too — receivers gate KLV
        // classification on the descriptor regardless of
        // stream_type. Sync KLV with metadata_descriptor
        // (tag 0x26) doesn't *replace* KLVA — TSDuck + ffmpeg
        // consume both side-by-side.
        if matches!(
            spec,
            StreamSpec::Klv {
                stream_type: KlvStreamType::PrivateData | KlvStreamType::SynchronousMetadata,
                ..
            }
        ) && !caller_has_registration
        {
            bytes.extend_from_slice(KLVA_REGISTRATION_DESCRIPTOR);
        }
        // Synchronous KLV (stream_type 0x15 = "Metadata in PES",
        // ITU-T H.222.0 §2.12.4) additionally REQUIRES, per MISB
        // ST 1402.2 ST 1402-15/-16/-17, a metadata_descriptor (tag
        // 0x26) for each metadata service plus a single
        // metadata_std_descriptor (tag 0x27), both inside the metadata
        // ES descriptor loop. These sit alongside the KLVA registration
        // above (ffmpeg/TSDuck consume both). service_id 0 matches
        // push_klv's default AU-cell metadata_service_id; a caller using
        // a non-zero service_id supplies its own metadata_klva, which
        // suppresses the auto-emit per-tag below. Async KLV (0x06) does
        // NOT carry these — RP 217 / ST 1402.2 §9.4.2 require only KLVA.
        if matches!(
            spec,
            StreamSpec::Klv {
                stream_type: KlvStreamType::SynchronousMetadata,
                ..
            }
        ) {
            let caller_has_metadata_descriptor = has_descriptor_tag(caller_descs, 0x26);
            let caller_has_metadata_std = has_descriptor_tag(caller_descs, 0x27);
            if !caller_has_metadata_descriptor {
                bytes.extend_from_slice(&crate::mpegts::descriptors::metadata_klva(0));
            }
            if !caller_has_metadata_std {
                bytes.extend_from_slice(&crate::mpegts::descriptors::metadata_std(0, 0, 0));
            }
        }
        // AV1 auto-emit: AV01 registration_descriptor (binding §2.1).
        // MUST be the FIRST descriptor in the per-stream PMT loop —
        // receivers gate AV1 classification on stream_type 0x06 +
        // first-position AV01 Registration. Suppress when the caller
        // has already supplied an AV01 Registration (mirrors KLVA
        // suppression). A conflict warning for a non-AV01 format_identifier
        // is emitted by `warn_on_descriptor_conflicts`.
        if let StreamSpec::Video {
            codec: VideoCodec::Av1,
            ..
        } = spec
        {
            let caller_has_av01 = has_registration_fid(caller_descs, b"AV01");
            if !caller_has_av01 {
                bytes.extend_from_slice(&crate::mpegts::descriptors::format_identifier_av01());
            }
        }
        // AC-3 auto-emit: Registration descriptor with format_identifier
        // "AC-3" per ATSC A/52:2018 §A.3. Receivers use this to distinguish
        // AC-3 from other private-stream-1 (PES stream_id 0xBD) audio.
        // Suppression: caller-supplied AC-3 Registration suppresses auto-emit;
        // a non-AC-3 Registration does NOT suppress (the AC-3 Registration is
        // still auto-emitted alongside it). Conflict warning emitted by
        // `warn_on_descriptor_conflicts`.
        if let StreamSpec::Audio {
            codec: AudioCodec::Ac3,
            ..
        } = spec
        {
            let caller_has_ac3 = has_registration_fid(caller_descs, b"AC-3");
            if !caller_has_ac3 {
                bytes.extend_from_slice(&crate::mpegts::descriptors::format_identifier_ac3());
            }
            // AC-3 audio descriptor auto-emit (tag 0x81).
            //
            // ATSC A/52:2018 §A.4.3 mandates this descriptor on every
            // System A (ATSC) AC-3 PMT entry; without it strict
            // receivers (ffmpeg, GStreamer, TSDuck) must probe the
            // elementary stream to learn sample_rate / bsid / channels.
            //
            // The muxer has no syncframe at PMT-build time (the
            // descriptor cache is built once in `Muxer::new`), so we
            // emit a permissive shape per Table A4.2 + A4.3 + A4.5:
            //   sample_rate_code = 0b111 ("48 or 44.1 or 32" — any)
            //   bsid             = 8     (canonical AC-3:2018 version)
            //   bit_rate_code    = 0b110010 (MSB=1 upper-limit, 640 kbps —
            //                      the table's maximum)
            //   surround_mode    = 0b00  (not indicated)
            //   bsmod            = 0     (CM, complete main)
            //   num_channels     = 0b1001 (MSB=1 upper-limit mode: ≤2
            //                      encoded channels — typical for ISR
            //                      payloads)
            //   full_svc         = true  (complete program — no associated
            //                      service overlay)
            //
            // Suppression: when the caller supplies any descriptor with
            // tag 0x81, we honor it verbatim (caller intent wins).
            // Callers needing exact-derived fields from a parsed
            // syncframe (via codec::ac3::parse_syncframe) can build
            // their own with descriptors::ac3_audio_stream_descriptor
            // and pass it through stream_descriptors_for_audio.
            let caller_has_ac3_audio_desc = has_descriptor_tag(caller_descs, 0x81);
            if !caller_has_ac3_audio_desc {
                bytes.extend_from_slice(&crate::mpegts::descriptors::ac3_audio_stream_descriptor(
                    0b111,    // sample_rate_code: any
                    8,        // bsid: AC-3:2018 canonical
                    0b110010, // bit_rate_code: MSB=1 upper-limit, 640 kbps
                    0,        // surround_mode: not indicated
                    0,        // bsmod: CM (complete main)
                    0b1001,   // num_channels: MSB=1 upper-limit, ≤2 channels
                    true,     // full_svc: complete program
                ));
            }
        }
        // ISO 639 language descriptor auto-emit on Audio when
        // StreamSpec::Audio.language is Some. Per ISO/IEC 13818-1
        // §2.6.18-19 (tag 0x0A, length 4: 3 lang bytes + 1
        // audio_type byte). audio_type=0x00 (undefined / clean
        // main) is the spec default; richer values come from
        // caller-supplied stream_descriptors_for_audio. Suppress
        // when caller already supplied any tag-0x0A descriptor —
        // caller intent wins (their language code may differ).
        if let StreamSpec::Audio {
            language: Some(lang),
            ..
        } = spec
        {
            let caller_has_lang = has_descriptor_tag(caller_descs, 0x0A);
            if !caller_has_lang {
                bytes.extend_from_slice(&crate::mpegts::descriptors::iso_639_language(*lang, 0x00));
            }
        }
        // Subtitle auto-emit: codec-disambiguating per-stream descriptor.
        // All four SubtitleCodec variants ride PMT stream_type 0x06; the
        // descriptor here is what tells receivers which codec rides on
        // this PID. Mirrors the KLV/AV1 caller-supplied-Registration
        // suppression rule: when the caller has already supplied any
        // descriptor that the receiver-side classifier recognizes as a
        // subtitle codec marker (subtitling 0x59 / teletext 0x56 /
        // VBI teletext 0x46 / Registration with VTTC or GA94
        // format_identifier), the auto-emit is suppressed — caller's
        // takes precedence and we don't double-emit.
        if let StreamSpec::Subtitle { codec, .. } = spec {
            if !caller_has_recognized_subtitle_descriptor(caller_descs) {
                let auto = match codec {
                    SubtitleCodec::DvbSubtitling {
                        language,
                        subtitling_type,
                        composition_page_id,
                        ancillary_page_id,
                    } => crate::mpegts::descriptors::subtitling_descriptor(
                        *language,
                        *subtitling_type,
                        *composition_page_id,
                        *ancillary_page_id,
                    ),
                    SubtitleCodec::DvbTeletext {
                        language,
                        teletext_type,
                        magazine_number,
                        page_number,
                    } => crate::mpegts::descriptors::teletext_descriptor(
                        *language,
                        *teletext_type,
                        *magazine_number,
                        *page_number,
                    ),
                    SubtitleCodec::Cea708Standalone => {
                        crate::mpegts::descriptors::format_identifier_ga94()
                    }
                    SubtitleCodec::WebVttInTs => {
                        crate::mpegts::descriptors::format_identifier_vttc()
                    }
                };
                bytes.extend_from_slice(&auto);
            }
        }
        // AV1-in-MPEG-2-TS binding §2.1 — AV01 registration_descriptor
        // MUST be the FIRST descriptor in the per-stream PMT loop.
        // When the caller-supplied descriptor set itself contains an
        // AV01 Registration (and we therefore suppressed the auto-emit
        // above), reorder it to the front so receiver classification
        // gates on the first-position Registration. Mirrors the
        // §2.1 "MUST be FIRST" constraint surfaced in the auto-emit
        // path. Non-AV1 streams pass through unchanged.
        if let StreamSpec::Video {
            codec: VideoCodec::Av1,
            ..
        } = spec
        {
            let av01_idx = caller_descs
                .iter()
                .position(|tlv| tlv.len() >= 6 && tlv[0] == 0x05 && &tlv[2..6] == b"AV01");
            if let Some(idx) = av01_idx {
                bytes.extend_from_slice(&caller_descs[idx]);
                for (i, tlv) in caller_descs.iter().enumerate() {
                    if i != idx {
                        bytes.extend_from_slice(tlv);
                    }
                }
            } else {
                for tlv in caller_descs {
                    bytes.extend_from_slice(tlv);
                }
            }
        } else {
            for tlv in caller_descs {
                bytes.extend_from_slice(tlv);
            }
        }
        cache.push(bytes);
    }
    cache
}

/// PMT `stream_type` byte for a video codec.
///
/// AV1 uses the PrivateData byte (`0x06`) — the AV01 Registration descriptor
/// in the PMT stream-info loop is what receivers use to identify AV1 streams.
pub(super) fn video_stream_type_byte(codec: VideoCodec) -> u8 {
    match codec {
        VideoCodec::H264 => StreamType::H264.as_u8(),
        VideoCodec::H265 => StreamType::H265.as_u8(),
        VideoCodec::H266 => StreamType::H266.as_u8(),
        VideoCodec::Av1 => StreamType::KlvPrivate.as_u8(),
    }
}

/// PMT `stream_type` byte for a KLV stream type.
pub(super) fn klv_stream_type_byte(stream_type: KlvStreamType) -> u8 {
    match stream_type {
        KlvStreamType::PrivateData => StreamType::KlvPrivate.as_u8(),
        KlvStreamType::SynchronousMetadata => StreamType::KlvSyncMetadata.as_u8(),
    }
}

/// PMT `stream_type` byte for an audio codec.
pub(super) fn audio_stream_type_byte(codec: AudioCodec) -> u8 {
    match codec {
        AudioCodec::Mp2 => StreamType::AudioMp2.as_u8(),
        AudioCodec::Aac => StreamType::AudioAac.as_u8(),
        AudioCodec::AacLatm => StreamType::AudioAacLatm.as_u8(),
        AudioCodec::Ac3 => StreamType::AudioAc3.as_u8(),
    }
}

/// Initialize per-stream stats entries for one program. Mutates `into` rather
/// than returning a fresh BTreeMap so the caller can accumulate across
/// programs in one shared map without an extra allocation+merge pass.
pub(super) fn initialize_stats(
    prog: &MuxerProgramConfig,
    video: &[VideoStreamState],
    klv: &[KlvStreamState],
    audio: &[AudioStreamState],
    subtitle: &[SubtitleStreamState],
    data: &[DataStreamState],
    into: &mut BTreeMap<u16, StreamStats>,
) {
    for v in video {
        let stream_type_byte = video_stream_type_byte(v.codec);
        into.insert(
            v.pid,
            StreamStats {
                pid: v.pid,
                stream_type: StreamTypeCode::from_byte(stream_type_byte),
                program_number: prog.program_number,
                ..Default::default()
            },
        );
    }
    for k in klv {
        let stream_type_byte = klv_stream_type_byte(k.stream_type);
        into.insert(
            k.pid,
            StreamStats {
                pid: k.pid,
                stream_type: StreamTypeCode::from_byte(stream_type_byte),
                program_number: prog.program_number,
                ..Default::default()
            },
        );
    }
    for a in audio {
        let stream_type_byte = audio_stream_type_byte(a.codec);
        into.insert(
            a.pid,
            StreamStats {
                pid: a.pid,
                stream_type: StreamTypeCode::from_byte(stream_type_byte),
                program_number: prog.program_number,
                ..Default::default()
            },
        );
    }
    for s in subtitle {
        // All four subtitle codecs ride PMT stream_type 0x06
        // (PrivateData); the per-stream PMT descriptor
        // disambiguates between DVB-sub, teletext, CEA-708
        // standalone, and WebVTT-in-TS. The codec-derived label
        // is the one human-readable distinguisher in stats.
        into.insert(
            s.pid,
            StreamStats {
                pid: s.pid,
                stream_type: StreamTypeCode::from_byte(StreamType::KlvPrivate.as_u8()),
                program_number: prog.program_number,
                label: Some(crate::mpegts::stats::subtitle_codec_label(&s.codec).to_string()),
                ..Default::default()
            },
        );
    }
    for d in data {
        into.insert(
            d.pid,
            StreamStats {
                pid: d.pid,
                stream_type: StreamTypeCode::from_byte(d.stream_type),
                program_number: prog.program_number,
                ..Default::default()
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpegts::demux::payload::{Av1BindingUnwrap, split_obus, unwrap_av1_binding};

    /// Build a single AV1 low-overhead OBU with `obu_has_size_field=1` and
    /// no extension byte. Mirrors the helper in
    /// `tests/av1_carriage_roundtrip.rs::synthetic_av1_au::obu` — kept local
    /// here so the unit-tests don't reach into integration-test sources.
    ///
    /// AV1 spec §5.3.2 OBU header byte layout:
    ///   `obu_forbidden_bit f(1) | obu_type f(4) | obu_extension_flag f(1)
    ///    | obu_has_size_field f(1) | obu_reserved_1bit f(1)`
    /// = `(obu_type << 3) | 0b010` for `extension_flag=0`, `has_size_field=1`.
    fn make_obu(obu_type: u8, body: &[u8]) -> Vec<u8> {
        let header = (obu_type << 3) | 0x02;
        let mut v = vec![header];
        // Body lengths < 128 fit in a single-byte LEB128.
        assert!(
            body.len() < 128,
            "test helper only handles <128-byte bodies"
        );
        v.push(body.len() as u8);
        v.extend_from_slice(body);
        v
    }

    /// `wrap_av1_obus_binding` paired with the demuxer's `unwrap_av1_binding`
    /// must form a faithful round-trip: a valid low-overhead OBU bytestream
    /// wraps to a binding-conformant payload that unwraps back byte-for-byte.
    #[test]
    fn av1_binding_wrap_unwrap_round_trip_single_obu() {
        // Single Frame Header OBU with a benign body.
        let raw = make_obu(3, &[0x42, 0xAA, 0x55, 0xFF, 0xDE, 0xAD, 0xBE, 0xEF]);
        let mut wrapped = Vec::new();
        wrap_av1_obus_binding(&raw, &mut wrapped);
        // Must begin with the binding §3.2 3-byte start code 0x000001.
        assert_eq!(&wrapped[..3], &[0x00, 0x00, 0x01]);
        // Exactly one start code (single OBU in).
        assert_eq!(
            wrapped
                .windows(3)
                .filter(|w| *w == [0x00, 0x00, 0x01])
                .count(),
            1,
        );
        match unwrap_av1_binding(&wrapped) {
            Av1BindingUnwrap::Conformant(out) => assert_eq!(&out[..], &raw[..]),
            Av1BindingUnwrap::MissingFraming => panic!("conformant input misclassified"),
        }
    }

    /// `escape_obu_unit_body` is the inner escape loop — it operates on
    /// arbitrary unit bytes and inserts `0x03` after any `0x00 0x00 X` with
    /// `X ≤ 0x03`. This is the per-spec emulation-prevention rule without
    /// the start-code-prefix wrapper.
    #[test]
    fn av1_binding_escape_inserts_for_zero_zero_one() {
        // Unit byte sequence 0x00 0x00 0x01 (the same as the binding start
        // code itself) is forbidden inside the wrapped body — escape MUST
        // insert 0x03 to yield 0x00 0x00 0x03 0x01 on the wire.
        let unit: &[u8] = &[0xAA, 0x00, 0x00, 0x01, 0xBB];
        let mut out = Vec::new();
        escape_obu_unit_body(unit, &mut out);
        assert_eq!(&out[..], &[0xAA, 0x00, 0x00, 0x03, 0x01, 0xBB]);
    }

    #[test]
    fn av1_binding_escape_handles_zero_zero_zero() {
        // 0x00 0x00 0x00 (three zeros) needs ONE escape inserted after the
        // first two zeros so the third zero can't form a forbidden
        // 0x000000 3-byte sequence. Verifies the zero-run resets after
        // an emulation-prevention insertion.
        let unit: &[u8] = &[0x00, 0x00, 0x00, 0xCC];
        let mut out = Vec::new();
        escape_obu_unit_body(unit, &mut out);
        assert_eq!(&out[..], &[0x00, 0x00, 0x03, 0x00, 0xCC]);
    }

    #[test]
    fn av1_binding_unwrap_missing_start_code_reports_missing_framing() {
        // Raw OBU shape (no start code prefix) — unwrap reports
        // MissingFraming; demuxer treats this as a non-conformance signal.
        let raw: &[u8] = &[0x12, 0x00, 0xAA];
        assert_eq!(unwrap_av1_binding(raw), Av1BindingUnwrap::MissingFraming);
    }

    #[test]
    fn av1_binding_empty_input_produces_empty_output() {
        // Empty input means "no OBUs to wrap" — produce empty output (no
        // start codes, no escape bytes). Per binding §3.2 the
        // `ts_open_bitstream_unit()` production is applied once per OBU,
        // so zero OBUs in → zero start codes out.
        let mut wrapped = Vec::new();
        wrap_av1_obus_binding(&[], &mut wrapped);
        assert!(wrapped.is_empty());
    }

    /// SPEC COMPLIANCE — `obu_start_code` is `uimsbf(24)` per binding §3.2
    /// syntax table, with value `0x000001`. The on-wire byte sequence MUST
    /// be exactly `0x00 0x00 0x01` (3 bytes), not the previously-incorrect
    /// 4-byte `0x00 0x00 0x00 0x02`. This test hand-constructs a single
    /// OBU and asserts the exact wire layout matches the spec table.
    #[test]
    fn av1_binding_wrap_emits_3byte_start_code_per_spec() {
        // Single OBU whose body deliberately contains no forbidden 3-byte
        // sequences so the output is start-code-prefix + raw-OBU verbatim.
        let body: &[u8] = &[0x42, 0xAA, 0x55, 0xFF, 0xDE, 0xAD, 0xBE, 0xEF];
        let obu = make_obu(1, body); // Sequence Header
        let mut wrapped = Vec::new();
        wrap_av1_obus_binding(&obu, &mut wrapped);
        // Hand-built expected: 3-byte start code then raw OBU verbatim
        // (header + LEB128 size + body — no escapes needed).
        let mut expected: Vec<u8> = vec![0x00, 0x00, 0x01];
        expected.extend_from_slice(&obu);
        assert_eq!(&wrapped[..], &expected[..]);
        // The constant itself MUST be 3 bytes per binding §3.2 syntax.
        assert_eq!(AV1_TS_OBU_START_CODE.len(), 3);
        assert_eq!(AV1_TS_OBU_START_CODE, [0x00, 0x00, 0x01]);

        // Hand-decode the start code as uimsbf(24): big-endian read of
        // the first 3 bytes MUST equal 0x000001 per the binding spec.
        let start_code_u24: u32 =
            (u32::from(wrapped[0]) << 16) | (u32::from(wrapped[1]) << 8) | u32::from(wrapped[2]);
        assert_eq!(start_code_u24, 0x000001);
    }

    /// SPEC COMPLIANCE — binding §3.2 forbids any 4-byte sequence starting
    /// with `0x000003` unless the 4th byte is in `{0x00, 0x01, 0x02, 0x03}`.
    /// The decoder consumes EVERY `0x00 0x00 0x03` triple as an escape,
    /// so a unit body literally containing a `0x03` after `0x00 0x00`
    /// MUST be wired as `0x00 0x00 0x03 0x03`. Tested at the helper level
    /// so the assertion is purely about the escape rule, independent of
    /// OBU framing.
    #[test]
    fn av1_binding_escape_escapes_0x03_after_zero_zero() {
        let unit: &[u8] = &[0x00, 0x00, 0x03, 0x04];
        let mut out = Vec::new();
        escape_obu_unit_body(unit, &mut out);
        // Expected: 00 00 03 03 04
        //   (first 0x03 inserted as emulation_prevention_three_byte;
        //    second 0x03 is the literal unit byte; 0x04 follows verbatim).
        assert_eq!(&out[..], &[0x00, 0x00, 0x03, 0x03, 0x04]);
        // Cross-check: no forbidden 4-byte sequence in the escape output.
        for window in out.windows(4) {
            assert!(
                !(window[0] == 0x00 && window[1] == 0x00 && window[2] == 0x03 && window[3] >= 0x04),
                "escape body contains forbidden 4-byte sequence 0x00 0x00 0x03 0x{:02X}",
                window[3]
            );
        }
    }

    /// SPEC COMPLIANCE — hand-encode a 3-byte start code byte sequence and
    /// confirm the demuxer recognizes it as Conformant (not MissingFraming).
    /// Mirrors what an external AV1-binding-conformant encoder would emit.
    #[test]
    fn av1_binding_unwrap_accepts_hand_encoded_3byte_start_code() {
        // Hand-built wire payload: 3-byte start code + a simple body.
        let wire: Vec<u8> = vec![
            0x00, 0x00, 0x01, // start code
            0x10, 0x20, 0x30, 0x40, // OBU body
        ];
        match unwrap_av1_binding(&wire) {
            Av1BindingUnwrap::Conformant(out) => {
                assert_eq!(&out[..], &[0x10, 0x20, 0x30, 0x40]);
            }
            Av1BindingUnwrap::MissingFraming => {
                panic!("spec-conformant 3-byte start code misclassified as MissingFraming")
            }
        }

        // Negative: a 4-byte `0x00 0x00 0x00 0x02` (the previously-wrong
        // start code) MUST now be classified as MissingFraming since it
        // doesn't begin with `0x00 0x00 0x01`.
        let wrong_start_code: Vec<u8> = vec![0x00, 0x00, 0x00, 0x02, 0x10, 0x20];
        assert_eq!(
            unwrap_av1_binding(&wrong_start_code),
            Av1BindingUnwrap::MissingFraming,
            "previously-incorrect 4-byte 0x00000002 must now be rejected"
        );
    }

    /// SPEC COMPLIANCE — hand-encode the 0x03-escape corner case (a 4-byte
    /// `0x000003XX` sequence where `XX >= 0x04` would be forbidden on the
    /// wire) and confirm the demuxer correctly recovers the body byte 0x03.
    #[test]
    fn av1_binding_unwrap_handles_hand_encoded_0x03_escape() {
        // Hand-built wire: start code + 00 00 03 03 FF
        //   Decoder rule: at position 3 (after start code), nextbits(24) at
        //   wire offset (3 + zero-run-tracking) sees 00 00 03 as escape →
        //   emit 00 00, consume 03. Then process the second 03 as a normal
        //   byte (since the zero-run reset after emit), then 0xFF.
        // Expected OBU body recovered: 00 00 03 FF.
        let wire: Vec<u8> = vec![
            0x00, 0x00, 0x01, // 3-byte start code
            0x00, 0x00, 0x03, 0x03, 0xFF, // escape encodes literal 00 00 03 then FF
        ];
        match unwrap_av1_binding(&wire) {
            Av1BindingUnwrap::Conformant(out) => {
                assert_eq!(
                    &out[..],
                    &[0x00, 0x00, 0x03, 0xFF],
                    "0x03-after-0x00-0x00 escape must recover literal 0x03"
                );
            }
            Av1BindingUnwrap::MissingFraming => panic!("escape sequence misclassified"),
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // Per-OBU start-code framing (binding §3.2).
    //
    // Spec rule: `ts_open_bitstream_unit()` is applied ONCE PER OBU, not
    // once per access unit. The wire shape for N OBUs is N copies of
    // `[0x00 0x00 0x01] [escape(OBU_n)]`. Each escape application is
    // independent (zero-run resets at each unit boundary). Mux+demux MUST
    // round-trip a multi-OBU input emitting AND recognizing N start codes.
    //
    // These tests pair hand-built spec-byte assertions with round-trips: a
    // closed-loop round-trip alone can pass even with a wrong wire format
    // if mux+demux agree on the wrong encoding.
    // ─────────────────────────────────────────────────────────────────────

    /// MUX SPEC COMPLIANCE — wrapping two OBUs MUST emit exactly two
    /// `0x00 0x00 0x01` start codes at hand-computable positions, with each
    /// OBU's bytes appearing verbatim after its start code (no escapes
    /// needed for the chosen bodies).
    #[test]
    fn av1_binding_wrap_emits_one_start_code_per_obu() {
        // OBU1: Temporal Delimiter (obu_type=2), empty body → header(0x12) + size(0x00)
        // OBU2: Sequence Header (obu_type=1), 2-byte body [0xAA, 0xBB]
        //   → header(0x0A) + size(0x02) + body
        let obu1 = make_obu(2, &[]);
        let obu2 = make_obu(1, &[0xAA, 0xBB]);
        assert_eq!(obu1, &[0x12, 0x00]);
        assert_eq!(obu2, &[0x0A, 0x02, 0xAA, 0xBB]);

        let mut raw = Vec::new();
        raw.extend_from_slice(&obu1);
        raw.extend_from_slice(&obu2);

        let mut wrapped = Vec::new();
        wrap_av1_obus_binding(&raw, &mut wrapped);

        // Hand-built expected wire layout — none of the bodies hit a
        // forbidden 3-byte sequence so no escapes are inserted.
        let expected: Vec<u8> = vec![
            0x00, 0x00, 0x01, // start code 1
            0x12, 0x00, // OBU1: TD header + size=0
            0x00, 0x00, 0x01, // start code 2
            0x0A, 0x02, 0xAA, 0xBB, // OBU2: SH header + size=2 + body
        ];
        assert_eq!(wrapped, expected);

        // Sanity: exactly TWO start codes in the wire payload.
        let count = wrapped
            .windows(3)
            .filter(|w| *w == [0x00, 0x00, 0x01])
            .count();
        assert_eq!(count, 2, "expected exactly 2 start codes for 2 OBUs");
    }

    /// DEMUX SPEC COMPLIANCE — hand-construct a binding PES with two
    /// start-code-delimited OBUs and verify the unwrap recovers the
    /// concatenated raw OBU bytes (header + size + body) and that
    /// `split_obus` then yields two Obu records.
    #[test]
    fn av1_binding_unwrap_recovers_multi_obu_payload() {
        // OBU1: Temporal Delimiter — header(0x12) + size(0x00)
        // OBU2: Sequence Header with body [0x00, 0x00, 0xAA] — header(0x0A)
        //   + size(0x03) + [0x00, 0x00, 0xAA].
        //   Note: 0x00 0x00 0xAA in body needs NO escape (0xAA > 0x03), so
        //   this is wire-verbatim too.
        let wire: Vec<u8> = vec![
            0x00, 0x00, 0x01, // start code 1
            0x12, 0x00, // OBU1: TD
            0x00, 0x00, 0x01, // start code 2
            0x0A, 0x03, 0x00, 0x00, 0xAA, // OBU2: SH header + size=3 + body
        ];

        let recovered = match unwrap_av1_binding(&wire) {
            Av1BindingUnwrap::Conformant(v) => v,
            Av1BindingUnwrap::MissingFraming => panic!("conformant input misclassified"),
        };
        // Recovered bytes = OBU1 raw ++ OBU2 raw (no start codes, no escapes).
        assert_eq!(recovered, vec![0x12, 0x00, 0x0A, 0x03, 0x00, 0x00, 0xAA]);

        // And `split_obus` must yield two records with correct obu_types.
        let (obus, issues) = split_obus(&crate::shared::SharedBytes::from_vec(recovered));
        assert_eq!(obus.len(), 2);
        assert_eq!(obus[0].obu_type, 2); // Temporal Delimiter
        assert_eq!(obus[1].obu_type, 1); // Sequence Header
        assert_eq!(obus[1].payload.as_slice(), &[0x00, 0x00, 0xAA]);
        // No non-conformance issues on a clean multi-OBU payload.
        assert!(issues.is_empty(), "unexpected issues: {issues:?}");
    }

    /// DEMUX SPEC COMPLIANCE — when an OBU body contains a literal
    /// `0x00 0x00 0x01` sequence (escaped on the wire as
    /// `0x00 0x00 0x03 0x01`), the unwrap MUST NOT mistake the escaped
    /// 0x01 for a NEW start code. Verifies the escape-vs-start-code
    /// disambiguation rule that makes start codes uniquely detectable.
    #[test]
    fn av1_binding_unwrap_does_not_split_on_escaped_start_code() {
        // Single OBU on the wire — body bytes contain an escaped
        // 0x00 0x00 0x01 sequence (escaped as 0x00 0x00 0x03 0x01).
        let wire: Vec<u8> = vec![
            0x00, 0x00, 0x01, // start code (genuine, only one)
            0xAA, // body byte
            0x00, 0x00, 0x03, 0x01, // body: literal 0x00 0x00 0x01 (escaped)
            0xBB, // body byte
        ];
        let recovered = match unwrap_av1_binding(&wire) {
            Av1BindingUnwrap::Conformant(v) => v,
            Av1BindingUnwrap::MissingFraming => panic!("conformant input misclassified"),
        };
        // Recovered body = 0xAA 0x00 0x00 0x01 0xBB — the escaped 0x01 came
        // through as a literal body byte, NOT as a new start code.
        assert_eq!(recovered, vec![0xAA, 0x00, 0x00, 0x01, 0xBB]);
    }

    /// DEMUX SPEC COMPLIANCE — counterpart to the previous test: an
    /// UN-escaped `0x00 0x00 0x01` past the leading start code MUST be
    /// detected as a new OBU boundary. Pairs the escape-aware
    /// non-splitting case with the split-detection case.
    #[test]
    fn av1_binding_unwrap_splits_on_unescaped_later_start_code() {
        // Two OBUs on the wire, second start code reachable after a body
        // byte that's NOT preceded by an escape sequence.
        let wire: Vec<u8> = vec![
            0x00, 0x00, 0x01, // start code 1
            0xAA, 0xBB, // OBU1 body bytes (no zero-run)
            0x00, 0x00, 0x01, // start code 2 — UN-escaped, a real boundary
            0xCC, 0xDD, // OBU2 body bytes
        ];
        let recovered = match unwrap_av1_binding(&wire) {
            Av1BindingUnwrap::Conformant(v) => v,
            Av1BindingUnwrap::MissingFraming => panic!("conformant input misclassified"),
        };
        // Recovered bytes = OBU1_body ++ OBU2_body (start codes stripped).
        assert_eq!(recovered, vec![0xAA, 0xBB, 0xCC, 0xDD]);
    }
}
