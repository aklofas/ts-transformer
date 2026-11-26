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
    APPROX_SLACK, ATTRIBUTION_WINDOW, AttributionReport, Class, Corrupter, Injection, LogHeader,
    RECOVERY_BOUND, parse_corrupt, parse_log,
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

/// A seed whose forced body flips on the `audio` profile hit a PCR base
/// byte of a PCR-only packet (found by sweeping seeds; see
/// [`a_body_flip_on_a_pcr_only_packet_explains_its_pcr_jumps`]).
const BODY_FLIP_PCR_ONLY_SEED: u64 = 26;

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
        parse_corrupt(spec, seed, 1).expect("the spec parses"),
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
            tst_test_helpers::rawts::classify_packet(pk).is_ok_and(|i| i.pid == 0x1000 && i.pusi)
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
            approx_slack: APPROX_SLACK,
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
///
/// What a seed varies here is the damage, not its place. [`forced`] injects
/// at the `min_gap` floor, so every seed hits the same packets: packet 0
/// (the PAT), and packet 1000, a video PES start on the PCR PID. The seed
/// draws the byte offsets, the header sub-kind and the truncation or
/// garbage length.
///
/// The sweep is also where the PCR lead oracle meets the framing classes:
/// damage that hands the raw reader a wrong clock (a truncation it re-locks
/// after) must come out excused, so at least one run has to have excused a
/// lead, or the sweep never exercised that path.
#[test]
fn every_class_is_attributed_across_a_seed_sweep() {
    let p = baseline();
    let bytes = gen_bytes(p, "sweep");
    let mut runs = 0;
    let mut leads_excused = 0;
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
            leads_excused += r
                .metrics
                .corruption_attribution
                .expect("attribution")
                .pcr_lead_excused;
            runs += 1;
        }
    }
    assert_eq!(runs, Class::ALL.len() * 24);
    assert!(leads_excused >= 1, "no run of the sweep excused a lead");
}

/// Seeds per (profile, class) in the sweep below: 1 to 8, and 13, whose
/// [`Class::Truncate`] run leaves a lead the excusal must cover on five
/// profiles (`baseline`, `klv-sync`, `pcr-tight`, `pcr-sparse`,
/// `pts-rollover`; measured over seeds 1 to 64). The guard at the sweep's end
/// asks for three excused leads, so a byte shift in one profile's stream
/// cannot leave it with none.
const LEAD_SWEEP_SEEDS: &[u64] = &[1, 2, 3, 4, 5, 6, 7, 8, 13];

/// The soak draws its profiles from the whole registry, so the PCR lead
/// floor has to hold under every class on every profile, not on `baseline`
/// alone: no `pcr_lead` line may survive the excusal. Other verdicts are
/// the other tests' subject and are not read here (some profiles' oracles
/// have their own limits under a forced class).
///
/// As in the sweep above, [`forced`] puts every seed's injections on the
/// same packets: the PAT at packet 0, and a video PES start on the PCR PID
/// at each multiple of 1000 (a deferred `psi_flip` lands on the next PAT).
/// So this covers each class's damage to a PSI packet and to a video PES
/// start on the PCR PID, at compact sizes. It never hits a KLV or audio
/// PID, a packet in the middle of a PES or a PCR-only packet: those are
/// [`the_pcr_lead_floor_survives_a_mixed_tap_at_realistic_sizes`] and
/// [`a_body_flip_on_a_pcr_only_packet_explains_its_pcr_jumps`].
#[test]
fn the_pcr_lead_floor_survives_every_class_on_every_profile() {
    let mut excused = 0;
    for p in profiles::all() {
        let bytes = gen_bytes(p, &format!("lead-sweep-{}", p.name));
        for class in Class::ALL {
            for &seed in LEAD_SWEEP_SEEDS {
                let (wire, header, injections) = tap(&bytes, &forced(class), seed);
                let r = judge(&wire, p, Some(&(header, injections.clone())));
                let lead: Vec<&String> = r
                    .failures
                    .iter()
                    .filter(|f| f.starts_with("pcr_lead"))
                    .collect();
                assert!(
                    lead.is_empty(),
                    "{} {class:?} seed {seed}: {lead:?} for {:?}",
                    p.name,
                    shape(&injections)
                );
                excused += r
                    .metrics
                    .corruption_attribution
                    .expect("attribution")
                    .pcr_lead_excused;
            }
        }
    }
    assert!(
        excused >= 3,
        "{excused} lead(s) excused over the sweep, expected at least 3"
    );
}

/// Seeds of the realistic sweep below. Chosen so that the runs between them
/// land on a payload-less packet and on PIDs other than the PCR PID, and so
/// that none draws damage that moves a lead; the test checks both.
const REALISTIC_SWEEP_SEEDS: [u64; 4] = [2, 6, 7, 8];

/// Capture length of the realistic sweep: about 25 000 packets a profile.
const REALISTIC_SWEEP_SECONDS: f64 = 20.0;

/// The tap as a soak leg meets it: access units of realistic size, so a
/// PES spans many packets and a 500-packet window is under half a second,
/// and all classes mixed, so the injections fall where the stream's bytes
/// are. Most land in the middle of a video PES, some on a KLV or audio
/// PID, on PSI or on a payload-less packet. No `pcr_lead` line may appear,
/// on any profile.
///
/// This shows the floor does not misfire on damage that leaves the clock
/// alone. It does not show the excusal: a flipped PCR byte or a re-lock
/// that misreads a clock is rare at these sizes (a longer run of the same
/// kind, 186 467 injections over eight specs, excused 15 leads), and with
/// these seeds none occurs, so every capture still reads exactly 100.0 ms.
/// A seed that did draw one would be excused and would fail that last
/// check, not the floor. The tests that force such damage are
/// [`a_body_flip_on_a_pcr_only_packet_explains_its_pcr_jumps`] and the
/// seed sweep above.
#[test]
fn the_pcr_lead_floor_survives_a_mixed_tap_at_realistic_sizes() {
    let mut classes = std::collections::BTreeSet::new();
    let (mut total, mut mid_pes, mut other_pids, mut no_payload) = (0, 0, 0, 0);
    for p in profiles::all() {
        let path = std::env::temp_dir().join(format!(
            "tst-interop-corruption-realistic-{}-{}.ts",
            p.name,
            std::process::id()
        ));
        r#gen::run(
            p,
            REALISTIC_SWEEP_SECONDS,
            &path,
            KlvSet::Compact,
            0,
            AuSizeMode::Realistic { scale: 1 },
        )
        .expect("gen::run");
        let bytes = std::fs::read(&path).expect("read the generated capture");
        let _ = std::fs::remove_file(&path);
        // The PIDs that carry a PCR in the clean capture.
        let pcr_pids: std::collections::BTreeSet<u16> = bytes
            .chunks_exact(188)
            .filter(|pkt| pkt[3] & 0x20 != 0 && pkt[4] >= 7 && pkt[5] & 0x10 != 0)
            .map(|pkt| (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2]))
            .collect();

        for seed in REALISTIC_SWEEP_SEEDS {
            let (wire, header, injections) = tap(&bytes, "rate=10000,min_gap=1000", seed);
            for i in &injections {
                classes.insert(i.class.name());
                let afc = i.before.get(3).map_or(0, |b| b & 0x30);
                total += 1;
                mid_pes += usize::from(!i.psi && !i.pes_start && afc & 0x10 != 0);
                other_pids += usize::from(!i.psi && !pcr_pids.contains(&i.pid));
                no_payload += usize::from(afc == 0x20);
            }
            let r = verify_bytes_with_corruption(
                &wire,
                p,
                REALISTIC_SWEEP_SECONDS,
                VerifyMode::Lossy,
                KlvExpect::compact(),
                Some(&(header, injections.clone())),
            );
            let lead: Vec<&String> = r
                .failures
                .iter()
                .filter(|f| f.starts_with("pcr_lead"))
                .collect();
            assert!(
                lead.is_empty(),
                "{} seed {seed}: {lead:?} for {:?}",
                p.name,
                shape(&injections)
            );
            assert_eq!(
                r.metrics.pcr_lead_min_ms,
                Some(100.0),
                "{} seed {seed}: a lead moved (an excused one shows here too; pick another seed)",
                p.name
            );
        }
    }
    // What the sweep reached, so the claim above stays true of it.
    assert_eq!(classes.len(), Class::ALL.len(), "{classes:?}");
    assert!(total >= 1_000, "{total} injections");
    assert!(mid_pes >= total / 2, "{mid_pes} of {total} inside a PES");
    assert!(
        other_pids >= 3,
        "{other_pids} on a PID other than a PCR PID"
    );
    assert!(no_payload >= 1, "{no_payload} on a payload-less packet");
}

/// The excusal is by position. The first injection of a forced `header`
/// run is a killed sync byte on the PAT at packet 0: damage that breaks
/// framing for the whole multiplex, so inside its window it excuses a lead
/// on any PID. A PCR rewritten by hand after that window has closed, and
/// before the next injection's opens, fails `pcr_lead`: the access unit
/// that carries it reads a lead ten seconds below zero, and nothing is
/// counted as excused.
///
/// The edit sits where the attribution still holds the first injection
/// (it retires one only `RECOVERY_BOUND + APPROX_SLACK` packets past it),
/// so an excusal that forgot to ask about the window would excuse it.
#[test]
fn a_bad_lead_outside_every_injection_window_is_not_excused() {
    let p = baseline();
    let bytes = gen_bytes(p, "lead-outside");
    let (mut wire, header, injections) = tap(&bytes, &forced(Class::Header), SEED);
    assert_eq!(
        wire.len(),
        bytes.len(),
        "a header rewrite keeps every packet"
    );
    let log = (header, injections);
    assert!(judge(&wire, p, Some(&log)).pass, "the control passes");
    let first = &log.1[0];
    assert!(
        first.ordinal == 0 && first.class == Class::Header && first.offsets == [0],
        "the first injection must be a killed sync byte at packet 0: {:?}",
        shape(&log.1)
    );

    // Past the first window's last packet (the window, plus the slack of
    // an approximate resolution), and still inside the span the
    // attribution keeps that injection for.
    let clear = (ATTRIBUTION_WINDOW + APPROX_SLACK) as usize + 10;
    let held = (RECOVERY_BOUND + APPROX_SLACK) as usize - 10;
    let at = (clear..held)
        .find(|&n| is_video_pcr_with_payload(&wire[n * 188..(n + 1) * 188]))
        .expect("a video packet with a PCR and a payload after the first window");
    assert!(
        log.1[1..].iter().all(|i| i.ordinal > at as u64 + 150),
        "packet {at} must be clear of every later injection: {:?}",
        shape(&log.1)
    );
    move_pcr_ten_seconds_ahead(&mut wire, at);

    let r = judge(&wire, p, Some(&log));
    assert_failure_starting_with(&r, "pcr_lead: PID 4113 min lead -9900.0 ms");
    let a = r.metrics.corruption_attribution.expect("attribution");
    assert_eq!(a.pcr_lead_excused, 0, "{a:?}");
}

/// The excusal is by kind of damage too. A forced `drop` run removes the
/// video PES start at packet 1000, on the PCR PID: damage that takes a
/// packet away and cannot move a clock forward. A PCR rewritten by hand a
/// few packets later, well inside that injection's window, still fails
/// `pcr_lead` with nothing excused.
#[test]
fn a_bad_lead_beside_damage_that_cannot_lower_one_is_not_excused() {
    let p = baseline();
    let bytes = gen_bytes(p, "lead-beside-drop");
    let (mut wire, header, injections) = tap(&bytes, &forced(Class::Drop), SEED);
    let log = (header, injections);
    assert!(judge(&wire, p, Some(&log)).pass, "the control passes");
    let drop = log
        .1
        .iter()
        .find(|i| i.class == Class::Drop && i.pid == 0x1011)
        .unwrap_or_else(|| panic!("a drop on the video PID: {:?}", shape(&log.1)));
    // The wire is short of every packet dropped before this point, so the
    // packet after the dropped one sits at most at the injection's ordinal.
    let from = drop.ordinal as usize;
    let at = (from + 5..from + 100)
        .find(|&n| is_video_pcr_with_payload(&wire[n * 188..(n + 1) * 188]))
        .expect("a video packet with a PCR and a payload inside the drop's window");
    move_pcr_ten_seconds_ahead(&mut wire, at);

    let r = judge(&wire, p, Some(&log));
    assert_failure_starting_with(&r, "pcr_lead: PID 4113 min lead -9900.0 ms");
    let a = r.metrics.corruption_attribution.expect("attribution");
    assert_eq!(a.pcr_lead_excused, 0, "{a:?}");
}

/// A packet of the baseline's video PID (0x1011) that has both a payload
/// and an adaptation field with a PCR.
fn is_video_pcr_with_payload(pkt: &[u8]) -> bool {
    let pid = (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2]);
    pid == 0x1011 && pkt[3] & 0x30 == 0x30 && pkt[4] >= 7 && pkt[5] & 0x10 != 0
}

/// Rewrite the PCR base of packet `at` of `wire` ten seconds ahead, so the
/// access unit in that packet reads a lead of 100 − 10 000 ms.
fn move_pcr_ten_seconds_ahead(wire: &mut [u8], at: usize) {
    let pcr = &mut wire[at * 188 + 6..at * 188 + 11];
    let base = (u64::from(pcr[0]) << 25)
        | (u64::from(pcr[1]) << 17)
        | (u64::from(pcr[2]) << 9)
        | (u64::from(pcr[3]) << 1)
        | u64::from(pcr[4] >> 7);
    let moved = base + 10 * 90_000;
    pcr[0] = (moved >> 25) as u8;
    pcr[1] = (moved >> 17) as u8;
    pcr[2] = (moved >> 9) as u8;
    pcr[3] = (moved >> 1) as u8;
    pcr[4] = (pcr[4] & 0x7F) | ((moved & 1) as u8) << 7;
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
/// invisible on PSI); `corrupt/tap.rs`'s own
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
        approx_slack: APPROX_SLACK,
    };
    let r = judge(&bytes, p, Some(&(header, Vec::new())));
    assert_failure_starting_with(&r, "corruption_attributed");
    assert_failure_starting_with(&r, "nonconformant_event");
}

/// The `audio` profile is the one whose muxer emits PCR-only packets
/// (adaptation field, no payload) on the video PID, and a body flip there
/// lands in the adaptation field: at this seed it flips a PCR base byte.
/// Measured through the real demuxer, the receiver reports the PCR jump
/// and the jump back as two `PcrAnomaly` events on that PID; the
/// body-flip arm of `expects` explains them.
#[test]
fn a_body_flip_on_a_pcr_only_packet_explains_its_pcr_jumps() {
    let p = profiles::by_name("audio").expect("the audio profile exists");
    let bytes = gen_bytes(p, "bf-pcr-only");
    let (wire, header, injections) = tap(
        &bytes,
        "rate=300,min_gap=1000,classes=body_flip",
        BODY_FLIP_PCR_ONLY_SEED,
    );
    assert!(
        injections.iter().any(|i| i.class == Class::BodyFlip
            && i.before[3] & 0x30 == 0x20
            && i.offsets.iter().any(|o| (6..=11).contains(o))),
        "seed {BODY_FLIP_PCR_ONLY_SEED} must flip a PCR byte of a PCR-only packet: {:?}",
        shape(&injections)
    );
    let log = (header, injections);
    let r = judge(&wire, p, Some(&log));
    assert!(r.pass, "{:?}", r.failures);
    // Until the next sound PCR, every PES of the program is measured
    // against the flipped one and reads a lead far below zero. Those are
    // the flip's doing: counted as excused, not as a `pcr_lead` failure.
    assert!(
        r.metrics.pcr_lead_min_ms.is_some_and(|ms| ms < 0.0),
        "{:?}",
        r.metrics.pcr_lead_min_ms
    );
    let excused = r
        .metrics
        .corruption_attribution
        .as_ref()
        .expect("attribution")
        .pcr_lead_excused;
    assert!(excused >= 1, "{excused}");
    // Strict attribution explains both events too. (Strict mode's
    // `pcr_interval` oracle still fails here: it holds every interval,
    // and the flipped PCR is one bogus interval it does not excuse.)
    let strict = judge_in(VerifyMode::Strict, &wire, p, Some(&log));
    let a = strict.metrics.corruption_attribution.expect("attribution");
    assert_transport_loss_excusal_is_inert(&a, "body_flip on a PCR-only packet");
    assert!(
        a.unexplained_events.is_empty(),
        "{:?}",
        a.unexplained_events
    );
    assert!(
        a.attributed_events >= 2,
        "the PCR jump and its return are both explained: {a:?}"
    );
}

/// First seed in `1..=400` whose body flips include one on a PCR-only packet
/// that touches nothing below offset 12 (stuffing only). Found with:
///
/// ```text
/// for seed in 1..=400 {
///     let (_, _, inj) = tap(&bytes, "rate=300,min_gap=1000,classes=body_flip", seed);
///     if inj.iter().any(|i| i.class == Class::BodyFlip
///         && i.before[3] & 0x30 == 0x20
///         && i.offsets.iter().all(|o| *o >= 12)) { /* first hit: 1 */ }
/// }
/// ```
const BODY_FLIP_STUFFING_ONLY_SEED: u64 = 1;

/// A flip confined to the stuffing of a PCR-only packet cannot have moved
/// the PCR, so no PCR anomaly is offered as its consequence.
#[test]
fn a_stuffing_only_body_flip_on_a_pcr_only_packet_is_not_offered_as_a_pcr_explanation() {
    let p = profiles::by_name("audio").expect("the audio profile exists");
    let bytes = gen_bytes(p, "bf-stuffing-only");
    let (wire, header, injections) = tap(
        &bytes,
        "rate=300,min_gap=1000,classes=body_flip",
        BODY_FLIP_STUFFING_ONLY_SEED,
    );
    assert!(
        injections.iter().any(|i| i.class == Class::BodyFlip
            && i.before[3] & 0x30 == 0x20
            && i.offsets.iter().all(|o| *o >= 12)),
        "seed must flip only stuffing: {:?}",
        shape(&injections)
    );
    let strict = judge_in(VerifyMode::Strict, &wire, p, Some(&(header, injections)));
    let a = strict.metrics.corruption_attribution.expect("attribution");
    // Nothing happened to the PCR, so nothing is attributed to the flip.
    assert_eq!(a.attributed_events, 0, "{a:?}");
    assert!(
        a.unexplained_events.is_empty(),
        "{:?}",
        a.unexplained_events
    );
}

/// A body flip that rewrites a PCR-only packet's adaptation_field_length to
/// a value the raw reader cannot parse: the field overruns the packet (188,
/// 231), or it is shorter than the PCR its flags announce (1). The reader
/// skips that one packet and records a resync at its ordinal, and the flip
/// explains it. A flipped length that still parses (165) causes no resync.
///
/// The tap picks the packet and writes the log; the flipped byte is then
/// set to the length under test on the wire and in the log alike, because
/// no seed draws these exact values.
#[test]
fn an_unparseable_body_flip_on_a_pcr_only_packet_explains_its_one_resync() {
    let p = profiles::by_name("audio").expect("the audio profile exists");
    let bytes = gen_bytes(p, "bf-unparseable");
    for (af_len, resyncs) in [(188u8, 1u64), (231, 1), (1, 1), (165, 0)] {
        let (mut wire, header, mut injections) = tap(
            &bytes,
            "rate=300,min_gap=1000,classes=body_flip",
            BODY_FLIP_PCR_ONLY_SEED,
        );
        let inj = injections
            .iter_mut()
            .find(|i| i.before[3] & 0x30 == 0x20 && i.before[4] == 183 && i.before[5] == 0x10)
            .expect("the seed flips a PCR-only packet");
        // Body flips keep the wire packet-aligned with the input.
        let at = usize::try_from(inj.ordinal).expect("ordinal") * 188;
        assert_eq!(&wire[at..at + 188], inj.after.as_slice());
        inj.after = inj.before.clone();
        inj.after[4] = af_len;
        inj.offsets = vec![4];
        wire[at..at + 188].copy_from_slice(&inj.after);

        let log = (header, injections);
        let r = judge(&wire, p, Some(&log));
        assert!(r.pass, "length {af_len}: {:?}", r.failures);
        let strict = judge_in(VerifyMode::Strict, &wire, p, Some(&log));
        let a = strict.metrics.corruption_attribution.expect("attribution");
        assert_transport_loss_excusal_is_inert(&a, "an unparseable body flip");
        assert_eq!(a.resyncs, resyncs, "length {af_len}: {a:?}");
        assert!(
            a.unexplained_events.is_empty(),
            "length {af_len}: {:?}",
            a.unexplained_events
        );
        assert_eq!(a.unexplained_resyncs, 0, "length {af_len}: {a:?}");
        assert!(a.attributed_events >= resyncs, "length {af_len}: {a:?}");
    }
}
