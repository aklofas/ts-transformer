//! H.266 / VVC parameter-set parsers.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! Hand-rolled per ITU-T H.266 V4 (2026-01) — the bitstream spec
//! (sections 7.3 syntax tables and 7.4 semantics). Mirrors the
//! structure of [`crate::codec::h265`]: per-set parsers
//! ([`parse_vps`] / [`parse_sps`] / [`parse_pps`]) plus a
//! collector ([`parse_parameter_sets`]) that walks a `Vec<NalUnit>`
//! partial-success-tolerantly.
//!
//! ## Spec coverage
//!
//! Parsed per ITU-T H.266 V4 (2026-01) and H.274 for VUI color:
//! - VPS: `vps_id`, `max_layers`, `max_sub_layers`.
//! - SPS: `sps_id`, `vps_id`, dimensions + conformance-window crop,
//!   chroma format + bit depths, PTL (profile/tier/level),
//!   VUI color (H.274 lookup), frame rate (if timing_hrd signalled).
//! - PPS: `pps_id` + `sps_id` linkage.
//! - Collector [`parse_parameter_sets`]: walks `Vec<NalUnit>`, groups
//!   by id, partial-success-tolerant.
//!
//! ## Not parsed (deferred)
//!
//! - APS NALs (types 17/18), Picture Header NALs (type 19).
//! - Multi-layer streams (`nuh_layer_id != 0`).
//! - `general_constraint_info` parsing.
//! - `scaling_list_data_present_flag = 1` — parser bails with
//!   `UnsupportedProfile`.
//! - SEI messages.
//! - Full slice headers — only the light subset is parsed; see
//!   [`parse_slice_header_light`].
//!
//! See `docs/project/deferred-features.md` for rationale and revisit triggers.

mod pps;
mod profile_tier_level;
mod slice_header_light;
mod sps;
mod vps;
mod vui;

#[cfg(test)]
mod tests;

pub use pps::{H266Pps, parse_pps};
pub use profile_tier_level::H266ProfileTierLevel;
pub use slice_header_light::{H266SliceHeaderLight, H266SliceType, parse_slice_header_light};
pub use sps::{H266Sps, parse_sps};
pub use vps::{H266Vps, parse_vps};

use crate::codec::CodecParseError;
use crate::codec::NalUnit;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// Convenience collector — walks `Vec<NalUnit>` and groups recognized
/// VPS / SPS / PPS by id. Partial-success-tolerant: bad NALs emit
/// `tracing::warn!` and are skipped; `Err` only when every parameter-set
/// NAL fails.
pub fn parse_parameter_sets(nals: &[NalUnit]) -> Result<H266ParameterSets, CodecParseError> {
    let mut vpses: BTreeMap<u8, H266Vps> = BTreeMap::new();
    let mut spses: BTreeMap<u8, H266Sps> = BTreeMap::new();
    let mut ppses: BTreeMap<u8, H266Pps> = BTreeMap::new();
    let mut all_failed = true;
    let mut any_seen = false;

    for nal in nals {
        if let NalUnit::H266 {
            nal_type, payload, ..
        } = nal
        {
            // Per H.266 V4 Table 5: VPS_NUT=14, SPS_NUT=15, PPS_NUT=16.
            match nal_type {
                14 => {
                    any_seen = true;
                    match parse_vps(payload) {
                        Ok(v) => {
                            vpses.insert(v.vps_id, v);
                            all_failed = false;
                        }
                        Err(e) => tracing::warn!(target: "tst_core::codec::h266",
                            error = ?e, "skipping malformed VPS"),
                    }
                }
                15 => {
                    any_seen = true;
                    match parse_sps(payload) {
                        Ok(s) => {
                            spses.insert(s.sps_id, s);
                            all_failed = false;
                        }
                        Err(e) => tracing::warn!(target: "tst_core::codec::h266",
                            error = ?e, "skipping malformed SPS"),
                    }
                }
                16 => {
                    any_seen = true;
                    match parse_pps(payload) {
                        Ok(p) => {
                            ppses.insert(p.pps_id, p);
                            all_failed = false;
                        }
                        Err(e) => tracing::warn!(target: "tst_core::codec::h266",
                            error = ?e, "skipping malformed PPS"),
                    }
                }
                _ => {}
            }
        }
    }
    if any_seen && all_failed {
        return Err(CodecParseError::EngineError(
            "all H.266 parameter-set NALs failed to parse".into(),
        ));
    }
    Ok(H266ParameterSets {
        vpses: vpses.into_values().collect(),
        spses: spses.into_values().collect(),
        ppses: ppses.into_values().collect(),
    })
}

#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H266ParameterSets {
    pub vpses: Vec<H266Vps>,
    pub spses: Vec<H266Sps>,
    pub ppses: Vec<H266Pps>,
}
