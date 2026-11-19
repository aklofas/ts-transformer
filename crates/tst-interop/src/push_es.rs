//! `push-es` and `verify-es`: publish a generated file's H.264 video and
//! KLV to an RTSP server as elementary RTP streams (RFC 6184 + RFC 6597),
//! and judge what a reader pulled back from that server against what was
//! pushed.
//!
//! `push-es` demuxes the file, pushes each video access unit and each KLV
//! unit at its PTS against a monotonic clock (a 10 s file takes about
//! 10 s), and writes what it pushed to two sidecar files: `<out>.h264`
//! (the access units, Annex B, back to back) and `<out>.klv` (the KLV units
//! back to back). `verify-es` compares those with the reader's two files:
//! the reader's copy must be a prefix of what was pushed, short at most by
//! what a reader loses at the end of a stream (see [`judge_video`] and
//! [`judge_klv`]).

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{
    DemuxEvent, Demuxer, DemuxerConfig, SamplePayload, VideoCodec as DemuxVideoCodec,
};

use crate::schedule::PTS_HZ;
use crate::transport;

/// One unit of a generated file, in the order it is pushed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unit {
    /// A video access unit, Annex B, as the file carries it.
    Video { pts: Pts90khz, au: Vec<u8> },
    /// A KLV unit (the local set, without a metadata access unit header).
    Klv { pts: Pts90khz, unit: Vec<u8> },
}

impl Unit {
    fn pts(&self) -> Pts90khz {
        match self {
            Unit::Video { pts, .. } | Unit::Klv { pts, .. } => *pts,
        }
    }
}

/// Demux `path` into its H.264 access units and KLV units, sorted by PTS
/// (the sort is stable, so units with one PTS keep the file's order).
///
/// The default demuxer config is enough here: it differs from a profile's
/// own only in AV1 carriage, and this path takes H.264 only.
pub fn read_units(path: &Path) -> Result<Vec<Unit>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let mut demux = Demuxer::with_config(DemuxerConfig::default());
    let mut units = Vec::new();
    let mut take = |ev: DemuxEvent| -> Result<(), String> {
        match ev {
            DemuxEvent::Sample {
                pts,
                payload: SamplePayload::Video { codec, raw, .. },
                ..
            } => {
                if codec != DemuxVideoCodec::H264 {
                    return Err(format!(
                        "{}: video is {codec:?}; push-es pushes H.264 only",
                        path.display()
                    ));
                }
                units.push(Unit::Video {
                    pts,
                    au: raw.to_vec(),
                });
            }
            DemuxEvent::Metadata { pts, payload, .. } => {
                units.push(Unit::Klv { pts, unit: payload });
            }
            _ => {}
        }
        Ok(())
    };
    demux
        .feed(&bytes)
        .map_err(|e| format!("demuxing {}: {e}", path.display()))?;
    while let Some(ev) = demux.next_event() {
        take(ev)?;
    }
    // Without the flush the last access unit of every stream stays in the
    // reassembler.
    demux.flush();
    while let Some(ev) = demux.next_event() {
        take(ev)?;
    }
    units.sort_by_key(|u| u.pts().as_ticks());
    Ok(units)
}

/// What [`run`] pushes and where.
#[derive(Debug, Clone)]
pub struct PushEs {
    /// `rtsp://` or `rtsps://` URL of the server to publish to.
    pub url: String,
    /// The generated `.ts` file to push.
    pub file: PathBuf,
    /// Announce and push a KLV track too; without it the file's KLV is
    /// skipped.
    pub klv: bool,
    /// Sidecar prefix: `<out>.h264` and `<out>.klv` are written.
    pub out: PathBuf,
    /// How long a refused connect is retried (see
    /// [`transport::rtsp_push_connect`]).
    pub refused_for: Duration,
    /// Quiet time after RECORD before the first unit (see
    /// [`crate::send::PushStart::start_delay`]).
    pub start_delay: Duration,
}

/// What [`run`] pushed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushEsSummary {
    pub video_aus: u64,
    pub video_bytes: u64,
    pub klv_units: u64,
    pub klv_bytes: u64,
}

/// `<prefix>.<ext>`, keeping any dots already in the prefix.
pub fn sidecar(prefix: &Path, ext: &str) -> PathBuf {
    let mut name = prefix.as_os_str().to_owned();
    name.push(".");
    name.push(ext);
    PathBuf::from(name)
}

/// Push `opts.file` to `opts.url` as elementary streams, paced by PTS, then
/// close the push. Fails unless the close reports a finished push
/// ([`transport::push_close_finished`]): TEARDOWN accepted, or refused with
/// a status after every queued frame was written.
pub fn run(opts: &PushEs) -> Result<PushEsSummary, String> {
    let units = read_units(&opts.file)?;
    let first_au = units
        .iter()
        .find_map(|u| match u {
            Unit::Video { au, .. } => Some(au),
            Unit::Klv { .. } => None,
        })
        .ok_or_else(|| format!("{}: no video access unit", opts.file.display()))?;
    let video = tst_rtp::VideoTrack::h264_from_key_frame(first_au).map_err(|e| {
        format!(
            "{}: the first access unit carries no SPS + PPS: {e}",
            opts.file.display()
        )
    })?;
    let desc = tst_rtp::PushDescription::Elementary {
        video,
        klv: opts.klv,
    };
    let mut push = transport::rtsp_push_connect(&opts.url, &desc, opts.refused_for)?;
    eprintln!("push-es: RTSP push recording to {}", opts.url);
    thread::sleep(opts.start_delay);

    let first_pts = units[0].pts().as_ticks();
    let mut pushed_h264 = Vec::new();
    let mut pushed_klv = Vec::new();
    let mut summary = PushEsSummary::default();
    let wall_start = Instant::now();
    for unit in &units {
        if matches!(unit, Unit::Klv { .. }) && !opts.klv {
            continue;
        }
        // Sleep until this unit's offset from the first PTS has elapsed;
        // recomputed from the start each time, so pacing does not drift.
        let offset = (unit.pts().as_ticks() - first_pts).max(0) as f64 / PTS_HZ as f64;
        let elapsed = wall_start.elapsed();
        if let Some(wait) = Duration::from_secs_f64(offset).checked_sub(elapsed) {
            thread::sleep(wait);
        }
        match unit {
            Unit::Video { pts, au } => {
                push.send_video(au, *pts)
                    .map_err(|e| format!("send_video: {e}"))?;
                pushed_h264.extend_from_slice(au);
                summary.video_aus += 1;
                summary.video_bytes += au.len() as u64;
            }
            Unit::Klv { pts, unit } => {
                push.send_klv(unit, *pts)
                    .map_err(|e| format!("send_klv: {e}"))?;
                pushed_klv.extend_from_slice(unit);
                summary.klv_units += 1;
                summary.klv_bytes += unit.len() as u64;
            }
        }
    }
    for (ext, bytes) in [("h264", &pushed_h264), ("klv", &pushed_klv)] {
        let path = sidecar(&opts.out, ext);
        std::fs::write(&path, bytes).map_err(|e| format!("writing {}: {e}", path.display()))?;
    }

    let stats = push.stats();
    eprintln!(
        "push-es: {} RTP packets, {} bytes, {} sender reports, {} receiver reports received",
        stats.packets_sent, stats.bytes_sent, stats.sender_reports_sent, stats.receiver_reports
    );
    let result = push.close();
    eprintln!("{}", transport::push_close_line("push-es", &result));
    if !transport::push_close_finished(&result) {
        let why = result.err().map_or_else(String::new, |e| e.to_string());
        return Err(format!("RTSP push to {}: close: {why}", opts.url));
    }
    Ok(summary)
}

/// The NAL units of an Annex B stream, without their start codes. A NAL
/// unit ends where the next start code (`00 00 01`, or `00 00 00 01`)
/// begins; zero bytes before a start code (a four-byte start code's first
/// byte, `trailing_zero_8bits`) are not part of the unit.
pub fn split_nals(annexb: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= annexb.len() {
        if annexb[i] == 0 && annexb[i + 1] == 0 && annexb[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (n, &start) in starts.iter().enumerate() {
        // The next unit's start code begins 3 bytes before its payload.
        let mut end = starts.get(n + 1).map_or(annexb.len(), |&next| next - 3);
        while end > start && annexb[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            nals.push(&annexb[start..end]);
        }
    }
    nals
}

/// H.264 NAL unit types 1 to 5 are coded slices: one per picture in the
/// generator's streams.
fn is_vcl(nal: &[u8]) -> bool {
    matches!(nal.first().map(|b| b & 0x1F), Some(1..=5))
}

/// H.264 NAL unit types 7 (SPS) and 8 (PPS).
fn is_parameter_set(nal: &[u8]) -> bool {
    matches!(nal.first().map(|b| b & 0x1F), Some(7 | 8))
}

/// Judge the pulled video against the pushed one, NAL unit by NAL unit
/// (start code lengths are the depacketizer's choice, so they are not
/// compared). The pulled units must be the pushed ones from the first on,
/// with at least one picture, and what is missing at the end may hold at
/// most one picture: a reader that ends with the stream does not write
/// the last access unit (ffmpeg 6.1 does not).
///
/// One addition is allowed: copies of the pushed parameter sets ahead of
/// the first pushed unit. A depacketizer that writes a byte stream puts
/// the SDP's `sprop-parameter-sets` there (GStreamer's `rtph264depay`
/// does); each must equal an SPS or PPS that was pushed. `Ok` carries a
/// summary.
pub fn judge_video(pushed: &[u8], pulled: &[u8]) -> Result<String, String> {
    let pushed = split_nals(pushed);
    let pulled = split_nals(pulled);
    if pushed.is_empty() {
        return Err("video: the pushed stream holds no NAL unit".to_string());
    }
    if !pulled.iter().any(|n| is_vcl(n)) {
        return Err(format!(
            "video: the pulled stream holds no picture ({} NAL units)",
            pulled.len()
        ));
    }
    let lead = pulled
        .iter()
        .take_while(|n| is_parameter_set(n) && pushed.iter().any(|p| p == *n))
        .count();
    // The fewest leading copies after which the rest lines up with the
    // pushed units; none lines up: judge from the first pulled unit.
    let inserted = (0..=lead)
        .find(|&o| {
            pulled.len() - o <= pushed.len() && pulled[o..].iter().zip(&pushed).all(|(a, b)| a == b)
        })
        .unwrap_or(0);
    let rest = &pulled[inserted..];
    if rest.len() > pushed.len() {
        return Err(format!(
            "video: {} NAL units pulled, more than the {} pushed",
            rest.len(),
            pushed.len()
        ));
    }
    if let Some(at) = rest.iter().zip(&pushed).position(|(a, b)| a != b) {
        return Err(format!(
            "video: NAL unit {at} differs (pulled {} bytes, type {}; pushed {} bytes, type {})",
            rest[at].len(),
            rest[at].first().map_or(0, |b| b & 0x1F),
            pushed[at].len(),
            pushed[at].first().map_or(0, |b| b & 0x1F),
        ));
    }
    let missing_pictures = pushed[rest.len()..].iter().filter(|n| is_vcl(n)).count();
    if missing_pictures > 1 {
        return Err(format!(
            "video: {} of {} NAL units pulled; the {missing_pictures} pictures missing at the \
             end are more than the last access unit",
            rest.len(),
            pushed.len()
        ));
    }
    Ok(format!(
        "video: {} of {} NAL units pulled in order ({inserted} parameter sets added ahead), \
         {missing_pictures} picture missing at the end",
        rest.len(),
        pushed.len()
    ))
}

/// The share of the pushed KLV bytes the pulled copy must hold.
pub const KLV_MIN_SHARE: f64 = 0.9;

/// Judge the pulled KLV bytes against the pushed ones: a non-empty prefix
/// of them holding at least [`KLV_MIN_SHARE`] of their length. The reader
/// writes units in arrival order and can miss those that arrive after it
/// stopped reading, never one in the middle. `Ok` carries a summary.
pub fn judge_klv(pushed: &[u8], pulled: &[u8]) -> Result<String, String> {
    if pushed.is_empty() {
        return Err("klv: nothing was pushed".to_string());
    }
    if pulled.is_empty() {
        return Err(format!(
            "klv: nothing pulled of {} bytes pushed",
            pushed.len()
        ));
    }
    if !pushed.starts_with(pulled) {
        let at = pulled
            .iter()
            .zip(pushed)
            .position(|(a, b)| a != b)
            .unwrap_or(pushed.len());
        return Err(format!(
            "klv: the pulled bytes are not a prefix of the pushed ones (first difference at \
             byte {at}; pulled {}, pushed {})",
            pulled.len(),
            pushed.len()
        ));
    }
    let share = pulled.len() as f64 / pushed.len() as f64;
    if share < KLV_MIN_SHARE {
        return Err(format!(
            "klv: {} of {} bytes pulled ({:.1} %), under {:.0} %",
            pulled.len(),
            pushed.len(),
            share * 100.0,
            KLV_MIN_SHARE * 100.0
        ));
    }
    Ok(format!(
        "klv: {} of {} bytes pulled ({:.1} %), a prefix",
        pulled.len(),
        pushed.len(),
        share * 100.0
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SC: [u8; 4] = [0, 0, 0, 1];

    /// An Annex B stream of `nals`, each behind a four-byte start code.
    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&SC);
            out.extend_from_slice(n);
        }
        out
    }

    const SPS: &[u8] = &[0x67, 0x42, 0x00, 0x1F];
    const PPS: &[u8] = &[0x68, 0xCE, 0x06, 0xE2];
    const IDR: &[u8] = &[0x65, 0x88, 0x80, 0x40];
    const P1: &[u8] = &[0x41, 0xA5, 0x01];
    const P2: &[u8] = &[0x41, 0xA5, 0x02];

    /// Four access units: a key frame, two inter frames, a key frame.
    fn pushed() -> Vec<u8> {
        annexb(&[SPS, PPS, IDR, P1, P2, SPS, PPS, IDR])
    }

    #[test]
    fn split_nals_takes_three_and_four_byte_start_codes() {
        let mut s = vec![0, 0, 1, 0x67, 0x42];
        s.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x00]);
        s.extend_from_slice(&[0, 0, 1, 0x41]);
        assert_eq!(
            split_nals(&s),
            vec![&[0x67, 0x42][..], &[0x68, 0xCE][..], &[0x41][..]]
        );
        assert!(split_nals(&[0x41, 0x42]).is_empty());
    }

    #[test]
    fn video_equal_passes() {
        let s = pushed();
        let ok = judge_video(&s, &s).expect("equal");
        assert!(ok.contains("8 of 8"), "{ok}");
    }

    #[test]
    fn video_with_other_start_codes_passes() {
        let mut pulled = Vec::new();
        for n in split_nals(&pushed()) {
            pulled.extend_from_slice(&[0, 0, 1]);
            pulled.extend_from_slice(n);
        }
        judge_video(&pushed(), &pulled).expect("start code length is not compared");
    }

    #[test]
    fn video_short_by_the_last_access_unit_passes() {
        // The last access unit (SPS, PPS, IDR) is missing whole or in part.
        judge_video(&pushed(), &annexb(&[SPS, PPS, IDR, P1, P2])).expect("last AU missing");
        judge_video(&pushed(), &annexb(&[SPS, PPS, IDR, P1, P2, SPS]))
            .expect("the last key frame missing after its SPS");
    }

    #[test]
    fn video_short_by_more_than_one_access_unit_fails() {
        let err = judge_video(&pushed(), &annexb(&[SPS, PPS, IDR, P1])).unwrap_err();
        assert!(err.contains("the 2 pictures missing"), "{err}");
    }

    #[test]
    fn video_without_a_picture_fails() {
        let err = judge_video(&pushed(), &annexb(&[SPS, PPS])).unwrap_err();
        assert!(err.contains("no picture"), "{err}");
        assert!(judge_video(&pushed(), &[]).is_err());
    }

    #[test]
    fn video_diverging_fails() {
        let err = judge_video(&pushed(), &annexb(&[SPS, PPS, IDR, P2, P1])).unwrap_err();
        assert!(err.contains("NAL unit 3 differs"), "{err}");
        // A late joiner: the right units, but not from the first one on.
        let err = judge_video(&pushed(), &annexb(&[P1, P2, SPS, PPS, IDR])).unwrap_err();
        assert!(err.contains("NAL unit 0 differs"), "{err}");
    }

    #[test]
    fn video_with_the_parameter_sets_repeated_ahead_passes() {
        let mut pulled = annexb(&[SPS, PPS]);
        pulled.extend_from_slice(&pushed());
        let ok = judge_video(&pushed(), &pulled).expect("sprop copies ahead");
        assert!(
            ok.contains("8 of 8") && ok.contains("2 parameter sets added"),
            "{ok}"
        );
    }

    #[test]
    fn video_with_a_foreign_parameter_set_ahead_fails() {
        let mut pulled = annexb(&[&[0x67, 0x64, 0x00, 0x28]]);
        pulled.extend_from_slice(&pushed());
        let err = judge_video(&pushed(), &pulled).unwrap_err();
        assert!(err.contains("more than the 8 pushed"), "{err}");
        // A parameter set the depacketizer added later than the start.
        let pulled = annexb(&[SPS, PPS, IDR, SPS, PPS, P1, P2, SPS, PPS, IDR]);
        assert!(judge_video(&pushed(), &pulled).is_err());
    }

    #[test]
    fn video_longer_than_pushed_fails() {
        let mut pulled = pushed();
        pulled.extend_from_slice(&annexb(&[P1]));
        let err = judge_video(&pushed(), &pulled).unwrap_err();
        assert!(err.contains("more than the 8 pushed"), "{err}");
    }

    #[test]
    fn klv_equal_passes() {
        let pushed: Vec<u8> = (0..200u8).collect();
        let ok = judge_klv(&pushed, &pushed).expect("equal");
        assert!(ok.contains("100.0 %"), "{ok}");
    }

    #[test]
    fn klv_prefix_of_at_least_ninety_percent_passes() {
        let pushed: Vec<u8> = (0..200u8).collect();
        judge_klv(&pushed, &pushed[..180]).expect("90 %");
    }

    #[test]
    fn klv_short_fails() {
        let pushed: Vec<u8> = (0..200u8).collect();
        let err = judge_klv(&pushed, &pushed[..179]).unwrap_err();
        assert!(err.contains("under 90 %"), "{err}");
        let err = judge_klv(&pushed, &[]).unwrap_err();
        assert!(err.contains("nothing pulled"), "{err}");
    }

    #[test]
    fn klv_diverging_fails() {
        let pushed: Vec<u8> = (0..200u8).collect();
        let mut pulled = pushed.clone();
        pulled[150] ^= 0xFF;
        let err = judge_klv(&pushed, &pulled).unwrap_err();
        assert!(err.contains("first difference at byte 150"), "{err}");
        let mut longer = pushed.clone();
        longer.push(0);
        assert!(judge_klv(&pushed, &longer).is_err());
    }

    #[test]
    fn sidecar_appends_the_extension() {
        assert_eq!(
            sidecar(Path::new("/w/cell.pushed"), "h264"),
            PathBuf::from("/w/cell.pushed.h264")
        );
    }
}
