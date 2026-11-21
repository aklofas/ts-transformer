use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::RecvTimeoutError;

use tst_core::transport::EndReason;
use tst_test_helpers::timing::scaled;

use crate::rtsp::auth::AuthSchemes;
use crate::rtsp::client::engine::{
    Credentials, EngineConfig, RouteKind, SetupAccepted, SetupRoute,
};
use crate::rtsp::message::{RtspMethod, RtspRequest, RtspResponse};
use crate::url::RtspVersion;

const URI: &str = "rtsp://127.0.0.1/x";

fn engine() -> ClientEngine {
    engine_with(None)
}

fn engine_with(credentials: Option<Credentials>) -> ClientEngine {
    ClientEngine::new(EngineConfig {
        uri: URI.to_string(),
        version: RtspVersion::V1_0,
        user_agent: "tst-rtp/test".to_string(),
        credentials,
        schemes: AuthSchemes::Automatic,
    })
}

fn options() -> RequestSpec {
    RequestSpec {
        method: RtspMethod::Options,
        uri: URI.to_string(),
        headers: Vec::new(),
        body: None,
        announce_tracks: None,
        setup: None,
    }
}

/// The driver under test with clones of what it shares.
struct Harness {
    handle: DriverHandle,
    cancel: FlagCancel,
    session_dead: Arc<AtomicBool>,
    end_reason: EndReasonHandle,
    media_dropped: Arc<AtomicU64>,
    frames_dropped_inbound: Arc<AtomicU64>,
    /// The queue of outbound frames `handle` sends through.
    media: Arc<MediaQueue>,
    /// The thread's `Shared::pass_gate`.
    pass_gate: PassGate,
    /// The thread's `Shared::cnonces_drawn`.
    cnonces_drawn: Arc<AtomicU64>,
}

fn drive(stream: Stream) -> Harness {
    drive_with(stream, CONTROL_WRITE_TIMEOUT)
}

fn drive_with(stream: Stream, write_stall: Duration) -> Harness {
    drive_engine(stream, engine(), write_stall)
}

fn drive_engine(stream: Stream, engine: ClientEngine, write_stall: Duration) -> Harness {
    let shared = Shared {
        cancel: FlagCancel::new(),
        session_dead: Arc::new(AtomicBool::new(false)),
        end_reason: EndReasonHandle::default(),
        media_dropped: Arc::new(AtomicU64::new(0)),
        frames_dropped_inbound: Arc::new(AtomicU64::new(0)),
        exit_gate: None,
        pass_gate: Arc::default(),
        cnonces_drawn: Arc::default(),
    };
    let cancel = shared.cancel.clone();
    let pass_gate = shared.pass_gate.clone();
    let cnonces_drawn = shared.cnonces_drawn.clone();
    let session_dead = shared.session_dead.clone();
    let end_reason = shared.end_reason.clone();
    let media_dropped = shared.media_dropped.clone();
    let frames_dropped_inbound = shared.frames_dropped_inbound.clone();
    let handle = spawn_with(stream, engine, shared, write_stall).unwrap();
    Harness {
        cancel,
        session_dead,
        end_reason,
        media_dropped,
        frames_dropped_inbound,
        media: handle.media_queue(),
        pass_gate,
        cnonces_drawn,
        handle,
    }
}

impl Harness {
    fn request(&self, spec: RequestSpec) -> mpsc::Receiver<Progress> {
        self.request_routed(spec, None)
    }

    fn request_routed(
        &self,
        spec: RequestSpec,
        route: Option<RouteSpec>,
    ) -> mpsc::Receiver<Progress> {
        let (progress, rx) = mpsc::channel();
        self.handle
            .send(Command::Request {
                spec,
                route,
                reply_deadline: None,
                progress,
            })
            .unwrap();
        rx
    }

    /// Send a SETUP for route `id` whose sinks are `media` and a fresh
    /// stats cell (reports are matched against SSRC `0x0102_0304`).
    fn setup(
        &self,
        id: u8,
        kind: RouteKind,
        media: Option<mpsc::SyncSender<Bytes>>,
    ) -> (mpsc::Receiver<Progress>, Arc<Mutex<RtcpStats>>) {
        let stats = Arc::new(Mutex::new(RtcpStats::default()));
        let rx = self.request_routed(
            RequestSpec {
                method: RtspMethod::Setup,
                uri: URI.to_string(),
                headers: Vec::new(),
                body: None,
                announce_tracks: None,
                setup: Some(SetupRoute {
                    id: RouteId(id),
                    kind,
                }),
            },
            Some(RouteSpec {
                id: RouteId(id),
                inbound_media: media,
                inbound_rtcp: RtcpSink {
                    stats: stats.clone(),
                    ssrc: OUR_SSRC,
                },
            }),
        );
        (rx, stats)
    }

    /// Parks the client's thread at the top of its loop, whatever passes it
    /// ran before, and returns what lets it go: until then it runs no pass.
    /// Dropping the sender lets it go too, so a failing test does not leave
    /// the thread held.
    fn hold_between_passes(&self) -> mpsc::Sender<()> {
        let (held_tx, held) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        *self.pass_gate.lock().unwrap() = Some((held_tx, release_rx));
        self.handle.wake();
        held.recv_timeout(scaled(Duration::from_secs(5)))
            .expect("the client's thread reached the top of its loop");
        release
    }

    fn wait_end(&self) -> EndReason {
        wait_until("an end reason", || self.end_reason.get().is_some());
        self.end_reason.get().unwrap()
    }

    fn stop_and_join(mut self) -> EndReasonHandle {
        self.handle.stop();
        assert!(self.handle.join_within(scaled(Duration::from_secs(2))));
        self.end_reason.clone()
    }
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + scaled(Duration::from_secs(5));
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn next(rx: &mpsc::Receiver<Progress>) -> Progress {
    rx.recv_timeout(scaled(Duration::from_secs(5)))
        .expect("progress within the deadline")
}

/// After its `Done` a request's channel is closed.
fn assert_no_more(rx: &mpsc::Receiver<Progress>) {
    match rx.recv_timeout(scaled(Duration::from_secs(2))) {
        Err(RecvTimeoutError::Disconnected) => {}
        other => panic!("expected the channel closed, got {other:?}"),
    }
}

fn done_outcome(p: Progress) -> ReplyOutcome {
    match p {
        Progress::Done(Ok(o)) => o,
        other => panic!("expected Done(Ok), got {other:?}"),
    }
}

fn done_ok(p: Progress) -> RtspResponse {
    done_outcome(p).response
}

fn done_io(p: Progress) -> io::ErrorKind {
    match p {
        Progress::Done(Err(RtspError::Io(k))) => k,
        other => panic!("expected Done(Err(Io)), got {other:?}"),
    }
}

/// A connected pair: the client's stream and the peer's socket.
fn plain_pair() -> (Stream, TcpStream) {
    let (client, peer) = socket_pair();
    (Stream::from_plain(client).unwrap(), peer)
}

fn socket_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(scaled(Duration::from_secs(5))))
        .unwrap();
    (client, peer)
}

/// Read one request head (through `\r\n\r\n`); bytes past it stay in
/// `rest`.
fn read_head(r: &mut impl Read, rest: &mut Vec<u8>) -> Vec<u8> {
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = rest.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = rest[..end + 4].to_vec();
            rest.drain(..end + 4);
            return head;
        }
        let n = r.read(&mut chunk).expect("peer read");
        assert!(n > 0, "EOF before a request head");
        rest.extend_from_slice(&chunk[..n]);
    }
}

fn cseq(head: &[u8]) -> u32 {
    RtspRequest::parse(head).unwrap().0.headers["cseq"]
        .parse()
        .unwrap()
}

fn ok_reply(cseq: u32) -> Vec<u8> {
    format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n").into_bytes()
}

/// One request answered: everything sent to the driver before it has
/// been handled.
fn round_trip(h: &Harness, peer: &mut impl ReadWrite, rest: &mut Vec<u8>) {
    let rx = h.request(options());
    assert!(matches!(next(&rx), Progress::Written));
    let head = read_head(peer, rest);
    peer.write_all(&ok_reply(cseq(&head))).unwrap();
    assert_eq!(done_ok(next(&rx)).status, 200);
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// The thread draws a cnonce for each Digest answer it builds: the second
/// attempt after a `401`, and the cached challenge's answer on the next
/// request and on every ping. It draws for nothing else: not for a request
/// before the challenge, a reply or an inbound frame.
#[test]
fn a_cnonce_is_drawn_once_for_each_digest_answer() {
    use secrecy::SecretString;

    fn request(head: &[u8]) -> RtspRequest {
        RtspRequest::parse(head).unwrap().0
    }
    /// The cnonce of a request's Digest answer.
    fn cnonce(head: &[u8]) -> String {
        let authorization = &request(head).headers["authorization"];
        let at = authorization.find("cnonce=\"").expect("a cnonce") + 8;
        let cnonce = &authorization[at..at + 32];
        assert!(
            cnonce.bytes().all(|b| b.is_ascii_hexdigit()),
            "{authorization}"
        );
        cnonce.to_string()
    }

    let (stream, mut peer) = plain_pair();
    let credentials = Credentials {
        username: "user".to_string(),
        password: SecretString::new("pass".into()),
    };
    let h = drive_engine(
        stream,
        engine_with(Some(credentials)),
        CONTROL_WRITE_TIMEOUT,
    );
    let drawn = || h.cnonces_drawn.load(Ordering::Relaxed);
    let mut rest = Vec::new();

    // No challenge yet.
    round_trip(&h, &mut peer, &mut rest);
    assert_eq!(drawn(), 0);

    // A `401` with a Digest challenge: one cnonce, for the second attempt.
    let rx = h.request(options());
    assert!(matches!(next(&rx), Progress::Written));
    let first = read_head(&mut peer, &mut rest);
    assert!(!request(&first).headers.contains_key("authorization"));
    peer.write_all(
        format!(
            "RTSP/1.0 401 Unauthorized\r\nCSeq: {}\r\nWWW-Authenticate: Digest realm=\"R\", nonce=\"n1\", qop=\"auth\"\r\n\r\n",
            cseq(&first)
        )
        .as_bytes(),
    )
    .unwrap();
    let retry = read_head(&mut peer, &mut rest);
    let retry_cnonce = cnonce(&retry);
    assert_eq!(drawn(), 1);
    peer.write_all(&ok_reply(cseq(&retry))).unwrap();
    assert_eq!(done_ok(next(&rx)).status, 200);
    assert_eq!(drawn(), 1, "nothing is drawn for a reply");

    // The next request answers the cached challenge: one more, and fresh.
    let rx = h.request(options());
    assert!(matches!(next(&rx), Progress::Written));
    let second = read_head(&mut peer, &mut rest);
    assert_ne!(cnonce(&second), retry_cnonce);
    assert_eq!(drawn(), 2);
    // Frames ahead of the reply are read without one.
    peer.write_all(b"$\x00\x00\x02ab$\x01\x00\x01c").unwrap();
    peer.write_all(&ok_reply(cseq(&second))).unwrap();
    assert_eq!(done_ok(next(&rx)).status, 200);
    assert_eq!(drawn(), 2);

    // Every ping answers it too. With the thread held, each cnonce drawn
    // since is a ping on the wire that carries it.
    h.handle
        .send(Command::EnableKeepalive(Some(Duration::from_millis(20))))
        .unwrap();
    let mut seen = vec![cnonce(&read_head(&mut peer, &mut rest))];
    let release = h.hold_between_passes();
    let pings = usize::try_from(drawn() - 2).unwrap();
    assert!(pings >= 1);
    while seen.len() < pings {
        let ping = read_head(&mut peer, &mut rest);
        assert_eq!(request(&ping).method, RtspMethod::Options);
        let c = cnonce(&ping);
        assert!(!seen.contains(&c), "a cnonce was used twice");
        seen.push(c);
    }
    drop(release);
    h.stop_and_join();
}

#[test]
fn a_request_gets_written_then_its_reply() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let rx = h.request(options());
    assert!(matches!(next(&rx), Progress::Written));
    let head = read_head(&mut peer, &mut Vec::new());
    assert_eq!(cseq(&head), 1);
    peer.write_all(&ok_reply(1)).unwrap();
    assert_eq!(done_ok(next(&rx)).status, 200);
    assert_no_more(&rx);
    assert_eq!(h.stop_and_join().get(), Some(EndReason::Cancelled));
}

#[test]
fn a_reply_split_across_segments_is_one_reply() {
    let (stream, mut peer) = plain_pair();
    peer.set_nodelay(true).unwrap();
    let h = drive(stream);
    let rx = h.request(options());
    assert!(matches!(next(&rx), Progress::Written));
    read_head(&mut peer, &mut Vec::new());
    peer.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nCon")
        .unwrap();
    peer.flush().unwrap();
    peer.write_all(b"tent-Length: 2\r\n\r\nok").unwrap();
    let resp = done_ok(next(&rx));
    assert_eq!(resp.body.as_ref(), b"ok");
    assert_no_more(&rx);
    h.stop_and_join();
}

#[test]
fn eof_fails_the_pending_request_and_records_clean_teardown() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let rx = h.request(options());
    assert!(matches!(next(&rx), Progress::Written));
    read_head(&mut peer, &mut Vec::new());
    drop(peer);
    assert_eq!(done_io(next(&rx)), io::ErrorKind::UnexpectedEof);
    assert_eq!(h.wait_end(), EndReason::CleanTeardown);
    assert!(h.session_dead.load(Ordering::Acquire));
    h.stop_and_join();
}

#[test]
fn a_reset_records_transport_failed() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let rx = h.request(options());
    assert!(matches!(next(&rx), Progress::Written));
    read_head(&mut peer, &mut Vec::new());
    socket2::SockRef::from(&peer)
        .set_linger(Some(Duration::ZERO))
        .unwrap();
    drop(peer);
    assert_eq!(done_io(next(&rx)), io::ErrorKind::ConnectionReset);
    match h.wait_end() {
        EndReason::TransportFailed { msg } => assert!(msg.starts_with("TCP read failed"), "{msg}"),
        other => panic!("expected TransportFailed, got {other:?}"),
    }
    h.stop_and_join();
}

#[test]
fn stop_records_cancelled_and_closes_the_socket() {
    let (stream, mut peer) = plain_pair();
    peer.set_read_timeout(Some(scaled(Duration::from_secs(2))))
        .unwrap();
    let h = drive(stream);
    let rx = h.request(options());
    assert!(matches!(next(&rx), Progress::Written));
    read_head(&mut peer, &mut Vec::new());
    let reasons = h.stop_and_join();
    assert_eq!(reasons.get(), Some(EndReason::Cancelled));
    assert_eq!(done_io(next(&rx)), io::ErrorKind::UnexpectedEof);
    let mut b = [0u8; 16];
    assert_eq!(peer.read(&mut b).expect("EOF, not a timeout"), 0);
}

/// Whoever reads the session dead finds its end reason recorded. In each
/// of many short sessions an observer spins on the flag and reads the
/// reason the moment the flag turns.
#[test]
fn the_end_reason_is_recorded_before_the_session_reads_dead() {
    for round in 0..200 {
        let (stream, peer) = plain_pair();
        let h = drive(stream);
        let (spinning_tx, spinning_rx) = mpsc::channel();
        let observer = std::thread::spawn({
            let (dead, reason) = (h.session_dead.clone(), h.end_reason.clone());
            move || {
                let _ = spinning_tx.send(());
                let give_up = Instant::now() + scaled(Duration::from_secs(5));
                loop {
                    // The clock is read rarely: the spin stays tight.
                    for _ in 0..4096 {
                        if dead.load(Ordering::Acquire) {
                            return Some(reason.get());
                        }
                        std::hint::spin_loop();
                    }
                    if Instant::now() >= give_up {
                        return None;
                    }
                }
            }
        });
        spinning_rx.recv().unwrap();
        drop(peer);
        let seen = observer
            .join()
            .unwrap()
            .unwrap_or_else(|| panic!("round {round}: the session never read dead"));
        assert_eq!(
            seen,
            Some(EndReason::CleanTeardown),
            "round {round}: the session read dead before its reason was recorded"
        );
        h.stop_and_join();
    }
}

/// Once the client's thread is gone the session reads dead, whichever way
/// the thread went: a stop reports no end of the session.
#[test]
fn a_thread_that_has_ended_leaves_the_session_dead() {
    let (stream, _peer) = plain_pair();
    let h = drive(stream);
    let dead = h.session_dead.clone();
    assert!(!dead.load(Ordering::Acquire));
    let reasons = h.stop_and_join();
    assert!(dead.load(Ordering::Acquire));
    assert_eq!(reasons.get(), Some(EndReason::Cancelled));
}

#[test]
fn stop_after_eof_keeps_the_first_reason() {
    let (stream, peer) = plain_pair();
    let h = drive(stream);
    drop(peer);
    assert_eq!(h.wait_end(), EndReason::CleanTeardown);
    assert_eq!(h.stop_and_join().get(), Some(EndReason::CleanTeardown));
}

/// The SSRC the routes' RTCP sinks match report blocks against.
const OUR_SSRC: u32 = 0x0102_0304;

fn setup_reply(cseq: u32, transport: &str) -> Vec<u8> {
    format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: S1\r\nTransport: {transport}\r\n\r\n")
        .into_bytes()
}

/// One SETUP answered with the channel pair `rtp`/`rtcp`: the route is
/// installed and everything sent to the driver before it has been handled.
fn route(
    h: &Harness,
    peer: &mut impl ReadWrite,
    rest: &mut Vec<u8>,
    (id, kind): (u8, RouteKind),
    (rtp, rtcp): (u8, u8),
    media: Option<mpsc::SyncSender<Bytes>>,
) -> Arc<Mutex<RtcpStats>> {
    let (rx, stats) = h.setup(id, kind, media);
    assert!(matches!(next(&rx), Progress::Written));
    let head = read_head(peer, rest);
    let transport = format!("RTP/AVP/TCP;unicast;interleaved={rtp}-{rtcp}");
    peer.write_all(&setup_reply(cseq(&head), &transport))
        .unwrap();
    assert_eq!(
        done_outcome(next(&rx)).setup,
        Some(SetupAccepted {
            route: RouteId(id),
            rtp_channel: rtp,
            rtcp_channel: rtcp,
        })
    );
    stats
}

/// An ANNOUNCE of `tracks` tracks answered `200`: the client publishes,
/// and a `Push` route is set up for track 0 first.
fn announced(h: &Harness, peer: &mut impl ReadWrite, rest: &mut Vec<u8>, tracks: u8) {
    let rx = h.request(RequestSpec {
        method: RtspMethod::Announce,
        announce_tracks: Some(tracks),
        ..options()
    });
    assert!(matches!(next(&rx), Progress::Written));
    let head = read_head(peer, rest);
    peer.write_all(format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n", cseq(&head)).as_bytes())
        .unwrap();
    assert_eq!(done_ok(next(&rx)).status, 200);
}

/// The receiving side's route, on channels 0/1, feeding `data_tx`.
fn interleaved(
    h: &Harness,
    peer: &mut impl ReadWrite,
    rest: &mut Vec<u8>,
    data_tx: mpsc::SyncSender<Bytes>,
) -> Arc<Mutex<RtcpStats>> {
    route(h, peer, rest, (0, RouteKind::Pull), (0, 1), Some(data_tx))
}

fn frame(channel: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![b'$', channel];
    f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    f.extend_from_slice(payload);
    f
}

fn rtp_packet(seq: u16, payload_len: usize) -> Vec<u8> {
    let mut rtp = vec![0u8; crate::packet::RTP_HEADER_LEN];
    crate::packet::RtpHeader::new(seq, 0, 0).encode_into(&mut rtp);
    rtp.extend(std::iter::repeat_n(0xAB, payload_len));
    rtp
}

fn rtp_frame(seq: u16, payload_len: usize) -> Vec<u8> {
    frame(0, &rtp_packet(seq, payload_len))
}

fn sender_report() -> Vec<u8> {
    crate::rtcp::SenderReport {
        ssrc: 0x1234_5678,
        ntp_timestamp: 0xE000_0000_8000_0000,
        rtp_timestamp: 90_000,
        sender_packet_count: 10,
        sender_octet_count: 1_880,
        report_blocks: vec![],
    }
    .encode()
    .unwrap()
}

/// A receiver report with one block, about `ssrc`, reporting `lost`.
fn receiver_report(ssrc: u32, lost: i32) -> Vec<u8> {
    crate::rtcp::ReceiverReport {
        ssrc: 0xDEAD_BEEF,
        report_blocks: vec![crate::rtcp::ReportBlock {
            ssrc,
            fraction_lost: 77,
            cumulative_lost: lost,
            extended_highest_seq: 0,
            jitter: 900,
            last_sr: 0,
            delay_since_last_sr: 0,
        }],
    }
    .encode()
    .unwrap()
}

fn assert_queue_closed(data_rx: &mpsc::Receiver<Bytes>) {
    assert!(matches!(
        data_rx.recv_timeout(scaled(Duration::from_secs(5))),
        Err(RecvTimeoutError::Disconnected)
    ));
}

/// The frames a server sends right behind its SETUP reply are delivered:
/// the sinks are with the client's thread before the reply can arrive, and
/// the channel pair is in place when the reply is accepted.
#[test]
fn a_setup_reply_and_frames_in_one_write_reach_the_pull_receiver() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let (data_tx, data_rx) = mpsc::sync_channel(8);
    let (rx, stats) = h.setup(0, RouteKind::Pull, Some(data_tx));
    assert!(matches!(next(&rx), Progress::Written));
    let head = read_head(&mut peer, &mut Vec::new());
    let mut bytes = setup_reply(cseq(&head), "RTP/AVP/TCP;unicast;interleaved=4-5");
    bytes.extend(frame(4, &rtp_packet(7, 16)));
    bytes.extend(frame(5, &sender_report()));
    peer.write_all(&bytes).unwrap();
    assert_eq!(
        done_outcome(next(&rx)).setup,
        Some(SetupAccepted {
            route: RouteId(0),
            rtp_channel: 4,
            rtcp_channel: 5,
        })
    );
    expect_media(&data_rx, 7);
    wait_until("the sender report counted", || {
        stats.lock().unwrap().sr_packets_received == 1
    });
    assert_eq!(h.media_dropped.load(Ordering::Relaxed), 0);
    h.stop_and_join();
}

/// A SETUP whose reply installs no channel pair leaves the route as it
/// was: its own queue closes and the installed one keeps delivering.
#[test]
fn a_setup_that_installs_nothing_drops_its_sinks_and_keeps_the_route() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let mut rest = Vec::new();
    let (first_tx, first_rx) = mpsc::sync_channel(8);
    interleaved(&h, &mut peer, &mut rest, first_tx);
    let mut seq = 0;
    let mut still_delivers = |peer: &mut TcpStream| {
        seq += 1;
        peer.write_all(&rtp_frame(seq, 8)).unwrap();
        expect_media(&first_rx, seq);
    };

    // A refusal.
    let (data_tx, data_rx) = mpsc::sync_channel(8);
    let (rx, _stats) = h.setup(0, RouteKind::Pull, Some(data_tx));
    let head = read_head(&mut peer, &mut rest);
    peer.write_all(
        format!(
            "RTSP/1.0 461 Unsupported Transport\r\nCSeq: {}\r\n\r\n",
            cseq(&head)
        )
        .as_bytes(),
    )
    .unwrap();
    assert!(matches!(next(&rx), Progress::Written));
    let outcome = done_outcome(next(&rx));
    assert_eq!((outcome.response.status, outcome.setup), (461, None));
    assert_queue_closed(&data_rx);
    still_delivers(&mut peer);

    // A reply that is not interleaved.
    let (data_tx, data_rx) = mpsc::sync_channel(8);
    let (rx, _stats) = h.setup(0, RouteKind::Pull, Some(data_tx));
    let head = read_head(&mut peer, &mut rest);
    peer.write_all(&setup_reply(
        cseq(&head),
        "RTP/AVP;unicast;client_port=5004-5005;server_port=6000-6001",
    ))
    .unwrap();
    assert!(matches!(next(&rx), Progress::Written));
    assert_eq!(done_outcome(next(&rx)).setup, None);
    assert_queue_closed(&data_rx);
    still_delivers(&mut peer);

    // A channel pair the engine refuses: another route holds it.
    let (other_tx, _other_rx) = mpsc::sync_channel(8);
    route(
        &h,
        &mut peer,
        &mut rest,
        (1, RouteKind::Pull),
        (6, 7),
        Some(other_tx),
    );
    let (data_tx, data_rx) = mpsc::sync_channel(8);
    let (rx, _stats) = h.setup(0, RouteKind::Pull, Some(data_tx));
    let head = read_head(&mut peer, &mut rest);
    peer.write_all(&setup_reply(
        cseq(&head),
        "RTP/AVP/TCP;unicast;interleaved=6-7",
    ))
    .unwrap();
    assert!(matches!(next(&rx), Progress::Written));
    assert!(matches!(
        next(&rx),
        Progress::Done(Err(RtspError::BadResponse { .. }))
    ));
    assert_queue_closed(&data_rx);
    still_delivers(&mut peer);

    // A SETUP replaced before its reply.
    let (data_tx, data_rx) = mpsc::sync_channel(8);
    let (replaced, _stats) = h.setup(0, RouteKind::Pull, Some(data_tx));
    assert!(matches!(next(&replaced), Progress::Written));
    let late = cseq(&read_head(&mut peer, &mut rest));
    round_trip(&h, &mut peer, &mut rest);
    assert_no_more(&replaced);
    assert_queue_closed(&data_rx);
    // Its late reply installs nothing either.
    peer.write_all(&setup_reply(late, "RTP/AVP/TCP;unicast;interleaved=2-3"))
        .unwrap();
    still_delivers(&mut peer);
    h.stop_and_join();
}

/// A second SETUP of the receiving route takes the route over when its
/// reply is accepted: the first queue closes, the second gets the frames.
#[test]
fn a_new_pull_setup_takes_the_route_over_at_its_reply() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let mut rest = Vec::new();
    let (first_tx, first_rx) = mpsc::sync_channel(8);
    interleaved(&h, &mut peer, &mut rest, first_tx);
    let (second_tx, second_rx) = mpsc::sync_channel(8);
    let (rx, _stats) = h.setup(0, RouteKind::Pull, Some(second_tx));
    assert!(matches!(next(&rx), Progress::Written));
    let head = read_head(&mut peer, &mut rest);
    // Until the reply the first queue is the route's.
    peer.write_all(&rtp_frame(1, 8)).unwrap();
    expect_media(&first_rx, 1);
    let mut bytes = setup_reply(cseq(&head), "RTP/AVP/TCP;unicast;interleaved=2-3");
    bytes.extend(frame(2, &rtp_packet(2, 8)));
    peer.write_all(&bytes).unwrap();
    assert!(done_outcome(next(&rx)).setup.is_some());
    expect_media(&second_rx, 2);
    assert_queue_closed(&first_rx);
    h.stop_and_join();
}

/// A SETUP the stream's end cuts off: its queue closes, and whoever sees
/// that reads the reason.
#[test]
fn a_setup_cut_off_by_the_streams_end_closes_its_queue() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let (data_tx, data_rx) = mpsc::sync_channel(8);
    let (rx, _stats) = h.setup(0, RouteKind::Pull, Some(data_tx));
    assert!(matches!(next(&rx), Progress::Written));
    read_head(&mut peer, &mut Vec::new());
    drop(peer);
    assert_queue_closed(&data_rx);
    assert_eq!(h.end_reason.get(), Some(EndReason::CleanTeardown));
    assert_eq!(done_io(next(&rx)), io::ErrorKind::UnexpectedEof);
    h.stop_and_join();
}

/// Sinks that name another route than their request's SETUP, sinks with
/// no SETUP route and a SETUP route with no sinks are refused before the
/// engine sees the request: the sinks are dropped, nothing is written, and
/// the request that was waiting still gets its reply.
#[test]
fn a_request_whose_sinks_and_setup_name_different_routes_is_refused() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let mut rest = Vec::new();
    let waiting = h.request(options());
    assert!(matches!(next(&waiting), Progress::Written));
    let head = read_head(&mut peer, &mut rest);

    let setup_of = |id: u8| RequestSpec {
        method: RtspMethod::Setup,
        setup: Some(SetupRoute {
            id: RouteId(id),
            kind: RouteKind::Pull,
        }),
        ..options()
    };
    let sinks_of = |id: u8, media: Option<mpsc::SyncSender<Bytes>>| RouteSpec {
        id: RouteId(id),
        inbound_media: media,
        inbound_rtcp: RtcpSink {
            stats: Arc::new(Mutex::new(RtcpStats::default())),
            ssrc: OUR_SSRC,
        },
    };
    let refused = |spec: RequestSpec, route: Option<RouteSpec>| {
        let rx = h.request_routed(spec, route);
        match next(&rx) {
            Progress::Done(Err(RtspError::NotAllowed { .. })) => {}
            other => panic!("expected Done(Err(NotAllowed)), got {other:?}"),
        }
    };
    let (data_tx, data_rx) = mpsc::sync_channel(8);
    refused(setup_of(0), Some(sinks_of(1, Some(data_tx))));
    assert_queue_closed(&data_rx);
    refused(options(), Some(sinks_of(0, None)));
    refused(setup_of(0), None);

    peer.write_all(&ok_reply(cseq(&head))).unwrap();
    assert_eq!(done_ok(next(&waiting)).status, 200);
    // The refused requests took no CSeq and left no byte on the wire.
    let rx = h.request(options());
    assert!(matches!(next(&rx), Progress::Written));
    assert_eq!(cseq(&read_head(&mut peer, &mut rest)), 2);
    assert!(rest.is_empty(), "bytes behind the request");
    h.stop_and_join();
}

/// On a route this side sends on, what the peer sends on the RTP channel
/// is counted and dropped, and its receiver reports reach the route's
/// stats.
#[test]
fn a_frame_on_a_push_rtp_channel_adds_to_frames_dropped_inbound() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let mut rest = Vec::new();
    announced(&h, &mut peer, &mut rest, 1);
    let stats = route(&h, &mut peer, &mut rest, (0, RouteKind::Push), (2, 3), None);
    let mut bytes = frame(2, &rtp_packet(1, 8));
    bytes.extend(frame(2, b"not RTP"));
    bytes.extend(frame(3, &receiver_report(OUR_SSRC, 5)));
    peer.write_all(&bytes).unwrap();
    wait_until("the receiver report counted", || {
        stats.lock().unwrap().rr_packets_received == 1
    });
    assert_eq!(h.frames_dropped_inbound.load(Ordering::Relaxed), 2);
    assert_eq!(h.media_dropped.load(Ordering::Relaxed), 0);
    assert_eq!(stats.lock().unwrap().cumulative_lost_send, 5);
    h.stop_and_join();
}

/// A report block about another source says nothing of this route's
/// stream: it is counted as a report and moves no loss figure.
#[test]
fn a_receiver_report_for_another_ssrc_changes_no_loss_figure() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let mut rest = Vec::new();
    announced(&h, &mut peer, &mut rest, 1);
    let stats = route(&h, &mut peer, &mut rest, (0, RouteKind::Push), (2, 3), None);
    peer.write_all(&frame(3, &receiver_report(OUR_SSRC ^ 1, 2024)))
        .unwrap();
    wait_until("the first report counted", || {
        stats.lock().unwrap().rr_packets_received == 1
    });
    {
        let s = stats.lock().unwrap();
        assert_eq!(
            (
                s.cumulative_lost_send,
                s.fraction_lost_q8,
                s.interarrival_jitter_us
            ),
            (0, 0, 0)
        );
    }
    // A block about this route's own source does move them.
    peer.write_all(&frame(3, &receiver_report(OUR_SSRC, 5)))
        .unwrap();
    wait_until("the second report counted", || {
        stats.lock().unwrap().rr_packets_received == 2
    });
    let s = stats.lock().unwrap();
    assert_eq!((s.cumulative_lost_send, s.fraction_lost_q8), (5, 77));
    assert_eq!(s.interarrival_jitter_us, 10_000);
}

fn seq_of(packet: &[u8]) -> u16 {
    u16::from_be_bytes([packet[2], packet[3]])
}

#[test]
fn media_is_delivered_and_a_full_queue_drops_the_newest() {
    const QUEUE: usize = 1024;
    const EXTRA: usize = 256;
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let (data_tx, data_rx) = mpsc::sync_channel(QUEUE);
    let mut rest = Vec::new();
    interleaved(&h, &mut peer, &mut rest, data_tx);
    let mut burst = Vec::new();
    for seq in 0..(QUEUE + EXTRA) as u16 {
        burst.extend(rtp_frame(seq, 8));
    }
    peer.write_all(&burst).unwrap();
    wait_until("the over-cap frames dropped", || {
        h.media_dropped.load(Ordering::Relaxed) == EXTRA as u64
    });
    let kept: Vec<u16> = data_rx.try_iter().map(|p| seq_of(&p)).collect();
    assert_eq!(kept, (0..QUEUE as u16).collect::<Vec<_>>());
    h.stop_and_join();
}

/// A burst many read budgets long, then silence: the bytes past the first
/// budget are read without a further readiness event, which silence never
/// brings.
#[test]
fn a_burst_larger_than_one_read_is_all_delivered() {
    const FRAMES: usize = 2_000;
    let (client, mut peer) = socket_pair();
    let h = drive(Stream::from_plain(client).unwrap().with_read_budget(1024));
    let (data_tx, data_rx) = mpsc::sync_channel(FRAMES);
    let mut rest = Vec::new();
    interleaved(&h, &mut peer, &mut rest, data_tx);
    let mut burst = Vec::new();
    for seq in 0..FRAMES {
        burst.extend(rtp_frame(seq as u16, 20));
    }
    peer.write_all(&burst).unwrap();
    let mut got = 0;
    wait_until("every frame", || {
        got += data_rx.try_iter().count();
        got == FRAMES
    });
    assert_eq!(h.media_dropped.load(Ordering::Relaxed), 0);
    h.stop_and_join();
}

#[test]
fn rtcp_reaches_the_stats_cell() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let (data_tx, _data_rx) = mpsc::sync_channel(8);
    let mut rest = Vec::new();
    let stats = interleaved(&h, &mut peer, &mut rest, data_tx);
    let sr = crate::rtcp::SenderReport {
        ssrc: 0x1234_5678,
        ntp_timestamp: 0xE000_0000_8000_0000,
        rtp_timestamp: 90_000,
        sender_packet_count: 10,
        sender_octet_count: 1_880,
        report_blocks: vec![],
    }
    .encode()
    .unwrap();
    let mut frame = vec![b'$', 1];
    frame.extend_from_slice(&(sr.len() as u16).to_be_bytes());
    frame.extend_from_slice(&sr);
    peer.write_all(&frame).unwrap();
    wait_until("the sender report counted", || {
        stats.lock().unwrap().sr_packets_received == 1
    });
    h.stop_and_join();
}

/// A handle dropped while its thread is busy (here: held in `deliver` on
/// the RTCP stats lock) still stops the thread: the wake `stop()` sent is
/// not lost when the handle goes, so the thread ends and closes the socket.
#[test]
fn a_handle_dropped_while_its_thread_is_busy_still_stops_it() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let (data_tx, data_rx) = mpsc::sync_channel(8);
    let mut rest = Vec::new();
    let stats = interleaved(&h, &mut peer, &mut rest, data_tx);

    let held = stats.lock().unwrap();
    let sr = crate::rtcp::SenderReport {
        ssrc: 0x1234_5678,
        ntp_timestamp: 0xE000_0000_8000_0000,
        rtp_timestamp: 90_000,
        sender_packet_count: 10,
        sender_octet_count: 1_880,
        report_blocks: vec![],
    }
    .encode()
    .unwrap();
    // One write: a media frame, then the sender report. The thread hands
    // the media frame on and next needs the lock held here for the report,
    // so once the media frame has come out it has read both and cannot
    // wait on its poll again before it takes that lock.
    let mut frames = rtp_frame(7, 16);
    frames.extend_from_slice(&[b'$', 1]);
    frames.extend_from_slice(&(sr.len() as u16).to_be_bytes());
    frames.extend_from_slice(&sr);
    peer.write_all(&frames).unwrap();
    expect_media(&data_rx, 7);
    drop(h);
    drop(held);

    peer.set_read_timeout(Some(scaled(Duration::from_secs(2))))
        .unwrap();
    let mut b = [0u8; 16];
    assert_eq!(
        peer.read(&mut b).expect("EOF, not a timeout"),
        0,
        "the thread did not end"
    );
}

#[test]
fn a_request_after_the_stream_ended_is_io_at_once() {
    let (stream, peer) = plain_pair();
    let h = drive(stream);
    drop(peer);
    assert_eq!(h.wait_end(), EndReason::CleanTeardown);
    let rx = h.request(options());
    assert_eq!(done_io(next(&rx)), io::ErrorKind::UnexpectedEof);
    assert_no_more(&rx);
    h.stop_and_join();
}

#[test]
fn a_replaced_request_sees_its_channel_close() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let mut rest = Vec::new();
    let first = h.request(options());
    assert!(matches!(next(&first), Progress::Written));
    read_head(&mut peer, &mut rest);
    let second = h.request(options());
    assert_no_more(&first);
    assert!(matches!(next(&second), Progress::Written));
    let head = read_head(&mut peer, &mut rest);
    peer.write_all(&ok_reply(cseq(&head))).unwrap();
    assert_eq!(done_ok(next(&second)).status, 200);
    h.stop_and_join();
}

/// The engine refuses the second request; the first one was dropped when
/// the second arrived, so its caller sees its channel close.
#[test]
fn a_request_arriving_drops_the_waiting_ones_progress() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let first = h.request(options());
    assert!(matches!(next(&first), Progress::Written));
    read_head(&mut peer, &mut Vec::new());
    let refused = h.request(RequestSpec {
        method: RtspMethod::Options,
        uri: URI.to_string(),
        headers: vec![("x-bad", "a\r\nb".to_string())],
        body: None,
        announce_tracks: None,
        setup: None,
    });
    assert!(matches!(
        next(&refused),
        Progress::Done(Err(RtspError::InvalidHeader { .. }))
    ));
    assert_no_more(&first);
    h.stop_and_join();
}

/// A receiver that sees its queue close reads the reason at once.
///
/// The two steps are a few instructions apart on the client's thread, so
/// the receiver spins rather than parks (a parked receiver wakes too late
/// to see a swapped order in most runs) and the scenario repeats on a
/// fresh connection each time.
#[test]
fn the_media_queue_closes_after_the_end_reason_is_recorded() {
    for iteration in 0..50 {
        let (stream, mut peer) = plain_pair();
        let h = drive(stream);
        let (data_tx, data_rx) = mpsc::sync_channel(8);
        interleaved(&h, &mut peer, &mut Vec::new(), data_tx);
        drop(peer);
        let deadline = Instant::now() + scaled(Duration::from_secs(5));
        loop {
            match data_rx.try_recv() {
                Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "iteration {iteration}: the queue did not close"
                    );
                    std::hint::spin_loop();
                }
                other => panic!("iteration {iteration}: expected the queue closed, got {other:?}"),
            }
        }
        assert_eq!(
            h.end_reason.get(),
            Some(EndReason::CleanTeardown),
            "iteration {iteration}: the queue closed before the reason was recorded"
        );
        h.stop_and_join();
    }
}

/// The next media frame arrives within the wait and has RTP sequence `seq`.
fn expect_media(data_rx: &mpsc::Receiver<Bytes>, seq: u16) {
    let p = data_rx
        .recv_timeout(scaled(Duration::from_secs(5)))
        .expect("a media frame");
    assert_eq!(seq_of(&p), seq);
}

/// A `454` to a ping ends the session, not the stream: media keeps coming.
#[test]
fn a_ping_answered_454_keeps_the_media_queue_open() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let (data_tx, data_rx) = mpsc::sync_channel(8);
    let mut rest = Vec::new();
    interleaved(&h, &mut peer, &mut rest, data_tx);
    h.handle
        .send(Command::EnableKeepalive(Some(Duration::from_millis(50))))
        .unwrap();
    let head = read_head(&mut peer, &mut rest);
    let ping = cseq(&head);
    assert_eq!(ping, 2, "the ping takes the CSeq after the request's");
    peer.write_all(format!("RTSP/1.0 454 Session Not Found\r\nCSeq: {ping}\r\n\r\n").as_bytes())
        .unwrap();
    // The flag is stored after the reason: wait on the flag, then read it.
    wait_until("the session dead", || {
        h.session_dead.load(Ordering::Acquire)
    });
    assert_eq!(h.end_reason.get(), Some(EndReason::SessionExpired));
    peer.write_all(&rtp_frame(7, 8)).unwrap();
    expect_media(&data_rx, 7);
    h.stop_and_join();
}

/// The closed queue is not sticky: a later SETUP's queue gets media.
#[test]
fn a_setup_after_an_accepted_teardown_delivers_media_again() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let (first_tx, first_rx) = mpsc::sync_channel(8);
    let mut rest = Vec::new();
    interleaved(&h, &mut peer, &mut rest, first_tx);
    let rx = h.request(RequestSpec {
        method: RtspMethod::Teardown,
        uri: URI.to_string(),
        headers: Vec::new(),
        body: None,
        announce_tracks: None,
        setup: None,
    });
    assert!(matches!(next(&rx), Progress::Written));
    let head = read_head(&mut peer, &mut rest);
    peer.write_all(&ok_reply(cseq(&head))).unwrap();
    assert_eq!(done_ok(next(&rx)).status, 200);
    assert!(matches!(
        first_rx.recv_timeout(scaled(Duration::from_secs(5))),
        Err(RecvTimeoutError::Disconnected)
    ));
    let (second_tx, second_rx) = mpsc::sync_channel(8);
    interleaved(&h, &mut peer, &mut rest, second_tx);
    peer.write_all(&rtp_frame(9, 8)).unwrap();
    expect_media(&second_rx, 9);
    assert_eq!(h.stop_and_join().get(), Some(EndReason::CleanTeardown));
}

#[test]
fn a_setup_after_the_stream_ended_fails_and_its_queue_is_closed() {
    let (stream, peer) = plain_pair();
    let h = drive(stream);
    drop(peer);
    assert_eq!(h.wait_end(), EndReason::CleanTeardown);
    let (data_tx, data_rx) = mpsc::sync_channel(8);
    let (rx, _stats) = h.setup(0, RouteKind::Pull, Some(data_tx));
    assert_eq!(done_io(next(&rx)), io::ErrorKind::UnexpectedEof);
    assert!(matches!(
        data_rx.recv_timeout(scaled(Duration::from_secs(5))),
        Err(RecvTimeoutError::Disconnected)
    ));
    h.stop_and_join();
}

/// The media queue closes on an accepted TEARDOWN (as it does once the
/// stream has ended), and the TEARDOWN's reason stays when the stream ends
/// afterwards.
#[test]
fn an_accepted_teardown_closes_the_media_queue_and_keeps_its_reason() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let (data_tx, data_rx) = mpsc::sync_channel(8);
    let mut rest = Vec::new();
    interleaved(&h, &mut peer, &mut rest, data_tx);
    let rx = h.request(RequestSpec {
        method: RtspMethod::Teardown,
        uri: URI.to_string(),
        headers: Vec::new(),
        body: None,
        announce_tracks: None,
        setup: None,
    });
    assert!(matches!(next(&rx), Progress::Written));
    let head = read_head(&mut peer, &mut rest);
    peer.write_all(&ok_reply(cseq(&head))).unwrap();
    assert_eq!(done_ok(next(&rx)).status, 200);
    assert!(matches!(
        data_rx.recv_timeout(scaled(Duration::from_secs(5))),
        Err(RecvTimeoutError::Disconnected)
    ));
    assert_eq!(h.end_reason.get(), Some(EndReason::CleanTeardown));
    assert!(!h.session_dead.load(Ordering::Acquire));
    drop(peer);
    // The accepted TEARDOWN did not mark the session dead; the stream's
    // end does.
    wait_until("the stream's end", || {
        h.session_dead.load(Ordering::Acquire)
    });
    let rx = h.request(options());
    assert_eq!(done_io(next(&rx)), io::ErrorKind::UnexpectedEof);
    assert_eq!(h.stop_and_join().get(), Some(EndReason::CleanTeardown));
}

/// The driver records what the engine reports: a TEARDOWN accepted with the
/// cancel set is still a clean end here. A dropped client's `Cancelled` is
/// recorded by its `Drop`, before its TEARDOWN.
#[test]
fn an_accepted_teardown_on_a_cancelled_client_records_clean_teardown() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let (data_tx, data_rx) = mpsc::sync_channel(8);
    let mut rest = Vec::new();
    interleaved(&h, &mut peer, &mut rest, data_tx);
    h.cancel.cancel();
    let rx = h.request(RequestSpec {
        method: RtspMethod::Teardown,
        uri: URI.to_string(),
        headers: Vec::new(),
        body: None,
        announce_tracks: None,
        setup: None,
    });
    assert!(matches!(next(&rx), Progress::Written));
    let head = read_head(&mut peer, &mut rest);
    peer.write_all(&ok_reply(cseq(&head))).unwrap();
    assert_eq!(done_ok(next(&rx)).status, 200);
    assert!(matches!(
        data_rx.recv_timeout(scaled(Duration::from_secs(5))),
        Err(RecvTimeoutError::Disconnected)
    ));
    assert_eq!(h.end_reason.get(), Some(EndReason::CleanTeardown));
    drop(peer);
    assert_eq!(h.stop_and_join().get(), Some(EndReason::CleanTeardown));
}

#[test]
fn a_framing_violation_ends_the_stream_and_closes_the_socket() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let rx = h.request(options());
    assert!(matches!(next(&rx), Progress::Written));
    read_head(&mut peer, &mut Vec::new());
    peer.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: nope\r\n\r\n")
        .unwrap();
    assert!(matches!(
        next(&rx),
        Progress::Done(Err(RtspError::BadResponse { .. }))
    ));
    assert!(matches!(h.wait_end(), EndReason::ProtocolError { .. }));
    let mut b = [0u8; 16];
    assert_eq!(peer.read(&mut b).expect("EOF, not a timeout"), 0);
    // The thread still answers.
    let rx = h.request(options());
    assert_eq!(done_io(next(&rx)), io::ErrorKind::InvalidData);
    h.stop_and_join();
}

/// The twin below has no cancel: a due ping is written, so the absence
/// here is the cancel's doing.
#[test]
fn a_ping_is_not_written_once_the_cancel_is_set() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    h.cancel.cancel();
    h.handle
        .send(Command::EnableKeepalive(Some(Duration::from_millis(1))))
        .unwrap();
    let mut rest = Vec::new();
    let rx = h.request(options());
    assert!(matches!(next(&rx), Progress::Written));
    let head = read_head(&mut peer, &mut rest);
    assert_eq!(cseq(&head), 1, "the first bytes are the request's");
    peer.write_all(&ok_reply(1)).unwrap();
    assert_eq!(done_ok(next(&rx)).status, 200);
    assert!(rest.is_empty(), "bytes after the request");
    // A bounded wait for bytes that must not come: at a 1 ms interval
    // pings would arrive many times over.
    peer.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut b = [0u8; 256];
    match peer.read(&mut b) {
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) => {}
        other => panic!("expected no bytes, got {other:?}"),
    }
    h.stop_and_join();
}

#[test]
fn a_due_ping_is_written() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    h.handle
        .send(Command::EnableKeepalive(Some(Duration::from_millis(1))))
        .unwrap();
    let head = read_head(&mut peer, &mut Vec::new());
    assert!(head.starts_with(b"OPTIONS "), "a ping is an OPTIONS");
    assert_eq!(cseq(&head), 1, "nothing was sent before the ping");
    h.stop_and_join();
}

/// The peer takes 4 KiB and never reads; the client's send buffer is
/// pinned small so the kernel cannot take the whole request either.
#[cfg(target_os = "linux")]
fn non_reading_pair() -> (TcpStream, mpsc::Sender<()>, JoinHandle<()>) {
    use crate::rtsp::client::test_sockets::non_reading_peer;
    let (port, done_tx, peer) = non_reading_peer();
    let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket2::SockRef::from(&tcp)
        .set_send_buffer_size(4096)
        .unwrap();
    (tcp, done_tx, peer)
}

#[cfg(target_os = "linux")]
fn huge_request() -> RequestSpec {
    RequestSpec {
        method: RtspMethod::Options,
        uri: URI.to_string(),
        headers: vec![("x-pad", "a".repeat(1 << 20))],
        body: None,
        announce_tracks: None,
        setup: None,
    }
}

/// The write-stall time the stall tests give the driver.
fn stall() -> Duration {
    scaled(Duration::from_millis(300))
}

#[cfg(target_os = "linux")]
#[test]
fn bytes_the_peer_does_not_take_end_the_stream_after_the_write_timeout() {
    let (tcp, done_tx, peer) = non_reading_pair();
    let h = drive_with(Stream::from_plain(tcp).unwrap(), stall());
    // Before the request is sent, so before the driver's stall clock starts.
    let t0 = Instant::now();
    let rx = h.request(huge_request());
    // `Written` would come first, so the first progress is the verdict.
    assert_eq!(done_io(next(&rx)), io::ErrorKind::TimedOut);
    assert!(t0.elapsed() >= stall(), "stalled after {:?}", t0.elapsed());
    match h.wait_end() {
        EndReason::TransportFailed { msg } => assert!(msg.contains("no progress"), "{msg}"),
        other => panic!("expected TransportFailed, got {other:?}"),
    }
    assert!(h.session_dead.load(Ordering::Acquire));
    h.stop_and_join();
    let _ = done_tx.send(());
    peer.join().unwrap();
}

/// Every byte the kernel takes restarts the stall clock: a peer that
/// reads slowly, but always within the stall time, never stalls a write
/// that takes it several stall times to drain.
#[cfg(target_os = "linux")]
#[test]
fn a_slow_peer_that_keeps_taking_bytes_does_not_stall_the_write() {
    use socket2::{Domain, Socket, Type};
    const LEN: usize = 256 * 1024;
    let sock = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    sock.set_recv_buffer_size(4096).unwrap();
    sock.bind(
        &"127.0.0.1:0"
            .parse::<std::net::SocketAddr>()
            .unwrap()
            .into(),
    )
    .unwrap();
    sock.listen(1).unwrap();
    let listener: TcpListener = sock.into();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    socket2::SockRef::from(&client)
        .set_send_buffer_size(4096)
        .unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(scaled(Duration::from_secs(5))))
        .unwrap();
    let h = drive_with(Stream::from_plain(client).unwrap(), stall());
    let rx = h.request(RequestSpec {
        method: RtspMethod::Options,
        uri: URI.to_string(),
        headers: vec![("x-pad", "a".repeat(LEN))],
        body: None,
        announce_tracks: None,
        setup: None,
    });
    let t0 = Instant::now();
    let mut got = 0;
    let mut chunk = [0u8; 4096];
    while got < LEN {
        // Pacing, not a sync: each read frees a few KiB well inside the
        // stall time.
        std::thread::sleep(stall() / 15);
        let n = peer.read(&mut chunk).unwrap();
        assert!(n > 0, "the client closed the stream");
        got += n;
    }
    assert!(
        t0.elapsed() > stall() * 2,
        "the write drained in {:?}: too fast to test the clock",
        t0.elapsed()
    );
    assert!(matches!(next(&rx), Progress::Written));
    assert_eq!(h.end_reason.get(), None);
    h.stop_and_join();
}

#[cfg(target_os = "linux")]
#[test]
fn a_ping_the_peer_does_not_take_records_keepalive_failed() {
    use crate::rtsp::client::test_sockets::{fill_send_buffer, non_reading_peer};
    let (port, done_tx, peer) = non_reading_peer();
    let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    fill_send_buffer(&tcp);
    let h = drive_with(Stream::from_plain(tcp).unwrap(), stall());
    // Before the keepalive is enabled, so before any ping is queued.
    let t0 = Instant::now();
    h.handle
        .send(Command::EnableKeepalive(Some(Duration::from_millis(1))))
        .unwrap();
    // The flag is stored after the reason: wait on the flag, then read it.
    wait_until("the session dead", || {
        h.session_dead.load(Ordering::Acquire)
    });
    match h.end_reason.get() {
        Some(EndReason::KeepaliveFailed { msg }) => assert!(msg.contains("no progress"), "{msg}"),
        other => panic!("expected KeepaliveFailed, got {other:?}"),
    }
    assert!(t0.elapsed() >= stall(), "stalled after {:?}", t0.elapsed());
    h.stop_and_join();
    let _ = done_tx.send(());
    peer.join().unwrap();
}

#[cfg(unix)]
mod signal {
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(super) static DELIVERED: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn on_sigusr1(_: libc::c_int) {
        DELIVERED.fetch_add(1, Ordering::SeqCst);
    }

    /// A SIGUSR1 handler without `SA_RESTART`, so a wait it interrupts
    /// returns `EINTR`.
    pub(super) fn install_handler() {
        // SAFETY: a zeroed sigaction is valid; the handler touches one atomic.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_sigusr1 as *const () as usize;
            sa.sa_flags = 0;
            libc::sigemptyset(&mut sa.sa_mask);
            assert_eq!(libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut()), 0);
        }
    }
}

/// Signals the driver's own thread (through its join handle, so it runs on
/// every Unix) while it waits: the waits it interrupts are retried and the
/// stream stays up.
#[cfg(unix)]
#[test]
fn a_signal_does_not_end_the_driver() {
    signal::install_handler();
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let mut rest = Vec::new();
    round_trip(&h, &mut peer, &mut rest);
    let thread = h.handle.pthread().expect("the driver thread is running");
    let delivered = signal::DELIVERED.load(Ordering::SeqCst);
    wait_until("signals delivered to the driver", || {
        // SAFETY: the thread is joinable (not joined) while `h` lives, so
        // its id is valid.
        let _ = unsafe { libc::pthread_kill(thread, libc::SIGUSR1) };
        signal::DELIVERED.load(Ordering::SeqCst) >= delivered + 5
    });
    round_trip(&h, &mut peer, &mut rest);
    assert_eq!(h.end_reason.get(), None);
    h.stop_and_join();
}

mod media;

#[cfg(feature = "tls")]
mod tls {
    use super::*;
    use crate::rtsp::client::tls::TlsStream;
    use crate::rtsp::client::tls::test_peer::tls_test_peer;
    use crate::url::RtspUrl;

    pub(super) type PeerStream = rustls::StreamOwned<rustls::ServerConnection, TcpStream>;

    pub(super) fn connect(port: u16, roots: rustls::RootCertStore) -> TlsStream {
        let sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let url = RtspUrl::parse(&format!("rtsps://127.0.0.1:{port}/x")).unwrap();
        TlsStream::connect(
            &url,
            sock,
            Some(roots),
            Duration::from_millis(100),
            Some(Instant::now() + scaled(Duration::from_secs(5))),
        )
        .unwrap()
    }

    /// Keep the connection until the client closes it.
    pub(super) fn hold(s: &mut PeerStream) {
        let mut b = [0u8; 256];
        while matches!(s.read(&mut b), Ok(n) if n > 0) {}
    }

    #[test]
    fn tls_request_round_trip() {
        let (port, roots, peer) = tls_test_peer(
            |mut s: PeerStream| {
                let head = read_head(&mut s, &mut Vec::new());
                s.write_all(&ok_reply(cseq(&head))).unwrap();
                s.flush().unwrap();
                hold(&mut s);
            },
            |_| {},
        );
        let h = drive(Stream::from_tls(connect(port, roots)).unwrap());
        let rx = h.request(options());
        assert!(matches!(next(&rx), Progress::Written));
        assert_eq!(done_ok(next(&rx)).status, 200);
        assert_no_more(&rx);
        h.stop_and_join();
        peer.join().unwrap();
    }

    #[test]
    fn tls_peer_close_notify_is_clean_teardown() {
        let (port, roots, peer) = tls_test_peer(
            |mut s: PeerStream| {
                s.conn.send_close_notify();
                s.flush().unwrap();
                hold(&mut s);
            },
            |_| {},
        );
        let h = drive(Stream::from_tls(connect(port, roots)).unwrap());
        assert_eq!(h.wait_end(), EndReason::CleanTeardown);
        h.stop_and_join();
        peer.join().unwrap();
    }

    #[test]
    fn tls_record_split_across_reads() {
        let (port, roots, peer) = tls_test_peer(
            |mut s: PeerStream| {
                let head = read_head(&mut s, &mut Vec::new());
                s.conn.writer().write_all(&ok_reply(cseq(&head))).unwrap();
                let mut record = Vec::new();
                while s.conn.wants_write() {
                    s.conn.write_tls(&mut record).unwrap();
                }
                s.sock.set_nodelay(true).unwrap();
                let half = record.len() / 2;
                s.sock.write_all(&record[..half]).unwrap();
                s.sock.flush().unwrap();
                s.sock.write_all(&record[half..]).unwrap();
                hold(&mut s);
            },
            |_| {},
        );
        let h = drive(Stream::from_tls(connect(port, roots)).unwrap());
        let rx = h.request(options());
        assert!(matches!(next(&rx), Progress::Written));
        assert_eq!(done_ok(next(&rx)).status, 200);
        h.stop_and_join();
        peer.join().unwrap();
    }

    /// The peer's first reply bytes ride in its handshake flight (TLS 1.3
    /// half-RTT data), so rustls has them decrypted before the driver
    /// exists. The reply is finished only after the request, so it can
    /// only be whole if the early bytes were kept.
    #[test]
    fn plaintext_sent_right_after_the_handshake_is_not_lost() {
        let (port, roots, peer) = tls_test_peer(
            |mut s: PeerStream| {
                let head = read_head(&mut s, &mut Vec::new());
                assert_eq!(cseq(&head), 1);
                s.write_all(b"Public: OPTIONS\r\n\r\n").unwrap();
                s.flush().unwrap();
                hold(&mut s);
            },
            |conn| {
                conn.writer()
                    .write_all(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n")
                    .unwrap();
            },
        );
        let (mut conn, sock) = connect(port, roots).into_parts();
        let early = conn
            .process_new_packets()
            .unwrap()
            .plaintext_bytes_to_read();
        assert!(
            early > 0,
            "the early bytes were not decrypted during the handshake"
        );
        let h = drive(Stream::from_tls_parts(conn, sock).unwrap());
        let rx = h.request(options());
        assert!(matches!(next(&rx), Progress::Written));
        let resp = done_ok(next(&rx));
        assert_eq!(
            resp.headers.get("public").map(String::as_str),
            Some("OPTIONS")
        );
        h.stop_and_join();
        peer.join().unwrap();
    }

    /// The client's send buffer is filled before the driver starts, so the
    /// request's ciphertext is all rustls has and the kernel takes none of it.
    #[cfg(target_os = "linux")]
    #[test]
    fn tls_bytes_the_peer_does_not_take_are_not_reported_written() {
        use crate::rtsp::client::test_sockets::fill_send_buffer;
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (port, roots, peer) = tls_test_peer(
            move |s: PeerStream| {
                ready_tx.send(()).unwrap();
                // Never read: the client's bytes pile up.
                let _ = done_rx.recv_timeout(Duration::from_secs(30));
                drop(s);
            },
            |_| {},
        );
        let (mut conn, mut sock) = connect(port, roots).into_parts();
        // The client's Finished is still queued; the peer reads up to it.
        while conn.wants_write() {
            conn.write_tls(&mut sock).unwrap();
        }
        ready_rx
            .recv_timeout(scaled(Duration::from_secs(5)))
            .expect("the peer finished its handshake");
        // Raw filler after the handshake on a stream the peer never reads
        // again: it is never decrypted.
        fill_send_buffer(&sock);
        let h = drive_with(Stream::from_tls_parts(conn, sock).unwrap(), stall());
        // Before the request is sent, so before the driver's stall clock
        // starts.
        let t0 = Instant::now();
        let rx = h.request(options());
        assert_eq!(done_io(next(&rx)), io::ErrorKind::TimedOut);
        assert!(t0.elapsed() >= stall(), "stalled after {:?}", t0.elapsed());
        match h.wait_end() {
            EndReason::TransportFailed { msg } => assert!(msg.contains("no progress"), "{msg}"),
            other => panic!("expected TransportFailed, got {other:?}"),
        }
        h.stop_and_join();
        let _ = done_tx.send(());
        peer.join().unwrap();
    }

    /// The peer's `close_notify` rides in its handshake flight, so rustls
    /// has read it before the driver exists and the peer sends nothing
    /// after it. The driver still finds it: its first pass reads, and the
    /// socket's writable event at registration leads to a read too.
    #[test]
    fn a_close_notify_read_during_the_handshake_ends_the_stream() {
        let (port, roots, peer) = tls_test_peer(
            |mut s: PeerStream| hold(&mut s),
            |conn| conn.send_close_notify(),
        );
        let (mut conn, sock) = connect(port, roots).into_parts();
        assert!(
            conn.process_new_packets().unwrap().peer_has_closed(),
            "the close_notify was not read during the handshake"
        );
        let h = drive(Stream::from_tls_parts(conn, sock).unwrap());
        assert_eq!(h.wait_end(), EndReason::CleanTeardown);
        h.stop_and_join();
        peer.join().unwrap();
    }
}
