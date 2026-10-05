//! Loads `tests/fixtures/local/*.klv` if the directory exists. No-op
//! otherwise — sensitive captures stay off the public repo, this test
//! passes silently in CI.
//!
//! The shape variants this test slot is meant to exercise are documented
//! in `tests/coverage/TEST_CORPUS.md`. Filename prefixes drive
//! per-shape assertions:
//!
//! - `multi-record-pes-*.klv` — PES payload with a wrapper UL preceding
//!   the ST 0601 LS. Single-shot `decode` is expected to FAIL; the
//!   record-iterating path must succeed.
//! - `framed-pes-prefix-skip-*.klv` — PES payload with a non-UL
//!   framing prefix before the ST 0601 LS. Per MISB ST 1402.2
//!   Appendix B Table 2, the observed prefix matches the standard
//!   ISO/IEC 13818-1 Metadata AU cell header used by the
//!   Synchronous Metadata Multiplex Method:
//!   `[service_id:1][seq:1][flags:1][au_cell_data_length:2 BE]`.
//!   Both `decode` from offset 0 and the simple UL+BER record
//!   iterator are expected to FAIL; recovery is to scan forward
//!   for the SMPTE UL prefix `06 0E 2B 34` and decode from that
//!   offset. (A spec-compliant AU cell parser would parse the
//!   header explicitly and use `au_cell_data_length` for precise
//!   bounding — future work.)
//! - `decode-unchecked-only-*.klv` — record with broken checksum.
//!   `decode` is expected to fail with `ChecksumMismatch`;
//!   `decode_unchecked` must succeed.
//! - everything else — single ST 0601 record at offset 0; `decode`
//!   should succeed (with `decode_unchecked` as a relaxed fallback).

use std::fs;
use std::path::Path;

use tst_core::klv::length::read_ber;
use tst_core::klv::st0601::{decode, decode_unchecked};
use tst_core::klv::universal_label::UniversalLabel;

const LOCAL_FIXTURE_DIR: &str = "tests/fixtures/local";

#[test]
fn local_fixtures_decode() {
    let dir = Path::new(LOCAL_FIXTURE_DIR);
    let Ok(entries) = fs::read_dir(dir) else {
        return; // directory absent — silent pass
    };
    let mut count = 0usize;
    let mut failures: Vec<String> = Vec::new();
    let mut vmti_records = 0usize;
    let mut vmti_targets = 0usize;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("klv") {
            continue;
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skip {}: {}", path.display(), e);
                continue;
            }
        };
        count += 1;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("(unknown)");

        let result: Result<(), String> = if stem.starts_with("multi-record-pes-") {
            assert_multi_record_pes(&bytes)
        } else if stem.starts_with("framed-pes-prefix-skip-") {
            assert_framed_pes_prefix(&bytes)
        } else if stem.starts_with("decode-unchecked-only-") {
            assert_unchecked_only(&bytes)
        } else {
            assert_single_record(&bytes)
        };

        if let Err(msg) = result {
            failures.push(format!("{}: {msg}", path.display()));
        }

        // ST 0102 sibling-layer probe (panic-freedom contract).
        // Probe runs after the per-shape assertions so a shape failure
        // doesn't block ST 0102 coverage on the same file.
        if let Some(probe_result) = probe_st0102(&bytes) {
            match probe_result {
                Ok(security) => {
                    eprintln!(
                        "  {} carries ST 0102: classification={:?}, version={:?}",
                        path.display(),
                        security.security_classification,
                        security.version,
                    );
                }
                Err(e) => {
                    eprintln!(
                        "  {} ST 0102 decode error (acceptable for probe): {}",
                        path.display(),
                        e,
                    );
                }
            }
        }

        // VMTI (ST 0903) sibling-layer probe.
        //
        // On each non-empty Tag 74, attempt `klv::st0903::decode` and
        // assert the result is panic-free. Lenient mode always returns
        // `Ok` for parseable BER framing — the load-bearing assertion
        // is *no panic*, not "Ok return". Counts decoded VMTI records
        // and total targets — reports under `eprintln` so
        // `cargo test -- --nocapture` shows the corpus shape.
        if let Some(vmti) = probe_st0903(&bytes) {
            vmti_records += 1;
            vmti_targets += vmti.targets.len();
            eprintln!(
                "  {} carries ST 0903: {} target(s)",
                path.display(),
                vmti.targets.len(),
            );
        }
    }

    if count == 0 {
        return;
    }
    assert!(
        failures.is_empty(),
        "{} local fixture(s) failed:\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
    eprintln!("local_fixtures: {count} fixture(s) parsed");
    eprintln!("ST 0903 corpus probe: {vmti_records} record(s), {vmti_targets} target(s) total");
}

/// Single ST 0601 record at offset 0. `decode` must succeed; if its
/// checksum is broken, `decode_unchecked` must succeed instead.
/// Every fixture in our corpus has the timestamp.
fn assert_single_record(bytes: &[u8]) -> Result<(), String> {
    let rec = match decode(bytes) {
        Ok(r) => r,
        Err(_) => decode_unchecked(bytes).map_err(|e| format!("decode_unchecked failed: {e}"))?,
    };
    if rec.timestamp_us.is_none() {
        return Err("decoded record missing Tag 2 (timestamp_us)".into());
    }
    Ok(())
}

/// Record with broken checksum: `decode` must fail with
/// `ChecksumMismatch`; `decode_unchecked` must then succeed.
fn assert_unchecked_only(bytes: &[u8]) -> Result<(), String> {
    use tst_core::error::KlvDecodeError;
    match decode(bytes) {
        Ok(_) => {
            Err("decode unexpectedly succeeded — fixture is not actually checksum-broken".into())
        }
        Err(KlvDecodeError::ChecksumMismatch { .. }) => decode_unchecked(bytes)
            .map(|_| ())
            .map_err(|e| format!("decode_unchecked failed: {e}")),
        Err(other) => Err(format!("decode failed with non-checksum error: {other}")),
    }
}

/// PES payload carrying a Precision Time Stamp Pack (MISB ST 0605)
/// followed by an ST 0601 LS:
/// `[ts UL][BER 0x09][status(1)+µs(8)][ST 0601 UL][BER len][body]`.
/// `decode` on the whole buffer is expected to FAIL (it tries to
/// parse the time stamp pack UL as ST 0601). The leading record must
/// decode via `klv::st0605::decode`; the record-iterator path gating
/// on `UniversalLabel::is_st0601_family` must find at least one
/// successfully decoded ST 0601 record.
fn assert_multi_record_pes(bytes: &[u8]) -> Result<(), String> {
    if decode(bytes).is_ok() {
        return Err(
            "decode unexpectedly succeeded — multi-record fixture should require record-iter"
                .into(),
        );
    }

    // Expect the FIRST record to be a Precision Time Stamp Pack (per ST 0605).
    let pack = tst_core::klv::st0605::decode(bytes)
        .map_err(|e| format!("Time Stamp Pack decode failed: {e}"))?;
    if !pack.time_status.reserved_bits_valid() {
        return Err(format!(
            "Time Stamp Pack reserved bits invalid: status=0x{:02X}",
            pack.time_status.0
        ));
    }
    if pack.timestamp_us == 0 {
        return Err("Time Stamp Pack has zero timestamp".into());
    }

    // Then iterate the rest of the buffer and find at least one ST 0601 record.
    let mut i = 0usize;
    let mut decoded = 0usize;
    while i + 16 <= bytes.len() {
        let mut ul = [0u8; 16];
        ul.copy_from_slice(&bytes[i..i + 16]);
        let label = UniversalLabel::new(ul);
        let after_ul = &bytes[i + 16..];
        let (decl, after_len) = match read_ber(after_ul) {
            Ok(v) => v,
            Err(_) => break,
        };
        let len_bytes = after_ul.len() - after_len.len();
        let body_start = i + 16 + len_bytes;
        if body_start + decl > bytes.len() {
            break;
        }
        let total = 16 + len_bytes + decl;
        if label.is_st0601_family()
            && decode(&bytes[i..i + total])
                .or_else(|_| decode_unchecked(&bytes[i..i + total]))
                .is_ok()
        {
            decoded += 1;
        }
        i += total;
    }

    if decoded == 0 {
        Err("record-iter found no decodable ST 0601 records in PES payload".into())
    } else {
        Ok(())
    }
}

/// PES payload with an encoder-specific framing prefix before the
/// ST 0601 LS UL. The leading bytes do not form a valid SMPTE UL, so
/// `decode` and the UL+BER record iterator both fail. Recovery is to
/// scan for the SMPTE UL prefix `06 0E 2B 34` and decode from there.
fn assert_framed_pes_prefix(bytes: &[u8]) -> Result<(), String> {
    if decode(bytes).is_ok() {
        return Err(
            "decode unexpectedly succeeded — framed-prefix fixture should require UL scan".into(),
        );
    }
    const SMPTE_UL_PREFIX: &[u8] = &[0x06, 0x0E, 0x2B, 0x34];
    let offset = bytes
        .windows(SMPTE_UL_PREFIX.len())
        .position(|w| w == SMPTE_UL_PREFIX)
        .ok_or_else(|| "no SMPTE UL prefix found in fixture bytes".to_string())?;
    if offset == 0 {
        return Err("UL is at offset 0 — fixture is not actually prefix-framed".into());
    }
    decode(&bytes[offset..])
        .or_else(|_| decode_unchecked(&bytes[offset..]))
        .map(|_| ())
        .map_err(|e| format!("decode at offset {offset} failed: {e}"))
}

/// If the fixture decodes as an ST 0601 record AND carries a non-empty
/// Tag 48 (security_local_set), attempt `klv::st0102::decode` on the
/// inner bytes. Returns:
/// - `None` — fixture didn't decode as ST 0601, or Tag 48 absent/empty.
/// - `Some(Ok(security_ls))` — typed Security LS successfully decoded.
/// - `Some(Err(e))` — Security LS bytes present but decode failed.
///
/// Either outcome is acceptable — the corpus probe's contract is
/// panic-freedom on `klv::st0102::decode` for arbitrary real-world
/// inputs. ST 0102 is conditionally emitted (only on classified
/// content), so most fixtures will return None.
fn probe_st0102(
    bytes: &[u8],
) -> Option<Result<tst_core::klv::st0102::SecurityLs, tst_core::error::KlvDecodeError>> {
    let record = decode_unchecked(bytes).ok()?;
    let security_bytes = record.security_local_set.as_deref()?;
    if security_bytes.is_empty() {
        return None;
    }
    Some(tst_core::klv::st0102::decode(security_bytes))
}

/// If the fixture decodes as an ST 0601 record AND carries a non-empty
/// Tag 74 (`vmti`), attempt `klv::st0903::decode` on the inner bytes.
/// Returns:
/// - `None` — fixture didn't decode as ST 0601, or Tag 74 absent/empty.
/// - `Some(VmtiLs)` — typed VMTI LS successfully decoded.
///
/// The corpus probe's contract is panic-freedom on
/// `klv::st0903::decode` for arbitrary real-world inputs. Lenient
/// `decode` always returns `Ok` for parseable BER framing
/// (malformed-BER framing returns Err but does not panic — `expect`
/// here is the hard assertion). VMTI is conditionally emitted (only
/// from platforms running on-board VMTI), so most corpus fixtures
/// will return None.
fn probe_st0903(bytes: &[u8]) -> Option<tst_core::klv::st0903::VmtiLs> {
    let record = decode_unchecked(bytes).ok()?;
    let vmti_bytes = record.vmti.as_deref()?;
    if vmti_bytes.is_empty() {
        return None;
    }
    Some(
        tst_core::klv::st0903::decode(vmti_bytes)
            .expect("klv::st0903::decode lenient panic-freedom"),
    )
}
