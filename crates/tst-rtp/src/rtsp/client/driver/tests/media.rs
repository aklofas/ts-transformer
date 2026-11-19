//! Outbound media through the client's thread: its order on the wire
//! among the requests, the back-pressure on a sender, and what ends a
//! waiting send and a drain.

use super::*;
use std::net::Shutdown;

use crate::rtsp::client::media_queue::MEDIA_QUEUE_BOUND;

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

/// `unit` is the media frame `seq` of `len` bytes, whole and unchanged.
fn assert_media(unit: &Unit, seq: u32, len: usize) {
    match unit {
        Unit::Frame {
            channel: 0,
            payload,
        } if payload[..] == media_frame(seq, len)[4..] => {}
        other => panic!("expected media frame {seq} of {len} bytes, got {other:?}"),
    }
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

#[test]
fn a_peer_that_reads_nothing_parks_send_and_a_cancel_releases_it_within_150_ms() {
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
        took <= scaled(Duration::from_millis(150)),
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

#[test]
fn drain_ends_on_a_cancel_within_150_ms_against_a_one_byte_a_second_reader() {
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
        took <= scaled(Duration::from_millis(150)),
        "the drain returned {took:?} after the cancel"
    );
    stop.send(()).unwrap();
    reader.join().unwrap();
    h.stop_and_join();
}

#[test]
fn pings_keep_their_cadence_under_a_saturating_load() {
    const LOAD: Duration = Duration::from_secs(2);
    let (stream, mut peer) = plain_pair();
    let h = drive(stream);
    let _unhang = unhang(&h.cancel);
    let mut rest = Vec::new();
    recording(&h, &mut peer, &mut rest);
    h.handle
        .send(Command::EnableKeepalive(Some(Duration::from_millis(200))))
        .unwrap();
    let pings = Arc::new(AtomicU64::new(0));
    // The peer reads as fast as it can and answers every ping.
    let reader = std::thread::spawn({
        let pings = pings.clone();
        move || {
            let mut units = Units::new(rest);
            let mut frames = 0;
            while let Some(unit) = units.next(&mut peer) {
                match unit {
                    Unit::Request(head) => {
                        assert_eq!(whole_request(&head).method, RtspMethod::Options);
                        pings.fetch_add(1, Ordering::Release);
                        if peer.write_all(&ok_reply(cseq(&head))).is_err() {
                            break;
                        }
                    }
                    frame => {
                        assert_media(&frame, frames, SMALL);
                        frames += 1;
                    }
                }
            }
            frames
        }
    });
    let t0 = Instant::now();
    let mut seq = 0;
    while t0.elapsed() < LOAD {
        send(&h, media_frame(seq, SMALL)).unwrap();
        seq += 1;
    }
    let pings = pings.load(Ordering::Acquire);
    assert!(
        pings >= 8,
        "{pings} pings reached the peer in {LOAD:?} at one every 200 ms"
    );
    h.stop_and_join();
    let frames = reader.join().unwrap();
    assert!(
        frames > MEDIA_QUEUE_BOUND as u32,
        "only {frames} frames: the load did not fill the queue"
    );
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
