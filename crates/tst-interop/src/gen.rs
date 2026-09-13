//! `gen` subcommand: generate one profile's synthetic MPEG-TS/KLV traffic
//! offline (no wall-clock sleeps, no transport) and write it to a file.
//!
//! This is the producer half of the zero-third-party-tools self-roundtrip
//! gate — see `tests/roundtrip.rs`.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use tst_core::codec::misp_time::MispTimestamp;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::Muxer;

use crate::fixtures;
use crate::mux_setup;
use crate::profiles::{KlvMode, Profile, VideoCodec};
use crate::schedule::{self, Event, PTS_HZ};

/// Generate `seconds` of synthetic traffic for profile `p` and write the
/// resulting MPEG-TS bytes to `out_path`.
///
/// Pacing is entirely offline: PTS advances `90_000 / p.fps` ticks per
/// video frame and `90_000 / p.klv_hz` ticks per KLV record, both from
/// `p.start_pts_ticks`, with no wall-clock sleeps — every event for the
/// whole `seconds` window is computed up front, sorted into ascending PTS
/// order, then pushed and drained in one pass. Audio (when `p.audio`)
/// runs on its own cadence — real AAC framing at 1024 samples/frame and
/// `schedule::AUDIO_SAMPLE_RATE_HZ` (48 kHz) — independent of the video
/// frame rate; see `schedule::build_schedule`'s doc comment and
/// `oracles::audio`, which checks that cadence on the wire.
///
/// `two-program` profiles push the same video AU / KLV record onto every
/// configured program's handles at the same PTS — the "duplicate the
/// video+KLV pair in program 2" shape.
pub fn run(p: &Profile, seconds: f64, out_path: &Path) -> io::Result<()> {
    let cfg = mux_setup::build_config(p);
    let mut mux = Muxer::new(cfg).expect("mux_setup::build_config always returns a valid config");
    // Handles must come from THIS muxer, not a throwaway one built from a
    // cloned config elsewhere — see mux_setup's doc comment on why
    // build_config doesn't hand them back itself.
    let video_handles = mux.video_handles();
    let klv_handles = mux.klv_handles();
    let audio_handle = mux.audio_handles().into_iter().next();
    let mut out = File::create(out_path)?;

    let (_, events) = schedule::build_schedule(p, seconds);

    let mut pull_buf = [0u8; 1316];
    for (pts_ticks, event) in events {
        let pts = Pts90khz::new(pts_ticks);
        match event {
            Event::Video { frame_idx } => {
                let (au, keyframe) = fixtures::video_au(p.video, frame_idx);
                for &handle in &video_handles {
                    if p.klv == KlvMode::AsyncWithMisp {
                        // ST 0604 SEI carriage is H.264/H.265-only; the
                        // `misp` profile is always H.264 (see
                        // `profiles::PROFILES`), so this is unreachable
                        // for the other codecs today, but the guard keeps
                        // it that way rather than assuming.
                        debug_assert!(matches!(p.video, VideoCodec::H264 | VideoCodec::H265));
                        let misp_us = (pts_ticks as u64).wrapping_mul(1_000_000) / PTS_HZ as u64;
                        let misp = MispTimestamp::micros(misp_us, 0x1F);
                        mux.push_video_misp_to(handle, &au, pts, keyframe, &misp)
                            .map_err(io::Error::other)?;
                    } else {
                        mux.push_video_to(handle, &au, pts, keyframe)
                            .map_err(io::Error::other)?;
                    }
                }
            }
            Event::Klv { seq } => {
                let record = fixtures::klv_record(seq);
                for &handle in &klv_handles {
                    mux.push_klv_to(handle, &record, pts, 0x00)
                        .map_err(io::Error::other)?;
                }
            }
            Event::Audio { frame_idx } => {
                if let Some(handle) = audio_handle {
                    let frame = fixtures::aac_frame(frame_idx);
                    mux.push_audio_to(handle, pts, &frame)
                        .map_err(io::Error::other)?;
                }
            }
        }

        loop {
            let n = mux.pull(&mut pull_buf);
            if n == 0 {
                break;
            }
            out.write_all(&pull_buf[..n])?;
        }
    }

    out.flush()
}
