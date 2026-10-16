//! ST 0601 tag schema as data. The encoder and decoder loop this table.
//!
//! Entries are pinned against MISB ST 0601.19. Adding or modifying a tag
//! is a one-entry change; do not duplicate the dispatch logic elsewhere.
//!
//! Each row also carries the accessors that bind it to its
//! [`UasDatalinkLs`] field, so decode and encode reach a field through
//! the row and never through a per-tag `match`.

use super::model::{
    IcingDetected, OperationalMode, PlatformStatus, SensorControlMode, SensorFovName, UasDatalinkLs,
};
use crate::error::KlvFieldError;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Encoding {
    /// Raw 1-byte value (e.g. version byte, generic flag data).
    U8,
    /// Raw 1-byte two's-complement value (e.g. outside air temperature).
    I8,
    /// Raw 2-byte big-endian value (bit-packed or code fields, e.g. weapon load).
    U16,
    /// Raw 2-byte big-endian value with a linear range mapping.
    U16Range,
    I16Range,
    /// Raw 4-byte big-endian value with a linear range mapping.
    U32Range,
    I32Range,
    /// 1-byte value mapped to an integer airspeed range (m/s).
    U8Range,
    /// UTF-8 string with a maximum length.
    Utf8 {
        max_bytes: usize,
    },
    /// Raw bytes (variable length); pass-through.
    RawBytes,
    /// Raw 8-byte big-endian unsigned (e.g. timestamp_us).
    U64,
    /// ST 1201.5 IMAPB variable-length float (ST 0601 extended items).
    /// Decode accepts any wire length in 1..=max_len; encode emits default_len
    /// (the spec's own example length for the item).
    Imapb {
        min: f64,
        max: f64,
        max_len: usize,
        default_len: usize,
    },
    /// MISB variable-length unsigned int: length-prefixed truncatable
    /// big-endian encoding — the TLV length
    /// IS the byte count, NOT BER-OID (see `crate::klv::length::read_var_uint`).
    /// Decode accepts any wire length in `1..=max_len`; encode always emits
    /// [`crate::klv::length::write_var_uint_min`]'s shortest form, which may
    /// be shorter than `max_len`.
    VarUint {
        max_len: usize,
    },
    /// Signed twin of [`Encoding::VarUint`] — two's complement,
    /// sign-extended from the MSB (`crate::klv::length::read_var_int` /
    /// `write_var_int_min`).
    VarInt {
        max_len: usize,
    },
    /// Marker for a MISB pack/list item — a small positional structure
    /// or flat list rather than one scalar.
    /// Carries no length/range metadata of its own; decode/encode
    /// dispatch on `spec.id` to the dedicated parse/emit fns in
    /// `packs.rs`.
    Pack,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LinearRange {
    /// True ⇒ value byte length-bit signed, INT_MIN reserved as INVALID.
    /// False ⇒ unsigned, no INVALID.
    pub signed: bool,
    pub byte_length: usize,
    pub min: f64,
    pub max: f64,
}

/// One decoded field value, the shape a [`TagSpec`] accessor reads or writes.
/// Rows whose encoding is [`Encoding::Pack`] never pass through here: a
/// pack is handled by its own parse and emit functions.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FieldValue {
    F64(f64),
    U64(u64),
    I64(i64),
    Str(String),
    Bytes(Vec<u8>),
    /// A coded enum travels as its wire code.
    Code(u8),
}

impl FieldValue {
    /// The error for a value whose variant is not the one the row's
    /// encoding produces. Unreachable by construction, since every caller
    /// builds the value from the row's own encoding.
    fn mismatch(tag: u8) -> KlvFieldError {
        KlvFieldError::InvalidLength {
            tag: u32::from(tag),
            expected: 0,
            got: 0,
        }
    }

    /// The error for an integer that does not fit the field's Rust type.
    fn too_wide(tag: u8, width: usize) -> KlvFieldError {
        KlvFieldError::InvalidLength {
            tag: u32::from(tag),
            expected: width,
            got: 8,
        }
    }

    pub(crate) fn f64(self, tag: u8) -> Result<f64, KlvFieldError> {
        match self {
            Self::F64(v) => Ok(v),
            _ => Err(Self::mismatch(tag)),
        }
    }

    pub(crate) fn code(self, tag: u8) -> Result<u8, KlvFieldError> {
        match self {
            Self::Code(c) => Ok(c),
            _ => Err(Self::mismatch(tag)),
        }
    }

    pub(crate) fn string(self, tag: u8) -> Result<String, KlvFieldError> {
        match self {
            Self::Str(s) => Ok(s),
            _ => Err(Self::mismatch(tag)),
        }
    }

    pub(crate) fn bytes(self, tag: u8) -> Result<Vec<u8>, KlvFieldError> {
        match self {
            Self::Bytes(b) => Ok(b),
            _ => Err(Self::mismatch(tag)),
        }
    }

    fn unsigned(self, tag: u8) -> Result<u64, KlvFieldError> {
        match self {
            Self::U64(v) => Ok(v),
            _ => Err(Self::mismatch(tag)),
        }
    }

    fn signed(self, tag: u8) -> Result<i64, KlvFieldError> {
        match self {
            Self::I64(v) => Ok(v),
            _ => Err(Self::mismatch(tag)),
        }
    }

    pub(crate) fn u8(self, tag: u8) -> Result<u8, KlvFieldError> {
        u8::try_from(self.unsigned(tag)?).map_err(|_| Self::too_wide(tag, 1))
    }

    pub(crate) fn u16(self, tag: u8) -> Result<u16, KlvFieldError> {
        u16::try_from(self.unsigned(tag)?).map_err(|_| Self::too_wide(tag, 2))
    }

    pub(crate) fn u32(self, tag: u8) -> Result<u32, KlvFieldError> {
        u32::try_from(self.unsigned(tag)?).map_err(|_| Self::too_wide(tag, 4))
    }

    pub(crate) fn u64(self, tag: u8) -> Result<u64, KlvFieldError> {
        self.unsigned(tag)
    }

    pub(crate) fn i8(self, tag: u8) -> Result<i8, KlvFieldError> {
        i8::try_from(self.signed(tag)?).map_err(|_| Self::too_wide(tag, 1))
    }

    pub(crate) fn i32(self, tag: u8) -> Result<i32, KlvFieldError> {
        i32::try_from(self.signed(tag)?).map_err(|_| Self::too_wide(tag, 4))
    }

    pub(crate) fn i64(self, tag: u8) -> Result<i64, KlvFieldError> {
        self.signed(tag)
    }
}

/// Reads a row's field from a record; `None` when the field is unset.
pub(crate) type FieldGet = fn(&UasDatalinkLs) -> Option<FieldValue>;
/// Writes a row's field; fails when the value does not fit the field.
pub(crate) type FieldSet = fn(&mut UasDatalinkLs, FieldValue) -> Result<(), KlvFieldError>;

#[derive(Debug, Clone, Copy)]
pub(crate) struct TagSpec {
    pub id: u8,
    pub name: &'static str,
    pub encoding: Encoding,
    pub range: Option<LinearRange>,
    /// `None` for a row with no scalar field: the checksum (tag 1), the
    /// pack rows and the two multi-instance tags (102, 115).
    pub get: Option<FieldGet>,
    pub set: Option<FieldSet>,
}

impl TagSpec {
    /// The `Option<f64>` view of a ranged or IMAPB row's field.
    pub(crate) fn get_f64(&self, r: &UasDatalinkLs) -> Option<f64> {
        match self.get?(r)? {
            FieldValue::F64(v) => Some(v),
            _ => None,
        }
    }

    /// Store `v` into a ranged or IMAPB row's field; a row without a
    /// scalar field ignores the call. Tests only: the encoder reads fields
    /// and the decoder writes them through `set`, never through this view.
    #[cfg(test)]
    pub(crate) fn set_f64(&self, r: &mut UasDatalinkLs, v: f64) {
        if let Some(set) = self.set {
            // A ranged row's `set` accepts any f64.
            let _ = set(r, FieldValue::F64(v));
        }
    }
}

pub(crate) const TAGS: &[TagSpec] = &[
    TagSpec {
        id: 1,
        name: "Checksum",
        encoding: Encoding::U16Range,
        range: None,
        get: None,
        set: None,
    },
    TagSpec {
        id: 2,
        name: "Precision Time Stamp",
        encoding: Encoding::U64,
        range: None,
        get: Some(|r| r.timestamp_us.map(FieldValue::U64)),
        set: Some(|r, v| {
            r.timestamp_us = Some(v.u64(2)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 3,
        name: "Mission ID",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
        get: Some(|r| r.mission_id.clone().map(FieldValue::Str)),
        set: Some(|r, v| {
            r.mission_id = Some(v.string(3)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 4,
        name: "Platform Tail Number",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
        get: Some(|r| r.platform_tail_number.clone().map(FieldValue::Str)),
        set: Some(|r, v| {
            r.platform_tail_number = Some(v.string(4)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 5,
        name: "Platform Heading Angle",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: 0.0,
            max: 360.0,
        }),
        get: Some(|r| r.platform_heading_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.platform_heading_deg = Some(v.f64(5)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 6,
        name: "Platform Pitch Angle",
        encoding: Encoding::I16Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 2,
            min: -20.0,
            max: 20.0,
        }),
        get: Some(|r| r.platform_pitch_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.platform_pitch_deg = Some(v.f64(6)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 7,
        name: "Platform Roll Angle",
        encoding: Encoding::I16Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 2,
            min: -50.0,
            max: 50.0,
        }),
        get: Some(|r| r.platform_roll_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.platform_roll_deg = Some(v.f64(7)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 8,
        name: "Platform True Airspeed",
        encoding: Encoding::U8Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 1,
            min: 0.0,
            max: 255.0,
        }),
        get: Some(|r| r.platform_true_airspeed.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.platform_true_airspeed = Some(v.f64(8)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 9,
        name: "Platform Indicated Airspeed",
        encoding: Encoding::U8Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 1,
            min: 0.0,
            max: 255.0,
        }),
        get: Some(|r| r.platform_indicated_airspeed.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.platform_indicated_airspeed = Some(v.f64(9)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 10,
        name: "Platform Designation",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
        get: Some(|r| r.platform_designation.clone().map(FieldValue::Str)),
        set: Some(|r, v| {
            r.platform_designation = Some(v.string(10)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 11,
        name: "Image Source Sensor",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
        get: Some(|r| r.image_source_sensor.clone().map(FieldValue::Str)),
        set: Some(|r, v| {
            r.image_source_sensor = Some(v.string(11)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 12,
        name: "Image Coordinate System",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
        get: Some(|r| r.image_coordinate_system.clone().map(FieldValue::Str)),
        set: Some(|r, v| {
            r.image_coordinate_system = Some(v.string(12)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 13,
        name: "Sensor Latitude",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -90.0,
            max: 90.0,
        }),
        get: Some(|r| r.sensor_lat_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.sensor_lat_deg = Some(v.f64(13)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 14,
        name: "Sensor Longitude",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -180.0,
            max: 180.0,
        }),
        get: Some(|r| r.sensor_lon_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.sensor_lon_deg = Some(v.f64(14)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 15,
        name: "Sensor True Altitude",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: -900.0,
            max: 19000.0,
        }),
        get: Some(|r| r.sensor_alt_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.sensor_alt_m = Some(v.f64(15)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 16,
        name: "Sensor Horizontal FOV",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: 0.0,
            max: 180.0,
        }),
        get: Some(|r| r.sensor_hfov_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.sensor_hfov_deg = Some(v.f64(16)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 17,
        name: "Sensor Vertical FOV",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: 0.0,
            max: 180.0,
        }),
        get: Some(|r| r.sensor_vfov_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.sensor_vfov_deg = Some(v.f64(17)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 18,
        name: "Sensor Relative Azimuth",
        encoding: Encoding::U32Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 4,
            min: 0.0,
            max: 360.0,
        }),
        get: Some(|r| r.sensor_rel_az_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.sensor_rel_az_deg = Some(v.f64(18)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 19,
        name: "Sensor Relative Elevation",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -180.0,
            max: 180.0,
        }),
        get: Some(|r| r.sensor_rel_el_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.sensor_rel_el_deg = Some(v.f64(19)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 20,
        name: "Sensor Relative Roll",
        encoding: Encoding::U32Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 4,
            min: 0.0,
            max: 360.0,
        }),
        get: Some(|r| r.sensor_rel_roll_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.sensor_rel_roll_deg = Some(v.f64(20)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 21,
        name: "Slant Range",
        encoding: Encoding::U32Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 4,
            min: 0.0,
            max: 5_000_000.0,
        }),
        get: Some(|r| r.slant_range_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.slant_range_m = Some(v.f64(21)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 22,
        name: "Target Width",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: 0.0,
            max: 10_000.0,
        }),
        get: Some(|r| r.target_width_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.target_width_m = Some(v.f64(22)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 23,
        name: "Frame Center Latitude",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -90.0,
            max: 90.0,
        }),
        get: Some(|r| r.frame_center_lat_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.frame_center_lat_deg = Some(v.f64(23)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 24,
        name: "Frame Center Longitude",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -180.0,
            max: 180.0,
        }),
        get: Some(|r| r.frame_center_lon_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.frame_center_lon_deg = Some(v.f64(24)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 25,
        name: "Frame Center Elevation",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: -900.0,
            max: 19000.0,
        }),
        get: Some(|r| r.frame_center_elev_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.frame_center_elev_m = Some(v.f64(25)?);
            Ok(())
        }),
    },
    // Image corner offsets (tags 26-33): i16 mapping ±0.075 deg, INT16_MIN INVALID
    TagSpec {
        id: 26,
        name: "Offset Corner Latitude Point 1",
        encoding: Encoding::I16Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 2,
            min: -0.075,
            max: 0.075,
        }),
        get: Some(|r| r.corner_lat_offset_p1_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.corner_lat_offset_p1_deg = Some(v.f64(26)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 27,
        name: "Offset Corner Longitude Point 1",
        encoding: Encoding::I16Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 2,
            min: -0.075,
            max: 0.075,
        }),
        get: Some(|r| r.corner_lon_offset_p1_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.corner_lon_offset_p1_deg = Some(v.f64(27)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 28,
        name: "Offset Corner Latitude Point 2",
        encoding: Encoding::I16Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 2,
            min: -0.075,
            max: 0.075,
        }),
        get: Some(|r| r.corner_lat_offset_p2_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.corner_lat_offset_p2_deg = Some(v.f64(28)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 29,
        name: "Offset Corner Longitude Point 2",
        encoding: Encoding::I16Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 2,
            min: -0.075,
            max: 0.075,
        }),
        get: Some(|r| r.corner_lon_offset_p2_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.corner_lon_offset_p2_deg = Some(v.f64(29)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 30,
        name: "Offset Corner Latitude Point 3",
        encoding: Encoding::I16Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 2,
            min: -0.075,
            max: 0.075,
        }),
        get: Some(|r| r.corner_lat_offset_p3_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.corner_lat_offset_p3_deg = Some(v.f64(30)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 31,
        name: "Offset Corner Longitude Point 3",
        encoding: Encoding::I16Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 2,
            min: -0.075,
            max: 0.075,
        }),
        get: Some(|r| r.corner_lon_offset_p3_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.corner_lon_offset_p3_deg = Some(v.f64(31)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 32,
        name: "Offset Corner Latitude Point 4",
        encoding: Encoding::I16Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 2,
            min: -0.075,
            max: 0.075,
        }),
        get: Some(|r| r.corner_lat_offset_p4_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.corner_lat_offset_p4_deg = Some(v.f64(32)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 33,
        name: "Offset Corner Longitude Point 4",
        encoding: Encoding::I16Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 2,
            min: -0.075,
            max: 0.075,
        }),
        get: Some(|r| r.corner_lon_offset_p4_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.corner_lon_offset_p4_deg = Some(v.f64(33)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 34,
        name: "Icing Detected",
        encoding: Encoding::U8,
        range: None,
        get: Some(|r| r.icing_detected.map(|e| FieldValue::Code(e.to_wire()))),
        set: Some(|r, v| {
            r.icing_detected = Some(IcingDetected::from_wire(v.code(34)?));
            Ok(())
        }),
    },
    TagSpec {
        id: 35,
        name: "Wind Direction",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: 0.0,
            max: 360.0,
        }),
        get: Some(|r| r.wind_direction_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.wind_direction_deg = Some(v.f64(35)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 36,
        name: "Wind Speed",
        encoding: Encoding::U8Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 1,
            min: 0.0,
            max: 100.0,
        }),
        get: Some(|r| r.wind_speed.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.wind_speed = Some(v.f64(36)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 37,
        name: "Static Pressure",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: 0.0,
            max: 5000.0,
        }),
        get: Some(|r| r.static_pressure_mbar.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.static_pressure_mbar = Some(v.f64(37)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 38,
        name: "Density Altitude",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: -900.0,
            max: 19000.0,
        }),
        get: Some(|r| r.density_altitude_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.density_altitude_m = Some(v.f64(38)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 39,
        name: "Outside Air Temperature",
        encoding: Encoding::I8,
        range: None,
        get: Some(|r| r.outside_air_temp_c.map(|x| FieldValue::I64(i64::from(x)))),
        set: Some(|r, v| {
            r.outside_air_temp_c = Some(v.i8(39)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 40,
        name: "Target Location Latitude",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -90.0,
            max: 90.0,
        }),
        get: Some(|r| r.target_location_lat_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.target_location_lat_deg = Some(v.f64(40)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 41,
        name: "Target Location Longitude",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -180.0,
            max: 180.0,
        }),
        get: Some(|r| r.target_location_lon_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.target_location_lon_deg = Some(v.f64(41)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 42,
        name: "Target Location Elevation",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: -900.0,
            max: 19000.0,
        }),
        get: Some(|r| r.target_location_elev_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.target_location_elev_m = Some(v.f64(42)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 43,
        name: "Target Track Gate Width",
        encoding: Encoding::U8Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 1,
            min: 0.0,
            max: 510.0,
        }),
        get: Some(|r| r.target_track_gate_width_px.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.target_track_gate_width_px = Some(v.f64(43)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 44,
        name: "Target Track Gate Height",
        encoding: Encoding::U8Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 1,
            min: 0.0,
            max: 510.0,
        }),
        get: Some(|r| r.target_track_gate_height_px.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.target_track_gate_height_px = Some(v.f64(44)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 45,
        name: "Target Error Estimate - CE90",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: 0.0,
            max: 4095.0,
        }),
        get: Some(|r| r.target_error_ce90_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.target_error_ce90_m = Some(v.f64(45)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 46,
        name: "Target Error Estimate - LE90",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: 0.0,
            max: 4095.0,
        }),
        get: Some(|r| r.target_error_le90_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.target_error_le90_m = Some(v.f64(46)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 47,
        name: "Generic Flag Data",
        encoding: Encoding::U8,
        range: None,
        get: Some(|r| r.generic_flag_data.map(|x| FieldValue::U64(u64::from(x)))),
        set: Some(|r, v| {
            r.generic_flag_data = Some(v.u8(47)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 48,
        name: "Security Local Set",
        encoding: Encoding::RawBytes,
        range: None,
        get: Some(|r| r.security_local_set.clone().map(FieldValue::Bytes)),
        set: Some(|r, v| {
            r.security_local_set = Some(v.bytes(48)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 49,
        name: "Differential Pressure",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: 0.0,
            max: 5000.0,
        }),
        get: Some(|r| r.differential_pressure_mbar.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.differential_pressure_mbar = Some(v.f64(49)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 50,
        name: "Platform Angle of Attack",
        encoding: Encoding::I16Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 2,
            min: -20.0,
            max: 20.0,
        }),
        get: Some(|r| r.platform_angle_of_attack_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.platform_angle_of_attack_deg = Some(v.f64(50)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 51,
        name: "Platform Vertical Speed",
        encoding: Encoding::I16Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 2,
            min: -180.0,
            max: 180.0,
        }),
        get: Some(|r| r.platform_vertical_speed.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.platform_vertical_speed = Some(v.f64(51)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 52,
        name: "Platform Sideslip Angle",
        encoding: Encoding::I16Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 2,
            min: -20.0,
            max: 20.0,
        }),
        get: Some(|r| r.platform_sideslip_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.platform_sideslip_deg = Some(v.f64(52)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 53,
        name: "Airfield Barometric Pressure",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: 0.0,
            max: 5000.0,
        }),
        get: Some(|r| r.airfield_barometric_pressure_mbar.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.airfield_barometric_pressure_mbar = Some(v.f64(53)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 54,
        name: "Airfield Elevation",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: -900.0,
            max: 19000.0,
        }),
        get: Some(|r| r.airfield_elevation_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.airfield_elevation_m = Some(v.f64(54)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 55,
        name: "Relative Humidity",
        encoding: Encoding::U8Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 1,
            min: 0.0,
            max: 100.0,
        }),
        get: Some(|r| r.relative_humidity_pct.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.relative_humidity_pct = Some(v.f64(55)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 56,
        name: "Platform Ground Speed",
        encoding: Encoding::U8Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 1,
            min: 0.0,
            max: 255.0,
        }),
        get: Some(|r| r.platform_ground_speed.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.platform_ground_speed = Some(v.f64(56)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 57,
        name: "Ground Range",
        encoding: Encoding::U32Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 4,
            min: 0.0,
            max: 5_000_000.0,
        }),
        get: Some(|r| r.ground_range_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.ground_range_m = Some(v.f64(57)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 58,
        name: "Platform Fuel Remaining",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: 0.0,
            max: 10_000.0,
        }),
        get: Some(|r| r.platform_fuel_remaining_kg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.platform_fuel_remaining_kg = Some(v.f64(58)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 59,
        name: "Platform Call Sign",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
        get: Some(|r| r.platform_call_sign.clone().map(FieldValue::Str)),
        set: Some(|r, v| {
            r.platform_call_sign = Some(v.string(59)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 60,
        name: "Weapon Load",
        encoding: Encoding::U16,
        range: None,
        get: Some(|r| r.weapon_load.map(|x| FieldValue::U64(u64::from(x)))),
        set: Some(|r, v| {
            r.weapon_load = Some(v.u16(60)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 61,
        name: "Weapon Fired",
        encoding: Encoding::U8,
        range: None,
        get: Some(|r| r.weapon_fired.map(|x| FieldValue::U64(u64::from(x)))),
        set: Some(|r, v| {
            r.weapon_fired = Some(v.u8(61)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 62,
        name: "Laser PRF Code",
        encoding: Encoding::U16,
        range: None,
        get: Some(|r| r.laser_prf_code.map(|x| FieldValue::U64(u64::from(x)))),
        set: Some(|r, v| {
            r.laser_prf_code = Some(v.u16(62)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 63,
        name: "Sensor Field of View Name",
        encoding: Encoding::U8,
        range: None,
        get: Some(|r| r.sensor_fov_name.map(|e| FieldValue::Code(e.to_wire()))),
        set: Some(|r, v| {
            r.sensor_fov_name = Some(SensorFovName::from_wire(v.code(63)?));
            Ok(())
        }),
    },
    TagSpec {
        id: 64,
        name: "Platform Magnetic Heading",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: 0.0,
            max: 360.0,
        }),
        get: Some(|r| r.platform_magnetic_heading_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.platform_magnetic_heading_deg = Some(v.f64(64)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 65,
        name: "UAS LS Version Number",
        encoding: Encoding::U8,
        range: None,
        get: Some(|r| r.uas_ls_version.map(|x| FieldValue::U64(u64::from(x)))),
        set: Some(|r, v| {
            r.uas_ls_version = Some(v.u8(65)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 67,
        name: "Alternate Platform Latitude",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -90.0,
            max: 90.0,
        }),
        get: Some(|r| r.alternate_platform_lat_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.alternate_platform_lat_deg = Some(v.f64(67)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 68,
        name: "Alternate Platform Longitude",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -180.0,
            max: 180.0,
        }),
        get: Some(|r| r.alternate_platform_lon_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.alternate_platform_lon_deg = Some(v.f64(68)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 69,
        name: "Alternate Platform Altitude",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: -900.0,
            max: 19000.0,
        }),
        get: Some(|r| r.alternate_platform_alt_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.alternate_platform_alt_m = Some(v.f64(69)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 70,
        name: "Alternate Platform Name",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
        get: Some(|r| r.alternate_platform_name.clone().map(FieldValue::Str)),
        set: Some(|r, v| {
            r.alternate_platform_name = Some(v.string(70)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 71,
        name: "Alternate Platform Heading",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: 0.0,
            max: 360.0,
        }),
        get: Some(|r| r.alternate_platform_heading_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.alternate_platform_heading_deg = Some(v.f64(71)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 72,
        name: "Event Start Time",
        encoding: Encoding::U64,
        range: None,
        get: Some(|r| r.event_start_time_us.map(FieldValue::U64)),
        set: Some(|r, v| {
            r.event_start_time_us = Some(v.u64(72)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 73,
        name: "RVT Local Set",
        encoding: Encoding::RawBytes,
        range: None,
        get: Some(|r| r.rvt.clone().map(FieldValue::Bytes)),
        set: Some(|r, v| {
            r.rvt = Some(v.bytes(73)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 74,
        name: "VMTI Local Set",
        encoding: Encoding::RawBytes,
        range: None,
        get: Some(|r| r.vmti.clone().map(FieldValue::Bytes)),
        set: Some(|r, v| {
            r.vmti = Some(v.bytes(74)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 75,
        name: "Sensor Ellipsoid Height",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: -900.0,
            max: 19000.0,
        }),
        get: Some(|r| r.sensor_ellipsoid_height_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.sensor_ellipsoid_height_m = Some(v.f64(75)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 76,
        name: "Alternate Platform Ellipsoid Height",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: -900.0,
            max: 19000.0,
        }),
        get: Some(|r| r.alternate_platform_ellipsoid_height_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.alternate_platform_ellipsoid_height_m = Some(v.f64(76)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 77,
        name: "Operational Mode",
        encoding: Encoding::U8,
        range: None,
        get: Some(|r| r.operational_mode.map(|e| FieldValue::Code(e.to_wire()))),
        set: Some(|r, v| {
            r.operational_mode = Some(OperationalMode::from_wire(v.code(77)?));
            Ok(())
        }),
    },
    TagSpec {
        id: 78,
        name: "Frame Center Height Above Ellipsoid",
        encoding: Encoding::U16Range,
        range: Some(LinearRange {
            signed: false,
            byte_length: 2,
            min: -900.0,
            max: 19000.0,
        }),
        get: Some(|r| r.frame_center_ellipsoid_height_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.frame_center_ellipsoid_height_m = Some(v.f64(78)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 79,
        name: "Sensor North Velocity",
        encoding: Encoding::I16Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 2,
            min: -327.0,
            max: 327.0,
        }),
        get: Some(|r| r.sensor_north_velocity.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.sensor_north_velocity = Some(v.f64(79)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 80,
        name: "Sensor East Velocity",
        encoding: Encoding::I16Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 2,
            min: -327.0,
            max: 327.0,
        }),
        get: Some(|r| r.sensor_east_velocity.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.sensor_east_velocity = Some(v.f64(80)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 81,
        name: "Image Horizon Pixels",
        encoding: Encoding::Pack,
        range: None,
        get: None,
        set: None,
    },
    // Image corners — full lat/lon (tags 82-89, added in ST 0601.13)
    TagSpec {
        id: 82,
        name: "Corner Latitude Point 1 (Full)",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -90.0,
            max: 90.0,
        }),
        get: Some(|r| r.corner_lat_p1_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.corner_lat_p1_deg = Some(v.f64(82)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 83,
        name: "Corner Longitude Point 1 (Full)",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -180.0,
            max: 180.0,
        }),
        get: Some(|r| r.corner_lon_p1_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.corner_lon_p1_deg = Some(v.f64(83)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 84,
        name: "Corner Latitude Point 2 (Full)",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -90.0,
            max: 90.0,
        }),
        get: Some(|r| r.corner_lat_p2_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.corner_lat_p2_deg = Some(v.f64(84)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 85,
        name: "Corner Longitude Point 2 (Full)",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -180.0,
            max: 180.0,
        }),
        get: Some(|r| r.corner_lon_p2_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.corner_lon_p2_deg = Some(v.f64(85)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 86,
        name: "Corner Latitude Point 3 (Full)",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -90.0,
            max: 90.0,
        }),
        get: Some(|r| r.corner_lat_p3_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.corner_lat_p3_deg = Some(v.f64(86)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 87,
        name: "Corner Longitude Point 3 (Full)",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -180.0,
            max: 180.0,
        }),
        get: Some(|r| r.corner_lon_p3_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.corner_lon_p3_deg = Some(v.f64(87)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 88,
        name: "Corner Latitude Point 4 (Full)",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -90.0,
            max: 90.0,
        }),
        get: Some(|r| r.corner_lat_p4_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.corner_lat_p4_deg = Some(v.f64(88)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 89,
        name: "Corner Longitude Point 4 (Full)",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -180.0,
            max: 180.0,
        }),
        get: Some(|r| r.corner_lon_p4_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.corner_lon_p4_deg = Some(v.f64(89)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 90,
        name: "Platform Pitch Angle (Full)",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -90.0,
            max: 90.0,
        }),
        get: Some(|r| r.platform_pitch_full_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.platform_pitch_full_deg = Some(v.f64(90)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 91,
        name: "Platform Roll Angle (Full)",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -90.0,
            max: 90.0,
        }),
        get: Some(|r| r.platform_roll_full_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.platform_roll_full_deg = Some(v.f64(91)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 92,
        name: "Platform Angle of Attack (Full)",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -90.0,
            max: 90.0,
        }),
        get: Some(|r| r.platform_angle_of_attack_full_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.platform_angle_of_attack_full_deg = Some(v.f64(92)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 93,
        name: "Platform Sideslip Angle (Full)",
        encoding: Encoding::I32Range,
        range: Some(LinearRange {
            signed: true,
            byte_length: 4,
            min: -180.0,
            max: 180.0,
        }),
        get: Some(|r| r.platform_sideslip_full_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.platform_sideslip_full_deg = Some(v.f64(93)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 94,
        name: "MIIS Core Identifier",
        encoding: Encoding::RawBytes,
        range: None,
        get: Some(|r| r.miis_core_id.clone().map(FieldValue::Bytes)),
        set: Some(|r, v| {
            r.miis_core_id = Some(v.bytes(94)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 95,
        name: "SAR Motion Imagery Local Set",
        encoding: Encoding::RawBytes,
        range: None,
        get: Some(|r| r.sar_mi_local_set.clone().map(FieldValue::Bytes)),
        set: Some(|r, v| {
            r.sar_mi_local_set = Some(v.bytes(95)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 96,
        name: "Target Width Extended",
        encoding: Encoding::Imapb {
            min: 0.0,
            max: 1_500_000.0,
            max_len: 8,
            default_len: 3,
        },
        range: None,
        get: Some(|r| r.target_width_extended_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.target_width_extended_m = Some(v.f64(96)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 97,
        name: "Range Image Local Set",
        encoding: Encoding::RawBytes,
        range: None,
        get: Some(|r| r.range_image_local_set.clone().map(FieldValue::Bytes)),
        set: Some(|r, v| {
            r.range_image_local_set = Some(v.bytes(97)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 98,
        name: "Geo-Registration Local Set",
        encoding: Encoding::RawBytes,
        range: None,
        get: Some(|r| r.geo_registration_local_set.clone().map(FieldValue::Bytes)),
        set: Some(|r, v| {
            r.geo_registration_local_set = Some(v.bytes(98)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 99,
        name: "Composite Imaging Local Set",
        encoding: Encoding::RawBytes,
        range: None,
        get: Some(|r| r.composite_imaging_local_set.clone().map(FieldValue::Bytes)),
        set: Some(|r, v| {
            r.composite_imaging_local_set = Some(v.bytes(99)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 100,
        name: "Segment Local Set",
        encoding: Encoding::RawBytes,
        range: None,
        get: Some(|r| r.segment_local_set.clone().map(FieldValue::Bytes)),
        set: Some(|r, v| {
            r.segment_local_set = Some(v.bytes(100)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 101,
        name: "Amend Local Set",
        encoding: Encoding::RawBytes,
        range: None,
        get: Some(|r| r.amend_local_set.clone().map(FieldValue::Bytes)),
        set: Some(|r, v| {
            r.amend_local_set = Some(v.bytes(101)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 102,
        name: "SDCC-FLP",
        encoding: Encoding::Pack,
        range: None,
        get: None,
        set: None,
    },
    TagSpec {
        id: 103,
        name: "Density Altitude Extended",
        encoding: Encoding::Imapb {
            min: -900.0,
            max: 40_000.0,
            max_len: 8,
            default_len: 3,
        },
        range: None,
        get: Some(|r| r.density_altitude_extended_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.density_altitude_extended_m = Some(v.f64(103)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 104,
        name: "Sensor Ellipsoid Height Extended",
        encoding: Encoding::Imapb {
            min: -900.0,
            max: 40_000.0,
            max_len: 8,
            default_len: 3,
        },
        range: None,
        get: Some(|r| r.sensor_ellipsoid_height_extended_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.sensor_ellipsoid_height_extended_m = Some(v.f64(104)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 105,
        name: "Alternate Platform Ellipsoid Height Extended",
        encoding: Encoding::Imapb {
            min: -900.0,
            max: 40_000.0,
            max_len: 8,
            default_len: 3,
        },
        range: None,
        get: Some(|r| {
            r.alternate_platform_ellipsoid_height_extended_m
                .map(FieldValue::F64)
        }),
        set: Some(|r, v| {
            r.alternate_platform_ellipsoid_height_extended_m = Some(v.f64(105)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 106,
        name: "Stream Designator",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
        get: Some(|r| r.stream_designator.clone().map(FieldValue::Str)),
        set: Some(|r, v| {
            r.stream_designator = Some(v.string(106)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 107,
        name: "Operational Base",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
        get: Some(|r| r.operational_base.clone().map(FieldValue::Str)),
        set: Some(|r, v| {
            r.operational_base = Some(v.string(107)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 108,
        name: "Broadcast Source",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
        get: Some(|r| r.broadcast_source.clone().map(FieldValue::Str)),
        set: Some(|r, v| {
            r.broadcast_source = Some(v.string(108)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 109,
        name: "Range To Recovery Location",
        encoding: Encoding::Imapb {
            min: 0.0,
            max: 21_000.0,
            max_len: 4,
            default_len: 3,
        },
        range: None,
        get: Some(|r| r.range_to_recovery_km.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.range_to_recovery_km = Some(v.f64(109)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 110,
        name: "Time Airborne",
        encoding: Encoding::VarUint { max_len: 4 },
        range: None,
        get: Some(|r| r.time_airborne_s.map(|x| FieldValue::U64(u64::from(x)))),
        set: Some(|r, v| {
            r.time_airborne_s = Some(v.u32(110)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 111,
        name: "Propulsion Unit Speed",
        encoding: Encoding::VarUint { max_len: 4 },
        range: None,
        get: Some(|r| {
            r.propulsion_unit_speed_rpm
                .map(|x| FieldValue::U64(u64::from(x)))
        }),
        set: Some(|r, v| {
            r.propulsion_unit_speed_rpm = Some(v.u32(111)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 112,
        name: "Platform Course Angle",
        encoding: Encoding::Imapb {
            min: 0.0,
            max: 360.0,
            max_len: 8,
            default_len: 2,
        },
        range: None,
        get: Some(|r| r.platform_course_angle_deg.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.platform_course_angle_deg = Some(v.f64(112)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 113,
        name: "Altitude AGL",
        encoding: Encoding::Imapb {
            min: -900.0,
            max: 40_000.0,
            max_len: 4,
            default_len: 3,
        },
        range: None,
        get: Some(|r| r.altitude_agl_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.altitude_agl_m = Some(v.f64(113)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 114,
        name: "Radar Altimeter",
        encoding: Encoding::Imapb {
            min: -900.0,
            max: 40_000.0,
            max_len: 4,
            default_len: 3,
        },
        range: None,
        get: Some(|r| r.radar_altimeter_m.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.radar_altimeter_m = Some(v.f64(114)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 115,
        name: "Control Command",
        encoding: Encoding::Pack,
        range: None,
        get: None,
        set: None,
    },
    TagSpec {
        id: 116,
        name: "Control Command Verification List",
        encoding: Encoding::Pack,
        range: None,
        get: None,
        set: None,
    },
    TagSpec {
        id: 117,
        name: "Sensor Azimuth Rate",
        encoding: Encoding::Imapb {
            min: -1000.0,
            max: 1000.0,
            max_len: 4,
            default_len: 2,
        },
        range: None,
        get: Some(|r| r.sensor_azimuth_rate_dps.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.sensor_azimuth_rate_dps = Some(v.f64(117)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 118,
        name: "Sensor Elevation Rate",
        encoding: Encoding::Imapb {
            min: -1000.0,
            max: 1000.0,
            max_len: 4,
            default_len: 3,
        },
        range: None,
        get: Some(|r| r.sensor_elevation_rate_dps.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.sensor_elevation_rate_dps = Some(v.f64(118)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 119,
        name: "Sensor Roll Rate",
        encoding: Encoding::Imapb {
            min: -1000.0,
            max: 1000.0,
            max_len: 4,
            default_len: 2,
        },
        range: None,
        get: Some(|r| r.sensor_roll_rate_dps.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.sensor_roll_rate_dps = Some(v.f64(119)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 120,
        name: "On-board MI Storage Percent Full",
        encoding: Encoding::Imapb {
            min: 0.0,
            max: 100.0,
            max_len: 3,
            default_len: 2,
        },
        range: None,
        get: Some(|r| r.mi_storage_percent_full.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.mi_storage_percent_full = Some(v.f64(120)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 121,
        name: "Active Wavelength List",
        encoding: Encoding::Pack,
        range: None,
        get: None,
        set: None,
    },
    TagSpec {
        id: 122,
        name: "Country Codes",
        encoding: Encoding::Pack,
        range: None,
        get: None,
        set: None,
    },
    TagSpec {
        id: 123,
        name: "Number of NAVSATs in View",
        encoding: Encoding::VarUint { max_len: 1 },
        range: None,
        get: Some(|r| r.navsats_in_view.map(|x| FieldValue::U64(u64::from(x)))),
        set: Some(|r, v| {
            r.navsats_in_view = Some(v.u8(123)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 124,
        name: "Positioning Method Source",
        encoding: Encoding::VarUint { max_len: 1 },
        range: None,
        get: Some(|r| {
            r.positioning_method_source
                .map(|x| FieldValue::U64(u64::from(x)))
        }),
        set: Some(|r, v| {
            r.positioning_method_source = Some(v.u8(124)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 125,
        name: "Platform Status",
        encoding: Encoding::VarUint { max_len: 1 },
        range: None,
        get: Some(|r| r.platform_status.map(|e| FieldValue::Code(e.to_wire()))),
        set: Some(|r, v| {
            r.platform_status = Some(PlatformStatus::from_wire(v.code(125)?));
            Ok(())
        }),
    },
    TagSpec {
        id: 126,
        name: "Sensor Control Mode",
        encoding: Encoding::VarUint { max_len: 1 },
        range: None,
        get: Some(|r| r.sensor_control_mode.map(|e| FieldValue::Code(e.to_wire()))),
        set: Some(|r, v| {
            r.sensor_control_mode = Some(SensorControlMode::from_wire(v.code(126)?));
            Ok(())
        }),
    },
    TagSpec {
        id: 127,
        name: "Sensor Frame Rate Pack",
        encoding: Encoding::Pack,
        range: None,
        get: None,
        set: None,
    },
    TagSpec {
        id: 128,
        name: "Wavelengths List",
        encoding: Encoding::Pack,
        range: None,
        get: None,
        set: None,
    },
    TagSpec {
        id: 129,
        name: "Target ID",
        encoding: Encoding::Utf8 { max_bytes: 32 },
        range: None,
        get: Some(|r| r.target_id.clone().map(FieldValue::Str)),
        set: Some(|r, v| {
            r.target_id = Some(v.string(129)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 130,
        name: "Airbase Locations",
        encoding: Encoding::Pack,
        range: None,
        get: None,
        set: None,
    },
    TagSpec {
        id: 131,
        name: "Take-off Time",
        encoding: Encoding::VarUint { max_len: 8 },
        range: None,
        get: Some(|r| r.take_off_time_us.map(FieldValue::U64)),
        set: Some(|r, v| {
            r.take_off_time_us = Some(v.u64(131)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 132,
        name: "Transmission Frequency",
        encoding: Encoding::Imapb {
            min: 1.0,
            max: 99_999.0,
            max_len: 4,
            default_len: 3,
        },
        range: None,
        get: Some(|r| r.transmission_frequency_mhz.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.transmission_frequency_mhz = Some(v.f64(132)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 133,
        name: "On-board MI Storage Capacity",
        encoding: Encoding::VarUint { max_len: 4 },
        range: None,
        get: Some(|r| {
            r.mi_storage_capacity_gb
                .map(|x| FieldValue::U64(u64::from(x)))
        }),
        set: Some(|r, v| {
            r.mi_storage_capacity_gb = Some(v.u32(133)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 134,
        name: "Zoom Percentage",
        encoding: Encoding::Imapb {
            min: 0.0,
            max: 100.0,
            max_len: 4,
            default_len: 2,
        },
        range: None,
        get: Some(|r| r.zoom_percentage.map(FieldValue::F64)),
        set: Some(|r, v| {
            r.zoom_percentage = Some(v.f64(134)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 135,
        name: "Communications Method",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
        get: Some(|r| r.communications_method.clone().map(FieldValue::Str)),
        set: Some(|r, v| {
            r.communications_method = Some(v.string(135)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 136,
        name: "Leap Seconds",
        encoding: Encoding::VarInt { max_len: 4 },
        range: None,
        get: Some(|r| r.leap_seconds.map(|x| FieldValue::I64(i64::from(x)))),
        set: Some(|r, v| {
            r.leap_seconds = Some(v.i32(136)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 137,
        name: "Correction Offset",
        encoding: Encoding::VarInt { max_len: 8 },
        range: None,
        get: Some(|r| r.correction_offset_us.map(FieldValue::I64)),
        set: Some(|r, v| {
            r.correction_offset_us = Some(v.i64(137)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 138,
        name: "Payload List",
        encoding: Encoding::Pack,
        range: None,
        get: None,
        set: None,
    },
    TagSpec {
        id: 139,
        name: "Active Payloads",
        encoding: Encoding::RawBytes,
        range: None,
        get: Some(|r| r.active_payloads.clone().map(FieldValue::Bytes)),
        set: Some(|r, v| {
            r.active_payloads = Some(v.bytes(139)?);
            Ok(())
        }),
    },
    TagSpec {
        id: 140,
        name: "Weapons Stores",
        encoding: Encoding::Pack,
        range: None,
        get: None,
        set: None,
    },
    TagSpec {
        id: 141,
        name: "Waypoint List",
        encoding: Encoding::Pack,
        range: None,
        get: None,
        set: None,
    },
    TagSpec {
        id: 142,
        name: "View Domain",
        encoding: Encoding::Pack,
        range: None,
        get: None,
        set: None,
    },
    TagSpec {
        id: 143,
        name: "Metadata Substream Id",
        encoding: Encoding::Pack,
        range: None,
        get: None,
        set: None,
    },
];

/// Row position plus one for every tag in [`TAGS`], zero for a tag with no
/// row. Built at compile time; the initializer fails the build if `TAGS` is
/// not strictly tag-ascending.
static INDEX: [u8; 256] = build_index();

const fn build_index() -> [u8; 256] {
    let mut index = [0u8; 256];
    let mut i = 0;
    while i < TAGS.len() {
        let id = TAGS[i].id as usize;
        assert!(index[id] == 0, "TAGS lists a tag twice");
        assert!(
            i == 0 || TAGS[i - 1].id < TAGS[i].id,
            "TAGS must be tag-ascending"
        );
        assert!(i < 255, "INDEX holds row positions in a u8");
        index[id] = (i + 1) as u8;
        i += 1;
    }
    index
}

/// Lookup a tag by ID. Returns None for tags we don't typed-model.
pub(crate) fn lookup(id: u8) -> Option<&'static TagSpec> {
    match INDEX[usize::from(id)] {
        0 => None,
        n => Some(&TAGS[usize::from(n) - 1]),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn unique_tag_ids() {
        let mut ids: Vec<u8> = TAGS.iter().map(|t| t.id).collect();
        ids.sort_unstable();
        let mut dedup = ids.clone();
        dedup.dedup();
        assert_eq!(ids, dedup, "duplicate tag IDs in TAGS");
    }

    #[test]
    fn ranged_encodings_have_range() {
        for spec in TAGS {
            match spec.encoding {
                Encoding::U16Range
                | Encoding::I16Range
                | Encoding::U32Range
                | Encoding::I32Range
                | Encoding::U8Range => {
                    if spec.id == 1 {
                        // Tag 1 (Checksum) is U16Range encoding-wise (raw u16) but has no float range.
                        continue;
                    }
                    assert!(
                        spec.range.is_some(),
                        "tag {} ({}) is a ranged encoding but has no range",
                        spec.id,
                        spec.name
                    );
                }
                Encoding::Imapb {
                    min,
                    max,
                    max_len,
                    default_len,
                } => {
                    assert!(
                        spec.range.is_none(),
                        "tag {} ({}) is Imapb but also carries a LinearRange",
                        spec.id,
                        spec.name
                    );
                    assert!(
                        min < max,
                        "tag {} ({}) has non-strict imapb range [{}, {}]",
                        spec.id,
                        spec.name,
                        min,
                        max
                    );
                    assert!(
                        default_len >= 1 && default_len <= max_len && max_len <= 8,
                        "tag {} ({}) has invalid imapb lengths: default_len={} max_len={}",
                        spec.id,
                        spec.name,
                        default_len,
                        max_len
                    );
                }
                Encoding::VarUint { max_len } | Encoding::VarInt { max_len } => {
                    assert!(
                        spec.range.is_none(),
                        "tag {} ({}) is var-length int but also carries a LinearRange",
                        spec.id,
                        spec.name
                    );
                    assert!(
                        (1..=8).contains(&max_len),
                        "tag {} ({}) has invalid var-len max_len={}",
                        spec.id,
                        spec.name,
                        max_len
                    );
                }
                Encoding::Pack => {
                    // Pack/list marker: no length/range metadata of
                    // its own (the per-tag wire shape lives in
                    // `packs.rs`) — pin that it never accidentally
                    // carries a LinearRange.
                    assert!(
                        spec.range.is_none(),
                        "tag {} ({}) is Pack-encoded but also carries a LinearRange",
                        spec.id,
                        spec.name
                    );
                }
                _ => {}
            }
        }
    }

    #[test]
    fn ranges_are_strict() {
        for spec in TAGS {
            if let Some(r) = spec.range {
                assert!(
                    r.min < r.max,
                    "tag {} has non-strict range [{}, {}]",
                    spec.id,
                    r.min,
                    r.max
                );
                assert!(
                    matches!(r.byte_length, 1 | 2 | 4),
                    "tag {} has nonstandard byte_length {}",
                    spec.id,
                    r.byte_length
                );
            }
        }
    }

    #[test]
    fn lookup_finds_known_tag() {
        let t = lookup(13).unwrap();
        assert_eq!(t.name, "Sensor Latitude");
    }

    #[test]
    fn lookup_misses_unknown_tag() {
        assert!(lookup(255).is_none());
    }

    // --- accessor-table completeness ---

    /// Rows with no scalar field: the checksum, the pack rows (including
    /// the multi-instance tags 102 and 115).
    const NO_SCALAR: &[u8] = &[
        1, 81, 102, 115, 116, 121, 122, 127, 128, 130, 138, 140, 141, 142, 143,
    ];

    fn is_ranged(spec: &TagSpec) -> bool {
        spec.id != 1 && (spec.range.is_some() || matches!(spec.encoding, Encoding::Imapb { .. }))
    }

    /// Every ranged-or-IMAPB row (tag 1 excluded: raw U16, not a float
    /// field) has float accessors, and `TAGS` is strictly tag-ascending
    /// with no tag listed twice. `lookup` finds every row and nothing else.
    #[test]
    fn tags_table_ascending_and_ranged_rows_carry_float_accessors() {
        for w in TAGS.windows(2) {
            assert!(w[0].id < w[1].id, "TAGS must be strictly tag-ascending");
        }
        let ranged = TAGS.iter().filter(|s| is_ranged(s)).count();
        assert_eq!(ranged, 83, "69 LinearRange rows plus 14 IMAPB rows");
        let mut r = UasDatalinkLs::default();
        for spec in TAGS.iter().filter(|s| is_ranged(s)) {
            assert_eq!(spec.get_f64(&r), None, "tag {} starts unset", spec.id);
            spec.set_f64(&mut r, 0.0);
            assert_eq!(spec.get_f64(&r), Some(0.0), "tag {} f64 view", spec.id);
        }
        for id in 0..=255u8 {
            let found = lookup(id).map(|s| s.id);
            assert_eq!(found, TAGS.iter().find(|s| s.id == id).map(|s| s.id));
        }
    }

    /// Every row with a scalar field has `get` and `set`, `set` then `get`
    /// returns what was set, and a value of the wrong variant is refused.
    /// Rows without a scalar field have neither accessor.
    #[test]
    fn every_tag_row_accessor_round_trips_a_sample() {
        for spec in TAGS {
            if NO_SCALAR.contains(&spec.id) {
                assert!(
                    spec.get.is_none() && spec.set.is_none(),
                    "tag {} must have no accessor",
                    spec.id
                );
                continue;
            }
            let (get, set) = (spec.get.expect("get"), spec.set.expect("set"));
            let sample = sample_value(spec);
            let mut r = UasDatalinkLs::default();
            assert_eq!(get(&r), None, "tag {} starts unset", spec.id);
            set(&mut r, sample.clone()).unwrap_or_else(|e| panic!("tag {} set: {e:?}", spec.id));
            assert_eq!(get(&r), Some(sample), "tag {} get after set", spec.id);

            // A value of a variant no row of this encoding produces names the
            // row's own tag.
            let wrong = match spec.encoding {
                Encoding::Utf8 { .. } | Encoding::RawBytes => FieldValue::F64(0.0),
                _ => FieldValue::Str("a".into()),
            };
            let err = set(&mut r, wrong).expect_err("wrong variant");
            assert!(
                matches!(err, KlvFieldError::InvalidLength { tag, .. } if tag == u32::from(spec.id)),
                "tag {}: {err:?}",
                spec.id
            );
        }
    }

    /// A value wider than the field's Rust type is refused with the tag.
    #[test]
    fn integer_accessors_refuse_a_value_wider_than_the_field() {
        let mut r = UasDatalinkLs::default();
        for (tag, wide) in [(60u8, 0x1_0000u64), (123, 0x100), (110, 0x1_0000_0000)] {
            let set = lookup(tag).and_then(|s| s.set).expect("set");
            assert!(matches!(
                set(&mut r, FieldValue::U64(wide)),
                Err(KlvFieldError::InvalidLength { tag: t, .. }) if t == u32::from(tag)
            ));
        }
        let set = lookup(136).and_then(|s| s.set).expect("set");
        assert!(set(&mut r, FieldValue::I64(i64::from(i32::MAX) + 1)).is_err());
    }

    /// A per-encoding sample inside every row's range: midpoints for ranged
    /// rows, 1 for integers, "a" for strings, [0x47] for bytes, code 1 for
    /// the coded enums.
    pub(crate) fn sample_value(spec: &TagSpec) -> FieldValue {
        const CODED: &[u8] = &[34, 63, 77, 125, 126];
        if CODED.contains(&spec.id) {
            return FieldValue::Code(1);
        }
        match spec.encoding {
            Encoding::Imapb { min, max, .. } => FieldValue::F64((min + max) / 2.0),
            Encoding::U8Range
            | Encoding::U16Range
            | Encoding::I16Range
            | Encoding::U32Range
            | Encoding::I32Range => {
                let r = spec.range.expect("ranged row has a range");
                FieldValue::F64((r.min + r.max) / 2.0)
            }
            Encoding::U8 | Encoding::U16 | Encoding::U64 | Encoding::VarUint { .. } => {
                FieldValue::U64(1)
            }
            Encoding::I8 | Encoding::VarInt { .. } => FieldValue::I64(-1),
            Encoding::Utf8 { .. } => FieldValue::Str("a".into()),
            Encoding::RawBytes => FieldValue::Bytes(alloc::vec![0x47]),
            Encoding::Pack => unreachable!("packs are excluded above"),
        }
    }
}
