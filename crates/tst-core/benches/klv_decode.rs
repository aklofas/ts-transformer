//! Decode benches for the KLV substrate + typed local sets.
//!
//! Three groups:
//! - st0601_lenient — typical UAS LS payload (~120 bytes, mixed tags).
//! - st0102_lenient — typical Security LS payload (~80 bytes).
//! - st0903_lenient — typical VMTI LS payload (~250 bytes, 3 targets).
//!
//! These benches are regression detectors: they record current decode
//! throughput so that refactors can be verified to not degrade hot paths.
//!
//! Run: `cargo bench -p tst-core --bench klv_decode`.
//! Quick mode (shorter warmup): add `-- --quick` at the end.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use tst_core::klv::st0102::{
    ClassifyingCountryCodingMethod, ObjectCountryCodingMethod, SecurityClassification,
};
use tst_core::klv::st0601::UasDatalinkLs;
use tst_core::klv::st0903::{VTargetPack, VmtiLs};
use tst_core::klv::{st0102, st0601, st0903};

/// Build a synthetic ST 0601 UAS Datalink LS with typical platform + sensor
/// fields populated. Encodes to ~120–140 bytes depending on field widths.
///
/// Field names follow `UasDatalinkLs` (the actual struct name in
/// `klv::st0601`): `timestamp_us`, `platform_heading_deg`, `sensor_lat_deg`,
/// etc. The draft plan used a hypothetical method-encode shape; the real API
/// uses free functions `st0601::encode_to_vec(&record)`.
fn synthetic_st0601() -> Vec<u8> {
    let record = UasDatalinkLs {
        // Identity
        platform_designation: Some("TestAircraft".into()),
        image_source_sensor: Some("EO".into()),
        // Time — microseconds since Unix epoch (2023-11-14 22:13:20 UTC)
        timestamp_us: Some(1_700_000_000_000_000),
        // Platform attitude
        platform_heading_deg: Some(180.0),
        platform_pitch_deg: Some(-2.5),
        platform_roll_deg: Some(0.5),
        // Sensor position + FOV
        sensor_lat_deg: Some(33.5),
        sensor_lon_deg: Some(-112.0),
        sensor_alt_m: Some(1500.0),
        sensor_hfov_deg: Some(15.0),
        sensor_vfov_deg: Some(8.5),
        // Slant range + frame center
        slant_range_m: Some(2000.0),
        frame_center_lat_deg: Some(33.49),
        frame_center_lon_deg: Some(-111.99),
        ..UasDatalinkLs::default()
    };
    st0601::encode_to_vec(&record).expect("encode st0601")
}

/// Build a synthetic ST 0102 Security LS with the 5 required fields.
/// Encodes to ~60–80 bytes (UTF-16 country strings add overhead).
///
/// The draft used integer coding-method fields; the real API uses typed
/// enums `ClassifyingCountryCodingMethod` and `ObjectCountryCodingMethod`,
/// and the object-countries field is `object_country_codes` (not
/// `object_countries`). Encode via `st0102::encode_to_vec(&record)`.
fn synthetic_st0102() -> Vec<u8> {
    let record = st0102::SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        classifying_country_coding_method: Some(ClassifyingCountryCodingMethod::Iso3166TwoLetter),
        classifying_country: Some("//US".into()),
        object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166TwoLetter),
        object_country_codes: Some("US".into()),
        version: Some(12),
        ..Default::default()
    };
    st0102::encode_to_vec(&record).expect("encode st0102")
}

/// Build a synthetic ST 0903 VMTI LS with 3 minimal targets.
/// Encodes to ~200–260 bytes depending on per-target field counts.
///
/// The draft used `vmti_version`/`frame_number`/`target_series`; the
/// real struct uses `version_number`/`total_targets_in_frame`/`targets`.
/// Encode via `st0903::encode_to_vec(&ls)`.
fn synthetic_st0903() -> Vec<u8> {
    let ls = VmtiLs {
        version_number: Some(6),
        total_targets_in_frame: Some(3),
        num_targets_reported: Some(3),
        frame_width: Some(1920),
        frame_height: Some(1080),
        targets: vec![
            VTargetPack {
                target_id: 1,
                centroid_pix_row: Some(540),
                centroid_pix_col: Some(960),
                priority: Some(1),
                confidence_level: Some(95),
                ..Default::default()
            },
            VTargetPack {
                target_id: 2,
                centroid_pix_row: Some(300),
                centroid_pix_col: Some(400),
                priority: Some(2),
                confidence_level: Some(80),
                ..Default::default()
            },
            VTargetPack {
                target_id: 3,
                centroid_pix_row: Some(700),
                centroid_pix_col: Some(1500),
                priority: Some(3),
                confidence_level: Some(70),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    st0903::encode_to_vec(&ls).expect("encode st0903")
}

fn bench_st0601_lenient(c: &mut Criterion) {
    let payload = synthetic_st0601();
    c.bench_function("st0601_lenient_decode", |b| {
        b.iter(|| {
            let ls = st0601::decode(black_box(&payload)).expect("decode");
            let _ = black_box(ls);
        })
    });
}

fn bench_st0102_lenient(c: &mut Criterion) {
    let payload = synthetic_st0102();
    c.bench_function("st0102_lenient_decode", |b| {
        b.iter(|| {
            let ls = st0102::decode(black_box(&payload)).expect("decode");
            let _ = black_box(ls);
        })
    });
}

fn bench_st0903_lenient(c: &mut Criterion) {
    let payload = synthetic_st0903();
    c.bench_function("st0903_lenient_decode", |b| {
        b.iter(|| {
            let ls = st0903::decode(black_box(&payload)).expect("decode");
            let _ = black_box(ls);
        })
    });
}

criterion_group!(
    benches,
    bench_st0601_lenient,
    bench_st0102_lenient,
    bench_st0903_lenient
);
criterion_main!(benches);
