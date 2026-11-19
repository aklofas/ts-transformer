//! The SDP an RTSP client announces. Rendered from typed values only: no
//! caller string reaches the text.

use std::net::IpAddr;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use tst_core::codec::annexb::nals;

pub(crate) const PT_MP2T: u8 = 33;
pub(crate) const PT_VIDEO: u8 = 96;
pub(crate) const PT_KLV: u8 = 97;

/// The video of an elementary announce, with its parameter sets (NAL units
/// without start codes). For H.264 a NAL that is neither an SPS nor a PPS is
/// ignored, and `profile-level-id` comes from the first SPS.
pub(crate) enum SdpVideo<'a> {
    H264 {
        parameter_sets: &'a [Vec<u8>],
    },
    H265 {
        vps: &'a [u8],
        sps: &'a [u8],
        pps: &'a [u8],
    },
}

pub(crate) enum SdpTracks<'a> {
    Mp2t,
    Elementary { video: SdpVideo<'a>, klv: bool },
}

/// CRLF line ends; `origin` is the control socket's local address and
/// `session_id` a random number.
pub(crate) fn write_announce(
    tracks: &SdpTracks<'_>,
    origin: IpAddr,
    session_id: u32,
) -> Result<String, &'static str> {
    let (family, any) = match origin {
        IpAddr::V4(_) => ("IP4", "0.0.0.0"),
        IpAddr::V6(_) => ("IP6", "::"),
    };
    let mut s = String::new();
    let mut line = |l: &str| {
        s.push_str(l);
        s.push_str("\r\n");
    };
    line("v=0");
    line(&format!("o=- {session_id} 1 IN {family} {origin}"));
    line("s=tstrans");
    line(&format!("c=IN {family} {any}"));
    line("t=0 0");
    line(&format!("a=tool:tstrans {}", env!("CARGO_PKG_VERSION")));
    match tracks {
        SdpTracks::Mp2t => {
            line(&format!("m=video 0 RTP/AVP {PT_MP2T}"));
            line(&format!("a=rtpmap:{PT_MP2T} MP2T/90000"));
            line("a=control:streamid=0");
        }
        SdpTracks::Elementary { video, klv } => {
            line(&format!("m=video 0 RTP/AVP {PT_VIDEO}"));
            match video {
                SdpVideo::H264 { parameter_sets } => {
                    let kind = |p: &Vec<u8>| p.first().map(|b| b & 0x1F);
                    let sps = parameter_sets
                        .iter()
                        .find(|p| kind(p) == Some(7))
                        .ok_or("an H.264 description needs an SPS")?;
                    if !parameter_sets.iter().any(|p| kind(p) == Some(8)) {
                        return Err("an H.264 description needs a PPS");
                    }
                    if sps.len() < 4 {
                        return Err("an H.264 SPS is shorter than four bytes");
                    }
                    let sprop = parameter_sets
                        .iter()
                        .filter(|p| kind(p) == Some(7))
                        .chain(parameter_sets.iter().filter(|p| kind(p) == Some(8)))
                        .map(|p| STANDARD.encode(p))
                        .collect::<Vec<_>>()
                        .join(",");
                    line(&format!("a=rtpmap:{PT_VIDEO} H264/90000"));
                    line(&format!(
                        "a=fmtp:{PT_VIDEO} packetization-mode=1;profile-level-id={:02X}{:02X}{:02X};sprop-parameter-sets={sprop}",
                        sps[1], sps[2], sps[3]
                    ));
                }
                SdpVideo::H265 { vps, sps, pps } => {
                    if vps.is_empty() || sps.is_empty() || pps.is_empty() {
                        return Err("an H.265 description needs a VPS, an SPS and a PPS");
                    }
                    line(&format!("a=rtpmap:{PT_VIDEO} H265/90000"));
                    line(&format!(
                        "a=fmtp:{PT_VIDEO} sprop-vps={};sprop-sps={};sprop-pps={}",
                        STANDARD.encode(vps),
                        STANDARD.encode(sps),
                        STANDARD.encode(pps)
                    ));
                }
            }
            line("a=control:streamid=0");
            if *klv {
                line(&format!("m=application 0 RTP/AVP {PT_KLV}"));
                line(&format!("a=rtpmap:{PT_KLV} smpte336m/90000"));
                line("a=control:streamid=1");
            }
        }
    }
    Ok(s)
}

/// Every SPS then every PPS of an Annex B access unit; `None` without both.
pub(crate) fn h264_parameter_sets(annexb: &[u8]) -> Option<Vec<Vec<u8>>> {
    let pick = |ty: u8| {
        nals(annexb)
            .filter(move |n| !n.is_empty() && n[0] & 0x1F == ty)
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>()
    };
    let (mut sets, pps) = (pick(7), pick(8));
    if sets.is_empty() || pps.is_empty() {
        return None;
    }
    sets.extend(pps);
    Some(sets)
}

/// The first VPS, SPS and PPS of an Annex B access unit.
pub(crate) fn h265_parameter_sets(annexb: &[u8]) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let first = |ty: u8| {
        nals(annexb)
            .find(|n| n.len() >= 2 && (n[0] >> 1) & 0x3F == ty)
            .map(<[u8]>::to_vec)
    };
    Some((first(32)?, first(33)?, first(34)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdp::Sdp;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const V4: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    const SPS: [u8; 5] = [0x67, 0x42, 0xC0, 0x1F, 0xAA];
    const PPS: [u8; 3] = [0x68, 0xCE, 0x38];

    fn h264() -> Vec<Vec<u8>> {
        vec![SPS.to_vec(), PPS.to_vec()]
    }

    #[test]
    fn mp2t_sdp_is_exactly_this_text() {
        let got = write_announce(&SdpTracks::Mp2t, V4, 7).unwrap();
        let want = format!(
            "v=0\r\no=- 7 1 IN IP4 127.0.0.1\r\ns=tstrans\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
             a=tool:tstrans {}\r\nm=video 0 RTP/AVP 33\r\na=rtpmap:33 MP2T/90000\r\n\
             a=control:streamid=0\r\n",
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(got, want);
    }

    #[test]
    fn h264_sdp_carries_profile_level_id_and_padded_sprop() {
        let sets = h264();
        let t = SdpTracks::Elementary {
            video: SdpVideo::H264 {
                parameter_sets: &sets,
            },
            klv: false,
        };
        let got = write_announce(&t, V4, 7).unwrap();
        let sprop = format!("{},{}", STANDARD.encode(SPS), STANDARD.encode(PPS));
        assert!(sprop.contains('='), "the fixture must exercise padding");
        assert!(got.contains("a=rtpmap:96 H264/90000\r\n"));
        assert!(got.contains(&format!(
            "a=fmtp:96 packetization-mode=1;profile-level-id=42C01F;sprop-parameter-sets={sprop}\r\n"
        )));
    }

    #[test]
    fn sprop_lists_every_sps_then_every_pps_whatever_the_input_order() {
        let (sps2, pps2) = (vec![0x67, 0x42, 0xC0, 0x1F, 0xBB], vec![0x68, 0x01]);
        let sets = vec![PPS.to_vec(), sps2.clone(), pps2.clone(), SPS.to_vec()];
        let t = SdpTracks::Elementary {
            video: SdpVideo::H264 {
                parameter_sets: &sets,
            },
            klv: false,
        };
        let got = write_announce(&t, V4, 7).unwrap();
        let want: Vec<String> = [&sps2[..], &SPS[..], &PPS[..], &pps2[..]]
            .iter()
            .map(|p| STANDARD.encode(p))
            .collect();
        assert!(got.contains(&format!("sprop-parameter-sets={}\r\n", want.join(","))));
        let swapped = vec![PPS.to_vec(), SPS.to_vec()];
        let t = SdpTracks::Elementary {
            video: SdpVideo::H264 {
                parameter_sets: &swapped,
            },
            klv: false,
        };
        let got = write_announce(&t, V4, 7).unwrap();
        assert!(got.contains(&format!(
            "sprop-parameter-sets={},{}\r\n",
            STANDARD.encode(SPS),
            STANDARD.encode(PPS)
        )));
    }

    #[test]
    fn the_h264_klv_sdp_is_exactly_this_text() {
        let sets = h264();
        let t = SdpTracks::Elementary {
            video: SdpVideo::H264 {
                parameter_sets: &sets,
            },
            klv: true,
        };
        let want = format!(
            "v=0\r\no=- 7 1 IN IP4 127.0.0.1\r\ns=tstrans\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
             a=tool:tstrans {}\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n\
             a=fmtp:96 packetization-mode=1;profile-level-id=42C01F;sprop-parameter-sets={},{}\r\n\
             a=control:streamid=0\r\nm=application 0 RTP/AVP 97\r\n\
             a=rtpmap:97 smpte336m/90000\r\na=control:streamid=1\r\n",
            env!("CARGO_PKG_VERSION"),
            STANDARD.encode(SPS),
            STANDARD.encode(PPS)
        );
        assert_eq!(write_announce(&t, V4, 7).unwrap(), want);
    }

    #[test]
    fn the_h265_sdp_is_exactly_this_text() {
        let (vps, sps, pps) = ([0x40, 1, 2], [0x42, 1, 3, 4], [0x44, 1]);
        let t = SdpTracks::Elementary {
            video: SdpVideo::H265 {
                vps: &vps,
                sps: &sps,
                pps: &pps,
            },
            klv: false,
        };
        let want = format!(
            "v=0\r\no=- 7 1 IN IP4 127.0.0.1\r\ns=tstrans\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
             a=tool:tstrans {}\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H265/90000\r\n\
             a=fmtp:96 sprop-vps={};sprop-sps={};sprop-pps={}\r\na=control:streamid=0\r\n",
            env!("CARGO_PKG_VERSION"),
            STANDARD.encode(vps),
            STANDARD.encode(sps),
            STANDARD.encode(pps)
        );
        assert_eq!(write_announce(&t, V4, 7).unwrap(), want);
    }

    #[test]
    fn h265_sdp_carries_sprop_vps_sps_pps() {
        let (vps, sps, pps) = ([0x40, 1, 2], [0x42, 1, 3, 4], [0x44, 1]);
        let t = SdpTracks::Elementary {
            video: SdpVideo::H265 {
                vps: &vps,
                sps: &sps,
                pps: &pps,
            },
            klv: false,
        };
        let got = write_announce(&t, V4, 7).unwrap();
        assert!(got.contains("a=rtpmap:96 H265/90000\r\n"));
        assert!(got.contains(&format!(
            "a=fmtp:96 sprop-vps={};sprop-sps={};sprop-pps={}\r\n",
            STANDARD.encode(vps),
            STANDARD.encode(sps),
            STANDARD.encode(pps)
        )));
    }

    #[test]
    fn klv_is_the_second_media_on_m_application_pt_97_streamid_1() {
        let sets = h264();
        let t = SdpTracks::Elementary {
            video: SdpVideo::H264 {
                parameter_sets: &sets,
            },
            klv: true,
        };
        let got = write_announce(&t, V4, 7).unwrap();
        assert!(got.ends_with(
            "m=application 0 RTP/AVP 97\r\na=rtpmap:97 smpte336m/90000\r\na=control:streamid=1\r\n"
        ));
        assert!(got.find("m=video").unwrap() < got.find("m=application").unwrap());
    }

    #[test]
    fn an_ipv6_origin_writes_ip6_lines() {
        let got = write_announce(&SdpTracks::Mp2t, IpAddr::V6(Ipv6Addr::LOCALHOST), 7).unwrap();
        assert!(got.contains("\r\no=- 7 1 IN IP6 ::1\r\n"));
        assert!(got.contains("\r\nc=IN IP6 ::\r\n"));
    }

    fn all_shapes<'a>(
        sets: &'a [Vec<u8>],
        h265: &'a ([u8; 3], [u8; 4], [u8; 2]),
    ) -> Vec<SdpTracks<'a>> {
        vec![
            SdpTracks::Mp2t,
            SdpTracks::Elementary {
                video: SdpVideo::H264 {
                    parameter_sets: sets,
                },
                klv: false,
            },
            SdpTracks::Elementary {
                video: SdpVideo::H264 {
                    parameter_sets: sets,
                },
                klv: true,
            },
            SdpTracks::Elementary {
                video: SdpVideo::H265 {
                    vps: &h265.0,
                    sps: &h265.1,
                    pps: &h265.2,
                },
                klv: false,
            },
        ]
    }

    #[test]
    fn every_generated_sdp_parses_back() {
        let sets = h264();
        let h265 = ([0x40, 1, 2], [0x42, 1, 3, 4], [0x44, 1]);
        let want: [&[u8]; 4] = [&[33], &[96], &[96, 97], &[96]];
        for origin in [V4, IpAddr::V6(Ipv6Addr::LOCALHOST)] {
            for (t, pts) in all_shapes(&sets, &h265).iter().zip(want) {
                let text = write_announce(t, origin, 7).unwrap();
                let sdp = Sdp::parse(text.as_bytes()).expect("parses");
                let got: Vec<u8> = sdp.media.iter().map(|m| m.payload_types[0]).collect();
                assert_eq!(got, pts);
                assert!(sdp.media.iter().all(|m| m.control.is_some()));
            }
        }
    }

    #[cfg(feature = "rtsp-server")]
    #[test]
    fn the_server_classifies_each_generated_sdp_as_intended() {
        use crate::rtsp::server::publish::PublishShape;
        use crate::rtsp::server::publish::shape::{ShapeReject, classify_announce};
        let sets = h264();
        let h265 = ([0x40, 1, 2], [0x42, 1, 3, 4], [0x44, 1]);
        let shapes = all_shapes(&sets, &h265);
        let classify = |t: &SdpTracks<'_>| {
            let text = write_announce(t, V4, 7).unwrap();
            classify_announce(&Sdp::parse(text.as_bytes()).unwrap())
        };
        assert_eq!(classify(&shapes[0]).unwrap().shape, PublishShape::Mp2t);
        assert_eq!(
            classify(&shapes[1]).unwrap().shape,
            PublishShape::Elementary { klv: false }
        );
        assert_eq!(
            classify(&shapes[2]).unwrap().shape,
            PublishShape::Elementary { klv: true }
        );
        assert!(matches!(
            classify(&shapes[3]),
            Err(ShapeReject::Unsupported(_))
        ));
    }

    fn refused(sets: &[Vec<u8>]) -> bool {
        let t = SdpTracks::Elementary {
            video: SdpVideo::H264 {
                parameter_sets: sets,
            },
            klv: false,
        };
        write_announce(&t, V4, 7).is_err()
    }

    #[test]
    fn an_h264_description_without_an_sps_or_a_pps_is_refused() {
        assert!(refused(&[]));
        assert!(refused(&[SPS.to_vec()]));
        assert!(refused(&[PPS.to_vec()]));
    }

    #[test]
    fn an_sps_shorter_than_four_bytes_is_refused() {
        assert!(refused(&[vec![0x67, 0x42, 0xC0], PPS.to_vec()]));
    }

    #[test]
    fn an_empty_h265_parameter_set_is_refused() {
        for (v, s, p) in [
            (&[][..], &[0x42, 1][..], &[0x44, 1][..]),
            (&[0x40, 1][..], &[][..], &[0x44, 1][..]),
            (&[0x40, 1][..], &[0x42, 1][..], &[][..]),
        ] {
            let t = SdpTracks::Elementary {
                video: SdpVideo::H265 {
                    vps: v,
                    sps: s,
                    pps: p,
                },
                klv: false,
            };
            assert!(write_announce(&t, V4, 7).is_err());
        }
    }

    fn annexb(units: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for u in units {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(u);
        }
        out
    }

    #[test]
    fn h264_parameter_sets_finds_sps_and_pps_in_a_key_frame() {
        let idr = [0x65, 0x88, 0x84];
        let buf = annexb(&[&SPS, &PPS, &idr]);
        assert_eq!(h264_parameter_sets(&buf), Some(h264()));
    }

    #[test]
    fn h264_parameter_sets_returns_none_for_a_frame_without_them() {
        assert_eq!(h264_parameter_sets(&annexb(&[&[0x41, 1, 2]])), None);
        assert_eq!(h264_parameter_sets(&annexb(&[&SPS])), None);
    }

    #[test]
    fn h265_parameter_sets_finds_vps_sps_pps_in_a_key_frame() {
        let (vps, sps, pps) = ([0x40, 1, 2], [0x42, 1, 3, 4], [0x44, 1]);
        let buf = annexb(&[&vps, &sps, &pps, &[0x26, 1, 9]]);
        assert_eq!(
            h265_parameter_sets(&buf),
            Some((vps.to_vec(), sps.to_vec(), pps.to_vec()))
        );
    }

    #[test]
    fn h265_parameter_sets_returns_none_for_a_frame_without_them() {
        assert_eq!(h265_parameter_sets(&annexb(&[&[0x26, 1, 9]])), None);
        assert_eq!(
            h265_parameter_sets(&annexb(&[&[0x40, 1], &[0x42, 1]])),
            None
        );
    }
}
