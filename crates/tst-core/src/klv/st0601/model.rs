//! ST 0601 typed model — the `UasDatalinkLs` flat struct plus derived
//! read-only views (`GeoPoint`, `Attitude`, `FieldOfView`, `Corners`)
//! and the encode-side `EncodeConfig`.

use crate::error::KlvFieldError;
use crate::klv::pack::OwnedRawField;
use crate::klv::universal_label::UniversalLabel;
use alloc::string::String;
use alloc::vec::Vec;

use super::packs::{
    AirbaseLocations, ControlCommand, CountryCodes, ImageHorizonPixels, MetadataSubstreamId,
    PayloadList, SensorFrameRate, ViewDomain, WavelengthRecord, Waypoint, WeaponsStore,
};

#[must_use]
#[derive(Debug, Clone, PartialEq)]
pub struct UasDatalinkLs {
    pub universal_label: UniversalLabel,
    pub declared_version: u8,

    // Identity
    pub mission_id: Option<String>,
    pub platform_tail_number: Option<String>,
    pub platform_designation: Option<String>,
    pub image_source_sensor: Option<String>,
    pub image_coordinate_system: Option<String>,
    pub platform_call_sign: Option<String>,
    pub uas_ls_version: Option<u8>,

    // Time
    pub timestamp_us: Option<u64>,

    // Platform state
    /// Item 5: Platform Heading Angle — encode range [0, 360] deg (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub platform_heading_deg: Option<f64>,
    /// Item 6: Platform Pitch Angle — encode range [-20, 20] deg (int16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`]; for
    /// the full ±90° range use [`Self::platform_pitch_full_deg`] (Item 90).
    pub platform_pitch_deg: Option<f64>,
    /// Item 7: Platform Roll Angle — encode range [-50, 50] deg (int16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`]; for
    /// the full ±90° range use [`Self::platform_roll_full_deg`] (Item 91).
    pub platform_roll_deg: Option<f64>,
    /// Item 8: Platform True Airspeed — encode range [0, 255] m/s (uint8).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub platform_true_airspeed: Option<f64>,
    /// Item 9: Platform Indicated Airspeed — encode range [0, 255] m/s (uint8).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub platform_indicated_airspeed: Option<f64>,
    /// Item 90: Platform Pitch Angle (Full) — encode range [-90, 90] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    /// Full-range twin of [`Self::platform_pitch_deg`] (Item 6, ±20°).
    pub platform_pitch_full_deg: Option<f64>,
    /// Item 91: Platform Roll Angle (Full) — encode range [-90, 90] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    /// Full-range twin of [`Self::platform_roll_deg`] (Item 7, ±50°).
    pub platform_roll_full_deg: Option<f64>,
    /// Item 94: MIIS Core Identifier (MISB ST 1204 binary format).
    /// Raw bytes — decode/inspect via [`crate::klv::st1204`].
    pub miis_core_id: Option<Vec<u8>>,
    /// Item 50: Platform Angle of Attack — encode range [-20, 20] deg (int16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub platform_angle_of_attack_deg: Option<f64>,

    // Sensor pose & position
    /// Item 13: Sensor Latitude — encode range [-90, 90] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub sensor_lat_deg: Option<f64>,
    /// Item 14: Sensor Longitude — encode range [-180, 180] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub sensor_lon_deg: Option<f64>,
    /// Item 15: Sensor True Altitude — encode range [-900, 19000] m (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub sensor_alt_m: Option<f64>,
    /// Item 75: Sensor Ellipsoid Height (WGS84) — encode range [-900, 19000] m (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`];
    /// for the extended [-900, 40000] m range use
    /// [`Self::sensor_ellipsoid_height_extended_m`] (Item 104, IMAPB).
    /// Per ST 0601.19 requirements 0601.9-20/-21, a decoder that
    /// understands Item 104 shall use it and ignore this restricted
    /// twin when both are present; this struct populates both fields
    /// on decode — prefer [`Self::sensor_ellipsoid_height_extended_m`]
    /// when producing new records.
    pub sensor_ellipsoid_height_m: Option<f64>,
    /// Item 16: Sensor Horizontal FOV — encode range [0, 180] deg (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub sensor_hfov_deg: Option<f64>,
    /// Item 17: Sensor Vertical FOV — encode range [0, 180] deg (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub sensor_vfov_deg: Option<f64>,
    /// Item 18: Sensor Relative Azimuth — encode range [0, 360] deg (uint32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub sensor_rel_az_deg: Option<f64>,
    /// Item 19: Sensor Relative Elevation — encode range [-180, 180] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub sensor_rel_el_deg: Option<f64>,
    /// Item 20: Sensor Relative Roll — encode range [0, 360] deg (uint32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub sensor_rel_roll_deg: Option<f64>,

    // Ranging & frame center
    /// Item 21: Slant Range — encode range [0, 5000000] m (uint32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub slant_range_m: Option<f64>,
    /// Item 22: Target Width — encode range [0, 10000] m (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`];
    /// for the extended [0, 1500000] m range use
    /// [`Self::target_width_extended_m`] (Item 96, IMAPB).
    /// Per ST 0601.19 requirements 0601.9-20/-21, a decoder that
    /// understands Item 96 shall use it and ignore this restricted
    /// twin when both are present; this struct populates both fields
    /// on decode — prefer [`Self::target_width_extended_m`] when
    /// producing new records.
    pub target_width_m: Option<f64>,
    /// Item 23: Frame Center Latitude — encode range [-90, 90] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub frame_center_lat_deg: Option<f64>,
    /// Item 24: Frame Center Longitude — encode range [-180, 180] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub frame_center_lon_deg: Option<f64>,
    /// Item 25: Frame Center Elevation — encode range [-900, 19000] m (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub frame_center_elev_m: Option<f64>,
    /// Item 78: Frame Center Height Above Ellipsoid (WGS84) — encode range
    /// [-900, 19000] m (uint16). Values outside raise
    /// [`crate::error::KlvEncodeError::OutOfRange`].
    pub frame_center_ellipsoid_height_m: Option<f64>,

    // Image corners — offsets from frame center (tags 26-33)
    /// Item 26: Offset Corner Latitude Point 1 — encode range [-0.075, 0.075] deg (int16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`]; for the full
    /// latitude range use [`Self::corner_lat_p1_deg`] (Item 82, ±90°).
    pub corner_lat_offset_p1_deg: Option<f64>,
    /// Item 27: Offset Corner Longitude Point 1 — encode range [-0.075, 0.075] deg (int16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`]; for the full
    /// longitude range use [`Self::corner_lon_p1_deg`] (Item 83, ±180°).
    pub corner_lon_offset_p1_deg: Option<f64>,
    /// Item 28: Offset Corner Latitude Point 2 — encode range [-0.075, 0.075] deg (int16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`]; for the full
    /// latitude range use [`Self::corner_lat_p2_deg`] (Item 84, ±90°).
    pub corner_lat_offset_p2_deg: Option<f64>,
    /// Item 29: Offset Corner Longitude Point 2 — encode range [-0.075, 0.075] deg (int16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`]; for the full
    /// longitude range use [`Self::corner_lon_p2_deg`] (Item 85, ±180°).
    pub corner_lon_offset_p2_deg: Option<f64>,
    /// Item 30: Offset Corner Latitude Point 3 — encode range [-0.075, 0.075] deg (int16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`]; for the full
    /// latitude range use [`Self::corner_lat_p3_deg`] (Item 86, ±90°).
    pub corner_lat_offset_p3_deg: Option<f64>,
    /// Item 31: Offset Corner Longitude Point 3 — encode range [-0.075, 0.075] deg (int16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`]; for the full
    /// longitude range use [`Self::corner_lon_p3_deg`] (Item 87, ±180°).
    pub corner_lon_offset_p3_deg: Option<f64>,
    /// Item 32: Offset Corner Latitude Point 4 — encode range [-0.075, 0.075] deg (int16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`]; for the full
    /// latitude range use [`Self::corner_lat_p4_deg`] (Item 88, ±90°).
    pub corner_lat_offset_p4_deg: Option<f64>,
    /// Item 33: Offset Corner Longitude Point 4 — encode range [-0.075, 0.075] deg (int16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`]; for the full
    /// longitude range use [`Self::corner_lon_p4_deg`] (Item 89, ±180°).
    pub corner_lon_offset_p4_deg: Option<f64>,

    // Image corners — full lat/lon (tags 82-89, ST 0601.13+)
    /// Item 82: Corner Latitude Point 1 (Full) — encode range [-90, 90] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    /// Full-range twin of [`Self::corner_lat_offset_p1_deg`] (Item 26, ±0.075°).
    pub corner_lat_p1_deg: Option<f64>,
    /// Item 83: Corner Longitude Point 1 (Full) — encode range [-180, 180] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    /// Full-range twin of [`Self::corner_lon_offset_p1_deg`] (Item 27, ±0.075°).
    pub corner_lon_p1_deg: Option<f64>,
    /// Item 84: Corner Latitude Point 2 (Full) — encode range [-90, 90] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    /// Full-range twin of [`Self::corner_lat_offset_p2_deg`] (Item 28, ±0.075°).
    pub corner_lat_p2_deg: Option<f64>,
    /// Item 85: Corner Longitude Point 2 (Full) — encode range [-180, 180] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    /// Full-range twin of [`Self::corner_lon_offset_p2_deg`] (Item 29, ±0.075°).
    pub corner_lon_p2_deg: Option<f64>,
    /// Item 86: Corner Latitude Point 3 (Full) — encode range [-90, 90] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    /// Full-range twin of [`Self::corner_lat_offset_p3_deg`] (Item 30, ±0.075°).
    pub corner_lat_p3_deg: Option<f64>,
    /// Item 87: Corner Longitude Point 3 (Full) — encode range [-180, 180] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    /// Full-range twin of [`Self::corner_lon_offset_p3_deg`] (Item 31, ±0.075°).
    pub corner_lon_p3_deg: Option<f64>,
    /// Item 88: Corner Latitude Point 4 (Full) — encode range [-90, 90] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    /// Full-range twin of [`Self::corner_lat_offset_p4_deg`] (Item 32, ±0.075°).
    pub corner_lat_p4_deg: Option<f64>,
    /// Item 89: Corner Longitude Point 4 (Full) — encode range [-180, 180] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    /// Full-range twin of [`Self::corner_lon_offset_p4_deg`] (Item 33, ±0.075°).
    pub corner_lon_p4_deg: Option<f64>,

    // Target location & tracking (tags 40-46)
    /// Item 40: Target Location Latitude — encode range [-90, 90] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub target_location_lat_deg: Option<f64>,
    /// Item 41: Target Location Longitude — encode range [-180, 180] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub target_location_lon_deg: Option<f64>,
    /// Item 42: Target Location Elevation — encode range [-900, 19000] m (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub target_location_elev_m: Option<f64>,
    /// Item 43: Target Track Gate Width — encode range [0, 510] px (uint8).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub target_track_gate_width_px: Option<f64>,
    /// Item 44: Target Track Gate Height — encode range [0, 510] px (uint8).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub target_track_gate_height_px: Option<f64>,
    /// Item 45: Target Error Estimate - CE90 — encode range [0, 4095] m (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub target_error_ce90_m: Option<f64>,
    /// Item 46: Target Error Estimate - LE90 — encode range [0, 4095] m (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub target_error_le90_m: Option<f64>,

    // Weather / atmospheric (tags 35-38, 49, 53-55)
    /// Item 35: Wind Direction — encode range [0, 360] deg (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub wind_direction_deg: Option<f64>,
    /// Item 36: Wind Speed — encode range [0, 100] m/s (uint8).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub wind_speed: Option<f64>,
    /// Item 37: Static Pressure — encode range [0, 5000] mbar (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub static_pressure_mbar: Option<f64>,
    /// Item 38: Density Altitude — encode range [-900, 19000] m (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`];
    /// for the extended [-900, 40000] m range use
    /// [`Self::density_altitude_extended_m`] (Item 103, IMAPB).
    /// Per ST 0601.19 requirements 0601.9-20/-21, a decoder that
    /// understands Item 103 shall use it and ignore this restricted
    /// twin when both are present; this struct populates both fields
    /// on decode — prefer [`Self::density_altitude_extended_m`] when
    /// producing new records.
    pub density_altitude_m: Option<f64>,
    /// Item 49: Differential Pressure — encode range [0, 5000] mbar (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub differential_pressure_mbar: Option<f64>,
    /// Item 53: Airfield Barometric Pressure — encode range [0, 5000] mbar (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub airfield_barometric_pressure_mbar: Option<f64>,
    /// Item 54: Airfield Elevation — encode range [-900, 19000] m (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub airfield_elevation_m: Option<f64>,
    /// Item 55: Relative Humidity — encode range [0, 100] % (uint8).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub relative_humidity_pct: Option<f64>,

    // Extended platform state (tags 51, 52, 56-58, 64, 92, 93)
    /// Item 51: Platform Vertical Speed — encode range [-180, 180] m/s (int16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub platform_vertical_speed: Option<f64>,
    /// Item 52: Platform Sideslip Angle — encode range [-20, 20] deg (int16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`]; for
    /// the full ±180° range use [`Self::platform_sideslip_full_deg`] (Item 93).
    pub platform_sideslip_deg: Option<f64>,
    /// Item 56: Platform Ground Speed — encode range [0, 255] m/s (uint8).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub platform_ground_speed: Option<f64>,
    /// Item 57: Ground Range — encode range [0, 5000000] m (uint32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub ground_range_m: Option<f64>,
    /// Item 58: Platform Fuel Remaining — encode range [0, 10000] kg (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub platform_fuel_remaining_kg: Option<f64>,
    /// Item 64: Platform Magnetic Heading — encode range [0, 360] deg (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub platform_magnetic_heading_deg: Option<f64>,
    /// Item 92: Platform Angle of Attack (Full) — encode range [-90, 90] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub platform_angle_of_attack_full_deg: Option<f64>,
    /// Item 93: Platform Sideslip Angle (Full) — encode range [-180, 180] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    /// Full-range twin of [`Self::platform_sideslip_deg`] (Item 52, ±20°).
    pub platform_sideslip_full_deg: Option<f64>,

    // Alternate platform (tags 67-69, 71, 76)
    /// Item 67: Alternate Platform Latitude — encode range [-90, 90] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub alternate_platform_lat_deg: Option<f64>,
    /// Item 68: Alternate Platform Longitude — encode range [-180, 180] deg (int32).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub alternate_platform_lon_deg: Option<f64>,
    /// Item 69: Alternate Platform Altitude — encode range [-900, 19000] m (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    /// Not to be confused with [`Self::alternate_platform_ellipsoid_height_extended_m`]
    /// (Item 105) — Item 105 extends Item 76 (Alternate Platform Ellipsoid
    /// Height, a WGS84 ellipsoid-height item), not this plain-altitude item;
    /// Item 69 has no IMAPB extended-range twin of its own. Per ST 0601.19
    /// §8.105, legacy systems preferring one representation should favor
    /// Item 105 over Item 76 over this item — consistent with the
    /// requirements 0601.9-20/-21 extended-representation precedence rule
    /// on the 76/105 pair (see [`Self::alternate_platform_ellipsoid_height_m`]).
    pub alternate_platform_alt_m: Option<f64>,
    /// Item 71: Alternate Platform Heading — encode range [0, 360] deg (uint16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub alternate_platform_heading_deg: Option<f64>,
    /// Item 76: Alternate Platform Ellipsoid Height (WGS84) — encode range
    /// [-900, 19000] m (uint16). Values outside raise
    /// [`crate::error::KlvEncodeError::OutOfRange`]; for the extended
    /// [-900, 40000] m range use
    /// [`Self::alternate_platform_ellipsoid_height_extended_m`] (Item 105, IMAPB).
    /// Per ST 0601.19 requirements 0601.9-20/-21, a decoder that
    /// understands Item 105 shall use it and ignore this restricted
    /// twin when both are present; this struct populates both fields
    /// on decode — prefer
    /// [`Self::alternate_platform_ellipsoid_height_extended_m`] when
    /// producing new records.
    pub alternate_platform_ellipsoid_height_m: Option<f64>,

    // Sensor velocity (tags 79-80)
    /// Item 79: Sensor North Velocity — encode range [-327, 327] m/s (int16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub sensor_north_velocity: Option<f64>,
    /// Item 80: Sensor East Velocity — encode range [-327, 327] m/s (int16).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub sensor_east_velocity: Option<f64>,

    // Extended-range items (ST 1201.5 IMAPB-encoded, tags 96-134) — WP-B
    // Table B1. Unlike the fixed-width LinearRange fields above, IMAPB
    // wire values decode at any length 1..=max_len and encode at
    // default_len; out-of-range encodes error by default or, under
    // `OutOfRangePolicy::Indicator`, emit the tag's ST 1201.5
    // BelowMin/AboveMax special (see `Self::imapb_specials`).
    /// Item 96: Target Width Extended — ST 1201.5 IMAPB range
    /// [0, 1500000] m, wire length 1..=8 bytes (encode emits 3 bytes).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    /// Extended-range twin of [`Self::target_width_m`] (Item 22, uint16,
    /// [0, 10000] m) — see that field's docs for the ST 0601.19
    /// extended-representation precedence rule (prefer this field).
    pub target_width_extended_m: Option<f64>,
    /// Item 103: Density Altitude Extended — ST 1201.5 IMAPB range
    /// [-900, 40000] m, wire length 1..=8 bytes (encode emits 3 bytes).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    /// Extended-range twin of [`Self::density_altitude_m`] (Item 38,
    /// uint16, [-900, 19000] m) — see that field's docs for the
    /// ST 0601.19 extended-representation precedence rule (prefer this
    /// field).
    pub density_altitude_extended_m: Option<f64>,
    /// Item 104: Sensor Ellipsoid Height Extended — ST 1201.5 IMAPB range
    /// [-900, 40000] m, wire length 1..=8 bytes (encode emits 3 bytes).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    /// Extended-range twin of [`Self::sensor_ellipsoid_height_m`]
    /// (Item 75, uint16, [-900, 19000] m) — see that field's docs for
    /// the ST 0601.19 extended-representation precedence rule (prefer
    /// this field).
    pub sensor_ellipsoid_height_extended_m: Option<f64>,
    /// Item 105: Alternate Platform Ellipsoid Height Extended —
    /// ST 1201.5 IMAPB range [-900, 40000] m, wire length 1..=8 bytes
    /// (encode emits 3 bytes). Values outside raise
    /// [`crate::error::KlvEncodeError::OutOfRange`]. Extended-range twin
    /// of [`Self::alternate_platform_ellipsoid_height_m`] (Item 76,
    /// uint16, [-900, 19000] m) — see that field's docs for the
    /// ST 0601.19 extended-representation precedence rule (prefer this
    /// field); see also [`Self::alternate_platform_alt_m`]
    /// (Item 69) for the disambiguation from plain (non-ellipsoid) altitude.
    pub alternate_platform_ellipsoid_height_extended_m: Option<f64>,
    /// Item 109: Range To Recovery Location — distance from current
    /// position to airframe recovery position. ST 1201.5 IMAPB range
    /// [0, 21000] km, wire length 1..=4 bytes (encode emits 3 bytes).
    /// Values outside raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub range_to_recovery_km: Option<f64>,
    /// Item 112: Platform Course Angle — direction the aircraft is
    /// moving relative to True North. ST 1201.5 IMAPB range [0, 360] deg,
    /// wire length 1..=8 bytes (encode emits 2 bytes). Values outside
    /// raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub platform_course_angle_deg: Option<f64>,
    /// Item 113: Altitude AGL — Above Ground Level height above the
    /// ground/water. ST 1201.5 IMAPB range [-900, 40000] m, wire length
    /// 1..=4 bytes (encode emits 3 bytes). Values outside raise
    /// [`crate::error::KlvEncodeError::OutOfRange`].
    pub altitude_agl_m: Option<f64>,
    /// Item 114: Radar Altimeter — height above the ground/water as
    /// reported by a RADAR altimeter (AGL, see [`Self::altitude_agl_m`]).
    /// ST 1201.5 IMAPB range [-900, 40000] m, wire length 1..=4 bytes
    /// (encode emits 3 bytes). Values outside raise
    /// [`crate::error::KlvEncodeError::OutOfRange`].
    pub radar_altimeter_m: Option<f64>,
    /// Item 117: Sensor Azimuth Rate — rate the sensor's azimuth angle
    /// is changing. ST 1201.5 IMAPB range [-1000, 1000] deg/s, wire
    /// length 1..=4 bytes (encode emits 2 bytes). Values outside raise
    /// [`crate::error::KlvEncodeError::OutOfRange`].
    pub sensor_azimuth_rate_dps: Option<f64>,
    /// Item 118: Sensor Elevation Rate — rate the sensor's elevation
    /// angle is changing. ST 1201.5 IMAPB range [-1000, 1000] deg/s,
    /// wire length 1..=4 bytes (encode emits 3 bytes). Values outside
    /// raise [`crate::error::KlvEncodeError::OutOfRange`].
    pub sensor_elevation_rate_dps: Option<f64>,
    /// Item 119: Sensor Roll Rate — rate the sensor's roll angle is
    /// changing. ST 1201.5 IMAPB range [-1000, 1000] deg/s, wire length
    /// 1..=4 bytes (encode emits 2 bytes). Values outside raise
    /// [`crate::error::KlvEncodeError::OutOfRange`].
    pub sensor_roll_rate_dps: Option<f64>,
    /// Item 120: On-board MI Storage Percent Full — amount of on-board
    /// Motion Imagery storage used, as a percentage of total storage.
    /// ST 1201.5 IMAPB range [0, 100] %, wire length 1..=3 bytes (encode
    /// emits 2 bytes). Values outside raise
    /// [`crate::error::KlvEncodeError::OutOfRange`].
    pub mi_storage_percent_full: Option<f64>,
    /// Item 132: Transmission Frequency — radio frequency used to
    /// transmit the Motion Imagery. ST 1201.5 IMAPB range [1, 99999] MHz,
    /// wire length 1..=4 bytes (encode emits 3 bytes). Values outside
    /// raise [`crate::error::KlvEncodeError::OutOfRange`]. First IMAPB
    /// item whose own tag number is 2-byte BER-OID encoded (like Items
    /// 129/135 in the raw/UTF-8 set).
    pub transmission_frequency_mhz: Option<f64>,
    /// Item 134: Zoom Percentage — for a variable-zoom system, the
    /// percentage of zoom. ST 1201.5 IMAPB range [0, 100] %, wire length
    /// 1..=4 bytes (encode emits 2 bytes). Values outside raise
    /// [`crate::error::KlvEncodeError::OutOfRange`].
    pub zoom_percentage: Option<f64>,

    // Var-length int/enum items (ST 0601 WP-B Table B2, tags 110-139) —
    // MISB variable-length truncatable integer encoding (see
    // `crate::klv::length::{read_var_uint, read_var_int}`): the TLV
    // length IS the byte count (not BER-OID), decode accepts any wire
    // length 1..=max_len, encode always emits the shortest form. Direct
    // integers/enums, not IMAPB-scaled floats.
    /// Item 110: Time Airborne — cumulative time the platform has been
    /// airborne. MISB var-length uint, wire length 1..=4 bytes (seconds).
    pub time_airborne_s: Option<u32>,
    /// Item 111: Propulsion Unit Speed — rotational speed of the
    /// platform's propulsion unit. MISB var-length uint, wire length
    /// 1..=4 bytes (RPM).
    pub propulsion_unit_speed_rpm: Option<u32>,
    /// Item 123: Number of NAVSATs in View — count of navigation
    /// satellites contributing to the platform's position solution.
    /// MISB var-length uint, wire length 1 byte.
    pub navsats_in_view: Option<u8>,
    /// Item 124: Positioning Method Source — bitfield naming which
    /// satellite positioning system(s) contributed to the platform's
    /// position solution. MISB var-length uint, wire length 1 byte.
    ///
    /// | Bit | System |
    /// |---:|---|
    /// | 0 | INS |
    /// | 1 | GPS |
    /// | 2 | Galileo |
    /// | 3 | QZSS |
    /// | 4 | NAVIC |
    /// | 5 | GLONASS |
    /// | 6 | BeiDou-1 |
    /// | 7 | BeiDou-2 |
    ///
    /// Per ST 0601.19 §8.124 a conformant producer sets at least one bit
    /// (an all-zero value is out of the item's declared `1..255` range);
    /// decode stays lenient and does not reject `0x00`.
    pub positioning_method_source: Option<u8>,
    /// Item 125: Platform Status — operational status of the platform.
    /// MISB var-length uint, wire length 1 byte. See [`PlatformStatus`].
    pub platform_status: Option<PlatformStatus>,
    /// Item 126: Sensor Control Mode — what is currently controlling
    /// the sensor. MISB var-length uint, wire length 1 byte. See
    /// [`SensorControlMode`].
    pub sensor_control_mode: Option<SensorControlMode>,
    /// Item 131: Take-off Time — MISB ST 0603 timestamp of the
    /// platform's take-off. MISB var-length uint, wire length 1..=8
    /// bytes (microseconds).
    pub take_off_time_us: Option<u64>,
    /// Item 133: On-board MI Storage Capacity — total on-board Motion
    /// Imagery storage capacity (compare
    /// [`Self::mi_storage_percent_full`], Item 120, the *used*
    /// percentage of that capacity). MISB var-length uint, wire length
    /// 1..=4 bytes (gigabytes).
    pub mi_storage_capacity_gb: Option<u32>,
    /// Item 136: Leap Seconds — the TAI-UTC leap-second offset in
    /// effect for [`Self::timestamp_us`]. MISB var-length int, wire
    /// length 1..=4 bytes (seconds).
    pub leap_seconds: Option<i32>,
    /// Item 137: Correction Offset — signed correction applied to a
    /// platform time source (e.g. GPS-disciplined clock drift). MISB
    /// var-length int, wire length 1..=8 bytes (microseconds).
    ///
    /// **Spec typo:** ST 0601.19 §8.137 prints this item's decode type as
    /// "KLVuint" (unsigned), but its own worked example only decodes
    /// correctly as SIGNED (MISB var-length int) — this implementation
    /// follows the worked example, not the printed type name.
    pub correction_offset_us: Option<i64>,
    /// Item 139: Active Payloads — bitmask of which Payload IDs (Item
    /// 138, Payload List — not yet typed-modeled) are currently active.
    /// Raw bytes, LSB-first within each byte: bit *i* of byte 0 is
    /// Payload ID *i* (0-7), byte 1 covers IDs 8-15, and so on
    /// (multi-byte extends upward). Use [`Self::active_payload_ids`] to
    /// iterate the set IDs instead of unpacking the bits by hand.
    pub active_payloads: Option<Vec<u8>>,

    // Misc
    /// Item 47: Generic Flag Data — bitfield per ST 0601.19 Table 3:
    /// bit 0 laser range on, bit 1 auto-track on, bit 2 IR polarity
    /// (set = black hot), bit 3 icing detected, bit 4 slant range
    /// measured (vs. calculated), bit 5 image invalid, bits 6-7 reserved.
    pub generic_flag_data: Option<u8>,
    pub security_local_set: Option<Vec<u8>>,
    /// Item 73: MISB ST 0806 Remote Video Terminal LS bytes.
    /// Pass-through bytes; consumers needing typed access call
    /// `klv::st0806::decode` on `rvt.as_deref()?`. See
    /// [`crate::klv::st0806`] for the typed layer.
    pub rvt: Option<Vec<u8>>,
    /// Tag 74 — VMTI Local Set (MISB ST 0903). Pass-through bytes;
    /// consumers needing typed access call `klv::st0903::decode` on
    /// `vmti.as_deref()?`. See `klv::st0903` for the typed layer.
    /// Sibling-layer pattern matches `security_local_set` (Tag 48 →
    /// `klv::st0102`).
    pub vmti: Option<Vec<u8>>,
    /// Item 95: MISB ST 1206 SAR Motion Imagery Local Set bytes.
    /// Pass-through bytes; interior typing deferred.
    pub sar_mi_local_set: Option<Vec<u8>>,
    /// Item 97: MISB ST 1002 Range Image Local Set bytes. Pass-through
    /// bytes; interior typing deferred.
    pub range_image_local_set: Option<Vec<u8>>,
    /// Item 98: MISB ST 1601 Geo-Registration Local Set bytes.
    /// Pass-through bytes; interior typing deferred.
    pub geo_registration_local_set: Option<Vec<u8>>,
    /// Item 99: MISB ST 1602 Composite Imaging Local Set bytes.
    /// Pass-through bytes; interior typing deferred.
    pub composite_imaging_local_set: Option<Vec<u8>>,
    /// Item 100: MISB ST 1607 Segment Local Set bytes. Pass-through
    /// bytes; interior typing deferred.
    pub segment_local_set: Option<Vec<u8>>,
    /// Item 101: MISB ST 1607 Amend Local Set bytes. Pass-through
    /// bytes; interior typing deferred.
    pub amend_local_set: Option<Vec<u8>>,

    // Raw scalar & string items (tags 39, 60-62, 70, 72, 106-108, 129, 135)
    pub outside_air_temp_c: Option<i8>,
    /// Item 60: Weapon Load — bit-packed nibbles (high→low): store
    /// station, store substation, weapon type, weapon variant. Kept as a
    /// raw opaque `u16`, not a sub-struct — callers needing individual
    /// nibbles shift and mask this value themselves.
    pub weapon_load: Option<u16>,
    pub weapon_fired: Option<u8>,
    pub laser_prf_code: Option<u16>,
    pub alternate_platform_name: Option<String>,
    pub event_start_time_us: Option<u64>,
    pub stream_designator: Option<String>,
    pub operational_base: Option<String>,
    pub broadcast_source: Option<String>,
    pub target_id: Option<String>,
    pub communications_method: Option<String>,

    // Coded enums (tags 34, 63, 77)
    /// Item 34: Icing Detected — icing-detector state. See [`IcingDetected`].
    pub icing_detected: Option<IcingDetected>,
    /// Item 63: Sensor Field of View Name — named FOV preset. See [`SensorFovName`].
    pub sensor_fov_name: Option<SensorFovName>,
    /// Item 77: Operational Mode — operating mode of the portrayed event.
    /// See [`OperationalMode`].
    pub operational_mode: Option<OperationalMode>,

    // Pack & list items (WP-C Table C1)
    /// Item 81: Image Horizon Pixels. See [`ImageHorizonPixels`].
    pub image_horizon: Option<ImageHorizonPixels>,
    /// Item 115: Control Command — MULTI-INSTANCE (ST 0601.19 Table 1
    /// "Multiples Allowed" = Yes); every wire occurrence appends one
    /// [`ControlCommand`]. Empty means none were present.
    pub control_commands: Vec<ControlCommand>,
    /// Item 116: Control Command Verification List — BER-OID command ids
    /// the platform has acknowledged/verified.
    pub control_command_verification: Option<Vec<u64>>,
    /// Item 121: Active Wavelength List — BER-OID wavelength ids
    /// currently active (`0` = none active, exclusive per ST 0601.19
    /// §8.121).
    pub active_wavelengths: Option<Vec<u64>>,
    /// Item 127: Sensor Frame Rate Pack. See [`SensorFrameRate`].
    pub sensor_frame_rate: Option<SensorFrameRate>,
    /// Item 143: Metadata Substream Id. See [`MetadataSubstreamId`].
    pub metadata_substream_id: Option<MetadataSubstreamId>,
    /// Item 122: Country Codes. See [`CountryCodes`].
    pub country_codes: Option<CountryCodes>,
    /// Item 128: Wavelengths List. See [`WavelengthRecord`].
    pub wavelengths_list: Option<Vec<WavelengthRecord>>,
    /// Item 130: Airbase Locations. See [`AirbaseLocations`].
    pub airbase_locations: Option<AirbaseLocations>,
    /// Item 138: Payload List. See [`PayloadList`].
    pub payload_list: Option<PayloadList>,
    /// Item 140: Weapons Stores. See [`WeaponsStore`].
    pub weapons_stores: Option<Vec<WeaponsStore>>,
    /// Item 141: Waypoint List. See [`Waypoint`].
    pub waypoint_list: Option<Vec<Waypoint>>,
    /// Item 142: View Domain. See [`ViewDomain`].
    pub view_domain: Option<ViewDomain>,
    /// Item 102: SDCC-FLP (Standard Deviation and Correlation
    /// Coefficient, Floating-Point). MULTI-INSTANCE per ST 0601.19
    /// Table 1 ("Multiples Allowed" = Yes) — the "Refined Source List"
    /// binding: every wire occurrence appends one [`SdccFlpField`],
    /// capturing which preceding items it refines. See [`SdccFlpField`]
    /// for the capture rule and the encode-side adjacency caveat.
    pub sdcc_flps: Vec<SdccFlpField>,

    // Pass-through
    pub unknown: Vec<OwnedRawField>,
    pub field_errors: Vec<KlvFieldError>,

    /// Tags whose wire value was the INT_MIN sentinel for their signed linear
    /// mapping. INT_MIN is a spec-defined signal (not an error), so the
    /// corresponding typed field is left as `None` and the tag is recorded
    /// here instead of in `field_errors`.
    ///
    /// Use [`crate::klv::st0601::st0601_sentinel_meaning`] to look up the
    /// spec-defined meaning for each tag (Out of Range / Reserved / N/A).
    ///
    /// **Encode semantics:** encoding a record that carries sentinel tags
    /// re-emits the INT_MIN bytes for each sentinel tag whose typed field is
    /// `None`. If the typed field is `Some(v)`, the value `v` is encoded
    /// instead — a non-None field always wins over a `sentinel_tags` entry.
    /// The encoder also auto-injects Tag 65 (version) and the trailing
    /// checksum, so the output byte sequence is not guaranteed to match the
    /// original wire input byte-for-byte; the sentinel *value* round-trips,
    /// but the emission order is: typed fields in ascending tag order, then
    /// `sentinel_tags` entries, then `unknown` fields in caller order.
    pub sentinel_tags: Vec<u32>,

    /// Tags whose ST 1201.5 IMAPB wire value decoded to a spec-defined
    /// special value (§7.2.3 — `+∞`/`−∞`, NaN families, the
    /// MISB-defined `BelowMin`/`AboveMax` overflow signals, or a
    /// user-defined signal) rather than a normal-range float. The
    /// corresponding typed `Option<f64>` field is left `None` and the
    /// tag+special pair is recorded here instead. Use
    /// [`crate::klv::ImapbSpecial`] to inspect which special was
    /// signaled.
    ///
    /// **Encode semantics:** mirrors [`Self::sentinel_tags`] — encoding a
    /// record re-emits the special's bytes (at the tag's `default_len`)
    /// for each `(tag, special)` entry whose typed field is currently
    /// `None`. If the typed field is `Some(v)`, the value wins and the
    /// special entry is not re-emitted. Entries whose tag is not an IMAPB-encoded
    /// item are silently skipped on encode.
    ///
    /// **Decode semantics for the two IMAPB outcomes NOT carried here:**
    /// a wire integer in ST 1201.5's reserved-but-unrecognized
    /// special-value space (`DecodedImapb::ReservedSpecial`) or one that
    /// arithmetic-decodes outside the tag's `[min, max]`
    /// (`DecodedImapb::OutOfRange`) are both treated as producer
    /// non-conformance from this typed consumer's view: they are NOT
    /// pushed here, and the raw wire bytes are NOT preserved. Instead
    /// they are recorded in [`Self::field_errors`] as
    /// [`crate::error::KlvFieldError::OutOfRange`] — `value: f64::NAN`
    /// for `ReservedSpecial` (no arithmetic decode exists for a
    /// non-conformant bit pattern), or the raw arithmetic decode for
    /// `OutOfRange`. This differs from `unknown`/`patch()`, which
    /// preserve raw bytes verbatim for genuinely untyped tags — these
    /// tags ARE typed, so a malformed value is a decode error, not a
    /// pass-through.
    pub imapb_specials: Vec<(u32, crate::klv::imapb::ImapbSpecial)>,
}

/// One captured ST 0601 Item 102 (SDCC-FLP) occurrence, with its
/// "Refined Source List" positional context (ST 0601.19 §8.102).
///
/// Tag 102 is MULTI-INSTANCE (ST 0601.19 Table 1 "Multiples Allowed" =
/// Yes): each occurrence's SDCC-FLP pack (MISB ST 1010.3 — parse with
/// [`crate::klv::st1010::decode_sdcc_flp`]) refines the accuracy of the
/// Local Set items that immediately precede it on the wire. This struct
/// keeps the raw pack bytes rather than a parsed
/// [`crate::klv::st1010::SdccFlp`] so a malformed or foreign-encoder
/// pack still round-trips byte-exact even when this crate cannot parse
/// its interior.
///
/// **Capture semantics (decode):** `preceding_tags` is the `N` wire-order
/// item tags — known or unknown, but never another Tag 102 — immediately
/// before this occurrence, where `N` is this pack's own parsed matrix
/// size (fewer if the Local Set had fewer prior items).
///
/// **Ascending-order emission caveat (encode):** every `sdcc_flps` entry
/// is re-emitted verbatim from one place in tag-ascending order, grouping
/// all occurrences together. This means the *original* interleaving with
/// `preceding_tags` (e.g. tags 13/14/15 followed by one Tag 102
/// occurrence, then tags 5/6/7 followed by another) is generally NOT
/// reproduced on re-encode — the re-encoded wire carries both
/// occurrences back to back instead. `preceding_tags` therefore records
/// what the *original* wire order was, not a guarantee about the
/// re-encoded one; the pack `bytes` themselves are still byte-exact.
#[derive(Debug, Clone, PartialEq)]
pub struct SdccFlpField {
    /// Wire-order item tags immediately preceding this occurrence (see
    /// struct doc for the capture rule). Empty if this occurrence had no
    /// preceding items (matrix size 0, or fewer prior items than the
    /// matrix size — spec-legal-but-degenerate, not an error).
    pub preceding_tags: Vec<u32>,
    /// Raw SDCC-FLP pack bytes, exactly as they appeared on the wire —
    /// parse with [`crate::klv::st1010::decode_sdcc_flp`].
    pub bytes: Vec<u8>,
}

impl Default for UasDatalinkLs {
    fn default() -> Self {
        Self {
            universal_label: UniversalLabel::ST_0601_LS,
            declared_version: UniversalLabel::ST_0601_LS.st0601_version_byte(),
            mission_id: None,
            platform_tail_number: None,
            platform_designation: None,
            image_source_sensor: None,
            image_coordinate_system: None,
            platform_call_sign: None,
            uas_ls_version: None,
            timestamp_us: None,
            platform_heading_deg: None,
            platform_pitch_deg: None,
            platform_roll_deg: None,
            platform_true_airspeed: None,
            platform_indicated_airspeed: None,
            platform_pitch_full_deg: None,
            platform_roll_full_deg: None,
            miis_core_id: None,
            platform_angle_of_attack_deg: None,
            sensor_lat_deg: None,
            sensor_lon_deg: None,
            sensor_alt_m: None,
            sensor_ellipsoid_height_m: None,
            sensor_hfov_deg: None,
            sensor_vfov_deg: None,
            sensor_rel_az_deg: None,
            sensor_rel_el_deg: None,
            sensor_rel_roll_deg: None,
            slant_range_m: None,
            target_width_m: None,
            frame_center_lat_deg: None,
            frame_center_lon_deg: None,
            frame_center_elev_m: None,
            frame_center_ellipsoid_height_m: None,
            corner_lat_offset_p1_deg: None,
            corner_lon_offset_p1_deg: None,
            corner_lat_offset_p2_deg: None,
            corner_lon_offset_p2_deg: None,
            corner_lat_offset_p3_deg: None,
            corner_lon_offset_p3_deg: None,
            corner_lat_offset_p4_deg: None,
            corner_lon_offset_p4_deg: None,
            corner_lat_p1_deg: None,
            corner_lon_p1_deg: None,
            corner_lat_p2_deg: None,
            corner_lon_p2_deg: None,
            corner_lat_p3_deg: None,
            corner_lon_p3_deg: None,
            corner_lat_p4_deg: None,
            corner_lon_p4_deg: None,
            target_location_lat_deg: None,
            target_location_lon_deg: None,
            target_location_elev_m: None,
            target_track_gate_width_px: None,
            target_track_gate_height_px: None,
            target_error_ce90_m: None,
            target_error_le90_m: None,
            wind_direction_deg: None,
            wind_speed: None,
            static_pressure_mbar: None,
            density_altitude_m: None,
            differential_pressure_mbar: None,
            airfield_barometric_pressure_mbar: None,
            airfield_elevation_m: None,
            relative_humidity_pct: None,
            platform_vertical_speed: None,
            platform_sideslip_deg: None,
            platform_ground_speed: None,
            ground_range_m: None,
            platform_fuel_remaining_kg: None,
            platform_magnetic_heading_deg: None,
            platform_angle_of_attack_full_deg: None,
            platform_sideslip_full_deg: None,
            alternate_platform_lat_deg: None,
            alternate_platform_lon_deg: None,
            alternate_platform_alt_m: None,
            alternate_platform_heading_deg: None,
            alternate_platform_ellipsoid_height_m: None,
            sensor_north_velocity: None,
            sensor_east_velocity: None,
            target_width_extended_m: None,
            density_altitude_extended_m: None,
            sensor_ellipsoid_height_extended_m: None,
            alternate_platform_ellipsoid_height_extended_m: None,
            range_to_recovery_km: None,
            platform_course_angle_deg: None,
            altitude_agl_m: None,
            radar_altimeter_m: None,
            sensor_azimuth_rate_dps: None,
            sensor_elevation_rate_dps: None,
            sensor_roll_rate_dps: None,
            mi_storage_percent_full: None,
            transmission_frequency_mhz: None,
            zoom_percentage: None,
            time_airborne_s: None,
            propulsion_unit_speed_rpm: None,
            navsats_in_view: None,
            positioning_method_source: None,
            platform_status: None,
            sensor_control_mode: None,
            take_off_time_us: None,
            mi_storage_capacity_gb: None,
            leap_seconds: None,
            correction_offset_us: None,
            active_payloads: None,
            generic_flag_data: None,
            security_local_set: None,
            rvt: None,
            vmti: None,
            sar_mi_local_set: None,
            range_image_local_set: None,
            geo_registration_local_set: None,
            composite_imaging_local_set: None,
            segment_local_set: None,
            amend_local_set: None,
            outside_air_temp_c: None,
            weapon_load: None,
            weapon_fired: None,
            laser_prf_code: None,
            alternate_platform_name: None,
            event_start_time_us: None,
            stream_designator: None,
            operational_base: None,
            broadcast_source: None,
            target_id: None,
            communications_method: None,
            icing_detected: None,
            sensor_fov_name: None,
            operational_mode: None,
            image_horizon: None,
            control_commands: Vec::new(),
            control_command_verification: None,
            active_wavelengths: None,
            sensor_frame_rate: None,
            metadata_substream_id: None,
            country_codes: None,
            wavelengths_list: None,
            airbase_locations: None,
            payload_list: None,
            weapons_stores: None,
            waypoint_list: None,
            view_domain: None,
            sdcc_flps: Vec::new(),
            unknown: Vec::new(),
            field_errors: Vec::new(),
            sentinel_tags: Vec::new(),
            imapb_specials: Vec::new(),
        }
    }
}

// ============================================================================
// Coded value enums (tags 34, 63, 77)
// ============================================================================

/// Item 34: Icing Detected (ST 0601.19 §8.34) — flag for icing detected at
/// the aircraft location, sensed by a vibrating-probe ice detector.
///
/// | Code | Meaning |
/// |---:|---|
/// | 0 | `DetectorOff` |
/// | 1 | `NoIcingDetected` |
/// | 2 | `IcingDetected` |
/// | other | `Other(code)` — wire-unknown, round-trips byte-exact |
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IcingDetected {
    DetectorOff,
    NoIcingDetected,
    IcingDetected,
    /// Wire-unknown codepoint; round-trips byte-exact through encode.
    Other(u8),
}

impl IcingDetected {
    /// Every variant, unit variants in declaration order plus one
    /// representative `Other`. Binding crates iterate this at test time to
    /// prove their mirrors cover every variant (they cannot `match`
    /// exhaustively across the crate boundary — `#[non_exhaustive]`).
    /// Completeness is pinned by the wildcard-free `match` in this
    /// module's `variant_inventory` tests.
    ///
    /// The `Other` payload here is an ARBITRARY representative, chosen only
    /// to be outside the mapped range so it round-trips; it carries no
    /// meaning and callers must not treat it as a sentinel value.
    pub const ALL: &'static [Self] = &[
        Self::DetectorOff,
        Self::NoIcingDetected,
        Self::IcingDetected,
        Self::Other(0xFF),
    ];

    /// ST 0601.19 §8.34 wire codepoint → variant (`Other(b)` for unknown codes).
    pub fn from_wire(b: u8) -> Self {
        match b {
            0 => Self::DetectorOff,
            1 => Self::NoIcingDetected,
            2 => Self::IcingDetected,
            other => Self::Other(other),
        }
    }

    /// Variant → ST 0601.19 §8.34 wire codepoint (inverse of [`Self::from_wire`]).
    pub fn to_wire(self) -> u8 {
        match self {
            Self::DetectorOff => 0,
            Self::NoIcingDetected => 1,
            Self::IcingDetected => 2,
            Self::Other(b) => b,
        }
    }
}

/// Item 63: Sensor Field of View Name (ST 0601.19 §8.63) — indicates the
/// Motion Imagery sensor's current lens type / FOV preset.
///
/// | Code | Meaning |
/// |---:|---|
/// | 0 | `Ultranarrow` |
/// | 1 | `Narrow` |
/// | 2 | `Medium` |
/// | 3 | `Wide` |
/// | 4 | `Ultrawide` |
/// | 5 | `NarrowMedium` |
/// | 6 | `TwoXUltranarrow` |
/// | 7 | `FourXUltranarrow` |
/// | 8 | `ContinuousZoom` |
/// | other | `Other(code)` — wire-unknown, round-trips byte-exact |
///
/// **Spec discrepancy:** the item's own definition table (§8.63) caps the
/// KLV range at `[0, 7]`, but the Details subsection's worked table — ST
/// 0601.19 §8.63.1 Table 4 — lists a 9th codepoint, `8` = "Continuous
/// Zoom". Modeled per Table 4 since it is the more complete of the two
/// spec tables and real-world encoders emit it.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorFovName {
    Ultranarrow,
    Narrow,
    Medium,
    Wide,
    Ultrawide,
    NarrowMedium,
    TwoXUltranarrow,
    FourXUltranarrow,
    ContinuousZoom,
    /// Wire-unknown codepoint; round-trips byte-exact through encode.
    Other(u8),
}

impl SensorFovName {
    /// Every variant, unit variants in declaration order plus one
    /// representative `Other`. Binding crates iterate this at test time to
    /// prove their mirrors cover every variant (they cannot `match`
    /// exhaustively across the crate boundary — `#[non_exhaustive]`).
    /// Completeness is pinned by the wildcard-free `match` in this
    /// module's `variant_inventory` tests.
    ///
    /// The `Other` payload here is an ARBITRARY representative, chosen only
    /// to be outside the mapped range so it round-trips; it carries no
    /// meaning and callers must not treat it as a sentinel value.
    pub const ALL: &'static [Self] = &[
        Self::Ultranarrow,
        Self::Narrow,
        Self::Medium,
        Self::Wide,
        Self::Ultrawide,
        Self::NarrowMedium,
        Self::TwoXUltranarrow,
        Self::FourXUltranarrow,
        Self::ContinuousZoom,
        Self::Other(0xFF),
    ];

    /// ST 0601.19 §8.63 wire codepoint → variant (`Other(b)` for unknown codes).
    pub fn from_wire(b: u8) -> Self {
        match b {
            0 => Self::Ultranarrow,
            1 => Self::Narrow,
            2 => Self::Medium,
            3 => Self::Wide,
            4 => Self::Ultrawide,
            5 => Self::NarrowMedium,
            6 => Self::TwoXUltranarrow,
            7 => Self::FourXUltranarrow,
            8 => Self::ContinuousZoom,
            other => Self::Other(other),
        }
    }

    /// Variant → ST 0601.19 §8.63 wire codepoint (inverse of [`Self::from_wire`]).
    pub fn to_wire(self) -> u8 {
        match self {
            Self::Ultranarrow => 0,
            Self::Narrow => 1,
            Self::Medium => 2,
            Self::Wide => 3,
            Self::Ultrawide => 4,
            Self::NarrowMedium => 5,
            Self::TwoXUltranarrow => 6,
            Self::FourXUltranarrow => 7,
            Self::ContinuousZoom => 8,
            Self::Other(b) => b,
        }
    }
}

/// Item 77: Operational Mode (ST 0601.19 §8.77) — indicates the mode of
/// operations of the event portrayed in the Motion Imagery, per the
/// §8.77.1 Table 5 enumeration.
///
/// | Code | Meaning |
/// |---:|---|
/// | 0 | `OtherMode` |
/// | 1 | `Operational` |
/// | 2 | `Training` |
/// | 3 | `Exercise` |
/// | 4 | `Maintenance` |
/// | 5 | `Test` |
/// | other | `Other(code)` — wire-unknown, round-trips byte-exact |
///
/// Spec code `0` is named "Other" in Table 5; this crate names it
/// `OtherMode` to avoid colliding with the catch-all `Other(code)`
/// fallback arm above.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationalMode {
    OtherMode,
    Operational,
    Training,
    Exercise,
    Maintenance,
    Test,
    /// Wire-unknown codepoint; round-trips byte-exact through encode.
    Other(u8),
}

impl OperationalMode {
    /// Every variant, unit variants in declaration order plus one
    /// representative `Other`. Binding crates iterate this at test time to
    /// prove their mirrors cover every variant (they cannot `match`
    /// exhaustively across the crate boundary — `#[non_exhaustive]`).
    /// Completeness is pinned by the wildcard-free `match` in this
    /// module's `variant_inventory` tests.
    ///
    /// The `Other` payload here is an ARBITRARY representative, chosen only
    /// to be outside the mapped range so it round-trips; it carries no
    /// meaning and callers must not treat it as a sentinel value.
    pub const ALL: &'static [Self] = &[
        Self::OtherMode,
        Self::Operational,
        Self::Training,
        Self::Exercise,
        Self::Maintenance,
        Self::Test,
        Self::Other(0xFF),
    ];

    /// ST 0601.19 §8.77 wire codepoint → variant (`Other(b)` for unknown codes).
    pub fn from_wire(b: u8) -> Self {
        match b {
            0 => Self::OtherMode,
            1 => Self::Operational,
            2 => Self::Training,
            3 => Self::Exercise,
            4 => Self::Maintenance,
            5 => Self::Test,
            other => Self::Other(other),
        }
    }

    /// Variant → ST 0601.19 §8.77 wire codepoint (inverse of [`Self::from_wire`]).
    pub fn to_wire(self) -> u8 {
        match self {
            Self::OtherMode => 0,
            Self::Operational => 1,
            Self::Training => 2,
            Self::Exercise => 3,
            Self::Maintenance => 4,
            Self::Test => 5,
            Self::Other(b) => b,
        }
    }
}

/// Item 125: Platform Status (ST 0601.19 §8.125) — indicates the
/// operational status of the platform.
///
/// | Code | Meaning |
/// |---:|---|
/// | 0 | `Active` |
/// | 1 | `PreFlight` |
/// | 2 | `PreFlightTaxiing` |
/// | 3 | `RunUp` |
/// | 4 | `TakeOff` |
/// | 5 | `Ingress` |
/// | 6 | `ManualOperation` |
/// | 7 | `AutomatedOrbit` |
/// | 8 | `Transitioning` |
/// | 9 | `Egress` |
/// | 10 | `Landing` |
/// | 11 | `LandedTaxiing` |
/// | 12 | `LandedParked` |
/// | other | `Other(code)` — wire-unknown, round-trips byte-exact |
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformStatus {
    Active,
    PreFlight,
    PreFlightTaxiing,
    RunUp,
    TakeOff,
    Ingress,
    ManualOperation,
    AutomatedOrbit,
    Transitioning,
    Egress,
    Landing,
    LandedTaxiing,
    LandedParked,
    /// Wire-unknown codepoint; round-trips byte-exact through encode.
    Other(u8),
}

impl PlatformStatus {
    /// Every variant, unit variants in declaration order plus one
    /// representative `Other`. Binding crates iterate this at test time to
    /// prove their mirrors cover every variant (they cannot `match`
    /// exhaustively across the crate boundary — `#[non_exhaustive]`).
    /// Completeness is pinned by the wildcard-free `match` in this
    /// module's `variant_inventory` tests.
    ///
    /// The `Other` payload here is an ARBITRARY representative, chosen only
    /// to be outside the mapped range so it round-trips; it carries no
    /// meaning and callers must not treat it as a sentinel value.
    pub const ALL: &'static [Self] = &[
        Self::Active,
        Self::PreFlight,
        Self::PreFlightTaxiing,
        Self::RunUp,
        Self::TakeOff,
        Self::Ingress,
        Self::ManualOperation,
        Self::AutomatedOrbit,
        Self::Transitioning,
        Self::Egress,
        Self::Landing,
        Self::LandedTaxiing,
        Self::LandedParked,
        Self::Other(0xFF),
    ];

    /// ST 0601.19 §8.125 wire codepoint → variant (`Other(b)` for unknown codes).
    pub fn from_wire(b: u8) -> Self {
        match b {
            0 => Self::Active,
            1 => Self::PreFlight,
            2 => Self::PreFlightTaxiing,
            3 => Self::RunUp,
            4 => Self::TakeOff,
            5 => Self::Ingress,
            6 => Self::ManualOperation,
            7 => Self::AutomatedOrbit,
            8 => Self::Transitioning,
            9 => Self::Egress,
            10 => Self::Landing,
            11 => Self::LandedTaxiing,
            12 => Self::LandedParked,
            other => Self::Other(other),
        }
    }

    /// Variant → ST 0601.19 §8.125 wire codepoint (inverse of [`Self::from_wire`]).
    pub fn to_wire(self) -> u8 {
        match self {
            Self::Active => 0,
            Self::PreFlight => 1,
            Self::PreFlightTaxiing => 2,
            Self::RunUp => 3,
            Self::TakeOff => 4,
            Self::Ingress => 5,
            Self::ManualOperation => 6,
            Self::AutomatedOrbit => 7,
            Self::Transitioning => 8,
            Self::Egress => 9,
            Self::Landing => 10,
            Self::LandedTaxiing => 11,
            Self::LandedParked => 12,
            Self::Other(b) => b,
        }
    }
}

/// Item 126: Sensor Control Mode (ST 0601.19 §8.126) — indicates what is
/// currently controlling the sensor.
///
/// | Code | Meaning |
/// |---:|---|
/// | 0 | `Off` |
/// | 1 | `HomePosition` |
/// | 2 | `Uncontrolled` |
/// | 3 | `ManualControl` |
/// | 4 | `Calibrating` |
/// | 5 | `AutoHoldingPosition` |
/// | 6 | `AutoTracking` |
/// | other | `Other(code)` — wire-unknown, round-trips byte-exact |
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorControlMode {
    Off,
    HomePosition,
    Uncontrolled,
    ManualControl,
    Calibrating,
    AutoHoldingPosition,
    AutoTracking,
    /// Wire-unknown codepoint; round-trips byte-exact through encode.
    Other(u8),
}

impl SensorControlMode {
    /// Every variant, unit variants in declaration order plus one
    /// representative `Other`. Binding crates iterate this at test time to
    /// prove their mirrors cover every variant (they cannot `match`
    /// exhaustively across the crate boundary — `#[non_exhaustive]`).
    /// Completeness is pinned by the wildcard-free `match` in this
    /// module's `variant_inventory` tests.
    ///
    /// The `Other` payload here is an ARBITRARY representative, chosen only
    /// to be outside the mapped range so it round-trips; it carries no
    /// meaning and callers must not treat it as a sentinel value.
    pub const ALL: &'static [Self] = &[
        Self::Off,
        Self::HomePosition,
        Self::Uncontrolled,
        Self::ManualControl,
        Self::Calibrating,
        Self::AutoHoldingPosition,
        Self::AutoTracking,
        Self::Other(0xFF),
    ];

    /// ST 0601.19 §8.126 wire codepoint → variant (`Other(b)` for unknown codes).
    pub fn from_wire(b: u8) -> Self {
        match b {
            0 => Self::Off,
            1 => Self::HomePosition,
            2 => Self::Uncontrolled,
            3 => Self::ManualControl,
            4 => Self::Calibrating,
            5 => Self::AutoHoldingPosition,
            6 => Self::AutoTracking,
            other => Self::Other(other),
        }
    }

    /// Variant → ST 0601.19 §8.126 wire codepoint (inverse of [`Self::from_wire`]).
    pub fn to_wire(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::HomePosition => 1,
            Self::Uncontrolled => 2,
            Self::ManualControl => 3,
            Self::Calibrating => 4,
            Self::AutoHoldingPosition => 5,
            Self::AutoTracking => 6,
            Self::Other(b) => b,
        }
    }
}

/// What the `encode*` entry points do when a ranged value falls outside
/// its ST 0601 mapped range.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutOfRangePolicy {
    /// Reject the record with [`crate::error::KlvEncodeError::OutOfRange`] (default).
    #[default]
    Error,
    /// Emit the tag's spec-defined "Out of Range" special value instead of
    /// erroring — ST 0601.19 §7.5 / requirement ST 0601.13-27 (`0x8000` /
    /// `0x80000000` for 2-/4-byte signed mappings). Applies to the tags
    /// whose INT_MIN sentinel means Out of Range (Tags 6, 7, 50, 51, 52,
    /// 79, 80, 90–93 — see
    /// [`st0601_sentinel_meaning`][crate::klv::st0601::st0601_sentinel_meaning]),
    /// all of which are modeled as encodable [`UasDatalinkLs`] fields. Any
    /// non-finite input still returns
    /// [`crate::error::KlvEncodeError::OutOfRange`].
    Indicator,
}

#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncodeConfig {
    pub universal_label: UniversalLabel,
    /// Version byte to use for Tag 65 if the struct's `uas_ls_version` is None.
    pub version: u8,
    /// Policy for ranged values that fall outside their ST 0601 mapped range.
    pub out_of_range_policy: OutOfRangePolicy,
}

impl Default for EncodeConfig {
    fn default() -> Self {
        Self {
            universal_label: UniversalLabel::ST_0601_LS,
            // Tag 65 ("UAS Datalink LS Version Number") encodes the
            // document revision the codebase conforms to: ST 0601.19 = 19.
            // Decoupled from `UL.version_byte()` because the canonical UL
            // per ST 0601.19 §6.2 has byte 13 = 0x00, which is not the
            // ST 0601 version number.
            version: 19,
            out_of_range_policy: OutOfRangePolicy::Error,
        }
    }
}

// ============================================================================
// Composite types
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoPoint {
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_m: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Attitude {
    pub heading_deg: f64,
    pub pitch_deg: f64,
    pub roll_deg: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FieldOfView {
    pub horizontal_deg: f64,
    pub vertical_deg: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Corners {
    /// (lat, lon); upper-left looking forward.
    pub p1: (f64, f64),
    pub p2: (f64, f64),
    pub p3: (f64, f64),
    pub p4: (f64, f64),
}

impl UasDatalinkLs {
    pub fn sensor_position(&self) -> Option<GeoPoint> {
        Some(GeoPoint {
            lat_deg: self.sensor_lat_deg?,
            lon_deg: self.sensor_lon_deg?,
            alt_m: self.sensor_alt_m?,
        })
    }

    pub fn sensor_attitude(&self) -> Option<Attitude> {
        Some(Attitude {
            heading_deg: self.sensor_rel_az_deg?,
            pitch_deg: self.sensor_rel_el_deg?,
            roll_deg: self.sensor_rel_roll_deg?,
        })
    }

    pub fn sensor_fov(&self) -> Option<FieldOfView> {
        Some(FieldOfView {
            horizontal_deg: self.sensor_hfov_deg?,
            vertical_deg: self.sensor_vfov_deg?,
        })
    }

    pub fn platform_attitude(&self) -> Option<Attitude> {
        Some(Attitude {
            heading_deg: self.platform_heading_deg?,
            pitch_deg: self.platform_pitch_deg?,
            roll_deg: self.platform_roll_deg?,
        })
    }

    pub fn frame_center(&self) -> Option<GeoPoint> {
        Some(GeoPoint {
            lat_deg: self.frame_center_lat_deg?,
            lon_deg: self.frame_center_lon_deg?,
            alt_m: self.frame_center_elev_m?,
        })
    }

    pub fn corners(&self) -> Option<Corners> {
        // Prefer absolute (tags 82-89) when fully populated.
        if let (Some(l1), Some(o1), Some(l2), Some(o2), Some(l3), Some(o3), Some(l4), Some(o4)) = (
            self.corner_lat_p1_deg,
            self.corner_lon_p1_deg,
            self.corner_lat_p2_deg,
            self.corner_lon_p2_deg,
            self.corner_lat_p3_deg,
            self.corner_lon_p3_deg,
            self.corner_lat_p4_deg,
            self.corner_lon_p4_deg,
        ) {
            return Some(Corners {
                p1: (l1, o1),
                p2: (l2, o2),
                p3: (l3, o3),
                p4: (l4, o4),
            });
        }
        // Fall back to offsets + frame center.
        let lat0 = self.frame_center_lat_deg?;
        let lon0 = self.frame_center_lon_deg?;
        let (dl1, do1) = (
            self.corner_lat_offset_p1_deg?,
            self.corner_lon_offset_p1_deg?,
        );
        let (dl2, do2) = (
            self.corner_lat_offset_p2_deg?,
            self.corner_lon_offset_p2_deg?,
        );
        let (dl3, do3) = (
            self.corner_lat_offset_p3_deg?,
            self.corner_lon_offset_p3_deg?,
        );
        let (dl4, do4) = (
            self.corner_lat_offset_p4_deg?,
            self.corner_lon_offset_p4_deg?,
        );
        Some(Corners {
            p1: (lat0 + dl1, lon0 + do1),
            p2: (lat0 + dl2, lon0 + do2),
            p3: (lat0 + dl3, lon0 + do3),
            p4: (lat0 + dl4, lon0 + do4),
        })
    }

    /// Iterate the Payload IDs marked active in [`Self::active_payloads`]
    /// (Item 139), decoding the LSB-first bitmask. Empty (or absent)
    /// bytes yield an empty iterator, never a panic.
    pub fn active_payload_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.active_payloads
            .iter()
            .flatten()
            .copied()
            .enumerate()
            .flat_map(|(byte_i, b): (usize, u8)| {
                (0u32..8)
                    .filter(move |bit| b & (1 << bit) != 0)
                    .map(move |bit| byte_i as u32 * 8 + bit)
            })
    }
}

#[cfg(test)]
mod variant_inventory {
    //! Exhaustiveness for this module's wire-code enums (Arc 2 R2). See
    //! [`crate::klv::inventory_test`] for what the generated test pins and why
    //! the compile-time half can only live inside tst-core.
    use super::*;
    use crate::klv::inventory_test;

    inventory_test!(
        icing_detected_all_is_complete_and_round_trips,
        IcingDetected,
        [
            IcingDetected::DetectorOff,
            IcingDetected::NoIcingDetected,
            IcingDetected::IcingDetected,
            IcingDetected::Other(_),
        ]
    );
    inventory_test!(
        sensor_fov_name_all_is_complete_and_round_trips,
        SensorFovName,
        [
            SensorFovName::Ultranarrow,
            SensorFovName::Narrow,
            SensorFovName::Medium,
            SensorFovName::Wide,
            SensorFovName::Ultrawide,
            SensorFovName::NarrowMedium,
            SensorFovName::TwoXUltranarrow,
            SensorFovName::FourXUltranarrow,
            SensorFovName::ContinuousZoom,
            SensorFovName::Other(_),
        ]
    );
    inventory_test!(
        operational_mode_all_is_complete_and_round_trips,
        OperationalMode,
        [
            OperationalMode::OtherMode,
            OperationalMode::Operational,
            OperationalMode::Training,
            OperationalMode::Exercise,
            OperationalMode::Maintenance,
            OperationalMode::Test,
            OperationalMode::Other(_),
        ]
    );
    inventory_test!(
        platform_status_all_is_complete_and_round_trips,
        PlatformStatus,
        [
            PlatformStatus::Active,
            PlatformStatus::PreFlight,
            PlatformStatus::PreFlightTaxiing,
            PlatformStatus::RunUp,
            PlatformStatus::TakeOff,
            PlatformStatus::Ingress,
            PlatformStatus::ManualOperation,
            PlatformStatus::AutomatedOrbit,
            PlatformStatus::Transitioning,
            PlatformStatus::Egress,
            PlatformStatus::Landing,
            PlatformStatus::LandedTaxiing,
            PlatformStatus::LandedParked,
            PlatformStatus::Other(_),
        ]
    );
    inventory_test!(
        sensor_control_mode_all_is_complete_and_round_trips,
        SensorControlMode,
        [
            SensorControlMode::Off,
            SensorControlMode::HomePosition,
            SensorControlMode::Uncontrolled,
            SensorControlMode::ManualControl,
            SensorControlMode::Calibrating,
            SensorControlMode::AutoHoldingPosition,
            SensorControlMode::AutoTracking,
            SensorControlMode::Other(_),
        ]
    );
}
