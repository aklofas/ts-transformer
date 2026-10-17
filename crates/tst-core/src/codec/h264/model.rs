//! Public types for H.264 / AVC parameter-set parsing.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::codec::{ChromaFormat, ColorInfo, Rational};

crate::binding_record! {
    /// Parsed H.264 Sequence Parameter Set.
    #[must_use]
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct H264Sps {
        /// `seq_parameter_set_id` — identifies this SPS (H.264 §7.4.2.1.1).
        pub seq_parameter_set_id: u8,
        /// Post-crop display width in luma samples.
        pub width: u32,
        /// Post-crop display height in luma samples.
        pub height: u32,
        /// `profile_idc` (66=Baseline, 77=Main, 100=High, …).
        pub profile_idc: u8,
        /// `level_idc` — e.g. 40 for Level 4.0.
        pub level_idc: u8,
        /// `constraint_set_flags` byte (bits 7-2 = flags; bits 1-0 = reserved
        /// zero).
        pub constraint_set_flags: u8,
        /// Luma bit depth (8 + `bit_depth_luma_minus8`).
        pub bit_depth_luma: u8,
        /// Chroma bit depth (8 + `bit_depth_chroma_minus8`).
        pub bit_depth_chroma: u8,
        /// Chroma subsampling format.
        pub chroma_format: ChromaFormat,
        /// True for progressive encoding (`frame_mbs_only_flag=1`).
        pub frame_mbs_only: bool,
        /// True when `fixed_frame_rate_flag=1` in the VUI.
        pub fixed_frame_rate: bool,
        /// True when the stream may contain B-frames. A heuristic: the VUI
        /// `max_num_reorder_frames` decides when present; otherwise Baseline and
        /// the constrained High subsets are B-frameless and every other profile
        /// may have B-frames.
        pub has_b_frames: bool,
        /// Frame rate as a rational number, or absent when the VUI is absent.
        pub frame_rate: Option<Rational>,
        /// VUI colour info, or absent when the VUI is absent or
        /// `video_signal_type` is not present.
        pub color: Option<ColorInfo>,
        /// Luma-sample crop offsets applied to the coded dimensions to produce
        /// `width`/`height`. Per H.264 §6.4: stored crop offsets are in chroma
        /// units; we surface them post-multiplication by `SubWidthC` /
        /// `SubHeightC * (2 - frame_mbs_only_flag)`, so `coded_width = width +
        /// crop_left + crop_right` (and similarly for height). Useful for sizing
        /// GPU buffers and for matching crops against container-level
        /// conformance-window descriptors. All four fields are zero when the SPS
        /// has no `frame_cropping_flag` set.
        pub crop_left: u32,
        /// Right crop offset in luma samples.
        pub crop_right: u32,
        /// Top crop offset in luma samples.
        pub crop_top: u32,
        /// Bottom crop offset in luma samples.
        pub crop_bottom: u32,
        /// `log2_max_frame_num_minus4` (H.264 §7.4.2.1.1). The bit width of
        /// `frame_num` in slice headers equals this + 4. Surfaced for the light
        /// slice-header parser.
        pub log2_max_frame_num_minus4: u8,
        /// The original RBSP bytes as supplied to the parser.
        pub raw_rbsp: Vec<u8>,
    }
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

crate::binding_record! {
    /// Parsed H.264 Picture Parameter Set.
    #[must_use]
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[non_exhaustive]
    pub struct H264Pps {
        /// `pic_parameter_set_id` ∈ [0, 255].
        pub pic_parameter_set_id: u8,
        /// `seq_parameter_set_id` — links this PPS to an SPS. ∈ [0, 31].
        pub seq_parameter_set_id: u8,
        /// Entropy coding mode: `CAVLC` or `CABAC`.
        pub entropy_coding_mode: EntropyCodingMode,
        /// The original RBSP bytes as supplied to the parser.
        pub raw_rbsp: Vec<u8>,
    }
}

/// H.264 entropy coding mode signalled in the PPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntropyCodingMode {
    /// Context-Adaptive Variable Length Coding (used by Baseline/Main profiles).
    Cavlc,
    /// Context-Adaptive Binary Arithmetic Coding (used by Main/High profiles).
    Cabac,
}

crate::binding_record! {
    /// All SPS and PPS NAL units parsed from a single access unit.
    #[must_use]
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    #[non_exhaustive]
    pub struct H264ParameterSets {
        /// The parsed SPS NAL units, keyed by `seq_parameter_set_id`.
        pub sps_by_id: BTreeMap<u8, H264Sps>,
        /// The parsed PPS NAL units, keyed by `pic_parameter_set_id`.
        pub pps_by_id: BTreeMap<u8, H264Pps>,
    }
}
