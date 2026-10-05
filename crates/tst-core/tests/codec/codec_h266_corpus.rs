//! Opt-in real-corpus cross-check for `codec::h266`.
//!
//! Skipped unless `SRT_RUN_CORPUS_CROSSCHECK=1` is set. Compares our
//! parser's recovered width / height to ffprobe's decoded values on
//! any `.ts` files present in `tests/fixtures/h266/` (typically
//! produced by `regen.sh` if `ffmpeg --enable-libvvenc` is available
//! locally). Same shape as `local_codec_corpus.rs`'s H.264 / H.265
//! cross-check.
//!
//! profile/level string comparison is intentionally skipped — ffprobe's
//! H.266 profile/level strings vary by build / version and don't have
//! a stable mapping to `general_profile_idc` / `general_level_idc` we
//! could assert against today. We surface the parsed numeric idc values
//! in the success log line for human inspection.
//!
//! No-ops cleanly when the env var is unset, when the fixtures directory
//! is missing, when ffprobe isn't on PATH, or when no `.ts` files are
//! present (e.g. fresh checkout where vvenc-equipped ffmpeg wasn't
//! available to run `regen.sh`).

use std::path::Path;
use std::process::Command;
use tst_core::codec::h266;
use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, SamplePayload, VideoCodec, VideoPayload, split_video,
};
use tst_core::mpegts::mux::Av1CarriageMode;

fn ffprobe_available() -> bool {
    Command::new("ffprobe").arg("-version").output().is_ok()
}

fn ffprobe_video_stream(path: &Path) -> Option<(u32, u32, String, String)> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,profile,level",
            "-of",
            "default=nw=1",
            path.to_str()?,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut w = 0u32;
    let mut h = 0u32;
    let mut profile = String::new();
    let mut level = String::new();
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("width=") {
            w = v.parse().ok()?;
        }
        if let Some(v) = line.strip_prefix("height=") {
            h = v.parse().ok()?;
        }
        if let Some(v) = line.strip_prefix("profile=") {
            profile = v.into();
        }
        if let Some(v) = line.strip_prefix("level=") {
            level = v.into();
        }
    }
    Some((w, h, profile, level))
}

#[test]
fn h266_fixtures_match_ffprobe() {
    if std::env::var("SRT_RUN_CORPUS_CROSSCHECK").is_err() {
        eprintln!("skipping (set SRT_RUN_CORPUS_CROSSCHECK=1 to enable)");
        return;
    }
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/h266");
    if !fixtures_dir.is_dir() {
        eprintln!("skipping (no fixtures at {fixtures_dir:?})");
        return;
    }
    if !ffprobe_available() {
        eprintln!("skipping (ffprobe not available)");
        return;
    }

    let mut checked = 0usize;
    let mut mismatches = Vec::new();

    for entry in std::fs::read_dir(&fixtures_dir).unwrap() {
        let entry = entry.unwrap();
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("ts") {
            continue;
        }

        let bytes = std::fs::read(&p).unwrap();
        let mut dx = Demuxer::new();
        // feed returns Result — ignore errors (best-effort corpus check;
        // an unrecoverable sync failure on a malformed fixture shouldn't
        // abort the whole run).
        let _ = dx.feed(&bytes);
        dx.flush();

        // Walk events; on the first H.266 Sample whose NALs yield an SPS,
        // record dimensions + profile/level idc.
        let mut got_dims: Option<(u32, u32)> = None;
        let mut got_profile: Option<u8> = None;
        let mut got_level: Option<u8> = None;
        while let Some(ev) = dx.next_event() {
            if let DemuxEvent::Sample {
                payload:
                    SamplePayload::Video {
                        codec: VideoCodec::H266,
                        raw,
                        ..
                    },
                ..
            } = ev
            {
                // Raw-first: split the encoded AU into NALs via the opt-in call.
                let VideoPayload::Nals(nals) =
                    split_video(&raw, VideoCodec::H266, Av1CarriageMode::Mpeg2TsBinding).0
                else {
                    continue;
                };
                if let Ok(sets) = h266::parse_parameter_sets(&nals) {
                    if let Some(sps) = sets.spses.first() {
                        got_dims = Some((sps.width, sps.height));
                        got_profile = Some(sps.profile_tier_level.general_profile_idc);
                        got_level = Some(sps.profile_tier_level.general_level_idc);
                        break;
                    }
                }
            }
        }

        let Some((fw, fh, profile_str, level_str)) = ffprobe_video_stream(&p) else {
            eprintln!("ffprobe failed on {p:?}, skipping");
            continue;
        };

        match got_dims {
            Some((gw, gh)) if gw == fw && gh == fh => {
                eprintln!(
                    "{}: ok {}x{} (parsed profile_idc={}, level_idc={}; ffprobe profile={}, level={})",
                    p.display(),
                    gw,
                    gh,
                    got_profile.unwrap_or(0),
                    got_level.unwrap_or(0),
                    profile_str,
                    level_str,
                );
                checked += 1;
            }
            Some((gw, gh)) => {
                mismatches.push(format!(
                    "{}: parsed {}x{} != ffprobe {}x{}",
                    p.display(),
                    gw,
                    gh,
                    fw,
                    fh
                ));
            }
            None => {
                mismatches.push(format!("{}: no SPS extracted", p.display()));
            }
        }
    }

    eprintln!("h266 cross-check: {checked} fixtures matched ffprobe");
    assert!(
        mismatches.is_empty(),
        "mismatches:\n{}",
        mismatches.join("\n")
    );
}
