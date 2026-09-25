//! Corruption tap round trips + mutation tests (spec §4.4).
//!
//! Everything here is offline and in-memory: `gen` a profile to a
//! temporary file, push those bytes through a [`Corrupter`] over
//! `corrupt::testing::VecTransport`, then judge the resulting WIRE bytes
//! with [`verify_bytes_with_corruption`] under the tap's own log. No
//! socket is opened, so this binary deliberately stays out of nextest's
//! serialised `network` group — which is also why no test here is named
//! `*round_trip*` / `*loopback*` / `*pairing*` / etc., all of which that
//! group's filter matches by SUBSTRING (`.config/nextest.toml`).
//!
//! # Packet arithmetic
//!
//! The compact baseline profile muxes 60 TS packets per second (see
//! `rawts`'s own 180-packets-in-3 s assertion), so [`SECONDS`] = 30 gives
//! **1800 packets**. With `rate=10000` (every eligible packet) and the
//! `min_gap` floor of 1000, a forced single-class run injects at packet 0
//! and again at packet ~1000: two injections, each with its full
//! 600-packet recovery window inside the capture, so both are judged for
//! detection AND for recovery. That last part is what makes these tests
//! bite — `Attribution::finish` declines to judge recovery for an
//! injection whose window runs off the end of the capture, so a shorter
//! stream would pass vacuously.
//!
//! Packet 0 of a fresh mux is the PAT, so every forced run's FIRST
//! injection lands on PSI. That is deliberate coverage, not an accident:
//! several classes are invisible there by design (a lost PAT repetition
//! is covered by the next one) and the tap must not claim otherwise.

use std::sync::{Arc, Mutex};
use tst_core::transport::Transport;
use tst_interop::corrupt::{
    ATTRIBUTION_WINDOW, AttributionReport, Class, Corrupter, Injection, LogHeader, RECOVERY_BOUND,
    parse_corrupt, parse_log,
    testing::{VecTransport, VecWriter},
};
use tst_interop::fixtures::{AuSizeMode, KlvSet};
use tst_interop::profiles::Profile;
use tst_interop::report_types::VerifyReport;
use tst_interop::verify::{KlvExpect, VerifyMode, verify_bytes_with_corruption};
use tst_interop::{r#gen, profiles};

/// 30 s of the compact baseline = 1800 packets. See the module doc.
const SECONDS: f64 = 30.0;

/// The seed the per-class positive controls and their mutations share.
const SEED: u64 = 11;

/// A seed whose forced `body_flip` run puts at least one flip inside the
/// PAT's CRC-covered section body. Most seeds land in the ~167 bytes of
/// 0xFF stuffing that follow a 17-byte PAT section instead, where a flip
/// is invisible and `detectable` is correctly `false`.
///
/// Re-pinned from 15 when PSI body flips stopped drawing from the section
/// HEADER (pointer field / table_id / section_length — bytes a decoder
/// rejects pre-CRC and silently, so a flip there can never be noticed).
/// Narrowing the draw range changes which offsets every seed produces on
/// a PSI packet, so the old pin no longer lands under a CRC. Nothing
/// about the property under test changed.
const BODY_FLIP_PSI_SEED: u64 = 19;

fn baseline() -> &'static Profile {
    profiles::by_name("baseline").expect("the baseline profile exists")
}

/// `gen` `p` for [`SECONDS`] and return the bytes. `tag` only has to make
/// the temporary path unique within this process.
fn gen_bytes(p: &Profile, tag: &str) -> Vec<u8> {
    let path = std::env::temp_dir().join(format!(
        "tst-interop-corruption-{tag}-{}.ts",
        std::process::id()
    ));
    r#gen::run(p, SECONDS, &path, KlvSet::Compact, 0, AuSizeMode::Compact).expect("gen::run");
    let bytes = std::fs::read(&path).expect("read the generated capture");
    let _ = std::fs::remove_file(&path);
    bytes
}

/// Push `bytes` through a tap configured by `spec`/`seed` in 1316-byte
/// pushes (7 × 188 — the classic SRT/UDP payload), and return what went on
/// the wire alongside the log the tap wrote.
fn tap(bytes: &[u8], spec: &str, seed: u64) -> (Vec<u8>, LogHeader, Vec<Injection>) {
    let wire = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut tap = Corrupter::new(
        VecTransport(Arc::clone(&wire)),
        parse_corrupt(spec, seed).expect("the spec parses"),
        Box::new(VecWriter(Arc::clone(&log))),
    )
    .expect("the tap config validates");
    for chunk in bytes.chunks(1316) {
        tap.send_bytes(chunk).expect("VecTransport never fails");
    }
    drop(tap);
    let text = String::from_utf8(log.lock().expect("log mutex").clone()).expect("the log is UTF-8");
    let (header, injections) = parse_log(&text).expect("the tap's own log parses");
    let wire = wire.lock().expect("wire mutex").clone();
    (wire, header, injections)
}

fn judge(wire: &[u8], p: &Profile, log: Option<&(LogHeader, Vec<Injection>)>) -> VerifyReport {
    judge_in(VerifyMode::Lossy, wire, p, log)
}

fn judge_in(
    mode: VerifyMode,
    wire: &[u8],
    p: &Profile,
    log: Option<&(LogHeader, Vec<Injection>)>,
) -> VerifyReport {
    verify_bytes_with_corruption(wire, p, SECONDS, mode, KlvExpect::compact(), log)
}

/// Every positive control in this file judges an OFFLINE byte buffer, in
/// `VerifyMode::Lossy` only because that is the mode a live soak leg uses.
/// There is no transport between `tap` and `judge` here, so nothing can be
/// lost in transit and the Lossy transport-loss excusal
/// (`Attribution::lossy`) must stay completely inert.
///
/// Without this, that excusal could quietly hollow the whole suite out:
/// `undetected`/`unrecovered`/`unexplained_events` would keep coming back
/// empty while real findings drained into the `*_lost` counters instead,
/// and every assertion above would still pass.
fn assert_transport_loss_excusal_is_inert(a: &AttributionReport, ctx: &str) {
    assert_eq!(a.undetected_lost, 0, "{ctx}: offline capture, {a:?}");
    assert_eq!(a.unrecovered_lost, 0, "{ctx}: offline capture, {a:?}");
    assert_eq!(
        a.unexplained_transport_loss, 0,
        "{ctx}: offline capture, {a:?}"
    );
}

/// Force `class` on every eligible packet, at the `min_gap` floor.
fn forced(class: Class) -> String {
    format!("rate=10000,min_gap=1000,classes={}", class.name())
}

/// Compact `(ordinal, pid, offsets, detectable)` rendering for a failure
/// message — a failing seed is only actionable if it says what it drew.
fn shape(injections: &[Injection]) -> Vec<(u64, u16, Vec<usize>, bool)> {
    injections
        .iter()
        .map(|i| (i.ordinal, i.pid, i.offsets.clone(), i.detectable))
        .collect()
}

fn assert_failure_starting_with(r: &VerifyReport, prefix: &str) {
    assert!(
        r.failures.iter().any(|f| f.starts_with(prefix)),
        "expected a {prefix} failure, got {:?}",
        r.failures
    );
}

/// Pin the run out of the VACUOUS REGIME before judging it.
///
/// `Attribution::finish` declines to judge recovery for an injection whose
/// window runs past the end of the capture — correctly, since absent
/// evidence is not evidence of a failure. But that means a capture which
/// shortened for any reason (a change to [`SECONDS`], to a profile's
/// packet rate, or to `min_gap`) would silently turn every
/// `unrecovered.is_empty()` assertion, and the recovery half of every
/// `r.pass`, into a tautology — the suite would still be green while
/// testing nothing. So pin both halves explicitly: more than one injection
/// landed, and the first one's whole recovery window lies inside the
/// capture.
fn assert_recovery_is_judged(wire: &[u8], injections: &[Injection], what: &str) {
    // Floor: truncation shortens the wire and inserted garbage lengthens
    // it, so this is a proxy for the reader's packet count, not an
    // identity. Being a floor is what makes it safe here.
    let packets = (wire.len() / 188) as u64;
    assert!(
        injections.len() >= 2,
        "{what}: only {} injection(s) landed in {packets} packets — at least two are \
         needed for the sweep to mean anything",
        injections.len()
    );
    assert!(
        injections[0].ordinal + RECOVERY_BOUND <= packets,
        "{what}: the injection at packet {} plus the {RECOVERY_BOUND}-packet recovery \
         bound runs past this {packets}-packet capture, so recovery is not judged at all",
        injections[0].ordinal
    );
}

// ============================================================
// Positive controls: every class, judged under its own log
// ============================================================

/// One class, forced, judged against the log the tap wrote. Everything
/// the attribution can say must come out clean: every injection placed,
/// every event it caused explained, every detectable one noticed, and the
/// stream producing media again inside the recovery bound.
fn class_is_attributed(class: Class) {
    let p = baseline();
    let bytes = gen_bytes(p, class.name());
    let (wire, header, injections) = tap(&bytes, &forced(class), SEED);
    assert_recovery_is_judged(&wire, &injections, &format!("{class:?}"));
    let detectable = injections.iter().filter(|i| i.detectable).count();

    let r = judge(&wire, p, Some(&(header, injections.clone())));
    assert!(r.pass, "{class:?}: {:?}", r.failures);
    let a = r
        .metrics
        .corruption_attribution
        .expect("a judged capture carries its attribution report");
    assert_eq!(a.injected, injections.len() as u64, "{class:?}: {a:?}");
    assert_eq!(a.unresolved, 0, "{class:?}: every injection places: {a:?}");
    assert!(a.undetected.is_empty(), "{class:?}: {:?}", a.undetected);
    assert!(a.unrecovered.is_empty(), "{class:?}: {:?}", a.unrecovered);
    assert!(
        a.unexplained_events.is_empty(),
        "{class:?}: {:?}",
        a.unexplained_events
    );
    assert_transport_loss_excusal_is_inert(&a, &format!("{class:?}"));
    // A class with a detectable injection must have produced evidence, or
    // "nothing went wrong" and "nothing happened" would look the same.
    if detectable > 0 {
        assert!(
            a.attributed_events > 0,
            "{class:?}: {detectable} detectable injection(s) but no attributed event: {a:?}"
        );
    }
}

#[test]
fn body_flip_injections_are_attributed() {
    class_is_attributed(Class::BodyFlip);
}
#[test]
fn header_injections_are_attributed() {
    class_is_attributed(Class::Header);
}
#[test]
fn truncate_injections_are_attributed() {
    class_is_attributed(Class::Truncate);
}
#[test]
fn garbage_injections_are_attributed() {
    class_is_attributed(Class::Garbage);
}
#[test]
fn drop_injections_are_attributed() {
    class_is_attributed(Class::Drop);
}
#[test]
fn dup_injections_are_attributed() {
    class_is_attributed(Class::Dup);
}
#[test]
fn psi_flip_injections_are_attributed() {
    class_is_attributed(Class::PsiFlip);
}

/// A body flip is only `detectable` when it lands under a section CRC,
/// and at [`SEED`] neither of the two flips does — so the control above
/// exercises only the silent half of the class. This one picks a seed
/// whose first flip DOES land in the PAT's section body, so the
/// CRC-mismatch path is covered too.
#[test]
fn a_body_flip_under_a_section_crc_is_noticed() {
    let p = baseline();
    let bytes = gen_bytes(p, "bf-psi");
    let (wire, header, injections) = tap(&bytes, &forced(Class::BodyFlip), BODY_FLIP_PSI_SEED);
    assert!(
        injections.iter().any(|i| i.detectable && i.psi),
        "seed {BODY_FLIP_PSI_SEED} must land a flip under a section CRC: {:?}",
        shape(&injections)
    );
    let r = judge(&wire, p, Some(&(header, injections)));
    assert!(r.pass, "{:?}", r.failures);
    let a = r.metrics.corruption_attribution.expect("attribution");
    assert!(a.undetected.is_empty(), "{:?}", a.undetected);
    assert_transport_loss_excusal_is_inert(&a, "body_flip under a CRC");
    assert!(
        a.attributed_nonconformant > 0,
        "a flip under a CRC must surface as a non-conformance: {a:?}"
    );
}

/// F3 probe (soak run 3, 2026-09-17: six unexplained `PsiChecksum` on the
/// rist leg's PMT PID, one per leg on the PAT). `expects()` lists no
/// `PsiChecksum` for `Header | Truncate | Garbage`; this measures, through
/// the real demuxer, what a receiver reports for framing damage on or
/// beside a PSI packet, and the arm follows the measurement — never the
/// reasoning (the file's own convention for every `expects` arm).
fn psi_framing_shapes(bytes: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
    let pmt_at = bytes
        .chunks_exact(188)
        .position(|pk| {
            let pk: &[u8; 188] = pk.try_into().expect("188-byte chunk");
            tst_interop::rawts::classify_packet(pk).is_ok_and(|i| i.pid == 0x1000 && i.pusi)
        })
        .expect("the generator emits a PMT at the head of the capture");
    let (before, pmt, after) = (
        &bytes[..pmt_at * 188],
        &bytes[pmt_at * 188..(pmt_at + 1) * 188],
        &bytes[(pmt_at + 1) * 188..],
    );
    let mut garbage = vec![0x55u8; 100];
    garbage[30] = 0x47; // a false sync candidate inside the run
    let prev_at = pmt_at.checked_sub(1).expect("a PAT precedes the PMT");
    vec![
        (
            "PMT truncated to 60 bytes",
            [before, &pmt[..60], after].concat(),
        ),
        (
            "100-byte garbage run after the PMT",
            [before, pmt, &garbage, after].concat(),
        ),
        (
            "packet before the PMT truncated to 60 bytes",
            [
                &bytes[..prev_at * 188],
                &bytes[prev_at * 188..prev_at * 188 + 60],
                pmt,
                after,
            ]
            .concat(),
        ),
    ]
}

#[test]
fn framing_damage_on_a_psi_packet_reports_what_expects_says_it_does() {
    let p = baseline();
    let bytes = gen_bytes(p, "psi-framing");
    // An EMPTY log with a real header: every event the demuxer reports is
    // then an unexplained sample whose text starts with the signal's name
    // (`{sig:?} on pid …`), which is a stabler thing to read than the
    // issue's `Display` text in the `nonconformant_event` failure.
    let empty_log = (
        LogHeader {
            tap_version: 1,
            seed: 1,
            rate_per_10k: 5,
            min_gap: 1000,
            classes: Class::ALL.to_vec(),
            attribution_window: ATTRIBUTION_WINDOW,
            recovery_bound: RECOVERY_BOUND,
        },
        Vec::<Injection>::new(),
    );
    let mut observed = Vec::new();
    for (name, wire) in psi_framing_shapes(&bytes) {
        let r = verify_bytes_with_corruption(
            &wire,
            p,
            SECONDS,
            VerifyMode::Strict,
            KlvExpect::compact(),
            Some(&empty_log),
        );
        let a = r
            .metrics
            .corruption_attribution
            .expect("an attribution was attached");
        let psi_checksum = a.unexplained_events.iter().any(|e| {
            e.starts_with("PsiChecksum on pid Some(4096)")
                || e.starts_with("PsiChecksum on pid Some(0)")
        });
        eprintln!(
            "{name}: nonconformant={} discontinuities={} resyncs={} unexplained={:?}",
            r.metrics.nonconformant, r.metrics.discontinuities, a.resyncs, a.unexplained_events
        );
        observed.push((name, psi_checksum));
    }
    assert!(
        observed.iter().all(|&(_, psi_checksum)| !psi_checksum),
        "measured 2026-09-25: no framing shape on a PSI packet surfaces as PsiChecksum — the \
         demuxer re-syncs past a damaged section without parsing it. If this ever flips, \
         expects()'s framing arm must learn PsiChecksum: {observed:?}"
    );
}

/// Every class at once, on every profile. `pts-rollover` needs a capture
/// longer than its 7 s wrap window, which [`SECONDS`] clears.
#[test]
fn every_class_on_every_profile_is_attributed() {
    for p in profiles::all() {
        let bytes = gen_bytes(p, p.name);
        let (wire, header, injections) = tap(&bytes, "rate=300,min_gap=1000", 3);
        assert_recovery_is_judged(&wire, &injections, p.name);
        let r = judge(&wire, p, Some(&(header, injections.clone())));
        assert!(
            r.pass,
            "{}: {:?} for {:?}",
            p.name,
            r.failures,
            shape(&injections)
        );
    }
}

/// The per-class controls above each pin ONE seed. This sweeps the whole
/// class set across many, so a verdict that only holds for a lucky draw
/// cannot pass: every seed's report must come out clean.
#[test]
fn every_class_is_attributed_across_a_seed_sweep() {
    let p = baseline();
    let bytes = gen_bytes(p, "sweep");
    let mut runs = 0;
    for class in Class::ALL {
        for seed in 1..=24u64 {
            let (wire, header, injections) = tap(&bytes, &forced(class), seed);
            assert_recovery_is_judged(&wire, &injections, &format!("{class:?} seed {seed}"));
            let r = judge(&wire, p, Some(&(header, injections.clone())));
            assert!(
                r.pass,
                "{class:?} seed {seed}: {:?} for {:?}",
                r.failures,
                shape(&injections)
            );
            runs += 1;
        }
    }
    assert_eq!(runs, Class::ALL.len() * 24);
}

/// The `header` class draws one of four sub-kinds on a media PID (kill
/// the sync byte / rewrite the PID to 0x1FFE / jump the continuity
/// counter / overrun the adaptation-field length). Each is a different
/// promise about what a receiver must notice, so each needs its own
/// passing evidence — a sweep that only ever drew one of them would pin
/// nothing about the other three.
///
/// PAT/PMT injections are excluded from the shape census because the
/// class collapses to the sync byte alone there (the other three are
/// invisible on PSI); `corrupt.rs`'s own
/// `header_class_on_a_psi_packet_only_ever_kills_the_sync_byte` covers
/// that half.
#[test]
fn header_every_media_sub_kind_is_attributed() {
    let p = baseline();
    let bytes = gen_bytes(p, "hdr-subkinds");
    let mut shapes: std::collections::BTreeMap<Vec<usize>, u32> = Default::default();
    for seed in 1..=64u64 {
        let (wire, header, injections) = tap(&bytes, &forced(Class::Header), seed);
        assert_recovery_is_judged(&wire, &injections, &format!("seed {seed}"));
        for i in injections.iter().filter(|i| !matches!(i.pid, 0 | 0x1000)) {
            *shapes.entry(i.offsets.clone()).or_default() += 1;
        }
        let r = judge(&wire, p, Some(&(header, injections.clone())));
        assert!(
            r.pass,
            "seed {seed}: {:?} for {:?}",
            r.failures,
            shape(&injections)
        );
    }
    // Offsets identify the sub-kind: [0] sync byte, [1, 2] PID,
    // [3] continuity counter, [4] adaptation_field_length.
    for want in [vec![0usize], vec![1, 2], vec![3], vec![4]] {
        assert!(
            shapes.get(&want).is_some_and(|&n| n > 0),
            "media sub-kind {want:?} never drawn across 64 seeds: {shapes:?}"
        );
    }
}

// ============================================================
// Mutation (a): withhold an injection's log line
// ============================================================

/// Delete one DETECTABLE injection's line from the log and the events it
/// caused have nothing to explain them, so the report must say so.
///
/// Withholding an INVISIBLE injection would leave nothing unexplained and
/// the mutation would pass vacuously — hence `position(detectable)` rather
/// than `remove(0)` (the forced first injection lands on the PAT, where
/// several classes are invisible), and hence `dup`'s separate test below.
fn withholding_a_log_line_is_caught(class: Class) {
    withholding_a_log_line_is_caught_in(VerifyMode::Lossy, class);
}

fn withholding_a_log_line_is_caught_in(mode: VerifyMode, class: Class) {
    let p = baseline();
    let bytes = gen_bytes(p, &format!("wh-{}", class.name()));
    let (wire, header, mut injections) = tap(&bytes, &forced(class), SEED);
    let at = injections
        .iter()
        .position(|i| i.detectable)
        .unwrap_or_else(|| panic!("{class:?}: no detectable injection to withhold"));
    injections.remove(at);

    let r = judge_in(mode, &wire, p, Some(&(header, injections)));
    assert_failure_starting_with(&r, "corruption_attributed");
}

#[test]
fn withheld_header_line_is_unexplained() {
    withholding_a_log_line_is_caught(Class::Header);
}
#[test]
fn withheld_truncate_line_is_unexplained() {
    withholding_a_log_line_is_caught(Class::Truncate);
}
#[test]
fn withheld_garbage_line_is_unexplained() {
    withholding_a_log_line_is_caught(Class::Garbage);
}
/// `Drop` is judged in STRICT mode, alone among the classes here, and the
/// reason is a real property rather than a test convenience.
///
/// A dropped packet's only observable is a continuity jump. In
/// `VerifyMode::Lossy` — the mode a LIVE capture is judged in — an
/// unexplained continuity jump is not corruption evidence at all, because
/// the transport itself loses packets and nothing distinguishes a gap the
/// tap made and hid from a gap the network made
/// (`Attribution::lossy`). So on a live lossy leg a withheld `drop`
/// line is, by construction, not catchable, and asserting otherwise here
/// would assert something the engine cannot deliver.
///
/// Strict is where the claim holds and where it matters: an offline file
/// has no transport to lose anything, so the jump is unexplained and the
/// report says so. Every other class survives Lossy unchanged — they
/// surface as resyncs or non-conformances, which no packet loss can forge.
#[test]
fn withheld_drop_line_is_unexplained_in_strict_mode() {
    withholding_a_log_line_is_caught_in(VerifyMode::Strict, Class::Drop);
}

/// The Lossy half of the same statement, asserted rather than left
/// implicit: the identical withheld `drop` line produces NO
/// `corruption_attributed` failure once transport loss is on the table,
/// and the jump is still counted as a discontinuity.
#[test]
fn withheld_drop_line_is_excused_as_transport_loss_in_lossy_mode() {
    let p = baseline();
    let bytes = gen_bytes(p, "wh-drop-lossy");
    let (wire, header, mut injections) = tap(&bytes, &forced(Class::Drop), SEED);
    let at = injections
        .iter()
        .position(|i| i.detectable)
        .expect("drop injections are detectable");
    injections.remove(at);

    let r = judge(&wire, p, Some(&(header, injections)));
    assert!(
        !r.failures
            .iter()
            .any(|f| f.starts_with("corruption_attributed")),
        "{:?}",
        r.failures
    );
    let a = r.metrics.corruption_attribution.expect("attribution");
    assert!(
        a.unexplained_transport_loss > 0,
        "the withheld drop's jump must be recorded as excused, not vanish: {a:?}"
    );
    assert!(
        r.metrics.discontinuities > 0,
        "and still counted as a discontinuity"
    );
}
#[test]
fn withheld_psi_flip_line_is_unexplained() {
    withholding_a_log_line_is_caught(Class::PsiFlip);
}

/// A body flip under a section CRC is the only flip a receiver must
/// notice, so it is the only one whose log line is load-bearing.
#[test]
fn withheld_body_flip_line_under_a_crc_is_unexplained() {
    let p = baseline();
    let bytes = gen_bytes(p, "wh-bf");
    let (wire, header, mut injections) = tap(&bytes, &forced(Class::BodyFlip), BODY_FLIP_PSI_SEED);
    let at = injections
        .iter()
        .position(|i| i.detectable)
        .expect("this seed lands a flip under a CRC");
    injections.remove(at);
    let r = judge(&wire, p, Some(&(header, injections)));
    assert_failure_starting_with(&r, "corruption_attributed");
}

/// A duplicated packet with an unchanged continuity counter is LEGAL
/// (§2.4.3.3 allows one repeat), so a receiver that says nothing about it
/// is conformant and `dup` is never `detectable`. Its log line is
/// therefore NOT load-bearing: the report must read the same with it and
/// without it. That is the honest positive statement for this class —
/// asserting an unexplained-event failure here would be asserting a bug.
#[test]
fn withheld_dup_line_changes_nothing() {
    let p = baseline();
    let bytes = gen_bytes(p, "wh-dup");
    let (wire, header, injections) = tap(&bytes, &forced(Class::Dup), SEED);
    assert!(!injections.is_empty());
    assert!(
        injections.iter().all(|i| !i.detectable),
        "a duplicate is invisible by design: {:?}",
        shape(&injections)
    );
    let with = judge(&wire, p, Some(&(header.clone(), injections)));
    assert!(with.pass, "{:?}", with.failures);
    let without = judge(&wire, p, Some(&(header, Vec::new())));
    assert!(
        without.pass,
        "a duplicated packet leaves no event to explain: {:?}",
        without.failures
    );
}

// ============================================================
// Mutation (b): the log claims damage the wire does not carry
// ============================================================

/// Judge the PRISTINE bytes against a log that says they were corrupted.
/// Every detectable injection then produced no event, which is exactly
/// what `corruption_detected` exists to catch — a receiver that sleeps
/// through deliberate damage.
fn a_clean_wire_under_a_log_is_undetected(class: Class) {
    let p = baseline();
    let bytes = gen_bytes(p, &format!("cl-{}", class.name()));
    let (_wire, header, injections) = tap(&bytes, &forced(class), SEED);
    assert!(
        injections.iter().any(|i| i.detectable),
        "{class:?}: this mutation needs a detectable injection"
    );
    let r = judge(&bytes, p, Some(&(header, injections)));
    assert_failure_starting_with(&r, "corruption_detected");
}

#[test]
fn clean_wire_header_is_undetected() {
    a_clean_wire_under_a_log_is_undetected(Class::Header);
}
#[test]
fn clean_wire_truncate_is_undetected() {
    a_clean_wire_under_a_log_is_undetected(Class::Truncate);
}
#[test]
fn clean_wire_garbage_is_undetected() {
    a_clean_wire_under_a_log_is_undetected(Class::Garbage);
}
#[test]
fn clean_wire_drop_is_undetected() {
    a_clean_wire_under_a_log_is_undetected(Class::Drop);
}
#[test]
fn clean_wire_psi_flip_is_undetected() {
    a_clean_wire_under_a_log_is_undetected(Class::PsiFlip);
}
#[test]
fn clean_wire_body_flip_under_a_crc_is_undetected() {
    let p = baseline();
    let bytes = gen_bytes(p, "cl-bf");
    let (_wire, header, injections) = tap(&bytes, &forced(Class::BodyFlip), BODY_FLIP_PSI_SEED);
    assert!(injections.iter().any(|i| i.detectable));
    let r = judge(&bytes, p, Some(&(header, injections)));
    assert_failure_starting_with(&r, "corruption_detected");
}

// ============================================================
// Mutation (c): the stream never recovers
// ============================================================

/// Replace the [`RECOVERY_BOUND`] packets after an injection with null-PID
/// packets. The damage is still explained and still noticed — but no media
/// follows it, which is what `corruption_recovered` exists to catch.
#[test]
fn no_media_after_an_injection_is_unrecovered() {
    let p = baseline();
    let bytes = gen_bytes(p, "rec");
    let (mut wire, header, injections) = tap(&bytes, &forced(Class::Header), SEED);
    // The forced first injection is at packet 0, so its whole recovery
    // window sits inside this 1800-packet capture and IS judged. (An
    // injection whose window ran off the end would not be, and the
    // mutation would pass vacuously — see the module doc.)
    assert_eq!(injections[0].ordinal, 0);
    let start = (injections[0].ordinal as usize + 1) * 188;
    let end = start + RECOVERY_BOUND as usize * 188;
    assert!(
        end <= wire.len(),
        "the whole recovery window must fit in the capture"
    );
    for pkt in wire[start..end].chunks_exact_mut(188) {
        // Null PID, payload-only, continuity counter 0: structurally
        // valid TS that carries nothing, so the reader keeps counting
        // packets and only the MEDIA disappears.
        pkt[1] = 0x1F;
        pkt[2] = 0xFF;
        pkt[3] = 0x10;
        for b in &mut pkt[4..] {
            *b = 0xFF;
        }
    }
    let r = judge(&wire, p, Some(&(header, injections)));
    assert_failure_starting_with(&r, "corruption_recovered");
}

// ============================================================
// Determinism and the un-logged control
// ============================================================

/// Same seed, same config, same input bytes: byte-identical wire and an
/// identical injection list. That is what lets a log written on a sender
/// judge a capture read back days later.
#[test]
fn the_same_seed_gives_an_identical_wire_and_log() {
    let p = baseline();
    let bytes = gen_bytes(p, "det");
    let first = tap(&bytes, "rate=200,min_gap=1000", 42);
    let second = tap(&bytes, "rate=200,min_gap=1000", 42);
    assert!(!first.2.is_empty(), "nothing injected — nothing to compare");
    assert_eq!(first.0, second.0, "wire bytes differ between runs");
    assert_eq!(first.2, second.2, "injection lists differ between runs");

    // And a different seed really does draw differently, so the equality
    // above is not just "the tap did nothing".
    let other = tap(&bytes, "rate=200,min_gap=1000", 43);
    assert_ne!(first.0, other.0, "seed 42 and 43 produced the same wire");
}

/// Damage the tap did NOT do is still fatal, log or no log. The library's
/// own non-conformance events only stop failing the report when an
/// injection accounts for them.
#[test]
fn unlogged_psi_damage_fails_nonconformant_and_attributed() {
    let p = baseline();
    let mut bytes = gen_bytes(p, "unl");
    // Byte 10 of packet 0 is inside the first PAT's section body (pointer
    // field at 4, 3-byte section header at 5..8, body from 8), so the
    // section fails its CRC and tst-core reports PsiChecksumMismatch.
    assert_eq!(bytes[1] & 0x1F, 0x00, "packet 0 is the PAT");
    bytes[10] ^= 0x5A;
    let header = LogHeader {
        tap_version: 1,
        seed: 0,
        rate_per_10k: 1,
        min_gap: 1000,
        classes: Class::ALL.to_vec(),
        attribution_window: ATTRIBUTION_WINDOW,
        recovery_bound: RECOVERY_BOUND,
    };
    let r = judge(&bytes, p, Some(&(header, Vec::new())));
    assert_failure_starting_with(&r, "corruption_attributed");
    assert_failure_starting_with(&r, "nonconformant_event");
}
