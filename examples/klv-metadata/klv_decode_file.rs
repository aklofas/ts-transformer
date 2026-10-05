//! Decode an ST 0601 .klv blob and pretty-print typed fields.
//!
//! Walks the strictness ladder: tries `decode_strict_compliance` first,
//! falls back through `decode_strict`, `decode`, and `decode_unchecked`,
//! reporting which level succeeded and what failed at the previous level.
//!
//!   cargo run -p tst-examples --example klv_decode_file -- path/to/record.klv
//!
//! To produce `.klv` blobs from a captured `.ts`:
//!   cargo run -p tst-examples --example extract_klv -- path/to/capture.ts /tmp/klv_out
//! (the second arg is an output *prefix*, producing
//! `/tmp/klv_out_0000_async.klv`, `/tmp/klv_out_0001_sync.klv`, ... — 0-indexed,
//! suffixed `sync`, `async` or `unknown` by metadata kind). The committed
//! synthetic records under `crates/tst-core/tests/fixtures/st0601/`
//! (`synthetic_minimal.klv`, `synthetic_full.klv`, ...) also work as input.

use std::env;
use std::fs;
use tst_core::klv::st0601::{
    UasDatalinkLs, decode, decode_strict, decode_strict_compliance, decode_unchecked,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args()
        .nth(1)
        .ok_or("usage: klv_decode_file <path.klv>")?;
    let buf = fs::read(&path)?;
    eprintln!("loaded {} bytes from {path}", buf.len());

    // -----------------------------------------------------------------
    // Strictness ladder. Each rung is *more permissive* than the one
    // above it, so callers walk down until something accepts. The four
    // rungs in order from strictest to loosest:
    //
    //   1. decode_strict_compliance — checksum + ST 0601 family UL +
    //      ST 0601.8-09/-11/-12 mandatory ordering rules (Tag 2 first,
    //      Tag 1 last, Tag 65 present). The "production validation"
    //      rung — gates publishing pipelines that promise full spec
    //      compliance.
    //   2. decode_strict — checksum + ST 0601 family UL gate. Skips
    //      the mandatory ordering rules. Useful when the producer is
    //      known good but doesn't follow the newer mandatory ordering
    //      (-09/-11/-12 land in ST 0601.8 and many real captures
    //      predate it).
    //   3. decode — checksum + any UL. The default for "I just want
    //      the typed fields, accept whatever Universal Label." Most
    //      real-corpus captures parse here.
    //   4. decode_unchecked — skips the checksum entirely. Diagnostic
    //      only — when the bytes look corrupted but you want to
    //      inspect anyway. Never the right choice in production.
    // -----------------------------------------------------------------
    let parsed = match decode_strict_compliance(&buf) {
        Ok(rec) => {
            println!("decoded with: decode_strict_compliance (most strict)");
            rec
        }
        Err(e1) => {
            // Surface why the strictest rung rejected. Common causes:
            // the producer doesn't put Tag 2 first, doesn't end with
            // Tag 1, or omits Tag 65. None of these break the bytes —
            // they just fail spec compliance.
            eprintln!("decode_strict_compliance rejected: {e1}");
            match decode_strict(&buf) {
                Ok(rec) => {
                    println!("decoded with: decode_strict (compliance check failed: {e1})");
                    rec
                }
                Err(e2) => {
                    // strict still verifies checksum and UL family,
                    // so failure here means the Universal Label isn't
                    // an ST 0601 LS UL (e.g. a different MISB
                    // standard, or a stub UL).
                    eprintln!("decode_strict rejected: {e2}");
                    match decode(&buf) {
                        Ok(rec) => {
                            println!("decoded with: decode (UL check failed: {e2})");
                            rec
                        }
                        Err(e3) => {
                            // decode only verifies the checksum. A
                            // failure at this rung means the trailing
                            // BCC-16 doesn't match — bytes were
                            // corrupted in flight, or the producer
                            // didn't compute the checksum correctly.
                            eprintln!("decode rejected: {e3}");
                            match decode_unchecked(&buf) {
                                Ok(rec) => {
                                    println!(
                                        "decoded with: decode_unchecked (checksum failed: {e3})"
                                    );
                                    rec
                                }
                                Err(e4) => {
                                    // Unrecoverable: not parseable as
                                    // KLV at all. Truncated buffer,
                                    // malformed BER lengths, or just
                                    // not KLV bytes. Surface the last
                                    // error to the caller — there's
                                    // nothing more to try.
                                    return Err(
                                        format!("all decoders rejected; last error: {e4}").into()
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    print_record(&parsed);
    Ok(())
}

/// Pretty-print a parsed `UasDatalinkLs` record for human inspection.
///
/// Emits a stable line per typed field. Every field on the struct is
/// `Option<T>` (typed but possibly absent — ST 0601 mandates very few
/// tags), so each row is gated behind an `if let Some(...)` and silent
/// when absent. Fields decoded from IMAPB or BER fixed-point ranges
/// are printed with explicit precision so the output diffs cleanly
/// across runs of the same capture.
fn print_record(r: &UasDatalinkLs) {
    println!("\n--- typed fields ---");
    if let Some(ts) = r.timestamp_us {
        println!("timestamp_us:           {ts}");
    }
    if let Some(v) = r.uas_ls_version {
        println!("uas_ls_version:         {v}");
    }
    if let Some(s) = &r.platform_designation {
        println!("platform_designation:   {s}");
    }
    if let Some(s) = &r.platform_tail_number {
        println!("platform_tail_number:   {s}");
    }
    if let Some(s) = &r.image_source_sensor {
        println!("image_source_sensor:    {s}");
    }
    if let (Some(lat), Some(lon)) = (r.sensor_lat_deg, r.sensor_lon_deg) {
        println!("sensor_lat/lon:         {lat:.6}, {lon:.6}");
    }
    if let Some(alt) = r.sensor_alt_m {
        println!("sensor_alt_m:           {alt:.2}");
    }
    if let Some(h) = r.platform_heading_deg {
        println!("platform_heading_deg:   {h:.3}");
    }
    if let Some(p) = r.platform_pitch_deg {
        println!("platform_pitch_deg:     {p:.3}");
    }
    if let Some(rl) = r.platform_roll_deg {
        println!("platform_roll_deg:      {rl:.3}");
    }
    if let (Some(lat), Some(lon)) = (r.frame_center_lat_deg, r.frame_center_lon_deg) {
        println!("frame_center_lat/lon:   {lat:.6}, {lon:.6}");
    }
    if let Some(e) = r.frame_center_elev_m {
        println!("frame_center_elev_m:    {e:.2}");
    }
    if let Some(f) = r.sensor_hfov_deg {
        println!("sensor_hfov_deg:        {f:.3}");
    }
    if let Some(f) = r.sensor_vfov_deg {
        println!("sensor_vfov_deg:        {f:.3}");
    }

    // ST 0107.5 future-proof skip rule: tags not in the typed table
    // are passed through as `OwnedRawField` so consumers can still see
    // them. This lets a record produced by a newer ST 0601 revision
    // round-trip through an older decoder without losing data — the
    // unrecognized tags survive in `unknown` and can be re-emitted by
    // `encode` (see `write_unknown_fields` in st0601/mod.rs). Cap the
    // displayed entries at 5 so a record packed with experimental
    // tags doesn't drown out the typed-field summary.
    if !r.unknown.is_empty() {
        println!("\nunknown tags (pass-through): {}", r.unknown.len());
        for f in r.unknown.iter().take(5) {
            println!("  tag={} len={}", f.tag, f.value.len());
        }
        if r.unknown.len() > 5 {
            println!("  ... ({} more)", r.unknown.len() - 5);
        }
    }

    // Per-field decode errors don't fail the whole record — the
    // decoder records them and continues. Typical causes: an out-of-
    // range mapped value, a wrong-length field, an INVALID sentinel
    // (some ST 0601 mappings reserve a code point to mean "no data").
    // Surfacing them lets the caller decide whether to drop the
    // record or carry the partial parse. Cap at 5 for output
    // readability — a flood of field errors usually means the buffer
    // is corrupted and the caller should drop anyway.
    if !r.field_errors.is_empty() {
        println!("\nfield decode errors: {}", r.field_errors.len());
        for e in r.field_errors.iter().take(5) {
            println!("  {e}");
        }
    }
}
