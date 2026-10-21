//! Smoke test for the `file` feature helpers.

#![cfg(feature = "file")]

use std::io::Write;

use tst_core::io_file::{
    TryDemuxFromFile, demux_file, try_demux_from_file_with_config, write_mux_to_file,
};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::DemuxerConfig;
use tst_core::mpegts::mux::{Muxer, MuxerConfig};

/// Build a minimal single-program Muxer with one H.264 video stream.
fn minimal_muxer() -> Muxer {
    Muxer::new(MuxerConfig::default()).expect("Muxer::new")
}

/// Push one IDR frame and one KLV blob so the muxer has output to drain.
fn push_one_keyframe(mux: &mut Muxer) {
    // Synthetic Annex-B H.264 IDR NAL (start code + IDR header byte).
    let nal: Vec<u8> = {
        let mut v = vec![0x00, 0x00, 0x00, 0x01];
        v.push(0x65); // forbidden_zero=0, nal_ref_idc=3, nal_unit_type=5 (IDR)
        v.extend(vec![0xA5u8; 64]);
        v
    };
    mux.push_video(&nal, Pts90khz::new(0), true)
        .expect("push_video");
}

#[test]
fn demux_file_round_trip() {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");

    let mut mux = minimal_muxer();
    push_one_keyframe(&mut mux);
    write_mux_to_file(&mut mux, tmp.path()).expect("write_mux_to_file");

    let events = demux_file(tmp.path()).expect("demux_file");
    assert!(!events.is_empty(), "expected at least one DemuxEvent");
}

#[test]
fn try_demux_from_file_matches_demux_file_on_clean_input() {
    // On a clean valid file the streaming iterator must yield the same
    // event sequence as the full-file helper, wrapped in `Ok`.
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");

    let mut mux = minimal_muxer();
    push_one_keyframe(&mut mux);
    write_mux_to_file(&mut mux, tmp.path()).expect("write_mux_to_file");

    let whole = demux_file(tmp.path()).expect("demux_file");
    let streamed: Vec<_> = TryDemuxFromFile::open(tmp.path())
        .expect("TryDemuxFromFile::open")
        .collect();

    assert!(
        !whole.is_empty(),
        "precondition: demux_file must yield events"
    );
    assert_eq!(
        streamed.len(),
        whole.len(),
        "streaming iterator must yield as many items as demux_file on clean input"
    );
    for (i, (r, expected)) in streamed.iter().zip(&whole).enumerate() {
        let got = r
            .as_ref()
            .unwrap_or_else(|e| panic!("clean file must not produce errors, got {e:?}"));
        assert_eq!(
            format!("{got:?}"),
            format!("{expected:?}"),
            "event {i} differs"
        );
    }
}

#[test]
fn try_demux_from_file_surfaces_truncated_pes() {
    // Build a valid file, then truncate it inside what would be a PES
    // payload. The current `feed()` doesn't itself error on a truncated
    // tail — it just buffers — but `flush()` and the subsequent EOF are
    // benign, so this test mostly proves we don't synthesize a false
    // error for plain short reads. The "surfaces errors" path is
    // exercised by the malformed-content test below.
    //
    // What this test DOES guarantee: a small/odd-length tail must not
    // crash the fallible iterator, and the iterator must terminate
    // cleanly (one path or the other) — never deadlock.
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let mut mux = minimal_muxer();
    push_one_keyframe(&mut mux);
    write_mux_to_file(&mut mux, tmp.path()).expect("write_mux_to_file");

    // Truncate to a non-188 boundary so the demuxer sees an odd tail.
    let len = std::fs::metadata(tmp.path()).expect("metadata").len();
    let truncated = (len / 188) * 188 - 47; // chop the last full packet + 47 bytes
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(tmp.path())
        .expect("open rw");
    f.set_len(truncated).expect("set_len");
    drop(f);

    let iter = TryDemuxFromFile::open(tmp.path()).expect("TryDemuxFromFile::open");
    let collected: Vec<_> = iter.collect();
    // Don't assert event count — truncation may drop the only IDR. Just
    // assert termination + the absence of an InvalidData error (truncated
    // mid-packet input is benign; the feed buffers it and EOF discards
    // the partial tail).
    for r in &collected {
        if let Err(e) = r {
            // Other I/O errors would be a real bug; truncation alone
            // shouldn't synthesize InvalidData.
            assert_ne!(
                e.kind(),
                std::io::ErrorKind::InvalidData,
                "truncated tail should not be flagged as InvalidData"
            );
        }
    }
}

#[test]
fn try_demux_from_file_surfaces_feed_error_on_garbage() {
    // Write > 4 MiB of non-sync-byte garbage (no 0x47) to a file. The
    // demuxer's `SyncBufExhausted` cap fires at MAX_SYNC_BUF_BYTES
    // (4 MiB). `TryDemuxFromFile` must surface `Err(InvalidData)` rather
    // than ending the stream early.
    let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
    // Use 0x00 (definitely not 0x47) to force the sync-search window to
    // walk the whole buffer without finding a packet boundary.
    let garbage = vec![0x00u8; 5 * 1024 * 1024];
    tmp.write_all(&garbage).expect("write garbage");
    tmp.flush().expect("flush");

    let iter = try_demux_from_file_with_config(tmp.path(), DemuxerConfig::default())
        .expect("try_demux_from_file_with_config");
    let collected: Vec<_> = iter.collect();

    // Must contain at least one Err.
    let err_count = collected.iter().filter(|r| r.is_err()).count();
    assert!(
        err_count >= 1,
        "garbage stream must surface at least one Err; got {} items, all Ok",
        collected.len()
    );
    // The first error must be InvalidData (mapped from DemuxError).
    let first_err = collected
        .iter()
        .find_map(|r| r.as_ref().err())
        .expect("must have an Err");
    assert_eq!(
        first_err.kind(),
        std::io::ErrorKind::InvalidData,
        "feed-error must map to InvalidData, got: {first_err:?}"
    );

    // After the Err, the iterator must be exhausted (terminator contract).
    // Rebuild and walk it manually to verify.
    let mut iter2 = try_demux_from_file_with_config(tmp.path(), DemuxerConfig::default())
        .expect("try_demux_from_file_with_config");
    let mut saw_err = false;
    for r in iter2.by_ref() {
        if r.is_err() {
            saw_err = true;
            break;
        }
    }
    assert!(saw_err, "did not observe the staged error");
    assert!(
        iter2.next().is_none(),
        "iterator must yield None after an Err"
    );
}

#[test]
fn write_mux_creates_nonempty_file() {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");

    let mut mux = minimal_muxer();
    push_one_keyframe(&mut mux);
    write_mux_to_file(&mut mux, tmp.path()).expect("write_mux_to_file");

    let metadata = std::fs::metadata(tmp.path()).expect("metadata");
    assert!(metadata.len() > 0, "output file must not be empty");
    // TS files are multiples of 188 bytes.
    assert_eq!(
        metadata.len() % 188,
        0,
        "TS file length must be a multiple of 188"
    );
}
