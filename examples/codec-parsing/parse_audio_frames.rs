//! Walk a `.ts` file with the demuxer, parse audio frames per PID, and
//! log first-change-only `(sample_rate, channel_count, profile/layer)`
//! tuples. Same stance as `parse_video_parameters.rs`:
//! we don't dump every frame's metadata (that's noise), only the points
//! where the typed surface changes.
//!
//! Why this is teaching code (not a minimal repro):
//! - Shows the dispatch off `SamplePayload::Audio { codec, frames }` —
//!   the handoff between the demuxer event surface (carriage layer) and
//!   the `codec::*` parser layer (per-frame metadata).
//! - Shows iterator-yields-Result handling: skip-on-Err is the right
//!   default for "tell me what's in this file"; consumers needing
//!   stop-on-Err semantics replace `.filter_map(Result::ok)` with
//!   `.collect::<Result<Vec<_>, _>>()`.
//! - Shows the `frames` accessor giving the caller the full frame
//!   bytes — the path to per-caller CRC verification or downstream
//!   re-emission.
//!
//! Run:
//!   cargo run -p tst-examples --example parse_audio_frames -- path/to/some.ts

use std::collections::HashMap;
use std::env;
use std::fs;
use std::process::ExitCode;
use tst_core::codec;
use tst_core::mpegts::demux::{AudioCodec, DemuxEvent, Demuxer, SamplePayload};

#[derive(Default, Debug, PartialEq, Eq)]
struct AudioState {
    sample_rate_hz: Option<u32>,
    channels: Option<u8>,
    layer_or_profile: Option<String>,
}

fn main() -> ExitCode {
    let path = match env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: parse_audio_frames <file.ts>");
            return ExitCode::from(2);
        }
    };

    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("read {}: {}", path, e);
            return ExitCode::from(1);
        }
    };

    let mut demuxer = Demuxer::new();
    if let Err(e) = demuxer.feed(&bytes) {
        eprintln!("demuxer feed: {:?}", e);
        return ExitCode::from(1);
    }
    demuxer.flush();

    let mut state_per_pid: HashMap<u16, AudioState> = HashMap::new();

    while let Some(ev) = demuxer.next_event() {
        if let DemuxEvent::Sample {
            stream,
            payload: SamplePayload::Audio { codec, frames, .. },
            ..
        } = ev
        {
            let pid = stream.pid;
            let state = state_per_pid.entry(pid).or_default();
            match codec {
                AudioCodec::Mp2 => {
                    // Both Mp2 and the corpus's Layer III streams come
                    // through here (stream_type 0x03/0x04 maps to
                    // AudioCodec::Mp2; the layer is recovered from
                    // the frame header itself).
                    for r in codec::mpegaudio::frames(&frames).filter_map(Result::ok) {
                        let new_state = AudioState {
                            sample_rate_hz: Some(r.sample_rate_hz),
                            channels: Some(r.channels),
                            layer_or_profile: Some(format!("{:?}", r.layer)),
                        };
                        if *state != new_state {
                            println!(
                                "PID 0x{:04x} mpegaudio: layer={:?} sample_rate={} channels={} version={:?}",
                                pid, r.layer, r.sample_rate_hz, r.channels, r.version
                            );
                            *state = new_state;
                        }
                    }
                }
                AudioCodec::Aac => {
                    for r in codec::aac::frames(&frames).filter_map(Result::ok) {
                        // C7 — `r.channels()` returns `None` for streams
                        // whose channel layout is PCE-defined
                        // (`channel_configuration == 0`). Walking the PCE
                        // to recover the count is deferred; the iterator
                        // surfaces None here so callers see "unknown"
                        // rather than silently dropping the frame (the
                        // pre-C7 behavior).
                        let new_state = AudioState {
                            sample_rate_hz: Some(r.sample_rate_hz),
                            channels: r.channels(),
                            layer_or_profile: Some(format!("{:?}", r.profile)),
                        };
                        if *state != new_state {
                            let channels_display = match r.channels() {
                                Some(c) => c.to_string(),
                                None => "pce-defined".to_string(),
                            };
                            println!(
                                "PID 0x{:04x} aac-adts: profile={:?} sample_rate={} channels={} blocks_per_frame={}",
                                pid,
                                r.profile,
                                r.sample_rate_hz,
                                channels_display,
                                r.num_raw_data_blocks
                            );
                            *state = new_state;
                        }
                    }
                }
                _ => {
                    // AacLatm / Ac3 are not yet covered by this slice.
                    // The deferred-features.md "AAC LATM / AC-3 frame
                    // parsers" entry is the trigger to revisit.
                }
            }
        }
    }

    if state_per_pid.is_empty() {
        eprintln!("(no audio PESes found in {})", path);
    }
    ExitCode::SUCCESS
}
