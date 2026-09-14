//! Rich-KLV oracle mutation tests (spec §5.5).
//!
//! Two kinds of mutation, because the oracles answer two different
//! questions and only one of them is reachable through the wire.
//!
//! The wire-level ones `gen` a rich capture and then damage the muxed
//! bytes — a flipped byte inside a KLV PES payload is exactly what a
//! corrupting peer produces, and `klv_rich_decode_clean` exists to catch
//! it. Judging the same clean capture with the WRONG seed is the other
//! wire-level mutation: it stands in for a generator that emitted the
//! wrong presence schedule, which is what `klv_rich_census` exists to
//! catch, and which cannot be produced by editing bytes (a record whose
//! tag set was rebuilt would need re-muxing, re-CRCing and re-paced PES
//! packing to stay a valid capture).
//!
//! The nested-security one goes through `verify::testing::judge_records`
//! instead: stripping Tag 48 from every record on the wire has the same
//! re-muxing problem, so that hook feeds decoded record bytes straight
//! into the same `Tally` the wire path uses. See its own doc comment for
//! what it does and does not judge.

use tst_interop::fixtures::KlvSet;
use tst_interop::r#gen;
use tst_interop::profiles;
use tst_interop::report_types::VerifyReport;
use tst_interop::verify::{KlvExpect, VerifyMode, verify_bytes_with_corruption};

/// Long enough to clear the count floors with margin (`gen` at 10 Hz KLV
/// gives 40 records), short enough to keep the whole file sub-second.
const SECONDS: f64 = 4.0;
const SEED: u64 = 5;

/// `gen` `SECONDS` of rich-KLV `baseline` traffic and return the bytes.
fn gen_rich(tag: &str) -> Vec<u8> {
    let p = profiles::by_name("baseline").expect("baseline profile must exist");
    // pid + the clock: `cargo test` runs a binary's tests in ONE process,
    // and several of them below generate their own capture.
    let path = std::env::temp_dir().join(format!(
        "tst-interop-klvrich-{tag}-{}-{}.ts",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time moves forward")
            .as_nanos()
    ));
    r#gen::run(p, SECONDS, &path, KlvSet::Rich, SEED).expect("gen::run must succeed");
    let bytes = std::fs::read(&path).expect("read the generated capture");
    let _ = std::fs::remove_file(&path);
    bytes
}

/// Judge `bytes` as a rich capture generated with `seed`.
fn judge(bytes: &[u8], seed: u64) -> VerifyReport {
    verify_bytes_with_corruption(
        bytes,
        profiles::by_name("baseline").expect("baseline profile must exist"),
        SECONDS,
        VerifyMode::Strict,
        KlvExpect {
            set: KlvSet::Rich,
            seed,
        },
        None,
    )
}

fn fails_with(r: &VerifyReport, prefix: &str) {
    assert!(
        r.failures.iter().any(|f| f.starts_with(prefix)),
        "want a {prefix} failure, got: {:?}",
        r.failures
    );
}

/// Positive control. Without this every mutation below could be passing
/// for the wrong reason (an oracle that fails on everything catches every
/// mutation and is worth nothing).
#[test]
fn rich_stream_passes_all_three_oracles() {
    let r = judge(&gen_rich("clean"), SEED);
    assert!(r.pass, "clean rich capture must pass: {:?}", r.failures);

    let m = r
        .metrics
        .klv_rich
        .expect("a rich judgement reports metrics");
    assert!(
        m.records > 20,
        "the capture must actually carry records: {m:?}"
    );
    assert_eq!(m.decode_errors, 0, "{m:?}");
    assert_eq!(m.field_error_records, 0, "{m:?}");
    assert_eq!(m.census_mismatches, 0, "{m:?}");
    // The security group recurs every fifth record (fixtures'
    // `rich_group_period`), so a 40-record capture must hit it — a zero
    // here would make the third oracle vacuous.
    assert!(
        m.security_expected > 0,
        "the presence schedule must demand Tag 48 somewhere: {m:?}"
    );
    assert_eq!(m.security_ok, m.security_expected, "{m:?}");
    assert!(m.first_problem.is_none(), "{m:?}");
}

/// A receiver told the wrong seed computes the wrong presence schedule —
/// which is indistinguishable, from the wire, from a generator that
/// emitted the wrong one. Either way the census oracle must say so
/// rather than shrugging at a record whose tags it did not expect.
#[test]
fn wrong_presence_schedule_fails_census() {
    let r = judge(&gen_rich("wrongseed"), SEED + 1);
    fails_with(&r, "klv_rich_census");
    assert!(!r.pass);
}

/// The ST 0601 UAS Datalink LS universal label (MISB ST 0601.19 §6.1) —
/// used below only to prove the offset arithmetic landed on a record.
const ST0601_UL: [u8; 16] = [
    0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00, 0x00,
];

/// A flipped byte inside a KLV record's VALUE region — past the PES
/// header, past the universal label and BER length — must surface as a
/// decode failure (or a field error), not be silently tallied as one
/// more healthy record.
///
/// The offsets are computed from the packet rather than hardcoded. A
/// rich record is ~128 bytes, so its PES leaves the TS packet with a
/// large adaptation-field stuffing run whose length shifts with whether
/// that particular PES header carries a PTS — a fixed offset lands
/// somewhere different for different records, and a flip that hit the
/// universal label rather than the record body did NOT fail this check
/// while the test was being written.
#[test]
fn corrupted_record_payload_fails_decode_clean() {
    let mut bytes = gen_rich("flip");
    // Read the KLV PID out of the profile's own invariants rather than
    // hardcoding it — a registry PID change would otherwise silently
    // start mutating some other stream's packets.
    let p = profiles::by_name("baseline").expect("baseline profile must exist");
    let klv_pid = profiles::invariants(p).programs[0].klv_pid;
    let pkt = 188
        * bytes
            .chunks_exact(188)
            .position(|pkt| {
                let pid = (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2]);
                pid == klv_pid && pkt[1] & 0x40 != 0
            })
            .expect("the capture must carry a KLV PES start packet");

    // 4-byte TS header, then the adaptation field when present
    // (ISO/IEC 13818-1 §2.4.3.2: adaptation_field_control bit 0x20),
    // whose own length byte does not count itself.
    let mut off = 4;
    if bytes[pkt + 3] & 0x20 != 0 {
        off += 1 + usize::from(bytes[pkt + 4]);
    }
    // PES: 6-byte start (00 00 01 <stream_id> <length:16>) + 2 flag
    // bytes + PES_header_data_length, then the optional fields that
    // length covers (§2.4.3.7).
    let ls = off + 9 + usize::from(bytes[pkt + off + 8]);
    assert_eq!(
        &bytes[pkt + ls..pkt + ls + 16],
        &ST0601_UL,
        "the computed offset must land on the record's universal label"
    );
    // UL (16) + BER long-form length (0x81 LL for a ~128-byte record) —
    // so +20 is a couple of bytes into the first TLV, inside the value
    // region the checksum covers.
    bytes[pkt + ls + 20] ^= 0xFF;

    let r = judge(&bytes, SEED);
    fails_with(&r, "klv_rich_decode_clean");
    assert!(!r.pass);
    // Exactly one record was damaged — a mutation that took out the
    // whole capture would fail this oracle for the wrong reason.
    let m = r
        .metrics
        .klv_rich
        .expect("a rich judgement reports metrics");
    assert_eq!(m.decode_errors + m.field_error_records, 1, "{m:?}");
}

/// A record whose nested ST 0102 set was dropped must fail the
/// nested-security oracle — and the census one too, since Tag 48 is part
/// of the presence schedule the census checks.
///
/// Goes through `verify::testing::judge_records` rather than the wire:
/// re-encoding one record inside a muxed capture is not a byte-stable
/// edit (PES lengths, continuity counters and the PCR schedule all
/// move), so the hook feeds the mutated record bytes directly into the
/// same `Tally` the wire path uses.
#[test]
fn missing_nested_security_set_fails_security_nested() {
    use tst_core::klv::st0601;
    use tst_interop::fixtures::{klv_record_rich, rich_presence};

    let seq = (0..500u32)
        .find(|&s| rich_presence(SEED, s).contains(&48))
        .expect("the schedule must demand Tag 48 within 500 records");
    let mut rec =
        st0601::decode(&klv_record_rich(SEED, seq).expect("rich record")).expect("record decodes");
    rec.security_local_set = None;
    let bytes = st0601::encode_to_vec(&rec).expect("the stripped record re-encodes");

    let r = tst_interop::verify::testing::judge_records(
        &[bytes],
        KlvExpect {
            set: KlvSet::Rich,
            seed: SEED,
        },
    );
    fails_with(&r, "klv_rich_security_nested");
    fails_with(&r, "klv_rich_census");
}

/// The same positive control for SYNC carriage (`klv-sync`), which wraps
/// every record in an H.222.0 §2.12.4.2 Metadata AU cell. The demuxer
/// documents that it peels the 5-byte cell header before handing the
/// payload over (`MetadataKind::KlvSyncAuCell`); this is the test that
/// the rich oracles therefore need no stripping of their own — with the
/// header still attached, every record would fail to decode.
#[test]
fn rich_sync_carriage_records_decode_without_stripping() {
    let p = profiles::by_name("klv-sync").expect("klv-sync profile must exist");
    let path = std::env::temp_dir().join(format!(
        "tst-interop-klvrich-sync-{}-{}.ts",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time moves forward")
            .as_nanos()
    ));
    r#gen::run(p, SECONDS, &path, KlvSet::Rich, SEED).expect("gen::run must succeed");
    let bytes = std::fs::read(&path).expect("read the generated capture");
    let _ = std::fs::remove_file(&path);

    let r = verify_bytes_with_corruption(
        &bytes,
        p,
        SECONDS,
        VerifyMode::Strict,
        KlvExpect {
            set: KlvSet::Rich,
            seed: SEED,
        },
        None,
    );
    assert!(r.pass, "sync-carriage rich capture: {:?}", r.failures);
    let m = r
        .metrics
        .klv_rich
        .expect("a rich judgement reports metrics");
    assert!(m.records > 20, "{m:?}");
    assert_eq!(m.decode_errors, 0, "{m:?}");
    assert_eq!(m.census_mismatches, 0, "{m:?}");
}
