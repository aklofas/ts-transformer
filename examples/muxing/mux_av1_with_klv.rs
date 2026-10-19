//! AV1 + sync-KLV (H.222.0 V9 §2.12.4.2 Metadata_AU_cell) flavor of `mux_to_file`.
//!
//! Diffs against `mux_h266_with_klv.rs` to show what flips when
//! switching from a NAL-shaped codec (H.264 / H.265 / H.266) to an
//! OBU-shaped codec (AV1):
//!
//!   - VideoCodec::H266 -> VideoCodec::Av1
//!   - Annex-B NAL builder (4-byte start code + NAL header + body)
//!     -> OBU builder (header byte + LEB128 size + body)
//!
//! AV1 in MPEG-TS uses PMT stream_type 0x06 plus an auto-emitted
//! `AV01` registration descriptor (per the AV1-in-MPEG-2-TS binding
//! §2.1). The muxer adds the descriptor automatically when
//! `VideoCodec::Av1` is configured — the same 0x06 byte that DVB-style
//! private-data streams use, so the registration descriptor is what
//! tells receivers "this is AV1".
//!
//! ## OBU framing vs Annex-B NAL framing
//!
//! AV1 has **no Annex-B start codes** (no `0x00000001` separator).
//! Instead, each OBU is self-describing and length-prefixed. Per
//! AV1 spec §5.3.2 the OBU header is a single byte:
//!
//! ```text
//!   forbidden_bit  f(1) = 0
//!   obu_type       f(4)
//!   extension_flag f(1) = 0  (no temporal/spatial layering here)
//!   has_size_field f(1) = 1  (REQUIRED by binding §3.1)
//!   reserved_1bit  f(1) = 0
//! ```
//!
//! ...followed immediately by an LEB128-encoded `obu_size`, then the
//! payload bytes. Concatenating OBUs with no separator produces a
//! complete access unit. The AV1-in-MPEG-2-TS binding §3.1 mandates
//! `obu_has_size_field = 1` so that demultiplexers can walk the OBU
//! stream without a separate framing layer.
//!
//!   cargo run -p tst-examples --example mux_av1_with_klv -- /tmp/av1.ts

use std::env;
use std::fs::File;
use std::io::Write;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

// PIDs are 13-bit identifiers in the TS header. The reserved well-known
// values are 0x0000 (PAT) and 0x1FFF (null padding); elementary streams
// live in 0x0010..=0x1FFE. Within that range the muxer doesn't care
// which PID a stream sits on as long as PIDs don't collide — the
// receiver discovers them from the PMT.
//
// The specific values 0x1011 (video) and 0x1031 (KLV) are the same as
// `MuxerConfig::default()`'s defaults. We set them explicitly here so the
// diff against `mux_h266_with_klv.rs` highlights only the codec/OBU
// knobs and not unrelated PID rearrangement.
const VIDEO_PID: u16 = 0x1011;
const KLV_PID: u16 = 0x1031;

/// Build an AV1 OBU with `obu_has_size_field = 1` (required by the
/// AV1-in-MPEG-2-TS binding §3.1).
///
/// Per AV1 spec §5.3.2:
///   header byte: forbidden(1)=0 | obu_type(4) | extension_flag(1)=0
///                | has_size_field(1)=1 | reserved(1)=0
///   then LEB128 size, then payload.
fn build_av1_obu(obu_type: u8, body: &[u8]) -> Vec<u8> {
    // (obu_type << 3) | 0b010 — extension=0, has_size=1, reserved=0.
    let header = (obu_type << 3) | 0x02;
    let mut v = vec![header];
    // For sizes < 128, single-byte LEB128 = the size byte itself
    // (high bit 0 = "this is the last byte"). Real encoders need a
    // full LEB128 encoder for larger OBUs; this synthetic helper
    // intentionally caps at 127 bytes so the assertion below catches
    // accidental misuse.
    assert!(
        body.len() < 128,
        "synthetic AU helper supports only small bodies"
    );
    v.push(body.len() as u8);
    v.extend_from_slice(body);
    v
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_path = env::args().nth(1).unwrap_or_else(|| "av1.ts".into());

    // -----------------------------------------------------------------
    // Canonical "build a MuxerConfig from scratch" pattern using the
    // standalone sub-builder shape.
    //
    //   1. `MuxerProgramConfigBuilder::new(program_number, pmt_pid)`
    //      constructs a stream-attaching builder for one program.
    //   2. `add_video` / `add_klv` register elementary streams inside
    //      that program; here we configure AV1 (stream_type 0x06
    //      with the AOM-defined `AV01` registration descriptor, both
    //      auto-emitted from `VideoCodec::Av1`) plus a sync-KLV
    //      stream sharing the program's PCR.
    //   3. `prog.build()` finalizes the program config (a plain owned
    //      value) and is then bound onto the top-level
    //      `MuxerConfig::builder()` via `add_program`.
    //   4. The top-level `b.build()?` runs `MuxerConfig::validate` and
    //      applies defaults for PCR/PSI/buffer intervals.
    //
    // The bind-then-build separation keeps each program description
    // self-contained — useful when you want to construct multiple
    // programs procedurally before assembling the final config.
    // -----------------------------------------------------------------
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(VIDEO_PID, VideoCodec::Av1);
        // SynchronousMetadata (PMT stream_type 0x15) is strict
        // sync KLV per MISB ST 1402.2 § 9.4.1 / ITU-T H.222.0
        // V9 § 2.12.4.2. The muxer auto-prepends a 5-byte
        // Metadata_AU_cell header (Tables 2-155+2-156:
        // metadata_service_id + sequence_number + flags +
        // AU_cell_data_length) before each push; pass raw KLV
        // LS bytes, not pre-wrapped bytes. PTS lives in the
        // PES header (per § 2.12.4.1). See
        // `docs/guides/mpegts-mux.md` for the full contract.
        //
        // SynchronousMetadata requires `carries_pts: true` —
        // the PTS is what lets a receiver align each metadata
        // record with the corresponding video frame. The
        // combo `SynchronousMetadata + carries_pts: false` is
        // rejected by `MuxerConfig::validate`.
        prog.add_klv(KLV_PID, KlvStreamType::SynchronousMetadata, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()?
    };

    // `Muxer::new` runs `MuxerConfig::validate` and returns
    // `MuxError::InvalidConfig` if anything is wrong (duplicate PIDs,
    // PSI interval below 10 ms, `SynchronousMetadata` with
    // `carries_pts: false`, etc.). The `?` propagates the error to
    // `main`'s `Box<dyn Error>` return.
    let mut mux = Muxer::new(cfg)?;
    let mut out = File::create(&out_path)?;

    // 30 fps live-video shape over 5 seconds wall-clock = 150 frames.
    // PTS is on the 90 kHz MPEG-TS clock, so 90_000/30 = 3000 ticks
    // per frame — `pts = i * pts_increment` walks the clock cleanly
    // at 30 fps cadence.
    let frame_rate = 30;
    let total_frames = 5 * frame_rate;
    let pts_increment = 90_000 / frame_rate as i64;

    // 1316 == `SrtTransport::DEFAULT_PAYLOAD` — also a typical SRT
    // payload size, big enough to hold exactly 7 TS packets
    // (7 * 188 = 1316). The muxer doesn't care about this size; it
    // just dictates how many TS packets `pull` returns at a time.
    // Sized here so the example's drain loop mirrors what an
    // SRT-bound caller would use.
    let mut buf = [0u8; 1316];

    for i in 0..total_frames {
        let pts = (i as i64) * pts_increment;
        // One key frame every 2 seconds (every 60th frame at 30 fps) —
        // a common GOP cadence for live encoders. For AV1 the "key
        // frame" boundary is signalled inside the FrameHeader OBU
        // (frame_type = KEY_FRAME); the muxer is opaque to OBU
        // contents, so the `key` flag here only drives the TS
        // adaptation field's `random_access_indicator` bit, which
        // receivers use to identify seek points.
        let key = i % (frame_rate * 2) == 0;

        // Synthetic AV1 access unit. Per AV1 spec §5.3.1, an access
        // unit is a sequence of OBUs starting with a Temporal
        // Delimiter and ending right before the next Temporal
        // Delimiter. The minimum useful AU has:
        //
        //   - TemporalDelimiter (obu_type=2, body always empty)
        //   - SequenceHeader    (obu_type=1, on key frames / changes)
        //   - FrameHeader       (obu_type=3)
        //   - TileGroup         (obu_type=4, the actual coded data)
        //
        // Real encoders may also emit MetadataOBU (5), Padding (15),
        // TileList (8), or use the combined Frame OBU (6) which fuses
        // FrameHeader + TileGroup. Body bytes here are placeholders
        // (the muxer is opaque to OBU contents and just packs whatever
        // bytes you give it into PES packets); a real encoder would
        // emit valid AV1 syntax here.
        let mut au = Vec::new();
        au.extend(build_av1_obu(2, &[])); // TemporalDelimiter
        if key {
            // SequenceHeader is mandatory on the first AU and at
            // resolution / profile / level changes. Emitting on every
            // key frame is the conservative live-encoder choice — it
            // lets a receiver tune in mid-stream at any GOP boundary.
            au.extend(build_av1_obu(1, &[0x00, 0x00]));
        }
        au.extend(build_av1_obu(3, &[0x00])); // FrameHeader
        // TileGroup body padded with filler. The size variation
        // (1000..1200) mimics real bitstream variability so downstream
        // tooling sees a believable shape — but capped under 128 by
        // the OBU helper, so we batch the filler into one larger OBU
        // would need a real LEB128 encoder. Instead, emit one
        // bounded-size TileGroup; the muxer concatenates everything
        // into the PES payload regardless.
        let tile_body: Vec<u8> = std::iter::repeat_n(0xA5, 100 + (i as usize % 27)).collect();
        au.extend(build_av1_obu(4, &tile_body));

        // One AU = one PES packet by construction. The `key` flag
        // drives the TS adaptation field's `random_access_indicator`
        // bit, which receivers use to identify seek points.
        mux.push_video(&au, Pts90khz::new(pts), key)?;

        // Synthetic inner KLV blob — 50 random-looking bytes. Real
        // ST 0601 KLV is built via `tst_core::klv::st0601` (see the
        // `klv_encode_minimal` example); for the muxer demo any
        // bytes will do.
        let inner_klv: Vec<u8> = (0..50).map(|j| (i as u8).wrapping_add(j as u8)).collect();
        // Sync KLV (`SynchronousMetadata` + `carries_pts: true`):
        // the muxer auto-wraps in a 5-byte H.222.0 § 2.12.4.2
        // Metadata_AU_cell header, then puts the AU cell bytes in
        // a PES whose header carries the PTS we pass below. The
        // PTS is what lets the receiver align each metadata record
        // back to the right video frame. With async KLV (the default
        // `PrivateData` + `carries_pts: false`) the receiver gets a
        // stream of KLV records but can't directly correlate each
        // to a specific video AU.
        // `metadata_service_id` goes into the AU cell header per H.222.0
        // §2.12.4.2 / ST 1402.2 App. B Table 2 for SynchronousMetadata
        // streams (stream_type 0x15); it is silently ignored for PrivateData
        // streams (0x06) like the one configured above. The spec default is
        // 0x00 — use a non-zero value only when you have multiple independent
        // metadata services on the same PID (e.g. to mirror a `service_id`
        // byte in a metadata_klva() PMT descriptor supplied at config time).
        mux.push_klv(&inner_klv, Pts90khz::new(pts), 0x00)?;

        // Standard pull pattern: drain after every push so muxer
        // memory stays bounded. `pull` returns 0 when there's nothing
        // more to emit right now, otherwise a multiple of 188 (one
        // or more whole TS packets that fit in `buf`).
        loop {
            let n = mux.pull(&mut buf);
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
        }
    }

    println!(
        "wrote {} AV1 frames (sync KLV, {} s) to {}",
        total_frames,
        total_frames / frame_rate,
        out_path
    );
    Ok(())
}
