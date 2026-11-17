use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::RecvTimeoutError;

use tst_core::transport::EndReason;
use tst_test_helpers::timing::scaled;

use crate::rtsp::auth::AuthSchemes;
use crate::rtsp::client::engine::EngineConfig;
use crate::rtsp::message::{RtspMethod, RtspRequest};
use crate::url::RtspVersion;

const URI: &str = "rtsp://127.0.0.1/x";

fn engine() -> ClientEngine {
    ClientEngine::new(EngineConfig {
        uri: URI.to_string(),
        version: RtspVersion::V1_0,
        user_agent: "tst-rtp/test".to_string(),
        credentials: None,
        schemes: AuthSchemes::Automatic,
    })
}

fn options() -> RequestSpec {
    RequestSpec {
        method: RtspMethod::Options,
        uri: URI.to_string(),
        headers: Vec::new(),
    }
}

/// The driver under test with clones of what it shares.
struct Harness {
    handle: DriverHandle,
    cancel: FlagCancel,
    session_dead: Arc<AtomicBool>,
    end_reason: EndReasonHandle,
    media_dropped: Arc<AtomicU64>,
}

fn drive(stream: Stream) -> Harness {
    drive_with(stream, CONTROL_WRITE_TIMEOUT)
}

fn drive_with(stream: Stream, write_stall: Duration) -> Harness {
    let shared = Shared {
        cancel: FlagCancel::new(),
        session_dead: Arc::new(AtomicBool::new(false)),
        end_reason: EndReasonHandle::default(),
        media_dropped: Arc::new(AtomicU64::new(0)),
    };
    Harness {
        cancel: shared.cancel.clone(),
        session_dead: shared.session_dead.clone(),
        end_reason: shared.end_reason.clone(),
        media_dropped: shared.media_dropped.clone(),
        handle: spawn_with(stream, engine(), shared, write_stall).unwrap(),
    }
}

impl Harness {
    fn request(&self, spec: RequestSpec) -> mpsc::Receiver<Progress> {
        let (progress, rx) = mpsc::channel();
        self.handle
            .send(Command::Request {
                spec,
                reply_deadline: None,
                progress,
            })
            .unwrap();
        rx
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

fn done_ok(p: Progress) -> RtspResponse {
    match p {
        Progress::Done(Ok(r)) => r,
        other => panic!("expected Done(Ok), got {other:?}"),
    }
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

#[test]
fn stop_after_eof_keeps_the_first_reason() {
    let (stream, peer) = plain_pair();
    let h = drive(stream);
    drop(peer);
    assert_eq!(h.wait_end(), EndReason::CleanTeardown);
    assert_eq!(h.stop_and_join().get(), Some(EndReason::CleanTeardown));
}

fn interleaved(h: &Harness, data_tx: mpsc::SyncSender<Bytes>) -> Arc<Mutex<RtcpStats>> {
    let stats = Arc::new(Mutex::new(RtcpStats::default()));
    h.handle
        .send(Command::SetInterleaved {
            channels: InterleavedChannels { rtp: 0, rtcp: 1 },
            data_tx,
            rtcp: RtcpSink {
                stats: stats.clone(),
                ssrc: 0x0102_0304,
            },
        })
        .unwrap();
    stats
}

fn rtp_frame(seq: u16, payload_len: usize) -> Vec<u8> {
    let mut rtp = vec![0u8; crate::packet::RTP_HEADER_LEN];
    crate::packet::RtpHeader::new(seq, 0, 0).encode_into(&mut rtp);
    rtp.extend(std::iter::repeat_n(0xAB, payload_len));
    let mut f = vec![b'$', 0];
    f.extend_from_slice(&(rtp.len() as u16).to_be_bytes());
    f.extend_from_slice(&rtp);
    f
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
    interleaved(&h, data_tx);
    let mut rest = Vec::new();
    round_trip(&h, &mut peer, &mut rest);
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
    interleaved(&h, data_tx);
    let mut rest = Vec::new();
    round_trip(&h, &mut peer, &mut rest);
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
    let stats = interleaved(&h, data_tx);
    let mut rest = Vec::new();
    round_trip(&h, &mut peer, &mut rest);
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
    });
    assert!(matches!(
        next(&refused),
        Progress::Done(Err(RtspError::InvalidHeader { .. }))
    ));
    assert_no_more(&first);
    h.stop_and_join();
}

/// A receiver that sees its queue close reads the reason at once.
#[test]
fn the_media_queue_closes_after_the_end_reason_is_recorded() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let (data_tx, data_rx) = mpsc::sync_channel(8);
    interleaved(&h, data_tx);
    round_trip(&h, &mut peer, &mut Vec::new());
    drop(peer);
    match data_rx.recv_timeout(scaled(Duration::from_secs(5))) {
        Err(RecvTimeoutError::Disconnected) => {}
        other => panic!("expected the queue closed, got {other:?}"),
    }
    assert_eq!(h.end_reason.get(), Some(EndReason::CleanTeardown));
    h.stop_and_join();
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
    interleaved(&h, data_tx);
    let mut rest = Vec::new();
    round_trip(&h, &mut peer, &mut rest);
    h.handle
        .send(Command::EnableKeepalive(Some(Duration::from_millis(50))))
        .unwrap();
    let head = read_head(&mut peer, &mut rest);
    let ping = cseq(&head);
    assert!(ping > 1_000_000, "a ping's CSeq");
    peer.write_all(format!("RTSP/1.0 454 Session Not Found\r\nCSeq: {ping}\r\n\r\n").as_bytes())
        .unwrap();
    assert_eq!(h.wait_end(), EndReason::SessionExpired);
    assert!(h.session_dead.load(Ordering::Acquire));
    peer.write_all(&rtp_frame(7, 8)).unwrap();
    expect_media(&data_rx, 7);
    h.stop_and_join();
}

/// The closed queue is not sticky: a later SETUP's queue gets media.
#[test]
fn set_interleaved_after_an_accepted_teardown_delivers_media_again() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let (first_tx, first_rx) = mpsc::sync_channel(8);
    interleaved(&h, first_tx);
    let mut rest = Vec::new();
    round_trip(&h, &mut peer, &mut rest);
    let rx = h.request(RequestSpec {
        method: RtspMethod::Teardown,
        uri: URI.to_string(),
        headers: Vec::new(),
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
    interleaved(&h, second_tx);
    round_trip(&h, &mut peer, &mut rest);
    peer.write_all(&rtp_frame(9, 8)).unwrap();
    expect_media(&second_rx, 9);
    assert_eq!(h.stop_and_join().get(), Some(EndReason::CleanTeardown));
}

#[test]
fn set_interleaved_after_the_stream_ended_gets_a_closed_queue() {
    let (stream, peer) = plain_pair();
    let h = drive(stream);
    drop(peer);
    assert_eq!(h.wait_end(), EndReason::CleanTeardown);
    let (data_tx, data_rx) = mpsc::sync_channel(8);
    interleaved(&h, data_tx);
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
    interleaved(&h, data_tx);
    let mut rest = Vec::new();
    round_trip(&h, &mut peer, &mut rest);
    let rx = h.request(RequestSpec {
        method: RtspMethod::Teardown,
        uri: URI.to_string(),
        headers: Vec::new(),
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
    assert!(cseq(&head) > 1_000_000, "a ping's CSeq");
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
    }
}

/// The write-stall time the stall tests give the driver.
#[cfg(target_os = "linux")]
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
    match h.wait_end() {
        EndReason::KeepaliveFailed { msg } => assert!(msg.contains("no progress"), "{msg}"),
        other => panic!("expected KeepaliveFailed, got {other:?}"),
    }
    assert!(t0.elapsed() >= stall(), "stalled after {:?}", t0.elapsed());
    assert!(h.session_dead.load(Ordering::Acquire));
    h.stop_and_join();
    let _ = done_tx.send(());
    peer.join().unwrap();
}

#[cfg(target_os = "linux")]
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

    /// Thread ids of this process named `name`.
    pub(super) fn threads_named(name: &str) -> Vec<libc::pid_t> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
            let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
            if comm.trim() == name
                && let Ok(tid) = entry.file_name().to_string_lossy().parse()
            {
                out.push(tid);
            }
        }
        out
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_signal_does_not_end_the_driver() {
    signal::install_handler();
    let (stream, mut peer) = plain_pair();
    let before = signal::threads_named(THREAD_NAME);
    let h = drive(stream);
    let mut rest = Vec::new();
    // The thread exists once it has answered.
    round_trip(&h, &mut peer, &mut rest);
    let ours: Vec<_> = signal::threads_named(THREAD_NAME)
        .into_iter()
        .filter(|t| !before.contains(t))
        .collect();
    assert!(!ours.is_empty(), "the driver thread not found");
    let delivered = signal::DELIVERED.load(Ordering::SeqCst);
    wait_until("signals delivered to the driver", || {
        for &tid in &ours {
            // SAFETY: tgkill with this process's id; a thread that has
            // gone meanwhile only makes it fail with ESRCH.
            let _ = unsafe { libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, libc::SIGUSR1) };
        }
        signal::DELIVERED.load(Ordering::SeqCst) >= delivered + 5
    });
    round_trip(&h, &mut peer, &mut rest);
    assert_eq!(h.end_reason.get(), None);
    h.stop_and_join();
}

#[cfg(feature = "tls")]
mod tls {
    use super::*;
    use crate::rtsp::client::tls::TlsStream;
    use crate::rtsp::client::tls::test_peer::tls_test_peer;
    use crate::url::RtspUrl;

    type PeerStream = rustls::StreamOwned<rustls::ServerConnection, TcpStream>;

    fn connect(port: u16, roots: rustls::RootCertStore) -> TlsStream {
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
    fn hold(s: &mut PeerStream) {
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
