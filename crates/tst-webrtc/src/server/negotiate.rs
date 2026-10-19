//! Video payload selection after str0m has accepted a browser's offer.
//!
//! str0m intersects the offer with its own codec list; this module picks
//! the one payload type a mount sends on. H.264 needs
//! `packetization-mode=1` (the packetizer emits FU-A fragments), and among
//! those the payload whose profile matches the stream's SPS is preferred.
//!
//! The choice is read from the offer's own `a=rtpmap`/`a=fmtp` lines and
//! cross-checked against str0m. str0m's codec list alone is not enough:
//! after `accept_offer` it still holds local entries that matched nothing,
//! and such an entry can keep a payload number the offer uses for another
//! format (an offer whose H.264 payloads are all mode 0 leaves a mode-1
//! entry on one of their numbers).

use str0m::Rtc;
use str0m::format::Codec;
use str0m::media::{MediaKind, Mid, Pt};

use crate::VideoCodec;

/// The negotiation outcome for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Negotiated {
    /// The video media and payload type the mount sends on.
    Video {
        /// The video media's mid.
        mid: Mid,
        /// The payload type for the mount's codec.
        pt: Pt,
        /// The mount's codec.
        codec: VideoCodec,
    },
    /// The offer cannot carry the mount's video (answered with 406).
    Rejected {
        /// Why, for the response body and the log.
        reason: &'static str,
    },
}

/// Finds the payload type the browser offered for the mount's codec.
///
/// Call after `rtc` has accepted `offer` (the SDP text). A payload type
/// qualifies when the offer's video section declares it with the mount's
/// codec, str0m lists it among the media's remote payload types, and
/// str0m's codec list holds the same codec under that number. For H.264
/// both sides must also say `packetization-mode=1`; the first such payload
/// in the offer's order whose profile idc equals `sps_profile_level_id[0]`
/// wins, else the first such payload. For H.265 the first qualifying
/// payload wins (str0m matches the profile, tier and level itself).
pub fn select_video(
    rtc: &Rtc,
    offer: &str,
    want: VideoCodec,
    sps_profile_level_id: Option<[u8; 3]>,
) -> Negotiated {
    let (name, codec, no_payload) = match want {
        VideoCodec::H264 => (
            "H264",
            Codec::H264,
            "no packetization-mode=1 H.264 payload in the offer",
        ),
        VideoCodec::H265 => ("H265", Codec::H265, "no H.265 payload in the offer"),
        _ => {
            return Negotiated::Rejected {
                reason: "the mount's codec is not carried over WebRTC",
            };
        }
    };
    let mut saw_video = false;
    for section in video_sections(offer) {
        let Some(mid) = section.mid else {
            continue;
        };
        let Some(media) = rtc.media(mid) else {
            continue;
        };
        if media.kind() != MediaKind::Video {
            continue;
        }
        saw_video = true;
        let qualifies = |p: &&OfferedPayload| {
            p.name.eq_ignore_ascii_case(name)
                && (codec != Codec::H264 || p.packetization_mode == 1)
                && media.remote_pts().contains(&p.pt)
                && rtc.codec_config().params().iter().any(|c| {
                    c.pt() == p.pt
                        && c.spec().codec == codec
                        && (codec != Codec::H264 || c.spec().format.packetization_mode == Some(1))
                })
        };
        let preferred = match (want, sps_profile_level_id) {
            (VideoCodec::H264, Some(sps)) => section
                .payloads
                .iter()
                .filter(qualifies)
                .find(|p| p.profile_idc == Some(sps[0])),
            _ => None,
        };
        if let Some(p) = preferred.or_else(|| section.payloads.iter().find(qualifies)) {
            return Negotiated::Video {
                mid,
                pt: p.pt,
                codec: want,
            };
        }
    }
    Negotiated::Rejected {
        reason: if saw_video {
            no_payload
        } else {
            "the offer has no video media"
        },
    }
}

/// One payload type declared in an offer's video section.
struct OfferedPayload {
    pt: Pt,
    /// The encoding name from `a=rtpmap`.
    name: String,
    /// `packetization-mode` from `a=fmtp` (RFC 6184 default 0).
    packetization_mode: u8,
    /// The first byte of `profile-level-id` from `a=fmtp`.
    profile_idc: Option<u8>,
}

/// An offer's `m=video` section: its mid and payload types, in `m=` order.
struct VideoSection {
    mid: Option<Mid>,
    payloads: Vec<OfferedPayload>,
}

/// The `m=video` sections of an SDP.
fn video_sections(sdp: &str) -> Vec<VideoSection> {
    let mut sections: Vec<VideoSection> = Vec::new();
    let mut in_video = false;
    for line in sdp.lines() {
        let line = line.trim_end();
        if let Some(media) = line.strip_prefix("m=") {
            in_video = media.starts_with("video ");
            if in_video {
                let payloads = media
                    .split(' ')
                    .skip(3)
                    .filter_map(|pt| pt.parse::<u8>().ok())
                    .filter(|pt| *pt < 128)
                    .map(|pt| OfferedPayload {
                        pt: Pt::from(pt),
                        name: String::new(),
                        packetization_mode: 0,
                        profile_idc: None,
                    })
                    .collect();
                sections.push(VideoSection {
                    mid: None,
                    payloads,
                });
            }
            continue;
        }
        let Some(section) = sections.last_mut().filter(|_| in_video) else {
            continue;
        };
        if let Some(mid) = line.strip_prefix("a=mid:") {
            section.mid = Some(Mid::from(mid.trim()));
        } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            if let Some((p, value)) = payload_line(section, rest) {
                p.name = value.split('/').next().unwrap_or_default().to_string();
            }
        } else if let Some(rest) = line.strip_prefix("a=fmtp:")
            && let Some((p, value)) = payload_line(section, rest)
        {
            for param in value.split(';') {
                let Some((key, v)) = param.trim().split_once('=') else {
                    continue;
                };
                if key.eq_ignore_ascii_case("packetization-mode") {
                    p.packetization_mode = v.trim().parse().unwrap_or(0);
                } else if key.eq_ignore_ascii_case("profile-level-id") {
                    p.profile_idc = v
                        .trim()
                        .get(..2)
                        .and_then(|h| u8::from_str_radix(h, 16).ok());
                }
            }
        }
    }
    sections
}

/// Splits `<pt> <value>` and finds that payload in the section.
fn payload_line<'a, 'b>(
    section: &'a mut VideoSection,
    rest: &'b str,
) -> Option<(&'a mut OfferedPayload, &'b str)> {
    let (pt, value) = rest.split_once(' ')?;
    let pt: u8 = pt.parse().ok()?;
    let p = section.payloads.iter_mut().find(|p| *p.pt == pt)?;
    Some((p, value.trim()))
}

/// The `profile-level-id` bytes of an H.264 SPS NAL unit (header byte
/// included, no start code): `[profile_idc, constraint_set_flags,
/// level_idc]`. `None` for H.265, whose payloads str0m matches itself, and
/// for a NAL unit that is not a parseable SPS.
pub fn profile_level_id_from_sps(codec: VideoCodec, sps_nal: &[u8]) -> Option<[u8; 3]> {
    if codec != VideoCodec::H264 {
        return None;
    }
    let (&header, rbsp) = sps_nal.split_first()?;
    if header & 0x1F != 7 {
        return None;
    }
    let sps = tst_core::codec::h264::parse_sps(rbsp).ok()?;
    Some([sps.profile_idc, sps.constraint_set_flags, sps.level_idc])
}
