//! The publish facade against a scripted server: what it writes, in which
//! order and on which channels, and what its calls return when the server,
//! the caller or the connection ends the session.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tst_core::mpegts::common::Pts90khz;
use tst_core::transport::{BrokenCause, EndReason, Transport, TransportError};
use tst_test_helpers::timing::scaled;

use super::{
    PUSH_MP2T_MAX_PAYLOAD, PushCloseOutcome, PushDescription, PushError, PushStatsHandle, RtspPush,
    VideoTrack,
};
use crate::error::RtspError;
use crate::packet::RtpHeader;
use crate::rtcp::{ReceiverReport, ReportBlock, SenderReport};
use crate::rtsp::client::builder::RtspClientBuilder;
use crate::rtsp::client::media_queue::MEDIA_QUEUE_BOUND;
use crate::rtsp::client::{RtspCancelHandle, RtspClient};
use crate::rtsp::message::{RtspMethod, RtspRequest};

/// The session id the scripted server names.
const SESSION: &str = "abc";

const SPS: &[u8] = &[0x67, 0x42, 0xC0, 0x1F, 0xDA, 0x01, 0x40, 0x16];
const PPS: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];

/// One thing the client wrote.
#[derive(Debug)]
enum Unit {
    /// A request, with its body.
    Request(RtspRequest),
    /// A `$` frame: its channel and what it carries.
    Frame(u8, Vec<u8>),
}

/// The first whole unit of `buf` and its length, once all of it is there.
fn split_unit(buf: &[u8]) -> Option<(Unit, usize)> {
    if *buf.first()? == b'$' {
        let len = usize::from(u16::from_be_bytes([*buf.get(2)?, *buf.get(3)?]));
        let carried = buf.get(4..4 + len)?;
        return Some((Unit::Frame(buf[1], carried.to_vec()), 4 + len));
    }
    let head_len = buf.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let head = String::from_utf8_lossy(&buf[..head_len]).to_ascii_lowercase();
    let body_len = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .map_or(0, |v| v.trim().parse::<usize>().expect("a body length"));
    let whole = buf.get(..head_len + body_len)?;
    let (request, used) = RtspRequest::parse(whole).expect("a well-formed request");
    assert_eq!(used, whole.len(), "bytes of the request left over");
    Some((Unit::Request(request), used))
}

/// The server's end of one connection, driven by a test's script.
struct Peer {
    sock: TcpStream,
    /// What was read; the bytes from `at` on are not yet handed out.
    pending: Vec<u8>,
    at: usize,
    /// Where a read lands.
    chunk: Vec<u8>,
}

impl Peer {
    /// The next whole unit; `None` once the client has closed or reset the
    /// connection.
    fn read_unit(&mut self) -> Option<Unit> {
        loop {
            if let Some((unit, used)) = split_unit(&self.pending[self.at..]) {
                self.at += used;
                return Some(unit);
            }
            // What was handed out goes, once per read and not per unit.
            self.pending.drain(..self.at);
            self.at = 0;
            match self.sock.read(&mut self.chunk) {
                Ok(0) => return None,
                Ok(n) => self.pending.extend_from_slice(&self.chunk[..n]),
                // How a hang-up with bytes unread shows, by platform.
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
                    ) =>
                {
                    return None;
                }
                Err(e) => panic!("the scripted server's read failed: {e}"),
            }
        }
    }

    /// The next unit, which has to be a request.
    fn request(&mut self) -> RtspRequest {
        match self.read_unit() {
            Some(Unit::Request(request)) => request,
            Some(Unit::Frame(channel, carried)) => panic!(
                "expected a request, got {} bytes on channel {channel}",
                carried.len()
            ),
            None => panic!("expected a request, the client hung up"),
        }
    }

    /// Answer `request` with `status` (code and reason) and `headers`
    /// (whole lines, each with its CRLF).
    fn answer(&mut self, request: &RtspRequest, status: &str, headers: &str) {
        let reply = reply(request, status, headers);
        self.sock.write_all(&reply).expect("the server's reply");
    }

    /// A whole publish handshake, each request answered `200`: ANNOUNCE, a
    /// SETUP per track on the channels it asks for, RECORD; a keepalive
    /// ping among them too. Returns the requests.
    fn accept_push(&mut self) -> Vec<RtspRequest> {
        let mut seen = Vec::new();
        loop {
            let request = self.request();
            if request.method == RtspMethod::Setup {
                let transport = request.headers["transport"].replace(";mode=record", "");
                let headers =
                    format!("Session: {SESSION};timeout=55\r\nTransport: {transport}\r\n");
                self.answer(&request, "200 OK", &headers);
            } else {
                self.answer(&request, "200 OK", "");
            }
            let recording = request.method == RtspMethod::Record;
            seen.push(request);
            if recording {
                return seen;
            }
        }
    }

    /// Everything up to and with the TEARDOWN, every request answered
    /// `200`.
    fn until_teardown(&mut self) -> Vec<Unit> {
        let mut seen = Vec::new();
        loop {
            let unit = self.read_unit().expect("a TEARDOWN before the hang-up");
            let mut last = false;
            if let Unit::Request(request) = &unit {
                last = request.method == RtspMethod::Teardown;
                // A push that was dropped does not wait for the TEARDOWN's
                // answer and may have hung up by now.
                let written = self.sock.write_all(&reply(request, "200 OK", ""));
                assert!(last || written.is_ok(), "the server's reply: {written:?}");
            }
            seen.push(unit);
            if last {
                return seen;
            }
        }
    }

    /// Everything the client writes until it hangs up; nothing is answered.
    fn until_hangup(&mut self) -> Vec<Unit> {
        std::iter::from_fn(|| self.read_unit()).collect()
    }
}

/// A reply to `request`: `status`, its `CSeq` and `headers`.
fn reply(request: &RtspRequest, status: &str, headers: &str) -> Vec<u8> {
    let cseq = &request.headers["cseq"];
    format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\n{headers}\r\n").into_bytes()
}

/// A listener on loopback whose first connection `script` serves on a
/// thread of its own. A script that fails panics with what it expected,
/// which drops the connection.
fn serve<T: Send + 'static>(
    script: impl FnOnce(&mut Peer) -> T + Send + 'static,
) -> (u16, JoinHandle<T>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let thread = std::thread::Builder::new()
        .name("scripted-rtsp-server".into())
        .spawn(move || {
            let (sock, _) = listener.accept().expect("the client connects");
            // A script the client never satisfies fails here, not never.
            sock.set_read_timeout(Some(scaled(Duration::from_secs(20))))
                .unwrap();
            let mut peer = Peer {
                sock,
                pending: Vec::new(),
                at: 0,
                chunk: vec![0; 256 * 1024],
            };
            script(&mut peer)
        })
        .unwrap();
    (port, thread)
}

fn joined<T>(peer: JoinHandle<T>) -> T {
    peer.join().expect("the scripted server's script")
}

fn builder(port: u16, rest: &str) -> RtspClientBuilder {
    RtspClientBuilder::new(&format!("rtsp://127.0.0.1:{port}{rest}")).unwrap()
}

/// What a script that takes the handshake and reads to the TEARDOWN
/// returns: the handshake's requests, then every unit behind them.
type Seen = (Vec<RtspRequest>, Vec<Unit>);

/// A recording push of `desc` at `rest` (path and query), and the server
/// that takes its handshake and everything up to its TEARDOWN.
fn recording_at(rest: &str, desc: &PushDescription) -> (RtspPush, JoinHandle<Seen>) {
    let (port, peer) = serve(|peer| (peer.accept_push(), peer.until_teardown()));
    let push = RtspPush::connect(builder(port, rest), desc).unwrap();
    (push, peer)
}

fn recording(desc: &PushDescription) -> (RtspPush, JoinHandle<Seen>) {
    recording_at("/live", desc)
}

/// Close `push`, which the server accepts, and return what the server saw.
fn closed(push: RtspPush, peer: JoinHandle<Seen>) -> Seen {
    push.close().expect("an accepted TEARDOWN");
    joined(peer)
}

/// A recording push whose server takes the handshake, then reads nothing
/// until `open` is sent, then runs `rest`.
fn recording_gated<T: Send + 'static>(
    desc: &PushDescription,
    rest: impl FnOnce(&mut Peer) -> T + Send + 'static,
) -> (RtspPush, mpsc::Sender<()>, JoinHandle<T>) {
    let (open, gate) = mpsc::channel::<()>();
    let (port, peer) = serve(move |peer| {
        peer.accept_push();
        gate.recv_timeout(scaled(Duration::from_secs(20)))
            .expect("the test opens the gate");
        rest(peer)
    });
    let push = RtspPush::connect(builder(port, "/live"), desc).unwrap();
    (push, open, peer)
}

fn elementary(klv: bool) -> PushDescription {
    let video = VideoTrack::H264 {
        parameter_sets: vec![SPS.to_vec(), PPS.to_vec()],
    };
    PushDescription::Elementary { video, klv }
}

/// `nals` behind four-byte start codes.
fn annexb(nals: &[&[u8]]) -> Vec<u8> {
    nals.iter()
        .flat_map(|nal| [&[0, 0, 0, 1][..], nal].concat())
        .collect()
}

/// A NAL unit of `len` bytes behind `header`, with no zero byte in it (so
/// no start code).
fn nal(header: &[u8], len: usize) -> Vec<u8> {
    let filler = (0..len - header.len()).map(|i| (i % 251) as u8 + 1);
    header.iter().copied().chain(filler).collect()
}

/// `count` TS packets filled with `fill`.
fn ts(count: usize, fill: u8) -> Vec<u8> {
    let mut packet = [fill; 188];
    packet[0] = 0x47;
    packet.repeat(count)
}

fn pts(ticks: i64) -> Pts90khz {
    Pts90khz::new(ticks)
}

/// The frames among `units`: channel and what each carries.
fn frames(units: &[Unit]) -> Vec<(u8, &[u8])> {
    units
        .iter()
        .filter_map(|unit| match unit {
            Unit::Frame(channel, carried) => Some((*channel, carried.as_slice())),
            Unit::Request(_) => None,
        })
        .collect()
}

/// What the frames on `channel` carry, in order.
fn on_channel(units: &[Unit], channel: u8) -> Vec<&[u8]> {
    let on = frames(units).into_iter().filter(|(c, _)| *c == channel);
    on.map(|(_, carried)| carried).collect()
}

/// The requests among `units`.
fn requests(units: &[Unit]) -> Vec<&RtspRequest> {
    units
        .iter()
        .filter_map(|unit| match unit {
            Unit::Request(request) => Some(request),
            Unit::Frame(..) => None,
        })
        .collect()
}

/// An RTP packet's header and payload.
fn rtp(packet: &[u8]) -> (RtpHeader, &[u8]) {
    let parsed = RtpHeader::decode(packet).expect("an RTP packet");
    (
        parsed.header,
        &packet[parsed.payload_offset..parsed.payload_end],
    )
}

/// The sender report that opens a compound RTCP packet.
fn sender_report(packet: &[u8]) -> SenderReport {
    SenderReport::decode(packet).expect("a sender report").0
}

/// A receiver report about `ssrc`.
fn receiver_report(ssrc: u32, fraction_lost: u8, cumulative_lost: i32) -> Vec<u8> {
    let block = ReportBlock {
        ssrc,
        fraction_lost,
        cumulative_lost,
        extended_highest_seq: 0,
        jitter: 0,
        last_sr: 0,
        delay_since_last_sr: 0,
    };
    let report = ReceiverReport {
        ssrc: 0x5E77_E700,
        report_blocks: vec![block],
    };
    report.encode().unwrap()
}

/// `carried` as a `$` frame on `channel`.
fn interleaved(channel: u8, carried: &[u8]) -> Vec<u8> {
    [
        &[b'$', channel][..],
        &(carried.len() as u16).to_be_bytes(),
        carried,
    ]
    .concat()
}

/// Wait until `cond` holds; fail when it does not within five seconds.
fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let give_up = Instant::now() + scaled(Duration::from_secs(5));
    while !cond() {
        assert!(Instant::now() < give_up, "never happened: {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn is_explicit_close(e: &PushError) -> bool {
    matches!(e, PushError::Transport(TransportError::ExplicitClose))
}

fn broken_cause(e: &TransportError) -> Option<BrokenCause> {
    match e {
        TransportError::Broken { cause, .. } => Some(*cause),
        _ => None,
    }
}

// ---- the handshake ---------------------------------------------------

#[test]
fn connect_runs_announce_setup_record_with_consecutive_cseqs_from_1() {
    let (push, peer) = recording(&elementary(true));
    assert_eq!(push.session_id(), SESSION);
    let (handshake, rest) = closed(push, peer);
    let methods: Vec<RtspMethod> = handshake.iter().map(|r| r.method).collect();
    use RtspMethod::{Announce, Record, Setup};
    assert_eq!(methods, [Announce, Setup, Setup, Record]);
    let cseqs: Vec<&str> = handshake
        .iter()
        .map(|r| r.headers["cseq"].as_str())
        .collect();
    assert_eq!(cseqs, ["1", "2", "3", "4"]);
    let teardown = requests(&rest);
    assert_eq!(teardown.len(), 1);
    assert_eq!(teardown[0].headers["cseq"], "5");
    assert_eq!(teardown[0].headers["session"], SESSION);
}

#[test]
fn the_announce_body_is_the_sdp_for_the_description() {
    use base64::Engine as _;
    let b64 = |nal: &[u8]| base64::engine::general_purpose::STANDARD.encode(nal);

    let (push, peer) = recording(&elementary(true));
    let (handshake, _) = closed(push, peer);
    let announce = &handshake[0];
    assert_eq!(announce.headers["content-type"], "application/sdp");
    let body = String::from_utf8(announce.body.to_vec()).unwrap();
    let lines: Vec<&str> = body.split("\r\n").collect();
    assert_eq!(lines[0], "v=0");
    // The origin is the control connection's local address.
    let origin: Vec<&str> = lines[1].split(' ').collect();
    assert_eq!(origin[0], "o=-");
    assert!(origin[1].parse::<u32>().is_ok(), "{}", lines[1]);
    assert_eq!(origin[2..], ["1", "IN", "IP4", "127.0.0.1"]);
    let fmtp = format!(
        "a=fmtp:96 packetization-mode=1;profile-level-id=42C01F;sprop-parameter-sets={},{}",
        b64(SPS),
        b64(PPS)
    );
    let media: Vec<&str> = lines
        .iter()
        .copied()
        .skip_while(|l| !l.starts_with("m="))
        .collect();
    assert_eq!(
        media,
        [
            "m=video 0 RTP/AVP 96",
            "a=rtpmap:96 H264/90000",
            fmtp.as_str(),
            "a=control:streamid=0",
            "m=application 0 RTP/AVP 97",
            "a=rtpmap:97 smpte336m/90000",
            "a=control:streamid=1",
            "",
        ]
    );
    assert!(crate::sdp::Sdp::parse(&announce.body).is_ok());
}

/// A client whose control socket has an IPv4-mapped IPv6 local address
/// names the IPv4 address as its origin. Linux only: elsewhere a socket
/// that dials a mapped address is not dual-stack by default.
#[cfg(target_os = "linux")]
#[test]
fn an_ipv4_mapped_local_address_is_announced_as_ipv4() {
    let (port, peer) = serve(|peer| (peer.accept_push(), peer.until_teardown()));
    let mapped = RtspClientBuilder::new(&format!("rtsp://[::ffff:127.0.0.1]:{port}/live")).unwrap();
    let push = RtspPush::connect(mapped, &PushDescription::Mp2t).unwrap();
    let local = push.client().local_ip;
    assert!(local.is_some_and(|ip| ip.is_ipv4()), "{local:?}");
    let (handshake, _) = closed(push, peer);
    let body = String::from_utf8(handshake[0].body.to_vec()).unwrap();
    assert!(body.contains(" 1 IN IP4 127.0.0.1\r\n"), "{body}");
    assert!(body.contains("c=IN IP4 0.0.0.0\r\n"), "{body}");
}

/// A socket that does not tell its local address: the description names
/// the unspecified address of the family the client is connected over.
#[test]
fn an_unknown_local_address_is_announced_as_the_unspecified_one() {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let v4: IpAddr = Ipv4Addr::new(192, 0, 2, 7).into();
    let v6: IpAddr = Ipv6Addr::LOCALHOST.into();
    assert_eq!(super::origin_address(Some(v4), v6), v4);
    assert_eq!(super::origin_address(None, v4), Ipv4Addr::UNSPECIFIED);
    assert_eq!(super::origin_address(None, v6), Ipv6Addr::UNSPECIFIED);
}

#[test]
fn an_mp2t_description_announces_one_track_of_payload_type_33() {
    let (push, peer) = recording(&PushDescription::Mp2t);
    let (handshake, _) = closed(push, peer);
    let body = String::from_utf8(handshake[0].body.to_vec()).unwrap();
    assert!(body.contains("m=video 0 RTP/AVP 33\r\n"), "{body}");
    assert_eq!(body.matches("m=").count(), 1);
    assert_eq!(handshake.len(), 3, "ANNOUNCE, one SETUP, RECORD");
}

#[test]
fn setup_urls_append_streamid_to_the_request_url() {
    for (path, track_url) in [
        ("/live", "/live/streamid="),
        ("/live/", "/live/streamid="),
        ("/a/b", "/a/b/streamid="),
        ("/", "/streamid="),
        ("", "/streamid="),
    ] {
        let (push, peer) = recording_at(path, &elementary(true));
        let (handshake, rest) = closed(push, peer);
        let port = handshake[0]
            .uri
            .strip_prefix("rtsp://127.0.0.1:")
            .and_then(|r| r.split('/').next())
            .unwrap()
            .to_string();
        let at = |rest: &str| format!("rtsp://127.0.0.1:{port}{rest}");
        let uris: Vec<&str> = handshake.iter().map(|r| r.uri.as_str()).collect();
        assert_eq!(
            uris,
            [
                at(path),
                format!("{}0", at(track_url)),
                format!("{}1", at(track_url)),
                at(path)
            ],
            "path {path:?}"
        );
        // The session's own requests keep the URL as given.
        assert_eq!(requests(&rest)[0].uri, at(path), "path {path:?}");
    }
}

#[test]
fn each_setup_carries_mode_record_and_later_ones_the_bare_session_id() {
    let (mut push, peer) = recording(&elementary(true));
    push.pause().unwrap();
    let (handshake, rest) = closed(push, peer);
    let (first, second, record) = (&handshake[1], &handshake[2], &handshake[3]);
    assert_eq!(
        first.headers["transport"],
        "RTP/AVP/TCP;unicast;interleaved=0-1;mode=record"
    );
    assert_eq!(
        second.headers["transport"],
        "RTP/AVP/TCP;unicast;interleaved=2-3;mode=record"
    );
    assert!(!handshake[0].headers.contains_key("session"));
    assert!(!first.headers.contains_key("session"));
    // The server said `abc;timeout=55`.
    assert_eq!(second.headers["session"], SESSION);
    assert_eq!(record.headers["session"], SESSION);
    for later in requests(&rest) {
        assert_eq!(later.headers["session"], SESSION, "{:?}", later.method);
    }
}

#[test]
fn any_2xx_is_success_for_announce_setup_record_and_pause() {
    let (port, peer) = serve(|peer| {
        let announce = peer.request();
        peer.answer(&announce, "201 Created", "");
        let setup = peer.request();
        let headers = "Session: abc\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n";
        peer.answer(&setup, "202 Accepted", headers);
        let record = peer.request();
        peer.answer(&record, "204 No Content", "");
        let pause = peer.request();
        assert_eq!(pause.method, RtspMethod::Pause);
        peer.answer(&pause, "250 Fine", "");
        peer.until_teardown().len()
    });
    let mut push = RtspPush::connect(builder(port, "/live"), &PushDescription::Mp2t).unwrap();
    push.pause().unwrap();
    push.close().unwrap();
    assert_eq!(joined(peer), 1, "only the TEARDOWN followed the PAUSE");
}

#[test]
fn a_refused_announce_is_protocol_with_its_status() {
    let (port, peer) = serve(|peer| {
        let announce = peer.request();
        peer.answer(&announce, "415 Unsupported Media Type", "");
        peer.until_hangup().len()
    });
    let r = RtspPush::connect(builder(port, "/live"), &PushDescription::Mp2t);
    assert!(
        matches!(&r, Err(RtspError::Protocol { code: 415, .. })),
        "{r:?}"
    );
    assert_eq!(joined(peer), 0, "nothing follows a refused ANNOUNCE");
}

#[test]
fn a_3xx_to_announce_is_protocol_and_not_followed() {
    let elsewhere = TcpListener::bind("127.0.0.1:0").unwrap();
    elsewhere.set_nonblocking(true).unwrap();
    let location = format!(
        "Location: rtsp://127.0.0.1:{}/moved\r\n",
        elsewhere.local_addr().unwrap().port()
    );
    let (port, peer) = serve(move |peer| {
        let announce = peer.request();
        peer.answer(&announce, "302 Found", &location);
        peer.until_hangup().len()
    });
    let r = RtspPush::connect(builder(port, "/live"), &PushDescription::Mp2t);
    assert!(
        matches!(&r, Err(RtspError::Protocol { code: 302, .. })),
        "{r:?}"
    );
    // The client is gone by now: it wrote nothing more here, and it never
    // connected there.
    assert_eq!(joined(peer), 0);
    let nobody = elsewhere.accept().map(|_| ());
    assert_eq!(nobody.map_err(|e| e.kind()), Err(ErrorKind::WouldBlock));
}

#[test]
fn frames_use_the_channels_the_server_assigned() {
    let (port, peer) = serve(|peer| {
        let announce = peer.request();
        peer.answer(&announce, "200 OK", "");
        let setup = peer.request();
        assert!(setup.headers["transport"].contains("interleaved=0-1"));
        let headers = "Session: abc\r\nTransport: RTP/AVP/TCP;unicast;interleaved=6-7\r\n";
        peer.answer(&setup, "200 OK", headers);
        let record = peer.request();
        peer.answer(&record, "200 OK", "");
        peer.until_teardown()
    });
    let mut push = RtspPush::connect(builder(port, "/live"), &PushDescription::Mp2t).unwrap();
    push.send_ts(&ts(7, 1)).unwrap();
    push.close().unwrap();
    let units = joined(peer);
    let channels: Vec<u8> = frames(&units).iter().map(|(c, _)| *c).collect();
    // The report, the packet, the report `close` sends.
    assert_eq!(channels, [7, 6, 7]);
}

/// The reply headers of a SETUP the client can use, on `channels`.
fn usable_setup(channels: &str) -> String {
    format!("Session: {SESSION}\r\nTransport: RTP/AVP/TCP;unicast;interleaved={channels}\r\n")
}

/// What a server saw of a client whose `setup_record` it answered with a
/// reply the client cannot use.
struct RefusedSetup {
    result: Result<(), RtspError>,
    /// The requests the server read behind that reply, each answered `200`.
    after: Vec<RtspRequest>,
    /// The client, alive: the server has seen its connection end all the
    /// same.
    client: RtspClient,
}

/// `setup_record` of a video and a KLV track against a server that answers
/// the SETUP of track `bad` `200` with `headers` and the ones before it as
/// asked.
fn setup_answered(bad: usize, headers: &'static str) -> RefusedSetup {
    setup_answered_with(bad, "200 OK", headers)
}

/// [`setup_answered`] with `status` on the SETUP of track `bad`.
fn setup_answered_with(bad: usize, status: &'static str, headers: &'static str) -> RefusedSetup {
    let (noted, notes) = mpsc::channel();
    let (hung_up, hang_up) = mpsc::channel();
    let (port, peer) = serve(move |peer| {
        let announce = peer.request();
        peer.answer(&announce, "200 OK", "");
        for track in 0..bad {
            let setup = peer.request();
            let channels = format!("{}-{}", 2 * track, 2 * track + 1);
            peer.answer(&setup, "200 OK", &usable_setup(&channels));
        }
        let setup = peer.request();
        peer.answer(&setup, status, headers);
        while let Some(Unit::Request(request)) = peer.read_unit() {
            // Noted before it is answered: the client reads the note once
            // it has the answer.
            noted.send(request.clone()).unwrap();
            peer.answer(&request, "200 OK", "");
        }
        hung_up.send(()).unwrap();
    });
    let mut client = builder(port, "/live").connect().unwrap();
    client.announce(&elementary(true)).unwrap();
    let result = client.setup_record();
    // The call waited for the client's thread to end.
    let thread_holds = Arc::strong_count(&client.session_dead) - 1;
    assert_eq!(thread_holds, 0, "the client's thread still runs");
    // With the client still here: its drop would end the connection too.
    hang_up
        .recv_timeout(scaled(Duration::from_secs(2)))
        .expect("the refused reply did not end the connection");
    joined(peer);
    let after = notes.try_iter().collect();
    RefusedSetup {
        result,
        after,
        client,
    }
}

/// The client is finished: its reason is the server's fault, its session
/// reads dead, a request that would be written finds no connection, and
/// there is no session left to tear down.
fn assert_finished(mut client: RtspClient) {
    let reason = client.end_reason.get();
    assert!(
        matches!(reason, Some(EndReason::ProtocolError { .. })),
        "{reason:?}"
    );
    assert!(!client.is_session_alive());
    assert!(client.session_id.is_none());
    let r = client.options();
    assert!(matches!(&r, Err(RtspError::Io(_))), "{r:?}");
    let r = client.describe();
    assert!(matches!(&r, Err(RtspError::Io(_))), "{r:?}");
    let r = client.announce(&elementary(true));
    assert!(matches!(&r, Err(RtspError::Io(_))), "{r:?}");
    let r = client.setup_record();
    assert!(matches!(&r, Err(RtspError::Io(_))), "{r:?}");
    let r = client.play();
    assert!(matches!(&r, Err(RtspError::BadResponse { .. })), "{r:?}");
    client.teardown().unwrap();
    client.close().unwrap();
}

const UDP_REPLY: &str =
    "Session: abc\r\nTransport: RTP/AVP;unicast;client_port=5000-5001;server_port=6000-6001\r\n";

/// The client takes no session from a reply it refuses, so on the first
/// track it has none to tear down: ending the connection is what tells the
/// server.
#[test]
fn a_refused_first_setup_reply_ends_the_connection_without_a_teardown() {
    let seen = setup_answered(0, UDP_REPLY);
    assert!(
        matches!(&seen.result, Err(RtspError::BadResponse { .. })),
        "{:?}",
        seen.result
    );
    assert!(seen.after.is_empty(), "{:?}", seen.after);
    assert_finished(seen.client);
}

#[test]
fn a_udp_reply_to_a_second_record_setup_is_bad_response_and_tears_down() {
    let seen = setup_answered(1, UDP_REPLY);
    assert!(
        matches!(&seen.result, Err(RtspError::BadResponse { .. })),
        "{:?}",
        seen.result
    );
    // The TEARDOWN, then the end of the connection.
    assert_eq!(seen.after.len(), 1, "one request follows the reply");
    assert_eq!(seen.after[0].method, RtspMethod::Teardown);
    assert_eq!(seen.after[0].headers["session"], SESSION);
    assert_finished(seen.client);
}

#[test]
fn a_second_setup_reply_that_reuses_a_channel_is_bad_response_and_tears_down() {
    let seen = setup_answered(
        1,
        "Session: abc\r\nTransport: RTP/AVP/TCP;unicast;interleaved=1-2\r\n",
    );
    assert!(
        matches!(&seen.result, Err(RtspError::BadResponse { .. })),
        "{:?}",
        seen.result
    );
    assert_eq!(seen.after.len(), 1, "one request follows the reply");
    assert_eq!(seen.after[0].method, RtspMethod::Teardown);
    assert_finished(seen.client);
}

/// A `401` with no challenge to answer is an unusable reply as well.
#[test]
fn a_401_without_a_challenge_to_a_record_setup_ends_the_connection_too() {
    let seen = setup_answered_with(1, "401 Unauthorized", "");
    assert!(
        matches!(&seen.result, Err(RtspError::BadResponse { .. })),
        "{:?}",
        seen.result
    );
    assert_eq!(seen.after.len(), 1, "one request follows the reply");
    assert_eq!(seen.after[0].method, RtspMethod::Teardown);
    assert_finished(seen.client);
}

#[test]
fn a_first_setup_2xx_without_a_session_header_is_bad_response_and_ends_the_connection() {
    let seen = setup_answered(0, "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n");
    assert!(
        matches!(&seen.result, Err(RtspError::BadResponse { .. })),
        "{:?}",
        seen.result
    );
    assert!(seen.after.is_empty(), "{:?}", seen.after);
    assert_finished(seen.client);
}

/// `record` looks at the client's own state first: without a session and
/// every track set up it asks nothing of its thread or its connection.
#[test]
fn record_on_a_client_that_is_not_set_up_is_refused_by_the_client_itself() {
    // No session: the first SETUP reply named none, and the client ended
    // its connection over it.
    let seen = setup_answered(0, "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n");
    let r = seen.client.record();
    assert!(
        matches!(
            &r,
            Err(RtspError::NotAllowed {
                detail: "RECORD before every track is set up"
            })
        ),
        "{r:?}"
    );
    // A session, and one track of two set up.
    let (port, peer) = serve(|peer| {
        let announce = peer.request();
        peer.answer(&announce, "200 OK", "");
        let first = peer.request();
        peer.answer(&first, "200 OK", &usable_setup("0-1"));
        let second = peer.request();
        peer.answer(&second, "461 Unsupported Transport", "");
        requests(&peer.until_hangup())
            .iter()
            .map(|r| r.method)
            .collect::<Vec<_>>()
    });
    let mut client = builder(port, "/live").connect().unwrap();
    client.announce(&elementary(true)).unwrap();
    let r = client.setup_record();
    assert!(
        matches!(&r, Err(RtspError::Protocol { code: 461, .. })),
        "{r:?}"
    );
    let r = client.record();
    assert!(matches!(&r, Err(RtspError::NotAllowed { .. })), "{r:?}");
    // No RECORD: only the TEARDOWN of the dropped client's session.
    assert_eq!(joined(peer), [RtspMethod::Teardown]);
}

#[test]
fn a_setup_reply_whose_transport_does_not_parse_ends_the_connection_too() {
    let seen = setup_answered(
        0,
        "Session: abc\r\nTransport: RTP/AVP/TCP;unicast;interleaved=one-two\r\n",
    );
    assert!(
        matches!(&seen.result, Err(RtspError::UnsupportedTransport)),
        "{:?}",
        seen.result
    );
    assert!(seen.after.is_empty(), "{:?}", seen.after);
    assert_finished(seen.client);
}

/// A SETUP the server refuses changes nothing: the same call again sets
/// the track up.
#[test]
fn a_refused_setup_is_protocol_and_the_call_can_be_made_again() {
    let (port, peer) = serve(|peer| {
        let announce = peer.request();
        peer.answer(&announce, "200 OK", "");
        let refused = peer.request();
        peer.answer(&refused, "503 Service Unavailable", "");
        let mut seen = vec![refused];
        seen.extend(peer.accept_push());
        (seen, peer.until_teardown())
    });
    let mut client = builder(port, "/live").connect().unwrap();
    client.announce(&PushDescription::Mp2t).unwrap();
    let r = client.setup_record();
    assert!(
        matches!(&r, Err(RtspError::Protocol { code: 503, .. })),
        "{r:?}"
    );
    client.setup_record().unwrap();
    let push = client.record().unwrap();
    let (handshake, _) = closed(push, peer);
    let methods: Vec<RtspMethod> = handshake.iter().map(|r| r.method).collect();
    use RtspMethod::{Record, Setup};
    assert_eq!(methods, [Setup, Setup, Record]);
    assert_eq!(handshake[0].uri, handshake[1].uri);
}

/// `record` consumes the client, so a refused RECORD ends it: the drop
/// tears the session down.
#[test]
fn a_refused_record_is_protocol_and_the_dropped_client_tears_down() {
    let (port, peer) = serve(|peer| {
        let announce = peer.request();
        peer.answer(&announce, "200 OK", "");
        let setup = peer.request();
        peer.answer(&setup, "200 OK", &usable_setup("0-1"));
        let record = peer.request();
        assert_eq!(record.method, RtspMethod::Record);
        peer.answer(&record, "403 Forbidden", "");
        peer.until_hangup()
    });
    let r = RtspPush::connect(builder(port, "/live"), &PushDescription::Mp2t);
    assert!(
        matches!(&r, Err(RtspError::Protocol { code: 403, .. })),
        "{r:?}"
    );
    let after = joined(peer);
    let after = requests(&after);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].method, RtspMethod::Teardown);
    assert_eq!(after[0].headers["session"], SESSION);
}

#[test]
fn transport_udp_is_refused_before_any_setup_is_written() {
    let (port, peer) = serve(|peer| {
        let announce = peer.request();
        peer.answer(&announce, "200 OK", "");
        peer.until_hangup().len()
    });
    let r = RtspPush::connect(builder(port, "/live?transport=udp"), &PushDescription::Mp2t);
    assert!(matches!(&r, Err(RtspError::UnsupportedTransport)), "{r:?}");
    assert_eq!(joined(peer), 0, "nothing follows the ANNOUNCE");
}

#[test]
fn transport_tcp_and_no_transport_key_both_publish_interleaved() {
    for rest in ["/live", "/live?transport=tcp"] {
        let (push, peer) = recording_at(rest, &PushDescription::Mp2t);
        let (handshake, _) = closed(push, peer);
        assert!(
            handshake[1].headers["transport"].starts_with("RTP/AVP/TCP;"),
            "{rest}"
        );
    }
}

#[test]
fn a_description_without_parameter_sets_is_refused_and_nothing_is_written() {
    let (port, peer) = serve(|peer| peer.until_hangup().len());
    let mut client = builder(port, "/live").connect().unwrap();
    let no_pps = PushDescription::Elementary {
        video: VideoTrack::H264 {
            parameter_sets: vec![SPS.to_vec()],
        },
        klv: false,
    };
    let r = client.announce(&no_pps);
    assert!(matches!(&r, Err(RtspError::NotAllowed { .. })), "{r:?}");
    drop(client);
    assert_eq!(joined(peer), 0);
}

#[test]
fn a_key_frame_yields_its_parameter_sets_and_one_without_them_is_refused() {
    let idr = nal(&[0x65], 40);
    let key_frame = annexb(&[SPS, PPS, &idr]);
    match VideoTrack::h264_from_key_frame(&key_frame).unwrap() {
        VideoTrack::H264 { parameter_sets } => assert_eq!(parameter_sets, [SPS, PPS]),
        other => panic!("{other:?}"),
    }
    let r = VideoTrack::h264_from_key_frame(&annexb(&[&idr]));
    assert!(matches!(&r, Err(RtspError::NotAllowed { .. })), "{r:?}");

    let (vps, sps, pps) = (nal(&[0x40, 1], 8), nal(&[0x42, 1], 8), nal(&[0x44, 1], 8));
    let slice = nal(&[0x26, 1], 40);
    match VideoTrack::h265_from_key_frame(&annexb(&[&vps, &sps, &pps, &slice])).unwrap() {
        VideoTrack::H265 {
            vps: v,
            sps: s,
            pps: p,
        } => assert_eq!((v, s, p), (vps.clone(), sps.clone(), pps)),
        other => panic!("{other:?}"),
    }
    let r = VideoTrack::h265_from_key_frame(&annexb(&[&vps, &sps, &slice]));
    assert!(matches!(&r, Err(RtspError::NotAllowed { .. })), "{r:?}");
}

#[test]
fn setup_record_and_record_out_of_order_are_refused_and_nothing_is_written() {
    let (port, peer) = serve(|peer| {
        let announce = peer.request();
        peer.answer(&announce, "200 OK", "");
        peer.until_hangup().len()
    });
    let mut client = builder(port, "/live").connect().unwrap();
    let r = client.setup_record();
    assert!(matches!(&r, Err(RtspError::NotAllowed { .. })), "{r:?}");
    client.announce(&PushDescription::Mp2t).unwrap();
    let r = client.announce(&PushDescription::Mp2t);
    assert!(matches!(&r, Err(RtspError::NotAllowed { .. })), "{r:?}");
    let r = client.record();
    assert!(matches!(&r, Err(RtspError::NotAllowed { .. })), "{r:?}");
    assert_eq!(joined(peer), 0, "nothing follows the one ANNOUNCE");
}

#[test]
fn a_refused_request_on_a_cancelled_client_reports_the_cancel() {
    let (port, peer) = serve(|peer| {
        let announce = peer.request();
        peer.answer(&announce, "200 OK", "");
        peer.until_hangup().len()
    });
    let mut client = builder(port, "/live?transport=udp").connect().unwrap();
    client.announce(&PushDescription::Mp2t).unwrap();
    client.cancel_handle().cancel();
    // Refused by the client's thread, by the description, by the URL's
    // transport, and again by the client's thread.
    let r = client.announce(&PushDescription::Mp2t);
    assert!(matches!(&r, Err(RtspError::LocalCancel)), "{r:?}");
    let no_sets = PushDescription::Elementary {
        video: VideoTrack::H264 {
            parameter_sets: Vec::new(),
        },
        klv: false,
    };
    let r = client.announce(&no_sets);
    assert!(matches!(&r, Err(RtspError::LocalCancel)), "{r:?}");
    let r = client.setup_record();
    assert!(matches!(&r, Err(RtspError::LocalCancel)), "{r:?}");
    let r = client.record();
    assert!(matches!(&r, Err(RtspError::LocalCancel)), "{r:?}");
    assert_eq!(joined(peer), 0, "nothing follows the one ANNOUNCE");
}

#[test]
fn a_cancel_between_announce_and_setup_returns_local_cancel_and_leaves_no_thread() {
    let (port, peer) = serve(|peer| {
        let announce = peer.request();
        peer.answer(&announce, "200 OK", "");
        // The SETUP is written, as every request of a cancelled client
        // is; it gets no answer.
        peer.until_hangup()
    });
    let mut client = builder(port, "/live").connect().unwrap();
    client.announce(&PushDescription::Mp2t).unwrap();
    client.cancel_handle().cancel();
    let r = client.setup_record();
    assert!(matches!(&r, Err(RtspError::LocalCancel)), "{r:?}");
    let thread_holds = client.session_dead.clone();
    drop(client);
    eventually("the client's thread ended", || {
        Arc::strong_count(&thread_holds) == 1
    });
    let after = joined(peer);
    let methods: Vec<RtspMethod> = requests(&after).iter().map(|r| r.method).collect();
    assert_eq!(methods, [RtspMethod::Setup]);
}

/// The server answers the ANNOUNCE one byte at a time, `pause` apart, with
/// `pad` bytes of header: whether the client took the whole reply.
fn dribbled_announce(
    pad: usize,
    pause: Duration,
    timeout: Duration,
) -> (Result<(), RtspError>, Duration) {
    let (port, peer) = serve(move |peer| {
        let announce = peer.request();
        let headers = format!("X-Pad: {}\r\n", "x".repeat(pad));
        for byte in reply(&announce, "200 OK", &headers) {
            // The client leaves when its deadline passes.
            if peer.sock.write_all(&[byte]).is_err() {
                return;
            }
            std::thread::sleep(pause);
        }
        peer.until_hangup();
    });
    let mut client = builder(port, "/live")
        .request_timeout(Some(timeout))
        .connect()
        .unwrap();
    let t0 = Instant::now();
    let r = client.announce(&PushDescription::Mp2t);
    let took = t0.elapsed();
    drop(client);
    joined(peer);
    (r, took)
}

#[test]
fn an_announce_reply_dribbled_bytewise_completes_or_times_out_at_the_deadline() {
    let (r, _) = dribbled_announce(0, Duration::from_millis(1), Duration::from_secs(10));
    assert!(r.is_ok(), "{r:?}");

    // Ten seconds of dribble against a 300 ms deadline: the bytes that
    // keep coming do not move it.
    let deadline = Duration::from_millis(300);
    let (r, took) = dribbled_announce(2_000, Duration::from_millis(5), deadline);
    assert!(matches!(&r, Err(RtspError::Timeout)), "{r:?}");
    assert!(
        took >= deadline && took < deadline + scaled(Duration::from_secs(2)),
        "the announce returned after {took:?}"
    );
}

// ---- what the data calls write ----------------------------------------

#[test]
fn send_ts_is_one_rtp_packet_pt_33_per_call() {
    let (mut push, peer) = recording(&PushDescription::Mp2t);
    let payloads = [ts(7, 1), ts(1, 2), ts(3, 3)];
    for payload in &payloads {
        push.send_ts(payload).unwrap();
    }
    let stats = push.stats();
    assert_eq!(stats.packets_sent, 3);
    assert_eq!(stats.bytes_sent, (3 * 12 + 11 * 188) as u64);
    let (_, units) = closed(push, peer);
    let packets = on_channel(&units, 0);
    assert_eq!(packets.len(), 3);
    let (first, _) = rtp(packets[0]);
    for (i, (packet, payload)) in packets.iter().zip(&payloads).enumerate() {
        let (header, carried) = rtp(packet);
        assert_eq!(header.payload_type, 33);
        assert!(!header.marker);
        assert_eq!(header.ssrc, first.ssrc);
        assert_eq!(header.seq, first.seq.wrapping_add(i as u16));
        assert_eq!(carried, payload.as_slice());
    }
}

#[test]
fn an_access_unit_is_fu_a_fragments_of_at_most_1400_bytes_with_the_marker_on_the_last() {
    let (mut push, peer) = recording(&elementary(false));
    let idr = nal(&[0x65], 3_000);
    push.send_video(&annexb(&[SPS, PPS, &idr]), pts(90_000))
        .unwrap();
    let (_, units) = closed(push, peer);
    let packets: Vec<(RtpHeader, &[u8])> = on_channel(&units, 0).into_iter().map(rtp).collect();
    // The parameter sets whole, then the slice in three fragments.
    assert_eq!(packets.len(), 5);
    assert_eq!(packets[0].1, SPS);
    assert_eq!(packets[1].1, PPS);
    let mut slice = vec![0x65];
    for (i, (header, payload)) in packets.iter().enumerate() {
        assert_eq!(header.payload_type, 96);
        assert_eq!(header.timestamp, packets[0].0.timestamp);
        assert_eq!(header.seq, packets[0].0.seq.wrapping_add(i as u16));
        assert_eq!(header.marker, i == 4, "packet {i}");
        assert!(payload.len() <= 1_400, "packet {i}: {}", payload.len());
        if i >= 2 {
            // A fragment that is not the last is as large as allowed.
            assert!(i == 4 || payload.len() == 1_400, "packet {i}");
            // FU-A: the indicator, then start and end bits over the type.
            assert_eq!(payload[0], 0x60 | 28);
            let (start, end) = (payload[1] & 0x80 != 0, payload[1] & 0x40 != 0);
            assert_eq!((start, end), (i == 2, i == 4), "packet {i}");
            assert_eq!(payload[1] & 0x1F, 5);
            slice.extend_from_slice(&payload[2..]);
        }
    }
    assert_eq!(slice, idr);
}

#[test]
fn an_h265_push_announces_h265_and_fragments_its_access_units() {
    let (vps, sps, pps) = (nal(&[0x40, 1], 8), nal(&[0x42, 1], 8), nal(&[0x44, 1], 8));
    let video = VideoTrack::H265 {
        vps: vps.clone(),
        sps,
        pps,
    };
    let (mut push, peer) = recording(&PushDescription::Elementary { video, klv: false });
    let slice = nal(&[0x26, 1], 3_000);
    push.send_video(&annexb(&[&vps, &slice]), pts(0)).unwrap();
    let (handshake, units) = closed(push, peer);
    let body = String::from_utf8(handshake[0].body.to_vec()).unwrap();
    assert!(body.contains("a=rtpmap:96 H265/90000\r\n"), "{body}");
    let packets: Vec<(RtpHeader, &[u8])> = on_channel(&units, 0).into_iter().map(rtp).collect();
    assert_eq!(packets.len(), 4);
    assert_eq!(packets[0].1, vps);
    for (i, (header, payload)) in packets.iter().enumerate().skip(1) {
        // A fragmentation unit is NAL type 49.
        assert_eq!((payload[0] >> 1) & 0x3F, 49, "packet {i}");
        assert!(payload.len() <= 1_400);
        assert_eq!(header.marker, i == 3);
    }
}

#[test]
fn a_klv_unit_is_one_rtp_packet_with_the_marker() {
    let (mut push, peer) = recording(&elementary(true));
    // Far over a video packet's size, and the most one packet carries.
    let units_sent = [nal(&[0x06, 0x0E], 5_000), nal(&[0x06, 0x0E], 65_523)];
    for (i, unit) in units_sent.iter().enumerate() {
        push.send_klv(unit, pts(3_000 * i as i64)).unwrap();
    }
    let (_, units) = closed(push, peer);
    let packets: Vec<(RtpHeader, &[u8])> = on_channel(&units, 2).into_iter().map(rtp).collect();
    assert_eq!(packets.len(), 2);
    for ((header, payload), unit) in packets.iter().zip(&units_sent) {
        assert_eq!(header.payload_type, 97);
        assert!(header.marker);
        assert_eq!(*payload, unit.as_slice());
    }
    assert_eq!(
        packets[1].0.timestamp.wrapping_sub(packets[0].0.timestamp),
        3_000
    );
}

#[test]
fn a_klv_unit_over_one_frame_is_split_with_the_marker_on_its_last_packet() {
    let (mut push, peer) = recording(&elementary(true));
    let unit = nal(&[0x06, 0x0E], 65_524);
    push.send_klv(&unit, pts(0)).unwrap();
    let (_, units) = closed(push, peer);
    let packets: Vec<(RtpHeader, &[u8])> = on_channel(&units, 2).into_iter().map(rtp).collect();
    let markers: Vec<bool> = packets.iter().map(|(h, _)| h.marker).collect();
    assert_eq!(markers, [false, true]);
    assert_eq!([packets[0].1, packets[1].1].concat(), unit);
}

#[test]
fn a_two_mebibyte_access_unit_is_sent_whole() {
    let (mut push, peer) = recording(&elementary(false));
    let idr = nal(&[0x65], 2 * 1024 * 1024);
    push.send_video(&annexb(&[&idr]), pts(0)).unwrap();
    let (_, units) = closed(push, peer);
    let packets = on_channel(&units, 0);
    assert!(
        packets.len() > MEDIA_QUEUE_BOUND,
        "{} packets do not overrun the queue",
        packets.len()
    );
    let mut slice = vec![0x65];
    for (i, packet) in packets.iter().enumerate() {
        let (header, payload) = rtp(packet);
        assert_eq!(header.marker, i + 1 == packets.len());
        slice.extend_from_slice(&payload[2..]);
    }
    assert!(slice == idr, "the access unit did not arrive whole");
}

/// A recording push on which `refused` runs, and what the server then saw
/// behind the handshake: a refused call queues nothing.
fn nothing_is_sent_by(desc: &PushDescription, refused: impl FnOnce(&mut RtspPush)) {
    let (mut push, peer) = recording(desc);
    refused(&mut push);
    assert_eq!(push.stats().packets_sent, 0);
    assert_eq!(push.stats().sender_reports_sent, 0);
    assert!(push.is_session_alive());
    let (_, units) = closed(push, peer);
    assert!(frames(&units).is_empty(), "{:?}", frames(&units));
}

#[test]
fn send_ts_refuses_a_payload_over_1316_as_too_large() {
    nothing_is_sent_by(&PushDescription::Mp2t, |push| {
        let r = push.send_ts(&ts(8, 0));
        let too_large = TransportError::TooLarge {
            len: 1504,
            max: 1316,
        };
        assert_eq!(r, Err(too_large));
    });
}

#[test]
fn send_ts_refuses_a_misaligned_payload_as_broken() {
    nothing_is_sent_by(&PushDescription::Mp2t, |push| {
        let mut no_sync = ts(2, 0);
        no_sync[188] = 0x48;
        for bad in [&[][..], &ts(1, 0)[..187], &ts(2, 0)[..189], &no_sync[..]] {
            let e = push.send_ts(bad).unwrap_err();
            assert_eq!(
                broken_cause(&e),
                Some(BrokenCause::Unspecified),
                "{} bytes: {e:?}",
                bad.len()
            );
        }
        // A refused payload does not end the push.
        assert!(Transport::is_alive(&*push));
    });
}

#[test]
fn send_video_on_an_mp2t_push_is_no_such_track() {
    nothing_is_sent_by(&PushDescription::Mp2t, |push| {
        let r = push.send_video(&annexb(&[&nal(&[0x65], 20)]), pts(0));
        assert!(matches!(r, Err(PushError::NoSuchTrack("video"))), "{r:?}");
        let r = push.send_klv(&[1, 2, 3], pts(0));
        assert!(matches!(r, Err(PushError::NoSuchTrack("KLV"))), "{r:?}");
    });
}

#[test]
fn send_klv_without_a_klv_track_is_no_such_track() {
    nothing_is_sent_by(&elementary(false), |push| {
        let r = push.send_klv(&[1, 2, 3], pts(0));
        assert!(matches!(r, Err(PushError::NoSuchTrack("KLV"))), "{r:?}");
    });
}

#[test]
fn an_access_unit_with_no_nal_is_empty() {
    nothing_is_sent_by(&elementary(true), |push| {
        for no_nal in [&[][..], &[0, 0, 0, 1][..], &[9, 9, 9][..]] {
            let r = push.send_video(no_nal, pts(0));
            assert!(
                matches!(r, Err(PushError::Empty("access unit"))),
                "{no_nal:?}: {r:?}"
            );
        }
        let r = push.send_klv(&[], pts(0));
        assert!(matches!(r, Err(PushError::Empty("KLV unit"))), "{r:?}");
    });
}

#[test]
fn send_ts_on_an_elementary_push_is_broken() {
    nothing_is_sent_by(&elementary(true), |push| {
        let e = push.send_ts(&ts(7, 0)).unwrap_err();
        assert_eq!(broken_cause(&e), Some(BrokenCause::Unspecified), "{e:?}");
        let e = Transport::send_bytes(push, &ts(7, 0)).unwrap_err();
        assert_eq!(broken_cause(&e), Some(BrokenCause::Unspecified), "{e:?}");
    });
}

// ---- sender reports -----------------------------------------------------

#[test]
fn the_first_sender_report_precedes_the_first_rtp_packet_on_each_track() {
    let (mut push, peer) = recording(&elementary(true));
    let idr = nal(&[0x65], 100);
    push.send_video(&annexb(&[&idr]), pts(1_000)).unwrap();
    push.send_klv(&[7; 30], pts(1_000)).unwrap();
    push.send_video(&annexb(&[&idr]), pts(4_000)).unwrap();
    assert_eq!(push.stats().sender_reports_sent, 2);
    let (_, units) = closed(push, peer);
    let sent = frames(&units);
    let channels: Vec<u8> = sent.iter().map(|(c, _)| *c).collect();
    // Video on 0 and 1, KLV on 2 and 3; `close` ends with a report each.
    assert_eq!(channels, [1, 0, 3, 2, 0, 1, 3]);
    for (report, packet) in [(sent[0].1, sent[1].1), (sent[2].1, sent[3].1)] {
        let (report, (header, _)) = (sender_report(report), rtp(packet));
        assert_eq!(report.ssrc, header.ssrc);
        assert_eq!(report.rtp_timestamp, header.timestamp);
        assert_eq!(report.sender_packet_count, 0);
        assert_eq!(report.sender_octet_count, 0);
    }
    // The last report of a track counts what it sent and repeats the times
    // of its last packet.
    let (last, (header, _)) = (sender_report(sent[5].1), rtp(sent[4].1));
    assert_eq!(last.sender_packet_count, 2);
    assert_eq!(last.sender_octet_count, 200);
    assert_eq!(last.rtp_timestamp, header.timestamp);
    assert_eq!(sender_report(sent[6].1).sender_packet_count, 1);
}

#[test]
fn a_sender_report_follows_every_five_seconds_of_wall_time() {
    let (mut push, peer) = recording(&PushDescription::Mp2t);
    push.send_ts(&ts(1, 0)).unwrap();
    // Inside the interval, on a clock one second ahead: the seam adds to
    // the real clock, so the edge is kept four real seconds away.
    push.clock_skew = Duration::from_secs(1);
    push.send_ts(&ts(1, 0)).unwrap();
    assert_eq!(push.stats().sender_reports_sent, 1);
    push.clock_skew = Duration::from_secs(5);
    push.send_ts(&ts(1, 0)).unwrap();
    assert_eq!(push.stats().sender_reports_sent, 2);
    // The interval starts again at that report.
    push.clock_skew = Duration::from_secs(6);
    push.send_ts(&ts(1, 0)).unwrap();
    assert_eq!(push.stats().sender_reports_sent, 2);
    push.clock_skew = Duration::from_secs(10);
    push.send_ts(&ts(1, 0)).unwrap();
    let (_, units) = closed(push, peer);
    let channels: Vec<u8> = frames(&units).iter().map(|(c, _)| *c).collect();
    assert_eq!(channels, [1, 0, 0, 1, 0, 0, 1, 0, 1]);
    let reports: Vec<SenderReport> = on_channel(&units, 1)
        .into_iter()
        .map(sender_report)
        .collect();
    let counts: Vec<u32> = reports.iter().map(|r| r.sender_packet_count).collect();
    assert_eq!(counts, [0, 2, 4, 5]);
    assert_eq!(reports[1].sender_octet_count, 2 * 188);
}

#[test]
fn video_and_klv_reports_share_one_ntp_base() {
    let (mut push, peer) = recording(&elementary(true));
    // The KLV unit is 50 ms after the access unit.
    let (video_pts, klv_pts): (i64, i64) = (900_000, 904_500);
    push.send_video(&annexb(&[&nal(&[0x65], 100)]), pts(video_pts))
        .unwrap();
    push.send_klv(&[7; 30], pts(klv_pts)).unwrap();
    let (_, units) = closed(push, peer);
    let video = sender_report(on_channel(&units, 1)[0]);
    let klv = sender_report(on_channel(&units, 3)[0]);
    assert_ne!(video.ssrc, klv.ssrc);
    let (video_packet, _) = rtp(on_channel(&units, 0)[0]);
    let (klv_packet, _) = rtp(on_channel(&units, 2)[0]);
    assert_eq!(video.rtp_timestamp, video_packet.timestamp);
    assert_eq!(klv.rtp_timestamp, klv_packet.timestamp);
    // 50 ms in 32.32 fixed point.
    let fifty_ms = (((klv_pts - video_pts) as u64) << 32) / 90_000;
    assert_eq!(klv.ntp_timestamp - video.ntp_timestamp, fifty_ms);
    // The base is the wall clock: within a minute of now.
    let now = crate::rtcp::sender::ntp_from_system_time(std::time::SystemTime::now());
    assert!(now.abs_diff(video.ntp_timestamp) >> 32 < 60);
}

/// The timeline starts at the first data call, whichever track it is on.
#[test]
fn the_first_data_call_can_be_send_klv() {
    let (mut push, peer) = recording(&elementary(true));
    let (klv_pts, video_pts): (i64, i64) = (900_000, 904_500);
    push.send_klv(&[7; 30], pts(klv_pts)).unwrap();
    push.send_video(&annexb(&[&nal(&[0x65], 100)]), pts(video_pts))
        .unwrap();
    let (_, units) = closed(push, peer);
    let channels: Vec<u8> = frames(&units).iter().map(|(c, _)| *c).collect();
    // The KLV's report and packet, the video's, and the two last reports
    // in track order.
    assert_eq!(channels, [3, 2, 1, 0, 1, 3]);
    let klv = sender_report(on_channel(&units, 3)[0]);
    let video = sender_report(on_channel(&units, 1)[0]);
    assert_eq!(klv.rtp_timestamp, rtp(on_channel(&units, 2)[0]).0.timestamp);
    let fifty_ms = (((video_pts - klv_pts) as u64) << 32) / 90_000;
    assert_eq!(video.ntp_timestamp - klv.ntp_timestamp, fifty_ms);
    let now = crate::rtcp::sender::ntp_from_system_time(std::time::SystemTime::now());
    assert!(now.abs_diff(klv.ntp_timestamp) >> 32 < 60);
}

#[test]
fn an_mp2t_report_pairs_the_wall_clock_with_the_packets_timestamp() {
    let (mut push, peer) = recording(&PushDescription::Mp2t);
    push.send_ts(&ts(1, 0)).unwrap();
    let (_, units) = closed(push, peer);
    let report = sender_report(on_channel(&units, 1)[0]);
    let (packet, _) = rtp(on_channel(&units, 0)[0]);
    assert_eq!(report.rtp_timestamp, packet.timestamp);
    let now = crate::rtcp::sender::ntp_from_system_time(std::time::SystemTime::now());
    assert!(now.abs_diff(report.ntp_timestamp) >> 32 < 60);
}

#[test]
fn a_receiver_report_in_the_same_segment_as_the_setup_reply_is_counted() {
    let (port, peer) = serve(|peer| {
        let announce = peer.request();
        peer.answer(&announce, "200 OK", "");
        let setup = peer.request();
        let headers = "Session: abc\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n";
        let mut segment = reply(&setup, "200 OK", headers);
        segment.extend(interleaved(1, &receiver_report(1, 0, 0)));
        peer.sock.write_all(&segment).unwrap();
        let record = peer.request();
        peer.answer(&record, "200 OK", "");
        peer.until_teardown().len()
    });
    let push = RtspPush::connect(builder(port, "/live"), &PushDescription::Mp2t).unwrap();
    eventually("the receiver report counted", || {
        push.stats().receiver_reports == 1
    });
    push.close().unwrap();
    joined(peer);
}

#[test]
fn a_receiver_report_for_a_track_reaches_the_loss_figures() {
    let (port, peer) = serve(|peer| {
        peer.accept_push();
        // The track's first frame is its sender report, which names the
        // SSRC a report block has to name.
        let Some(Unit::Frame(1, report)) = peer.read_unit() else {
            panic!("expected the first sender report");
        };
        let ssrc = sender_report(&report).ssrc;
        let about_another = interleaved(1, &receiver_report(!ssrc, 200, 999));
        peer.sock.write_all(&about_another).unwrap();
        let about_ours = interleaved(1, &receiver_report(ssrc, 77, 5));
        peer.sock.write_all(&about_ours).unwrap();
        // A frame on the channel the client sends its media on.
        peer.sock.write_all(&interleaved(0, &[0; 20])).unwrap();
        peer.until_teardown().len()
    });
    let mut push = RtspPush::connect(builder(port, "/live"), &PushDescription::Mp2t).unwrap();
    push.send_ts(&ts(1, 0)).unwrap();
    eventually("both receiver reports and the stray frame", || {
        let stats = push.stats();
        stats.receiver_reports == 2 && stats.frames_dropped_inbound == 1
    });
    let stats = push.stats();
    assert_eq!((stats.fraction_lost_q8, stats.cumulative_lost), (77, 5));
    let socket = Transport::socket_stats(&push).unwrap();
    assert_eq!(socket.packets_lost_send, 5);
    assert_eq!(socket.packets_sent, 1);
    assert_eq!(socket.bytes_sent, 200);
    push.close().unwrap();
    joined(peer);
}

#[test]
fn the_loss_figures_are_those_of_the_track_whose_report_came_last() {
    let (second, gate) = mpsc::channel::<()>();
    let (port, peer) = serve(move |peer| {
        peer.accept_push();
        // Each track opens with its sender report, then its packet.
        let mut ssrcs = Vec::new();
        for _ in 0..4 {
            if let Some(Unit::Frame(1 | 3, report)) = peer.read_unit() {
                ssrcs.push(sender_report(&report).ssrc);
            }
        }
        let about_video = interleaved(1, &receiver_report(ssrcs[0], 10, 1));
        peer.sock.write_all(&about_video).unwrap();
        gate.recv_timeout(scaled(Duration::from_secs(20))).unwrap();
        let about_klv = interleaved(3, &receiver_report(ssrcs[1], 20, 2));
        peer.sock.write_all(&about_klv).unwrap();
        peer.until_teardown().len()
    });
    let mut push = RtspPush::connect(builder(port, "/live"), &elementary(true)).unwrap();
    push.send_video(&annexb(&[&nal(&[0x65], 50)]), pts(0))
        .unwrap();
    push.send_klv(&[7; 30], pts(0)).unwrap();
    eventually("the video's report", || push.stats().receiver_reports == 1);
    let stats = push.stats();
    assert_eq!((stats.fraction_lost_q8, stats.cumulative_lost), (10, 1));
    second.send(()).unwrap();
    eventually("the KLV's report", || push.stats().receiver_reports == 2);
    let stats = push.stats();
    assert_eq!((stats.fraction_lost_q8, stats.cumulative_lost), (20, 2));
    push.close().unwrap();
    joined(peer);
}

// ---- how a push ends ----------------------------------------------------

#[test]
fn close_after_an_accepted_teardown_is_clean_teardown() {
    let (mut push, peer) = recording(&PushDescription::Mp2t);
    push.send_ts(&ts(7, 0)).unwrap();
    let reason = push.client().end_reason.clone();
    let cancel = push.cancel_handle();
    assert_eq!(push.end_reason(), None);
    push.close().unwrap();
    assert_eq!(reason.get(), Some(EndReason::CleanTeardown));
    // Closing ends the client, whose handle reads cancelled from then on.
    assert!(cancel.is_cancelled());
    joined(peer);
}

#[test]
fn close_with_a_refused_teardown_returns_its_status_and_records_cancelled() {
    let (port, peer) = serve(|peer| {
        peer.accept_push();
        let teardown = peer.request();
        assert_eq!(teardown.method, RtspMethod::Teardown);
        // Without a `CSeq`, as one server refuses a method it lacks.
        let refusal = b"RTSP/1.0 501 Not Implemented\r\n\r\n";
        peer.sock.write_all(refusal).unwrap();
        peer.until_hangup().len()
    });
    let push = RtspPush::connect(builder(port, "/live"), &PushDescription::Mp2t).unwrap();
    let reason = push.client().end_reason.clone();
    let r = push.close();
    assert!(
        matches!(&r, Err(RtspError::Protocol { code: 501, .. })),
        "{r:?}"
    );
    assert_eq!(reason.get(), Some(EndReason::Cancelled));
    assert_eq!(joined(peer), 0);
}

#[test]
fn close_with_an_unanswered_teardown_times_out_and_records_cancelled() {
    let (port, peer) = serve(|peer| {
        peer.accept_push();
        peer.until_hangup().len()
    });
    let push = RtspPush::connect(builder(port, "/live"), &PushDescription::Mp2t).unwrap();
    let reason = push.client().end_reason.clone();
    let t0 = Instant::now();
    let r = push.close();
    assert!(matches!(&r, Err(RtspError::Timeout)), "{r:?}");
    // The TEARDOWN's 500 ms, not the request timeout's ten seconds.
    assert!(
        t0.elapsed() < scaled(Duration::from_secs(3)),
        "{:?}",
        t0.elapsed()
    );
    assert_eq!(reason.get(), Some(EndReason::Cancelled));
    assert_eq!(joined(peer), 1, "the TEARDOWN and nothing else");
}

#[test]
fn transport_close_with_a_refused_teardown_records_cancelled() {
    let (port, peer) = serve(|peer| {
        peer.accept_push();
        let teardown = peer.request();
        peer.answer(&teardown, "500 Internal Server Error", "");
        peer.until_hangup().len()
    });
    let mut push = RtspPush::connect(builder(port, "/live"), &PushDescription::Mp2t).unwrap();
    Transport::close(&mut push);
    assert_eq!(push.end_reason(), Some(EndReason::Cancelled));
    // Closed is not alive, also while the server still holds the
    // connection.
    assert!(!push.is_session_alive());
    assert_eq!(push.send_ts(&ts(7, 0)), Err(TransportError::Closed));
    drop(push);
    assert_eq!(joined(peer), 0, "one TEARDOWN, and none from the drop");
}

/// A KLV unit so large that a queue of them is far more than the
/// connection holds while the server reads nothing.
static BIG_UNIT: [u8; 60_000] = [0xAB; 60_000];

/// Send [`BIG_UNIT`]s until the queue is full, against a server that reads
/// nothing: whatever the connection still takes, most of them stay
/// queued. Returns the number sent.
///
/// The server has to read again within five seconds of the connection
/// taking its last byte, or the client ends the connection as stalled:
/// a test fills (some 60 MB of copies) and opens its gate well inside
/// that, also many times slower than natively.
fn fill_the_queue(push: &mut RtspPush) -> usize {
    let queue = push.client().driver.media_queue();
    let mut sent = 0;
    while queue.len() < MEDIA_QUEUE_BOUND {
        push.send_klv(&BIG_UNIT, pts(sent as i64)).unwrap();
        sent += 1;
    }
    sent
}

/// The stall rule would end the wait only after five seconds, so within
/// the bound here nothing but the cancel does.
#[test]
fn a_cancel_makes_a_parked_send_explicit_close_promptly() {
    let (mut push, open, peer) = recording_gated(&elementary(true), |peer| peer.until_hangup());
    let canceller = std::thread::spawn({
        let (queue, cancel) = (push.client().driver.media_queue(), push.cancel_handle());
        move || {
            eventually("the media queue full", || queue.len() == MEDIA_QUEUE_BOUND);
            // Pacing: the sender is parked by now, not between two sends.
            std::thread::sleep(Duration::from_millis(50));
            let at = Instant::now();
            cancel.cancel();
            at
        }
    });
    let refused = loop {
        if let Err(e) = push.send_klv(&BIG_UNIT, pts(0)) {
            break e;
        }
    };
    let returned = Instant::now();
    assert!(is_explicit_close(&refused), "{refused:?}");
    let cancelled = canceller.join().unwrap();
    let took = returned.saturating_duration_since(cancelled);
    assert!(
        took <= scaled(Duration::from_secs(1)),
        "the parked send returned {took:?} after the cancel"
    );
    // The caller's send recorded the end; the session itself is open.
    assert_eq!(push.end_reason(), Some(EndReason::Cancelled));
    assert!(push.is_session_alive());
    assert!(!Transport::is_alive(&push));
    let r = push.send_video(&annexb(&[&nal(&[0x65], 20)]), pts(0));
    assert!(r.as_ref().is_err_and(is_explicit_close), "{r:?}");
    // The cancel is reported ahead of a call the push would refuse anyway.
    assert_eq!(push.send_ts(&ts(7, 0)), Err(TransportError::ExplicitClose));
    let r = push.pause();
    assert!(matches!(&r, Err(RtspError::LocalCancel)), "{r:?}");
    // The drop gives its TEARDOWN half a second to be written: the server
    // is reading again before it starts.
    let queue = push.client().driver.media_queue();
    open.send(()).unwrap();
    eventually("the server reading again", || {
        queue.len() < MEDIA_QUEUE_BOUND
    });
    drop(push);
    // The cancelled pause wrote no PAUSE; the drop wrote its TEARDOWN.
    let units = joined(peer);
    let methods: Vec<RtspMethod> = requests(&units).iter().map(|r| r.method).collect();
    assert_eq!(methods, [RtspMethod::Teardown]);
}

#[test]
fn drop_drops_the_queue_and_writes_teardown() {
    let (mut push, open, peer) = recording_gated(&elementary(true), |peer| peer.until_hangup());
    fill_the_queue(&mut push);
    let queued = push.stats().packets_sent + push.stats().sender_reports_sent;
    let queue = push.client().driver.media_queue();
    let reason = push.client().end_reason.clone();
    // The server reads again, and the push is dropped with its queue all
    // but full.
    open.send(()).unwrap();
    eventually("the server reading again", || {
        queue.len() < MEDIA_QUEUE_BOUND
    });
    let t0 = Instant::now();
    drop(push);
    let took = t0.elapsed();
    // A drain would have had tens of megabytes to wait for.
    assert!(took < scaled(Duration::from_secs(2)), "Drop took {took:?}");
    assert_eq!(reason.get(), Some(EndReason::Cancelled));
    let units = joined(peer);
    let dropped = queue.dropped_at_close();
    assert!(dropped > 0, "nothing was dropped");
    // What was not dropped arrived, and the TEARDOWN behind it.
    assert_eq!(frames(&units).len() as u64 + dropped, queued);
    let Some(Unit::Request(last)) = units.last() else {
        panic!("the last thing written is not a request");
    };
    assert_eq!(last.method, RtspMethod::Teardown);
    assert_eq!(requests(&units).len(), 1);
}

/// The media queue can close with no reason recorded (the client's
/// thread is gone without having said why): a data call then reports the
/// push closed.
#[test]
fn a_closed_queue_with_no_reason_recorded_is_closed() {
    use crate::rtsp::client::media_queue::MediaWaitEnd;
    let (push, peer) = recording(&PushDescription::Mp2t);
    assert_eq!(push.end_reason(), None);
    assert_eq!(push.refusal(MediaWaitEnd::Ended), TransportError::Closed);
    // Reporting it recorded nothing.
    assert_eq!(push.end_reason(), None);
    closed(push, peer);
}

/// A push whose server took the handshake and then did `ending` to the
/// connection, once the client's thread has recorded the end.
fn ended_by(ending: impl FnOnce(&mut Peer) + Send + 'static) -> (RtspPush, JoinHandle<()>) {
    let (port, peer) = serve(move |peer| {
        peer.accept_push();
        ending(peer);
    });
    let push = RtspPush::connect(builder(port, "/live"), &PushDescription::Mp2t).unwrap();
    eventually("the client's thread recorded the end", || {
        push.end_reason().is_some()
    });
    (push, peer)
}

/// What `send_ts` returns once the end has reached the media queue. The
/// client's thread records the end before it closes the queue, so a send
/// in between is still taken; a refused one counts nothing.
fn refused_send(push: &mut RtspPush) -> TransportError {
    let give_up = Instant::now() + scaled(Duration::from_secs(5));
    loop {
        let before = push.stats().packets_sent;
        if let Err(e) = push.send_ts(&ts(7, 0)) {
            assert_eq!(push.stats().packets_sent, before, "a refused send counted");
            return e;
        }
        assert!(Instant::now() < give_up, "sends were still taken");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn the_peer_closing_gives_broken_clean_eof() {
    let (mut push, peer) = ended_by(|_| {});
    let e = refused_send(&mut push);
    assert_eq!(broken_cause(&e), Some(BrokenCause::CleanEof), "{e:?}");
    assert_eq!(push.end_reason(), Some(EndReason::CleanTeardown));
    assert!(!Transport::is_alive(&push));
    joined(peer);
}

/// The server resets only after it has read something the client sent
/// behind the handshake: a reset can discard what the other side has not
/// read yet, the RECORD's answer included.
#[test]
fn a_reset_gives_broken_unspecified() {
    let (port, peer) = serve(|peer| {
        peer.accept_push();
        peer.read_unit().expect("the client's first frame");
        // Closing with no linger time resets the connection.
        socket2::SockRef::from(&peer.sock)
            .set_linger(Some(Duration::ZERO))
            .unwrap();
    });
    let mut push = RtspPush::connect(builder(port, "/live"), &PushDescription::Mp2t).unwrap();
    push.send_ts(&ts(7, 0)).unwrap();
    eventually("the client's thread recorded the reset", || {
        push.end_reason().is_some()
    });
    let e = refused_send(&mut push);
    assert_eq!(broken_cause(&e), Some(BrokenCause::Unspecified), "{e:?}");
    assert!(
        matches!(push.end_reason(), Some(EndReason::TransportFailed { .. })),
        "{:?}",
        push.end_reason()
    );
    joined(peer);
}

#[test]
fn a_framing_violation_gives_broken_and_protocol_error() {
    let (mut push, peer) = ended_by(|peer| {
        let garbage = b"RTSP/1.0 200 OK\r\nContent-Length: many\r\n\r\n";
        peer.sock.write_all(garbage).unwrap();
        peer.until_hangup();
    });
    let e = refused_send(&mut push);
    assert_eq!(broken_cause(&e), Some(BrokenCause::Unspecified), "{e:?}");
    assert!(
        matches!(push.end_reason(), Some(EndReason::ProtocolError { .. })),
        "{:?}",
        push.end_reason()
    );
    drop(push);
    joined(peer);
}

#[test]
fn a_ping_answered_454_gives_broken_and_session_expired() {
    let (port, peer) = serve(|peer| {
        peer.accept_push();
        let ping = peer.request();
        assert_eq!(ping.method, RtspMethod::Options);
        peer.answer(&ping, "454 Session Not Found", "");
        peer.until_hangup();
    });
    let pinging = builder(port, "/live").keepalive_interval(Duration::from_millis(40));
    let mut push = RtspPush::connect(pinging, &PushDescription::Mp2t).unwrap();
    // The flag is stored after the reason: wait on the flag, then read it.
    eventually("the session dead", || !push.is_session_alive());
    assert_eq!(push.end_reason(), Some(EndReason::SessionExpired));
    let e = refused_send(&mut push);
    assert_eq!(broken_cause(&e), Some(BrokenCause::Unspecified), "{e:?}");
    assert_eq!(push.end_reason(), Some(EndReason::SessionExpired));
    drop(push);
    joined(peer);
}

/// On a session the server forgot, its connection still open, the client
/// refuses a pause and a resume itself: neither is written.
#[test]
fn pause_and_resume_on_an_expired_session_are_not_allowed() {
    let (port, peer) = serve(|peer| {
        peer.accept_push();
        let ping = peer.request();
        assert_eq!(ping.method, RtspMethod::Options);
        peer.answer(&ping, "454 Session Not Found", "");
        peer.until_hangup()
    });
    let pinging = builder(port, "/live").keepalive_interval(Duration::from_millis(40));
    let mut push = RtspPush::connect(pinging, &PushDescription::Mp2t).unwrap();
    eventually("the ping's answer recorded", || {
        push.end_reason() == Some(EndReason::SessionExpired)
    });
    let r = push.pause();
    assert!(matches!(r, Err(RtspError::NotAllowed { .. })), "{r:?}");
    let r = push.resume();
    assert!(matches!(r, Err(RtspError::NotAllowed { .. })), "{r:?}");
    drop(push);
    // Only the TEARDOWN of the drop followed the ping.
    let methods: Vec<RtspMethod> = joined(peer)
        .iter()
        .filter_map(|unit| match unit {
            Unit::Request(request) => Some(request.method),
            Unit::Frame(..) => None,
        })
        .collect();
    assert_eq!(methods, [RtspMethod::Teardown]);
}

#[test]
fn a_send_after_a_cancel_and_a_peer_close_is_explicit_close() {
    // The server closes first, then the caller cancels.
    let (mut push, peer) = ended_by(|_| {});
    push.cancel_handle().cancel();
    assert_eq!(push.send_ts(&ts(7, 0)), Err(TransportError::ExplicitClose));
    joined(peer);

    // The caller cancels first, then the server closes.
    let (close, closing) = mpsc::channel::<()>();
    let (port, peer) = serve(move |peer| {
        peer.accept_push();
        closing
            .recv_timeout(scaled(Duration::from_secs(20)))
            .unwrap();
    });
    let mut push = RtspPush::connect(builder(port, "/live"), &PushDescription::Mp2t).unwrap();
    push.cancel_handle().cancel();
    close.send(()).unwrap();
    eventually("the session dead", || !push.is_session_alive());
    assert!(push.end_reason().is_some());
    assert_eq!(push.send_ts(&ts(7, 0)), Err(TransportError::ExplicitClose));
    joined(peer);
}

#[test]
fn close_delivers_every_queued_frame_then_teardown() {
    let (mut push, open, peer) = recording_gated(&elementary(true), |peer| peer.until_teardown());
    let sent = fill_the_queue(&mut push);
    let queue = push.client().driver.media_queue();
    open.send(()).unwrap();
    push.close().unwrap();
    assert_eq!(queue.dropped_at_close(), 0);
    let units = joined(peer);
    let packets = on_channel(&units, 2);
    assert_eq!(packets.len(), sent);
    let (first, _) = rtp(packets[0]);
    for (i, packet) in packets.iter().enumerate() {
        assert_eq!(rtp(packet).0.seq, first.seq.wrapping_add(i as u16));
    }
    // Every frame, then the TEARDOWN.
    assert_eq!(units.len(), sent + 3, "two reports and the TEARDOWN");
    assert!(matches!(units.last(), Some(Unit::Request(r)) if r.method == RtspMethod::Teardown));
}

/// `close` on a session the server forgot while media was queued: a ping
/// is answered `454` with the queue full, the frames are dropped, and the
/// TEARDOWN that `close` still writes is answered `teardown_status`. With
/// no status the server answers nothing: it fires the push's cancel when
/// it reads the TEARDOWN, so the cancel lands after the close's wait for
/// the queue has ended, and hangs up.
fn close_after_expiry_dropped_media(
    teardown_status: Option<&'static str>,
) -> Result<(), RtspError> {
    closed_after_expiry_dropped_media(teardown_status, RtspPush::close).0
}

/// [`close_after_expiry_dropped_media`] with `close` as the way the push
/// is closed; returns what `close` did and the push's handle.
fn closed_after_expiry_dropped_media<T>(
    teardown_status: Option<&'static str>,
    close: impl FnOnce(RtspPush) -> T,
) -> (T, PushStatsHandle) {
    let (held, holding) = mpsc::channel::<()>();
    let (open, gate) = mpsc::channel::<RtspCancelHandle>();
    let (port, peer) = serve(move |peer| {
        peer.accept_push();
        // A ping is held unanswered while the client fills its queue
        // against a server that reads nothing.
        let ping = peer.request();
        assert_eq!(ping.method, RtspMethod::Options);
        held.send(()).unwrap();
        let cancel = gate
            .recv_timeout(scaled(Duration::from_secs(20)))
            .expect("the test opens the gate");
        // From here on the server knows no session: the held ping and
        // every later one are answered so.
        peer.answer(&ping, "454 Session Not Found", "");
        loop {
            let unit = peer.read_unit().expect("a TEARDOWN before the hang-up");
            let Unit::Request(request) = unit else {
                continue;
            };
            if request.method == RtspMethod::Teardown {
                match teardown_status {
                    Some(status) => peer.answer(&request, status, ""),
                    None => cancel.cancel(),
                }
                return;
            }
            peer.answer(&request, "454 Session Not Found", "");
        }
    });
    let pinging = builder(port, "/live").keepalive_interval(Duration::from_millis(40));
    let mut push = RtspPush::connect(pinging, &elementary(true)).unwrap();
    let handle = push.stats_handle();
    holding
        .recv_timeout(scaled(Duration::from_secs(5)))
        .expect("a ping reaches the server");
    fill_the_queue(&mut push);
    let queue = push.client().driver.media_queue();
    open.send(push.cancel_handle()).unwrap();
    // The flag is stored after the reason: wait on the flag, then read it.
    eventually("the session dead", || !push.is_session_alive());
    assert_eq!(push.end_reason(), Some(EndReason::SessionExpired));
    // The queue closes a moment after the session is marked dead.
    eventually("the queued frames dropped", || {
        push.stats().frames_dropped_at_close > 0
    });
    let dropped = push.stats().frames_dropped_at_close;
    assert_eq!(handle.stats().frames_dropped_at_close, dropped);
    let r = close(push);
    assert_eq!(queue.dropped_at_close(), dropped);
    // The handle keeps the loss readable once the push is gone.
    assert_eq!(handle.stats().frames_dropped_at_close, dropped);
    // The server read the TEARDOWN: its script ends there.
    joined(peer);
    (r, handle)
}

/// A session the server forgot while media was queued: the frames are
/// dropped, the TEARDOWN is still written and answered `454`, which ends
/// the session as asked, and `close` does not report that as success.
#[test]
fn close_after_a_ping_454_dropped_queued_media_is_an_error() {
    let r = close_after_expiry_dropped_media(Some("454 Session Not Found"));
    assert!(
        matches!(&r, Err(RtspError::Protocol { code: 454, .. })),
        "{r:?}"
    );
}

/// The loss is what `close` reports, whatever the server then makes of
/// the TEARDOWN: a refusal with another status does not take its place.
#[test]
fn close_reports_the_lost_media_whatever_the_teardown_is_answered() {
    let r = close_after_expiry_dropped_media(Some("501 Not Implemented"));
    assert!(
        matches!(&r, Err(RtspError::Protocol { code: 454, .. })),
        "{r:?}"
    );
}

/// The cancel comes first: a close that lost media and whose cancel has
/// fired by the time it returns reports the cancel, not the loss. Here the
/// cancel fires once the wait for the queue is over, while the TEARDOWN
/// waits for its reply.
#[test]
fn a_cancelled_close_that_lost_media_reports_the_cancel() {
    let r = close_after_expiry_dropped_media(None);
    assert!(matches!(&r, Err(RtspError::LocalCancel)), "{r:?}");
}

/// A frame announces its length in 16 bits: what does not fit is refused,
/// not cut.
#[test]
fn a_frame_over_the_16_bit_length_is_too_large() {
    use super::session::frame;
    let most = vec![0xA5u8; usize::from(u16::MAX) - 12];
    let whole = frame(7, &[&[1u8; 12], &most]).unwrap();
    assert_eq!(whole.len(), 4 + 65_535);
    assert_eq!(whole[..4], [b'$', 7, 0xFF, 0xFF]);
    assert_eq!(whole[4..16], [1u8; 12]);
    assert_eq!(whole[16..], most[..]);
    // One byte more, wherever it is.
    for parts in [
        [&[1u8; 13][..], &most[..]],
        [&[1u8; 12][..], &[most.as_slice(), &[0]].concat()[..]],
    ] {
        let r = frame(7, &parts);
        assert!(
            matches!(
                r,
                Err(TransportError::TooLarge {
                    len: 65_536,
                    max: 65_535
                })
            ),
            "{r:?}"
        );
    }
}

// ---- what close calls lost media -------------------------------------

mod lost {
    use super::super::session::{Queued, QueuedKinds, media_lost};
    use crate::rtsp::client::media_queue::{MEDIA_QUEUE_BOUND, MediaWaitEnd};
    use tst_core::transport::EndReason;

    use Queued::{Media, Report};

    fn noted(kinds: &[Queued]) -> QueuedKinds {
        let mut q = QueuedKinds::default();
        for k in kinds {
            q.note(*k);
        }
        q
    }

    /// `media_lost` for a session that expired while the close waited.
    fn expired(kinds: &[Queued], dropped: u64) -> bool {
        media_lost(
            Err(MediaWaitEnd::Ended),
            Some(EndReason::SessionExpired),
            &noted(kinds),
            dropped,
        )
    }

    #[test]
    fn nothing_dropped_is_nothing_lost() {
        assert!(!expired(&[Report, Media, Media, Report], 0));
    }

    #[test]
    fn only_the_closes_own_reports_dropped_is_nothing_lost() {
        // Two tracks: media, then the last report of each.
        assert!(!expired(&[Report, Media, Report, Media, Report, Report], 2));
    }

    #[test]
    fn media_dropped_with_the_reports_behind_it_is_lost() {
        assert!(expired(&[Report, Media, Media, Report], 2));
        assert!(expired(&[Report, Media, Media, Report], 4));
    }

    /// A paused push queues no last report at its close; the reports its
    /// pause queued are not media.
    #[test]
    fn only_a_pauses_reports_dropped_is_nothing_lost() {
        assert!(!expired(&[Report, Media, Media, Report, Report], 2));
    }

    /// A send queued its leading report and the queue then refused the
    /// packet: the caller saw that send fail, and the report is not media.
    #[test]
    fn the_report_of_a_refused_send_dropped_is_nothing_lost() {
        assert!(!expired(&[Report, Media, Report], 1));
    }

    #[test]
    fn a_close_ended_by_its_cancel_or_drained_reports_no_loss() {
        let q = noted(&[Report, Media, Media]);
        let expired = Some(EndReason::SessionExpired);
        assert!(!media_lost(
            Err(MediaWaitEnd::Cancelled),
            expired.clone(),
            &q,
            2
        ));
        assert!(!media_lost(Ok(()), expired, &q, 0));
    }

    /// Media dropped for another end is reported by the TEARDOWN that
    /// cannot be written, not as an expired session.
    #[test]
    fn media_dropped_for_another_end_is_not_an_expired_session() {
        let q = noted(&[Report, Media, Media]);
        for reason in [
            None,
            Some(EndReason::CleanTeardown),
            Some(EndReason::Cancelled),
        ] {
            assert!(!media_lost(Err(MediaWaitEnd::Ended), reason, &q, 2));
        }
    }

    /// The note keeps as many frames as the queue holds, so every frame
    /// the queue can drop is among them, and none older.
    #[test]
    fn the_note_keeps_as_many_frames_as_the_queue_holds() {
        let mut q = QueuedKinds::default();
        q.note(Media);
        for _ in 0..MEDIA_QUEUE_BOUND {
            q.note(Report);
        }
        assert_eq!(q.len(), MEDIA_QUEUE_BOUND);
        // The media frame fell out: the queue could not still hold it.
        assert!(!q.media_among_last(u64::MAX));
        q.note(Media);
        assert!(q.media_among_last(1));
        assert!(!q.media_among_last(0));
    }
}

/// The push notes each frame it queues as a report or media, also the
/// last reports of its close, and nothing for a call it refuses.
#[test]
fn the_push_notes_what_it_queues() {
    use super::session::Queued::{Media, Report};
    let (mut push, peer) = recording(&PushDescription::Mp2t);
    push.send_ts(&ts(7, 0)).unwrap();
    push.send_ts(&ts(7, 1)).unwrap();
    // Refused before anything is queued.
    assert!(push.send_ts(&ts(1, 0)[..100]).is_err());
    assert_eq!(push.queued.kinds(), [Report, Media, Media]);
    Transport::close(&mut push);
    assert_eq!(push.queued.kinds(), [Report, Media, Media, Report]);
    drop(push);
    joined(peer);
}

/// A session the server forgot after every frame was written: nothing was
/// lost, so `close` reports the TEARDOWN's answer, which accepts the end.
#[test]
fn close_after_a_ping_454_with_nothing_queued_is_the_teardowns_result() {
    let (port, peer) = serve(|peer| {
        peer.accept_push();
        // Pings are held until both frames of the one send were read, so
        // the session ends with nothing left to write.
        let mut frames = 0;
        let mut held = Vec::new();
        while frames < 2 || held.is_empty() {
            match peer.read_unit().expect("frames and a ping") {
                Unit::Frame(..) => frames += 1,
                Unit::Request(ping) => {
                    assert_eq!(ping.method, RtspMethod::Options);
                    held.push(ping);
                }
            }
        }
        for ping in &held {
            peer.answer(ping, "454 Session Not Found", "");
        }
        loop {
            let unit = peer.read_unit().expect("a TEARDOWN before the hang-up");
            let Unit::Request(request) = unit else {
                panic!("a frame after the session ended");
            };
            peer.answer(&request, "454 Session Not Found", "");
            if request.method == RtspMethod::Teardown {
                return;
            }
        }
    });
    let pinging = builder(port, "/live").keepalive_interval(Duration::from_millis(40));
    let mut push = RtspPush::connect(pinging, &PushDescription::Mp2t).unwrap();
    push.send_ts(&ts(7, 0)).unwrap();
    eventually("the ping's answer recorded", || {
        push.end_reason() == Some(EndReason::SessionExpired)
    });
    assert_eq!(push.stats().frames_dropped_at_close, 0);
    push.close().unwrap();
    joined(peer);
}

#[test]
fn transport_close_delivers_the_queue_tears_down_once_and_later_sends_are_closed() {
    let (mut push, open, peer) = recording_gated(&elementary(true), |peer| peer.until_teardown());
    let sent = fill_the_queue(&mut push);
    open.send(()).unwrap();
    Transport::close(&mut push);
    assert_eq!(push.end_reason(), Some(EndReason::CleanTeardown));
    assert!(!push.is_session_alive());
    // One answer from the control calls too, whether or not the stopped
    // thread has ended yet.
    for _ in 0..2 {
        let closed = "the push is closed";
        let r = push.pause();
        assert!(
            matches!(&r, Err(RtspError::NotAllowed { detail }) if *detail == closed),
            "{r:?}"
        );
        let r = push.resume();
        assert!(
            matches!(&r, Err(RtspError::NotAllowed { detail }) if *detail == closed),
            "{r:?}"
        );
        eventually("the client's thread ended", || {
            Arc::strong_count(&push.client().session_dead) == 1
        });
    }
    let r = push.send_klv(&[1, 2, 3], pts(0));
    assert!(
        matches!(r, Err(PushError::Transport(TransportError::Closed))),
        "{r:?}"
    );
    assert_eq!(push.send_ts(&ts(7, 0)), Err(TransportError::Closed));
    let r = Transport::send_bytes(&mut push, &ts(7, 0));
    assert_eq!(r, Err(TransportError::Closed));
    assert!(!Transport::is_alive(&push));
    assert!(Transport::socket_stats(&push).is_none());
    assert_eq!(push.stats().frames_dropped_at_close, 0);
    // The caller's own close is not a cancel.
    assert!(!push.cancel_handle().is_cancelled());
    let units = joined(peer);
    assert_eq!(on_channel(&units, 2).len(), sent);
    assert_eq!(units.len(), sent + 3, "two reports and the TEARDOWN");
    assert!(matches!(units.last(), Some(Unit::Request(r)) if r.method == RtspMethod::Teardown));
    // A second close and the consuming one write nothing: the server has
    // hung up, and neither fails on it.
    Transport::close(&mut push);
    push.close().unwrap();
}

#[test]
fn pause_drains_then_sends_pause_and_nothing_follows_it() {
    let (mut push, open, peer) = recording_gated(&elementary(true), |peer| peer.until_teardown());
    let sent = fill_the_queue(&mut push);
    open.send(()).unwrap();
    push.pause().unwrap();
    // A second pause is refused and queues no report.
    let r = push.pause();
    assert!(matches!(&r, Err(RtspError::NotAllowed { .. })), "{r:?}");
    assert_eq!(push.stats().sender_reports_sent, 2);
    push.close().unwrap();
    let units = joined(peer);
    // Every packet and the track's last report, then PAUSE, then nothing
    // but the TEARDOWN.
    assert_eq!(units.len(), sent + 4);
    assert_eq!(on_channel(&units[..sent + 2], 2).len(), sent);
    assert!(matches!(&units[sent + 1], Unit::Frame(3, _)));
    let tail = requests(&units[sent + 2..]);
    let tail: Vec<RtspMethod> = tail.iter().map(|r| r.method).collect();
    assert_eq!(tail, [RtspMethod::Pause, RtspMethod::Teardown]);
}

#[test]
fn resume_records_again_and_a_frame_sent_while_paused_is_written() {
    let (mut push, peer) = recording(&PushDescription::Mp2t);
    push.send_ts(&ts(1, 1)).unwrap();
    push.pause().unwrap();
    push.send_ts(&ts(1, 2)).unwrap();
    // A request goes ahead of queued media: the RECORD waits here until
    // the client's thread has taken the packet.
    let queue = push.client().driver.media_queue();
    eventually("the packet sent while paused taken", || queue.len() == 0);
    push.resume().unwrap();
    push.send_ts(&ts(1, 3)).unwrap();
    let (_, units) = closed(push, peer);
    let shape: Vec<String> = units
        .iter()
        .map(|unit| match unit {
            Unit::Request(r) => format!("{:?}", r.method),
            Unit::Frame(channel, _) => channel.to_string(),
        })
        .collect();
    assert_eq!(
        shape,
        ["1", "0", "1", "Pause", "0", "Record", "0", "1", "Teardown"]
    );
}

#[test]
fn resume_while_recording_is_refused_locally() {
    let (mut push, peer) = recording(&PushDescription::Mp2t);
    let r = push.resume();
    assert!(matches!(&r, Err(RtspError::NotAllowed { .. })), "{r:?}");
    assert!(push.is_session_alive());
    push.send_ts(&ts(1, 0)).unwrap();
    let (_, units) = closed(push, peer);
    let methods: Vec<RtspMethod> = requests(&units).iter().map(|r| r.method).collect();
    assert_eq!(methods, [RtspMethod::Teardown]);
}

#[test]
fn a_pause_answered_454_is_protocol_and_the_session_stays() {
    let (port, peer) = serve(|peer| {
        peer.accept_push();
        let pause = peer.request();
        peer.answer(&pause, "454 Session Not Found", "");
        peer.until_teardown()
    });
    let mut push = RtspPush::connect(builder(port, "/live"), &PushDescription::Mp2t).unwrap();
    let r = push.pause();
    assert!(
        matches!(&r, Err(RtspError::Protocol { code: 454, .. })),
        "{r:?}"
    );
    assert!(push.is_session_alive());
    assert_eq!(push.end_reason(), None);
    push.send_ts(&ts(1, 0)).unwrap();
    push.close().unwrap();
    let units = joined(peer);
    assert_eq!(on_channel(&units, 0).len(), 1);
}

#[test]
fn the_transport_impl_reports_max_payload_1316_and_is_alive() {
    let (mut push, peer) = recording(&PushDescription::Mp2t);
    assert_eq!(PUSH_MP2T_MAX_PAYLOAD, 1316);
    assert_eq!(Transport::max_payload(&push), 1316);
    assert!(Transport::is_alive(&push));
    // The owner of a type-erased shell reaches the push itself.
    let any = Transport::as_any(&push).expect("the push opts in");
    assert_eq!(
        any.downcast_ref::<RtspPush>().map(RtspPush::session_id),
        Some(SESSION)
    );
    Transport::send_bytes(&mut push, &ts(7, 9)).unwrap();
    let handle = Transport::cancel_handle(&push).expect("a cancel handle");
    assert!(!handle.is_cancelled());
    let stats = Transport::socket_stats(&push).unwrap();
    assert_eq!((stats.packets_sent, stats.bytes_sent), (1, 12 + 1316));
    // The trait's handle is the push's own.
    handle.cancel();
    assert!(push.cancel_handle().is_cancelled());
    assert!(!Transport::is_alive(&push));
    drop(push);
    joined(peer);
}

// ---- the stats handle -------------------------------------------------

// The handle is read from threads the push is not on.
const _: fn() = || {
    fn sync<T: Sync + Send + Clone + std::fmt::Debug>() {}
    sync::<super::PushStatsHandle>();
    sync::<super::PushCloseOutcome>();
};

/// A send parks against a server that reads nothing: the handle answers on
/// another thread while the sending thread still holds the push.
#[test]
fn the_stats_handle_answers_while_a_send_holds_the_push() {
    let (mut push, open, peer) = recording_gated(&elementary(true), |peer| peer.until_hangup());
    let handle = push.stats_handle();
    let queue = push.client().driver.media_queue();
    let cancel = push.cancel_handle();
    let sender = std::thread::spawn(move || {
        let refused = loop {
            if let Err(e) = push.send_klv(&BIG_UNIT, pts(0)) {
                break e;
            }
        };
        (refused, push)
    });
    eventually("the media queue full", || queue.len() == MEDIA_QUEUE_BOUND);
    eventually("the send parked on the queue", || queue.parked() == 1);
    let (tx, rx) = mpsc::channel();
    let reader = {
        let handle = handle.clone();
        std::thread::spawn(move || {
            let _ = tx.send((handle.stats(), handle.end_reason()));
        })
    };
    let (stats, reason) = rx
        .recv_timeout(scaled(Duration::from_secs(5)))
        .expect("the handle answered while the send held the push");
    reader.join().unwrap();
    assert!(!sender.is_finished(), "the send still holds the push");
    assert!(stats.packets_sent >= MEDIA_QUEUE_BOUND as u64, "{stats:?}");
    assert_eq!(reason, None);

    cancel.cancel();
    let (refused, push) = sender.join().unwrap();
    assert!(is_explicit_close(&refused), "{refused:?}");
    assert_eq!(handle.stats(), push.stats());
    assert_eq!(handle.end_reason(), Some(EndReason::Cancelled));
    let queue = push.client().driver.media_queue();
    open.send(()).unwrap();
    eventually("the server reading again", || {
        queue.len() < MEDIA_QUEUE_BOUND
    });
    drop(push);
    joined(peer);
}

/// After `close` the handle reads the push's last counters: what the server
/// was sent, the close's own sender report included.
#[test]
fn the_stats_handle_answers_after_close_with_the_last_values() {
    let (mut push, peer) = recording(&PushDescription::Mp2t);
    let handle = push.stats_handle();
    for i in 0..3 {
        push.send_ts(&ts(7, i)).unwrap();
    }
    let before = push.stats();
    let (_, units) = closed(push, peer);
    let after = handle.stats();
    assert_eq!(after.packets_sent, before.packets_sent);
    assert_eq!(after.bytes_sent, before.bytes_sent);
    assert!(
        after.sender_reports_sent > before.sender_reports_sent,
        "{after:?}"
    );
    assert_eq!(
        frames(&units).len() as u64,
        after.packets_sent + after.sender_reports_sent,
        "every counted frame reached the server"
    );
    assert_eq!(after.frames_dropped_at_close, 0);
    assert_eq!(handle.end_reason(), Some(EndReason::CleanTeardown));
}

/// `RtspPush::stats` and the handle read the same counters, the RTCP loss
/// figures and the dropped inbound frames included.
#[test]
fn stats_and_the_handle_agree() {
    let (port, peer) = serve(|peer| {
        peer.accept_push();
        let Some(Unit::Frame(1, report)) = peer.read_unit() else {
            panic!("expected the first sender report");
        };
        let ssrc = sender_report(&report).ssrc;
        peer.sock
            .write_all(&interleaved(1, &receiver_report(ssrc, 77, 5)))
            .unwrap();
        peer.sock.write_all(&interleaved(0, &[0; 20])).unwrap();
        peer.until_teardown().len()
    });
    let mut push = RtspPush::connect(builder(port, "/live"), &PushDescription::Mp2t).unwrap();
    let handle = push.stats_handle();
    push.send_ts(&ts(1, 0)).unwrap();
    eventually("the receiver report and the stray frame", || {
        let stats = handle.stats();
        stats.receiver_reports == 1 && stats.frames_dropped_inbound == 1
    });
    let stats = handle.stats();
    assert_eq!((stats.fraction_lost_q8, stats.cumulative_lost), (77, 5));
    assert_eq!(stats.packets_sent, 1);
    assert_eq!(stats, push.stats());
    assert_eq!(
        push.stats_handle().stats(),
        stats,
        "every handle reads the same"
    );
    assert_eq!(handle.end_reason(), push.end_reason());
    push.close().unwrap();
    joined(peer);
}

/// The server closes the connection: the handle names the end, also once
/// the push is gone.
#[test]
fn end_reason_through_the_handle_after_a_server_close() {
    let (mut push, peer) = ended_by(|_| {});
    let handle = push.stats_handle();
    assert_eq!(handle.end_reason(), Some(EndReason::CleanTeardown));
    refused_send(&mut push);
    assert_eq!(handle.stats(), push.stats());
    drop(push);
    assert_eq!(handle.end_reason(), Some(EndReason::CleanTeardown));
    joined(peer);
}

// ---- how a close ended, through the handle ------------------------------

/// A muxing shell over `push`, as a caller builds one.
fn shell(push: RtspPush) -> tst_pipeline::MuxSender<RtspPush> {
    use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
    let mut program = MuxerProgramConfigBuilder::new(1, 0x1000);
    program.add_video(0x1011, VideoCodec::H264);
    let mut config = MuxerConfig::builder();
    config.add_program(program.build());
    tst_pipeline::MuxSender::new(push, config.build().unwrap()).unwrap()
}

/// A recording push whose server answers the TEARDOWN with `status`, or
/// with nothing when there is none, and reads on until the hang-up.
fn recording_whose_teardown_gets(status: Option<&'static str>) -> (RtspPush, JoinHandle<()>) {
    let (port, peer) = serve(move |peer| {
        peer.accept_push();
        loop {
            let unit = peer.read_unit().expect("a TEARDOWN before the hang-up");
            let Unit::Request(request) = unit else {
                continue;
            };
            if request.method == RtspMethod::Teardown {
                if let Some(status) = status {
                    peer.answer(&request, status, "");
                }
                break;
            }
            peer.answer(&request, "200 OK", "");
        }
        peer.until_hangup();
    });
    let push = RtspPush::connect(builder(port, "/live"), &PushDescription::Mp2t).unwrap();
    (push, peer)
}

fn refused(code: u16, reason: &str) -> Option<PushCloseOutcome> {
    Some(PushCloseOutcome::TeardownRefused {
        code,
        reason: reason.to_string(),
    })
}

/// Whether `result`, what [`RtspPush::close`] returned, and `outcome`
/// describe the same case.
fn same_case(result: &Result<(), RtspError>, outcome: &PushCloseOutcome) -> bool {
    use PushCloseOutcome as O;
    match (result, outcome) {
        (Ok(()), O::Clean) => true,
        (Err(RtspError::LocalCancel), O::Cancelled) => true,
        (Err(RtspError::Timeout), O::TeardownTimedOut) => true,
        // A TEARDOWN answered `454` is accepted: as an error it is the loss.
        (Err(RtspError::Protocol { code: 454, .. }), O::MediaLost) => true,
        (Err(RtspError::Protocol { code, reason }), O::TeardownRefused { code: c, reason: r }) => {
            *code != 454 && code == c && reason == r
        }
        (
            Err(RtspError::LocalCancel | RtspError::Timeout | RtspError::Protocol { .. }),
            O::Failed { .. },
        ) => false,
        (Err(e), O::Failed { detail }) => *detail == e.to_string(),
        _ => false,
    }
}

#[test]
fn the_close_outcome_is_none_before_a_close() {
    let (mut push, peer) = recording(&PushDescription::Mp2t);
    let handle = push.stats_handle();
    assert_eq!(handle.close_outcome(), None);
    push.send_ts(&ts(7, 0)).unwrap();
    // A cancel ends the data calls; it is not a close.
    push.cancel_handle().cancel();
    assert_eq!(push.send_ts(&ts(7, 0)), Err(TransportError::ExplicitClose));
    assert_eq!(push.end_reason(), Some(EndReason::Cancelled));
    assert_eq!(handle.close_outcome(), None);
    // Nor is dropping the push, though it writes a TEARDOWN.
    drop(push);
    joined(peer);
    assert_eq!(handle.close_outcome(), None);

    // Nor the session's end from the server's side.
    let (push, peer) = ended_by(|_| {});
    let handle = push.stats_handle();
    assert_eq!(handle.end_reason(), Some(EndReason::CleanTeardown));
    assert_eq!(handle.close_outcome(), None);
    drop(push);
    joined(peer);
}

#[test]
fn a_clean_shell_close_records_clean() {
    let (push, peer) = recording(&PushDescription::Mp2t);
    let handle = push.stats_handle();
    let sender = shell(push);
    sender.finish().unwrap();
    assert_eq!(handle.close_outcome(), Some(PushCloseOutcome::Clean));
    assert_eq!(handle.end_reason(), Some(EndReason::CleanTeardown));
    joined(peer);
}

#[test]
fn a_shell_close_with_a_refused_teardown_records_the_refusal_and_its_status() {
    let (push, peer) = recording_whose_teardown_gets(Some("500 Internal Server Error"));
    let handle = push.stats_handle();
    let sender = shell(push);
    // The shell's own result has no place for the server's answer.
    sender.finish().unwrap();
    assert_eq!(
        handle.close_outcome(),
        refused(500, "Internal Server Error")
    );
    // The end reason alone does not tell a refusal from a cancel.
    assert_eq!(handle.end_reason(), Some(EndReason::Cancelled));
    drop(sender);
    joined(peer);
}

/// A session the server forgot while media was queued, closed through a
/// shell: the loss is recorded, whatever the TEARDOWN is answered.
#[test]
fn a_shell_close_after_a_forgotten_session_records_the_lost_media() {
    for status in ["454 Session Not Found", "501 Not Implemented"] {
        let (finished, handle) = closed_after_expiry_dropped_media(Some(status), |push| {
            let sender = shell(push);
            sender.finish()
        });
        finished.unwrap();
        assert_eq!(
            handle.close_outcome(),
            Some(PushCloseOutcome::MediaLost),
            "{status}"
        );
        assert_eq!(handle.end_reason(), Some(EndReason::SessionExpired));
    }
}

/// The cancel is set before the close, never during it.
#[test]
fn a_cancelled_close_records_the_cancel() {
    // The transport's close under a cancel the caller fired.
    let (mut push, peer) = recording_whose_teardown_gets(None);
    let handle = push.stats_handle();
    push.cancel_handle().cancel();
    Transport::close(&mut push);
    assert_eq!(handle.close_outcome(), Some(PushCloseOutcome::Cancelled));
    drop(push);
    joined(peer);

    // A muxing shell's `close` fires the cancel itself, then closes.
    let (push, peer) = recording_whose_teardown_gets(Some("200 OK"));
    let handle = push.stats_handle();
    let sender = shell(push);
    sender.close();
    assert_eq!(handle.close_outcome(), Some(PushCloseOutcome::Cancelled));
    drop(sender);
    joined(peer);
}

/// Each shape of a close, through the inherent `close`: what it returns
/// and the outcome it leaves describe the same case.
#[test]
fn the_inherent_close_and_the_outcome_agree() {
    let check = |result: Result<(), RtspError>,
                 handle: &PushStatsHandle,
                 expected: Option<PushCloseOutcome>| {
        let outcome = handle.close_outcome().expect("a close has run");
        assert!(same_case(&result, &outcome), "{result:?} and {outcome:?}");
        if let Some(expected) = expected {
            assert_eq!(outcome, expected);
        }
    };
    let close = |status: Option<&'static str>, cancelled: bool| {
        let (push, peer) = recording_whose_teardown_gets(status);
        let handle = push.stats_handle();
        if cancelled {
            push.cancel_handle().cancel();
        }
        let result = push.close();
        joined(peer);
        (result, handle)
    };

    // Accepted, by `200` and by `454` (the session is gone, as asked).
    for status in ["200 OK", "454 Session Not Found"] {
        let (result, handle) = close(Some(status), false);
        assert!(result.is_ok(), "{status}: {result:?}");
        check(result, &handle, Some(PushCloseOutcome::Clean));
    }
    // Refused.
    let (result, handle) = close(Some("501 Not Implemented"), false);
    check(result, &handle, refused(501, "Not Implemented"));
    // Not answered within the TEARDOWN's bound.
    let (result, handle) = close(None, false);
    check(result, &handle, Some(PushCloseOutcome::TeardownTimedOut));
    // Cancelled before the close.
    let (result, handle) = close(None, true);
    check(result, &handle, Some(PushCloseOutcome::Cancelled));
    // The connection had ended: the TEARDOWN cannot be written.
    let (push, peer) = ended_by(|_| {});
    let handle = push.stats_handle();
    let result = push.close();
    joined(peer);
    assert!(matches!(result, Err(RtspError::Io(_))), "{result:?}");
    assert!(
        matches!(
            handle.close_outcome(),
            Some(PushCloseOutcome::Failed { .. })
        ),
        "{:?}",
        handle.close_outcome()
    );
    check(result, &handle, None);
    // Media lost to a session the server forgot.
    let (result, handle) =
        closed_after_expiry_dropped_media(Some("454 Session Not Found"), RtspPush::close);
    check(result, &handle, Some(PushCloseOutcome::MediaLost));
}

#[test]
fn the_first_close_outcome_is_kept() {
    let (mut push, peer) = recording_whose_teardown_gets(Some("500 Internal Server Error"));
    let handle = push.stats_handle();
    Transport::close(&mut push);
    assert_eq!(
        handle.close_outcome(),
        refused(500, "Internal Server Error")
    );
    // A second close does nothing and reports nothing new.
    Transport::close(&mut push);
    assert_eq!(
        handle.close_outcome(),
        refused(500, "Internal Server Error")
    );
    // Nor does the inherent close of a push that is closed already: it has
    // no session left to end, so it succeeds.
    RtspPush::close(push).unwrap();
    assert_eq!(
        handle.close_outcome(),
        refused(500, "Internal Server Error")
    );
    joined(peer);
}

#[test]
fn the_outcome_answers_after_the_push_was_dropped() {
    let (push, peer) = recording(&PushDescription::Mp2t);
    let handle = push.stats_handle();
    let later = push.stats_handle();
    let sender = shell(push);
    sender.finish().unwrap();
    drop(sender);
    joined(peer);
    assert_eq!(handle.close_outcome(), Some(PushCloseOutcome::Clean));
    // Every handle reads the one outcome, clones included.
    assert_eq!(later.close_outcome(), Some(PushCloseOutcome::Clean));
    assert_eq!(
        handle.clone().close_outcome(),
        Some(PushCloseOutcome::Clean)
    );
}

/// A close parked on a server that reads nothing: the handle answers on
/// another thread, with no outcome yet, while the close still holds the
/// push.
#[test]
fn the_close_outcome_answers_none_while_the_close_holds_the_push() {
    let (mut push, open, peer) = recording_gated(&elementary(true), |peer| peer.until_teardown());
    fill_the_queue(&mut push);
    let handle = push.stats_handle();
    let queue = push.client().driver.media_queue();
    let closer = std::thread::spawn(move || push.close());
    // The close is inside its wait for the queue, and stays there until
    // the server reads.
    eventually("the close parked on the queue", || queue.parked() == 1);
    let (tx, rx) = mpsc::channel();
    let reader = {
        let handle = handle.clone();
        std::thread::spawn(move || {
            let _ = tx.send(handle.close_outcome());
        })
    };
    let outcome = rx
        .recv_timeout(scaled(Duration::from_secs(5)))
        .expect("the handle answered while the close held the push");
    reader.join().unwrap();
    assert!(!closer.is_finished(), "the close still holds the push");
    assert_eq!(outcome, None);

    open.send(()).unwrap();
    closer.join().unwrap().unwrap();
    assert_eq!(handle.close_outcome(), Some(PushCloseOutcome::Clean));
    joined(peer);
}
