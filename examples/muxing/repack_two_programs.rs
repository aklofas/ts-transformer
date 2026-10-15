//! Repack two single-program TS inputs into one 2-program TS multiplex.
//!
//! ## Use case
//!
//! A gimbaled-platform aggregator receives two independent aircraft feeds,
//! each emitting its own single-program MPEG-TS (one EO or IR video stream
//! and one KLV metadata stream). The aggregator needs to forward both feeds
//! to a ground station over a **single SRT socket**, without requiring the
//! receiver to open two sockets on separate ports.
//!
//! The solution is a multi-program TS multiplex: both programs travel inside
//! the same byte stream, identified by their `program_number` in the PAT.
//! A conformant receiver demuxes the PAT, discovers both programs, and
//! dispatches samples to the appropriate program-aware consumer.
//!
//! ## Why PIDs must be renumbered
//!
//! ISO 13818-1 §2.4.3.6 (Transport Stream): **every PID in a transport
//! stream must be unique**. The two input files are independent single-
//! program captures — they both use the same canonical PID range
//! (0x1011 for video, 0x1031 for KLV). Attempting to put them in the same
//! multiplex without renaming would yield duplicate PIDs, which is an
//! illegal TS structure. Downstream demuxers would bind the first-seen
//! program's PIDs and silently drop the second.
//!
//! This example illustrates the repacking workflow: demux both inputs
//! with their original PIDs, then re-mux onto non-overlapping PID ranges:
//! - Program 1 keeps 0x1011 / 0x1031 (unchanged from input 1).
//! - Program 2 gets 0x1111 / 0x1131 (shifted to a non-colliding range).
//!
//! ## How to run
//!
//! ```text
//! cargo run -p tst-examples --example repack_two_programs -- input1.ts input2.ts output.ts
//! ```
//!
//! If you have a single test capture and want to see the multi-program
//! structure, you can pass the same file twice — the output will have two
//! identical programs with different program numbers, which is legal TS
//! and useful as a smoke test.
//!
//! ## How to verify
//!
//! ```text
//! ffprobe -show_programs output.ts 2>/dev/null | grep -E "program_num|nb_streams"
//! ```
//!
//! Expected output:
//! ```text
//! program_num=1
//! nb_streams=2
//! program_num=2
//! nb_streams=2
//! ```
//!
//! With TSDuck:
//! ```text
//! tsanalyze output.ts --pid-analysis
//! ```
//! You should see PIDs 0x1000, 0x1011, 0x1031 owned by program 1 and
//! PIDs 0x1100, 0x1111, 0x1131 owned by program 2.

use std::env;
use std::fs;
use std::io::Write;
use tst_core::codec::NalUnit;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, SamplePayload, VideoPayload, split_video};
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── Argument parsing ─────────────────────────────────────────────────────
    let args: Vec<String> = env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: {} <input1.ts> <input2.ts> <output.ts>", args[0]);
        std::process::exit(1);
    }
    let (in1_path, in2_path, out_path) = (&args[1], &args[2], &args[3]);

    // ── Build the 2-program output config ────────────────────────────────────
    //
    // Why these PMT PIDs:
    // - 0x1000 is program 1's PMT PID. The stream PIDs 0x1011 and 0x1031 are
    //   well above 0x1000, so there's no conflict with the PMT itself.
    // - 0x1100 is program 2's PMT PID. Same reasoning — 0x1111 / 0x1131 are
    //   both higher than 0x1100. The PMT and stream PIDs for each program must
    //   not overlap each other OR any PID in any other program.
    //
    // Why program 2 gets 0x11xx PIDs and not 0x10xx:
    // - Program 1 already claims 0x1011 and 0x1031. ISO 13818-1 forbids PID
    //   reuse across programs. We chose 0x1100/0x1111/0x1131 — a clean 256-
    //   PID stride from program 1's range — so the renumbering is visually
    //   obvious in a Wireshark or tsanalyze capture. Any non-colliding PIDs
    //   below 0x1FFF (the max valid PID is 0x1FFE; 0x1FFF is the null packet
    //   PID) would also be acceptable.
    //
    // Why KlvStreamType::PrivateData with carries_pts=false:
    // - PrivateData (stream_type 0x06) is the async KLV shape — no PTS in
    //   the KLV PES. The muxer carries the raw KLV bytes through unchanged.
    //   This is the correct shape for most async KLV sources.
    // - If the input was SynchronousMetadata (stream_type 0x15), the input
    //   demuxer peeled the H.222.0 § 2.12.4.2 5-byte Metadata_AU_cell
    //   header and surfaced the inner KLV bytes as
    //   `MetadataKind::KlvSyncAuCell`. For re-muxing as sync you'd use
    //   KlvStreamType::SynchronousMetadata + carries_pts=true; the muxer
    //   auto-wraps each push in a fresh AU cell header. Here we use
    //   PrivateData to keep the example simple and work for both input
    //   KLV flavors.
    let config = {
        let mut prog0 = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog0.add_video(0x1011, VideoCodec::H264);
        prog0.add_klv(0x1031, KlvStreamType::PrivateData, false);
        prog0.pcr_pid(0x1011);
        let mut prog1 = MuxerProgramConfigBuilder::new(2, 0x1100);
        prog1.add_video(0x1111, VideoCodec::H264);
        prog1.add_klv(0x1131, KlvStreamType::PrivateData, false);
        prog1.pcr_pid(0x1111);
        let mut b = MuxerConfig::builder();
        b.add_program(prog0.build());
        b.add_program(prog1.build());
        b.build()?
    };

    let mut muxer = Muxer::new(config)?;

    // Resolve per-program stream handles. The `_for_program(N)` accessors
    // look up streams by program_number (not by index), so program order
    // in the MuxerConfig doesn't matter.
    let p1_video = muxer.video_handles_for_program(1)?[0];
    let p1_klv = muxer.klv_handles_for_program(1)?[0];
    let p2_video = muxer.video_handles_for_program(2)?[0];
    let p2_klv = muxer.klv_handles_for_program(2)?[0];

    // Open the output file before the demux loops so we can drain as we go.
    // Draining after each pushed sample is the canonical file-output pattern
    // (see mux_dual_camera.rs): it keeps the muxer's internal ring buffer
    // from filling up. The default capacity is 10 000 TS packets (~1.8 MB at
    // 188 B/packet) — generous for a single frame, but a full-length ISR
    // capture at 25 Mbps can easily exceed it if you push the whole file
    // before pulling. The pull_buf at 188 × 256 ≈ 48 KB gives 256 packets
    // per drain call, which is a good throughput / stack-usage trade-off.
    let mut out_file = fs::File::create(out_path)?;
    let mut pull_buf = vec![0u8; 188 * 256];

    // Helper closure: drain everything currently queued in the muxer to disk.
    // Defined once and called after every event push so the queue stays small.
    let mut drain = |muxer: &mut Muxer, out_file: &mut fs::File| -> std::io::Result<()> {
        loop {
            let n = muxer.pull(&mut pull_buf);
            if n == 0 {
                break;
            }
            out_file.write_all(&pull_buf[..n])?;
        }
        Ok(())
    };

    // ── Demux input 1, push onto program 1 PIDs ──────────────────────────────

    let bytes1 = fs::read(in1_path)?;
    let mut demux1 = Demuxer::new();

    // Lenient demuxer (the default). For a repacking workflow this is almost
    // always the right posture — the goal is to pass through every sample we
    // can recover, even from slightly non-conformant captures. NonConformant
    // events surface the anomalies without blocking the demux.
    demux1.feed(&bytes1)?;

    // flush() commits the trailing AU. Video PES with PES_packet_length=0 (the
    // common shape for large video AUs) is only finalised when the *next* PES
    // header arrives. At end-of-file there is no next PES, so flush() is
    // mandatory or the last frame of every file is silently dropped.
    demux1.flush();

    while let Some(event) = demux1.next_event() {
        repack_event(event, p1_video, p1_klv, &mut muxer)?;
        drain(&mut muxer, &mut out_file)?;
    }

    // ── Demux input 2, push onto program 2 PIDs ──────────────────────────────

    let bytes2 = fs::read(in2_path)?;
    let mut demux2 = Demuxer::new();
    demux2.feed(&bytes2)?;
    demux2.flush();

    while let Some(event) = demux2.next_event() {
        repack_event(event, p2_video, p2_klv, &mut muxer)?;
        drain(&mut muxer, &mut out_file)?;
    }

    // Final drain: any PSI/PCR packets buffered after the last sample push.
    drain(&mut muxer, &mut out_file)?;

    println!("Wrote output to {out_path}");
    println!();
    println!("Verify with:");
    println!("  ffprobe -show_programs {out_path} 2>/dev/null | grep -E 'program_num|nb_streams'");
    println!("  # expect: program_num=1, nb_streams=2, program_num=2, nb_streams=2");
    println!();
    println!("Or with TSDuck:");
    println!("  tsanalyze {out_path} --pid-analysis");

    Ok(())
}

/// Consume one `DemuxEvent` from either input and push samples onto the
/// given program's muxer handles. Called once per event for each input.
///
/// Why a free function rather than inline in main: the two demux loops are
/// structurally identical — same event shape, same push calls, different
/// handles. Factoring it out makes the symmetry explicit and avoids copy-
/// paste drift if the logic changes (e.g. you add H.265 support or KLV
/// filtering).
fn repack_event(
    event: DemuxEvent,
    video_handle: tst_core::mpegts::mux::VideoStreamHandle,
    klv_handle: tst_core::mpegts::mux::KlvStreamHandle,
    muxer: &mut Muxer,
) -> Result<(), Box<dyn std::error::Error>> {
    match event {
        // ── Video sample ─────────────────────────────────────────────────────
        DemuxEvent::Sample {
            pts,
            payload:
                SamplePayload::Video {
                    codec,
                    // Raw-first: the demuxer hands you the EXACT encoded access
                    // unit — already Annex-B-framed with its start codes intact.
                    // For a transparent repack this is exactly what we want: no
                    // NAL split, no Annex-B reconstruction — forward `raw`
                    // straight into the output muxer's `push_video_to` (which
                    // validates the leading start code and wraps its own PES).
                    raw,
                    // `random_access_indicator` is the TS adaptation-field RAI
                    // bit from the input PES_start packet (ISO/IEC 13818-1
                    // §2.4.3.4). Forwarding this directly to the output's
                    // `key_frame` is cheaper and more authoritative than
                    // re-scanning NALs for nal_type==5 — it reflects the
                    // upstream encoder's own RA signal.
                    random_access_indicator,
                    av1_carriage,
                    ..
                },
            ..
        } => {
            // Why only H.264: this example is scoped to H.264 fixtures. An
            // H.265 source would arrive as `VideoCodec::H265`; the output
            // MuxerConfig is H.264-only, so other codecs are skipped below.
            // Adding H.265 support is straightforward — build the output
            // config for H.265 and push with `VideoCodec::H265`.
            use tst_core::codec::VideoCodec as DemuxCodec;
            if matches!(codec, DemuxCodec::H264) && !raw.is_empty() {
                // Forward the input AU's adaptation-field RAI as the output's
                // key_frame signal. As a defensive cross-check we also scan
                // for an IDR NAL (nal_type==5) via the opt-in `split_video` —
                // if RAI is set OR an IDR slice is present, mark as key_frame.
                // The OR fallback handles upstream muxers that fail to set RAI
                // on IDR boundaries (some software encoders).
                let (split, _issues) =
                    split_video(&raw, codec, av1_carriage.unwrap_or_default());
                let has_idr = matches!(&split, VideoPayload::Nals(nals)
                    if nals.iter().any(|n| matches!(n, NalUnit::H264 { nal_type: 5, .. })));
                let key_frame = random_access_indicator || has_idr;
                // `raw` is the complete Annex-B AU; the muxer carries the whole
                // AU in one PES, matching what real encoders emit.
                muxer.push_video_to(video_handle, &raw, pts, key_frame)?;
            }
            // Non-H.264 codecs (H.265 etc.) are silently skipped here.
            // The output MuxerConfig is H.264-only; pushing H.265 onto an
            // H.264 stream would be a mux error. A production repacker
            // would detect the codec at ProgramMap time and build the
            // output MuxerConfig accordingly.
        }

        // ── KLV metadata ─────────────────────────────────────────────────────
        DemuxEvent::Metadata {
            pts, kind, payload, ..
        } => {
            // Forward both KlvAsync and KlvSyncAuCell into the output's
            // PrivateData (async) KLV stream. The demuxer already peeled
            // the H.222.0 § 2.12.4.2 5-byte Metadata_AU_cell header for
            // KlvSyncAuCell events — `payload` is the inner bare KLV LS
            // bytes in both cases. The output config uses PrivateData
            // (async), so the muxer carries `payload` through unchanged.
            // If you wanted to preserve the sync shape, you'd configure
            // the output with KlvStreamType::SynchronousMetadata +
            // carries_pts=true; the output muxer would auto-wrap each
            // push in a fresh AU cell header.
            //
            // Unknown metadata stream types are passed through as-is.
            // They may not decode correctly at the receiver if the stream_type
            // in the output PMT doesn't match, but including them avoids
            // silently dropping vendor-specific metadata.
            let _ = kind; // provenance noted above; not used for routing
            // `metadata_service_id` goes into the AU cell header per H.222.0
            // §2.12.4.2 / ST 1402.2 App. B Table 2 for SynchronousMetadata
            // streams (stream_type 0x15); silently ignored for PrivateData
            // streams (0x06) and for the transparent repack case here.
            // Spec default is 0x00; use non-zero only when mirroring a
            // metadata_klva() PMT descriptor `service_id` from the source.
            muxer.push_klv_to(klv_handle, &payload, pts, 0x00)?;
        }

        // ── Everything else is ignored ────────────────────────────────────────
        //
        // ProgramMap events carry PSI topology from the *input* mux — not
        // relevant to the output mux, which generates its own PAT/PMT from
        // the MuxerConfig we built above.
        //
        // Discontinuity and NonConformant events are diagnostic; a production
        // repacker would log them for observability (e.g. correlating a
        // playback glitch with a continuity_counter jump in the input).
        DemuxEvent::ProgramMap(_)
        | DemuxEvent::Discontinuity { .. }
        | DemuxEvent::NonConformant { .. }
        // Emitted only by `ManagedDemuxReceiver` reconnect-aware shells;
        // this repacker reads from a file via plain `Demuxer`, never
        // emits this variant. Included for exhaustive matching.
        | DemuxEvent::ReconnectDiscontinuity => {}

        // Guard: exhaustive match so future DemuxEvent variants cause a
        // compile error here rather than being silently swallowed.
        DemuxEvent::Sample { .. } => {
            // Non-video sample (audio, subtitle, unknown stream_type).
            // Skipped — the output config is video+KLV only.
        }
    }

    Ok(())
}
