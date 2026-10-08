//! §1 "Shape classification": which announced SDPs we accept.

use super::PublishShape;
use crate::h264::fmtp::H264FmtpParams;
use crate::sdp::{Sdp, SdpMedia};

pub(crate) const MAX_ANNOUNCE_TRACKS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackKind {
    Mp2t,
    H264,
    Klv,
}

#[derive(Debug, Clone)]
pub(crate) struct AnnouncedTrack {
    /// SDP media index — also this track's position in
    /// `AnnounceShape::tracks` (built in SDP order), which is the `track`
    /// index the session hands a `PublishAdapter`.
    pub(crate) index: usize,
    pub(crate) control: Option<String>,
    pub(crate) payload_type: u8,
    pub(crate) kind: TrackKind,
    /// The H.264 track's fmtp parameters; the elementary adapter seeds
    /// its depacketizer with `sprop-parameter-sets`.
    pub(crate) h264_fmtp: Option<H264FmtpParams>,
}

#[derive(Debug, Clone)]
pub(crate) struct AnnounceShape {
    pub(crate) shape: PublishShape,
    pub(crate) tracks: Vec<AnnouncedTrack>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShapeReject {
    /// 400 — structurally unusable (no media, >4 tracks, control ids missing/duplicated).
    BadRequest(&'static str),
    /// 415 — well-formed, but no accepted shape matches.
    Unsupported(&'static str),
}

/// The encoding name of `pt` from this media's `a=rtpmap`, lowercased
/// (`"h264"`, `"mp2t"`, `"smpte336m"`), and its clock rate when it parses,
/// or `None` when there is no `a=rtpmap` for `pt`.
fn rtpmap_encoding(media: &SdpMedia, pt: u8) -> Option<(String, Option<u32>)> {
    media.attributes.iter().find_map(|(k, v)| {
        if !k.eq_ignore_ascii_case("rtpmap") {
            return None;
        }
        let v = v.as_deref()?;
        let (pt_str, rest) = v.trim().split_once(' ')?;
        if pt_str.parse::<u8>().ok()? != pt {
            return None;
        }
        let mut parts = rest.split('/');
        let name = parts.next()?.trim().to_ascii_lowercase();
        let rate = parts.next().and_then(|r| r.trim().parse::<u32>().ok());
        Some((name, rate))
    })
}

/// The clock rate the elementary adapter assumes for H.264 (RFC 6184
/// §8.2.1 mandates it) and KLV (its aligner and fallback count KLV ticks
/// at the video rate).
const ELEMENTARY_CLOCK_RATE: u32 = 90_000;

fn classify_track(index: usize, m: &SdpMedia) -> Result<AnnouncedTrack, ShapeReject> {
    let pt = *m
        .payload_types
        .first()
        .ok_or(ShapeReject::BadRequest("m= line without a payload type"))?;
    if m.payload_types.len() != 1 {
        return Err(ShapeReject::Unsupported(
            "multiple payload types on one m= line",
        ));
    }
    let enc = rtpmap_encoding(m, pt);
    let kind = match (pt, enc.as_ref().map(|(n, r)| (n.as_str(), *r))) {
        (33, None) | (_, Some(("mp2t", _))) => TrackKind::Mp2t,
        (_, Some(("h264", Some(ELEMENTARY_CLOCK_RATE)))) => TrackKind::H264,
        (_, Some(("smpte336m", Some(ELEMENTARY_CLOCK_RATE)))) => TrackKind::Klv,
        (_, Some(("h264" | "smpte336m", _))) => {
            return Err(ShapeReject::Unsupported(
                "H264 or smpte336m clock rate is not 90000",
            ));
        }
        _ => {
            return Err(ShapeReject::Unsupported(
                "payload type is not MP2T, H264 or smpte336m",
            ));
        }
    };
    let h264_fmtp = (kind == TrackKind::H264).then(|| H264FmtpParams::parse(m, pt));
    Ok(AnnouncedTrack {
        index,
        control: m.control.clone(),
        payload_type: pt,
        kind,
        h264_fmtp,
    })
}

pub(crate) fn classify_announce(sdp: &Sdp) -> Result<AnnounceShape, ShapeReject> {
    if sdp.media.is_empty() {
        return Err(ShapeReject::BadRequest("SDP has no m= line"));
    }
    if sdp.media.len() > MAX_ANNOUNCE_TRACKS {
        return Err(ShapeReject::BadRequest("more than four m= lines"));
    }
    let tracks = sdp
        .media
        .iter()
        .enumerate()
        .map(|(i, m)| classify_track(i, m))
        .collect::<Result<Vec<_>, _>>()?;
    if tracks.len() > 1 {
        let mut seen = std::collections::HashSet::new();
        for t in &tracks {
            let c = t.control.as_deref().ok_or(ShapeReject::BadRequest(
                "multi-track announce without a=control on every track",
            ))?;
            if !seen.insert(c) {
                return Err(ShapeReject::BadRequest("duplicate a=control id"));
            }
        }
    }
    let n = |k: TrackKind| tracks.iter().filter(|t| t.kind == k).count();
    let shape = match (n(TrackKind::Mp2t), n(TrackKind::H264), n(TrackKind::Klv)) {
        (1, 0, 0) => PublishShape::Mp2t,
        (0, 1, 0) => PublishShape::Elementary { klv: false },
        (0, 1, 1) => PublishShape::Elementary { klv: true },
        _ => {
            return Err(ShapeReject::Unsupported(
                "track combination is not MP2T, H264, or H264+smpte336m",
            ));
        }
    };
    Ok(AnnounceShape { shape, tracks })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdp::Sdp;

    fn sdp(body: &str) -> Sdp {
        Sdp::parse(body.replace('\n', "\r\n").as_bytes()).unwrap()
    }
    const HEAD: &str = "v=0\no=- 0 0 IN IP4 127.0.0.1\ns=x\nc=IN IP4 127.0.0.1\nt=0 0\n";

    #[test]
    fn mp2t_static_pt_33_without_rtpmap() {
        let s = sdp(&format!(
            "{HEAD}m=video 0 RTP/AVP 33\na=control:streamid=0\n"
        ));
        let a = classify_announce(&s).unwrap();
        assert_eq!(a.shape, PublishShape::Mp2t);
        assert_eq!(a.tracks.len(), 1);
        assert_eq!(a.tracks[0].payload_type, 33);
        assert!(matches!(a.tracks[0].kind, TrackKind::Mp2t));
        assert_eq!(a.tracks[0].control.as_deref(), Some("streamid=0"));
    }

    #[test]
    fn mp2t_dynamic_pt_with_rtpmap() {
        let s = sdp(&format!(
            "{HEAD}m=video 0 RTP/AVP 96\na=rtpmap:96 MP2T/90000\na=control:trackID=0\n"
        ));
        let a = classify_announce(&s).unwrap();
        assert_eq!(a.shape, PublishShape::Mp2t);
        assert_eq!(a.tracks[0].payload_type, 96);
    }

    #[test]
    fn ffmpeg_h264_only_is_elementary_without_klv() {
        // Verbatim shape from spec appendix A.
        let s = sdp(&format!(
            "{HEAD}a=tool:libavformat 60.16.100\nm=video 0 RTP/AVP 96\na=rtpmap:96 H264/90000\na=fmtp:96 packetization-mode=1; sprop-parameter-sets=Z/QADJGWgUH7ARAAAAMAEAAAAwHg8UKq,aM4PGSA=; profile-level-id=F4000C\na=control:streamid=0\n"
        ));
        let a = classify_announce(&s).unwrap();
        assert_eq!(a.shape, PublishShape::Elementary { klv: false });
        assert_eq!(
            a.tracks[0]
                .h264_fmtp
                .as_ref()
                .unwrap()
                .sprop_parameter_sets
                .len(),
            2
        );
    }

    #[test]
    fn h264_plus_smpte336m_is_elementary_with_klv() {
        let s = sdp(&format!(
            "{HEAD}m=video 0 RTP/AVP 96\na=rtpmap:96 H264/90000\na=control:streamid=0\nm=application 0 RTP/AVP 97\na=rtpmap:97 smpte336m/90000\na=control:streamid=1\n"
        ));
        let a = classify_announce(&s).unwrap();
        assert_eq!(a.shape, PublishShape::Elementary { klv: true });
        assert!(matches!(a.tracks[1].kind, TrackKind::Klv));
        assert_eq!(a.tracks[1].payload_type, 97);
    }

    #[test]
    fn rtpmap_encoding_names_are_case_insensitive() {
        let s = sdp(&format!(
            "{HEAD}m=video 0 RTP/AVP 96\na=rtpmap:96 h264/90000\na=control:a\nm=application 0 RTP/AVP 97\na=rtpmap:97 SMPTE336M/90000\na=control:b\n"
        ));
        assert_eq!(
            classify_announce(&s).unwrap().shape,
            PublishShape::Elementary { klv: true }
        );
    }

    #[test]
    fn unsupported_mixes_are_415() {
        for body in [
            "m=audio 0 RTP/AVP 97\na=rtpmap:97 MPEG4-GENERIC/48000/2\na=control:a\n",
            "m=video 0 RTP/AVP 96\na=rtpmap:96 H265/90000\na=control:a\n",
            "m=video 0 RTP/AVP 96\na=rtpmap:96 H264/90000\na=control:a\nm=video 0 RTP/AVP 97\na=rtpmap:97 H264/90000\na=control:b\n",
            "m=application 0 RTP/AVP 97\na=rtpmap:97 smpte336m/90000\na=control:a\n",
            "m=video 0 RTP/AVP 96\na=control:a\n", // dynamic PT, no rtpmap (ffmpeg's KLV track shape)
            "m=video 0 RTP/AVP 33\na=control:a\nm=application 0 RTP/AVP 97\na=rtpmap:97 smpte336m/90000\na=control:b\n",
        ] {
            let s = sdp(&format!("{HEAD}{body}"));
            assert!(
                matches!(classify_announce(&s), Err(ShapeReject::Unsupported(_))),
                "{body}"
            );
        }
    }

    #[test]
    fn elementary_clock_rates_other_than_90000_are_415() {
        for body in [
            "m=video 0 RTP/AVP 96\na=rtpmap:96 H264/90000\na=control:a\nm=application 0 RTP/AVP 97\na=rtpmap:97 smpte336m/1000\na=control:b\n",
            "m=video 0 RTP/AVP 96\na=rtpmap:96 H264/48000\na=control:a\n",
            "m=video 0 RTP/AVP 96\na=rtpmap:96 H264\na=control:a\n",
        ] {
            let s = sdp(&format!("{HEAD}{body}"));
            assert!(
                matches!(classify_announce(&s), Err(ShapeReject::Unsupported(_))),
                "{body}"
            );
        }
    }

    #[test]
    fn bad_control_sets_are_400() {
        // duplicate control ids
        let s = sdp(&format!(
            "{HEAD}m=video 0 RTP/AVP 96\na=rtpmap:96 H264/90000\na=control:x\nm=application 0 RTP/AVP 97\na=rtpmap:97 smpte336m/90000\na=control:x\n"
        ));
        assert!(matches!(
            classify_announce(&s),
            Err(ShapeReject::BadRequest(_))
        ));
        // multi-track with a missing control
        let s = sdp(&format!(
            "{HEAD}m=video 0 RTP/AVP 96\na=rtpmap:96 H264/90000\nm=application 0 RTP/AVP 97\na=rtpmap:97 smpte336m/90000\na=control:b\n"
        ));
        assert!(matches!(
            classify_announce(&s),
            Err(ShapeReject::BadRequest(_))
        ));
        // no media at all
        let s = sdp(HEAD);
        assert!(matches!(
            classify_announce(&s),
            Err(ShapeReject::BadRequest(_))
        ));
    }

    #[test]
    fn more_than_four_tracks_is_400() {
        let mut body = String::from(HEAD);
        for i in 0..5 {
            body.push_str(&format!(
                "m=video 0 RTP/AVP 96\na=rtpmap:96 H264/90000\na=control:t{i}\n"
            ));
        }
        assert!(matches!(
            classify_announce(&sdp(&body)),
            Err(ShapeReject::BadRequest(_))
        ));
    }
}
