//! The hand-written parts of the generated codec classes
//! (`generated/codec.rs`): the methods and properties their fields do not
//! give, the one custom field, and the conversion of a class back to the
//! Rust record where a binding function needs it.
//!
//! The generator calls these by name: `<record>_<member>`, in snake case.

use tst_core::codec::CodecParseError;
use tst_core::codec::misp_time::MispTimestamp as RustMispTimestamp;

use crate::codec::MispTimeKindPy;
use crate::generated::codec::{
    H264Sps, H265ProfileTierLevel, H265Sps, H265Vps, H266ProfileTierLevel, H266Sps, MispTimestamp,
    Rational,
};

pub(crate) fn rational_as_float(r: &Rational) -> f64 {
    f64::from(r.num) / f64::from(r.den)
}

// --- MISP timestamp ---------------------------------------------------------

pub(crate) fn misp_timestamp_micros(value_us: u64, time_status: u8) -> MispTimestamp {
    MispTimestamp {
        kind: MispTimeKindPy::Micro,
        time_status,
        value: value_us,
    }
}

pub(crate) fn misp_timestamp_nanos(value_ns: u64, time_status: u8) -> MispTimestamp {
    MispTimestamp {
        kind: MispTimeKindPy::Nano,
        time_status,
        value: value_ns,
    }
}

/// How `repr` names the time base.
pub(crate) fn misp_timestamp_kind_text(kind: MispTimeKindPy) -> &'static str {
    match kind {
        MispTimeKindPy::Micro => "MispTimeKind.MICRO",
        MispTimeKindPy::Nano => "MispTimeKind.NANO",
    }
}

impl From<MispTimestamp> for RustMispTimestamp {
    fn from(ts: MispTimestamp) -> Self {
        match ts.kind {
            MispTimeKindPy::Micro => RustMispTimestamp::micros(ts.value, ts.time_status),
            MispTimeKindPy::Nano => RustMispTimestamp::nanos(ts.value, ts.time_status),
        }
    }
}

// --- H.264 ------------------------------------------------------------------

pub(crate) fn h264_sps_coded_width(s: &H264Sps) -> u32 {
    s.width + s.crop_left + s.crop_right
}

pub(crate) fn h264_sps_coded_height(s: &H264Sps) -> u32 {
    s.height + s.crop_top + s.crop_bottom
}

// --- H.265 ------------------------------------------------------------------

/// `general_profile_space` is not stored on the SPS or the VPS (it is always
/// 0 for the ITU-T registered profiles), so the reconstructed block carries 0.
pub(crate) fn h265_sps_profile_tier_level(s: &H265Sps) -> H265ProfileTierLevel {
    H265ProfileTierLevel {
        general_profile_space: 0,
        general_tier_flag: s.general_tier_flag,
        general_profile_idc: s.general_profile_idc,
        general_profile_compatibility_flags: s.general_profile_compatibility_flags,
        general_progressive_source_flag: s.general_progressive_source_flag,
        general_interlaced_source_flag: s.general_interlaced_source_flag,
        general_non_packed_constraint_flag: s.general_non_packed_constraint_flag,
        general_frame_only_constraint_flag: s.general_frame_only_constraint_flag,
        general_level_idc: s.general_level_idc,
    }
}

pub(crate) fn h265_sps_coded_width(s: &H265Sps) -> u32 {
    s.width + s.crop_left + s.crop_right
}

pub(crate) fn h265_sps_coded_height(s: &H265Sps) -> u32 {
    s.height + s.crop_top + s.crop_bottom
}

pub(crate) fn h265_vps_profile_tier_level(v: &H265Vps) -> H265ProfileTierLevel {
    H265ProfileTierLevel {
        general_profile_space: 0,
        general_tier_flag: v.general_tier_flag,
        general_profile_idc: v.general_profile_idc,
        general_profile_compatibility_flags: v.general_profile_compatibility_flags,
        general_progressive_source_flag: v.general_progressive_source_flag,
        general_interlaced_source_flag: v.general_interlaced_source_flag,
        general_non_packed_constraint_flag: v.general_non_packed_constraint_flag,
        general_frame_only_constraint_flag: v.general_frame_only_constraint_flag,
        general_level_idc: v.general_level_idc,
    }
}

// --- H.266 ------------------------------------------------------------------

pub(crate) fn h266_sps_general_profile_idc(s: &H266Sps) -> u8 {
    s.profile_tier_level.general_profile_idc
}

pub(crate) fn h266_sps_general_tier_flag(s: &H266Sps) -> bool {
    s.profile_tier_level.general_tier_flag
}

pub(crate) fn h266_sps_general_level_idc(s: &H266Sps) -> u8 {
    s.profile_tier_level.general_level_idc
}

pub(crate) fn h266_sps_profile_tier_level(s: &H266Sps) -> H266ProfileTierLevel {
    s.profile_tier_level
}

pub(crate) fn h266_sps_coded_width(s: &H266Sps) -> u32 {
    s.width + s.crop_left + s.crop_right
}

pub(crate) fn h266_sps_coded_height(s: &H266Sps) -> u32 {
    s.height + s.crop_top + s.crop_bottom
}

// --- AV1 --------------------------------------------------------------------

/// `(obu_type, message)` for each OBU the walk could not parse.
pub(crate) fn av1_obu_stream_unparseable_to_py(v: &[(u8, CodecParseError)]) -> Vec<(u8, String)> {
    v.iter().map(|(t, e)| (*t, format!("{e}"))).collect()
}
