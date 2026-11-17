use super::*;
use crate::packet::RtpHeader;
use crate::rtsp::framing::parse_binary_frame_header;
use crate::rtsp::message::{
    MAX_RTSP_MESSAGE_BYTES, RtspFraming, content_length_from_header_text, rtsp_frame_decision,
};

/// What is known of the incomplete RTSP message at the front of the input
/// buffer, so a header or body dribbled in is not searched again per read.
#[derive(Debug, Clone, Copy)]
pub(super) struct FrontScan {
    /// Buffered bytes of it already searched for a header terminator.
    scanned: usize,
    /// Header block, terminator and body, once the header block is in.
    total_len: Option<usize>,
}

impl FrontScan {
    /// `Some` when `rest` (the message at the front, `known` its previous
    /// scan) is still incomplete in a way the shared decision would answer
    /// `NeedMore` to; `None` when the decision must be asked.
    fn still_waiting(rest: &[u8], known: Option<FrontScan>) -> Option<FrontScan> {
        if let Some(FrontScan {
            total_len: Some(total_len),
            ..
        }) = known
        {
            return (rest.len() < total_len).then_some(FrontScan {
                scanned: rest.len(),
                total_len: Some(total_len),
            });
        }
        // A terminator can end in the new bytes and start up to three
        // bytes before them.
        let from = known.map_or(0, |k| k.scanned.saturating_sub(3));
        let terminated = rest
            .get(from..)
            .unwrap_or_default()
            .windows(4)
            .any(|w| w == b"\r\n\r\n");
        // Past the cap the decision refuses an unterminated buffer.
        (!terminated && rest.len() <= MAX_RTSP_MESSAGE_BYTES).then_some(FrontScan {
            scanned: rest.len(),
            total_len: None,
        })
    }

    /// The scan of `rest`, a message whose header block is in and whose
    /// body the decision is waiting for; `None` if its length cannot be
    /// read (then the decision is asked at every read).
    fn awaiting_body(rest: &[u8]) -> Option<FrontScan> {
        let header_end = rest.windows(4).position(|w| w == b"\r\n\r\n")?;
        let header_text = std::str::from_utf8(rest.get(..header_end)?).ok()?;
        let content_length = content_length_from_header_text(header_text).ok()?;
        let total_len = header_end.checked_add(4)?.checked_add(content_length)?;
        Some(FrontScan {
            scanned: rest.len(),
            total_len: Some(total_len),
        })
    }
}

impl ClientEngine {
    /// Bytes read from the control stream: interleaved `$` frames and RTSP
    /// messages, split at any byte. Ignored once the engine is closed (by
    /// `handle_closed` or a framing violation).
    pub(crate) fn handle_input(&mut self, bytes: &[u8], now: Instant) {
        if self.closed.is_some() {
            return;
        }
        self.inbuf.extend_from_slice(bytes);
        // Frames are consumed by offset and the buffer drained once, so a
        // read holding many small frames costs one copy, not one per frame.
        let mut at = 0;
        // Describes the first frame only; whatever is consumed resets it.
        let mut carried = self.front.take();
        while self.closed.is_none() {
            let rest = self.inbuf.get(at..).unwrap_or_default();
            if rest.is_empty() {
                break;
            }
            let used = if rest.first() == Some(&b'$') {
                let Some((channel, total_len)) = parse_binary_frame_header(rest) else {
                    break;
                };
                let payload = Bytes::copy_from_slice(rest.get(4..total_len).unwrap_or_default());
                self.route_frame(channel, payload);
                total_len
            } else {
                if let Some(scan) = FrontScan::still_waiting(rest, carried.take()) {
                    self.front = Some(scan);
                    break;
                }
                self.counters.frame_decisions += 1;
                match rtsp_frame_decision(rest) {
                    RtspFraming::NeedMore => {
                        self.front = FrontScan::awaiting_body(rest);
                        break;
                    }
                    RtspFraming::Complete { total_len } => {
                        let msg = rest.get(..total_len).unwrap_or_default();
                        let response_like = msg.starts_with(b"RTSP/");
                        let parsed = RtspResponse::parse(msg).map(|(resp, _)| resp);
                        self.route_message(parsed, response_like, now);
                        total_len
                    }
                    RtspFraming::NonUtf8Headers { skip } => {
                        self.counters.malformed_frames += 1;
                        if let Some(t) = self.caller.take() {
                            self.reply(
                                t.id,
                                Err(RtspError::BadResponse {
                                    detail: "non-UTF8 RTSP headers",
                                }),
                            );
                        }
                        skip
                    }
                    RtspFraming::HeadersTooLong => {
                        let msg = format!(
                            "control stream buffer exceeded RTSP header/body caps ({} bytes buffered)",
                            rest.len()
                        );
                        self.framing_violation("RTSP response headers exceed maximum", msg);
                        break;
                    }
                    RtspFraming::BadContentLength(detail) => {
                        let msg = format!("malformed RTSP Content-Length: {detail}");
                        self.framing_violation(detail, msg);
                        break;
                    }
                    RtspFraming::BodyOffsetOverflow => {
                        let detail = "RTSP body offset overflow";
                        self.framing_violation(detail, detail.to_string());
                        break;
                    }
                }
            };
            carried = None;
            at += used;
        }
        if self.closed.is_none() {
            self.inbuf.drain(..at.min(self.inbuf.len()));
        }
    }

    /// The interleaved channels SETUP agreed; until called, every `$` frame
    /// is counted as malformed and dropped.
    pub(crate) fn set_interleaved(&mut self, channels: InterleavedChannels) {
        self.channels = Some(channels);
    }

    /// The control stream broke a framing rule it cannot be resynced past.
    fn framing_violation(&mut self, detail: &'static str, msg: String) {
        tracing::warn!(target: "tst_rtp::client::pump", %msg, "closing the control stream");
        self.counters.malformed_frames += 1;
        self.end_stream(
            io::ErrorKind::InvalidData,
            EndReason::ProtocolError { msg },
            true,
            RtspError::BadResponse { detail },
        );
    }

    /// One `$` frame: whole RTP packets on the RTP channel (their header is
    /// checked, not stripped), payloads on the RTCP channel.
    fn route_frame(&mut self, channel: u8, payload: Bytes) {
        match self.channels {
            Some(c) if channel == c.rtp => {
                self.counters.rtp_frames += 1;
                match RtpHeader::decode(&payload) {
                    Ok(_) => self.events.push_back(Event::Media(payload)),
                    Err(e) => {
                        self.counters.malformed_frames += 1;
                        tracing::debug!(
                            target: "tst_rtp::client::pump",
                            error = ?e,
                            "interleaved RTP frame rejected; counter ticked",
                        );
                    }
                }
            }
            Some(c) if channel == c.rtcp => {
                self.counters.rtcp_frames += 1;
                self.events.push_back(Event::Rtcp(payload));
            }
            _ => {
                self.counters.malformed_frames += 1;
                tracing::debug!(
                    target: "tst_rtp::client::pump",
                    channel,
                    "frame on unknown channel; dropping"
                );
            }
        }
    }

    /// One complete RTSP message. A response that does not parse fails the
    /// waiting request with the parse error; a message that is not a
    /// response (a server request) answers nothing.
    fn route_message(
        &mut self,
        parsed: Result<RtspResponse, RtspError>,
        response_like: bool,
        now: Instant,
    ) {
        let resp = match parsed {
            Ok(resp) => resp,
            Err(e) => {
                self.counters.rtsp_messages += 1;
                match self.caller.take() {
                    Some(t) if response_like => self.reply(t.id, Err(e)),
                    other => {
                        self.caller = other;
                        self.counters.unmatched_replies += 1;
                    }
                }
                return;
            }
        };
        if !resp.cseq().is_some_and(|c| self.pings.contains(&c)) {
            self.counters.rtsp_messages += 1;
        }
        self.handle_reply(resp, now);
    }
}
