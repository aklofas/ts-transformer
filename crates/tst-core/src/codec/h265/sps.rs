//! SPS parser per H.265 §7.3.2.2 + §E.2.1 (VUI).

use super::{profile_tier_level, vui};
use crate::codec::bitreader::BitReader;
use crate::codec::{ChromaFormat, CodecParseError, ColorInfo, Rational, validate_bit_depth_minus8};
use alloc::string::ToString;
use alloc::vec::Vec;

/// Parsed H.265 SPS fields. Populated by [`parse_sps`].
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct H265Sps {
    pub sps_seq_parameter_set_id: u8,
    pub sps_video_parameter_set_id: u8,
    pub width: u32,
    pub height: u32,
    pub general_profile_idc: u8,
    pub general_tier_flag: bool,
    pub general_level_idc: u8,
    /// 32-bit `general_profile_compatibility_flags` per H.265 §7.3.3 — bit
    /// `i` set means the stream conforms to profile `i`. Real Main10 streams
    /// often have `general_profile_idc=1` (Main) but bit 2 set; ffmpeg
    /// `hevc/ps.c:267-270` uses this to disambiguate Main vs Main10 vs
    /// Main10-Intra. Surfaced here so consumers can do the same.
    pub general_profile_compatibility_flags: u32,
    /// `general_progressive_source_flag` (§7.4.4).
    pub general_progressive_source_flag: bool,
    /// `general_interlaced_source_flag` (§7.4.4).
    pub general_interlaced_source_flag: bool,
    /// `general_non_packed_constraint_flag` (§7.4.4).
    pub general_non_packed_constraint_flag: bool,
    /// `general_frame_only_constraint_flag` (§7.4.4).
    pub general_frame_only_constraint_flag: bool,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub chroma_format: ChromaFormat,
    pub max_sub_layers_minus1: u8,
    pub frame_rate: Option<Rational>,
    pub color: Option<ColorInfo>,
    /// Luma-sample crop offsets applied to the coded picture dimensions to
    /// produce `width`/`height`. Computed from `conformance_window_flag` +
    /// `conf_win_*_offset` per H.265 §7.4.3.2.1, already multiplied by
    /// `SubWidthC` / `SubHeightC` (the chroma-array unit conversion). So
    /// `coded_width = width + crop_left + crop_right` (and similarly for
    /// height). Useful for sizing GPU buffers and for matching crops
    /// against container-level conformance-window descriptors. All four
    /// fields are zero when the SPS has no `conformance_window_flag` set.
    pub crop_left: u32,
    pub crop_right: u32,
    pub crop_top: u32,
    pub crop_bottom: u32,
    /// `log2_max_pic_order_cnt_lsb_minus4` (H.265 §7.4.3.2.1). The bit width
    /// of `pic_order_cnt_lsb` in slice headers equals this value plus 4.
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub raw_rbsp: Vec<u8>,
}

impl H265Sps {
    /// Coded picture width before conformance-window crop is applied
    /// (luma samples). Equal to `width + crop_left + crop_right`.
    /// Cannot overflow for an SPS returned by `parse_sps`, which rejects a
    /// crop whose sum reaches the raw dimension.
    pub fn coded_width(&self) -> u32 {
        self.width + self.crop_left + self.crop_right
    }

    /// Coded picture height before conformance-window crop is applied
    /// (luma samples). Equal to `height + crop_top + crop_bottom`.
    /// Cannot overflow for an SPS returned by `parse_sps`, which rejects a
    /// crop whose sum reaches the raw dimension.
    pub fn coded_height(&self) -> u32 {
        self.height + self.crop_top + self.crop_bottom
    }
}

/// Map `chroma_format_idc` (+ `separate_colour_plane_flag` for 4:4:4) to
/// the typed [`ChromaFormat`]. Per H.265 §7.4.2.1.1, valid values are
/// 0..=3; any other value is reserved.
fn chroma_format_from(
    chroma_format_idc: u32,
    _separate_colour_plane_flag: bool,
) -> Result<ChromaFormat, CodecParseError> {
    match chroma_format_idc {
        0 => Ok(ChromaFormat::Monochrome),
        1 => Ok(ChromaFormat::Yuv420),
        2 => Ok(ChromaFormat::Yuv422),
        3 => Ok(ChromaFormat::Yuv444),
        other => Err(CodecParseError::ReservedValue {
            field: "chroma_format_idc",
            value: other,
        }),
    }
}

/// Parse an H.265 SPS RBSP per §7.3.2.2 (+ §E.2.1 VUI).
///
/// # Errors
///
/// Returns a [`CodecParseError`] when the RBSP is truncated, contains a
/// reserved/out-of-range value, or hits a parser gap. In particular, an
/// SPS with `scaling_list_enabled_flag=1` **and**
/// `sps_scaling_list_data_present_flag=1` returns
/// [`CodecParseError::EngineError`] referencing
/// `scaling_list_data` — the syntax structure at H.265 §7.3.4 is not yet
/// implemented in this parser. This is a parser limitation, **not** a
/// profile-level rejection: conformant HDR Main10 streams routinely set
/// this flag.
pub fn parse_sps(rbsp: &[u8]) -> Result<H265Sps, CodecParseError> {
    if rbsp.is_empty() {
        return Err(CodecParseError::TruncatedRbsp {
            offset_bits: 0,
            needed_bits: 8,
        });
    }
    let mut br = BitReader::new(rbsp);
    let sps_video_parameter_set_id = br.read_u(4)? as u8;
    let max_sub_layers_minus1 = br.read_u(3)? as u8;
    let _temporal_id_nesting_flag = br.read_bool()?;

    let ptl = profile_tier_level::parse(&mut br, max_sub_layers_minus1)?;

    let sps_seq_parameter_set_id = br.read_ue_max("sps_seq_parameter_set_id", 15)? as u8;
    let chroma_format_idc = br.read_ue()?;
    let separate_colour_plane_flag = if chroma_format_idc == 3 {
        br.read_bool()?
    } else {
        false
    };

    let pic_width_in_luma_samples = br.read_ue()?;
    let pic_height_in_luma_samples = br.read_ue()?;

    let conformance_window_flag = br.read_bool()?;
    let (mut crop_x_left, mut crop_x_right, mut crop_y_top, mut crop_y_bottom) = (0u32, 0, 0, 0);
    if conformance_window_flag {
        let conf_win_left_offset = br.read_ue()?;
        let conf_win_right_offset = br.read_ue()?;
        let conf_win_top_offset = br.read_ue()?;
        let conf_win_bottom_offset = br.read_ue()?;
        let (sub_w, sub_h) = match chroma_format_idc {
            1 => (2u32, 2u32),
            2 => (2, 1),
            3 => (1, 1),
            _ => (1, 1),
        };
        crop_x_left = sub_w.saturating_mul(conf_win_left_offset);
        crop_x_right = sub_w.saturating_mul(conf_win_right_offset);
        crop_y_top = sub_h.saturating_mul(conf_win_top_offset);
        crop_y_bottom = sub_h.saturating_mul(conf_win_bottom_offset);
    }

    let bit_depth_luma_minus8 = br.read_ue()?;
    let bit_depth_luma = validate_bit_depth_minus8("bit_depth_luma_minus8", bit_depth_luma_minus8)?;
    let bit_depth_chroma_minus8 = br.read_ue()?;
    let bit_depth_chroma =
        validate_bit_depth_minus8("bit_depth_chroma_minus8", bit_depth_chroma_minus8)?;

    let log2_max_pic_order_cnt_lsb_minus4 = br.read_ue()?;
    // Per H.265 §7.4.3.2.1, log2_max_pic_order_cnt_lsb_minus4 is in the
    // range 0..=12. The value is later used as a bit width via
    // `read_u(log2_max_pic_order_cnt_lsb_minus4 + 4)` (~line 184); a
    // hostile value near u32::MAX would overflow the `+ 4`. Reject
    // out-of-range values eagerly.
    if log2_max_pic_order_cnt_lsb_minus4 > 12 {
        return Err(CodecParseError::ReservedValue {
            field: "log2_max_pic_order_cnt_lsb_minus4",
            value: log2_max_pic_order_cnt_lsb_minus4,
        });
    }

    let sub_layer_ordering_info_present_flag = br.read_bool()?;
    let layers_to_read = if sub_layer_ordering_info_present_flag {
        max_sub_layers_minus1 as usize + 1
    } else {
        1
    };
    for _ in 0..layers_to_read {
        let _ = br.read_ue()?;
        let _ = br.read_ue()?;
        let _ = br.read_ue()?;
    }

    let _ = br.read_ue()?;
    let _ = br.read_ue()?;
    let _ = br.read_ue()?;
    let _ = br.read_ue()?;
    let _ = br.read_ue()?;
    let _ = br.read_ue()?;

    let scaling_list_enabled_flag = br.read_bool()?;
    if scaling_list_enabled_flag {
        let sps_scaling_list_data_present_flag = br.read_bool()?;
        if sps_scaling_list_data_present_flag {
            // The `scaling_list_data()` syntax structure (H.265 §7.3.4) is
            // a parser gap, not a profile-level rejection: conformant
            // Main10 HDR streams routinely set this flag. Surface the
            // gap honestly via `EngineError` so consumers debugging HDR
            // streams aren't misdirected toward "profile not supported".
            return Err(CodecParseError::EngineError(
                "scaling_list_data parsing not yet implemented (H.265 §7.4.2.2)".to_string(),
            ));
        }
    }

    let _amp_enabled_flag = br.read_bool()?;
    let _sample_adaptive_offset_enabled_flag = br.read_bool()?;
    let pcm_enabled_flag = br.read_bool()?;
    if pcm_enabled_flag {
        br.skip(4)?;
        br.skip(4)?;
        let _ = br.read_ue()?;
        let _ = br.read_ue()?;
        br.skip(1)?;
    }

    let num_short_term_ref_pic_sets = br.read_ue()?;
    super::short_term_rps::walk_short_term_ref_pic_sets(&mut br, num_short_term_ref_pic_sets)?;

    let long_term_ref_pics_present_flag = br.read_bool()?;
    if long_term_ref_pics_present_flag {
        let num_long_term_ref_pics_sps = br.read_ue()?;
        for _ in 0..num_long_term_ref_pics_sps {
            let _ = br.read_u(log2_max_pic_order_cnt_lsb_minus4 + 4)?;
            let _ = br.read_bool()?;
        }
    }

    let _sps_temporal_mvp_enabled_flag = br.read_bool()?;
    let _strong_intra_smoothing_enabled_flag = br.read_bool()?;

    let vui_parameters_present_flag = br.read_bool()?;
    let vui_out = if vui_parameters_present_flag {
        vui::parse(&mut br, max_sub_layers_minus1)?
    } else {
        vui::VuiOut {
            frame_rate: None,
            color: None,
        }
    };

    let chroma_format = chroma_format_from(chroma_format_idc, separate_colour_plane_flag)?;

    let raw_w = pic_width_in_luma_samples;
    let raw_h = pic_height_in_luma_samples;
    let width = crate::codec::apply_crop(
        "conf_win_left_offset + conf_win_right_offset",
        raw_w,
        crop_x_left,
        crop_x_right,
    )?;
    let height = crate::codec::apply_crop(
        "conf_win_top_offset + conf_win_bottom_offset",
        raw_h,
        crop_y_top,
        crop_y_bottom,
    )?;

    Ok(H265Sps {
        sps_seq_parameter_set_id,
        sps_video_parameter_set_id,
        width,
        height,
        general_profile_idc: ptl.general_profile_idc,
        general_tier_flag: ptl.general_tier_flag,
        general_level_idc: ptl.general_level_idc,
        general_profile_compatibility_flags: ptl.general_profile_compatibility_flags,
        general_progressive_source_flag: ptl.general_progressive_source_flag,
        general_interlaced_source_flag: ptl.general_interlaced_source_flag,
        general_non_packed_constraint_flag: ptl.general_non_packed_constraint_flag,
        general_frame_only_constraint_flag: ptl.general_frame_only_constraint_flag,
        bit_depth_luma,
        bit_depth_chroma,
        chroma_format,
        max_sub_layers_minus1,
        frame_rate: vui_out.frame_rate,
        color: vui_out.color,
        crop_left: crop_x_left,
        crop_right: crop_x_right,
        crop_top: crop_y_top,
        crop_bottom: crop_y_bottom,
        log2_max_pic_order_cnt_lsb_minus4: log2_max_pic_order_cnt_lsb_minus4 as u8,
        raw_rbsp: rbsp.to_vec(),
    })
}
