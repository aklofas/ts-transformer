//! Per-commit smoke test for the PTS-rollover and PCR-jitter tools.
//!
//! Both binaries already exist at crates/tst-core/tests/tools/ as
//! [[bin]] targets for release-validation steps 8/9. Without this test
//! they are unreachable from PR CI — a refactor breaking either main()
//! would only surface at release-tag time.
//!
//! Cargo injects each [[bin]]'s built path into CARGO_BIN_EXE_<name>
//! for integration tests in the same package, so no recursive
//! `cargo run` and no PATH lookup are needed.
//!
//! The test chains the two binaries: gen-pts-rollover-fixture writes
//! a tmp .ts, measure-pcr-jitter reads it and asserts cadence.
//! Chaining is required because measure-pcr-jitter exits non-zero
//! on median > 67ms or p95 > 100ms — running it on an arbitrary
//! checked-in fixture would be flaky. The muxer-generated output is
//! guaranteed cadence-clean.

use std::process::Command;

#[test]
fn pts_rollover_fixture_passes_pcr_jitter_threshold() {
    let tmp = std::env::temp_dir().join("pts_rollover_smoke.ts");
    if tmp.exists() {
        std::fs::remove_file(&tmp).expect("clear stale tmp file");
    }

    let gen_bin = env!("CARGO_BIN_EXE_gen-pts-rollover-fixture");
    let gen_status = Command::new(gen_bin)
        .arg(tmp.to_str().expect("utf-8 tmp path"))
        // The generator's initial PTS is 5s before the 33-bit rollover boundary.
        // 8s of stream straddles the boundary by ~3s post-wrap — enough to
        // exercise the wrap path on both sides without bloating the fixture.
        .arg("8")
        .status()
        .expect("spawn gen-pts-rollover-fixture");

    assert!(
        gen_status.success(),
        "gen-pts-rollover-fixture exited non-zero: {gen_status:?}",
    );
    let bytes = std::fs::metadata(&tmp).expect("output exists").len();
    assert!(bytes > 0, "fixture is empty");
    assert_eq!(
        bytes % 188,
        0,
        "fixture is not 188-byte-aligned ({bytes} bytes)"
    );

    let measure_bin = env!("CARGO_BIN_EXE_measure-pcr-jitter");
    let measure_output = Command::new(measure_bin)
        .arg(&tmp)
        .output()
        .expect("spawn measure-pcr-jitter");

    let cleanup = || {
        std::fs::remove_file(&tmp).ok();
    };

    if !measure_output.status.success() {
        cleanup();
        panic!(
            "measure-pcr-jitter exited non-zero on muxer output: {:?}\nstdout: {}\nstderr: {}",
            measure_output.status,
            String::from_utf8_lossy(&measure_output.stdout),
            String::from_utf8_lossy(&measure_output.stderr),
        );
    }
    assert!(
        !measure_output.stdout.is_empty(),
        "measure-pcr-jitter produced no stdout",
    );

    cleanup();
}
