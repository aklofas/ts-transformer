//! ST 0806.4 typed model — the RVT (Remote Video Terminal) Local Set plus
//! its two repeatable nested sets (`RvtPoi`, `RvtAoi`) and the User Defined
//! LS (`RvtUserData`). RVT LS is standalone-capable (own Universal Label,
//! [`RVT_LS_UL`]) and also embeddable in ST 0601 Tag 73 (body form only,
//! no UL — see [`crate::klv::st0601`]).

use crate::error::KlvFieldError;
use crate::klv::pack::OwnedRawField;
use crate::klv::universal_label::UniversalLabel;
use alloc::string::String;
use alloc::vec::Vec;

/// RVT Local Set Universal Label — ST 0806.4-06 (§7.1).
pub const RVT_LS_UL: UniversalLabel = UniversalLabel([
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x02, 0x00, 0x00, 0x00,
]);
/// POI Local Set Universal Label — ST 0806.4-07.
pub const RVT_POI_LS_UL: UniversalLabel = UniversalLabel([
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x0C, 0x00, 0x00, 0x00,
]);
/// AOI Local Set Universal Label — ST 0806.4-12.
pub const RVT_AOI_LS_UL: UniversalLabel = UniversalLabel([
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x0D, 0x00, 0x00, 0x00,
]);
/// User Defined Local Set Universal Label — ST 0806.4-20.
pub const RVT_USER_DEFINED_LS_UL: UniversalLabel = UniversalLabel([
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x0F, 0x00, 0x00, 0x00,
]);

/// POI/AOI Type code — POI variant (ST 0806.4 Table 8-2 Tag 5: value 3 = Target).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RvtPoiType {
    Friendly,
    Hostile,
    Target,
    Unknown,
    Other(u8),
}

/// POI/AOI Type code — AOI variant (Table 8-3 Tag 6: value 3 = Reserved).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RvtAoiType {
    Friendly,
    Hostile,
    Reserved,
    Unknown,
    Other(u8),
}

/// User Defined LS Tag 1 top-2-bit data-type code (Table 8-4).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RvtUserDataType {
    Strings,
    Int,
    Uint,
    Experimental,
}

impl RvtPoiType {
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
        Self::Friendly,
        Self::Hostile,
        Self::Target,
        Self::Unknown,
        Self::Other(0xFF),
    ];

    /// ST 0806.4 Table 8-2 Tag 5 wire codepoint → variant (`Other(v)` for
    /// unknown codes).
    pub fn from_wire(v: u8) -> Self {
        match v {
            1 => Self::Friendly,
            2 => Self::Hostile,
            3 => Self::Target,
            4 => Self::Unknown,
            o => Self::Other(o),
        }
    }
    /// Variant → ST 0806.4 Table 8-2 Tag 5 wire codepoint (inverse of
    /// [`Self::from_wire`]).
    pub fn to_wire(self) -> u8 {
        match self {
            Self::Friendly => 1,
            Self::Hostile => 2,
            Self::Target => 3,
            Self::Unknown => 4,
            Self::Other(o) => o,
        }
    }
}
impl RvtAoiType {
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
        Self::Friendly,
        Self::Hostile,
        Self::Reserved,
        Self::Unknown,
        Self::Other(0xFF),
    ];

    /// ST 0806.4 Table 8-3 Tag 6 wire codepoint → variant (`Other(v)` for
    /// unknown codes).
    pub fn from_wire(v: u8) -> Self {
        match v {
            1 => Self::Friendly,
            2 => Self::Hostile,
            3 => Self::Reserved,
            4 => Self::Unknown,
            o => Self::Other(o),
        }
    }
    /// Variant → ST 0806.4 Table 8-3 Tag 6 wire codepoint (inverse of
    /// [`Self::from_wire`]).
    pub fn to_wire(self) -> u8 {
        match self {
            Self::Friendly => 1,
            Self::Hostile => 2,
            Self::Reserved => 3,
            Self::Unknown => 4,
            Self::Other(o) => o,
        }
    }
}

/// Point of Interest Local Set (ST 0806.4 Table 8-2), carried in RVT Tag 12.
#[must_use]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RvtPoi {
    /// Tag 1: POI Number — mandatory on encode; unique identifier (uint16).
    pub number: Option<u16>,
    /// Tag 2: POI Latitude — encode range [-90, 90] deg (int32); `0x80000000`
    /// is the "error" sentinel (see [`Self::sentinel_tags`]).
    pub lat_deg: Option<f64>,
    /// Tag 3: POI Longitude — encode range [-180, 180] deg (int32); same
    /// sentinel handling as [`Self::lat_deg`].
    pub lon_deg: Option<f64>,
    /// Tag 4: POI Altitude — encode range [-900, 19000] m MSL (uint16).
    pub alt_m: Option<f64>,
    /// Tag 5: POI Type (uint8) — see [`RvtPoiType`].
    pub poi_type: Option<RvtPoiType>,
    /// Tag 6: POI Text Description — ISO 7 string, max 2048 bytes.
    pub text: Option<String>,
    /// Tag 7: POI Source Icon — ISO 7 string, max 127 bytes (MIL-STD-2525B).
    pub source_icon: Option<String>,
    /// Tag 8: POI Source ID — ISO 7 string, max 255 bytes.
    pub source_id: Option<String>,
    /// Tag 9: POI Label — ISO 7 string, max 16 bytes. The spec text for this
    /// item gives the bare number "16" rather than the "Max. N" phrasing
    /// used by sibling items; treated here as a ≤16-byte cap like the
    /// others, but a fixed-width-padded reading remains possible pending a
    /// real-capture check (spec ambiguity, not yet resolved).
    pub label: Option<String>,
    /// Tag 10: POI Operation ID — ISO 7 string, max 127 bytes.
    pub operation_id: Option<String>,
    /// Tags whose lat/lon carried the `0x80000000` "error" sentinel on
    /// decode (see [`Self::lat_deg`] / [`Self::lon_deg`]).
    pub sentinel_tags: Vec<u32>,
    /// Tags not modeled above, passed through byte-for-byte.
    pub unknown: Vec<OwnedRawField>,
    /// Per-field decode errors collected instead of aborting the whole set.
    pub field_errors: Vec<KlvFieldError>,
}

/// Area of Interest Local Set (Table 8-3), carried in RVT Tag 13.
/// Corners: Point 1 = NW (upper-left), Point 3 = SE (lower-right).
#[must_use]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RvtAoi {
    /// Tag 1: AOI Number — mandatory on encode; unique identifier (uint16).
    pub number: Option<u16>,
    /// Tag 2: AOI Point 1 (NW) Latitude — encode range [-90, 90] deg (int32).
    pub corner_lat_p1_deg: Option<f64>,
    /// Tag 3: AOI Point 1 (NW) Longitude — encode range [-180, 180] deg (int32).
    pub corner_lon_p1_deg: Option<f64>,
    /// Tag 4: AOI Point 3 (SE) Latitude — encode range [-90, 90] deg (int32).
    pub corner_lat_p3_deg: Option<f64>,
    /// Tag 5: AOI Point 3 (SE) Longitude — encode range [-180, 180] deg (int32).
    pub corner_lon_p3_deg: Option<f64>,
    /// Tag 6: AOI Type (uint8) — see [`RvtAoiType`].
    pub aoi_type: Option<RvtAoiType>,
    /// Tag 7: AOI Text Description — ISO 7 string, max 2048 bytes.
    pub text: Option<String>,
    /// Tag 8: AOI Source ID — ISO 7 string, max 255 bytes.
    pub source_id: Option<String>,
    /// Tag 9: AOI Label — ISO 7 string, max 16 bytes. Same bare-"16"
    /// spec-ambiguity caveat as [`RvtPoi::label`] applies here.
    pub label: Option<String>,
    /// Tag 10: AOI Operation ID — ISO 7 string, max 127 bytes.
    pub operation_id: Option<String>,
    /// Tags whose lat/lon carried the `0x80000000` "error" sentinel on
    /// decode (see the corner lat/lon fields above).
    pub sentinel_tags: Vec<u32>,
    /// Tags not modeled above, passed through byte-for-byte.
    pub unknown: Vec<OwnedRawField>,
    /// Per-field decode errors collected instead of aborting the whole set.
    pub field_errors: Vec<KlvFieldError>,
}

/// User Defined Local Set (Table 8-4), carried in RVT Tag 11. Exactly two
/// items, fixed order (-21..-24): the numeric-id byte, then the payload.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RvtUserData {
    /// Tag 1 raw byte: bits 8-7 = data type, bits 6-1 = numeric id 0-63.
    pub numeric_id_raw: u8,
    /// Tag 2 payload; concrete width is conveyed by its length.
    pub data: Vec<u8>,
}

impl RvtUserData {
    #[must_use]
    pub fn data_type(&self) -> RvtUserDataType {
        match self.numeric_id_raw >> 6 {
            0b00 => RvtUserDataType::Strings,
            0b01 => RvtUserDataType::Int,
            0b10 => RvtUserDataType::Uint,
            _ => RvtUserDataType::Experimental,
        }
    }
    #[must_use]
    pub fn numeric_id(&self) -> u8 {
        self.numeric_id_raw & 0x3F
    }
}

/// Remote Video Terminal Local Set (ST 0806.4 Table 8-1).
#[must_use]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RvtLs {
    /// Tag 1: CRC-32/MPEG-2 checksum (uint32) — verified only by
    /// `decode_standalone` (embedded RVT need not carry it).
    pub crc32: Option<u32>,
    /// Tag 2: User Defined Time Stamp — microseconds (uint64).
    pub timestamp_us: Option<u64>,
    /// Tag 3: Platform True Airspeed — m/s, 1:1 counts (uint16).
    pub platform_true_airspeed: Option<u16>,
    /// Tag 4: Platform Indicated Airspeed — m/s, 1:1 counts (uint16).
    pub platform_indicated_airspeed: Option<u16>,
    /// Tag 5: Telemetry Accuracy Indicator (uint8) — reserved by the spec.
    pub telemetry_accuracy_indicator: Option<u8>,
    /// Tag 6: Frag Circle Radius — meters (uint16).
    pub frag_circle_radius_m: Option<u16>,
    /// Tag 7: Frame Code — 60 Hz counter (uint32).
    pub frame_code: Option<u32>,
    /// Tag 8: UAS LS Version Number (uint8) — informational; do NOT assert
    /// this equals 4 (the document edition), it tracks the wire schema.
    pub rvt_ls_version: Option<u8>,
    /// Tag 9: Video Data Rate — bps / Hz (uint32).
    pub video_data_rate: Option<u32>,
    /// Tag 10: Digital Video File Format — ISO 7 string, max 127 bytes.
    pub digital_video_file_format: Option<String>,
    /// Tag 11: User Defined LS — repeatable (ST 0806.4-25).
    pub user_defined: Vec<RvtUserData>,
    /// Tag 12: Point of Interest LS — repeatable (ST 0806.4-25).
    pub points_of_interest: Vec<RvtPoi>,
    /// Tag 13: Area of Interest LS — repeatable (ST 0806.4-25).
    pub areas_of_interest: Vec<RvtAoi>,
    /// Tag 14: Aircraft MGRS Zone — UTM zone 1-60 (uint8).
    pub aircraft_mgrs_zone: Option<u8>,
    /// Tag 15: Aircraft MGRS Latitude Band and Grid Square — 3-char ISO 7 string.
    pub aircraft_mgrs_band_grid: Option<String>,
    /// Tag 16: Aircraft MGRS Easting — meters, 0-99999 (uint24).
    pub aircraft_mgrs_easting_m: Option<u32>,
    /// Tag 17: Aircraft MGRS Northing — meters, 0-99999 (uint24).
    pub aircraft_mgrs_northing_m: Option<u32>,
    /// Tag 18: Frame Center MGRS Zone — UTM zone 1-60 (uint8).
    pub frame_center_mgrs_zone: Option<u8>,
    /// Tag 19: Frame Center MGRS Latitude Band and Grid Square — 3-char ISO 7 string.
    pub frame_center_mgrs_band_grid: Option<String>,
    /// Tag 20: Frame Center MGRS Easting — meters, 0-99999 (uint24).
    pub frame_center_mgrs_easting_m: Option<u32>,
    /// Tag 21: Frame Center MGRS Northing — meters, 0-99999 (uint24).
    pub frame_center_mgrs_northing_m: Option<u32>,
    /// Tags not modeled above, passed through byte-for-byte.
    pub unknown: Vec<OwnedRawField>,
    /// Per-field decode errors collected instead of aborting the whole set.
    pub field_errors: Vec<KlvFieldError>,
}

impl RvtLs {
    /// Reconstruct the 15-char aircraft MGRS string (zone zero-padded to 2,
    /// band+grid 3 chars, easting/northing zero-padded to 5), or None when
    /// any of the four components is missing.
    #[must_use]
    pub fn aircraft_mgrs(&self) -> Option<String> {
        mgrs_string(
            self.aircraft_mgrs_zone,
            self.aircraft_mgrs_band_grid.as_deref(),
            self.aircraft_mgrs_easting_m,
            self.aircraft_mgrs_northing_m,
        )
    }
    /// Frame-center MGRS string (tags 18-21), same layout.
    #[must_use]
    pub fn frame_center_mgrs(&self) -> Option<String> {
        mgrs_string(
            self.frame_center_mgrs_zone,
            self.frame_center_mgrs_band_grid.as_deref(),
            self.frame_center_mgrs_easting_m,
            self.frame_center_mgrs_northing_m,
        )
    }
}

fn mgrs_string(
    zone: Option<u8>,
    band_grid: Option<&str>,
    easting: Option<u32>,
    northing: Option<u32>,
) -> Option<String> {
    use alloc::format;
    Some(format!(
        "{:02}{}{:05}{:05}",
        zone?, band_grid?, easting?, northing?
    ))
}

#[cfg(test)]
mod variant_inventory {
    //! Exhaustiveness for this module's wire-code enums (Arc 2 R2). See
    //! [`crate::klv::inventory_test`].
    use super::*;
    use crate::klv::inventory_test;

    inventory_test!(
        rvt_poi_type_all_is_complete_and_round_trips,
        RvtPoiType,
        [
            RvtPoiType::Friendly,
            RvtPoiType::Hostile,
            RvtPoiType::Target,
            RvtPoiType::Unknown,
            RvtPoiType::Other(_),
        ]
    );
    inventory_test!(
        rvt_aoi_type_all_is_complete_and_round_trips,
        RvtAoiType,
        [
            RvtAoiType::Friendly,
            RvtAoiType::Hostile,
            RvtAoiType::Reserved,
            RvtAoiType::Unknown,
            RvtAoiType::Other(_),
        ]
    );
}
