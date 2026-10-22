//! Payload selection after str0m has accepted a browser's offer.
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

use std::collections::{BTreeSet, HashMap};

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
/// both sides must also say `packetization-mode=1` with the same profile
/// idc (the first byte of `profile-level-id`, Baseline when the offer
/// names none); the first such payload
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
        VideoCodec::H266 | VideoCodec::Av1 => {
            return Negotiated::Rejected {
                reason: "the mount's codec is not carried over WebRTC",
            };
        }
    };
    let mut saw_video = false;
    for section in media_sections(offer, "video") {
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
                        && (codec != Codec::H264
                            || (c.spec().format.packetization_mode == Some(1)
                                && profile_idc_of(c.spec().format.profile_level_id)
                                    == Some(p.profile_idc.unwrap_or(BASELINE_PROFILE_IDC))))
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

/// Whether every payload type in `offer` (SDP text) keeps one format.
///
/// str0m 0.24 panics on an offer that dictates its payload types
/// (`recvonly` or `sendrecv`) when two formats claim one payload number:
/// an `rtx` payload whose `apt` names two formats, one number mapped to
/// different formats in two media sections (a BUNDLE shares one
/// payload-type space, RFC 8843 §9.1), or one number given two `a=rtpmap`
/// lines in a section. str0m checks only the numbers in a section's `m=`
/// line, but reads every `a=rtpmap`, so a section is also refused when it
/// repeats an `a=rtpmap` or `a=fmtp` for one number, or carries either
/// line for a number its `m=` line does not list. Call this before
/// `accept_offer` and refuse the offer when it returns `false`. Pass
/// str0m's rendering of the parsed offer (`SdpOffer::to_sdp_string`),
/// which writes each attribute it parsed as one canonical line.
pub fn payload_types_are_consistent(offer: &str) -> bool {
    // Payload number -> (rtpmap value, fmtp parameters), offer-wide.
    let mut offer_formats: HashMap<u8, Format> = HashMap::new();
    let mut section: HashMap<u8, Format> = HashMap::new();
    // The current section's `m=` payload numbers, and the numbers it has
    // given an `a=rtpmap` and an `a=fmtp`.
    let mut listed: BTreeSet<u8> = BTreeSet::new();
    let mut mapped: BTreeSet<u8> = BTreeSet::new();
    let mut with_fmtp: BTreeSet<u8> = BTreeSet::new();
    for line in offer.lines().chain(["m="]) {
        let line = line.trim_end();
        if let Some(media) = line.strip_prefix("m=") {
            listed = media
                .split(' ')
                .skip(3)
                .filter_map(|pt| pt.parse::<u8>().ok())
                .collect();
            mapped.clear();
            with_fmtp.clear();
            for (pt, format) in section.drain() {
                let is_rtx = format.0.starts_with("rtx/");
                if is_rtx && format.1.iter().filter(|p| p.starts_with("apt=")).count() > 1 {
                    return false;
                }
                // Only a mapped payload has a format to compare.
                if format.0.is_empty() {
                    continue;
                }
                if offer_formats.get(&pt).is_some_and(|seen| *seen != format) {
                    return false;
                }
                offer_formats.insert(pt, format);
            }
            continue;
        }
        let (rest, is_rtpmap) = if let Some(r) = line.strip_prefix("a=rtpmap:") {
            (r, true)
        } else if let Some(r) = line.strip_prefix("a=fmtp:") {
            (r, false)
        } else {
            continue;
        };
        let Some((pt, value)) = rest.split_once(' ') else {
            continue;
        };
        let Ok(pt) = pt.parse::<u8>() else {
            continue;
        };
        let first = if is_rtpmap {
            mapped.insert(pt)
        } else {
            with_fmtp.insert(pt)
        };
        if !first || !listed.contains(&pt) {
            return false;
        }
        let format = section.entry(pt).or_default();
        if is_rtpmap {
            format.0 = value.trim().to_ascii_lowercase();
        } else {
            format.1.extend(
                value
                    .split(';')
                    .map(|p| p.trim().to_ascii_lowercase())
                    .filter(|p| !p.is_empty()),
            );
        }
    }
    true
}

/// A payload type's `a=rtpmap` value and its `a=fmtp` parameters.
type Format = (String, BTreeSet<String>);

/// The H.264 profile idc of a payload whose `a=fmtp` names no
/// `profile-level-id`: Baseline (RFC 6184 §8.1).
const BASELINE_PROFILE_IDC: u8 = 0x42;

/// The profile idc (first byte) of a str0m `profile-level-id` value.
fn profile_idc_of(profile_level_id: Option<u32>) -> Option<u8> {
    profile_level_id.map(|v| (v >> 16) as u8)
}

/// One payload type declared in an offer's media section.
struct OfferedPayload {
    pt: Pt,
    /// The encoding name from `a=rtpmap`.
    name: String,
    /// The clock rate from `a=rtpmap`.
    clock_rate: Option<u32>,
    /// `packetization-mode` from `a=fmtp` (RFC 6184 default 0).
    packetization_mode: u8,
    /// The first byte of `profile-level-id` from `a=fmtp`.
    profile_idc: Option<u8>,
}

/// An offer's `m=` section: its mid and payload types, in `m=` order.
struct MediaSection {
    mid: Option<Mid>,
    payloads: Vec<OfferedPayload>,
}

/// The `m=<kind>` sections of an SDP.
fn media_sections(sdp: &str, kind: &str) -> Vec<MediaSection> {
    let prefix = format!("{kind} ");
    let mut sections: Vec<MediaSection> = Vec::new();
    let mut in_section = false;
    for line in sdp.lines() {
        let line = line.trim_end();
        if let Some(media) = line.strip_prefix("m=") {
            in_section = media.starts_with(&prefix);
            if in_section {
                let payloads = media
                    .split(' ')
                    .skip(3)
                    .filter_map(|pt| pt.parse::<u8>().ok())
                    .filter(|pt| *pt < 128)
                    .map(|pt| OfferedPayload {
                        pt: Pt::from(pt),
                        name: String::new(),
                        clock_rate: None,
                        packetization_mode: 0,
                        profile_idc: None,
                    })
                    .collect();
                sections.push(MediaSection {
                    mid: None,
                    payloads,
                });
            }
            continue;
        }
        let Some(section) = sections.last_mut().filter(|_| in_section) else {
            continue;
        };
        if let Some(mid) = line.strip_prefix("a=mid:") {
            section.mid = Some(Mid::from(mid.trim()));
        } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            if let Some((p, value)) = payload_line(section, rest) {
                let mut parts = value.split('/');
                p.name = parts.next().unwrap_or_default().to_string();
                p.clock_rate = parts.next().and_then(|r| r.trim().parse().ok());
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
    section: &'a mut MediaSection,
    rest: &'b str,
) -> Option<(&'a mut OfferedPayload, &'b str)> {
    let (pt, value) = rest.split_once(' ')?;
    let pt: u8 = pt.parse().ok()?;
    let p = section.payloads.iter_mut().find(|p| *p.pt == pt)?;
    Some((p, value.trim()))
}

/// Finds the Opus payload type the browser offered, when there is one.
///
/// Call after `rtc` has accepted `offer` (the SDP text). The first audio
/// section we send on (the viewer offered `recvonly` or `sendrecv`) with a
/// payload the offer declares as `opus/48000` (the RTP clock is 48 kHz
/// whatever the sampling rate, RFC 7587 §4.1), that str0m lists among the
/// media's remote payload types and whose number str0m's codec list holds
/// for Opus, qualifies; the first such payload in the offer's order wins.
/// `None` when the offer has no such section: the viewer is then
/// video-only, since audio never decides whether an offer is served.
pub fn select_audio(rtc: &Rtc, offer: &str) -> Option<(Mid, Pt)> {
    for section in media_sections(offer, "audio") {
        let Some(mid) = section.mid else {
            continue;
        };
        let Some(media) = rtc.media(mid) else {
            continue;
        };
        // str0m opens no send stream for a `sendonly` or `inactive` offer.
        if media.kind() != MediaKind::Audio || !media.direction().is_sending() {
            continue;
        }
        let qualifies = |p: &&OfferedPayload| {
            p.name.eq_ignore_ascii_case("opus")
                && p.clock_rate == Some(48_000)
                && media.remote_pts().contains(&p.pt)
                && rtc
                    .codec_config()
                    .params()
                    .iter()
                    .any(|c| c.pt() == p.pt && c.spec().codec == Codec::Opus)
        };
        if let Some(p) = section.payloads.iter().find(qualifies) {
            return Some((mid, p.pt));
        }
    }
    None
}

/// Gives a rejected `m=` line of `answer` the format list RFC 8866 §5.14
/// requires.
///
/// str0m rejects an offered section it has no codec for with port 0 and an
/// empty format list (`m=audio 0 UDP/TLS/RTP/SAVPF `), which a browser's
/// SDP parser refuses. Only port-0 lines are touched. The line gets the static payload type `0`, the
/// placeholder RFC 3264 §6 allows on a rejected stream (the format is
/// never used, and a static number needs no `a=rtpmap`, so no stray codec
/// description is added). Every other line is returned as it was.
pub fn repair_rejected_media(answer: &str) -> String {
    let mut out = String::with_capacity(answer.len() + 8);
    for raw in answer.split_inclusive('\n') {
        let line = raw.trim_end_matches(['\r', '\n']);
        let ending = &raw[line.len()..];
        let fields: Vec<&str> = line.split(' ').filter(|f| !f.is_empty()).collect();
        if line.starts_with("m=") && fields.len() == 3 && fields[1] == "0" {
            out.push_str(&format!("{} 0{ending}", fields.join(" ")));
        } else {
            out.push_str(raw);
        }
    }
    out
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
