//! `send` subcommand: build a live transport from a URL, then push one
//! profile's synthetic MPEG-TS/KLV traffic through a [`MuxSender`],
//! paced to real (wall-clock) time.
//!
//! Unlike `gen::run` (this crate's OFFLINE producer — every event for
//! the whole window is computed and written with no sleeps, as fast as
//! the muxer/file allow), this is the LIVE producer: the same
//! per-profile event schedule (see `gen.rs`'s doc comment for the
//! shared shape), but each push waits until its target PTS offset has
//! actually elapsed on the wall clock before firing — matching how a
//! real sender paces traffic to the stream's own clock.

use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tst_core::codec::misp_time::MispTimestamp;
use tst_core::mpegts::common::Pts90khz;
use tst_core::transport::{BrokenCause, Transport, TransportError};
use tst_pipeline::{ManagedTransport, MuxSender, ReconnectPolicy};

use crate::cli::write_json;
use crate::fixtures::{self, AuSizeMode};
use crate::mux_setup;
use crate::profiles::{KlvMode, Profile, VideoCodec};
use crate::report_types::CellMetrics;
use crate::schedule::{self, Event, PTS_HZ};
use crate::transport::{self, Teeing};
use crate::verify;

/// Build a transport from `url` for profile `p` and push `seconds` of
/// its synthetic traffic through it, paced to real time. Returns the
/// sent-side ground truth as a [`CellMetrics`] (video/KLV/audio counts,
/// `klv_set_sha256` over the records actually pushed, and
/// `bytes`/`stream_sha256` from the exact TS bytes handed to the
/// transport). Writes the same metrics as JSON to `json_out` (or
/// stdout for `"-"`) when given.
///
/// `no_klv_digest` skips accumulating the per-record digest list this
/// function would otherwise need to compute `klv_set_sha256` —
/// `CellMetrics::klv_set_sha256` comes back `None` instead. See that
/// field's own doc comment for why a multi-day soak run needs this.
///
/// `au_sizes` picks the video AU size regime — `Compact` (the
/// original tiny fixtures every interop-matrix cell uses) or
/// `Realistic` (GOP-structured multi-KB AUs, the soak's true-bandwidth
/// mode); see [`AuSizeMode`].
pub fn run(
    p: &Profile,
    url: &str,
    seconds: f64,
    json_out: Option<&str>,
    no_klv_digest: bool,
    au_sizes: AuSizeMode,
) -> Result<CellMetrics, String> {
    let transport = transport::make_send(url)?;
    let metrics = send_over_transport(p, transport, seconds, no_klv_digest, au_sizes)?;
    if let Some(target) = json_out {
        write_json(target, &metrics)?;
    }
    Ok(metrics)
}

/// Core of [`run`], split out so a caller that already holds a
/// constructed transport (e.g. this crate's `tests/loopback.rs`) can
/// drive the same push loop without going through `--url` parsing
/// twice.
pub fn send_over_transport(
    p: &Profile,
    transport: Box<dyn Transport>,
    seconds: f64,
    no_klv_digest: bool,
    au_sizes: AuSizeMode,
) -> Result<CellMetrics, String> {
    let cfg = mux_setup::build_config(p);
    let (teeing, tap) = Teeing::new(transport);
    let sender = MuxSender::new(teeing, cfg).map_err(|e| format!("MuxSender::new: {e}"))?;

    // Handles must come from THIS sender's live muxer, not a throwaway
    // one — see mux_setup.rs's doc comment for why build_config doesn't
    // hand them back itself.
    let video_handles = sender.video_handles();
    let klv_handles = sender.klv_handles();
    let audio_handle = sender.audio_handles().into_iter().next();

    let (start, events) = schedule::build_schedule(p, seconds);

    let mut video_aus = 0u64;
    let mut keyframes = 0u64;
    let mut klv_records = 0u64;
    let mut audio_frames = 0u64;
    let mut misp_sei_seen = false;
    // One digest per WIRE occurrence (i.e. per handle push, not per
    // logical record) — a two-program profile duplicates each record
    // onto both programs' KLV PIDs, and a receiver demuxing that capture
    // sees (and digests) each occurrence separately, so the sent-side
    // fingerprint must count the same way for the two to compare equal.
    // Never pushed to when `no_klv_digest` — see `CellMetrics::
    // klv_set_sha256`'s doc comment for why this list is unbounded over
    // a multi-day run and needs an explicit opt-out.
    let mut klv_digests: Vec<String> = Vec::new();

    let wall_start = Instant::now();
    let mut last_heartbeat = Instant::now();
    for (pts_ticks, event) in events {
        // Progress heartbeat → stderr → the soak's per-process log
        // file. See `crate::HEARTBEAT_INTERVAL`'s doc comment.
        if last_heartbeat.elapsed() >= crate::HEARTBEAT_INTERVAL {
            last_heartbeat = Instant::now();
            eprintln!(
                "send: heartbeat elapsed_s={} video_aus={video_aus} keyframes={keyframes} \
                 klv_records={klv_records} audio_frames={audio_frames} wire_bytes={}",
                wall_start.elapsed().as_secs(),
                transport::tee_bytes_so_far(&tap),
            );
        }
        // Wall-clock pacing: sleep until this event's target offset from
        // `wall_start` has actually elapsed. Recomputed from the target
        // offset each iteration (not a fixed per-event sleep) so pacing
        // doesn't drift as the pushes themselves take non-zero time.
        let target = Duration::from_secs_f64((pts_ticks - start) as f64 / PTS_HZ as f64);
        let elapsed = wall_start.elapsed();
        if target > elapsed {
            thread::sleep(target - elapsed);
        }

        let pts = Pts90khz::new(pts_ticks);
        match event {
            Event::Video { frame_idx } => {
                let (au, keyframe) = fixtures::video_au_sized(p.video, frame_idx, au_sizes);
                for &handle in &video_handles {
                    if p.klv == KlvMode::AsyncWithMisp {
                        // ST 0604 SEI carriage is H.264/H.265-only; the
                        // `misp` profile is always H.264 (see
                        // `profiles::PROFILES`), so this is unreachable
                        // for the other codecs today — mirrors gen.rs's
                        // own guard.
                        debug_assert!(matches!(p.video, VideoCodec::H264 | VideoCodec::H265));
                        let misp_us = (pts_ticks as u64).wrapping_mul(1_000_000) / PTS_HZ as u64;
                        let misp = MispTimestamp::micros(misp_us, 0x1F);
                        sender
                            .send_video_misp_to(handle, &au, pts, keyframe, &misp)
                            .map_err(|e| format!("send_video_misp_to: {e}"))?;
                        misp_sei_seen = true;
                    } else {
                        sender
                            .send_video_to(handle, &au, pts, keyframe)
                            .map_err(|e| format!("send_video_to: {e}"))?;
                    }
                    video_aus += 1;
                    if keyframe {
                        keyframes += 1;
                    }
                }
            }
            Event::Klv { seq } => {
                let record = fixtures::klv_record(seq);
                for &handle in &klv_handles {
                    sender
                        .send_klv_to(handle, &record, pts, 0x00)
                        .map_err(|e| format!("send_klv_to: {e}"))?;
                    klv_records += 1;
                    if !no_klv_digest {
                        klv_digests.push(verify::to_hex(&Sha256::digest(&record)));
                    }
                }
            }
            Event::Audio { frame_idx } => {
                if let Some(handle) = audio_handle {
                    let frame = fixtures::aac_frame(frame_idx);
                    sender
                        .send_audio_to(handle, &frame, pts)
                        .map_err(|e| format!("send_audio_to: {e}"))?;
                    audio_frames += 1;
                }
            }
        }
    }

    // Drop (not `sender.close()`) before reading the tee tally back.
    // `MuxSender::close()` cancels the transport's cancel handle FIRST
    // (needed so a `close()` racing another thread parked inside
    // `send_bytes` doesn't deadlock on the inner mutex) and only drains
    // `pending_bytes` afterward — for a transport with a real cancel
    // handle (SRT, RIST; a plain in-memory test mock has none) that
    // cancel already tears the connection down, so anything still
    // sitting in `pending_bytes` fails to send and is discarded.
    // `Drop`'s impl does the same drain but BEFORE closing the
    // transport, so nothing pending is lost — this crate's single
    // sender thread never needs `close()`'s cross-thread-safe ordering,
    // so plain `drop` is both correct and simpler here. (`tee_tally`
    // also requires the `Teeing` — owned by `sender`'s inner transport
    // state — to have no other owner, which `drop` satisfies too.)
    drop(sender);
    let (bytes, stream_sha256) = transport::tee_tally(tap);

    Ok(CellMetrics {
        video_aus,
        keyframes,
        klv_records,
        klv_set_sha256: (!no_klv_digest).then(|| verify::klv_set_hash(&klv_digests)),
        audio_frames,
        programs_seen: p.programs,
        // We generate every event in strictly ascending PTS order (the
        // same schedule gen.rs sorts before writing), so the sent
        // stream is monotonic by construction.
        pts_monotonic: true,
        misp_sei_seen,
        bytes,
        stream_sha256,
    })
}

/// Like [`run`], but wraps the transport in a [`ManagedTransport`] that
/// reconnects by re-dialing `url` on transport breakage, instead of
/// failing the whole push loop the moment the underlying transport goes
/// `Broken`. `soak.sh`'s SRT leg uses this: a scheduled proxy outage
/// window (every packet dropped for its duration) eventually surfaces
/// to libsrt as a dead connection, and the reconnect factory here just
/// re-dials the same `url` — the proxy itself never goes away, it just
/// resumes forwarding once the outage window closes.
///
/// Uses `max_attempts: None` (retry forever) rather than
/// `ReconnectPolicy::default()`'s `Some(10)`: the default's exponential
/// backoff caps at 10s/attempt, so exhausting 10 attempts takes at most
/// ~100s of waiting alone — uncomfortably close to (and, with any
/// scheduling jitter, sometimes less than) a 90s outage window this
/// arc's soak topology configures. Giving up mid-outage would strand
/// the sender for the rest of a multi-day run, which is a strictly
/// worse failure mode than retrying indefinitely against a proxy
/// address that's known to still be alive (it's discarding packets, not
/// gone).
pub fn run_managed(
    p: &Profile,
    url: &str,
    seconds: f64,
    json_out: Option<&str>,
    no_klv_digest: bool,
    au_sizes: AuSizeMode,
) -> Result<CellMetrics, String> {
    let initial = transport::make_send(url)?;
    let dial_url = url.to_string();
    let factory = move || {
        transport::make_send(&dial_url).map_err(|e| TransportError::Broken {
            msg: e,
            errno_code: None,
            cause: BrokenCause::Unspecified,
        })
    };
    let policy = ReconnectPolicy {
        max_attempts: None,
        ..ReconnectPolicy::default()
    };
    let managed: Box<dyn Transport> = Box::new(ManagedTransport::new(initial, factory, policy));

    let metrics = send_over_transport(p, managed, seconds, no_klv_digest, au_sizes)?;
    if let Some(target) = json_out {
        write_json(target, &metrics)?;
    }
    Ok(metrics)
}
