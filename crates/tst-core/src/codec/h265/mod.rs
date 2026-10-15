//! H.265 / HEVC parameter-set parsers.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! See [`crate::codec`] for umbrella architecture and design rationale.
//!
//! ## Spec coverage
//!
//! Parsed per ITU-T H.265 V11 (07/2024) and H.273 V4 for VUI color:
//! - VPS: video_parameter_set_id, general_level_idc, general_tier_flag,
//!   profile_compatibility_flags (32 bits), max_sublayers,
//!   temporal_id_nesting.
//! - SPS: seq_parameter_set_id, video_parameter_set_id link,
//!   chroma_format + bit_depths, dimensions + conformance_window crop,
//!   profile_tier_level surfacing, VUI color (H.273 lookup),
//!   frame_rate (if VUI signalled).
//! - PPS: pic_parameter_set_id + seq_parameter_set_id linkage.
//! - Collector [`parse_parameter_sets`]: walks `Vec<NalUnit>`, groups
//!   by id, partial-success-tolerant.
//!
//! ## Not parsed (deferred)
//!
//! - `scaling_list_data` (H.265 §7.3.4) — parser bails with
//!   `EngineError("scaling_list_data parsing not yet implemented ...")`
//!   when `sps_scaling_list_data_present_flag = 1`. This is a parser
//!   gap, not a profile-level rejection.
//! - `short_term_ref_pic_sets` past num_short_term_ref_pic_sets > 0 —
//!   handled by `short_term_rps` walker but consumed fields aren't
//!   surfaced.
//! - SEI messages.
//! - Full slice headers — only the light subset is parsed; see
//!   [`parse_slice_header_light`].
//!
//! H.265 parsing is hand-rolled (the `hevc-parser` crate's struct fields
//! are crate-private, and `h265-parser` does not exist); reference
//! sections are noted on each module.

mod pps;
mod profile_tier_level;
mod short_term_rps;
mod sps;
mod slice_header_light;
mod vps;
mod vui;

#[cfg(test)]
mod tests;

pub use pps::{H265Pps, parse_pps};
pub use profile_tier_level::H265ProfileTierLevel;
pub use slice_header_light::{H265SliceHeaderLight, H265SliceType, parse_slice_header_light};
pub use sps::{H265Sps, parse_sps};
pub use vps::{H265Vps, parse_vps};

use crate::codec::CodecParseError;
use crate::codec::NalUnit;
use alloc::collections::BTreeMap;

/// All VPS, SPS, and PPS NAL units parsed from a slice.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct H265ParameterSets {
    pub vps_by_id: BTreeMap<u8, H265Vps>,
    pub sps_by_id: BTreeMap<u8, H265Sps>,
    pub pps_by_id: BTreeMap<u8, H265Pps>,
}

/// Parse all VPS/SPS/PPS NAL units from a slice. See [`crate::codec`]
/// crate root for the partial-success-tolerant behavior. Returns Ok with
/// empty maps when no parameter set NALs are present.
pub fn parse_parameter_sets(nals: &[NalUnit]) -> Result<H265ParameterSets, CodecParseError> {
    let mut out = H265ParameterSets::default();
    let mut had_param_set = false;
    let mut all_failed = true;

    for nal in nals {
        let NalUnit::H265 {
            nal_type, payload, ..
        } = nal
        else {
            continue;
        };
        match *nal_type {
            32 => {
                had_param_set = true;
                match parse_vps(payload) {
                    Ok(vps) => {
                        out.vps_by_id.insert(vps.vps_video_parameter_set_id, vps);
                        all_failed = false;
                    }
                    Err(e) => tracing::warn!(target: "tst_core::codec::h265",
                        error = ?e, "skipping malformed VPS"),
                }
            }
            33 => {
                had_param_set = true;
                match parse_sps(payload) {
                    Ok(sps) => {
                        out.sps_by_id.insert(sps.sps_seq_parameter_set_id, sps);
                        all_failed = false;
                    }
                    Err(e) => tracing::warn!(target: "tst_core::codec::h265",
                        error = ?e, "skipping malformed SPS"),
                }
            }
            34 => {
                had_param_set = true;
                match parse_pps(payload) {
                    Ok(pps) => {
                        out.pps_by_id.insert(pps.pps_pic_parameter_set_id, pps);
                        all_failed = false;
                    }
                    Err(e) => tracing::warn!(target: "tst_core::codec::h265",
                        error = ?e, "skipping malformed PPS"),
                }
            }
            _ => {}
        }
    }

    if had_param_set && all_failed {
        return Err(CodecParseError::EngineError(
            "every parameter set NAL in the input failed to parse".into(),
        ));
    }
    Ok(out)
}
