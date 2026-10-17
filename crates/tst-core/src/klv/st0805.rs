//! MISB ST 0805.1 KLV → Cursor-on-Target (CoT) conversion.
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! ST 0805.1 defines two one-way conversions from a decoded ST 0601 UAS
//! Datalink LS record ([`UasDatalinkLs`]) to Cursor-on-Target XML: a
//! **Platform Position** event (the platform's own position) and a
//! **Sensor Point of Interest** (SPI) event (the sensor's ground aimpoint),
//! linked back to the platform event via `detail/link`.
//!
//! Per §1, the spec deliberately forbids UUIDs for `uid` — CoT generated
//! from a replayed file must be byte-identical to CoT generated live, and a
//! UUID minted after the fact would differ run to run. `uid` is instead a
//! deterministic concatenation of KLV tags: [`platform_uid`] =
//! `"{tag10}_{tag3}"`, [`spi_uid`] = `"{tag10}_{tag3}_{tag11}"`.
//!
//! This module implements KLV→CoT only — ST 0805.1 defines no reverse
//! (CoT→KLV) mapping (see `docs/project/deferred-features.md`).
//!
//! This is a pure conversion over an already-decoded record, not a KLV byte
//! parser, so it has no fuzz target.
//!
//! **Deterministic-output contract:** numeric attributes are formatted via
//! Rust's default `Display for f64` (`format!("{v}")`) — the shortest
//! round-trip representation, which omits a trailing `.0` for whole values
//! (`1524.0` → `"1524"`, `34.05` → `"34.05"`, `30.0` → `"30"`). The
//! `point/@ce` / `point/@le` "no value given" sentinel is written as the
//! literal integer string `"9999999"` (§3), not routed through float
//! formatting. This rule is part of what keeps replayed-file CoT
//! byte-identical to live CoT (§1) — do not switch to `{:?}` or a
//! fixed-precision formatter.

use super::st0601::UasDatalinkLs;
use crate::error::CotError;
use alloc::format;
use alloc::string::String;

/// ST 0805.1 §5 Table 2: the SPI CoT `type` is fixed — "note that this will
/// not change, unlike platform type."
const SPI_TYPE: &str = "b-m-p-s-p-i";

/// ST 0805.1 §5 Table 1 / Table 2 / §3: sentinel for `point/@ce` and
/// `point/@le` when no error estimate is mapped (Platform, always) or the
/// source tag is absent (SPI). Spec verbatim: "This represents 'no value
/// given'" / "If key is not available, replace with 9999999." Written as
/// the literal integer string per the spec text — see the module-level
/// deterministic-output contract above.
const SENTINEL_CE_LE: &str = "9999999";

/// ST 0805.1 §5 Table 2: "Conversion from 2.146 σ (CE90) to 1 σ (CoT
/// standard) necessary."
const CE90_TO_1SIGMA: f64 = 2.146;

/// ST 0805.1 §5 Table 2: "Conversion from 1.645 σ (LE90) to 1 σ (CoT
/// standard) necessary."
const LE90_TO_1SIGMA: f64 = 1.645;

crate::binding_record! {
    /// Configuration for KLV→CoT conversion (ST 0805.1).
    #[derive(Debug, Clone, PartialEq)]
    pub struct CotConfig {
        /// CoT type for the Platform Position event. ST 0805.1 §5 gives
        /// `a-f-A-M-F` as the fixed-wing example and explicitly requires it be
        /// overridable per platform (rotary-wing, manned pods, ...).
        @[py_default("\"a-f-A-M-F\"")]
        pub platform_type: String,
        /// The `stale` time is the event time plus this interval. ST 0805.1 defines `stale` as
        /// "time of next message" but gives no concrete interval — this default
        /// is an implementation choice, not a spec value.
        @[py_default("5_000_000")]
        pub update_interval_us: u64,
        /// XML attribute name stamped in `<detail><_flow-tags_ .../>`. Written
        /// verbatim as an XML `Name` production (an attribute *name*, not an
        /// *value*) — it must be a syntactically valid XML Name. It is neither
        /// validated nor escaped; an invalid value produces malformed XML.
        @[py_default("\"ST0601CoT\"")]
        pub producer: String,
        /// Geoid undulation (HAE − MSL) applied when only an MSL-referenced
        /// altitude tag is available. Absent emits the MSL value as-is.
        pub geoid_undulation_m: Option<f64>,
        /// CoT `how` attribute. ST 0805.1 §5 fixes this at `m-p`
        /// (machine-passed) for both event types.
        @[py_default("\"m-p\"")]
        pub how: String,
    }
}

impl Default for CotConfig {
    fn default() -> Self {
        Self {
            platform_type: String::from("a-f-A-M-F"),
            update_interval_us: 5_000_000,
            producer: String::from("ST0601CoT"),
            geoid_undulation_m: None,
            how: String::from("m-p"),
        }
    }
}

/// Deterministic Platform Position `uid`: `"{tag10}_{tag3}"` (ST 0805.1 §5
/// Table 1: "concatenate Tags 10 and 3 separated by an underscore").
pub fn platform_uid(ls: &UasDatalinkLs) -> Result<String, CotError> {
    let tag10 = ls
        .platform_designation
        .as_deref()
        .ok_or(CotError::MissingField {
            tag: 10,
            name: "Platform Designation",
        })?;
    let tag3 = ls.mission_id.as_deref().ok_or(CotError::MissingField {
        tag: 3,
        name: "Mission ID",
    })?;
    Ok(format!("{tag10}_{tag3}"))
}

/// Deterministic SPI `uid`: `"{tag10}_{tag3}_{tag11}"` (ST 0805.1 §5
/// Table 2: "concatenate Tags 10, 3, and 11 with an underscore before and
/// after Tag 3").
pub fn spi_uid(ls: &UasDatalinkLs) -> Result<String, CotError> {
    let tag10 = ls
        .platform_designation
        .as_deref()
        .ok_or(CotError::MissingField {
            tag: 10,
            name: "Platform Designation",
        })?;
    let tag3 = ls.mission_id.as_deref().ok_or(CotError::MissingField {
        tag: 3,
        name: "Mission ID",
    })?;
    let tag11 = ls
        .image_source_sensor
        .as_deref()
        .ok_or(CotError::MissingField {
            tag: 11,
            name: "Image Source Sensor",
        })?;
    Ok(format!("{tag10}_{tag3}_{tag11}"))
}

/// Escapes the five predefined XML entities (`& < > " '`) in `s`.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Formats a POSIX-epoch microsecond timestamp (ST 0601 Tag 2 / MISB
/// ST 0603) as ISO 8601 `YYYY-MM-DDThh:mm:ss.ffffffZ`.
///
/// Pure integer math — no `chrono`, no `std::time` — via the standard
/// Howard-Hinnant `civil_from_days` days-since-epoch → (y, m, d) algorithm,
/// so this stays `no_std`+`alloc`-clean.
fn iso8601_us(micros: u64) -> String {
    let total_seconds = micros / 1_000_000;
    let frac_micros = micros % 1_000_000;
    let days = (total_seconds / 86_400) as i64;
    let secs_of_day = total_seconds % 86_400;
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;

    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{frac_micros:06}Z")
}

/// Days-since-1970-01-01 → (year, month, day) in the proleptic Gregorian
/// calendar. Howard Hinnant's `civil_from_days`
/// (<http://howardhinnant.github.io/date_algorithms.html>), ported
/// verbatim — the integer-division tricks are load-bearing, do not
/// "simplify" the arithmetic.
///
/// Only called from `iso8601_us`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m: u32 = if mp < 10 {
        (mp + 3) as u32
    } else {
        (mp - 9) as u32
    }; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

/// Applies the configured geoid undulation (HAE = MSL + N) to an
/// MSL-referenced altitude/elevation tag, when the caller has supplied one
/// via [`CotConfig::geoid_undulation_m`]. `None` passes the MSL value
/// through as-is — the documented datum caveat used when no undulation
/// value is available (item 7 of the mapping analysis).
fn hae_from_msl(msl: Option<f64>, cfg: &CotConfig) -> Option<f64> {
    msl.map(|v| match cfg.geoid_undulation_m {
        Some(n) => v + n,
        None => v,
    })
}

/// Serializes a **Platform Position** CoT event (ST 0805.1 §5 Table 1) from
/// a decoded ST 0601 record: the platform's own position, with a `sensor`
/// sub-element describing where the sensor is looking.
///
/// `generated_us` (POSIX epoch microseconds) is the wall-clock generation
/// time stamped into `detail/_flow-tags_`. It is an **argument**, not
/// sampled internally, so conversion stays deterministic and `no_std` — a
/// replayed-file CoT run must be byte-identical to a live one (§1).
///
/// `point/@hae` prefers the HAE-native fields, in the ST 0601.19 extended-
/// representation precedence order (reqs 0601.9-20/-21, quoted on
/// [`UasDatalinkLs::sensor_ellipsoid_height_m`](crate::klv::st0601::UasDatalinkLs::sensor_ellipsoid_height_m):
/// a decoder that understands Item 104 uses it and ignores Item 75 when
/// both are present — Item 75 also saturates at 19000 m): **Sensor
/// Ellipsoid Height Extended (Tag 104)**, then **Sensor Ellipsoid Height
/// (Tag 75)**. Only when neither is present does it fall
/// back to the MSL-referenced Sensor True Altitude (Tag 15), adjusted by
/// [`CotConfig::geoid_undulation_m`] when the caller has supplied one
/// (item 7 of the mapping analysis — ST 0805.1 requires an MSL→HAE
/// conversion but supplies no geoid model).
///
/// The `sensor` sub-element is omitted entirely when either Tag 5
/// (Platform Heading Angle) or Tag 18 (Sensor Relative Azimuth Angle) is
/// absent, since `azimuth` cannot be computed; its other attributes
/// (`fov`/`vfov`/`model`/`range`) are each omitted individually when their
/// source tag is absent.
///
/// # Errors
/// [`CotError::MissingField`] when a KLV tag the mapping requires (uid
/// components, timestamp, sensor position, altitude) is absent from `ls`.
pub fn platform_position_xml(
    ls: &UasDatalinkLs,
    cfg: &CotConfig,
    generated_us: u64,
) -> Result<String, CotError> {
    let uid = platform_uid(ls)?;
    let timestamp_us = ls.timestamp_us.ok_or(CotError::MissingField {
        tag: 2,
        name: "Precision Time Stamp",
    })?;
    let lat = ls.sensor_lat_deg.ok_or(CotError::MissingField {
        tag: 13,
        name: "Sensor Latitude",
    })?;
    let lon = ls.sensor_lon_deg.ok_or(CotError::MissingField {
        tag: 14,
        name: "Sensor Longitude",
    })?;
    let hae = ls
        .sensor_ellipsoid_height_extended_m
        .or(ls.sensor_ellipsoid_height_m)
        .or_else(|| hae_from_msl(ls.sensor_alt_m, cfg))
        .ok_or(CotError::MissingField {
            tag: 15,
            name: "Sensor True Altitude",
        })?;

    let time = iso8601_us(timestamp_us);
    let stale = iso8601_us(timestamp_us.saturating_add(cfg.update_interval_us));
    let generated = iso8601_us(generated_us);

    let sensor = match (ls.platform_heading_deg, ls.sensor_rel_az_deg) {
        (Some(heading), Some(rel_az)) => {
            // Absolute azimuth normalized into [0, 360) — the spec states
            // "absolute azimuth" without spelling out mod-360, but a sum of
            // two angles needs it to stay a valid CoT azimuth.
            let sum = (heading + rel_az) % 360.0;
            let azimuth = if sum < 0.0 { sum + 360.0 } else { sum };

            let mut s = format!(r#"<sensor azimuth="{azimuth}""#);
            if let Some(fov) = ls.sensor_hfov_deg {
                s.push_str(&format!(r#" fov="{fov}""#));
            }
            if let Some(vfov) = ls.sensor_vfov_deg {
                s.push_str(&format!(r#" vfov="{vfov}""#));
            }
            if let Some(model) = ls.image_source_sensor.as_deref() {
                s.push_str(&format!(r#" model="{}""#, xml_escape(model)));
            }
            if let Some(range) = ls.slant_range_m {
                s.push_str(&format!(r#" range="{range}""#));
            }
            s.push_str("/>");
            s
        }
        _ => String::new(),
    };

    Ok(format!(
        concat!(
            r#"<?xml version='1.0' standalone='yes'?>"#,
            r#"<event version="2.0" uid="{uid}" type="{ptype}" "#,
            r#"time="{time}" start="{time}" stale="{stale}" how="{how}">"#,
            r#"<point lat="{lat}" lon="{lon}" hae="{hae}" ce="{sentinel}" le="{sentinel}"/>"#,
            r#"<detail><_flow-tags_ {producer}="{generated}"/>{sensor}</detail></event>"#,
        ),
        uid = xml_escape(&uid),
        ptype = xml_escape(&cfg.platform_type),
        time = time,
        stale = stale,
        how = xml_escape(&cfg.how),
        lat = lat,
        lon = lon,
        hae = hae,
        sentinel = SENTINEL_CE_LE,
        producer = cfg.producer,
        generated = generated,
        sensor = sensor,
    ))
}

/// Serializes a **Sensor Point of Interest** CoT event (ST 0805.1 §5
/// Table 2) from a decoded ST 0601 record: the sensor's ground aimpoint,
/// linked back to the Platform Position event via `detail/link`.
///
/// `generated_us` — see [`platform_position_xml`]; same determinism
/// contract.
///
/// `point/@lat`/`@lon` prefer Target Location (Tags 40/41) when present,
/// else Frame Center (Tags 23/24). `point/@hae` follows the same pairing:
/// Target Location Elevation (Tag 42, MSL, +[`CotConfig::geoid_undulation_m`])
/// when Target Location supplies the position, else the HAE-native Frame
/// Center Ellipsoid Height (Tag 78) when present, else Frame Center
/// Elevation (Tag 25, MSL, +geoid undulation).
///
/// `point/@ce`/`@le` divide Tags 45/46 (CE90/LE90) by the spec-exact
/// constants `2.146`/`1.645` to convert to CoT's 1σ convention, falling
/// back to the `9999999` "no value given" sentinel when the source tag is
/// absent (§3).
///
/// # Errors
/// [`CotError::MissingField`] when a KLV tag the mapping requires (uid
/// components, timestamp, an aimpoint position pair, that pair's
/// elevation) is absent from `ls`.
pub fn sensor_point_of_interest_xml(
    ls: &UasDatalinkLs,
    cfg: &CotConfig,
    generated_us: u64,
) -> Result<String, CotError> {
    let uid = spi_uid(ls)?;
    let platform_uid = platform_uid(ls)?;
    let timestamp_us = ls.timestamp_us.ok_or(CotError::MissingField {
        tag: 2,
        name: "Precision Time Stamp",
    })?;

    let (lat, lon, hae) = if let (Some(lat), Some(lon)) =
        (ls.target_location_lat_deg, ls.target_location_lon_deg)
    {
        let hae = hae_from_msl(ls.target_location_elev_m, cfg).ok_or(CotError::MissingField {
            tag: 42,
            name: "Target Location Elevation",
        })?;
        (lat, lon, hae)
    } else if let (Some(lat), Some(lon)) = (ls.frame_center_lat_deg, ls.frame_center_lon_deg) {
        let hae = ls
            .frame_center_ellipsoid_height_m
            .or_else(|| hae_from_msl(ls.frame_center_elev_m, cfg))
            .ok_or(CotError::MissingField {
                tag: 25,
                name: "Frame Center Elevation",
            })?;
        (lat, lon, hae)
    } else {
        return Err(CotError::MissingField {
            tag: 23,
            name: "Frame Center Latitude",
        });
    };

    let ce = match ls.target_error_ce90_m {
        Some(v) => format!("{}", v / CE90_TO_1SIGMA),
        None => String::from(SENTINEL_CE_LE),
    };
    let le = match ls.target_error_le90_m {
        Some(v) => format!("{}", v / LE90_TO_1SIGMA),
        None => String::from(SENTINEL_CE_LE),
    };

    let time = iso8601_us(timestamp_us);
    let stale = iso8601_us(timestamp_us.saturating_add(cfg.update_interval_us));
    let generated = iso8601_us(generated_us);

    Ok(format!(
        concat!(
            r#"<?xml version='1.0' standalone='yes'?>"#,
            r#"<event version="2.0" uid="{uid}" type="{spi_type}" "#,
            r#"time="{time}" start="{time}" stale="{stale}" how="{how}">"#,
            r#"<point lat="{lat}" lon="{lon}" hae="{hae}" ce="{ce}" le="{le}"/>"#,
            r#"<detail><_flow-tags_ {producer}="{generated}"/>"#,
            r#"<link relation="p-p" type="{ptype}" uid="{platform_uid}"/></detail></event>"#,
        ),
        uid = xml_escape(&uid),
        spi_type = SPI_TYPE,
        time = time,
        stale = stale,
        how = xml_escape(&cfg.how),
        lat = lat,
        lon = lon,
        hae = hae,
        ce = ce,
        le = le,
        producer = cfg.producer,
        generated = generated,
        ptype = xml_escape(&cfg.platform_type),
        platform_uid = xml_escape(&platform_uid),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_from_micros_matches_spec_example() {
        assert_eq!(
            iso8601_us(798_039_894_000_000),
            "1995-04-16T13:44:54.000000Z"
        );
        assert_eq!(
            iso8601_us(1_529_588_637_122_999),
            "2018-06-21T13:43:57.122999Z"
        ); // Tag 131 example
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn uids_are_deterministic_concatenations() {
        let mut ls = UasDatalinkLs::default();
        ls.platform_designation = Some("PRED01".into());
        ls.mission_id = Some("M05".into());
        ls.image_source_sensor = Some("EO".into());
        assert_eq!(platform_uid(&ls).unwrap(), "PRED01_M05");
        assert_eq!(spi_uid(&ls).unwrap(), "PRED01_M05_EO");
        ls.mission_id = None;
        assert!(matches!(
            platform_uid(&ls).unwrap_err(),
            CotError::MissingField { tag: 3, .. }
        ));
    }

    #[test]
    fn xml_escape_covers_five_entities() {
        assert_eq!(
            xml_escape("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }

    #[allow(clippy::field_reassign_with_default)]
    fn fixture() -> UasDatalinkLs {
        let mut ls = UasDatalinkLs::default();
        ls.timestamp_us = Some(798_039_894_000_000);
        ls.platform_designation = Some("PRED01".into());
        ls.mission_id = Some("M05".into());
        ls.image_source_sensor = Some("EO".into());
        ls.sensor_lat_deg = Some(34.05);
        ls.sensor_lon_deg = Some(-118.25);
        ls.sensor_ellipsoid_height_m = Some(1524.0); // HAE-native → no geoid needed
        ls.platform_heading_deg = Some(90.0);
        ls.sensor_rel_az_deg = Some(300.0); // 90+300 = 390 → azimuth 30.0
        ls.sensor_hfov_deg = Some(2.5);
        ls.sensor_vfov_deg = Some(1.9);
        ls.slant_range_m = Some(12_000.0);
        ls.target_location_lat_deg = Some(34.10); // SPI prefers 40/41 over 23/24
        ls.target_location_lon_deg = Some(-118.20);
        ls.target_location_elev_m = Some(250.0); // MSL, no undulation set → as-is (documented)
        ls.target_error_ce90_m = Some(425.215152);
        ls.target_error_le90_m = Some(608.9231);
        ls
    }

    #[test]
    fn platform_position_golden() {
        let xml =
            platform_position_xml(&fixture(), &CotConfig::default(), 798_039_895_000_000).unwrap();
        assert_eq!(
            xml,
            concat!(
                r#"<?xml version='1.0' standalone='yes'?>"#,
                r#"<event version="2.0" uid="PRED01_M05" type="a-f-A-M-F" "#,
                r#"time="1995-04-16T13:44:54.000000Z" start="1995-04-16T13:44:54.000000Z" "#,
                r#"stale="1995-04-16T13:44:59.000000Z" how="m-p">"#,
                r#"<point lat="34.05" lon="-118.25" hae="1524" ce="9999999" le="9999999"/>"#,
                r#"<detail><_flow-tags_ ST0601CoT="1995-04-16T13:44:55.000000Z"/>"#,
                r#"<sensor azimuth="30" fov="2.5" vfov="1.9" model="EO" range="12000"/>"#,
                r#"</detail></event>"#,
            )
        );
    }

    #[test]
    fn spi_golden_with_ce_le_divisors() {
        let xml =
            sensor_point_of_interest_xml(&fixture(), &CotConfig::default(), 798_039_895_000_000)
                .unwrap();
        // ce = 425.215152 / 2.146 = 198.14312767940353 ; le = 608.9231 / 1.645 = 370.166018237082
        assert!(xml.contains(r#"type="b-m-p-s-p-i""#));
        assert!(xml.contains(r#"uid="PRED01_M05_EO""#));
        assert!(xml.contains(r#"lat="34.1""#) && xml.contains(r#"lon="-118.2""#));
        assert!(xml.contains(r#"ce="198.14312"#) && xml.contains(r#"le="370.16601"#));
        assert!(xml.contains(r#"<link relation="p-p" type="a-f-A-M-F" uid="PRED01_M05"/>"#));
    }

    #[test]
    fn spi_falls_back_to_frame_center_and_sentinels() {
        let mut ls = fixture();
        ls.target_location_lat_deg = None;
        ls.target_location_lon_deg = None;
        ls.target_location_elev_m = None;
        ls.frame_center_lat_deg = Some(34.2);
        ls.frame_center_lon_deg = Some(-118.3);
        ls.frame_center_ellipsoid_height_m = Some(300.0); // HAE-native frame-center (tag 78) preferred
        ls.target_error_ce90_m = None;
        ls.target_error_le90_m = None;
        let xml = sensor_point_of_interest_xml(&ls, &CotConfig::default(), 0).unwrap();
        assert!(xml.contains(r#"lat="34.2""#));
        assert!(xml.contains(r#"hae="300""#));
        assert!(xml.contains(r#"ce="9999999""#) && xml.contains(r#"le="9999999""#));
    }

    #[test]
    fn azimuth_normalizes_and_missing_inputs_error() {
        // (heading 90 + rel-az 300) mod 360 = 30 — covered by the golden; negative case:
        let mut ls = fixture();
        ls.timestamp_us = None;
        assert!(matches!(
            platform_position_xml(&ls, &CotConfig::default(), 0).unwrap_err(),
            CotError::MissingField { tag: 2, .. }
        ));

        // Sum-of-two-angles can go negative before the final mod-360 step
        // (the `%` operator keeps the dividend's sign); renormalization
        // must still land the result in [0, 360).
        let mut neg = fixture();
        neg.platform_heading_deg = Some(-10.0);
        neg.sensor_rel_az_deg = Some(-20.0); // sum = -30 → renormalized to 330
        let xml = platform_position_xml(&neg, &CotConfig::default(), 798_039_895_000_000).unwrap();
        assert!(xml.contains(r#"azimuth="330""#));

        // Missing Tag 10 (Platform Designation) surfaces its own MissingField
        // (uid is checked before timestamp/position).
        let mut no_tag10 = fixture();
        no_tag10.platform_designation = None;
        assert!(matches!(
            platform_position_xml(&no_tag10, &CotConfig::default(), 0).unwrap_err(),
            CotError::MissingField { tag: 10, .. }
        ));
    }

    #[test]
    fn platform_hae_uses_item_104_when_only_extended_present() {
        // Item 104 (Sensor Ellipsoid Height Extended) is an HAE-native
        // source in its own right — usable even with no Tag 75 and no
        // Tag 15 present at all.
        let mut ls = fixture();
        ls.sensor_ellipsoid_height_m = None;
        ls.sensor_ellipsoid_height_extended_m = Some(25_000.0);
        let xml = platform_position_xml(&ls, &CotConfig::default(), 798_039_895_000_000).unwrap();
        assert!(xml.contains(r#"hae="25000""#));
    }

    #[test]
    fn platform_hae_item_104_wins_over_75_when_both_present() {
        // ST 0601.19 reqs 0601.9-20/-21 (see UasDatalinkLs::sensor_ellipsoid_height_m's
        // rustdoc): a decoder that understands Item 104 uses it and ignores
        // Item 75 when both are present. Item 75 also saturates at 19000 m,
        // so this is the only way to represent an altitude above that ceiling.
        let mut ls = fixture();
        ls.sensor_ellipsoid_height_m = Some(19_000.0); // Tag 75, saturated
        ls.sensor_ellipsoid_height_extended_m = Some(25_000.0); // Tag 104
        let xml = platform_position_xml(&ls, &CotConfig::default(), 798_039_895_000_000).unwrap();
        assert!(xml.contains(r#"hae="25000""#));
    }

    #[test]
    fn platform_hae_applies_geoid_undulation_to_msl_altitude() {
        // Tag-15-only altitude (no 75, no 104) plus a configured geoid
        // undulation: hae = msl + undulation, applied exactly.
        let mut ls = fixture();
        ls.sensor_ellipsoid_height_m = None;
        ls.sensor_alt_m = Some(1500.0);
        let cfg = CotConfig {
            geoid_undulation_m: Some(24.5),
            ..CotConfig::default()
        };
        let xml = platform_position_xml(&ls, &cfg, 798_039_895_000_000).unwrap();
        assert!(xml.contains(r#"hae="1524.5""#));
    }

    #[test]
    fn sensor_element_omits_individually_absent_attributes() {
        // Tags 5/18 (heading/rel-az) still present → the sensor element is
        // still emitted, but `range` is dropped since its source tag
        // (slant_range_m, Tag 21) is absent — omission is per-attribute,
        // not all-or-nothing like the azimuth-driven whole-element case.
        let mut ls = fixture();
        ls.slant_range_m = None;
        let xml = platform_position_xml(&ls, &CotConfig::default(), 798_039_895_000_000).unwrap();
        assert!(xml.contains(r#"<sensor azimuth="30" fov="2.5" vfov="1.9" model="EO"/>"#));
        assert!(!xml.contains("range="));
    }
}
