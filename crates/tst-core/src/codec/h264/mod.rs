//! H.264 / AVC parameter-set parsers.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! See [`crate::codec`] for umbrella architecture and design rationale.
//!
//! ## Spec coverage
//!
//! Parsed per ITU-T H.264 (08/2021):
//! - SPS: profile/level/constraints, dimensions + frame_crop (§6.4),
//!   chroma_format + bit_depth (§7.4.2.1.1), VUI color signalling
//!   via H.273, frame_mbs_only, has_b_frames, frame_rate (numerator
//!   inferred from time_scale, denominator from 2*num_units_in_tick).
//! - PPS: entropy_coding_mode + seq_parameter_set_id linkage.
//! - Collector [`parse_parameter_sets`]: walks `Vec<NalUnit>`, groups
//!   by id, partial-success-tolerant.
//!
//! ## Not parsed (deferred)
//!
//! - VUI HRD parameters (CBR/VBR signalling).
//! - SEI messages (user data, picture timing, mastering display).
//! - Full slice headers ([`parse_slice_header_light`] reads the light
//!   subset: `first_mb_in_slice`, `slice_type`, `pic_parameter_set_id`,
//!   `frame_num`).
//! - SVC / MVC extension SPSes (`subset_seq_parameter_set_rbsp`).

mod decode;
mod model;
mod slice_header_light;

#[cfg(test)]
mod tests;

pub use decode::{parse_parameter_sets, parse_pps, parse_sps};
pub use model::{EntropyCodingMode, H264ParameterSets, H264Pps, H264Sps};
pub use slice_header_light::{H264SliceHeaderLight, H264SliceType, parse_slice_header_light};
