//! Real-corpus structural tests for `mpegts::demux` over `tests/fixtures/local/*.ts`.
//!
//! The fixtures directory is gitignored — sensitive captures don't go on
//! the public repo. This test silently passes when the directory is
//! absent (CI case); when present, every `*.ts` file is fed through a
//! fresh `Demuxer` and asserted against structural invariants:
//!
//! - `ProgramMap` event fires (PAT + PMT parsed cleanly).
//! - At least one `Sample` with `SamplePayload::Video { .. }` event fires.
//! - `MalformedPes` from `feed` is a real failure (the curated corpus
//!   should be clean).
//! - `Unrecoverable` from `feed` is a per-file skip-with-warning — a
//!   non-TS file misnamed `.ts` shouldn't fail the whole test.
//!
//! KLV presence and non-conformant event counts are tallied per file
//! and reported via `eprintln!` for visibility (not asserted: shape A
//! captures may be KLV-bearing, shape C may not be — see
//! tests/coverage/TEST_CORPUS.md).
//!
//! `Demuxer::flush()` is invoked after the last `feed` to surface the
//! trailing video AU that real-corpus files almost always end mid-emit
//! on (final PES with length=0 only completes on next PUSI, which
//! never arrives at end-of-file).

use std::fs;
use std::path::PathBuf;
use tst_core::error::DemuxError;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, MetadataKind, SamplePayload};

fn fixtures_dir() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("local");
    if p.exists() { Some(p) } else { None }
}

#[test]
fn corpus_files_demux_cleanly_in_lenient_mode() {
    let dir = match fixtures_dir() {
        Some(d) => d,
        None => {
            eprintln!("no local fixtures; skipping");
            return;
        }
    };

    let mut total = 0usize;
    let mut total_skipped = 0usize;

    for entry in fs::read_dir(&dir).expect("read_dir on existing fixtures dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("ts") {
            continue;
        }

        let bytes = fs::read(&path).expect("read .ts fixture");
        let mut d = Demuxer::new();

        // Lenient mode: non-conformance surfaces as `NonConformant` events,
        // not errors. The remaining error variants are:
        // - `Unrecoverable`: byte stream isn't TS at all (skip with warning;
        //   a misnamed file shouldn't fail the whole suite).
        // - `MalformedPes`: structurally invalid PES header (real failure;
        //   the curated corpus should be clean).
        // - `StrictRejection`: only fires under `StrictMode::*`; we use
        //   the default (`StrictMode::Off`), so unreachable here.
        match d.feed(&bytes) {
            Ok(()) => {}
            Err(DemuxError::Unrecoverable { after_bytes }) => {
                eprintln!(
                    "{}: skipped — sync recovery failed after {after_bytes} bytes (not a TS file?)",
                    path.display()
                );
                total_skipped += 1;
                continue;
            }
            Err(e) => panic!("{}: demux failed: {e}", path.display()),
        }

        // Surface the trailing AU. Real captures almost always end
        // mid-PES (the final video PES with length=0 only completes
        // on the next PUSI, which never arrives at end-of-file).
        d.flush();

        let mut saw_pmap = false;
        let mut saw_video = false;
        let mut saw_klv = false;
        let mut nonconformant = 0usize;
        while let Some(e) = d.next_event() {
            match e {
                DemuxEvent::ProgramMap(_) => saw_pmap = true,
                DemuxEvent::Sample {
                    payload: SamplePayload::Video { .. },
                    ..
                } => saw_video = true,
                DemuxEvent::Metadata {
                    kind: MetadataKind::KlvSyncAuCell { .. } | MetadataKind::KlvAsync,
                    ..
                } => saw_klv = true,
                DemuxEvent::NonConformant { .. } => nonconformant += 1,
                _ => {}
            }
        }

        assert!(saw_pmap, "{}: no ProgramMap event", path.display());
        assert!(saw_video, "{}: no video samples", path.display());

        eprintln!(
            "{}: video={saw_video} klv={saw_klv} nonconformant={nonconformant}",
            path.display()
        );
        total += 1;
    }

    eprintln!("processed {total} corpus file(s); skipped {total_skipped}");
}
