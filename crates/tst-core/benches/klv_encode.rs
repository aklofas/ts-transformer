//! Encode-side guard for the ST 0601 table loops: a populated record
//! (about twenty common scalar fields, the 14 IMAPB extended-range fields
//! and the common ranged ones set to mid-range values, the packs left
//! empty) run through `encode_to_vec` and `encoded_len`.
//!
//! Run: `cargo bench -p tst-core --bench klv_encode`.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use tst_core::klv::st0601::{self, UasDatalinkLs};

/// A record with the identity strings, the time, the platform attitude, the
/// sensor position and field of view, and all 14 IMAPB fields set.
fn populated() -> UasDatalinkLs {
    UasDatalinkLs {
        timestamp_us: Some(1_700_000_000_000_000),
        mission_id: Some("MISSION".into()),
        platform_tail_number: Some("N12345".into()),
        platform_designation: Some("TestAircraft".into()),
        image_source_sensor: Some("EO".into()),
        platform_heading_deg: Some(180.0),
        platform_pitch_deg: Some(-2.5),
        platform_roll_deg: Some(0.5),
        sensor_lat_deg: Some(33.5),
        sensor_lon_deg: Some(-112.0),
        sensor_alt_m: Some(1500.0),
        sensor_hfov_deg: Some(15.0),
        sensor_vfov_deg: Some(8.5),
        slant_range_m: Some(2000.0),
        frame_center_lat_deg: Some(33.49),
        frame_center_lon_deg: Some(-111.99),
        target_width_extended_m: Some(750_000.0),
        density_altitude_extended_m: Some(1000.0),
        sensor_ellipsoid_height_extended_m: Some(1500.0),
        alternate_platform_ellipsoid_height_extended_m: Some(1400.0),
        range_to_recovery_km: Some(10_000.0),
        platform_course_angle_deg: Some(180.0),
        altitude_agl_m: Some(1200.0),
        radar_altimeter_m: Some(1190.0),
        sensor_azimuth_rate_dps: Some(10.0),
        sensor_elevation_rate_dps: Some(-5.0),
        sensor_roll_rate_dps: Some(1.0),
        mi_storage_percent_full: Some(50.0),
        transmission_frequency_mhz: Some(2400.0),
        zoom_percentage: Some(50.0),
        ..UasDatalinkLs::default()
    }
}

fn bench_encode(c: &mut Criterion) {
    let record = populated();
    c.bench_function("st0601_encode_full", |b| {
        b.iter(|| st0601::encode_to_vec(black_box(&record)).expect("encode"))
    });
    c.bench_function("st0601_encoded_len", |b| {
        b.iter(|| st0601::encoded_len(black_box(&record)))
    });
}

criterion_group!(benches, bench_encode);
criterion_main!(benches);
