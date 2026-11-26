//! Dual-camera (EO + IR) + KLV → MPEG-TS file.
//!
//! Multi-stream within ONE program. EO + IR + KLV all bound together as
//! a single logical channel. Receivers tune to "the program" and get all
//! three time-aligned via shared PCR.
//!
//! For multi-PROGRAM examples (multiple independent channels in one TS,
//! e.g. two aircraft each with their own program), see
//! `examples/repack_two_programs.rs`.
//!
//! Demonstrates the multi-stream shape of `mpegts::mux::Muxer`:
//! - Two video elementary streams on distinct PIDs (0x1011 EO,
//!   0x1021 IR), both H.264.
//! - One KLV elementary stream on PID 0x1031, async (no PTS in PES).
//! - PCR pinned to the EO video stream — the convention is "pin PCR
//!   to whichever stream you'd want a downstream demuxer to use as its
//!   master clock," and that's typically the primary visible-light
//!   feed.
//!
//! This example doesn't push real H.264 data — it pushes minimal
//! Annex-B NAL units + a minimal KLV blob, just enough that the
//! resulting `.ts` is structurally valid (PAT/PMT/PCR/PES all present
//! on the right PIDs). Run `ffprobe -show_streams /tmp/dual_camera.ts` to
//! see the muxer reports two video streams + one data stream.
//!
//! Invocation:
//!   cargo run -p tst-examples --example mux_dual_camera

use std::fs::File;
use std::io::Write;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::descriptors;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

fn main() -> std::io::Result<()> {
    // Build a multi-stream MuxerConfig:
    //
    // - Video PIDs are deliberately 16 apart (0x1011 + 0x10 = 0x1021).
    //   No spec requires this, but spreading PIDs by ≥16 makes them
    //   easier to spot in a Wireshark capture and avoids accidentally
    //   ending up adjacent to the PMT PID (0x1000 by default).
    // - KLV is async (`carries_pts: false`). For sync KLV (PTS aligned
    //   with video), use `KlvStreamType::SynchronousMetadata` + true;
    //   the muxer auto-wraps each push in an H.222.0 § 2.12.4.2 5-byte
    //   Metadata_AU_cell header — pass raw KLV LS bytes.
    // - PCR is pinned to the EO video PID (0x1011). With multi-stream
    //   the auto-default is also "first video stream's PID," so this
    //   `.pcr_pid(0x1011)` is redundant — included here so consumers
    //   reading the example see the explicit form.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264); // EO (visible-light)
        // Tag 0xFF (user_private) is the de-facto label slot used in the
        // wild — it's ISO-reserved, but real-world senders of this shape
        // put the human-readable stream name here. Our own demuxer's
        // `extract_user_label` reads it last, when none of the conformant
        // sources before it (a component, stream identifier, metadata or
        // ISO 639 language descriptor) gives a label.
        // TSDuck will flag it as "Forbidden Descriptor Id 0xFF" — that's
        // expected; the bytes are still parsed. See
        // docs/guides/mpegts-mux.md for the full descriptor builder menu
        // including spec-conformant alternatives (Component 0x50, Stream
        // Identifier 0x52).
        // Builders now return Result so oversized payloads surface as
        // DescriptorError::TooLarge rather than silently truncating.
        // Static byte strings here are well under the 255-byte cap, so
        // `.expect()` is safe and documents the precondition.
        prog.stream_descriptors_for_video(
            0,
            vec![descriptors::user_private(b"EO 1080p").expect("label within 255-byte cap")],
        )
        .unwrap();
        prog.add_video(0x1021, VideoCodec::H264); // IR (thermal)
        prog.stream_descriptors_for_video(
            1,
            vec![descriptors::user_private(b"IR 640x480").expect("label within 255-byte cap")],
        )
        .unwrap();
        prog.add_klv(0x1031, KlvStreamType::PrivateData, false);
        // KLV is PrivateData here (no PTS in PES), so we skip the
        // canonical 0x26 + 0x27 metadata-service descriptor pair — those
        // are conventional only for SynchronousMetadata KLV (stream_type
        // 0x15). For PrivateData (stream_type 0x06) a user_private label
        // is enough: receivers that call us out by label still find this
        // stream, and tools like ffprobe surface it as "Data: KLVA".
        prog.stream_descriptors_for_klv(
            0,
            vec![descriptors::user_private(b"KLV_META").expect("label within 255-byte cap")],
        )
        .unwrap();
        prog.pcr_pid(0x1011);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().expect("config validation")
    };

    let mut mux = Muxer::new(cfg).expect("muxer construction");

    // Resolve handles AFTER `Muxer::new` — they're tied to this muxer.
    // Order matches the order of `add_video` / `add_klv` calls above.
    let eo = mux.video_stream_handle(0).expect("EO handle");
    let ir = mux.video_stream_handle(1).expect("IR handle");
    let klv = mux.klv_stream_handle(0).expect("KLV handle");

    // Synthetic minimal Annex-B NAL — 4-byte start code + nal_unit_type
    // 0x67 (SPS) + a couple of payload bytes. ffprobe identifies the
    // codec from the PMT stream_type byte (0x1B for H.264), not from
    // the NAL contents — so a tiny stub is enough for the routing test.
    let nal_eo = [0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0xFF];
    let nal_ir = [0x00, 0x00, 0x00, 0x01, 0x67, 0xBB, 0xFF];

    // Minimal 17-byte KLV — 16-byte UAS Datalink LS UL + 1-byte length=0.
    let klv_blob: [u8; 17] = [
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00, 0x00,
    ];

    // Cross-platform temp path: `/tmp/...` on Linux/macOS, `%TEMP%\...` on Windows.
    let out_path = std::env::temp_dir().join("dual_camera.ts");
    let mut out = File::create(&out_path)?;
    let mut buf = vec![0u8; 188 * 64];

    // Push 30 frames at 30 fps (3000 90 kHz ticks per frame).
    // - First frame is a keyframe on both video streams (random_access bit
    //   in the adaptation field).
    // - All three streams share the same PTS timeline. Sync KLV would
    //   make this strict; with async KLV (this example) the PTS is just
    //   a wall-clock-ish driver for PSI/PCR cadence.
    for i in 0..30 {
        let pts = i * 3000;
        let key = i == 0;
        mux.push_video_to(eo, &nal_eo, Pts90khz::new(pts), key)
            .expect("EO push");
        mux.push_video_to(ir, &nal_ir, Pts90khz::new(pts), key)
            .expect("IR push");
        // `metadata_service_id` goes into the AU cell header per H.222.0
        // §2.12.4.2 / ST 1402.2 App. B Table 2 for SynchronousMetadata
        // streams (stream_type 0x15); silently ignored for PrivateData
        // streams (0x06) like the one used here. The spec default is 0x00
        // — use a non-zero value only when mirroring a metadata_klva()
        // PMT descriptor `service_id` you supplied at config time.
        mux.push_klv_to(klv, &klv_blob, Pts90khz::new(pts), 0x00)
            .expect("KLV push");

        // Drain after every frame so the muxer's internal buffer doesn't
        // fill — the default `buffer_packets: 10000` is generous (~600 ms
        // at 25 Mbps), but a tight loop is the canonical pattern for
        // file output.
        loop {
            let n = mux.pull(&mut buf);
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
        }
    }

    println!("Wrote {}", out_path.display());
    println!("Try: ffprobe -show_streams {}", out_path.display());
    Ok(())
}
