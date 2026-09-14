//! `serve` module: HLS + RTSP serve (bind) modes.
//!
//! Every scheme in `transport.rs` CONNECTS OUT to a peer (`send`/`recv`
//! build a `Transport`/`RecvTransport` and dial or listen-then-accept a
//! single session). HLS and RTSP are different in kind: there is no
//! single peer to connect to — a real player/tool (ffmpeg, ffprobe, VLC,
//! or this crate's own `tst_rtp::RtspClient`) connects IN whenever it
//! likes. [`run_hls`]/[`run_rtsp`] bind a real publisher/server for one
//! [`Profile`]'s synthetic traffic and push it, wall-clock paced (same
//! shape as `send.rs`'s push loop), so a caller-driven external tool can
//! pull it.
//!
//! # The `finish_serving` deadlock lesson
//!
//! [`run_hls`] always finishes via `HlsPublisher::finish_serving` (never
//! the plain `Publisher::finish`) once the profile's traffic has been
//! pushed. Plain `finish()` tears the HTTP server down the moment the
//! write side is done; a puller that hasn't started fetching yet (or is
//! mid-fetch of the terminal ENDLIST-tagged playlist) then hangs forever
//! against a server that's already gone. This exact deadlock previously
//! wedged this project's own `hls_e2e` ffmpeg test — see
//! `crates/tst-hls/tests/hls_e2e.rs`'s
//! `hls_pipeline_via_ffmpeg_validates_playlist` doc comment.
//! `finish_serving` keeps the server up — serving the now-complete VOD
//! asset — for `LINGER` past the last push, giving a puller time to
//! grab the whole thing. [`run_rtsp`] mirrors the same idea by keeping
//! the `RtspServer` running for `LINGER` after its last push before
//! calling `stop()`.

use std::net::SocketAddr;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use tst_core::codec::misp_time::MispTimestamp;
use tst_core::mpegts::common::Pts90khz;
use tst_hls::{HlsMode, HlsPublisherBuilder};
use tst_pipeline::MuxPublisher;
use tst_rtp::RtspServer;

use crate::fixtures::{self, AuSizeMode, KlvSet};
use crate::mux_setup;
use crate::profiles::{KlvMode, Profile, VideoCodec};
use crate::schedule::{self, Event, PTS_HZ};

/// How long a completed serve (HLS `finish_serving` / RTSP
/// push-complete) stays up before this module tears it down — gives a
/// puller that hasn't started yet (or is mid-fetch) time to grab the
/// whole capture. See the module doc's "finish_serving deadlock lesson".
const LINGER: Duration = Duration::from_secs(10);

/// HLS target segment duration used by [`run_hls`]. Short relative to
/// the few-second profile windows this crate exercises, so even a brief
/// serve cuts at least one real segment boundary instead of finalizing a
/// single giant open segment.
const HLS_SEGMENT_DURATION: Duration = Duration::from_secs(1);

/// Sleep until `pts_ticks`'s offset from `start` has elapsed since
/// `wall_start` — the same drift-free, recomputed-each-iteration pacing
/// `send.rs::send_over_transport` uses.
fn sleep_until(wall_start: Instant, start: i64, pts_ticks: i64) {
    let target = Duration::from_secs_f64((pts_ticks - start) as f64 / PTS_HZ as f64);
    let elapsed = wall_start.elapsed();
    if target > elapsed {
        thread::sleep(target - elapsed);
    }
}

/// Process-unique suffix for a temp directory name (pid alone collides
/// across repeated runs from the same process within a test suite).
fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_nanos()
}

/// Serve `p`'s synthetic traffic as an HLS asset bound at `bind_addr`.
///
/// Prints the bound playlist URL to stdout as a single JSON line
/// (`{"serving": "http://host:port/playlist.m3u8"}`) as soon as the HTTP
/// server is bound — before any traffic is pushed — so a script driving
/// this process can start a puller immediately.
///
/// Blocks for `seconds` (wall-clock paced push), then finalizes via
/// `finish_serving` and keeps serving for `LINGER` before returning.
///
/// # Scope
/// `MuxPublisher` (the pipeline shell this function pushes through)
/// exposes only single-target `send_*` methods — no `send_*_to(handle,
/// ...)` family. A `two-program` profile's second video/KLV stream has
/// no way to receive its own pushes here, so that profile surfaces
/// `MuxPublisherError::Mux(MuxError::AmbiguousTarget)` the first time
/// this function pushes video. `MISP` (`KlvMode::AsyncWithMisp`, the
/// `misp` profile) IS supported — `MuxPublisher::send_video_misp` exists.
///
/// `klv`/`klv_seed` pick the ST 0601 record factory and `au_sizes` the
/// video AU size regime — see `gen::run`'s doc comment for both.
pub fn run_hls(
    p: &Profile,
    bind_addr: SocketAddr,
    seconds: f64,
    klv: KlvSet,
    klv_seed: u64,
    au_sizes: AuSizeMode,
) -> Result<(), String> {
    let out_dir = std::env::temp_dir().join(format!(
        "tst-interop-hls-serve-{}-{}",
        std::process::id(),
        unique_suffix(),
    ));

    let result = run_hls_inner(p, bind_addr, seconds, &out_dir, klv, klv_seed, au_sizes);
    // Best-effort cleanup on every path (success or error): the segments
    // + playlist HlsPublisherBuilder wrote under out_dir have already
    // been fully served (or never will be, on an error exit) by the
    // time this function returns, so nothing further needs them.
    // Ignore failures — a leftover temp dir is a nuisance, not a
    // correctness problem, and panicking here would mask the real
    // result from `run_hls_inner`.
    let _ = std::fs::remove_dir_all(&out_dir);
    result
}

fn run_hls_inner(
    p: &Profile,
    bind_addr: SocketAddr,
    seconds: f64,
    out_dir: &Path,
    klv: KlvSet,
    klv_seed: u64,
    au_sizes: AuSizeMode,
) -> Result<(), String> {
    let publisher = HlsPublisherBuilder::new()
        .bind(bind_addr)
        .output_dir(out_dir)
        .segment_duration(HLS_SEGMENT_DURATION)
        .mode(HlsMode::Event)
        .build()
        .map_err(|e| format!("HlsPublisherBuilder::build: {e}"))?;

    let addr = publisher
        .local_addr()
        .ok_or_else(|| "HLS publisher has no bound HTTP address".to_string())?;
    println!("{{\"serving\": \"http://{addr}/playlist.m3u8\"}}");

    let cfg = mux_setup::build_config(p);
    let shell = MuxPublisher::with_config(publisher, cfg)
        .map_err(|e| format!("MuxPublisher::with_config: {e}"))?;

    let (start, events) = schedule::build_schedule(p, seconds);
    let wall_start = Instant::now();
    for (pts_ticks, event) in events {
        sleep_until(wall_start, start, pts_ticks);
        let pts = Pts90khz::new(pts_ticks);
        match event {
            Event::Video { frame_idx } => {
                let (au, keyframe) = fixtures::video_au_sized(p.video, frame_idx, au_sizes);
                if p.klv == KlvMode::AsyncWithMisp {
                    // Mirrors gen.rs/send.rs's own guard: MISP profiles
                    // are always H.264 in the registry today.
                    debug_assert!(matches!(p.video, VideoCodec::H264 | VideoCodec::H265));
                    let misp_us = (pts_ticks as u64).wrapping_mul(1_000_000) / PTS_HZ as u64;
                    let misp = MispTimestamp::micros(misp_us, 0x1F);
                    shell
                        .send_video_misp(&au, pts, keyframe, &misp)
                        .map_err(|e| format!("send_video_misp: {e}"))?;
                } else {
                    shell
                        .send_video(&au, pts, keyframe)
                        .map_err(|e| format!("send_video: {e}"))?;
                }
            }
            Event::Klv { seq } => {
                let record = fixtures::klv_record_for(klv, klv_seed, seq)?;
                shell
                    .send_klv(&record, pts, 0x00)
                    .map_err(|e| format!("send_klv: {e}"))?;
            }
            Event::Audio { frame_idx } => {
                let frame = fixtures::aac_frame(frame_idx);
                shell
                    .send_audio(&frame, pts)
                    .map_err(|e| format!("send_audio: {e}"))?;
            }
        }
    }

    let publisher = shell
        .finish()
        .map_err(|e| format!("MuxPublisher::finish: {e}"))?;
    let server = publisher
        .finish_serving()
        .map_err(|e| format!("HlsPublisher::finish_serving: {e}"))?;
    thread::sleep(LINGER);
    server.shutdown();
    Ok(())
}

/// Serve `p`'s synthetic traffic over RTSP: bind an [`RtspServer`] at
/// `bind_addr`, register `mount`, and push the profile's video/KLV/audio
/// through the resulting `MountHandle`, wall-clock paced. PTS-only (no
/// DTS) — `MountHandle::push_video_to_with_dts` exists but DTS parity
/// isn't needed for baseline serving.
///
/// Prints the bound mount URL to stdout as a single JSON line
/// (`{"serving": "rtsp://host:port/mount"}`) as soon as the listener is
/// bound — before any traffic is pushed.
///
/// Blocks for `seconds` (wall-clock paced push), then keeps the server up
/// for `LINGER` before calling `stop()`.
///
/// `klv`/`klv_seed` pick the ST 0601 record factory and `au_sizes` the
/// video AU size regime — see `gen::run`'s doc comment for both.
///
/// # Scope
/// `MountHandle` (`tst_rtp::rtsp::server::mount::MountHandle`) mirrors
/// `MuxSender`'s handle-targeted `push_*_to` family — including
/// multi-program (`two-program`) and audio — but has no
/// `push_video_misp`/`push_video_misp_to`. A `KlvMode::AsyncWithMisp`
/// profile (only `misp` in the registry) is rejected up front with a
/// clear error rather than silently served without its MISP SEI.
pub fn run_rtsp(
    p: &Profile,
    bind_addr: SocketAddr,
    mount: &str,
    seconds: f64,
    klv: KlvSet,
    klv_seed: u64,
    au_sizes: AuSizeMode,
) -> Result<(), String> {
    if p.klv == KlvMode::AsyncWithMisp {
        return Err(format!(
            "profile {}: tst_rtp::MountHandle has no push_video_misp/push_video_misp_to — \
             RTSP serve cannot carry the MISP SEI",
            p.name
        ));
    }

    let cfg = mux_setup::build_config(p);
    let server = RtspServer::bind(&format!("rtsp://{bind_addr}"))
        .map_err(|e| format!("RtspServer::bind: {e}"))?;
    let mount_handle = server
        .add_mount(mount, cfg)
        .map_err(|e| format!("add_mount: {e}"))?;
    server
        .start()
        .map_err(|e| format!("RtspServer::start: {e}"))?;
    let addr = server
        .local_addr()
        .ok_or_else(|| "RTSP server has no bound address".to_string())?;
    println!("{{\"serving\": \"rtsp://{addr}{mount}\"}}");

    let video_handles = mount_handle.video_handles();
    let klv_handles = mount_handle.klv_handles();
    let audio_handle = mount_handle.audio_handles().into_iter().next();

    let (start, events) = schedule::build_schedule(p, seconds);
    let wall_start = Instant::now();
    for (pts_ticks, event) in events {
        sleep_until(wall_start, start, pts_ticks);
        let pts = Pts90khz::new(pts_ticks);
        match event {
            Event::Video { frame_idx } => {
                let (au, keyframe) = fixtures::video_au_sized(p.video, frame_idx, au_sizes);
                for &handle in &video_handles {
                    mount_handle
                        .push_video_to(handle, &au, pts, keyframe)
                        .map_err(|e| format!("push_video_to: {e}"))?;
                }
            }
            Event::Klv { seq } => {
                let record = fixtures::klv_record_for(klv, klv_seed, seq)?;
                for &handle in &klv_handles {
                    mount_handle
                        .push_klv_to(handle, &record, pts, 0x00)
                        .map_err(|e| format!("push_klv_to: {e}"))?;
                }
            }
            Event::Audio { frame_idx } => {
                if let Some(handle) = audio_handle {
                    let frame = fixtures::aac_frame(frame_idx);
                    mount_handle
                        .push_audio_to(handle, &frame, pts)
                        .map_err(|e| format!("push_audio_to: {e}"))?;
                }
            }
        }
    }

    thread::sleep(LINGER);
    server
        .stop()
        .map_err(|e| format!("RtspServer::stop: {e}"))?;
    Ok(())
}

/// Which serve (bind) mode a URL's scheme selects, or `None` if
/// `transport::make_send`'s connect-side dispatch should handle it
/// instead. Consulted by the CLI's `send` subcommand.
pub enum ServeScheme {
    Hls,
    Rtsp,
}

/// Classify `url`'s scheme as a serve (bind) scheme. Mirrors
/// `transport.rs`'s own private `scheme_of` — a distinct copy rather than
/// a shared helper, per this crate's convention of small per-module
/// "same shape" parsing (see `gen.rs`/`send.rs`'s duplicated `Event`).
fn scheme_of(url: &str) -> Option<&str> {
    url.split_once("://").map(|(scheme, _)| scheme)
}

/// `url`'s scheme, classified as a serve (bind) scheme, or `None` for
/// anything `transport::make_send` should handle instead.
pub fn serve_scheme_of(url: &str) -> Option<ServeScheme> {
    match scheme_of(url)? {
        "hls" | "hlss" => Some(ServeScheme::Hls),
        "rtsp" | "rtsps" => Some(ServeScheme::Rtsp),
        _ => None,
    }
}

/// Parse an `hls://` URL's bind address and serve `p` over it for
/// `seconds`. `hlss://` (TLS) is rejected — this crate's serve modes
/// don't wire cert/key options through the CLI.
pub fn run_hls_url(
    p: &Profile,
    url: &str,
    seconds: f64,
    klv: KlvSet,
    klv_seed: u64,
    au_sizes: AuSizeMode,
) -> Result<(), String> {
    let parsed = tst_hls::HlsUrl::parse(url).map_err(|e| format!("hls url {url}: {e}"))?;
    if parsed.tls {
        return Err(
            "hlss:// (TLS) is not implemented by tst-interop's serve mode; use hls://".to_string(),
        );
    }
    let bind_addr = SocketAddr::new(parsed.addr, parsed.port);
    run_hls(p, bind_addr, seconds, klv, klv_seed, au_sizes)
}

/// Parse an `rtsp://` URL's bind address + mount path and serve `p` over
/// it for `seconds`. `rtsps://` (TLS) is rejected for the same reason as
/// [`run_hls_url`]. The URL's path becomes the mount path; a URL with no
/// path is rejected (a serve needs a mount to register).
pub fn run_rtsp_url(
    p: &Profile,
    url: &str,
    seconds: f64,
    klv: KlvSet,
    klv_seed: u64,
    au_sizes: AuSizeMode,
) -> Result<(), String> {
    let parsed = tst_rtp::RtspUrl::parse(url).map_err(|e| format!("rtsp url {url}: {e}"))?;
    if parsed.scheme() == tst_rtp::RtspScheme::Rtsps {
        return Err(
            "rtsps:// (TLS) is not implemented by tst-interop's serve mode; use rtsp://"
                .to_string(),
        );
    }
    if parsed.path.is_empty() {
        return Err(format!(
            "rtsp url {url}: serve mode requires a mount path (e.g. rtsp://host:port/mount)"
        ));
    }
    let host = if parsed.host.is_empty() {
        "0.0.0.0"
    } else {
        parsed.host.as_str()
    };
    let ip: std::net::IpAddr = host
        .parse()
        .map_err(|e| format!("rtsp url {url}: host '{host}' is not a literal IP: {e}"))?;
    let bind_addr = SocketAddr::new(ip, parsed.port);
    run_rtsp(p, bind_addr, &parsed.path, seconds, klv, klv_seed, au_sizes)
}
