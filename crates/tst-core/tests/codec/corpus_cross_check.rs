//! Opt-in cross-checks of the codec parsers against `ffprobe` on real
//! transport streams: the generated AV1 and H.266 fixtures
//! (`tests/fixtures/{av1,h266}/`, written by `regen.sh` when the local
//! ffmpeg has libaom / libvvenc; gated on `SRT_RUN_CORPUS_CROSSCHECK=1`) and
//! the gitignored local corpus (`tests/fixtures/local/`, H.264 / H.265).
//! Each case compares the parser's width × height with ffprobe's. Profile
//! and level are logged, not compared: ffprobe's strings have no stable
//! mapping to the numeric profile / level the parsers return. Every case
//! no-ops cleanly when its directory, its fixtures or ffprobe are missing.

use std::path::Path;
use std::process::Command;

use tst_core::codec::{av1, h264, h265, h266};
use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, SamplePayload, VideoCodec, VideoPayload, split_video,
};
use tst_core::mpegts::mux::Av1CarriageMode;

/// What a parser recovered from one fixture.
struct Parsed {
    width: u32,
    height: u32,
    profile: u8,
    level: Option<u8>,
}

struct CorpusCase {
    /// Log label.
    label: &'static str,
    /// Fixture directory under the crate root.
    dir: &'static str,
    /// Run only when this variable is set (`None`: always run).
    env_gate: Option<&'static str>,
    /// Skip the whole case when ffprobe is missing (`false`: skip per file).
    require_ffprobe: bool,
    /// A fixture the parser recovers nothing from is a mismatch with this
    /// text (`None`: the file is skipped).
    unparsed: Option<&'static str>,
    parse: fn(&mut Demuxer) -> Option<Parsed>,
}

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

fn run(case: &CorpusCase) {
    if let Some(var) = case.env_gate {
        if std::env::var(var).is_err() {
            eprintln!("skipping (set {var}=1 to enable)");
            return;
        }
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(case.dir);
    if !dir.is_dir() {
        eprintln!("skipping (no fixtures at {dir:?})");
        return;
    }
    if case.require_ffprobe && !ffprobe_available() {
        eprintln!("skipping (ffprobe not available)");
        return;
    }
    let mut checked = 0usize;
    let mut mismatches = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let p = entry.unwrap().path();
        if p.extension().and_then(|s| s.to_str()) != Some("ts") {
            continue;
        }
        let bytes = std::fs::read(&p).unwrap();
        let mut dx = Demuxer::new();
        // Best effort: a malformed fixture's sync failure must not abort the run.
        let _ = dx.feed(&bytes);
        dx.flush();
        let parsed = (case.parse)(&mut dx);
        if parsed.is_none() && case.unparsed.is_none() {
            eprintln!("{}: no parameter sets found, skipping", p.display());
            continue;
        }
        let Some((fw, fh, profile, level)) = ffprobe_video_stream(&p) else {
            eprintln!("ffprobe failed on {p:?}, skipping");
            continue;
        };
        match parsed {
            Some(g) if (g.width, g.height) == (fw, fh) => {
                eprintln!(
                    "{}: ok {}x{} (parsed profile={}, level={:?}; ffprobe profile={profile}, level={level})",
                    p.display(),
                    g.width,
                    g.height,
                    g.profile,
                    g.level,
                );
                checked += 1;
            }
            Some(g) => mismatches.push(format!(
                "{}: parsed {}x{} != ffprobe {fw}x{fh}",
                p.display(),
                g.width,
                g.height
            )),
            None => mismatches.push(format!(
                "{}: {}",
                p.display(),
                case.unparsed.unwrap_or_default()
            )),
        }
    }
    eprintln!(
        "{} cross-check: {checked} fixtures matched ffprobe",
        case.label
    );
    assert!(
        mismatches.is_empty(),
        "mismatches:\n{}",
        mismatches.join("\n")
    );
}

/// AV1 sequence headers need not ride the first sample: collect every OBU,
/// then parse once and take the first sequence header.
fn parse_av1(dx: &mut Demuxer) -> Option<Parsed> {
    let mut all_obus = Vec::new();
    while let Some(ev) = dx.next_event() {
        if let DemuxEvent::Sample {
            payload:
                SamplePayload::Video {
                    codec: VideoCodec::Av1,
                    raw,
                    ..
                },
            ..
        } = ev
        {
            if let VideoPayload::Obus(obus) =
                split_video(&raw, VideoCodec::Av1, Av1CarriageMode::Mpeg2TsBinding).0
            {
                all_obus.extend(obus);
            }
        }
    }
    if all_obus.is_empty() {
        return None;
    }
    let stream = av1::parse_obu_stream(&all_obus);
    let sh = stream.sequence_headers.first()?;
    Some(Parsed {
        width: sh.max_frame_width,
        height: sh.max_frame_height,
        profile: sh.profile,
        level: None,
    })
}

/// The first H.266 sample whose NALs yield an SPS.
fn parse_h266(dx: &mut Demuxer) -> Option<Parsed> {
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
            let VideoPayload::Nals(nals) =
                split_video(&raw, VideoCodec::H266, Av1CarriageMode::Mpeg2TsBinding).0
            else {
                continue;
            };
            if let Ok(sets) = h266::parse_parameter_sets(&nals) {
                if let Some(sps) = sets.spses.first() {
                    return Some(Parsed {
                        width: sps.width,
                        height: sps.height,
                        profile: sps.profile_tier_level.general_profile_idc,
                        level: Some(sps.profile_tier_level.general_level_idc),
                    });
                }
            }
        }
    }
    None
}

/// The first H.264 or H.265 sample whose NALs yield an SPS.
fn parse_local(dx: &mut Demuxer) -> Option<Parsed> {
    while let Some(ev) = dx.next_event() {
        let DemuxEvent::Sample {
            payload: SamplePayload::Video { codec, raw, .. },
            ..
        } = ev
        else {
            continue;
        };
        let VideoPayload::Nals(nals) = split_video(&raw, codec, Av1CarriageMode::Mpeg2TsBinding).0
        else {
            continue;
        };
        match codec {
            VideoCodec::H264 => {
                if let Ok(ps) = h264::parse_parameter_sets(&nals) {
                    if let Some(sps) = ps.sps_by_id.values().next() {
                        return Some(Parsed {
                            width: sps.width,
                            height: sps.height,
                            profile: sps.profile_idc,
                            level: Some(sps.level_idc),
                        });
                    }
                }
            }
            VideoCodec::H265 => {
                if let Ok(ps) = h265::parse_parameter_sets(&nals) {
                    if let Some(sps) = ps.sps_by_id.values().next() {
                        return Some(Parsed {
                            width: sps.width,
                            height: sps.height,
                            profile: sps.general_profile_idc,
                            level: Some(sps.general_level_idc),
                        });
                    }
                }
            }
            // H.266 and AV1 have their own cases above.
            VideoCodec::H266 | VideoCodec::Av1 => {}
        }
    }
    None
}

#[test]
fn av1_fixtures_match_ffprobe() {
    run(&CorpusCase {
        label: "av1",
        dir: "tests/fixtures/av1",
        env_gate: Some("SRT_RUN_CORPUS_CROSSCHECK"),
        require_ffprobe: true,
        unparsed: Some("no SH extracted"),
        parse: parse_av1,
    });
}

#[test]
fn h266_fixtures_match_ffprobe() {
    run(&CorpusCase {
        label: "h266",
        dir: "tests/fixtures/h266",
        env_gate: Some("SRT_RUN_CORPUS_CROSSCHECK"),
        require_ffprobe: true,
        unparsed: Some("no SPS extracted"),
        parse: parse_h266,
    });
}

#[test]
fn local_corpus_parameter_sets_match_ffprobe() {
    run(&CorpusCase {
        label: "local",
        dir: "tests/fixtures/local",
        env_gate: None,
        require_ffprobe: false,
        unparsed: None,
        parse: parse_local,
    });
}
