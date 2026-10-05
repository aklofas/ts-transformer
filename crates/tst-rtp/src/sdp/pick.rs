//! MP2T and H.264 media selection helpers.
//!
//! [`pick_mp2t`] finds the unique PT=33 m-line for `RtspClient::setup_mp2t_auto`.
//! [`pick_h264`] finds the unique H.264 m-line (identified by `a=rtpmap H264/90000`)
//! for `RtspClient::setup_h264_auto`.

use crate::error::RtspError;
use crate::h264::fmtp::{H264FmtpParams, parse_rtpmap_h264};
use crate::sdp::{Sdp, SdpMedia};

/// Find the unique `m=` line whose `payload_types` contains `33` (MP2T,
/// per RFC 3551 §6).
///
/// # Errors
///
/// - [`RtspError::NoMp2tMedia`] if no m-line has PT=33.
/// - [`RtspError::MultipleMp2tMedia`] if more than one m-line has PT=33.
pub fn pick_mp2t(sdp: &Sdp) -> Result<&SdpMedia, RtspError> {
    let mut found: Option<&SdpMedia> = None;
    let mut count = 0;
    for m in &sdp.media {
        if m.payload_types.contains(&33) {
            count += 1;
            if count == 1 {
                found = Some(m);
            }
        }
    }
    match count {
        0 => Err(RtspError::NoMp2tMedia),
        1 => Ok(found.unwrap()),
        _ => Err(RtspError::MultipleMp2tMedia { count }),
    }
}

/// Result of [`pick_h264`]: the unique H.264 m-line together with the
/// negotiated payload type and parsed fmtp parameters.
///
/// Borrowed from the source [`Sdp`] — the lifetime `'a` ties `media` back
/// to the SDP document returned by `DESCRIBE`. Not `#[non_exhaustive]`:
/// this is an exhaustive view struct; all three fields are always populated
/// by `pick_h264`.
#[derive(Debug)]
pub struct H264Media<'a> {
    /// The `m=` line chosen as the unique H.264 stream.
    pub media: &'a SdpMedia,
    /// Dynamic RTP payload type from `a=rtpmap H264/<clock>` (typically 96).
    pub payload_type: u8,
    /// Parsed `a=fmtp` parameters (mode, sprop-parameter-sets, profile-level-id).
    /// Absent or malformed fmtp yields defaults per [`H264FmtpParams::parse`].
    pub fmtp: H264FmtpParams,
}

/// Find the unique `m=` line whose `a=rtpmap` names `H264` (case-insensitive)
/// and whose payload type is listed in `m=`'s payload type set.
///
/// # Errors
///
/// - [`RtspError::NoH264Media`] — no m-line has an H.264 rtpmap entry.
/// - [`RtspError::MultipleH264Media`] — more than one m-line matches.
pub fn pick_h264(sdp: &Sdp) -> Result<H264Media<'_>, RtspError> {
    let mut found: Option<(&SdpMedia, u8)> = None;
    let mut count = 0usize;
    for m in &sdp.media {
        if let Some(pt) = parse_rtpmap_h264(m) {
            count += 1;
            if count == 1 {
                found = Some((m, pt));
            }
        }
    }
    match count {
        0 => Err(RtspError::NoH264Media),
        1 => {
            let (media, payload_type) = found.unwrap();
            let fmtp = H264FmtpParams::parse(media, payload_type);
            Ok(H264Media {
                media,
                payload_type,
                fmtp,
            })
        }
        _ => Err(RtspError::MultipleH264Media { count }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_media(media: &str, pts: &[u8]) -> SdpMedia {
        SdpMedia {
            media: media.to_string(),
            port: 0,
            protocol: "RTP/AVP".to_string(),
            payload_types: pts.to_vec(),
            connection: None,
            attributes: Vec::new(),
            control: None,
        }
    }

    /// Backward compat: servers that still emit `m=application` (old shape)
    /// are accepted; pick_mp2t selects by PT=33, not by media type name.
    #[test]
    fn picks_unique_mp2t_application_shape() {
        let sdp = Sdp {
            media: vec![
                make_media("video", &[96]),
                make_media("application", &[33]),
                make_media("audio", &[97]),
            ],
            session_connection: None,
            session_name: String::new(),
        };
        let m = pick_mp2t(&sdp).unwrap();
        assert_eq!(m.media, "application");
    }

    /// RFC 2250 §2 shape: `m=video` with PT=33 (what the tst-rtp server
    /// emits). pick_mp2t must accept this too.
    #[test]
    fn picks_unique_mp2t_video_shape() {
        let sdp = Sdp {
            media: vec![make_media("video", &[33]), make_media("audio", &[97])],
            session_connection: None,
            session_name: String::new(),
        };
        let m = pick_mp2t(&sdp).unwrap();
        assert_eq!(m.media, "video");
        assert_eq!(m.payload_types, vec![33]);
    }

    #[test]
    fn rejects_no_mp2t() {
        let sdp = Sdp {
            media: vec![make_media("video", &[96])],
            session_connection: None,
            session_name: String::new(),
        };
        assert!(matches!(
            pick_mp2t(&sdp).unwrap_err(),
            RtspError::NoMp2tMedia
        ));
    }

    #[test]
    fn rejects_multiple_mp2t() {
        let sdp = Sdp {
            media: vec![
                make_media("application", &[33]),
                make_media("application", &[33]),
            ],
            session_connection: None,
            session_name: String::new(),
        };
        let e = pick_mp2t(&sdp).unwrap_err();
        assert!(matches!(e, RtspError::MultipleMp2tMedia { count: 2 }));
    }

    // --- pick_h264 tests -------------------------------------------------------

    fn make_h264_media(pts: &[u8]) -> SdpMedia {
        SdpMedia {
            media: "video".to_string(),
            port: 0,
            protocol: "RTP/AVP".to_string(),
            payload_types: pts.to_vec(),
            connection: None,
            attributes: vec![("rtpmap".to_string(), Some(format!("{} H264/90000", pts[0])))],
            control: None,
        }
    }

    /// Unique H.264 video m-line is selected; payload_type and fmtp are
    /// populated (fmtp defaults when no a=fmtp line is present).
    #[test]
    fn picks_unique_h264_video_shape() {
        let sdp = Sdp {
            media: vec![make_h264_media(&[96]), make_media("audio", &[97])],
            session_connection: None,
            session_name: String::new(),
        };
        let h = pick_h264(&sdp).unwrap();
        assert_eq!(h.media.media, "video");
        assert_eq!(h.payload_type, 96);
        // No a=fmtp → defaults.
        assert_eq!(h.fmtp.packetization_mode, 0);
        assert!(h.fmtp.sprop_parameter_sets.is_empty());
    }

    /// PT listed in `m=` but without a matching `a=rtpmap H264` line →
    /// `NoH264Media` (mirrors pick_mp2t's "PT listed but no rtpmap" case).
    #[test]
    fn rejects_no_h264_rtpmap() {
        // m=video with PT=96 but no rtpmap at all.
        let sdp = Sdp {
            media: vec![make_media("video", &[96])],
            session_connection: None,
            session_name: String::new(),
        };
        assert!(matches!(
            pick_h264(&sdp).unwrap_err(),
            RtspError::NoH264Media
        ));
    }

    /// No H.264 m-line at all → `NoH264Media`.
    #[test]
    fn rejects_empty_sdp() {
        let sdp = Sdp {
            media: vec![],
            session_connection: None,
            session_name: String::new(),
        };
        assert!(matches!(
            pick_h264(&sdp).unwrap_err(),
            RtspError::NoH264Media
        ));
    }

    /// Two H.264 m-lines → `MultipleH264Media { count: 2 }`.
    #[test]
    fn rejects_multiple_h264() {
        let sdp = Sdp {
            media: vec![make_h264_media(&[96]), make_h264_media(&[97])],
            session_connection: None,
            session_name: String::new(),
        };
        let e = pick_h264(&sdp).unwrap_err();
        assert!(matches!(e, RtspError::MultipleH264Media { count: 2 }));
    }
}
