//! `RtspClient::play` + `RtspClient::pause` + `RTP-Info:` header parsing.

use crate::error::RtspError;
use crate::rtsp::client::RtspClient;
use crate::rtsp::message::RtspMethod;

/// `RTP-Info:` header from a PLAY response. Tells us the first RTP
/// sequence number and RTP timestamp the server will emit, which
/// callers can use to align demuxer state.
#[derive(Debug, Clone, Default)]
pub struct RtpInfo {
    pub url: Option<String>,
    pub seq: Option<u16>,
    pub rtptime: Option<u32>,
}

impl RtspClient {
    /// Send PLAY. Returns parsed `RTP-Info:` header (if present) so
    /// caller can align demuxer.
    ///
    /// # Call order
    ///
    /// Issue PLAY after SETUP (`setup_h264_auto` / `setup_mp2t_auto`) and
    /// before consuming the returned `RtspSession` — via
    /// [`RtspSession::into_h264_receiver`](crate::RtspSession::into_h264_receiver)
    /// on the H.264 path, or
    /// [`RtspSession::into_recv_transport`](crate::RtspSession::into_recv_transport)
    /// on the MPEG-TS-over-RTP path — the server does not start streaming
    /// until it sees PLAY, so converting the session first just leaves
    /// the receiver blocked waiting for data that only starts arriving
    /// once this call runs.
    ///
    /// # Errors
    ///
    /// - `RtspError::BadResponse` with `detail: "PLAY before SETUP"` if
    ///   the client has no active session id.
    /// - `RtspError::Io` on socket-level failure.
    /// - `RtspError::Protocol` on non-200 server status.
    pub fn play(&mut self) -> Result<RtpInfo, RtspError> {
        let sid = self
            .session_id
            .as_ref()
            .ok_or(RtspError::BadResponse {
                detail: "PLAY before SETUP",
            })?
            .clone();
        let uri = self.url.render_no_credentials();
        let resp = self.send_authenticated(
            RtspMethod::Play,
            &uri,
            &[("session", sid), ("range", "npt=0.000-".to_string())],
        )?;
        self.expect_ok(&resp)?;
        Ok(parse_rtp_info(
            resp.headers
                .get("rtp-info")
                .map(|s| s.as_str())
                .unwrap_or(""),
        ))
    }

    /// Send PAUSE. Stream stops but session remains valid for a
    /// subsequent PLAY.
    ///
    /// # Errors
    ///
    /// - `RtspError::BadResponse` with `detail: "PAUSE before SETUP"`
    ///   if the client has no active session id.
    /// - `RtspError::Io` on socket-level failure.
    /// - `RtspError::Protocol` on non-200 server status.
    pub fn pause(&mut self) -> Result<(), RtspError> {
        let sid = self
            .session_id
            .as_ref()
            .ok_or(RtspError::BadResponse {
                detail: "PAUSE before SETUP",
            })?
            .clone();
        let uri = self.url.render_no_credentials();
        let resp = self.send_authenticated(RtspMethod::Pause, &uri, &[("session", sid)])?;
        self.expect_ok(&resp)
    }
}

/// Parse an `RTP-Info:` header value like
/// `url=rtsp://cam/h264/streamid=0;seq=1234;rtptime=5000000`.
pub(crate) fn parse_rtp_info(s: &str) -> RtpInfo {
    let mut out = RtpInfo::default();
    for part in s.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("url=") {
            out.url = Some(v.to_string());
        } else if let Some(v) = part.strip_prefix("seq=") {
            out.seq = v.parse().ok();
        } else if let Some(v) = part.strip_prefix("rtptime=") {
            out.rtptime = v.parse().ok();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rtp_info_full() {
        let s = "url=rtsp://cam/h264/streamid=0;seq=1234;rtptime=5000000";
        let info = parse_rtp_info(s);
        assert_eq!(info.seq, Some(1234));
        assert_eq!(info.rtptime, Some(5000000));
        assert_eq!(info.url.as_deref(), Some("rtsp://cam/h264/streamid=0"));
    }

    #[test]
    fn parse_rtp_info_seq_only() {
        let info = parse_rtp_info("seq=42");
        assert_eq!(info.seq, Some(42));
        assert!(info.url.is_none());
        assert!(info.rtptime.is_none());
    }

    #[test]
    fn parse_rtp_info_empty() {
        let info = parse_rtp_info("");
        assert_eq!(info.seq, None);
    }
}
