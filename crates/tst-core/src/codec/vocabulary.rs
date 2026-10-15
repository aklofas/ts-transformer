//! The codec vocabulary every layer shares: which codec a stream carries
//! and the units an access unit splits into.
//!
//! Defined here, below MPEG-TS, so code that never touches a transport
//! stream (an RTP packetizer, a parameter-set parser) names the same types
//! the muxer and the demuxer do. `mpegts::mux` and `mpegts::demux`
//! re-export them under their old paths.
//!
//! # NAL header bytes
//!
//! Two conventions meet here. [`NalUnit`]'s `payload` holds the RBSP with
//! the NAL header stripped (1 byte for H.264, 2 for H.265/H.266) — the
//! header's fields are already parsed into the variant — which is also
//! what the `parse_sps`/`parse_pps` family takes. Every byte-slice API that
//! deals in whole NAL units — `codec::annexb::nals`,
//! [`crate::codec::nal_framing::extract_parameter_sets`],
//! `codec::nal::nal_unit_type` and the RTP packetizers built on them —
//! includes the header and excludes the Annex-B start code.

use crate::shared::SharedBytes;

/// Video codec of a video elementary stream.
///
/// Drives the PMT `stream_type` byte on the mux side and is reported for
/// each video PID on the demux side: 0x1B for H.264 / AVC, 0x24 for
/// H.265 / HEVC, 0x33 for H.266 / VVC. AV1 sits on `stream_type 0x06` with
/// an `AV01` `registration_descriptor`. Mid-stream codec change is out of
/// scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    H264,
    H265,
    /// H.266 / VVC (ITU-T H.266 V4). PMT stream_type 0x33.
    H266,
    /// AV1 (AOM Bitstream Spec). PMT stream_type 0x06 with an
    /// auto-emitted AV01 `registration_descriptor`.
    Av1,
}

/// Audio codec carried by an audio elementary stream.
///
/// Drives the PMT `stream_type` byte:
/// - `Mp2` → 0x03 (ISO/IEC 11172-3 Audio — covers MPEG-1 Layer I/II/III)
/// - `Aac` → 0x0F (ISO/IEC 13818-7 ADTS Audio)
/// - `AacLatm` → 0x11 (ISO/IEC 14496-3 LATM Audio)
/// - `Ac3` → 0x81 (ATSC AC-3)
///
/// On the demux side it identifies the codec of a `SamplePayload::Audio`
/// for typed dispatch; the frames stay raw PES payload bytes. E-AC-3,
/// DVB-shaped AC-3 (`stream_type 0x06` + AC-3 registration) and MP3 on
/// user-private stream_types are not classified automatically; route them
/// with `DemuxerConfig::treat_as`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    Mp2,
    Aac,
    AacLatm,
    Ac3,
}

/// One AV1 Open Bitstream Unit. Per AV1 Bitstream Spec §5.3.2.
///
/// The header byte (`obu_forbidden_bit | obu_type | obu_extension_flag |
/// obu_has_size_field | obu_reserved_1bit`) and any extension byte are
/// parsed during split; the LEB128 `obu_size` field is consumed and
/// stripped. `payload` carries only the OBU body bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Obu {
    /// 4-bit `obu_type` (AV1 §5.3.2). 1=SequenceHeader, 2=TemporalDelimiter,
    /// 3=FrameHeader, 4=TileGroup, 5=Metadata, 6=Frame,
    /// 7=RedundantFrameHeader, 8=TileList, 15=Padding.
    pub obu_type: u8,
    /// Optional extension header bytes when `obu_extension_flag = 1`.
    pub extension: Option<ObuExtension>,
    /// OBU payload bytes — header + extension byte + LEB128 size field
    /// stripped. Pass-through; parsed by `codec::av1::parse_*` if the
    /// consumer wants typed fields.
    pub payload: SharedBytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObuExtension {
    /// 3-bit `temporal_id` (AV1 §5.3.3).
    pub temporal_id: u8,
    /// 2-bit `spatial_id` (AV1 §5.3.3).
    pub spatial_id: u8,
}

/// One H.264 or H.265 NAL unit. Codec-tagged so wrapped languages
/// (Kotlin, Swift, Java) get idiomatic `when`/`switch` exhaustiveness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NalUnit {
    H264 {
        /// 5-bit `nal_unit_type` (H.264 §7.3.1).
        nal_type: u8,
        /// 2-bit `nal_ref_idc` (H.264 §7.3.1).
        ref_idc: u8,
        /// RBSP bytes; Annex-B start codes stripped, emulation prevention
        /// bytes preserved (consumer's decoder removes them).
        payload: SharedBytes,
    },
    H265 {
        /// 6-bit `nal_unit_type` (H.265 §7.3.1.2).
        nal_type: u8,
        /// 6-bit `nuh_layer_id` (H.265 §7.3.1.2).
        layer_id: u8,
        /// 3-bit `nuh_temporal_id_plus1` (H.265 §7.3.1.2).
        temporal_id_plus1: u8,
        /// RBSP bytes; same stripping/preservation rules as `H264`.
        payload: SharedBytes,
    },
    /// One H.266 / VVC NAL unit. Header parsed per H.266 V4 §7.3.1.2.
    ///
    /// Common `nal_type` values: VPS_NUT=14, SPS_NUT=15, PPS_NUT=16,
    /// PREFIX_APS_NUT=17, SUFFIX_APS_NUT=18, PH_NUT=19, AUD_NUT=20,
    /// IDR_W_RADL=7, IDR_N_LP=8, CRA_NUT=9, GDR_NUT=10. Full table at
    /// H.266 V4 Table 5.
    H266 {
        /// 5-bit `nal_unit_type` (H.266 V4 §7.3.1.2).
        nal_type: u8,
        /// 6-bit `nuh_layer_id` (H.266 V4 §7.3.1.2).
        layer_id: u8,
        /// 3-bit `nuh_temporal_id_plus1` (H.266 V4 §7.3.1.2).
        temporal_id_plus1: u8,
        /// RBSP bytes; Annex-B start codes stripped, emulation
        /// prevention preserved (consumer's decoder removes 0x03 escapes).
        payload: SharedBytes,
    },
}
