//! ST 0601 tag schema as data. The encoder and decoder loop this table.
//!
//! Entries are pinned against MISB ST 0601.19. Adding or modifying a tag
//! is a one-entry change; do not duplicate the dispatch logic elsewhere.

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TagSpec {
    pub id: u8,
    pub name: &'static str,
    pub encoding: Encoding,
    pub range: Option<LinearRange>,
}

pub(crate) const TAGS: &[TagSpec] = &[
    TagSpec {
        id: 1,
        name: "Checksum",
        encoding: Encoding::U16Range,
        range: None,
    },
    TagSpec {
        id: 2,
        name: "Precision Time Stamp",
        encoding: Encoding::U64,
        range: None,
    },
    TagSpec {
        id: 3,
        name: "Mission ID",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
    },
    TagSpec {
        id: 4,
        name: "Platform Tail Number",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
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
    },
    TagSpec {
        id: 10,
        name: "Platform Designation",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
    },
    TagSpec {
        id: 11,
        name: "Image Source Sensor",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
    },
    TagSpec {
        id: 12,
        name: "Image Coordinate System",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
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
    },
    TagSpec {
        id: 34,
        name: "Icing Detected",
        encoding: Encoding::U8,
        range: None,
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
    },
    TagSpec {
        id: 39,
        name: "Outside Air Temperature",
        encoding: Encoding::I8,
        range: None,
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
    },
    TagSpec {
        id: 47,
        name: "Generic Flag Data",
        encoding: Encoding::U8,
        range: None,
    },
    TagSpec {
        id: 48,
        name: "Security Local Set",
        encoding: Encoding::RawBytes,
        range: None,
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
    },
    TagSpec {
        id: 59,
        name: "Platform Call Sign",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
    },
    TagSpec {
        id: 60,
        name: "Weapon Load",
        encoding: Encoding::U16,
        range: None,
    },
    TagSpec {
        id: 61,
        name: "Weapon Fired",
        encoding: Encoding::U8,
        range: None,
    },
    TagSpec {
        id: 62,
        name: "Laser PRF Code",
        encoding: Encoding::U16,
        range: None,
    },
    TagSpec {
        id: 63,
        name: "Sensor Field of View Name",
        encoding: Encoding::U8,
        range: None,
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
    },
    TagSpec {
        id: 65,
        name: "UAS LS Version Number",
        encoding: Encoding::U8,
        range: None,
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
    },
    TagSpec {
        id: 70,
        name: "Alternate Platform Name",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
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
    },
    TagSpec {
        id: 72,
        name: "Event Start Time",
        encoding: Encoding::U64,
        range: None,
    },
    TagSpec {
        id: 73,
        name: "RVT Local Set",
        encoding: Encoding::RawBytes,
        range: None,
    },
    TagSpec {
        id: 74,
        name: "VMTI Local Set",
        encoding: Encoding::RawBytes,
        range: None,
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
    },
    TagSpec {
        id: 77,
        name: "Operational Mode",
        encoding: Encoding::U8,
        range: None,
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
    },
    TagSpec {
        id: 81,
        name: "Image Horizon Pixels",
        encoding: Encoding::Pack,
        range: None,
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
    },
    TagSpec {
        id: 94,
        name: "MIIS Core Identifier",
        encoding: Encoding::RawBytes,
        range: None,
    },
    TagSpec {
        id: 95,
        name: "SAR Motion Imagery Local Set",
        encoding: Encoding::RawBytes,
        range: None,
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
    },
    TagSpec {
        id: 97,
        name: "Range Image Local Set",
        encoding: Encoding::RawBytes,
        range: None,
    },
    TagSpec {
        id: 98,
        name: "Geo-Registration Local Set",
        encoding: Encoding::RawBytes,
        range: None,
    },
    TagSpec {
        id: 99,
        name: "Composite Imaging Local Set",
        encoding: Encoding::RawBytes,
        range: None,
    },
    TagSpec {
        id: 100,
        name: "Segment Local Set",
        encoding: Encoding::RawBytes,
        range: None,
    },
    TagSpec {
        id: 101,
        name: "Amend Local Set",
        encoding: Encoding::RawBytes,
        range: None,
    },
    TagSpec {
        id: 102,
        name: "SDCC-FLP",
        encoding: Encoding::Pack,
        range: None,
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
    },
    TagSpec {
        id: 106,
        name: "Stream Designator",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
    },
    TagSpec {
        id: 107,
        name: "Operational Base",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
    },
    TagSpec {
        id: 108,
        name: "Broadcast Source",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
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
    },
    TagSpec {
        id: 110,
        name: "Time Airborne",
        encoding: Encoding::VarUint { max_len: 4 },
        range: None,
    },
    TagSpec {
        id: 111,
        name: "Propulsion Unit Speed",
        encoding: Encoding::VarUint { max_len: 4 },
        range: None,
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
    },
    TagSpec {
        id: 115,
        name: "Control Command",
        encoding: Encoding::Pack,
        range: None,
    },
    TagSpec {
        id: 116,
        name: "Control Command Verification List",
        encoding: Encoding::Pack,
        range: None,
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
    },
    TagSpec {
        id: 121,
        name: "Active Wavelength List",
        encoding: Encoding::Pack,
        range: None,
    },
    TagSpec {
        id: 122,
        name: "Country Codes",
        encoding: Encoding::Pack,
        range: None,
    },
    TagSpec {
        id: 123,
        name: "Number of NAVSATs in View",
        encoding: Encoding::VarUint { max_len: 1 },
        range: None,
    },
    TagSpec {
        id: 124,
        name: "Positioning Method Source",
        encoding: Encoding::VarUint { max_len: 1 },
        range: None,
    },
    TagSpec {
        id: 125,
        name: "Platform Status",
        encoding: Encoding::VarUint { max_len: 1 },
        range: None,
    },
    TagSpec {
        id: 126,
        name: "Sensor Control Mode",
        encoding: Encoding::VarUint { max_len: 1 },
        range: None,
    },
    TagSpec {
        id: 127,
        name: "Sensor Frame Rate Pack",
        encoding: Encoding::Pack,
        range: None,
    },
    TagSpec {
        id: 128,
        name: "Wavelengths List",
        encoding: Encoding::Pack,
        range: None,
    },
    TagSpec {
        id: 129,
        name: "Target ID",
        encoding: Encoding::Utf8 { max_bytes: 32 },
        range: None,
    },
    TagSpec {
        id: 130,
        name: "Airbase Locations",
        encoding: Encoding::Pack,
        range: None,
    },
    TagSpec {
        id: 131,
        name: "Take-off Time",
        encoding: Encoding::VarUint { max_len: 8 },
        range: None,
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
    },
    TagSpec {
        id: 133,
        name: "On-board MI Storage Capacity",
        encoding: Encoding::VarUint { max_len: 4 },
        range: None,
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
    },
    TagSpec {
        id: 135,
        name: "Communications Method",
        encoding: Encoding::Utf8 { max_bytes: 127 },
        range: None,
    },
    TagSpec {
        id: 136,
        name: "Leap Seconds",
        encoding: Encoding::VarInt { max_len: 4 },
        range: None,
    },
    TagSpec {
        id: 137,
        name: "Correction Offset",
        encoding: Encoding::VarInt { max_len: 8 },
        range: None,
    },
    TagSpec {
        id: 138,
        name: "Payload List",
        encoding: Encoding::Pack,
        range: None,
    },
    TagSpec {
        id: 139,
        name: "Active Payloads",
        encoding: Encoding::RawBytes,
        range: None,
    },
    TagSpec {
        id: 140,
        name: "Weapons Stores",
        encoding: Encoding::Pack,
        range: None,
    },
    TagSpec {
        id: 141,
        name: "Waypoint List",
        encoding: Encoding::Pack,
        range: None,
    },
    TagSpec {
        id: 142,
        name: "View Domain",
        encoding: Encoding::Pack,
        range: None,
    },
    TagSpec {
        id: 143,
        name: "Metadata Substream Id",
        encoding: Encoding::Pack,
        range: None,
    },
];

/// Lookup a tag by ID. Returns None for tags we don't typed-model.
pub(crate) fn lookup(id: u8) -> Option<&'static TagSpec> {
    TAGS.iter().find(|t| t.id == id)
}

#[cfg(test)]
mod tests {
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

    // --- KLV-1 accessor-table completeness ---

    /// Every TAGS entry with `range: Some(...)` (excluding Tag 1, which is
    /// raw U16 not a float field) OR `encoding: Encoding::Imapb { .. }`
    /// must appear in RANGED_FIELDS exactly once. RANGED_FIELDS must not
    /// contain entries that don't have a corresponding ranged/Imapb TAGS
    /// entry. Imapb tags share the same `Option<f64>` accessor table as
    /// the LinearRange tags even though `spec.range` stays `None` for
    /// them — the ENCODING dispatch (not `spec.range`) distinguishes
    /// linear vs IMAPB wire formats.
    #[test]
    fn ranged_fields_table_complete_and_injective() {
        let ranged_table = crate::klv::st0601::decode::RANGED_FIELDS;

        // Strictly ascending tag order — the precondition for the
        // binary_search_by_key lookup in `decode::ranged_entry`.
        for w in ranged_table.windows(2) {
            assert!(
                w[0].id < w[1].id,
                "RANGED_FIELDS must be strictly tag-ascending (binary-search \
                 precondition): {} then {}",
                w[0].id,
                w[1].id
            );
        }

        // Each ranged-or-Imapb TAGS entry must appear exactly once in the table.
        let mut ranged_tag_count = 0usize;
        for spec in TAGS {
            if spec.id == 1 {
                continue; // Checksum: U16Range encoding but no float range
            }
            let is_ranged = spec.range.is_some() || matches!(spec.encoding, Encoding::Imapb { .. });
            if !is_ranged {
                continue;
            }
            ranged_tag_count += 1;
            let count = ranged_table.iter().filter(|e| e.id == spec.id).count();
            assert_eq!(
                count, 1,
                "TAGS entry id={} ({}) appears {} times in RANGED_FIELDS (expected 1)",
                spec.id, spec.name, count
            );
        }

        // RANGED_FIELDS must not contain entries not in TAGS with range.
        assert_eq!(
            ranged_table.len(),
            ranged_tag_count,
            "RANGED_FIELDS has {} entries but {} TAGS have range \
             (table has extra or missing entries)",
            ranged_table.len(),
            ranged_tag_count
        );

        // IDs in RANGED_FIELDS must be unique.
        let mut ids: Vec<u8> = ranged_table.iter().map(|e| e.id).collect();
        ids.sort_unstable();
        let mut dedup = ids.clone();
        dedup.dedup();
        assert_eq!(ids, dedup, "duplicate IDs in RANGED_FIELDS");
    }
}
