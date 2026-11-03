//! Read an MPEG-TS file and dump the full `DemuxEvent` stream to stdout.
//!
//! Why this exists: the simplest "what does my stream look like?"
//! diagnostic. Drop a `.ts` capture in front of `Demuxer::new()`, watch
//! the typed events fall out — PSI topology, every video AU, every KLV
//! record, plus discontinuities and non-conformance reports inline. For
//! triaging a capture that won't play in some downstream tool this is
//! usually the first thing to run.
//!
//! Usage: `cargo run -p tst-examples --example demux_to_events -- <input.ts>`
//!
//! What to look for in the output:
//! - One `ProgramMap` line per PSI version (typically just one for a
//!   stable file). The PIDs + stream_types + KLV link rows are the
//!   topology summary — if a stream you expected is missing, the PMT
//!   never declared it.
//! - `Sample` lines on the video PID, one per access unit. The `nals=N`
//!   count tells you how many NAL units the AU split into; for a typical
//!   IDR you'll see SPS + PPS + IDR slice (3+ NALs); for a P-frame just
//!   one slice NAL (1).
//! - `Metadata` lines on the KLV PID. `kind=KlvSyncAuCell` means the
//!   demuxer unwrapped a Metadata_AU_cell (H.222.0 V9 §2.12.4.2,
//!   also defined in ST 1402.2 §9.4.1); `kind=KlvAsync` means bare
//!   KLV LS bytes from a private-data PID. Both decode the same way via
//!   `klv::st0601::decode` — the kind is just provenance.
//! - `Discontinuity` and `NonConformant` lines flag problems that the
//!   lenient demuxer recovered from. A `Discontinuity::ContinuityJump`
//!   usually means dropped UDP packets upstream; `NonConformant` lines
//!   typically point at encoder bugs (missing `metadata_descriptor`,
//!   wrong `stream_type`, etc.). Both are useful triage signal — the
//!   raw bytes still parsed fine, but the stream isn't fully spec.
//!
//! Counterpoint: when you want to convert a live SRT stream to typed
//! events for a downstream program, use `srt_recv_typed.rs` — same
//! event shape, but reading from a connected SRT socket instead of a
//! file.

use std::env;
use std::fs;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, SamplePayload, VideoPayload, split_video};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: demux_to_events <input.ts>");
        std::process::exit(2);
    }
    let bytes = fs::read(&args[1]).expect("read input");

    // Lenient by default. Strict modes are useful for compliance
    // workflows (CI gating an encoder change, e.g.) but for stream
    // triage lenient is what you want — the demuxer keeps going past
    // every recoverable problem and surfaces what it found as
    // `NonConformant` events. To opt into hard-fail behavior swap to
    // `Demuxer::with_config(DemuxerConfig::builder().strict(StrictMode::Full).build())`; see
    // `docs/guides/pipeline.md` for the strict-mode contract.
    let mut d = Demuxer::new();

    // Single-shot feed of the whole file. The demuxer accepts arbitrary
    // byte slices and re-syncs on TS packet boundaries internally — no
    // alignment requirement on the caller. For a streaming source you
    // would call `feed` repeatedly as bytes arrive; the event queue
    // accumulates across feeds.
    // Lenient mode never errors on PSI / PES non-conformance — those
    // surface as inline `NonConformant` events. It can still return
    // `Unrecoverable` (no 0x47 sync byte within the search window;
    // i.e. input isn't TS at all) or `MalformedPes` — both are fatal
    // for an offline triage tool, so panic with a useful message.
    d.feed(&bytes)
        .expect("input could not be decoded as MPEG-TS");

    // `flush` is the canonical end-of-stream signal. It matters because
    // a video PES with `PES_packet_length=0` (the common shape for video
    // AUs that exceed 65535 bytes — i.e. nearly all real video) is only
    // committed when the *next* PES starts. At end-of-file there is no
    // next PES, so the trailing AU sits in the reassembler until you
    // call `flush`. Without this the last frame of every clip would
    // disappear silently.
    d.flush();

    while let Some(e) = d.next_event() {
        match e {
            DemuxEvent::ProgramMap(m) => {
                println!("--- ProgramMap (program {}) ---", m.program_number);
                println!("  pcr_pid=0x{:04X}", m.pcr_pid);
                for s in &m.streams {
                    println!(
                        "  PID 0x{:04X}: stream_type=0x{:02X} kind={:?}",
                        s.pid,
                        s.stream_type.as_byte(),
                        s.kind
                    );
                }
                // KLV-to-video links: declared by the PMT's
                // `metadata_descriptor` (`Declared`), inferred from
                // single-video + single-KLV topology (`Inferred`), or
                // forced by the caller via `DemuxerConfigBuilder::link_klv`
                // (`Override`). `Inferred` is best-effort — treat it as
                // a hint, not authority.
                for l in &m.klv_links {
                    println!(
                        "  klv 0x{:04X} -> video 0x{:04X} ({:?})",
                        l.klv_pid, l.video_pid, l.source
                    );
                }
            }
            DemuxEvent::Sample {
                stream,
                pts,
                dts,
                payload,
            } => {
                let pts = pts.as_ticks();
                match payload {
                    SamplePayload::Video {
                        codec,
                        // Raw-first: the demuxer no longer parses the video
                        // elementary stream — it hands you the exact encoded
                        // access unit. Parsing NAL/OBU units is now an OPT-IN
                        // call via `split_video` (mirrors how KLV surfaces raw
                        // bytes with an opt-in decode). `_issues` carries any
                        // ES-conformance findings; we drop them here.
                        raw,
                        // `random_access_indicator` is sourced from the TS
                        // adaptation-field RAI bit on the PES_start packet
                        // (ISO/IEC 13818-1 §2.4.3.4). True on AUs the encoder
                        // marked as decoder-resync points (IDR / CRA / etc.).
                        random_access_indicator,
                        av1_carriage,
                        ..
                    } => {
                        let (payload, _issues) =
                            split_video(&raw, codec, av1_carriage.unwrap_or_default());
                        match payload {
                            VideoPayload::Nals(nals) => {
                                println!(
                                    "Sample PID=0x{:04X} pts={pts} dts={dts:?} codec={codec:?} nals={} rai={random_access_indicator}",
                                    stream.pid,
                                    nals.len()
                                );
                            }
                            VideoPayload::Obus(obus) => {
                                // OBU-shaped video (AV1).
                                println!(
                                    "Sample PID=0x{:04X} pts={pts} dts={dts:?} codec={codec:?} obus={} rai={random_access_indicator}",
                                    stream.pid,
                                    obus.len()
                                );
                            }
                        }
                    }
                    // Audio + Subtitle are reserved variants today (no
                    // typed codec values are defined yet) but matching
                    // them keeps this example exhaustive — adding e.g.
                    // `AudioCodec::Aac` later won't silently change
                    // behavior here.
                    SamplePayload::Audio { codec, frames } => {
                        println!(
                            "Sample PID=0x{:04X} pts={pts} audio={codec:?} bytes={}",
                            stream.pid,
                            frames.len()
                        );
                    }
                    SamplePayload::Subtitle { codec, payload } => {
                        println!(
                            "Sample PID=0x{:04X} pts={pts} subtitle={codec:?} bytes={}",
                            stream.pid,
                            payload.len()
                        );
                    }
                    SamplePayload::Unknown { stream_type, raw } => {
                        println!(
                            "Sample PID=0x{:04X} pts={pts} stream_type=0x{:02X} bytes={} (unrecognized ES)",
                            stream.pid,
                            stream_type.as_byte(),
                            raw.len()
                        );
                    }
                }
            }
            DemuxEvent::Metadata {
                stream,
                pts,
                kind,
                payload,
            } => {
                // For sync KLV the AU cell wrap has already been peeled
                // by the demuxer, so `payload` is the inner KLV LS
                // bytes — feed directly to `klv::st0601::decode`. The
                // `pts` field is the AU cell's metadata access-unit
                // timestamp for sync KLV, or the raw PES PTS for async.
                println!(
                    "Metadata PID=0x{:04X} pts={} kind={kind:?} bytes={}",
                    stream.pid,
                    pts.as_ticks(),
                    payload.len()
                );
            }
            // A `Discontinuity` is a real signal — typically dropped UDP
            // packets upstream causing a continuity_counter jump, or a
            // PES exceeding the per-PID reassembly cap. Surface them so
            // operators can correlate playback glitches with the wire.
            DemuxEvent::Discontinuity { stream, kind } => {
                println!("Discontinuity PID=0x{:04X} {kind:?}", stream.pid);
            }
            // `NonConformant` means the demuxer recovered from a
            // spec violation. Common in production captures: encoders
            // omit the `metadata_descriptor`, mix sync/async stream
            // types incorrectly, or emit PUSI mid-PES. Worth logging
            // but doesn't block extraction.
            DemuxEvent::NonConformant { stream, issue } => {
                println!("NonConformant PID=0x{:04X} {issue:?}", stream.pid);
            }
            // Only emitted by `ManagedDemuxReceiver` (tst-pipeline) on
            // transport reconnect; not produced by the file-driven
            // `Demuxer` this example uses. `DemuxEvent` is non-exhaustive,
            // so the wildcard also absorbs variants added later.
            _ => {}
        }
    }
}
