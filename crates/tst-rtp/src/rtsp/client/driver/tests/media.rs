//! Outbound media through the client's thread: its order on the wire
//! among the requests, the back-pressure on a sender, and what ends a
//! waiting send and a drain.

use super::*;
use std::net::Shutdown;

use crate::rtsp::client::media_queue::{MEDIA_QUEUE_BOUND, MEDIA_WATERMARK};

/// A frame the size of an RTP packet that carries seven TS packets.
const SMALL: usize = 4 + 12 + 1316;

/// A frame so large that a queue of them is far more than the kernel's
/// socket buffers hold, whatever their size.
const BIG: usize = 60_000;

/// A whole `$` frame of `len` bytes on the RTP channel: its payload opens
/// with `seq` and is filled with a byte that follows from it.
fn media_frame(seq: u32, len: usize) -> Bytes {
    let mut payload = seq.to_be_bytes().to_vec();
    payload.resize(len - 4, seq as u8);
    Bytes::from(frame(0, &payload))
}

fn send(h: &Harness, frame: Bytes) -> Result<(), MediaWaitEnd> {
    h.handle.send_media(frame, &h.cancel)
}

/// Sends `frame` again and again until a send is refused.
fn send_until_refused(h: &Harness, frame: &Bytes) -> MediaWaitEnd {
    loop {
        if let Err(end) = send(h, frame.clone()) {
            return end;
        }
    }
}

/// Sets the cancel if the test still runs after ten seconds, so a wait
/// that nothing ends fails its assertion instead of hanging the test. It
/// stands down when the returned sender is dropped.
fn unhang(cancel: &FlagCancel) -> mpsc::Sender<()> {
    let (tx, rx) = mpsc::channel();
    let cancel = cancel.clone();
    std::thread::spawn(move || {
        if rx.recv_timeout(scaled(Duration::from_secs(10))) == Err(RecvTimeoutError::Timeout) {
            cancel.cancel();
        }
    });
    tx
}

/// One request of `method` answered `200`.
fn answered(h: &Harness, peer: &mut impl ReadWrite, rest: &mut Vec<u8>, method: RtspMethod) {
    let rx = h.request(RequestSpec {
        method,
        ..options()
    });
    assert!(matches!(next(&rx), Progress::Written));
    let head = read_head(peer, rest);
    peer.write_all(&ok_reply(cseq(&head))).unwrap();
    assert_eq!(done_ok(next(&rx)).status, 200);
}

/// A publish session of one track, recording: ANNOUNCE, a SETUP on
/// channels 0/1 and RECORD, each answered.
fn recording(h: &Harness, peer: &mut impl ReadWrite, rest: &mut Vec<u8>) {
    announced(h, peer, rest, 1);
    route(h, peer, rest, (0, RouteKind::Push), (0, 1), None);
    answered(h, peer, rest, RtspMethod::Record);
}

/// One whole thing the client wrote.
enum Unit {
    Frame { channel: u8, payload: Vec<u8> },
    Request(Vec<u8>),
}

// By hand: a payload can be 60 kB.
impl std::fmt::Debug for Unit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unit::Frame { channel, payload } => f
                .debug_struct("Frame")
                .field("channel", channel)
                .field("len", &payload.len())
                .field("opens", &&payload[..payload.len().min(8)])
                .finish(),
            Unit::Request(head) => write!(f, "Request({:?})", String::from_utf8_lossy(head)),
        }
    }
}

/// Splits the client's byte stream into `$` frames and requests (none has
/// a body here). A request written inside a frame, or a frame inside a
/// request, comes out as a frame with the wrong bytes or as a request that
/// does not parse.
struct Units {
    buf: Vec<u8>,
    at: usize,
    chunk: Vec<u8>,
    /// Slept before every read.
    pause: Duration,
}

impl Units {
    /// Starts with `rest`, the bytes read behind the last request head.
    fn new(rest: Vec<u8>) -> Self {
        Self::paced(rest, 64 * 1024, Duration::ZERO)
    }

    /// A slow reader: at most `chunk` bytes a read, `pause` before each.
    fn paced(rest: Vec<u8>, chunk: usize, pause: Duration) -> Self {
        Self {
            buf: rest,
            at: 0,
            chunk: vec![0; chunk],
            pause,
        }
    }

    /// The next whole unit; `None` once the stream has ended.
    fn next(&mut self, r: &mut impl Read) -> Option<Unit> {
        loop {
            if let Some(unit) = self.take() {
                return Some(unit);
            }
            self.buf.drain(..self.at);
            self.at = 0;
            if !self.pause.is_zero() {
                std::thread::sleep(self.pause);
            }
            match r.read(&mut self.chunk) {
                Ok(0) => return None,
                Ok(n) => self.buf.extend_from_slice(&self.chunk[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    return None;
                }
                Err(e) => panic!("peer read: {e}"),
            }
        }
    }

    fn take(&mut self) -> Option<Unit> {
        let b = &self.buf[self.at..];
        let (unit, used) = if *b.first()? == b'$' {
            let len = usize::from(u16::from_be_bytes([*b.get(2)?, *b.get(3)?]));
            let payload = b.get(4..4 + len)?.to_vec();
            let channel = b[1];
            (Unit::Frame { channel, payload }, 4 + len)
        } else {
            let end = b.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
            (Unit::Request(b[..end].to_vec()), end)
        };
        self.at += used;
        Some(unit)
    }

    /// The stream's end cut no unit in two.
    fn ended_between_units(&self) -> bool {
        self.at == self.buf.len()
    }
}

/// `unit` is `frame`, whole and unchanged.
fn is_frame(unit: &Unit, frame: &[u8]) -> bool {
    matches!(unit, Unit::Frame { channel: 0, payload } if payload[..] == frame[4..])
}

/// `unit` is the media frame `seq` of `len` bytes, whole and unchanged.
fn assert_media(unit: &Unit, seq: u32, len: usize) {
    assert!(
        is_frame(unit, &media_frame(seq, len)),
        "expected media frame {seq} of {len} bytes, got {unit:?}"
    );
}

/// `head` is one whole request and nothing more.
fn whole_request(head: &[u8]) -> RtspRequest {
    let (request, used) = RtspRequest::parse(head).expect("a whole request");
    assert_eq!(used, head.len(), "bytes behind the request");
    request
}

#[test]
fn frames_arrive_whole_and_in_order_between_rtsp_messages() {
    const FRAMES: u32 = 200;
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let _unhang = unhang(&h.cancel);
    let mut rest = Vec::new();
    recording(&h, &mut peer, &mut rest);
    for seq in 0..FRAMES / 2 {
        send(&h, media_frame(seq, SMALL)).unwrap();
    }
    let rx = h.request(options());
    for seq in FRAMES / 2..FRAMES {
        send(&h, media_frame(seq, SMALL)).unwrap();
    }
    let mut units = Units::new(rest);
    let (mut frames, mut requests) = (0, 0);
    while frames < FRAMES || requests == 0 {
        match units.next(&mut peer).expect("the stream is open") {
            Unit::Request(head) => {
                assert_eq!(whole_request(&head).method, RtspMethod::Options);
                requests += 1;
                peer.write_all(&ok_reply(cseq(&head))).unwrap();
            }
            frame => {
                assert_media(&frame, frames, SMALL);
                frames += 1;
            }
        }
    }
    assert_eq!(requests, 1);
    assert!(matches!(next(&rx), Progress::Written));
    assert_eq!(done_ok(next(&rx)).status, 200);
    assert_eq!(h.handle.drain_media(&h.cancel), Ok(()));
    let media = h.media.clone();
    h.stop_and_join();
    assert_eq!(media.dropped_at_close(), 0);
}

/// Within one write pass the client's own request goes ahead of the media
/// moved onto the stream in that pass: a request submitted with frames
/// already queued does not wait behind them.
#[test]
fn a_request_is_written_ahead_of_media_moved_in_the_same_pass() {
    const FRAMES: u32 = 8;
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let _unhang = unhang(&h.cancel);
    let mut rest = Vec::new();
    recording(&h, &mut peer, &mut rest);
    // Straight onto the queue, without the wake a send gives: the thread
    // sleeps on with the stream idle, and finds the frames (well under the
    // watermark) and the request together when the request wakes it.
    for seq in 0..FRAMES {
        h.media.push(media_frame(seq, SMALL), &h.cancel).unwrap();
    }
    let rx = h.request(options());
    let mut units = Units::new(rest);
    match units.next(&mut peer).expect("the stream is open") {
        Unit::Request(head) => {
            assert_eq!(whole_request(&head).method, RtspMethod::Options);
            peer.write_all(&ok_reply(cseq(&head))).unwrap();
        }
        frame => panic!("media was written ahead of the request: {frame:?}"),
    }
    for seq in 0..FRAMES {
        let unit = units.next(&mut peer).expect("the stream is open");
        assert_media(&unit, seq, SMALL);
    }
    assert!(matches!(next(&rx), Progress::Written));
    assert_eq!(done_ok(next(&rx)).status, 200);
    h.stop_and_join();
}

/// The cancel fires a moment after the queue fills, long before the
/// stream's stall time (`CONTROL_WRITE_TIMEOUT`), so it is the cancel that
/// ends the send: a stall would end it `Ended`. How soon a parked wait
/// sees a cancel is pinned on the queue itself, where the wait's phase is
/// known.
#[test]
fn a_peer_that_reads_nothing_parks_send_and_a_cancel_releases_it_promptly() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let _unhang = unhang(&h.cancel);
    recording(&h, &mut peer, &mut Vec::new());
    let canceller = std::thread::spawn({
        let (media, cancel) = (h.media.clone(), h.cancel.clone());
        move || {
            wait_until("the media queue full", || media.len() == MEDIA_QUEUE_BOUND);
            // Pacing: the sender is parked by now, not between two sends.
            std::thread::sleep(Duration::from_millis(20));
            let at = Instant::now();
            cancel.cancel();
            at
        }
    });
    let end = send_until_refused(&h, &media_frame(0, BIG));
    let returned = Instant::now();
    assert_eq!(end, MediaWaitEnd::Cancelled);
    let cancelled = canceller.join().unwrap();
    let took = returned.saturating_duration_since(cancelled);
    assert!(
        took <= scaled(Duration::from_secs(1)),
        "the parked send returned {took:?} after the cancel"
    );
    // The cancel ends the wait, not the session.
    assert_eq!(h.end_reason.get(), None);
    h.stop_and_join();
}

#[test]
fn a_peer_that_reads_nothing_parks_send_and_the_stall_ends_it_broken() {
    let (stream, mut peer) = plain_pair();
    let h = drive_with(stream, stall());
    let _unhang = unhang(&h.cancel);
    recording(&h, &mut peer, &mut Vec::new());
    // Pings are queued behind media the peer does not take: the bytes that
    // stall are the media's, and the end is not a keepalive's.
    h.handle
        .send(Command::EnableKeepalive(Some(stall() / 3)))
        .unwrap();
    let t0 = Instant::now();
    let end = send_until_refused(&h, &media_frame(0, BIG));
    // At once: the reason is recorded before the queue closes.
    let reason = h.end_reason.get();
    assert_eq!(end, MediaWaitEnd::Ended);
    match reason {
        Some(EndReason::TransportFailed { msg }) => assert!(msg.contains("no progress"), "{msg}"),
        other => panic!("expected TransportFailed, got {other:?}"),
    }
    assert!(t0.elapsed() >= stall(), "stalled after {:?}", t0.elapsed());
    assert!(h.session_dead.load(Ordering::Acquire));
    let dropped = h.handle.frames_dropped_at_close();
    assert!(
        (1..=MEDIA_QUEUE_BOUND as u64).contains(&dropped),
        "{dropped} frames dropped"
    );
    h.stop_and_join();
}

/// A connected pair whose kernel buffers hold a few KiB, so what the peer
/// does not read stays with the client.
#[cfg(target_os = "linux")]
fn small_buffer_pair() -> (TcpStream, TcpStream) {
    use socket2::{Domain, Socket, Type};
    let sock = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    // Before `listen`, so the accepted socket inherits it.
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
    let (peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(scaled(Duration::from_secs(5))))
        .unwrap();
    (client, peer)
}

#[cfg(target_os = "linux")]
#[test]
fn a_slow_reader_bounds_the_queue_and_requests_still_complete() {
    const FRAMES: u32 = 3000;
    const LEN: usize = 100;
    let (client, mut peer) = small_buffer_pair();
    let h = drive(Stream::from_plain(client).unwrap());
    let _unhang = unhang(&h.cancel);
    let mut rest = Vec::new();
    recording(&h, &mut peer, &mut rest);
    // The peer reads 4 KiB every 20 ms and answers the request it finds.
    let reader = std::thread::spawn(move || {
        let mut units = Units::paced(rest, 4096, Duration::from_millis(20));
        let mut frames = 0;
        while let Some(unit) = units.next(&mut peer) {
            match unit {
                Unit::Request(head) => peer.write_all(&ok_reply(cseq(&head))).unwrap(),
                frame => {
                    assert_media(&frame, frames, LEN);
                    frames += 1;
                }
            }
        }
        frames
    });
    let mut reply = None;
    for seq in 0..FRAMES {
        if seq == FRAMES / 2 {
            let (progress, rx) = mpsc::channel();
            h.handle
                .send(Command::Request {
                    spec: options(),
                    route: None,
                    reply_deadline: Some(Instant::now() + scaled(Duration::from_secs(3))),
                    progress,
                })
                .unwrap();
            reply = Some(rx);
        }
        send(&h, media_frame(seq, LEN)).unwrap();
    }
    let rx = reply.unwrap();
    assert!(matches!(next(&rx), Progress::Written));
    assert_eq!(done_ok(next(&rx)).status, 200);
    assert_eq!(
        h.media.high_water(),
        MEDIA_QUEUE_BOUND,
        "the queue filled, and held no more than its bound"
    );
    h.stop_and_join();
    assert!(reader.join().unwrap() > 0);
}

/// Against a peer that takes bytes slowly without ever stalling the write,
/// what waits on the stream stays bounded however many pings fall due: the
/// media watermark and one frame, one request and one ping.
#[cfg(target_os = "linux")]
#[test]
fn pings_do_not_pile_up_behind_a_slow_reader() {
    const FRAMES: u32 = 1000;
    const LEN: usize = 200;
    /// More than a request or a ping of this client takes.
    const REQUEST_MAX: usize = 128;
    const PING_INTERVAL: Duration = Duration::from_millis(5);
    let (client, mut peer) = small_buffer_pair();
    // The peer never stops reading, so the stall rule stays out of this:
    // its time is far past the test's.
    let h = drive_with(
        Stream::from_plain(client).unwrap(),
        scaled(Duration::from_secs(60)),
    );
    let _unhang = unhang(&h.cancel);
    let mut rest = Vec::new();
    recording(&h, &mut peer, &mut rest);
    // The peer reads 64 bytes every 20 ms until it is told to read fast,
    // and ends once it has read the request and a ping.
    let fast = Arc::new(AtomicBool::new(false));
    let reader = std::thread::spawn({
        let fast = fast.clone();
        move || {
            let mut units = Units::paced(rest, 64, Duration::from_millis(20));
            let (mut frames, mut requests) = (0, 0);
            while requests < 2 {
                if fast.load(Ordering::Acquire) && !units.pause.is_zero() {
                    units.pause = Duration::ZERO;
                    units.chunk = vec![0; 64 * 1024];
                }
                match units.next(&mut peer).expect("the stream is open") {
                    Unit::Request(head) => {
                        assert_eq!(whole_request(&head).method, RtspMethod::Options);
                        assert!(head.len() <= REQUEST_MAX, "{} bytes", head.len());
                        requests += 1;
                    }
                    frame => {
                        assert_media(&frame, frames, LEN);
                        frames += 1;
                    }
                }
            }
            peer
        }
    });
    // More than the kernel and the stream hold: the rest waits in the
    // queue, below its bound.
    for seq in 0..FRAMES {
        send(&h, media_frame(seq, LEN)).unwrap();
    }
    wait_until("the stream at its watermark", || {
        h.media.unsent_high_water() >= MEDIA_WATERMARK
    });
    h.handle
        .send(Command::EnableKeepalive(Some(PING_INTERVAL)))
        .unwrap();
    let _rx = h.request(options());
    // A hundred pings fall due while the first is still behind the media.
    // The sleep decides nothing: the bound holds at any time.
    std::thread::sleep(PING_INTERVAL * 100);
    let held = h.media.unsent_high_water();
    let bound = MEDIA_WATERMARK + (LEN + 2 * REQUEST_MAX) as u64;
    assert!(
        held <= bound,
        "{held} bytes waited on the stream, more than {bound}"
    );
    assert_eq!(h.end_reason.get(), None);
    fast.store(true, Ordering::Release);
    let _peer = reader.join().unwrap();
    h.stop_and_join();
}

/// Only an unwritten ping holds the next one back: a ping that falls due
/// while media, and no ping, waits on the stream is queued behind that
/// media, ahead of what is still in the queue. The peer's pace decides
/// nothing: whenever the client's thread first runs after the ping is due,
/// frames are still waiting to be moved, since moving them takes rounds of
/// that thread and the peer's sleeps make the reading far slower than the
/// ping interval.
#[cfg(target_os = "linux")]
#[test]
fn a_ping_due_behind_unwritten_media_is_queued_ahead_of_the_rest() {
    const FRAMES: u32 = 1000;
    const LEN: usize = 200;
    const PING_INTERVAL: Duration = Duration::from_millis(5);
    let (client, mut peer) = small_buffer_pair();
    // The peer never stops reading, so the stall rule stays out of this.
    let h = drive_with(
        Stream::from_plain(client).unwrap(),
        scaled(Duration::from_secs(60)),
    );
    let _unhang = unhang(&h.cancel);
    let mut rest = Vec::new();
    recording(&h, &mut peer, &mut rest);
    // The peer reads nothing yet: the stream fills to its watermark and
    // the rest waits in the queue, below its bound.
    for seq in 0..FRAMES {
        send(&h, media_frame(seq, LEN)).unwrap();
    }
    wait_until("the stream at its watermark", || {
        h.media.unsent_high_water() >= MEDIA_WATERMARK
    });
    h.handle
        .send(Command::EnableKeepalive(Some(PING_INTERVAL)))
        .unwrap();
    // 1 KiB every 5 ms: about a second for the 200 kB.
    let mut units = Units::paced(rest, 1024, Duration::from_millis(5));
    let mut frames = 0;
    loop {
        match units.next(&mut peer).expect("the stream is open") {
            Unit::Request(head) => {
                assert_eq!(whole_request(&head).method, RtspMethod::Options);
                break;
            }
            frame => {
                assert_media(&frame, frames, LEN);
                frames += 1;
            }
        }
    }
    assert!(
        frames < FRAMES,
        "the ping came behind all {frames} frames: it waited for the stream to empty"
    );
    assert_eq!(h.end_reason.get(), None);
    h.stop_and_join();
}

#[test]
fn a_request_overtakes_queued_media() {
    const FRAMES: u32 = 1000;
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let _unhang = unhang(&h.cancel);
    let mut rest = Vec::new();
    recording(&h, &mut peer, &mut rest);
    // The peer reads nothing yet: most of the frames wait in the queue.
    for seq in 0..FRAMES {
        send(&h, media_frame(seq, BIG)).unwrap();
    }
    let rx = h.request(RequestSpec {
        method: RtspMethod::Teardown,
        ..options()
    });
    let mut units = Units::new(rest);
    let mut before = 0;
    let head = loop {
        match units.next(&mut peer).expect("the stream is open") {
            Unit::Request(head) => break head,
            frame => {
                assert_media(&frame, before, BIG);
                before += 1;
            }
        }
    };
    assert_eq!(whole_request(&head).method, RtspMethod::Teardown);
    assert!(
        before < FRAMES,
        "every frame was written before the TEARDOWN"
    );
    peer.write_all(&ok_reply(cseq(&head))).unwrap();
    assert!(matches!(next(&rx), Progress::Written));
    assert_eq!(done_ok(next(&rx)).status, 200);
    assert_eq!(
        h.handle.frames_dropped_at_close(),
        u64::from(FRAMES - before)
    );
    assert_eq!(
        send(&h, media_frame(FRAMES, SMALL)),
        Err(MediaWaitEnd::Ended)
    );
    // The frames behind the TEARDOWN were not written.
    h.stop_and_join();
    let after = units.next(&mut peer);
    assert!(after.is_none(), "{after:?} followed the TEARDOWN");
    assert!(units.ended_between_units());
}

/// A TEARDOWN ends the publish session when it is submitted, not when it
/// is answered: the queue closes at once and for good, whatever the reply
/// and without one. The thread records no reason for it, so a sender that
/// sees the close reads what the TEARDOWN's submitter recorded before
/// submitting, which may be nothing; only an accepted reply records the
/// clean end.
#[test]
fn the_queue_closes_at_teardown_submission_whatever_its_reply() {
    // The peer's answer, and the reason the submitter records first.
    let cases: [(Option<u16>, Option<EndReason>); 3] = [
        (Some(200), None),
        (Some(501), None),
        (None, Some(EndReason::Cancelled)),
    ];
    for (reply, recorded) in cases {
        let (stream, mut peer) = plain_pair();
        let h = drive(stream);
        let _unhang = unhang(&h.cancel);
        let mut rest = Vec::new();
        recording(&h, &mut peer, &mut rest);
        // From a second thread, once the sender is parked on a full queue.
        // The peer has read nothing by then.
        let submitter = std::thread::spawn({
            let (commands, waker) = (h.handle.commands.clone(), h.handle.waker.clone());
            let (media, end_reason) = (h.media.clone(), h.end_reason.clone());
            let recorded = recorded.clone();
            move || {
                wait_until("the media queue full", || media.len() == MEDIA_QUEUE_BOUND);
                // Pacing: the sender is parked by now, not between two sends.
                std::thread::sleep(Duration::from_millis(20));
                if let Some(reason) = recorded {
                    record_end(&end_reason, reason);
                }
                let (progress, rx) = mpsc::channel();
                commands
                    .try_send(Command::Request {
                        spec: RequestSpec {
                            method: RtspMethod::Teardown,
                            ..options()
                        },
                        route: None,
                        reply_deadline: None,
                        progress,
                    })
                    .unwrap();
                waker.wake().unwrap();
                rx
            }
        });
        let frame = media_frame(0, BIG);
        let mut queued = 0u64;
        let end = loop {
            match send(&h, frame.clone()) {
                Ok(()) => queued += 1,
                Err(end) => break end,
            }
        };
        // At once: only the submission has happened.
        let seen = h.end_reason.get();
        assert_eq!(end, MediaWaitEnd::Ended, "reply {reply:?}");
        assert_eq!(seen, recorded, "reply {reply:?}");
        let rx = submitter.join().unwrap();

        // The frames already on the stream go out ahead of the TEARDOWN;
        // the ones in the queue were dropped and counted.
        let mut units = Units::new(rest);
        let mut before = 0u64;
        let head = loop {
            match units.next(&mut peer).expect("the stream is open") {
                Unit::Request(head) => break head,
                frame => {
                    assert_media(&frame, 0, BIG);
                    before += 1;
                }
            }
        };
        assert_eq!(whole_request(&head).method, RtspMethod::Teardown);
        let dropped = h.handle.frames_dropped_at_close();
        assert!(dropped > 0, "reply {reply:?}");
        assert_eq!(dropped, queued - before, "reply {reply:?}");

        assert!(matches!(next(&rx), Progress::Written));
        if let Some(status) = reply {
            let line = format!("RTSP/1.0 {status} Reply\r\nCSeq: {}\r\n\r\n", cseq(&head));
            peer.write_all(line.as_bytes()).unwrap();
            assert_eq!(done_ok(next(&rx)).status, status);
        }
        let after = if reply == Some(200) {
            Some(EndReason::CleanTeardown)
        } else {
            recorded
        };
        assert_eq!(h.end_reason.get(), after, "reply {reply:?}");
        // Closed for good, with nothing more counted.
        assert_eq!(send(&h, frame), Err(MediaWaitEnd::Ended), "reply {reply:?}");
        assert_eq!(h.handle.frames_dropped_at_close(), dropped);
        h.stop_and_join();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn drain_returns_when_the_peer_has_every_frame() {
    // Together less than the watermark, so every frame leaves the queue,
    // and more than the kernel's pinned buffers hold.
    const FRAMES: u32 = 45;
    let (client, mut peer) = small_buffer_pair();
    let h = drive(Stream::from_plain(client).unwrap());
    let _unhang = unhang(&h.cancel);
    let mut rest = Vec::new();
    recording(&h, &mut peer, &mut rest);
    for seq in 0..FRAMES {
        send(&h, media_frame(seq, SMALL)).unwrap();
    }
    // What the kernel did not take now waits on the stream.
    wait_until("the media queue empty", || h.media.len() == 0);
    let reading = Arc::new(AtomicBool::new(false));
    let reader = std::thread::spawn({
        let reading = reading.clone();
        move || {
            // Pacing: a drain that took the empty queue for the end would
            // have returned by now.
            std::thread::sleep(Duration::from_millis(300));
            reading.store(true, Ordering::Release);
            let mut units = Units::new(rest);
            for seq in 0..FRAMES {
                assert_media(&units.next(&mut peer).expect("a frame"), seq, SMALL);
            }
            peer
        }
    });
    assert_eq!(h.handle.drain_media(&h.cancel), Ok(()));
    assert!(
        reading.load(Ordering::Acquire),
        "the drain returned with bytes the peer had not begun to read"
    );
    let _peer = reader.join().unwrap();
    h.stop_and_join();
}

/// As for the parked send: the cancel fires long before the stream's
/// stall time, so it is the cancel that ends the drain.
#[test]
fn drain_ends_on_a_cancel_promptly_against_a_one_byte_a_second_reader() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let _unhang = unhang(&h.cancel);
    recording(&h, &mut peer, &mut Vec::new());
    let frame = media_frame(0, BIG);
    for _ in 0..MEDIA_QUEUE_BOUND - 1 {
        send(&h, frame.clone()).unwrap();
    }
    let (stop, stopped) = mpsc::channel::<()>();
    let reader = std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        while peer.read(&mut byte).is_ok_and(|n| n == 1)
            && stopped.recv_timeout(Duration::from_secs(1)) == Err(RecvTimeoutError::Timeout)
        {
        }
    });
    let (draining, drain_begun) = mpsc::channel::<()>();
    let canceller = std::thread::spawn({
        let cancel = h.cancel.clone();
        move || {
            drain_begun.recv().unwrap();
            // Pacing: the drain is parked by now.
            std::thread::sleep(Duration::from_millis(50));
            let at = Instant::now();
            cancel.cancel();
            at
        }
    });
    draining.send(()).unwrap();
    let end = h.handle.drain_media(&h.cancel);
    let returned = Instant::now();
    assert_eq!(end, Err(MediaWaitEnd::Cancelled));
    let took = returned.saturating_duration_since(canceller.join().unwrap());
    assert!(
        took <= scaled(Duration::from_secs(1)),
        "the drain returned {took:?} after the cancel"
    );
    stop.send(()).unwrap();
    reader.join().unwrap();
    h.stop_and_join();
}

/// Media does not starve the pings: with the queue kept full, one ping
/// after another still reaches the peer. The sends go on until the peer
/// has seen eight (1.6 s of pings at one every 200 ms); only a deadline
/// far beyond that fails the test.
#[test]
fn a_saturating_media_load_does_not_starve_the_pings() {
    const PINGS: u64 = 8;
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let _unhang = unhang(&h.cancel);
    let mut rest = Vec::new();
    recording(&h, &mut peer, &mut rest);
    h.handle
        .send(Command::EnableKeepalive(Some(Duration::from_millis(200))))
        .unwrap();
    let pings = Arc::new(AtomicU64::new(0));
    // One large frame again and again: a send costs far less than writing
    // and reading the frame, so the queue fills and stays full.
    let frame = media_frame(0, BIG);
    // The peer reads as fast as it can and answers every ping.
    let reader = std::thread::spawn({
        let (pings, frame) = (pings.clone(), frame.clone());
        move || {
            let mut units = Units::new(rest);
            while let Some(unit) = units.next(&mut peer) {
                match unit {
                    Unit::Request(head) => {
                        assert_eq!(whole_request(&head).method, RtspMethod::Options);
                        pings.fetch_add(1, Ordering::Release);
                        if peer.write_all(&ok_reply(cseq(&head))).is_err() {
                            break;
                        }
                    }
                    other => assert!(is_frame(&other, &frame), "not the frame sent: {other:?}"),
                }
            }
        }
    });
    let seen = || pings.load(Ordering::Acquire);
    let deadline = Instant::now() + scaled(Duration::from_secs(10));
    while seen() < PINGS {
        assert!(
            Instant::now() < deadline && send(&h, frame.clone()).is_ok(),
            "{} of {PINGS} pings reached the peer under the load",
            seen()
        );
    }
    assert_eq!(
        h.media.high_water(),
        MEDIA_QUEUE_BOUND,
        "the load did not fill the queue"
    );
    h.stop_and_join();
    reader.join().unwrap();
}

#[test]
fn an_inbound_frame_flood_does_not_delay_outbound_media() {
    const FRAMES: u32 = 2000;
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let _unhang = unhang(&h.cancel);
    let mut rest = Vec::new();
    recording(&h, &mut peer, &mut rest);
    // Frames on the RTP channel of a route this side sends on: each is
    // read, counted and dropped.
    let stop = Arc::new(AtomicBool::new(false));
    let flood = std::thread::spawn({
        let mut peer = peer.try_clone().unwrap();
        let stop = stop.clone();
        move || {
            let burst: Vec<u8> = (0..64).flat_map(|seq| rtp_frame(seq, 1000)).collect();
            while !stop.load(Ordering::Acquire) && peer.write_all(&burst).is_ok() {}
        }
    });
    let flooded = || h.frames_dropped_inbound.load(Ordering::Relaxed);
    wait_until("the flood being read", || flooded() > 0);
    let reader = std::thread::spawn(move || {
        let mut units = Units::new(rest);
        for seq in 0..FRAMES {
            assert_media(&units.next(&mut peer).expect("a frame"), seq, SMALL);
        }
        peer
    });
    let flooded_before = flooded();
    let t0 = Instant::now();
    for seq in 0..FRAMES {
        send(&h, media_frame(seq, SMALL)).unwrap();
    }
    assert_eq!(h.handle.drain_media(&h.cancel), Ok(()));
    let _peer = reader.join().unwrap();
    let took = t0.elapsed();
    assert!(
        took < scaled(Duration::from_secs(5)),
        "the frames took {took:?} to reach the peer"
    );
    assert!(
        flooded() > flooded_before,
        "nothing of the flood was read while the frames went out"
    );
    stop.store(true, Ordering::Release);
    h.stop_and_join();
    flood.join().unwrap();
}

#[test]
fn the_queue_closes_when_the_session_ends() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let _unhang = unhang(&h.cancel);
    recording(&h, &mut peer, &mut Vec::new());
    // The peer's FIN, once the sender is parked on a full queue.
    let closer = std::thread::spawn({
        let media = h.media.clone();
        move || {
            wait_until("the media queue full", || media.len() == MEDIA_QUEUE_BOUND);
            // Pacing: the sender is parked by now, not between two sends.
            std::thread::sleep(Duration::from_millis(20));
            peer.shutdown(Shutdown::Write).unwrap();
            peer
        }
    });
    let frame = media_frame(0, BIG);
    let end = send_until_refused(&h, &frame);
    // At once: the reason is recorded before the queue closes.
    let reason = h.end_reason.get();
    assert_eq!(end, MediaWaitEnd::Ended);
    assert_eq!(reason, Some(EndReason::CleanTeardown));
    assert_eq!(send(&h, frame), Err(MediaWaitEnd::Ended));
    assert_eq!(
        h.handle.drain_media(&h.cancel),
        Err(MediaWaitEnd::Ended),
        "a drain after the end"
    );
    let dropped = h.handle.frames_dropped_at_close();
    assert!(
        (1..=MEDIA_QUEUE_BOUND as u64).contains(&dropped),
        "{dropped} frames dropped"
    );
    let _peer = closer.join().unwrap();
    h.stop_and_join();
}

#[test]
fn media_is_not_moved_before_recording() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let _unhang = unhang(&h.cancel);
    let mut rest = Vec::new();
    announced(&h, &mut peer, &mut rest, 1);
    route(&h, &mut peer, &mut rest, (0, RouteKind::Push), (0, 1), None);
    for seq in 0..3 {
        send(&h, media_frame(seq, SMALL)).unwrap();
    }
    // The thread handles this request after the frames were queued, so it
    // has passed over them; a frame written ahead of the request would
    // spoil its head.
    round_trip(&h, &mut peer, &mut rest);
    assert!(rest.is_empty(), "bytes behind the request");
    assert_eq!(h.media.len(), 3);
    answered(&h, &mut peer, &mut rest, RtspMethod::Record);
    let mut units = Units::new(rest);
    for seq in 0..3 {
        assert_media(&units.next(&mut peer).expect("a frame"), seq, SMALL);
    }
    h.stop_and_join();
}

/// Frames that wait when the session starts recording all go out behind
/// the RECORD reply, though they are several times the watermark and
/// nothing wakes the thread again: no further send, no request, and no
/// event from a socket that took every byte it was given.
#[test]
fn queued_media_past_the_watermark_goes_out_without_another_wake() {
    const FRAMES: u32 = 200;
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let _unhang = unhang(&h.cancel);
    let mut rest = Vec::new();
    announced(&h, &mut peer, &mut rest, 1);
    route(&h, &mut peer, &mut rest, (0, RouteKind::Push), (0, 1), None);
    for seq in 0..FRAMES {
        send(&h, media_frame(seq, SMALL)).unwrap();
    }
    answered(&h, &mut peer, &mut rest, RtspMethod::Record);
    let mut units = Units::new(rest);
    for seq in 0..FRAMES {
        assert_media(&units.next(&mut peer).expect("a frame"), seq, SMALL);
    }
    assert_eq!(h.handle.drain_media(&h.cancel), Ok(()));
    h.stop_and_join();
}

/// A paused session still takes what is sent: the pause is the caller's
/// to keep.
#[test]
fn media_is_written_while_paused() {
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let _unhang = unhang(&h.cancel);
    let mut rest = Vec::new();
    recording(&h, &mut peer, &mut rest);
    answered(&h, &mut peer, &mut rest, RtspMethod::Pause);
    send(&h, media_frame(0, SMALL)).unwrap();
    let mut units = Units::new(rest);
    assert_media(&units.next(&mut peer).expect("a frame"), 0, SMALL);
    h.stop_and_join();
}

#[test]
fn a_stop_closes_the_queue_and_counts_what_was_queued() {
    let (stream, _peer) = plain_pair();
    let mut h = drive(stream);
    // Not recording: the frames stay queued.
    for seq in 0..3 {
        send(&h, media_frame(seq, SMALL)).unwrap();
    }
    h.handle.stop();
    assert!(h.handle.join_within(scaled(Duration::from_secs(2))));
    assert_eq!(send(&h, media_frame(3, SMALL)), Err(MediaWaitEnd::Ended));
    assert_eq!(h.handle.drain_media(&h.cancel), Err(MediaWaitEnd::Ended));
    assert_eq!(h.handle.frames_dropped_at_close(), 3);
    assert_eq!(h.end_reason.get(), Some(EndReason::Cancelled));
}

/// Whoever sees the queue close reads the end reason at once. The window
/// between the two is a few instructions wide, so the scenario repeats on
/// a fresh connection each time, with a drain parked on the queue.
#[test]
fn the_outbound_queue_closes_after_the_end_reason_is_recorded() {
    for iteration in 0..50 {
        let (stream, peer) = plain_pair();
        let h = drive(stream);
        let _unhang = unhang(&h.cancel);
        // Not recording: the frame stays queued and the drain waits.
        send(&h, media_frame(0, SMALL)).unwrap();
        let closer = std::thread::spawn(move || drop(peer));
        assert_eq!(
            h.handle.drain_media(&h.cancel),
            Err(MediaWaitEnd::Ended),
            "iteration {iteration}"
        );
        assert_eq!(
            h.end_reason.get(),
            Some(EndReason::CleanTeardown),
            "iteration {iteration}: the queue closed before the reason was recorded"
        );
        closer.join().unwrap();
        h.stop_and_join();
    }
}

#[cfg(feature = "tls")]
mod tls {
    use super::super::tls::{PeerStream, connect, hold};
    use super::*;
    use crate::rtsp::client::tls::test_peer::tls_test_peer;

    /// Over TLS a byte counts as written only once rustls has passed on
    /// everything it was given, so the unsent count falls in steps. Frames
    /// worth many times the watermark still all go out, in order, and the
    /// drain ends.
    #[test]
    fn tls_media_past_the_watermark_all_reaches_the_peer() {
        const FRAMES: u32 = 400;
        let (port, roots, peer) = tls_test_peer(
            |mut s: PeerStream| {
                let mut rest = Vec::new();
                // ANNOUNCE, SETUP and RECORD.
                for _ in 0..3 {
                    let head = read_head(&mut s, &mut rest);
                    let reply = if head.starts_with(b"SETUP ") {
                        setup_reply(cseq(&head), "RTP/AVP/TCP;unicast;interleaved=0-1")
                    } else {
                        ok_reply(cseq(&head))
                    };
                    s.write_all(&reply).unwrap();
                    s.flush().unwrap();
                }
                let mut units = Units::new(rest);
                for seq in 0..FRAMES {
                    assert_media(&units.next(&mut s).expect("a frame"), seq, SMALL);
                }
                hold(&mut s);
            },
            |_| {},
        );
        let h = drive(Stream::from_tls(connect(port, roots)).unwrap());
        let _unhang = unhang(&h.cancel);
        let done = |rx: mpsc::Receiver<Progress>| loop {
            if let Progress::Done(result) = next(&rx) {
                break result.unwrap();
            }
        };
        done(h.request(RequestSpec {
            method: RtspMethod::Announce,
            announce_tracks: Some(1),
            ..options()
        }));
        assert!(done(h.setup(0, RouteKind::Push, None).0).setup.is_some());
        done(h.request(RequestSpec {
            method: RtspMethod::Record,
            ..options()
        }));
        for seq in 0..FRAMES {
            send(&h, media_frame(seq, SMALL)).unwrap();
        }
        assert_eq!(h.handle.drain_media(&h.cancel), Ok(()));
        h.stop_and_join();
        peer.join().unwrap();
    }
}
