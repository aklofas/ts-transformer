//! PES (Packetized Elementary Stream) header packing.
//!
//! Five call shapes on the muxer hot path:
//! - Video: stream_id 0xE0, PTS only, PES_packet_length = 0 (unbounded).
//!   AV1 sets data_alignment_indicator=1 per binding §3.4; other video codecs
//!   leave it 0 (H.222.0 §2.4.3.7 codec-defined).
//! - KLV synchronous (stream_type 0x15): stream_id 0xFC, PTS only, bounded,
//!   data_alignment=1 per H.222.0 V9 §2.12.4.1. 0xFC is reserved for
//!   *metadata streams* (Table 2-22) and must only be used with stream_type 0x15.
//! - KLV asynchronous (stream_type 0x06): stream_id 0xBD (private_stream_1),
//!   no PTS, bounded. Uses 0xBD per ffmpeg + GStreamer convention; H.222.0
//!   Table 2-22 reserves 0xFC for metadata streams (stream_type 0x15) only.
//! - Audio: stream_id 0xC0..=0xCF (base + within-program index) for MP2/AAC/
//!   LATM, 0xBD (private_stream_1) for AC-3 with data_alignment=1 per ATSC
//!   A/52:2018 §A.6.3.
//! - Subtitle: stream_id 0xBD, PTS only, bounded, data_alignment=1.
//!   DVB-sub adds a 3-byte EN 300 743 §6.2 envelope; DVB-teletext uses a
//!   45-byte stuffed PES per EN 300 472 §4.2.
//!
//! PES header layout (per ISO/IEC 13818-1 §2.4.3.6):
//!   start_code(3): 0x00 0x00 0x01
//!   stream_id(1)
//!   PES_packet_length(2): 0 = unbounded (video only)
//!   flags1(1): '10' marker | scrambling=0 | priority=0 | data_alignment | copyright=0 | original=0
//!   flags2(1): PTS_DTS_flags(2 high) | other flags (all 0 for our case)
//!   PES_header_data_length(1)
//!   [PTS(5)] [DTS(5)]

use crate::mpegts::common::Pts90khz;
use crate::mpegts::mux::AudioCodec;
use alloc::vec::Vec;

pub(crate) const STREAM_ID_VIDEO: u8 = 0xE0;
/// PES `stream_id` for synchronous KLV metadata streams (stream_type 0x15).
///
/// Per H.222.0 V9 Table 2-22, `0xFC` is reserved for *metadata streams*
/// (stream_type 0x15 `SynchronousMetadata`). Do NOT use this for async KLV
/// (stream_type 0x06 `PrivateData`) — those streams use
/// [`STREAM_ID_PRIVATE_STREAM_1`] (`0xBD`) per ffmpeg + GStreamer convention.
pub(crate) const STREAM_ID_KLV: u8 = 0xFC;
/// Base PES `stream_id` for MP2 / AAC / LATM audio elementary streams.
///
/// These codecs use `STREAM_ID_AUDIO_BASE + within_program_index`,
/// consuming the `0xC0..=0xCF` slice of ISO/IEC 13818-1 Table 2-22's audio
/// stream_id space (16 audio streams per program; H.222.0 allows up to 32
/// at `0xC0..=0xDF` but `MAX_AUDIO_STREAMS_PER_PROGRAM` caps at 16).
///
/// AC-3 is the exception: per ATSC A/52:2018 §A.4.2, AC-3 PES on PMT
/// stream_type 0x81 MUST use `stream_id = 0xBD` (private_stream_1).
pub(crate) const STREAM_ID_AUDIO_BASE: u8 = 0xC0;
/// PES `stream_id` for `private_stream_1` (ISO/IEC 13818-1 Table 2-22, 0xBD).
///
/// Used by AC-3 audio (ATSC A/52:2018 §A.4.2 mandate), DVB subtitling (EN 300 743
/// §6.2), DVB teletext (EN 300 472 §4.2), CEA-708 standalone, and WebVTT-in-TS.
/// MPEG-TS demuxer dispatch is by `elementary_PID`, not `stream_id`.
pub(crate) const STREAM_ID_PRIVATE_STREAM_1: u8 = 0xBD;

/// PES PTS/DTS field selector. Embeds the PTS so callers can't construct an
/// inconsistent state (e.g. PtsOnly with no value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PesPtsField {
    /// No PTS, no DTS — async metadata.
    None,
    /// PTS present, no DTS — video without B-frame reorder, sync KLV.
    PtsOnly(Pts90khz),
    /// Both PTS and DTS present — reordered video (H.264/H.265/H.266/AV1
    /// streams with B-frames) per ISO/IEC 13818-1 §2.4.3.6. Emits
    /// `PTS_DTS_flags = '11'` and writes 10 bytes (5-byte PTS with
    /// marker_high `0b0011` + 5-byte DTS with marker_high `0b0001`).
    PtsAndDts { pts: Pts90khz, dts: Pts90khz },
}

/// PES header flag bits the writer can set up-front, passed into the
/// writer rather than OR-ed in at the call site.
///
/// Only `data_alignment_indicator` is exposed today — that's the only flag any
/// of our codec paths set. `PES_priority`, `copyright`, `original_or_copy`,
/// `PES_scrambling_control` are all zero in our output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PesFlags {
    /// Bit 2 of flags1 (byte 6 of the PES header). Set when the PES carries
    /// one logical access unit (a complete subtitle composition page, KLV
    /// access unit, AAC ADTS frame, etc.). Required for AC-3 (ATSC A/52
    /// §A.6.3), DVB-sub (EN 300 743 §6.2), DVB-teletext (EN 300 472 §4.2),
    /// metadata streams (H.222.0 V9 §2.12.4.1), AV1 (binding §3.4).
    pub data_alignment_indicator: bool,
}

/// Maximum size of a PES header for the cases this muxer emits.
/// = 3(start) + 1(stream_id) + 2(length) + 1(flags1) + 1(flags2) + 1(header_data_length) + 5(PTS) + 5(DTS) = 19
///
/// The 19-byte ceiling covers the `PesPtsField::PtsAndDts` variant used by
/// reordered video (B-frames). Non-DTS callers (audio, KLV, subtitle,
/// non-reordered video) only use the first 14 bytes; the extra 5-byte
/// reservation is a no-cost array bound — `write_pes_header` returns the
/// actual bytes-written count.
pub(crate) const MAX_PES_HEADER_SIZE: usize = 19;

/// Write a complete audio PES packet (header + caller's frame bytes) into `out`.
///
/// PES `stream_id` dispatched by codec:
/// * `Ac3` — `0xBD` (private_stream_1) per ATSC A/52:2018 §A.4.2 (PDF p.116,
///   normative "shall"); `data_alignment_indicator` set to 1 per §A.6.3.
/// * `Mp2` / `Aac` / `AacLatm` — `STREAM_ID_AUDIO_BASE + within_program_index`
///   (range `0xC0..=0xCF`) per ISO/IEC 13818-1 Table 2-22.
///
/// PES_packet_length is bounded (audio frames are bounded, unlike video's 0 =
/// unbounded sentinel). Callers must ensure `frames.len()` fits in u16 after
/// accounting for the 3-byte PES header overhead — this is enforced by the
/// per-stream payload cap checked in `push_audio_to`.
pub(crate) fn write_audio_pes(
    out: &mut Vec<u8>,
    codec: AudioCodec,
    within_program_index: u8,
    pts: PesPtsField,
    frames: &[u8],
) {
    debug_assert!(within_program_index < 16, "audio cap is 16 per program");
    let stream_id = match codec {
        AudioCodec::Ac3 => STREAM_ID_PRIVATE_STREAM_1, // 0xBD = private_stream_1
        AudioCodec::Mp2 | AudioCodec::Aac | AudioCodec::AacLatm => {
            STREAM_ID_AUDIO_BASE + within_program_index
        }
    };
    let mut header = [0u8; MAX_PES_HEADER_SIZE];
    // AC-3 PES requires data_alignment_indicator=1 per ATSC A/52:2018 §A.6.3.
    // Other audio codecs (MP2 / AAC ADTS / AAC LATM) leave the bit clear.
    let flags = PesFlags {
        data_alignment_indicator: matches!(codec, AudioCodec::Ac3),
    };
    let header_len = write_pes_header(
        &mut header,
        stream_id,
        pts,
        Some(frames.len() as u16),
        flags,
    );
    out.extend_from_slice(&header[..header_len]);
    out.extend_from_slice(frames);
}

/// Codec-dispatching subtitle PES writer.
///
/// Routes to the correct PES_data_field envelope per codec:
/// - `DvbSub` — wraps payload in `0x20 + 0x00 + payload + 0xFF` per
///   ETSI EN 300 743 §6.2 (`wrap_dvb_sub_pes_data_field`).
/// - `DvbTeletext` — emits a 45-byte stuffed PES header
///   (`PES_header_data_length=0x24`) and pads the PES with `0xFF` to reach
///   `N × 184` bytes total per ETSI EN 300 472 §4.2.
/// - `Passthrough` — `Cea708Standalone` / `WebVttInTs` — no codec-specific
///   envelope; payload passes through verbatim (informal industry conventions).
///
/// Builds a `private_stream_1` (0xBD) PES with a PTS-only header and the
/// `data_alignment_indicator` flag set — every PES carries one logical
/// subtitle unit (DVB-sub composition page, teletext data field, CEA-708
/// service block, or WebVTT cue), never a fragment.
///
/// `pts_90khz` is the PES PTS in 90 kHz ticks; values outside the 33-bit
/// range are masked at the wire level by `write_pts`. Empty payloads are
/// accepted (symmetric with audio / KLV / video).
///
/// Caller must ensure the on-wire size fits in u16 (PES_packet_length must
/// cover flags + PTS field + envelope + payload). The caller-side check
/// lives in `Muxer::push_subtitle_to`, which surfaces it as
/// `MuxError::SubtitleTooLarge` against the codec-specific envelope budget.
pub(crate) fn write_subtitle_pes(
    out: &mut Vec<u8>,
    pts_90khz: i64,
    codec: SubtitlePesShape,
    payload: &[u8],
) {
    match codec {
        SubtitlePesShape::DvbSub => {
            let wrapped = wrap_dvb_sub_pes_data_field(payload);
            write_subtitle_pes_passthrough(out, pts_90khz, &wrapped);
        }
        SubtitlePesShape::DvbTeletext => {
            write_dvb_teletext_pes(out, pts_90khz, payload);
        }
        SubtitlePesShape::Passthrough => {
            write_subtitle_pes_passthrough(out, pts_90khz, payload);
        }
    }
}

/// Compute the on-wire DVB teletext PES packet size in bytes, or `None`
/// if it would overflow `PES_packet_length`'s 16-bit field.
///
/// Per ETSI EN 300 472 §4.2, a DVB teletext PES is composed of:
/// * a fixed 45-byte PES header (`HEADER_TOTAL`),
/// * an optional auto-prepended `data_identifier` byte (`0x10`) when the
///   caller's first payload byte is not already in the `0x10..=0x1F`
///   range mandated by EN 300 472 §4.4.1,
/// * the caller's payload, and
/// * tail stuffing padding the PES to exactly `N * 184` bytes (one TS
///   payload area per packet, no adaptation field).
///
/// The wire `PES_packet_length` field (ISO/IEC 13818-1 §2.4.3.7) is a
/// 16-bit big-endian unsigned and equals `total_pes_bytes - 6`. This
/// helper returns `Some(total_pes_bytes)` when that subtraction fits in
/// `u16`, and `None` otherwise. Callers (`push_subtitle_to`) use this to
/// pre-validate against `MuxError::SubtitleTooLarge` so the writer can
/// safely `u16::try_from(...).expect(...)` the cast.
///
/// `auto_prepend` mirrors the writer's own auto-prepend decision: pass
/// `true` when the caller's first payload byte is NOT in `0x10..=0x1F`
/// (forcing the writer to insert a `0x10`), `false` otherwise. The
/// boundary differs by 1 byte across this flag because the prepended
/// byte counts toward `useful`.
pub(super) fn dvb_teletext_total_pes_bytes(
    payload_len: usize,
    auto_prepend: bool,
) -> Option<usize> {
    const HEADER_TOTAL: usize = 45;
    const TS_PAYLOAD_PER_PKT: usize = 184;
    let auto_id_byte = usize::from(auto_prepend);
    let useful = HEADER_TOTAL + auto_id_byte + payload_len;
    let n = useful.div_ceil(TS_PAYLOAD_PER_PKT).max(1);
    let total_pes_bytes = n * TS_PAYLOAD_PER_PKT;
    // PES_packet_length = total_pes_bytes - 6 must fit in u16.
    if total_pes_bytes < 6 || total_pes_bytes - 6 > u16::MAX as usize {
        None
    } else {
        Some(total_pes_bytes)
    }
}

/// Caller-side helper: does `write_dvb_teletext_pes` auto-prepend a
/// `data_identifier` byte for this payload? EN 300 472 §4.4.1 mandates
/// the first payload byte be in `0x10..=0x1F`; the writer inserts `0x10`
/// when the caller has not. Exposed at module scope so `push_subtitle_to`
/// can compute the same flag the writer will use when sizing the PES.
pub(super) fn dvb_teletext_will_auto_prepend(payload: &[u8]) -> bool {
    !matches!(payload.first(), Some(0x10..=0x1F))
}

/// Write a complete DVB teletext PES packet per ETSI EN 300 472 §4.2 + §4.4.
///
/// - 45-byte PES header total: `start_code(3) + stream_id(1) + length(2) +
///   flags1(1) + flags2(1) + PES_header_data_length(1) + PTS(5) +
///   stuffing(31×0xFF)`.
/// - `PES_header_data_length = 0x24` (36 — covers 5 PTS bytes + 31 stuffing
///   bytes).
/// - PES payload begins with a `data_identifier` byte ∈ `0x10..=0x1F`
///   (EN 300 472 §4.4.1). If `payload[0]` is not in that range, `0x10`
///   (EBU teletext) is auto-prepended to mirror gst-tsmux
///   (`gstbasetsmuxttxt.c:100-103`). Without this byte ffmpeg's
///   `ff_data_identifier_is_teletext()` probe rejects the stream.
/// - Tail stuffing past the caller's data_units is itself a sequence of
///   `data_unit_id=0xFF` stuffing_data_units per EN 300 472 §4.4 — each
///   is 46 bytes: `[0xFF, 0x2C (=44), 0x00 × 44]`. ffmpeg's `dvbtxt.c:40-44`
///   probe rejects raw-0xFF stuffing.
/// - `PES_packet_length = (N × 184) − 6`, where
///   `N = ceil((45 + auto_id_byte + payload.len()) / 184)`. The PES
///   packet is exactly `N × 184` bytes, with stuffing data_units padding
///   the tail. Callers must pre-validate via
///   [`dvb_teletext_total_pes_bytes`] so `pes_packet_length` fits in u16
///   per EN 300 472 §4.2 + ISO/IEC 13818-1 §2.4.3.7.
/// - `data_alignment_indicator = 1` (every PES carries one logical teletext
///   data unit).
fn write_dvb_teletext_pes(out: &mut Vec<u8>, pts_90khz: i64, payload: &[u8]) {
    /// Total PES header size for EBU teletext (EN 300 472 §4.2).
    const HEADER_TOTAL: usize = 45;
    /// `PES_header_data_length` for EBU teletext: 36 = 5 PTS + 31 stuffing.
    const PES_HEADER_DATA_LENGTH: u8 = 0x24;
    /// Stuffing byte for the 31-byte PES-header stuffing region.
    const PES_HEADER_STUFFING: u8 = 0xFF;
    /// Default `data_identifier` per EN 300 472 §4.4.1 Table 1 (EBU teletext).
    const DEFAULT_DATA_IDENTIFIER: u8 = 0x10;
    /// Stuffing data_unit per EN 300 472 §4.4: 46 bytes total.
    const STUFFING_DATA_UNIT: [u8; 46] = {
        let mut arr = [0x00u8; 46];
        arr[0] = 0xFF; // data_unit_id (stuffing per Table 2)
        arr[1] = 0x2C; // data_unit_length = 44
        arr
    };

    let auto_prepend = dvb_teletext_will_auto_prepend(payload);

    // The caller (push_subtitle_to) must have pre-validated payload size
    // against `dvb_teletext_total_pes_bytes` and rejected oversized inputs
    // with `MuxError::SubtitleTooLarge`. The expect() here is a hard
    // invariant — see EN 300 472 §4.2 + ISO/IEC 13818-1 §2.4.3.7.
    let total_pes_bytes = dvb_teletext_total_pes_bytes(payload.len(), auto_prepend)
        .expect("caller pre-validates DVB teletext PES length fits in u16");
    // PES_packet_length excludes the 6 fixed bytes (start_code + stream_id +
    // length itself) per ISO/IEC 13818-1 §2.4.3.7.
    let pes_packet_length = u16::try_from(total_pes_bytes - 6)
        .expect("caller pre-validates DVB teletext PES length fits in u16");

    // Fixed prefix: start_code(3) + stream_id(1).
    out.extend_from_slice(&[0x00, 0x00, 0x01]);
    out.push(STREAM_ID_PRIVATE_STREAM_1);
    // PES_packet_length (BE u16).
    out.extend_from_slice(&pes_packet_length.to_be_bytes());
    // flags1: '10' marker | data_alignment_indicator (bit 2). Hardcoded
    // inline (vs routed through write_pes_header) because the teletext
    // path doesn't share the standard 14-byte header — it has 36 bytes
    // of stuffing after the PTS to reach a 45-byte header total.
    out.push(0b1000_0100);
    // flags2: PTS_DTS_flags = '10' (PTS only) in bits 7..6.
    out.push(0b1000_0000);
    // PES_header_data_length.
    out.push(PES_HEADER_DATA_LENGTH);
    // PTS (5 bytes) per ISO/IEC 13818-1 §2.4.3.6.
    let mut pts_buf = [0u8; 5];
    write_pts(&mut pts_buf, Pts90khz::new(pts_90khz), 0b0010);
    out.extend_from_slice(&pts_buf);
    // Stuffing to reach the 45-byte header total. After PTS we are at
    // byte 14 (3 + 1 + 2 + 1 + 1 + 1 + 5).
    debug_assert_eq!(out.len(), 14);
    out.resize(HEADER_TOTAL, PES_HEADER_STUFFING);
    debug_assert_eq!(out.len(), HEADER_TOTAL);
    // Auto-prepended data_identifier (EN 300 472 §4.4.1).
    if auto_prepend {
        out.push(DEFAULT_DATA_IDENTIFIER);
    }
    // Caller's data_units (or first byte = caller's data_identifier
    // followed by data_units).
    out.extend_from_slice(payload);
    // Tail stuffing per EN 300 472 §4.4 — emit whole stuffing_data_units
    // until we reach `total_pes_bytes`. The PES byte total is always a
    // multiple of 184 by construction; each stuffing unit is 46 bytes.
    // 184 / 46 = 4 exactly, so whole units always fit. If a partial unit
    // is needed at the end (auto_id_byte+payload misaligns), emit
    // [0xFF, length=remaining-2, 0x00 × (remaining-2)] as a single
    // shorter stuffing_data_unit (EN 300 472 §4.4 permits any length
    // 0..=44 in the data_unit_length field).
    while out.len() + STUFFING_DATA_UNIT.len() <= total_pes_bytes {
        out.extend_from_slice(&STUFFING_DATA_UNIT);
    }
    let remaining = total_pes_bytes - out.len();
    if remaining > 0 {
        // Partial stuffing data_unit: at minimum 2 bytes (id + length),
        // up to 46.
        if remaining >= 2 {
            out.push(0xFF); // data_unit_id
            out.push((remaining - 2) as u8); // data_unit_length
            out.resize(out.len() + (remaining - 2), 0x00);
        } else {
            // remaining == 1 — pad with a single 0xFF byte. Belt-and-
            // suspenders; never fires in practice since total_pes_bytes
            // is always N × 184.
            out.push(0xFF);
        }
    }
    debug_assert_eq!(out.len(), total_pes_bytes);
}

/// Wrap caller-supplied DVB subtitling segment bytes in the EN 300 743
/// §6.2 PES_data_field envelope:
/// `data_identifier(0x20) + subtitle_stream_id(0x00) + segments +
/// end_of_PES_data_field_marker(0xFF)`.
pub(crate) fn wrap_dvb_sub_pes_data_field(segments: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + segments.len() + 1);
    out.push(0x20); // data_identifier (DVB subtitling)
    out.push(0x00); // subtitle_stream_id
    out.extend_from_slice(segments);
    out.push(0xFF); // end_of_PES_data_field_marker
    out
}

/// Internal helper that writes the actual PES packet (header + payload) for
/// codecs that don't add a codec-specific envelope.
fn write_subtitle_pes_passthrough(out: &mut Vec<u8>, pts_90khz: i64, payload: &[u8]) {
    let pts = PesPtsField::PtsOnly(Pts90khz::new(pts_90khz));
    let mut header = [0u8; MAX_PES_HEADER_SIZE];
    // Each subtitle PES advertises that it contains a complete logical unit
    // (DVB-sub composition page, CEA-708 service block, or WebVTT cue) via
    // data_alignment_indicator=1.
    let header_len = write_pes_header(
        &mut header,
        STREAM_ID_PRIVATE_STREAM_1,
        pts,
        Some(payload.len() as u16),
        PesFlags {
            data_alignment_indicator: true,
        },
    );
    out.extend_from_slice(&header[..header_len]);
    out.extend_from_slice(payload);
}

/// Per-codec PES envelope shape selector. Internal to the muxer.
pub(crate) enum SubtitlePesShape {
    DvbSub,
    DvbTeletext,
    Passthrough,
}

/// Write a PES header to `out`. Returns bytes written.
///
/// `payload_length`:
/// - `Some(n)` — bounded; emits PES_packet_length covering flags1, flags2,
///   header_data_length, PES header data, and ES payload (i.e., everything
///   after PES_packet_length itself).
/// - `None` — unbounded (PES_packet_length = 0). Used for video PES.
pub(crate) fn write_pes_header(
    out: &mut [u8],
    stream_id: u8,
    pts_field: PesPtsField,
    payload_length: Option<u16>,
    flags: PesFlags,
) -> usize {
    debug_assert!(out.len() >= MAX_PES_HEADER_SIZE);

    // flags2 high two bits = PTS_DTS_flags per ISO/IEC 13818-1 §2.4.3.6:
    //   '00' — neither PTS nor DTS present
    //   '10' — PTS only (5 bytes)
    //   '11' — PTS + DTS (10 bytes) — used by reordered video (B-frames)
    //   '01' — forbidden
    let (pts_dts_flags, pts_size) = match pts_field {
        PesPtsField::None => (0b00u8, 0u8),
        PesPtsField::PtsOnly(_) => (0b10u8, 5u8),
        PesPtsField::PtsAndDts { .. } => (0b11u8, 10u8),
    };

    // PES_header_data_length = bytes after this field that are PES-header (PTS/DTS/etc.)
    // Equal to pts_size in our cases.
    let header_data_length = pts_size;

    // PES_packet_length: covers flags1, flags2, header_data_length, PES header data,
    // and ES payload — i.e., everything after PES_packet_length itself.
    let pes_packet_length = match payload_length {
        None => 0u16, // unbounded (video)
        Some(n) => 3u16 + header_data_length as u16 + n,
    };

    // start_code prefix
    out[0] = 0x00;
    out[1] = 0x00;
    out[2] = 0x01;
    // stream_id
    out[3] = stream_id;
    // PES_packet_length
    out[4] = (pes_packet_length >> 8) as u8;
    out[5] = (pes_packet_length & 0xFF) as u8;
    // flags1: '10' marker (bits 7..6)
    //         | 00 PES_scrambling_control
    //         | 0  PES_priority
    //         | <data_alignment_indicator>
    //         | 0  copyright
    //         | 0  original_or_copy
    let mut flags1: u8 = 0x80;
    if flags.data_alignment_indicator {
        flags1 |= 0b0000_0100;
    }
    out[6] = flags1;
    // flags2: PTS_DTS_flags << 6 | rest 0
    out[7] = pts_dts_flags << 6;
    // PES_header_data_length
    out[8] = header_data_length;

    let mut idx = 9;
    match pts_field {
        PesPtsField::None => {}
        PesPtsField::PtsOnly(pts) => {
            // marker_high '0010' per ISO/IEC 13818-1 §2.4.3.6 for PTS-only.
            write_pts(&mut out[idx..idx + 5], pts, /*marker_high=*/ 0b0010);
            idx += 5;
        }
        PesPtsField::PtsAndDts { pts, dts } => {
            // marker_high '0011' for the PTS half of a PTS+DTS pair,
            // '0001' for the DTS half, per ISO/IEC 13818-1 §2.4.3.6.
            write_pts(&mut out[idx..idx + 5], pts, /*marker_high=*/ 0b0011);
            idx += 5;
            write_pts(&mut out[idx..idx + 5], dts, /*marker_high=*/ 0b0001);
            idx += 5;
        }
    }
    idx
}

/// Encode a 33-bit PTS into 5 bytes per ISO/IEC 13818-1 §2.4.3.6.
///
/// Layout (bits, MSB first):
///   marker_high(4) | PTS[32..30](3) | marker(1)
///   PTS[29..22](8)
///   PTS[21..15](7) | marker(1)
///   PTS[14..7](8)
///   PTS[6..0](7) | marker(1)
///
/// `marker_high` is 0b0010 for PTS-only ("0010"), 0b0011 for PTS+DTS PTS,
/// 0b0001 for the DTS half. We only emit PTS-only here.
fn write_pts(out: &mut [u8], pts: Pts90khz, marker_high: u8) {
    debug_assert!(out.len() >= 5);
    let pts_val: u64 = pts.masked_33bit();

    out[0] = (marker_high << 4) | ((((pts_val >> 30) & 0x07) << 1) as u8) | 0x01;
    out[1] = ((pts_val >> 22) & 0xFF) as u8;
    out[2] = ((((pts_val >> 15) & 0x7F) << 1) as u8) | 0x01;
    out[3] = ((pts_val >> 7) & 0xFF) as u8;
    out[4] = (((pts_val & 0x7F) << 1) as u8) | 0x01;
}

/// Decode the 33-bit PTS from a 5-byte field. Used by tests for round-trip
/// verification.
#[cfg(test)]
fn read_pts(buf: &[u8]) -> u64 {
    debug_assert!(buf.len() >= 5);
    let b0 = buf[0] as u64;
    let b1 = buf[1] as u64;
    let b2 = buf[2] as u64;
    let b3 = buf[3] as u64;
    let b4 = buf[4] as u64;
    (((b0 >> 1) & 0x07) << 30)
        | (b1 << 22)
        | (((b2 >> 1) & 0x7F) << 15)
        | (b3 << 7)
        | ((b4 >> 1) & 0x7F)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_pes_header_unbounded() {
        let mut buf = [0u8; MAX_PES_HEADER_SIZE];
        let n = write_pes_header(
            &mut buf,
            STREAM_ID_VIDEO,
            PesPtsField::PtsOnly(Pts90khz::new(90_000)),
            None,
            PesFlags::default(),
        );
        assert_eq!(n, 14);
        assert_eq!(&buf[..3], &[0x00, 0x00, 0x01]);
        assert_eq!(buf[3], STREAM_ID_VIDEO);
        // PES_packet_length = 0 (unbounded)
        assert_eq!(buf[4], 0);
        assert_eq!(buf[5], 0);
        // flags1 marker
        assert_eq!(buf[6], 0x80);
        // PTS_DTS_flags = 10 (PTS only)
        assert_eq!(buf[7], 0x80);
        assert_eq!(buf[8], 5);
        // PTS round-trips
        assert_eq!(read_pts(&buf[9..14]), 90_000);
    }

    #[test]
    fn video_pes_header_pts_and_dts() {
        // Reordered video (B-frames) carries both PTS and DTS per
        // ISO/IEC 13818-1 §2.4.3.6 with PTS_DTS_flags = '11'.
        let mut buf = [0u8; MAX_PES_HEADER_SIZE];
        let n = write_pes_header(
            &mut buf,
            STREAM_ID_VIDEO,
            PesPtsField::PtsAndDts {
                pts: Pts90khz::new(200),
                dts: Pts90khz::new(100),
            },
            None,
            PesFlags::default(),
        );
        assert_eq!(n, 19);
        assert_eq!(&buf[..3], &[0x00, 0x00, 0x01]);
        assert_eq!(buf[3], STREAM_ID_VIDEO);
        // PES_packet_length = 0 (unbounded).
        assert_eq!(buf[4], 0);
        assert_eq!(buf[5], 0);
        // flags1 marker '10', no data_alignment.
        assert_eq!(buf[6], 0x80);
        // PTS_DTS_flags = '11' in flags2 bits 7..6.
        assert_eq!((buf[7] >> 6) & 0b11, 0b11);
        // PES_header_data_length = 10 (5 PTS + 5 DTS).
        assert_eq!(buf[8], 10);
        // PTS marker_high '0011' in byte 9 high nibble.
        assert_eq!(buf[9] >> 4, 0b0011);
        // DTS marker_high '0001' in byte 14 high nibble.
        assert_eq!(buf[14] >> 4, 0b0001);
        // Marker bits (low bit of bytes 9/11/13 for PTS, 14/16/18 for DTS).
        for b in [buf[9], buf[11], buf[13], buf[14], buf[16], buf[18]] {
            assert_eq!(b & 0x01, 0x01, "marker bit must be 1");
        }
        // PTS and DTS round-trip.
        assert_eq!(read_pts(&buf[9..14]), 200);
        assert_eq!(read_pts(&buf[14..19]), 100);
    }

    #[test]
    fn klv_async_pes_header_no_pts() {
        let mut buf = [0u8; MAX_PES_HEADER_SIZE];
        let n = write_pes_header(
            &mut buf,
            STREAM_ID_KLV,
            PesPtsField::None,
            Some(20),
            PesFlags::default(),
        );
        assert_eq!(n, 9);
        assert_eq!(buf[3], STREAM_ID_KLV);
        // PES_packet_length = 3 + 0 + 20 = 23
        assert_eq!(buf[4], 0);
        assert_eq!(buf[5], 23);
        // PTS_DTS_flags = 00
        assert_eq!(buf[7], 0x00);
        // PES_header_data_length = 0
        assert_eq!(buf[8], 0);
    }

    #[test]
    fn klv_sync_pes_header_with_pts() {
        let mut buf = [0u8; MAX_PES_HEADER_SIZE];
        let n = write_pes_header(
            &mut buf,
            STREAM_ID_KLV,
            PesPtsField::PtsOnly(Pts90khz::new(45_000)),
            Some(100),
            PesFlags::default(),
        );
        assert_eq!(n, 14);
        // PES_packet_length = 3 + 5 + 100 = 108
        assert_eq!(buf[4], 0);
        assert_eq!(buf[5], 108);
        assert_eq!(buf[7], 0x80); // PTS only
        assert_eq!(buf[8], 5); // PES_header_data_length = 5
        assert_eq!(read_pts(&buf[9..14]), 45_000);
    }

    #[test]
    fn pts_marker_bits_set() {
        let mut buf = [0u8; 5];
        write_pts(&mut buf, Pts90khz::new(0), 0b0010);
        // Marker bits (low bit of bytes 0/2/4) must be 1.
        assert_eq!(buf[0] & 0x01, 0x01);
        assert_eq!(buf[2] & 0x01, 0x01);
        assert_eq!(buf[4] & 0x01, 0x01);
        // Marker high nibble = 0010 (PTS-only)
        assert_eq!(buf[0] >> 4, 0b0010);
    }

    #[test]
    fn pts_round_trip_max_value() {
        let mut buf = [0u8; 5];
        let max_33bit = (1u64 << 33) - 1;
        write_pts(&mut buf, Pts90khz::new(max_33bit as i64), 0b0010);
        assert_eq!(read_pts(&buf), max_33bit);
    }

    #[test]
    fn pts_round_trip_sweep() {
        let values: [u64; 6] = [0, 1, 90_000, 1 << 16, 1 << 30, (1u64 << 33) - 1];
        let mut buf = [0u8; 5];
        for v in values {
            write_pts(&mut buf, Pts90khz::new(v as i64), 0b0010);
            assert_eq!(read_pts(&buf), v, "value {}", v);
        }
    }

    /// Per ATSC A/52:2018 §A.4.2 (PDF p.116, normative "shall"): for AC-3
    /// (PMT stream_type 0x81), "the value of stream_id in the PES header
    /// shall be 0xBD (indicating private_stream_1)." §A.6.3 mandates
    /// data_alignment_indicator = 1.
    #[test]
    fn ac3_pes_uses_stream_id_0xbd_with_alignment() {
        use crate::mpegts::mux::AudioCodec;

        let mut out = Vec::new();
        let frames = vec![0xAAu8; 10];
        let pts = PesPtsField::PtsOnly(Pts90khz::new(45_000));
        write_audio_pes(&mut out, AudioCodec::Ac3, 0, pts, &frames);

        assert_eq!(&out[0..3], &[0x00, 0x00, 0x01], "PES start code");
        assert_eq!(
            out[3], 0xBD,
            "AC-3 PES stream_id must be 0xBD (private_stream_1) per ATSC A/52:2018 §A.4.2",
        );
        // data_alignment_indicator is bit 2 of flags1 (byte 6).
        assert_eq!(
            (out[6] >> 2) & 0b1,
            1,
            "AC-3 PES data_alignment_indicator must be set per ATSC A/52:2018 §A.6.3",
        );
    }

    #[test]
    fn non_ac3_audio_pes_keeps_stream_id_in_audio_range() {
        use crate::mpegts::mux::AudioCodec;

        let mut out = Vec::new();
        let frames = vec![0xAAu8; 10];
        let pts = PesPtsField::PtsOnly(Pts90khz::new(0));
        write_audio_pes(&mut out, AudioCodec::Mp2, 0, pts, &frames);

        // MP2 / AAC / LATM should still use 0xC0..0xCF (= AUDIO_BASE + within_idx).
        assert_eq!(out[3], 0xC0, "MP2 PES stream_id is 0xC0 (audio range)");
        // No data_alignment_indicator for non-AC-3 audio.
        assert_eq!((out[6] >> 2) & 0b1, 0, "MP2 PES has no alignment bit");
    }

    #[test]
    fn write_subtitle_pes_pts_only_header_data_alignment_set() {
        // Passthrough shape — exercises the non-DVB-sub codec branches
        // (CEA-708 / WebVTT). No codec-specific envelope; payload passes
        // through verbatim into the PES.
        let mut out = Vec::new();
        write_subtitle_pes(
            &mut out,
            0x12345,
            SubtitlePesShape::Passthrough,
            &[0xAA, 0xBB, 0xCC],
        );
        // packet_start_code_prefix
        assert_eq!(&out[0..3], &[0x00, 0x00, 0x01]);
        // stream_id is private_stream_1
        assert_eq!(out[3], STREAM_ID_PRIVATE_STREAM_1);
        assert_eq!(STREAM_ID_PRIVATE_STREAM_1, 0xBD);
        // PES_packet_length covers flags1(1) + flags2(1) + header_data_length(1)
        // + PES header data (5 PTS bytes) + ES payload (3) = 11.
        assert_eq!(u16::from_be_bytes([out[4], out[5]]), 11);
        // flags1 byte: bit 7-6 = '10' marker, bit 2 = data_alignment_indicator.
        assert_eq!(out[6] & 0b1100_0000, 0b1000_0000);
        assert_eq!((out[6] >> 2) & 0b1, 0b1, "data_alignment_indicator set");
        // PTS_DTS_flags = 0b10 (PTS only) in flags2 byte high two bits.
        assert_eq!((out[7] >> 6) & 0b11, 0b10);
        // Trailing 3 bytes are the payload.
        assert_eq!(&out[out.len() - 3..], &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn write_subtitle_pes_empty_payload_accepted() {
        let mut out = Vec::new();
        write_subtitle_pes(&mut out, 0x100, SubtitlePesShape::Passthrough, &[]);
        // 6 fixed bytes (start prefix + stream_id + length) +
        // 3 mandatory PES header bytes + 5 PTS bytes = 14.
        assert_eq!(out.len(), 14);
        // PES_packet_length = 3 + 5 + 0 = 8.
        assert_eq!(u16::from_be_bytes([out[4], out[5]]), 8);
        // data_alignment_indicator still set on empty payloads.
        assert_eq!((out[6] >> 2) & 0b1, 0b1);
    }

    #[test]
    fn write_subtitle_pes_dvb_sub_wraps_payload_in_envelope() {
        // EN 300 743 §6.2 envelope: data_identifier=0x20 + subtitle_stream_id=0x00
        // + raw segment bytes + end_of_PES_data_field_marker=0xFF.
        let mut out = Vec::new();
        let segment = [0x0F, 0x10, 0x00, 0x01, 0x00, 0x02, 0x00, 0x10];
        write_subtitle_pes(&mut out, 0x100, SubtitlePesShape::DvbSub, &segment);
        // Skip 14-byte PES header (3 start + 1 stream_id + 2 length + 3 flags
        // + 5 PTS) — the next byte should be the envelope's data_identifier.
        assert_eq!(out[14], 0x20, "data_identifier");
        assert_eq!(out[15], 0x00, "subtitle_stream_id");
        assert_eq!(&out[16..16 + segment.len()], &segment[..]);
        assert_eq!(out[16 + segment.len()], 0xFF, "marker");
        // Total ES payload = 2 + 8 + 1 = 11 bytes; PES_packet_length covers
        // flags(3) + PTS(5) + 11 = 19.
        assert_eq!(u16::from_be_bytes([out[4], out[5]]), 19);
    }

    #[test]
    fn write_pes_header_accepts_alignment_flag_directly() {
        let mut buf = [0u8; MAX_PES_HEADER_SIZE];
        let flags = PesFlags {
            data_alignment_indicator: true,
        };
        let n = write_pes_header(
            &mut buf,
            STREAM_ID_VIDEO,
            PesPtsField::PtsOnly(Pts90khz::new(0)),
            None,
            flags,
        );
        assert_eq!(n, 14);
        // bit 2 of flags1 (byte 6) is data_alignment_indicator
        assert_eq!((buf[6] >> 2) & 0b1, 0b1);
        // marker is still '10' in bits 7..6
        assert_eq!(buf[6] & 0b1100_0000, 0b1000_0000);
    }

    #[test]
    fn wrap_dvb_sub_pes_data_field_round_trip() {
        // Empty segment list still emits the 3-byte envelope.
        let wrapped_empty = wrap_dvb_sub_pes_data_field(&[]);
        assert_eq!(wrapped_empty, vec![0x20, 0x00, 0xFF]);

        // Nontrivial segments are concatenated verbatim between prefix
        // and marker; library does not interpret them.
        let segs = [0x0F, 0x10, 0x00, 0x01, 0x00, 0x02, 0xAA, 0xBB];
        let wrapped = wrap_dvb_sub_pes_data_field(&segs);
        assert_eq!(wrapped[0], 0x20);
        assert_eq!(wrapped[1], 0x00);
        assert_eq!(&wrapped[2..2 + segs.len()], &segs[..]);
        assert_eq!(wrapped[2 + segs.len()], 0xFF);
        assert_eq!(wrapped.len(), 2 + segs.len() + 1);
    }

    #[test]
    fn dvb_teletext_pes_auto_prepends_data_identifier() {
        // Caller passes a raw teletext data_unit body (no leading 0x10).
        // The writer must prepend 0x10 (gst-tsmux behavior) so ffmpeg's
        // ff_data_identifier_is_teletext probe accepts the stream.
        let mut out = Vec::new();
        let body = [0x02u8, 0x2C, 0xAA, 0xBB]; // arbitrary 4-byte payload
        write_dvb_teletext_pes(&mut out, /*pts_90khz=*/ 0, &body);

        // After 45-byte header, byte 45 must be 0x10 (auto-prepended ID).
        assert_eq!(
            out[45], 0x10,
            "auto-prepended data_identifier=0x10 expected, got {:#04x}",
            out[45]
        );
        // The caller's bytes follow at byte 46.
        assert_eq!(
            &out[46..50],
            &body,
            "caller body intact after the prepended ID"
        );
    }

    #[test]
    fn dvb_teletext_pes_passes_through_caller_data_identifier() {
        // Caller's first byte is already in the EN 300 472 0x10..=0x1F
        // range — pass through unchanged, no double-prepend.
        let mut out = Vec::new();
        let body = [0x12u8, 0x2C, 0xAA]; // first byte 0x12 = subtitle teletext per EN 300 472 Table 1
        write_dvb_teletext_pes(&mut out, 0, &body);
        assert_eq!(out[45], 0x12, "caller's data_identifier preserved");
        assert_eq!(out[46], 0x2C);
        assert_eq!(out[47], 0xAA);
    }

    #[test]
    fn dvb_teletext_pes_tail_stuffs_with_spec_data_units() {
        // Per EN 300 472 §4.4, tail stuffing must be sequences of stuffing
        // data_units: [0xFF, 0x2C, 0x00 × 44] = 46 bytes each. Raw 0xFF
        // bytes (today's behavior) are rejected by ffmpeg's dvbtxt.c:40-44
        // probe.
        let mut out = Vec::new();
        let body = [0x02u8, 0x2C, 0xAA]; // tiny payload — most of N×184 is stuffing
        write_dvb_teletext_pes(&mut out, 0, &body);

        // Total = 184 (single TS packet's worth of PES payload area).
        assert_eq!(out.len(), 184);
        // Layout:
        //   bytes 0..45    : PES header (45 bytes)
        //   byte 45        : auto-prepended 0x10
        //   bytes 46..49   : caller body (3 bytes)
        //   byte 49        : start of tail stuffing
        //
        // First stuffing data_unit must be [0xFF, 0x2C, 0x00 × 44].
        assert_eq!(out[49], 0xFF, "stuffing data_unit_id=0xFF");
        assert_eq!(out[50], 0x2C, "stuffing data_unit_length=44");
        assert!(
            out[51..51 + 44].iter().all(|b| *b == 0x00),
            "stuffing payload is 44 zero bytes, got {:?}",
            &out[51..51 + 44]
        );
    }
}
