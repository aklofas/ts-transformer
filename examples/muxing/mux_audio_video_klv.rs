//! Example: multi-stream MPEG-TS with H.264 video, MP2 audio, and KLV metadata.
//!
//! Demonstrates the typical gimbaled-platform pipeline shape: one program with
//! synchronized audio, video, and asynchronous KLV metadata in a single
//! transport stream.
//!
//! Why three streams in one program (not three programs):
//!   - Single PMT keeps the demuxer simple (one classify pass per PID).
//!   - One PCR (Program Clock Reference) for everything avoids timing skew
//!     between video and audio at the receiver.
//!   - Audio PTS aligned to video means lip-sync is a consumer's choice
//!     downstream, not a transport concern. Both encode to the same
//!     90 kHz clock and share PCR cadence.
//!
//! Why this example uses MP2 audio instead of AAC:
//!   - MP2 (MPEG-1 Layer II) is the most common audio codec in real-world
//!     ISR captures (~85% of corpus). AAC ADTS is second most common.
//!   - The shape is identical: provide pre-framed audio bytes with a PTS
//!     timestamp. Swapping to AAC is two-line change: use AudioCodec::Aac
//!     and replace build_mp2_silent_frame with an ADTS encoder.
//!
//! Why the KLV stream carries no PTS:
//!   - We use KlvStreamType::PrivateData with `carries_pts: false` (async KLV).
//!     The muxer emits one KLV AU per tick, not anchored to video frame times.
//!   - For sync KLV (locked to video keyframes), use
//!     KlvStreamType::SynchronousMetadata with `carries_pts: true`. The
//!     muxer auto-wraps each push in an H.222.0 § 2.12.4.2 5-byte
//!     Metadata_AU_cell header — pass raw KLV LS bytes. See
//!     mux_h265_with_klv.rs for that case.
//!
//! The synthetic encoder placeholders (build_h264_keyframe_au,
//! build_mp2_silent_frame) are not real decodable frames — they are shape
//! placeholders so the example compiles without codec dependencies. Real
//! consumers swap in their own encoder output.
//!
//! Run:  `cargo run -p tst-examples --example mux_audio_video_klv -- /tmp/output.ts`
//! Then: `ffprobe /tmp/output.ts | grep codec_type` to confirm three streams.

use std::fs::File;
use std::io::Write;

use tst_core::klv::st0601::{UasDatalinkLs, encode_to_vec};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    AudioCodec, KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

// Clippy's `field_reassign_with_default` would prefer struct-update syntax,
// but we use field-by-field reassignment on purpose to group related ST 0601
// tags with teaching comments about why they belong together (like
// klv_encode_minimal.rs does).
#[allow(clippy::field_reassign_with_default)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Default to a cross-platform temp path when no argv path is supplied.
    let out_path = std::env::args().nth(1).unwrap_or_else(|| {
        std::env::temp_dir()
            .join("audio_video_klv.ts")
            .to_string_lossy()
            .into_owned()
    });

    // Build a single-program config: video + audio + KLV.
    //
    // PIDs are conventional across the corpus:
    //   - 0x100: video (main camera)
    //   - 0x200: KLV (metadata)
    //   - 0x300: audio (mono or stereo)
    //   - 0x1000: PMT (program map table)
    //
    // All streams clock from the same 90 kHz reference. PCR
    // (Program Clock Reference) pace defaults to the first video
    // stream's PID when not pinned explicitly.
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        // KLV at PMT stream_type 0x06 (PrivateData) + KLVA registration
        // descriptor. This is the broadly-recognized async-KLV carriage form.
        // Real streams: PrivateData (async) or SynchronousMetadata (sync, keyframe-locked).
        prog.add_klv(0x200, KlvStreamType::PrivateData, false); // carries_pts: false (async)
        // MP2 audio at PMT stream_type 0x03. To swap to AAC ADTS
        // (0x0F) or AAC LATM (0x11), change AudioCodec variant.
        prog.add_audio(0x300, AudioCodec::Mp2);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build()?
    };

    let mut muxer = Muxer::new(cfg)?;
    let mut out = File::create(&out_path)?;

    // Emit 90 video AUs (one second at 30 fps) + paired audio frames
    // + one KLV record every second.
    //
    // PTS is in 90 kHz ticks. 90,000 ticks per second; 3,000 per frame
    // at 30 fps. Audio and video both advance by 3,000 per loop so
    // they remain time-locked at the transport level.
    for i in 0..90i64 {
        let pts = 90_000 + i * 3000;

        // Video: a minimal H.264 SPS/PPS/IDR sequence. Real code would
        // fetch frames from an encoder (libx264, x265, or hardware codec).
        // We use a synthetic placeholder so the example is dependency-free.
        let nal_h264 = build_h264_keyframe_au(i);
        muxer.push_video(&nal_h264, Pts90khz::new(pts), /*key_frame=*/ true)?;

        // Audio: a minimal MP2 frame. Same caveat — synthetic placeholder.
        // Real code would pull pre-framed bytes from an audio encoder
        // (libmp2enc, ffmpeg libavcodec, etc.). The library treats bytes
        // as opaque — frame headers and sync words are caller's responsibility.
        let mp2_frame = build_mp2_silent_frame();
        muxer.push_audio(&mp2_frame, Pts90khz::new(pts))?;

        // KLV: emit an ST 0601 record once per second (every 30 frames).
        // For async KLV, the PTS argument is still honored (it sets the
        // PES PTS field) but does not anchor to video keyframes.
        if i % 30 == 0 {
            let mut rec = UasDatalinkLs::default();

            // ST 0601 Tag 2: Precision Time Stamp, microseconds since
            // Unix epoch. Convert from 90 kHz ticks to microseconds.
            rec.timestamp_us = Some(((pts as u64) * 1_000_000) / 90_000);

            // ST 0601 Tag 10: Platform Designation (human-readable).
            // Max length is 127 bytes per ST 0601.
            rec.platform_designation = Some("EXAMPLE".to_string());

            // Encode to KLV wire format (BER length, TL tags, checksum).
            let klv = encode_to_vec(&rec)?;

            // `metadata_service_id` goes into the AU cell header per H.222.0
            // §2.12.4.2 / ST 1402.2 App. B Table 2 for SynchronousMetadata
            // streams (stream_type 0x15); silently ignored for PrivateData
            // streams (0x06) like the one configured here. The spec default
            // is 0x00.
            muxer.push_klv(&klv, Pts90khz::new(pts), 0x00)?;
        }

        // Drain TS bytes to disk. pull() returns byte count written;
        // muxer buffers internally and emits when PSI/PCR cadence or
        // payload threshold is met. Between loop iterations there may
        // be zero bytes; that's normal (PSI emission is ~100 ms intervals).
        let mut buf = vec![0u8; 188 * 64];
        let n = muxer.pull(&mut buf);
        out.write_all(&buf[..n])?;
    }

    out.flush()?;
    eprintln!("wrote {out_path}");
    eprintln!(
        "validate: ffprobe -v error -show_streams {} | grep codec_type",
        out_path
    );
    eprintln!("expected: codec_type=video, codec_type=audio, codec_type=data (KLV)");
    Ok(())
}

/// Build a minimal H.264 keyframe access unit (SPS + PPS + IDR slice).
///
/// This is a synthetic placeholder — real code uses an encoder's output.
/// A proper IDR would require profile/level/constraint encoding, proper
/// slice syntax, etc. We provide the Annex B wrapper (0x00_00_00_01 sync
/// codes) + minimal NAL headers so the muxer can fragment into PES packets.
fn build_h264_keyframe_au(_frame_num: i64) -> Vec<u8> {
    let mut au = Vec::new();

    // SPS (Sequence Parameter Set, NAL type 7)
    // Annex B start code + placeholder SPS bytes.
    au.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    au.extend_from_slice(&[0x67, 0x42, 0x00, 0x1F, 0xE9, 0x02, 0x80, 0x14, 0x07, 0x80]);

    // PPS (Picture Parameter Set, NAL type 8)
    // Annex B start code + placeholder PPS bytes.
    au.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    au.extend_from_slice(&[0x68, 0xCE, 0x06, 0xE2]);

    // IDR slice (NAL type 5)
    // Annex B start code + placeholder slice bytes (not a real decodable
    // IDR, but the muxer only checks for Annex B wrappers + NAL headers).
    au.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    au.extend_from_slice(&[
        0x65, 0x88, 0x80, 0x40, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00,
    ]);

    au
}

/// Build a minimal MP2 frame (sync header + silent payload).
///
/// MP2 (MPEG-1 Audio Layer II) frame structure:
///   - 11 sync bits (all 1)
///   - 2 version bits (11 = MPEG-1)
///   - 2 layer bits (01 = Layer II)
///   - 1 protection bit (1 = no CRC)
///   - 4 bitrate index bits (1011 = 192 kbps)
///   - 2 sample rate bits (00 = 44.1 kHz)
///   - 1 padding bit
///   - 1 private bit
///   - Followed by the frame payload (silence for this example).
///
/// This is a synthetic placeholder — real code gets frames from an encoder
/// like libtwolame or ffmpeg's libavcodec.
fn build_mp2_silent_frame() -> Vec<u8> {
    // MP2 header: 0xFF 0xFD 0xB0 0xC4 encodes:
    //   sync=0xFFF (11 ones)
    //   version=11 (MPEG-1)
    //   layer=01 (Layer II)
    //   protection=1 (no CRC)
    //   bitrate=1011 (192 kbps)
    //   samplerate=00 (44.1 kHz)
    //   padding=0
    //   private=0
    // Then 140 bytes of silence (zero payload for this example).
    let mut frame = vec![0xFF, 0xFD, 0xB0, 0xC4];
    frame.extend(std::iter::repeat_n(0u8, 140));
    frame
}
