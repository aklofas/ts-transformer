//! `serve-whep` subcommand: bind a [`WhepServer`] with one mount and feed
//! it one profile's KLV schedule, wall-clock paced, for a browser to play.
//!
//! The interop matrix's `whep/*` cells point headless Chrome at the mount
//! and judge what the page reports (see `scripts/interop/whep-page.html`):
//! the KLV record set seen in the SEI and on the data channel against the
//! generator's, the decoded frame count, and the audio packets received.
//!
//! The KLV records are the profile's own schedule (`schedule::build_schedule`
//! with `fixtures::klv_record_for`), exactly the set `gen` writes for the
//! same profile and window, so `verify --file <gen file>` gives the
//! expected set digest. The video is NOT the generator's: its synthetic
//! slices decode in no real decoder (the interop README's gap item 10),
//! so a browser given them renders nothing and a "video plays" oracle
//! could never pass. Each video event instead pushes one access unit of a
//! real 30-frame Constrained Baseline GOP (320x240, the fixture
//! `tst-webrtc`'s own loopback viewer tests play), cycled for the window,
//! at the schedule's PTS.
//!
//! Two mount shapes, chosen by [`MountFeed`]:
//! - [`MountFeed::Mp2t`]: the events go through a [`Muxer`] built from the
//!   profile's own configuration and the TS bytes are pushed with
//!   `push_ts`, the path a muxed source takes.
//! - [`MountFeed::Elementary`]: `push_video` / `push_klv` directly, and
//!   with `opus` one synthesized Opus silence packet every 20 ms through
//!   `push_audio` under `AudioPolicy::PassthroughOpus`, the only way a
//!   mount can carry audio (the TS demuxer has no Opus mapping).
//!
//! The push waits, up to `wait_viewer`, for one viewer session to be ICE
//! connected before the first event, so a browser that joins after bind
//! still sees the first keyframe and every record bound to it; without a
//! viewer in time it pushes anyway and the cell fails on its oracles. Once
//! the schedule is done the server stays up for `LINGER` so the browser
//! decodes what is in flight, the counters are written as JSON, and the
//! server stops.

use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::Muxer;
use tst_webrtc::{AudioPolicy, MountConfig, MountHandle, MountKind, VideoCodec, WhepServer};

use crate::cli;
use crate::fixtures::{self, AuSizeMode, KlvSet};
use crate::mux_setup;
use crate::profiles::Profile;
use crate::schedule::{self, Event, PTS_HZ};

/// How long the server stays up after the last push: the viewer's jitter
/// buffer and decoder still hold the tail, and the page samples its
/// counters once a second.
const LINGER: Duration = Duration::from_secs(3);

/// After the viewer's session reports ICE connected, the time its data
/// channel is given to open before the first KLV record is bound to a
/// frame (a record sent on a channel that is not open yet is dropped and
/// counted).
const DATA_CHANNEL_SETTLE: Duration = Duration::from_millis(500);

/// How often the wait for a viewer polls the session table.
const VIEWER_POLL: Duration = Duration::from_millis(100);

/// 90 kHz ticks between Opus packets: 20 ms, the packet duration the
/// silence frame below declares.
const OPUS_FRAME_TICKS: i64 = 1800;

/// One Opus packet (RFC 6716) of 20 ms of silence: the TOC byte names
/// configuration 31 (CELT, fullband, 20 ms), mono, code 0 (one frame), and
/// the frame is the CELT silence flag as the range coder finalizes it.
/// What `libopus` emits for a 20 ms frame of digital silence; a viewer's
/// decoder plays silence from it and its `packetsReceived` counts it.
const OPUS_SILENCE_20MS: [u8; 3] = [0xF8, 0xFF, 0xFE];

/// A real 30-frame GOP (IDR, then 29 P frames, each opened by an access
/// unit delimiter): `crates/tst-webrtc/tests/fixtures/regen.sh` encodes a
/// 320x240 test pattern with libx264, Constrained Baseline, no B frames.
const GOP: &[u8] = include_bytes!("../../tst-webrtc/tests/fixtures/h264/gop.h264");

/// How the mount is fed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountFeed {
    /// An MP2T mount: the schedule is muxed in-process and pushed as TS
    /// bytes. Audio is always dropped by such a mount.
    Mp2t,
    /// An elementary mount: access units and records pushed one by one;
    /// with `opus`, a silence Opus packet every 20 ms as well.
    Elementary { opus: bool },
}

/// The `serve-whep` options after parsing.
#[derive(Debug, Clone)]
pub struct Options {
    /// The `whep://` bind URL.
    pub url: String,
    /// The mount path, `/`-prefixed.
    pub mount: String,
    /// The profile whose KLV schedule is served.
    pub profile: &'static Profile,
    /// The window length.
    pub seconds: f64,
    /// How the mount is fed.
    pub feed: MountFeed,
    /// How long to wait for a connected viewer before pushing.
    pub wait_viewer: Duration,
}

/// The profiles `serve-whep` can serve: one program whose video is
/// H.264, the codec of the GOP fixture it pushes. An MP2T mount built from
/// any other profile would mux the H.264 units under the profile's own
/// video stream type, and the mount's demuxer would count every one of
/// them as non-conformant while the server exits 0 having served nothing;
/// a second program has no second GOP to feed.
pub fn check_profile(p: &Profile) -> Result<(), String> {
    // The profile's codec enum, not tst-webrtc's `VideoCodec` imported above.
    if p.video != crate::profiles::VideoCodec::H264 {
        return Err(format!(
            "profile {} carries {:?} video; serve-whep pushes an H.264 GOP and serves \
             H.264 profiles only",
            p.name, p.video
        ));
    }
    if p.programs != 1 {
        return Err(format!(
            "profile {} has {} programs; serve-whep serves one mount from a one-program \
             profile",
            p.name, p.programs
        ));
    }
    Ok(())
}

/// One scheduled push.
#[derive(Debug, Clone, Copy)]
enum Push {
    Video { frame_idx: u32 },
    Klv { seq: u32 },
    Opus,
}

/// Splits the GOP fixture into `(access unit, is keyframe)` pairs: a new
/// unit opens at each access unit delimiter (NAL type 9), and a unit is a
/// keyframe when it holds an IDR slice (type 5).
fn gop_access_units() -> Vec<(Vec<u8>, bool)> {
    let starts: Vec<usize> = GOP
        .windows(4)
        .enumerate()
        .filter(|(_, w)| *w == [0, 0, 0, 1])
        .map(|(i, _)| i)
        .collect();
    let mut aus: Vec<(Vec<u8>, bool)> = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let end = starts.get(k + 1).copied().unwrap_or(GOP.len());
        let nal = &GOP[s..end];
        let nal_type = nal[4] & 0x1f;
        if nal_type == 9 || aus.is_empty() {
            aus.push((Vec::new(), false));
        }
        let au = aus.last_mut().expect("an access unit was just opened");
        au.0.extend_from_slice(nal);
        au.1 |= nal_type == 5;
    }
    aus
}

/// The PTS-ordered pushes for `seconds` of `p`'s schedule: the profile's
/// video and KLV events, plus an Opus packet every 20 ms when `opus`. On a
/// shared tick the order is video, KLV, Opus, the schedule's own tie rule
/// extended by one kind. Lazy, like the schedule the senders walk: a
/// 72-hour window is millions of pushes, and none of them is held.
fn build_pushes(p: &Profile, seconds: f64, opus: bool) -> (i64, Pushes) {
    let (start, schedule) = schedule::build_schedule(p, seconds);
    let opus_count = if opus {
        (seconds * PTS_HZ as f64 / OPUS_FRAME_TICKS as f64).round() as i64
    } else {
        0
    };
    (
        start,
        Pushes {
            schedule,
            pending: None,
            start,
            opus_next: 0,
            opus_count,
        },
    )
}

/// [`build_pushes`]' iterator: the schedule's video and KLV events merged
/// with an arithmetic Opus sequence, in O(1) state.
struct Pushes {
    schedule: schedule::Schedule,
    /// The next video or KLV push, pulled from `schedule` and not yet
    /// yielded because an earlier Opus packet goes first.
    pending: Option<(i64, Push)>,
    start: i64,
    opus_next: i64,
    opus_count: i64,
}

impl Iterator for Pushes {
    type Item = (i64, Push);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pending.is_none() {
            // The profile's AAC cadence has no Opus packet to forward.
            self.pending = self.schedule.find_map(|(pts, ev)| match ev {
                Event::Video { frame_idx } => Some((pts, Push::Video { frame_idx })),
                Event::Klv { seq } => Some((pts, Push::Klv { seq })),
                Event::Audio { .. } => None,
            });
        }
        let opus_pts = (self.opus_next < self.opus_count)
            .then(|| self.start + self.opus_next * OPUS_FRAME_TICKS);
        match (self.pending, opus_pts) {
            // `<=`: on a shared tick video and KLV go before Opus.
            (Some((pts, _)), Some(o)) if pts > o => {
                self.opus_next += 1;
                Some((o, Push::Opus))
            }
            (Some(push), _) => {
                self.pending = None;
                Some(push)
            }
            (None, Some(o)) => {
                self.opus_next += 1;
                Some((o, Push::Opus))
            }
            (None, None) => None,
        }
    }
}

/// Sleep until `pts_ticks`'s offset from `start` has elapsed since
/// `wall_start`: the drift-free pacing `send.rs` and `serve.rs` use.
fn sleep_until(wall_start: Instant, start: i64, pts_ticks: i64) {
    let target = Duration::from_secs_f64((pts_ticks - start) as f64 / PTS_HZ as f64);
    let elapsed = wall_start.elapsed();
    if target > elapsed {
        thread::sleep(target - elapsed);
    }
}

/// Waits up to `limit` for a session on `mount` to report ICE connected.
/// `true` when one did.
fn wait_for_viewer(mount: &MountHandle, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if mount.sessions().iter().any(|s| s.connected_for.is_some()) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(VIEWER_POLL);
    }
}

/// The counters as one JSON document: the mount's, the server's and each
/// session's. Written with `--json`.
fn counters_json(
    opts: &Options,
    server: &WhepServer,
    mount: &MountHandle,
    viewer_connected: bool,
) -> serde_json::Value {
    let m = mount.stats();
    let s = server.stats();
    let mount_kind = match opts.feed {
        MountFeed::Mp2t => "mp2t",
        MountFeed::Elementary { .. } => "elementary",
    };
    let sessions: Vec<serde_json::Value> = mount
        .sessions()
        .iter()
        .map(|v| {
            json!({
                "id": v.id,
                "remote_addr": v.remote_addr.map(|a| a.to_string()),
                "ice_state": format!("{:?}", v.ice_state),
                "connected_for_ms": v.connected_for.map(|d| d.as_millis() as u64),
                "rtp_packets": v.rtp_packets,
                "rtp_bytes": v.rtp_bytes,
                "nacks": v.nacks,
                "plis": v.plis,
                "dc_messages": v.dc_messages,
                "packets_dropped_queue_full": v.packets_dropped_queue_full,
                "dc_messages_dropped": v.dc_messages_dropped,
            })
        })
        .collect();
    json!({
        // What was served, so `report soak` can hold the leg to its
        // declaration the way the transport legs' recv reports are.
        "profile": opts.profile.name,
        "mount_kind": mount_kind,
        "seconds": opts.seconds,
        "viewer_connected_before_push": viewer_connected,
        "mount": {
            "frames_pushed": m.frames_pushed,
            "keyframes_pushed": m.keyframes_pushed,
            "keyframe_flag_mismatch": m.keyframe_flag_mismatch,
            "viewers": m.viewers,
            "viewers_waiting_keyframe": m.viewers_waiting_keyframe,
            "klv_records_pushed": m.klv_records_pushed,
            "klv_records_dropped_unbound": m.klv_records_dropped_unbound,
            "klv_sei_inserted": m.klv_sei_inserted,
            "klv_sei_dropped_oversize": m.klv_sei_dropped_oversize,
            "klv_dc_messages_sent": m.klv_dc_messages_sent,
            "klv_dc_dropped_oversize": m.klv_dc_dropped_oversize,
            "klv_dc_dropped_viewer": m.klv_dc_dropped_viewer,
            "audio_frames_pushed": m.audio_frames_pushed,
            "audio_frames_dropped": m.audio_frames_dropped,
            "plis_received": m.plis_received,
            "nacks_received": m.nacks_received,
            "viewer_packets_dropped_queue_full": m.viewer_packets_dropped_queue_full,
            "rtp_clock_rebases": m.rtp_clock_rebases,
            "frames_pts_reordered": m.frames_pts_reordered,
            "demux_nonconformant": m.demux_nonconformant,
        },
        "server": {
            "active_sessions": s.active_sessions,
            "sessions_total": s.sessions_total,
            "sessions_rejected_cap": s.sessions_rejected_cap,
            "sessions_rejected_codec": s.sessions_rejected_codec,
            "sessions_rejected_auth": s.sessions_rejected_auth,
            "sessions_reaped_ice_timeout": s.sessions_reaped_ice_timeout,
            "sessions_reaped_disconnected": s.sessions_reaped_disconnected,
            "rtp_packets_sent": s.rtp_packets_sent,
            "rtp_bytes_sent": s.rtp_bytes_sent,
            "datagrams_unrouted": s.datagrams_unrouted,
        },
        "sessions": sessions,
    })
}

/// Serves `opts`' schedule on a WHEP mount and writes the counters to
/// `json` (a path, or `-` for stdout) when given.
///
/// Prints `{"serving": "http://host:port/mount"}` to stdout once the server
/// is bound, before any wait or push, so a script can start the browser.
pub fn run(opts: &Options, json: Option<&str>) -> Result<(), String> {
    let mut cfg = MountConfig::default();
    cfg.video = VideoCodec::H264;
    match opts.feed {
        MountFeed::Mp2t => cfg.kind = MountKind::Mp2t,
        MountFeed::Elementary { opus } => {
            cfg.kind = MountKind::Elementary;
            if opus {
                cfg.audio = AudioPolicy::PassthroughOpus;
            }
        }
    }
    let server = WhepServer::bind(&opts.url).map_err(|e| format!("WhepServer::bind: {e}"))?;
    let mount = server
        .add_mount(&opts.mount, cfg)
        .map_err(|e| format!("add_mount: {e}"))?;
    println!(
        "{{\"serving\": \"http://{}{}\"}}",
        server.local_addr(),
        opts.mount
    );

    let viewer_connected = if opts.wait_viewer.is_zero() {
        false
    } else {
        let connected = wait_for_viewer(&mount, opts.wait_viewer);
        if connected {
            thread::sleep(DATA_CHANNEL_SETTLE);
        } else {
            eprintln!(
                "serve-whep: no viewer connected within {:?}; pushing anyway",
                opts.wait_viewer
            );
        }
        connected
    };

    let gop = gop_access_units();
    let opus = matches!(opts.feed, MountFeed::Elementary { opus: true });
    let (start, pushes) = build_pushes(opts.profile, opts.seconds, opus);
    let mut muxer = match opts.feed {
        MountFeed::Mp2t => {
            let cfg = mux_setup::build_config(opts.profile, AuSizeMode::Compact);
            Some(Muxer::new(cfg).map_err(|e| format!("Muxer::new: {e}"))?)
        }
        MountFeed::Elementary { .. } => None,
    };
    // Handles come from this muxer, not a throwaway (see `mux_setup`).
    let (video_handles, klv_handles) = match &muxer {
        Some(mux) => (mux.video_handles(), mux.klv_handles()),
        None => (Vec::new(), Vec::new()),
    };
    let mut pull_buf = [0u8; 1316];
    let wall_start = Instant::now();
    for (pts_ticks, push) in pushes {
        sleep_until(wall_start, start, pts_ticks);
        let pts = Pts90khz::new(pts_ticks);
        match (&mut muxer, push) {
            (Some(mux), Push::Video { frame_idx }) => {
                let (au, keyframe) = &gop[frame_idx as usize % gop.len()];
                for &handle in &video_handles {
                    mux.push_video_to(handle, au, pts, *keyframe)
                        .map_err(|e| format!("push_video_to: {e}"))?;
                }
            }
            (Some(mux), Push::Klv { seq }) => {
                let record = fixtures::klv_record_for(KlvSet::Compact, 0, seq)?;
                for &handle in &klv_handles {
                    mux.push_klv_to(handle, &record, pts, 0x00)
                        .map_err(|e| format!("push_klv_to: {e}"))?;
                }
            }
            (Some(_), Push::Opus) => unreachable!("an MP2T feed schedules no Opus"),
            (None, Push::Video { frame_idx }) => {
                let (au, keyframe) = &gop[frame_idx as usize % gop.len()];
                mount
                    .push_video(au, pts, *keyframe)
                    .map_err(|e| format!("push_video: {e}"))?;
            }
            (None, Push::Klv { seq }) => {
                let record = fixtures::klv_record_for(KlvSet::Compact, 0, seq)?;
                mount
                    .push_klv(&record, pts, 0x00)
                    .map_err(|e| format!("push_klv: {e}"))?;
            }
            (None, Push::Opus) => {
                mount
                    .push_audio(&OPUS_SILENCE_20MS, pts)
                    .map_err(|e| format!("push_audio: {e}"))?;
            }
        }
        if let Some(mux) = &mut muxer {
            loop {
                let n = mux.pull(&mut pull_buf);
                if n == 0 {
                    break;
                }
                mount
                    .push_ts(&pull_buf[..n])
                    .map_err(|e| format!("push_ts: {e}"))?;
            }
        }
    }

    thread::sleep(LINGER);
    if let Some(target) = json {
        cli::write_json(
            target,
            &counters_json(opts, &server, &mount, viewer_connected),
        )?;
    }
    server.stop();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles;
    use crate::verify::{klv_set_hash, to_hex};
    use sha2::{Digest, Sha256};

    #[test]
    fn the_gop_fixture_is_thirty_access_units_opening_on_a_keyframe() {
        let gop = gop_access_units();
        assert_eq!(gop.len(), 30);
        assert!(gop[0].1, "the first access unit is the IDR");
        assert!(gop[1..].iter().all(|(_, key)| !key), "P frames follow");
        assert!(
            gop.iter()
                .all(|(au, _)| au.starts_with(&[0, 0, 0, 1, 0x09]))
        );
    }

    /// The pushed KLV set is the generator's: the same records as
    /// `gen --profile baseline --seconds 3` hashes to the same set digest,
    /// so a cell can hold the browser to `verify --file <gen file>`.
    #[test]
    fn the_klv_pushes_are_the_profiles_schedule() {
        let p = profiles::by_name("baseline").expect("baseline exists");
        let pushes: Vec<(i64, Push)> = build_pushes(p, 3.0, false).1.collect();
        let digests: Vec<String> = pushes
            .iter()
            .filter_map(|(_, push)| match push {
                Push::Klv { seq } => Some(*seq),
                _ => None,
            })
            .map(|seq| to_hex(&Sha256::digest(fixtures::klv_record(seq))))
            .collect();
        assert_eq!(digests.len(), 30, "3 s of KLV at 10 Hz");
        let (_, events) = schedule::build_schedule(p, 3.0);
        let expected: Vec<String> = events
            .filter_map(|(_, ev)| match ev {
                Event::Klv { seq } => Some(to_hex(&Sha256::digest(fixtures::klv_record(seq)))),
                _ => None,
            })
            .collect();
        assert_eq!(klv_set_hash(&digests), klv_set_hash(&expected));
        assert!(
            pushes.windows(2).all(|w| w[0].0 <= w[1].0),
            "pushes are in PTS order"
        );
    }

    #[test]
    fn opus_packets_run_every_twenty_milliseconds_after_video_and_klv() {
        let p = profiles::by_name("baseline").expect("baseline exists");
        let (start, pushes) = build_pushes(p, 1.0, true);
        let pushes: Vec<(i64, Push)> = pushes.collect();
        let opus: Vec<i64> = pushes
            .iter()
            .filter(|(_, push)| matches!(push, Push::Opus))
            .map(|(pts, _)| *pts)
            .collect();
        assert_eq!(opus.len(), 50);
        assert_eq!(opus[0], start);
        assert!(opus.windows(2).all(|w| w[1] - w[0] == OPUS_FRAME_TICKS));
        // On the shared first tick: video, then KLV, then Opus.
        let first_tick: Vec<u8> = pushes
            .iter()
            .take_while(|(pts, _)| *pts == start)
            .map(|(_, push)| match push {
                Push::Video { .. } => 0,
                Push::Klv { .. } => 1,
                Push::Opus => 2,
            })
            .collect();
        assert_eq!(first_tick, [0, 1, 2]);
        let (_, mut without) = build_pushes(p, 1.0, false);
        assert!(without.all(|(_, push)| !matches!(push, Push::Opus)));
    }

    /// The eager body `build_pushes` had before it was lazy: every push
    /// collected and stably sorted. Kept as the order's reference.
    fn eager_reference(p: &Profile, seconds: f64, opus: bool) -> (i64, Vec<(i64, Push)>) {
        let (start, events) = schedule::build_schedule(p, seconds);
        let mut pushes: Vec<(i64, u8, Push)> = events
            .filter_map(|(pts, ev)| match ev {
                Event::Video { frame_idx } => Some((pts, 0, Push::Video { frame_idx })),
                Event::Klv { seq } => Some((pts, 1, Push::Klv { seq })),
                // The profile's AAC cadence has no Opus packet to forward.
                Event::Audio { .. } => None,
            })
            .collect();
        if opus {
            let count = (seconds * PTS_HZ as f64 / OPUS_FRAME_TICKS as f64).round() as i64;
            pushes.extend((0..count).map(|i| (start + i * OPUS_FRAME_TICKS, 2, Push::Opus)));
        }
        pushes.sort_by_key(|&(pts, order, _)| (pts, order));
        (
            start,
            pushes
                .into_iter()
                .map(|(pts, _, push)| (pts, push))
                .collect(),
        )
    }

    fn key(item: (i64, Push)) -> (i64, u8, u32) {
        match item.1 {
            Push::Video { frame_idx } => (item.0, 0, frame_idx),
            Push::Klv { seq } => (item.0, 1, seq),
            Push::Opus => (item.0, 2, 0),
        }
    }

    #[test]
    fn lazy_pushes_match_the_eager_order_and_hold_no_list() {
        for (name, opus) in [("baseline", true), ("baseline", false), ("audio", true)] {
            let p = profiles::by_name(name).unwrap();
            let (start, lazy) = build_pushes(p, 10.0, opus);
            let (s2, eager) = eager_reference(p, 10.0, opus);
            assert_eq!(start, s2);
            assert_eq!(
                lazy.map(key).collect::<Vec<_>>(),
                eager.into_iter().map(key).collect::<Vec<_>>(),
                "{name} opus={opus}"
            );
        }
        assert!(std::mem::size_of::<Pushes>() <= 160);
    }

    #[test]
    fn check_profile_accepts_h264_one_program_profiles_only() {
        for name in ["baseline", "klv-sync"] {
            let p = profiles::by_name(name).expect("profile exists");
            assert_eq!(check_profile(p), Ok(()), "{name}");
        }
        let e = check_profile(profiles::by_name("h265-klv").unwrap()).unwrap_err();
        assert!(e.contains("carries H265 video"), "{e}");
        let e = check_profile(profiles::by_name("av1-klv-a").unwrap()).unwrap_err();
        assert!(e.contains("carries Av1 video"), "{e}");
        let e = check_profile(profiles::by_name("two-program").unwrap()).unwrap_err();
        assert!(e.contains("has 2 programs"), "{e}");
    }

    #[test]
    fn the_silence_packet_is_one_celt_fullband_20ms_frame() {
        let toc = OPUS_SILENCE_20MS[0];
        assert_eq!(toc >> 3, 31, "configuration 31: CELT fullband 20 ms");
        assert_eq!(toc & 0x04, 0, "mono");
        assert_eq!(toc & 0x03, 0, "code 0: one frame");
    }
}
