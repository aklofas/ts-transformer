//! A WebRTC (WHEP) test viewer built on str0m in its normal (frame) mode,
//! with a minimal HTTP/1.1 client for the signalling requests. Shared by
//! tst-webrtc's socket-opening tests and the JVM binding's viewer probe.
//!
//! The viewer drives its sans-IO loop with the real clock; only the server
//! under test may run on a manual clock.

use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::channel::ChannelId;
use str0m::media::{Direction, MediaKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig, RtcError};

/// The viewer's longest wait between STUN checks: short, so a viewer
/// refreshes its consent often enough for the reaper tests.
const CLIENT_MAX_STUN_RTO: Duration = Duration::from_millis(300);

/// A WebRTC viewer: the offerer, receive-only video and, optionally, a
/// data-channel section the server opens its `klv` channel on.
pub struct TestViewer {
    pub rtc: Rtc,
    pub sock: UdpSocket,
    pub mid: Option<Mid>,
    /// The audio section's mid, when the offer had one.
    pub audio_mid: Option<Mid>,
    pending: Option<SdpPendingOffer>,
    /// (RTP timestamp, Annex-B access unit) per frame received.
    pub frames: Vec<(u32, Vec<u8>)>,
    /// (RTP timestamp, payload) per audio packet received.
    pub audio_frames: Vec<(u64, Vec<u8>)>,
    /// Messages received on the `klv` data channel, in order.
    pub dc_msgs: Vec<Vec<u8>>,
    /// DTLS is up.
    pub connected: bool,
    /// The server opened the `klv` data channel.
    pub dc_open: bool,
    /// The `klv` channel's id, once the server opened it.
    dc_id: Option<ChannelId>,
    /// The channels the viewer opened itself; the server's `klv` channel
    /// is never one of them, whatever their labels.
    own_channels: Vec<ChannelId>,
    /// Messages received on the viewer's own channels: the server writes
    /// KLV only to the channel it opened, so this stays 0.
    pub own_channel_msgs: u64,
    /// Every address a datagram came from.
    pub sources: HashSet<SocketAddr>,
    /// STUN binding responses received: one per consent check the server
    /// answered.
    pub stun_responses: u64,
    /// ICE state changes, for a failure message.
    pub ice_log: Vec<String>,
    /// The first str0m error seen, if any.
    pub error: Option<String>,
    buf: Vec<u8>,
}

impl Default for TestViewer {
    fn default() -> Self {
        Self::new()
    }
}

impl TestViewer {
    /// A viewer with str0m's default codecs.
    pub fn new() -> Self {
        Self::with_config(Rtc::builder())
    }

    /// A viewer that offers H.264 only.
    pub fn h264_only() -> Self {
        Self::with_config(Rtc::builder().clear_codecs().enable_h264(true))
    }

    fn with_config(mut cfg: RtcConfig) -> Self {
        cfg.set_max_stun_rto(CLIENT_MAX_STUN_RTO);
        let mut rtc = cfg.build(Instant::now());
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind the viewer socket");
        let addr = sock.local_addr().expect("viewer address");
        rtc.add_local_candidate(Candidate::host(addr, "udp").expect("host candidate"));
        Self {
            rtc,
            sock,
            mid: None,
            audio_mid: None,
            pending: None,
            frames: Vec::new(),
            audio_frames: Vec::new(),
            dc_msgs: Vec::new(),
            connected: false,
            dc_open: false,
            dc_id: None,
            own_channels: Vec::new(),
            own_channel_msgs: 0,
            sources: HashSet::new(),
            stun_responses: 0,
            ice_log: Vec::new(),
            error: None,
            buf: vec![0; 2048],
        }
    }

    /// The SDP offer: a receive-only video section and, with `with_dc`, a
    /// data-channel section (`m=application`), brought up by a channel of
    /// the viewer's own as a browser page does before `createOffer`. The
    /// server opens its `klv` channel on that section; without one the
    /// viewer gets no data channel.
    pub fn offer(&mut self, with_dc: bool) -> String {
        self.build_offer(with_dc.then_some("viewer"), None)
    }

    /// The offer of [`offer`](Self::offer) with the viewer's own channel
    /// labelled `label` (a page may name it `klv` too).
    pub fn offer_with_channel_label(&mut self, label: &str) -> String {
        self.build_offer(Some(label), None)
    }

    /// The offer of [`offer`](Self::offer) plus a receive-only audio
    /// section (Opus among str0m's default codecs).
    pub fn offer_with_audio(&mut self, with_dc: bool) -> String {
        self.build_offer(with_dc.then_some("viewer"), Some(Direction::RecvOnly))
    }

    /// The offer of [`offer`](Self::offer) plus an audio section in the
    /// viewer's direction `audio`.
    pub fn offer_with_audio_direction(&mut self, with_dc: bool, audio: Direction) -> String {
        self.build_offer(with_dc.then_some("viewer"), Some(audio))
    }

    fn build_offer(&mut self, channel: Option<&str>, audio: Option<Direction>) -> String {
        let mut api = self.rtc.sdp_api();
        let mid = api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        if let Some(dir) = audio {
            self.audio_mid = Some(api.add_media(MediaKind::Audio, dir, None, None, None));
        }
        if let Some(label) = channel {
            // Any channel brings the SCTP association into the offer; the
            // server's `klv` channel is a different one, whatever this
            // one is called.
            self.own_channels.push(api.add_channel(label.into()));
        }
        let (offer, pending) = api.apply().expect("an offer");
        self.mid = Some(mid);
        self.pending = Some(pending);
        offer.to_sdp_string()
    }

    /// Opens one more channel of the viewer's own, in-band (DCEP) on the
    /// connected session; no renegotiation follows.
    pub fn open_channel(&mut self, label: &str) {
        let mut api = self.rtc.sdp_api();
        self.own_channels.push(api.add_channel(label.into()));
        assert!(
            api.apply().is_none(),
            "a channel on a live session needs no offer"
        );
    }

    /// Applies the server's answer.
    pub fn accept_answer(&mut self, sdp: &str) {
        let answer = SdpAnswer::from_sdp_string(sdp).expect("the answer parses");
        let pending = self.pending.take().expect("an offer was made");
        self.rtc
            .sdp_api()
            .accept_answer(pending, answer)
            .expect("the answer is accepted");
    }

    /// The SSRC the viewer receives video on, once media arrived.
    pub fn video_ssrc(&mut self) -> Option<u32> {
        let mid = self.mid?;
        self.rtc
            .direct_api()
            .stream_rx_by_mid(mid, None)
            .map(|s| *s.ssrc())
    }

    /// One turn of the loop: drain str0m's output, wait up to `max_wait`
    /// for one datagram, feed it and the clock.
    pub fn pump_once(&mut self, max_wait: Duration) {
        let mut next = Instant::now() + max_wait;
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(t)) => {
                    next = next.min(t);
                    break;
                }
                Ok(Output::Transmit(t)) => {
                    let _ = self.sock.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(ev)) => self.on_event(ev),
                // One frame that does not depacketize; keep polling.
                Err(RtcError::Packet(..)) => {}
                Err(e) => {
                    self.error.get_or_insert(format!("poll_output: {e}"));
                    break;
                }
            }
        }
        let wait = next.saturating_duration_since(Instant::now()).clamp(
            Duration::from_micros(200),
            max_wait.max(Duration::from_micros(200)),
        );
        let _ = self.sock.set_read_timeout(Some(wait));
        match self.sock.recv_from(&mut self.buf) {
            Ok((n, source)) => {
                self.sources.insert(source);
                // A STUN binding success response: type 0x0101.
                if n >= 2 && self.buf[0] == 0x01 && self.buf[1] == 0x01 {
                    self.stun_responses += 1;
                }
                let destination = self.sock.local_addr().expect("viewer address");
                if let Ok(contents) = self.buf[..n].try_into() {
                    let input = Input::Receive(
                        Instant::now(),
                        Receive {
                            proto: Protocol::Udp,
                            source,
                            destination,
                            contents,
                        },
                    );
                    if self.rtc.accepts(&input)
                        && let Err(e) = self.rtc.handle_input(input)
                    {
                        self.error.get_or_insert(format!("handle_input: {e}"));
                    }
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => panic!("viewer recv_from failed: {e}"),
        }
        if let Err(e) = self.rtc.handle_input(Input::Timeout(Instant::now())) {
            self.error.get_or_insert(format!("timeout input: {e}"));
        }
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Connected => self.connected = true,
            Event::IceConnectionStateChange(s) => self.ice_log.push(format!("{s:?}")),
            Event::ChannelOpen(id, _) if self.own_channels.contains(&id) => {}
            Event::ChannelOpen(id, label) if label == "klv" => {
                self.dc_open = true;
                self.dc_id = Some(id);
            }
            Event::ChannelData(d) if Some(d.id) == self.dc_id => self.dc_msgs.push(d.data),
            Event::ChannelData(d) if self.own_channels.contains(&d.id) => {
                self.own_channel_msgs += 1;
            }
            Event::MediaData(d) if Some(d.mid) == self.audio_mid => {
                self.audio_frames.push((d.time.numer(), d.data.to_vec()));
            }
            Event::MediaData(d) => self.frames.push((d.time.numer() as u32, d.data.to_vec())),
            _ => {}
        }
    }

    /// Pumps until `until` holds or `budget` runs out; returns whether it
    /// held.
    pub fn pump(&mut self, until: impl Fn(&Self) -> bool, budget: Duration) -> bool {
        let end = Instant::now() + budget;
        loop {
            if until(self) {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            self.pump_once(Duration::from_millis(5));
        }
    }

    /// A short description for a failed assertion.
    pub fn trace(&self) -> String {
        format!(
            "connected={} dc_open={} frames={} audio={} dc_msgs={} own_channel_msgs={} ice={:?} error={:?}",
            self.connected,
            self.dc_open,
            self.frames.len(),
            self.audio_frames.len(),
            self.dc_msgs.len(),
            self.own_channel_msgs,
            self.ice_log,
            self.error
        )
    }
}

/// Pumps every viewer in turn until `until` holds or `budget` runs out.
pub fn pump_all(
    viewers: &mut [TestViewer],
    until: impl Fn(&[TestViewer]) -> bool,
    budget: Duration,
) -> bool {
    let end = Instant::now() + budget;
    loop {
        if until(viewers) {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        for v in viewers.iter_mut() {
            v.pump_once(Duration::from_millis(1));
        }
    }
}

/// A parsed HTTP response: the status, the headers (names lowercased),
/// the body.
pub type HttpResponse = (u16, HashMap<String, String>, String);

/// The bytes of one HTTP/1.1 request.
fn request_bytes(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    let mut bytes = req.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// One HTTP/1.1 exchange over a fresh connection.
pub fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> HttpResponse {
    let mut s = TcpStream::connect(addr).expect("connect to the signalling port");
    s.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    s.write_all(&request_bytes(addr, method, path, headers, body))
        .expect("send the request");
    read_response(&mut s)
}

/// A TLS client connection to `addr` that trusts `root_pem` and names the
/// server `localhost`.
pub fn tls_client(
    addr: SocketAddr,
    root_pem: &str,
) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
    use std::sync::Arc;
    let roots = tst_core::net::tls::load_root_store(root_pem.as_bytes()).expect("trust bundle");
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    let conn = rustls::ClientConnection::new(Arc::new(config), "localhost".try_into().unwrap())
        .expect("client connection");
    let sock = TcpStream::connect(addr).expect("connect to the signalling port");
    sock.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    rustls::StreamOwned::new(conn, sock)
}

/// [`http`] over TLS.
pub fn https(
    addr: SocketAddr,
    root_pem: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> HttpResponse {
    let mut s = tls_client(addr, root_pem);
    s.write_all(&request_bytes(addr, method, path, headers, body))
        .expect("send the request");
    let mut raw = Vec::new();
    match s.read_to_end(&mut raw) {
        Ok(_) => {}
        // A server that closes without close_notify after its response.
        Err(e) if e.kind() == ErrorKind::UnexpectedEof && !raw.is_empty() => {}
        Err(e) => panic!("read the response: {e}"),
    }
    parse_response(&raw).unwrap_or_else(|| panic!("not an HTTP response: {raw:?}"))
}

/// Reads a response until the server closes the connection.
pub fn read_response(s: &mut TcpStream) -> HttpResponse {
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).expect("read the response");
    parse_response(&raw).unwrap_or_else(|| panic!("not an HTTP response: {raw:?}"))
}

/// Parses a complete response; `None` when it is not one.
pub fn parse_response(raw: &[u8]) -> Option<HttpResponse> {
    let text = std::str::from_utf8(raw).ok()?;
    let (head, body) = text.split_once("\r\n\r\n")?;
    let mut lines = head.split("\r\n");
    let status = lines.next()?.split(' ').nth(1)?.parse().ok()?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Some((status, headers, body.to_string()))
}

/// POSTs an SDP offer to `mount`, with a bearer token when given.
pub fn whep_post(addr: SocketAddr, mount: &str, offer: &str, bearer: Option<&str>) -> HttpResponse {
    let auth = bearer.map(|t| format!("Bearer {t}"));
    let mut headers = vec![("Content-Type", "application/sdp")];
    if let Some(a) = &auth {
        headers.push(("Authorization", a.as_str()));
    }
    http(addr, "POST", mount, &headers, offer.as_bytes())
}

/// [`whep_post`] over TLS.
pub fn whep_post_tls(
    addr: SocketAddr,
    root_pem: &str,
    mount: &str,
    offer: &str,
    bearer: Option<&str>,
) -> HttpResponse {
    let auth = bearer.map(|t| format!("Bearer {t}"));
    let mut headers = vec![("Content-Type", "application/sdp")];
    if let Some(a) = &auth {
        headers.push(("Authorization", a.as_str()));
    }
    https(addr, root_pem, "POST", mount, &headers, offer.as_bytes())
}

/// POSTs the viewer's offer, applies the answer, and returns the session
/// resource path.
pub fn join(addr: SocketAddr, mount: &str, v: &mut TestViewer, with_dc: bool) -> String {
    let offer = v.offer(with_dc);
    let (status, headers, answer) = whep_post(addr, mount, &offer, None);
    assert_eq!(status, 201, "offer refused: {answer}");
    v.accept_answer(&answer);
    headers["location"].clone()
}

/// [`join`] with an offer that also asks for audio; returns the answer SDP
/// too.
pub fn join_with_audio(addr: SocketAddr, mount: &str, v: &mut TestViewer) -> (String, String) {
    let offer = v.offer_with_audio(false);
    let (status, headers, answer) = whep_post(addr, mount, &offer, None);
    assert_eq!(status, 201, "offer refused: {answer}");
    v.accept_answer(&answer);
    (headers["location"].clone(), answer)
}
/// Polls `cond` until it holds or `budget` runs out; returns whether it
/// held. Waits a millisecond between polls.
pub fn eventually(cond: impl Fn() -> bool, budget: Duration) -> bool {
    let end = Instant::now() + budget;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::park_timeout(Duration::from_millis(1));
    }
}
