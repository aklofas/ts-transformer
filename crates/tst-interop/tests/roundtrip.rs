//! All-profile self-roundtrip: `gen` each canonical profile's synthetic
//! traffic to a temp `.ts` file, then `verify::verify_file` it against
//! that same profile's invariants — `verify_file` runs in `VerifyMode::Strict`
//! and now also runs the demuxer-independent wire oracles (`oracles.rs`)
//! against a raw-TS parse of the same bytes, not just the demuxed tallies.
//!
//! This is the zero-third-party-tools harness gate — every profile must
//! generate and verify clean here before any transport/tool cell
//! (later tasks) can be trusted to mean anything.

use tst_interop::fixtures::{AuSizeMode, KlvSet};
use tst_interop::verify::KlvExpect;
use tst_interop::{r#gen, profiles, verify};

/// Seconds of synthetic traffic per profile. Long enough to clear the
/// 70%-of-nominal count floors (`verify::NOMINAL_COUNT_SLACK`) with
/// margin, short enough to keep the suite fast.
const SECONDS: f64 = 3.0;

/// `pts-rollover`'s `start_pts_ticks` sits only 5s (450_000 ticks) of
/// 90 kHz clock below the 2^33 PES PTS wrap (see
/// `profiles::PTS_ROLLOVER_START`). The default [`SECONDS`] window ends
/// 2s short of the boundary, so it never actually exercises the wrap —
/// this profile alone runs long enough for its window to straddle it
/// (see `pts_rollover_run_window_straddles_the_2_33_wrap` below, which
/// guards this arithmetic against silent regression).
const PTS_ROLLOVER_SECONDS: f64 = 7.0;

/// `gen::run` profile `name` for `seconds` to a temp file, `verify_file`
/// it, delete the temp file, and assert the report passed.
fn assert_profile_roundtrips(name: &str, seconds: f64) {
    let p = profiles::by_name(name).unwrap_or_else(|| panic!("profile {name} must be registered"));

    let path = std::env::temp_dir().join(format!(
        "tst-interop-roundtrip-{name}-{}.ts",
        std::process::id()
    ));

    let result = r#gen::run(p, seconds, &path, KlvSet::Compact, 0, AuSizeMode::Compact)
        .and_then(|()| verify::verify_file(&path, p, seconds));
    let _ = std::fs::remove_file(&path);

    let report = result.unwrap_or_else(|e| panic!("{name}: gen/verify IO error: {e}"));
    assert!(
        report.pass,
        "{name}: verify failures: {:?}",
        report.failures
    );
}

#[test]
fn baseline_roundtrips() {
    assert_profile_roundtrips("baseline", SECONDS);
}

/// The same gate for `--klv-set rich`: a ~32-tag ST 0601 record with a
/// nested ST 0102 security set, whose tag set varies record to record on
/// a seeded schedule (`fixtures::rich_presence`). Rich records are an
/// order of magnitude larger than the compact ones every other test here
/// uses, so this is also the gate that the KLV PID's own PES packing
/// still carries a full record per PTS at the profile's 10 Hz cadence.
#[test]
fn baseline_rich_klv_roundtrips() {
    let p = profiles::by_name("baseline").expect("baseline profile must exist");
    let path = std::env::temp_dir().join(format!("tst-interop-rt-rich-{}.ts", std::process::id()));

    let result =
        r#gen::run(p, SECONDS, &path, KlvSet::Rich, 5, AuSizeMode::Compact).and_then(|()| {
            verify::verify_file_with(
                &path,
                p,
                SECONDS,
                KlvExpect {
                    set: KlvSet::Rich,
                    seed: 5,
                },
            )
        });
    let _ = std::fs::remove_file(&path);

    let report = result.expect("gen/verify IO error");
    assert!(report.pass, "rich verify failures: {:?}", report.failures);
    assert!(
        report.metrics.klv_rich.is_some(),
        "a rich-mode judgement must report a rich-KLV block"
    );
}

#[test]
fn klv_sync_roundtrips() {
    assert_profile_roundtrips("klv-sync", SECONDS);
}

#[test]
fn misp_roundtrips() {
    assert_profile_roundtrips("misp", SECONDS);
}

#[test]
fn h265_klv_roundtrips() {
    assert_profile_roundtrips("h265-klv", SECONDS);
}

#[test]
fn av1_klv_a_roundtrips() {
    assert_profile_roundtrips("av1-klv-a", SECONDS);
}

#[test]
fn av1_klv_b_roundtrips() {
    assert_profile_roundtrips("av1-klv-b", SECONDS);
}

#[test]
fn h266_klv_roundtrips() {
    assert_profile_roundtrips("h266-klv", SECONDS);
}

#[test]
fn audio_roundtrips() {
    assert_profile_roundtrips("audio", SECONDS);
}

#[test]
fn two_program_roundtrips() {
    assert_profile_roundtrips("two-program", SECONDS);
}

#[test]
fn pcr_tight_roundtrips() {
    assert_profile_roundtrips("pcr-tight", SECONDS);
}

#[test]
fn pcr_sparse_roundtrips() {
    assert_profile_roundtrips("pcr-sparse", SECONDS);
}

#[test]
fn pts_rollover_roundtrips() {
    assert_profile_roundtrips("pts-rollover", PTS_ROLLOVER_SECONDS);
}

/// The same all-profile gate at [`AuSizeMode::Realistic`]: GOP-structured
/// AU sizes (keyframes tens of KB, inter frames single-digit KB) instead of
/// the compact fixtures' tens of bytes. This is the size regime the matrix
/// runner now runs in, so every profile has to verify clean there too — a
/// keyframe spanning hundreds of TS packets exercises PES packetization
/// bursts, PCR catch-up insertion and the demuxer's reassembly path that
/// the one-or-two-packet compact AUs never reach.
///
/// The byte floor is the regime assert: 3s of compact traffic is a few tens
/// of KB, so anything above 300 KB proves the realistic payload actually
/// reached the wire rather than the call silently falling back to compact.
#[test]
fn every_profile_roundtrips_at_realistic_sizes() {
    for p in profiles::all() {
        let seconds = if p.name == "pts-rollover" {
            PTS_ROLLOVER_SECONDS
        } else {
            SECONDS
        };
        let path = std::env::temp_dir().join(format!(
            "tst-interop-rt-real-{}-{}.ts",
            p.name,
            std::process::id()
        ));

        let result = r#gen::run(p, seconds, &path, KlvSet::Compact, 0, AuSizeMode::Realistic)
            .and_then(|()| verify::verify_file(&path, p, seconds));
        let _ = std::fs::remove_file(&path);

        let report = result.unwrap_or_else(|e| panic!("{}: gen/verify IO error: {e}", p.name));
        assert!(
            report.pass,
            "{}: realistic-size verify failures: {:?}",
            p.name, report.failures
        );
        assert!(
            report.metrics.bytes > 300_000,
            "{}: realistic mode must change the size regime, got {} bytes",
            p.name,
            report.metrics.bytes
        );
    }
}

/// Drift guard: every profile in the registry must have a dedicated test
/// above. Without this, a renamed/added/removed profile would silently
/// under-cover (or over-claim) the roundtrip gate instead of failing loudly.
#[test]
fn every_registered_profile_has_a_dedicated_roundtrip_test() {
    let covered = [
        "baseline",
        "klv-sync",
        "misp",
        "h265-klv",
        "av1-klv-a",
        "av1-klv-b",
        "h266-klv",
        "audio",
        "two-program",
        "pcr-tight",
        "pcr-sparse",
        "pts-rollover",
    ];
    let mut expected: Vec<&str> = profiles::all().iter().map(|p| p.name).collect();
    expected.sort_unstable();
    let mut covered_sorted = covered.to_vec();
    covered_sorted.sort_unstable();
    assert_eq!(
        covered_sorted, expected,
        "roundtrip.rs must have exactly one #[test] per registered profile"
    );
}

/// Arithmetic guard for [`PTS_ROLLOVER_SECONDS`]: if `pts-rollover`'s
/// `start_pts_ticks` ever moves (e.g. a profile-registry edit) such that
/// [`PTS_ROLLOVER_SECONDS`] of traffic no longer reaches the 2^33 PES PTS
/// wrap, this fails loudly instead of letting `pts_rollover_roundtrips`
/// silently stop exercising the wrap it exists to test.
#[test]
fn pts_rollover_run_window_straddles_the_2_33_wrap() {
    let p = profiles::by_name("pts-rollover").expect("pts-rollover profile must exist");
    const PTS_HZ: u64 = 90_000; // the MPEG-TS PTS clock, ITU-T H.222.0 V9 §2.4.3.6
    let duration_ticks = (PTS_ROLLOVER_SECONDS * PTS_HZ as f64).round() as u64;
    let wrap = 1u64 << 33;
    assert!(
        p.start_pts_ticks + duration_ticks > wrap,
        "pts-rollover's {PTS_ROLLOVER_SECONDS}s roundtrip window (start {} + {duration_ticks} ticks = {}) must cross the 2^33 ({wrap}) PTS wrap",
        p.start_pts_ticks,
        p.start_pts_ticks + duration_ticks,
    );
}
