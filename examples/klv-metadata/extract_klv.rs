//! Extract `.klv` payload blobs from an MPEG-TS file using the public
//! `tst_core::mpegts::demux::Demuxer` API.
//!
//! Usage: `cargo run -p tst-examples --example extract_klv -- <input.ts> [output_prefix]`
//!
//! This is a teaching demo for the demuxer — every non-obvious choice has
//! a `// why+how` comment, as every example in this repository does. The
//! companion `extract_video_au` example shows the same pattern for the
//! video side.
//!
//! Output: one `<prefix>_NNNN_<sync|async>.klv` file per metadata event,
//! written next to the input file by default. The `_sync` / `_async`
//! suffix lets downstream consumers tell at a glance whether a file is a
//! sync record (H.222.0 V9 §2.12.4.2 Metadata_AU_cell, also defined in
//! ST 1402.2 §9.4.1 — paired in time with a video AU) or an
//! async / free-running record (typically 1–10 Hz, no AU pairing). Both
//! flavors are valid MISB ST 0601 KLV LS bytes and decode the same way
//! via `klv::st0601::decode`; the suffix is purely advisory.
//!
//! For sync KLV the demuxer has already unwrapped the Metadata_AU_cell, so
//! the file contains the inner KLV LS bytes — NOT the AU-cell-wrapped
//! form. The AU cell's Precision Time Stamp Pack timestamp is dropped on
//! the floor by this example; if you need it, switch to using `pts` on
//! the parent event (see the `Sample` arm of the match below — same
//! `pts` field shape).

use std::env;
use std::fs;
use std::path::Path;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, MetadataKind};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: extract_klv <input.ts> [output_prefix]");
        std::process::exit(2);
    }
    let input = Path::new(&args[1]);
    // Default prefix is the input file's stem, so
    // `extract_klv foo.ts` writes `foo_0000_async.klv`, `foo_0001_async.klv`,
    // etc., next to `foo.ts`. Pass an explicit second arg to override.
    let default_prefix = input.file_stem().unwrap().to_string_lossy().into_owned();
    let prefix = args.get(2).map(String::as_str).unwrap_or(&default_prefix);

    let bytes = fs::read(input).expect("read input");

    // `Demuxer::new` uses lenient defaults: missing `metadata_descriptor`,
    // `stream_type` drift, PUSI-mid-PES, and PCR anomalies all surface as
    // `DemuxEvent::NonConformant` events but never fail the demux loop.
    // For dev-tooling / extraction purposes that's exactly what we want —
    // a non-conformant capture should still yield as much extractable
    // KLV as possible. To opt into hard-fail behavior for compliance
    // testing, swap to `Demuxer::with_config(DemuxerConfig::builder().strict(StrictMode::Full)
    // .build())`; see `docs/guides/pipeline.md` for the
    // strict-mode contract.
    let mut d = Demuxer::new();

    // Single-shot feed of the whole file. The demuxer accepts arbitrary
    // byte slices and recovers TS sync internally — no need to align on
    // 188-byte boundaries. For streaming use you'd `feed` repeatedly as
    // bytes arrive; the event queue accumulates across feeds.
    d.feed(&bytes)
        .expect("demuxer recovered from any non-conformance");

    // `flush` is the canonical end-of-stream signal. Without it, any PES
    // still mid-reassembly when the file ends would be silently dropped
    // — including potentially the last KLV record. Always call `flush`
    // when you've reached the end of the input you intend to feed.
    d.flush();

    let mut idx = 0usize;
    while let Some(event) = d.next_event() {
        // We only care about metadata events here; ignore PSI updates,
        // video samples, discontinuities, and non-conformance reports.
        // A real pipeline would log discontinuities + non-conformance
        // for observability.
        if let DemuxEvent::Metadata { payload, kind, .. } = event {
            // Distinguish sync vs async KLV in the filename:
            //
            // - `KlvSyncAuCell`: PMT stream_type 0x15 (Synchronous
            //   Metadata) carrying Metadata_AU_cell-wrapped KLV per
            //   H.222.0 V9 §2.12.4.2 (also defined in ST 1402.2 §9.4.1). The
            //   demuxer has already stripped the AU cell header — the
            //   `payload` is the inner KLV LS bytes, not the wrapped
            //   form. The parent event's `pts` is the AU cell's
            //   Precision Time Stamp Pack timestamp.
            //
            // - `KlvAsync`: PMT stream_type 0x06 (private data) with a
            //   `KLVA` registration descriptor, carrying bare KLV LS
            //   bytes (no AU cell wrap). The parent event's `pts` is
            //   the PES PTS (asynchronous; not necessarily aligned
            //   with any video frame).
            //
            // - `Unknown(stream_type)`: metadata-shaped PID with an
            //   unrecognized `stream_type`. Rare — included for
            //   completeness so this example never silently drops an
            //   event it doesn't understand.
            let suffix = match kind {
                MetadataKind::KlvSyncAuCell { .. } => "sync",
                MetadataKind::KlvAsync => "async",
                MetadataKind::Unknown(_) => "unknown",
            };
            let path = input
                .parent()
                .unwrap_or(Path::new("."))
                .join(format!("{prefix}_{idx:04}_{suffix}.klv"));
            fs::write(&path, &payload).expect("write klv blob");
            eprintln!("wrote {} ({} bytes)", path.display(), payload.len());
            idx += 1;
        }
    }

    println!("extracted {idx} KLV blob(s)");
}
