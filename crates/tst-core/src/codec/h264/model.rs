//! Public types for H.264 / AVC parameter-set parsing.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::codec::{ChromaFormat, ColorInfo, Rational};

/// Parsed H.264 Sequence Parameter Set.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct H264Sps {
    pub seq_parameter_set_id: u8,
    pub width: u32,
    pub height: u32,
    pub profile_idc: u8,
    pub level_idc: u8,
    pub constraint_set_flags: u8,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub chroma_format: ChromaFormat,
    pub frame_mbs_only: bool,
    pub frame_rate: Option<Rational>,
    pub fixed_frame_rate: bool,
    pub has_b_frames: bool,
    pub color: Option<ColorInfo>,
    /// Luma-sample crop offsets applied to the coded dimensions to produce
    /// `width`/`height`. Per H.264 §6.4: stored crop offsets are in chroma
    /// units; we surface them post-multiplication by `SubWidthC` /
    /// `SubHeightC * (2 - frame_mbs_only_flag)`, so
    /// `coded_width = width + crop_left + crop_right` (and similarly for
    /// height). Useful for sizing GPU buffers and for matching crops
    /// against container-level conformance-window descriptors. All four
    /// fields are zero when the SPS has no `frame_cropping_flag` set.
    pub crop_left: u32,
    pub crop_right: u32,
    pub crop_top: u32,
    pub crop_bottom: u32,
    /// `log2_max_frame_num_minus4` (H.264 §7.4.2.1.1). The bit width of
    /// `frame_num` in slice headers equals this + 4. Surfaced for
    /// `slice_header_light::parse_slice_header_light`.
    pub log2_max_frame_num_minus4: u8,
    /// The original RBSP bytes as supplied by the caller.
    pub raw_rbsp: Vec<u8>,
}

impl H264Sps {
    /// Coded picture width before `frame_crop` is applied (luma samples).
    /// Equal to `width + crop_left + crop_right`.
    /// Cannot overflow for an SPS returned by `parse_sps`, which rejects a
    /// crop whose sum reaches the raw dimension.
    pub fn coded_width(&self) -> u32 {
        self.width + self.crop_left + self.crop_right
    }

    /// Coded picture height before `frame_crop` is applied (luma samples).
    /// Equal to `height + crop_top + crop_bottom`.
    /// Cannot overflow for an SPS returned by `parse_sps`, which rejects a
    /// crop whose sum reaches the raw dimension.
    pub fn coded_height(&self) -> u32 {
        self.height + self.crop_top + self.crop_bottom
    }
}

/// Parsed H.264 Picture Parameter Set.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct H264Pps {
    pub pic_parameter_set_id: u8,
    pub seq_parameter_set_id: u8,
    pub entropy_coding_mode: EntropyCodingMode,
    /// The original RBSP bytes as supplied by the caller.
    pub raw_rbsp: Vec<u8>,
}

/// H.264 entropy coding mode signalled in the PPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntropyCodingMode {
    /// Context-Adaptive Variable Length Coding (used by Baseline/Main profiles).
    Cavlc,
    /// Context-Adaptive Binary Arithmetic Coding (used by Main/High profiles).
    Cabac,
}

/// All SPS and PPS NAL units parsed from a single access unit.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct H264ParameterSets {
    pub sps_by_id: BTreeMap<u8, H264Sps>,
    pub pps_by_id: BTreeMap<u8, H264Pps>,
}
