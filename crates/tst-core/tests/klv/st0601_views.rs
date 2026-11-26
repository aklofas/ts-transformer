//! The ST 0601 composite views. The cases live in one table,
//! `fixtures/st0601/composite-views.json`, which the Python and JVM suites
//! read too, so the three implementations of the views answer the same rows.

use serde_json::Value;
use tst_core::klv::ImapbSpecial;
use tst_core::klv::st0601::{self, AltitudeDatum, UasDatalinkLs};

fn table() -> Vec<Value> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/st0601/composite-views.json"
    );
    let text = std::fs::read_to_string(path).unwrap();
    let doc: Value = serde_json::from_str(&text).unwrap();
    doc["rows"].as_array().unwrap().clone()
}

fn set_field(ls: &mut UasDatalinkLs, name: &str, v: f64) {
    let field = match name {
        "platform_heading_deg" => &mut ls.platform_heading_deg,
        "platform_pitch_deg" => &mut ls.platform_pitch_deg,
        "platform_roll_deg" => &mut ls.platform_roll_deg,
        "platform_pitch_full_deg" => &mut ls.platform_pitch_full_deg,
        "platform_roll_full_deg" => &mut ls.platform_roll_full_deg,
        "sensor_lat_deg" => &mut ls.sensor_lat_deg,
        "sensor_lon_deg" => &mut ls.sensor_lon_deg,
        "sensor_alt_m" => &mut ls.sensor_alt_m,
        "sensor_ellipsoid_height_m" => &mut ls.sensor_ellipsoid_height_m,
        "sensor_ellipsoid_height_extended_m" => &mut ls.sensor_ellipsoid_height_extended_m,
        "frame_center_lat_deg" => &mut ls.frame_center_lat_deg,
        "frame_center_lon_deg" => &mut ls.frame_center_lon_deg,
        "frame_center_elev_m" => &mut ls.frame_center_elev_m,
        "frame_center_ellipsoid_height_m" => &mut ls.frame_center_ellipsoid_height_m,
        "corner_lat_p1_deg" => &mut ls.corner_lat_p1_deg,
        "corner_lon_p1_deg" => &mut ls.corner_lon_p1_deg,
        "corner_lat_p2_deg" => &mut ls.corner_lat_p2_deg,
        "corner_lon_p2_deg" => &mut ls.corner_lon_p2_deg,
        "corner_lat_p3_deg" => &mut ls.corner_lat_p3_deg,
        "corner_lon_p3_deg" => &mut ls.corner_lon_p3_deg,
        "corner_lat_p4_deg" => &mut ls.corner_lat_p4_deg,
        "corner_lon_p4_deg" => &mut ls.corner_lon_p4_deg,
        "corner_lat_offset_p1_deg" => &mut ls.corner_lat_offset_p1_deg,
        "corner_lon_offset_p1_deg" => &mut ls.corner_lon_offset_p1_deg,
        "corner_lat_offset_p2_deg" => &mut ls.corner_lat_offset_p2_deg,
        "corner_lon_offset_p2_deg" => &mut ls.corner_lon_offset_p2_deg,
        "corner_lat_offset_p3_deg" => &mut ls.corner_lat_offset_p3_deg,
        "corner_lon_offset_p3_deg" => &mut ls.corner_lon_offset_p3_deg,
        "corner_lat_offset_p4_deg" => &mut ls.corner_lat_offset_p4_deg,
        "corner_lon_offset_p4_deg" => &mut ls.corner_lon_offset_p4_deg,
        other => panic!("the case table names a field this test does not set: {other}"),
    };
    *field = Some(v);
}

fn special(code: &str, payload: u64) -> ImapbSpecial {
    match code {
        "below_min" => ImapbSpecial::BelowMin,
        "above_max" => ImapbSpecial::AboveMax,
        "pos_infinity" => ImapbSpecial::PositiveInfinity,
        "neg_infinity" => ImapbSpecial::NegativeInfinity,
        "pos_quiet_nan" => ImapbSpecial::PositiveQuietNaN { nan_id: payload },
        "neg_quiet_nan" => ImapbSpecial::NegativeQuietNaN { nan_id: payload },
        "pos_signaling_nan" => ImapbSpecial::PositiveSignalingNaN { signal: payload },
        "neg_signaling_nan" => ImapbSpecial::NegativeSignalingNaN { signal: payload },
        "user_defined" => ImapbSpecial::UserDefined { signal: payload },
        other => panic!("unknown special code {other}"),
    }
}

fn record(row: &Value) -> UasDatalinkLs {
    let mut ls = UasDatalinkLs::default();
    for (name, v) in row["fields"].as_object().unwrap() {
        set_field(&mut ls, name, v.as_f64().unwrap());
    }
    if let Some(tags) = row.get("sentinel_tags") {
        ls.sentinel_tags = tags
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_u64().unwrap() as u32)
            .collect();
    }
    if let Some(entries) = row.get("imapb_specials") {
        ls.imapb_specials = entries
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e[0].as_u64().unwrap() as u32,
                    special(e[1].as_str().unwrap(), e[2].as_u64().unwrap()),
                )
            })
            .collect();
    }
    ls
}

fn close(name: &str, tol: f64, got: f64, want: &Value) {
    let want = want.as_f64().unwrap();
    assert!(
        if tol == 0.0 {
            got == want
        } else {
            (got - want).abs() <= tol
        },
        "{name}: got {got:?}, want {want:?}"
    );
}

fn datum_name(d: Option<AltitudeDatum>) -> Value {
    match d {
        None => Value::Null,
        Some(AltitudeDatum::Hae) => Value::from("hae"),
        Some(AltitudeDatum::Msl) => Value::from("msl"),
        Some(other) => panic!("a datum the table cannot name: {other:?}"),
    }
}

fn check_row(row: &Value) {
    let name = row["name"].as_str().unwrap();
    let tol = row.get("tolerance").and_then(Value::as_f64).unwrap_or(0.0);
    let ls = record(row);
    for (view, want) in row["expect"].as_object().unwrap() {
        let got: Option<Vec<f64>> = match view.as_str() {
            "platform_attitude" => ls
                .platform_attitude()
                .map(|a| vec![a.heading_deg, a.pitch_deg, a.roll_deg]),
            "sensor_position" => ls
                .sensor_position()
                .map(|p| vec![p.lat_deg, p.lon_deg, p.alt_m]),
            "frame_center" => ls
                .frame_center()
                .map(|p| vec![p.lat_deg, p.lon_deg, p.alt_m]),
            "corners" => ls.corners().map(|c| {
                vec![
                    c.p1.0, c.p1.1, c.p2.0, c.p2.1, c.p3.0, c.p3.1, c.p4.0, c.p4.1,
                ]
            }),
            "sensor_position_datum" => {
                assert_eq!(&datum_name(ls.sensor_position_datum()), want, "{name}");
                continue;
            }
            "frame_center_datum" => {
                assert_eq!(&datum_name(ls.frame_center_datum()), want, "{name}");
                continue;
            }
            other => panic!("{name}: unknown view {other}"),
        };
        match (got, want) {
            (None, Value::Null) => {}
            (Some(got), Value::Array(want)) => {
                let want: Vec<&Value> = want
                    .iter()
                    .flat_map(|w| match w {
                        Value::Array(pair) => pair.iter().collect::<Vec<_>>(),
                        one => vec![one],
                    })
                    .collect();
                assert_eq!(got.len(), want.len(), "{name}: {view}");
                for (g, w) in got.iter().zip(want) {
                    close(&format!("{name}: {view}"), tol, *g, w);
                }
            }
            (got, want) => panic!("{name}: {view} is {got:?}, want {want}"),
        }
    }
}

/// Runs every row whose name is `prefix`, or starts with `prefix: `.
fn check_rows(prefix: &str) {
    let rows: Vec<Value> = table()
        .into_iter()
        .filter(|r| {
            let n = r["name"].as_str().unwrap();
            n == prefix || n.starts_with(&format!("{prefix}: "))
        })
        .collect();
    assert!(!rows.is_empty(), "no case-table row named {prefix}");
    for row in &rows {
        check_row(row);
    }
}

#[test]
fn every_composite_view_case_holds() {
    for row in &table() {
        check_row(row);
    }
}

#[test]
fn platform_attitude_prefers_full_range_twins() {
    check_rows("platform_attitude_prefers_full_range_twins");
}

#[test]
fn sensor_position_prefers_hae_and_reports_datum() {
    check_rows("sensor_position_prefers_hae_and_reports_datum");
}

#[test]
fn sensor_position_msl_only_unchanged() {
    check_rows("sensor_position_msl_only_unchanged");
}

#[test]
fn frame_center_prefers_hae() {
    check_rows("frame_center_prefers_hae");
}

#[test]
fn corners_do_not_fall_back_through_off_earth_full_corner() {
    check_rows("corners_do_not_fall_back_through_off_earth_full_corner");
}

#[test]
fn corners_mix_families_per_point() {
    check_rows("corners_mix_families_per_point");
}

#[test]
fn corners_wrap_longitude_across_the_antimeridian() {
    check_rows("corners_wrap_longitude_across_the_antimeridian");
}

#[test]
fn a_preferred_item_sent_as_no_value_ends_the_view() {
    check_rows("a_preferred_item_sent_as_no_value_ends_the_view");
}

/// Offsets around the frame centre at every non-finite value: no panic, no
/// loop, and the sum comes back as itself (JSON cannot hold these values).
#[test]
fn non_finite_offsets_do_not_panic() {
    for (offset, want) in [
        (f64::INFINITY, f64::INFINITY),
        (f64::NEG_INFINITY, f64::NEG_INFINITY),
        (f64::NAN, f64::NAN),
    ] {
        let mut ls = UasDatalinkLs {
            frame_center_lat_deg: Some(10.0),
            frame_center_lon_deg: Some(170.0),
            ..UasDatalinkLs::default()
        };
        for p in [
            &mut ls.corner_lat_offset_p1_deg,
            &mut ls.corner_lat_offset_p2_deg,
            &mut ls.corner_lat_offset_p3_deg,
            &mut ls.corner_lat_offset_p4_deg,
        ] {
            *p = Some(0.0);
        }
        for p in [
            &mut ls.corner_lon_offset_p1_deg,
            &mut ls.corner_lon_offset_p2_deg,
            &mut ls.corner_lon_offset_p3_deg,
            &mut ls.corner_lon_offset_p4_deg,
        ] {
            *p = Some(offset);
        }
        let c = ls.corners().unwrap();
        for lon in [c.p1.1, c.p2.1, c.p3.1, c.p4.1] {
            if want.is_nan() {
                assert!(lon.is_nan(), "{offset}: {lon}");
            } else {
                assert_eq!(lon, want, "{offset}");
            }
        }
    }
}

/// A NaN in a preferred height is a value the record holds: the view takes
/// it, it does not fall back to a less preferred item.
#[test]
fn an_hae_item_holding_nan_is_taken_as_it_is() {
    let ls = UasDatalinkLs {
        sensor_lat_deg: Some(34.0),
        sensor_lon_deg: Some(-118.0),
        sensor_alt_m: Some(1500.0),
        sensor_ellipsoid_height_extended_m: Some(f64::NAN),
        ..UasDatalinkLs::default()
    };
    assert!(ls.sensor_position().unwrap().alt_m.is_nan());
    assert_eq!(ls.sensor_position_datum(), Some(AltitudeDatum::Hae));
}

/// The views and the encoder agree on what was sent as "no value": a view
/// of a record equals the view of a decode of the encoder's output, for
/// each way a record can hold a value, a sentinel or an IMAPB special.
/// The values are ones the items represent exactly.
#[test]
fn a_view_equals_the_view_after_an_encode_and_decode() {
    let base = UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        platform_heading_deg: Some(0.0),
        platform_pitch_deg: Some(0.0),
        platform_roll_deg: Some(0.0),
        platform_roll_full_deg: Some(0.0),
        sensor_lat_deg: Some(0.0),
        sensor_lon_deg: Some(0.0),
        sensor_alt_m: Some(-900.0),
        ..UasDatalinkLs::default()
    };
    let specials = [
        ImapbSpecial::BelowMin,
        ImapbSpecial::AboveMax,
        ImapbSpecial::PositiveInfinity,
        ImapbSpecial::NegativeInfinity,
        ImapbSpecial::PositiveQuietNaN { nan_id: 0 },
        ImapbSpecial::NegativeQuietNaN { nan_id: 1 },
        ImapbSpecial::PositiveSignalingNaN { signal: 1 },
        ImapbSpecial::NegativeSignalingNaN { signal: 2 },
        ImapbSpecial::UserDefined { signal: 3 },
    ];
    let mut cases: Vec<(&str, UasDatalinkLs)> = vec![
        (
            "Item 104 value only",
            UasDatalinkLs {
                sensor_ellipsoid_height_extended_m: Some(1471.0),
                ..base.clone()
            },
        ),
        (
            "Item 90 sentinel entry only",
            UasDatalinkLs {
                sentinel_tags: vec![90],
                ..base.clone()
            },
        ),
        (
            "Item 90 value with a redundant sentinel entry",
            UasDatalinkLs {
                platform_pitch_full_deg: Some(0.0),
                sentinel_tags: vec![90],
                ..base.clone()
            },
        ),
        (
            "Item 104 one step inside its minimum with a BelowMin entry",
            UasDatalinkLs {
                sensor_ellipsoid_height_extended_m: Some(-900.0 + 1.0 / 128.0),
                imapb_specials: vec![(104, ImapbSpecial::BelowMin)],
                ..base.clone()
            },
        ),
        (
            "Item 104 with two entries",
            UasDatalinkLs {
                imapb_specials: vec![(104, ImapbSpecial::BelowMin), (104, ImapbSpecial::AboveMax)],
                ..base.clone()
            },
        ),
        (
            "an IMAPB entry on a row that is not IMAPB",
            UasDatalinkLs {
                sensor_ellipsoid_height_m: Some(-900.0),
                imapb_specials: vec![(75, ImapbSpecial::BelowMin)],
                ..base.clone()
            },
        ),
        (
            "entries on an unknown tag and on a pack tag",
            UasDatalinkLs {
                sentinel_tags: vec![200, 48],
                imapb_specials: vec![(200, ImapbSpecial::BelowMin), (48, ImapbSpecial::AboveMax)],
                ..base.clone()
            },
        ),
    ];
    for s in specials {
        cases.push((
            "Item 104 unset with a special entry",
            UasDatalinkLs {
                imapb_specials: vec![(104, s)],
                ..base.clone()
            },
        ));
    }
    for (what, ls) in cases {
        let bytes = st0601::encode_to_vec(&ls).unwrap();
        let back = st0601::decode(&bytes).unwrap();
        assert_eq!(ls.sensor_position(), back.sensor_position(), "{what}");
        assert_eq!(
            ls.sensor_position_datum(),
            back.sensor_position_datum(),
            "{what}"
        );
        assert_eq!(ls.platform_attitude(), back.platform_attitude(), "{what}");
    }
}
