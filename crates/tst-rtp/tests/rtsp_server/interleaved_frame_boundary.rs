//! A TCP-interleaved session whose fanout is parked mid-frame on a full
//! socket must not have that frame cut by a second PLAY: the RTSP
//! response may only follow a COMPLETE `$` frame, never a partial one.
//! And a fanout retired that way must still die with its session.
//!
//! The old fanout writes RTP frames under the session's shared TCP writer;
//! a client that stops reading fills the kernel buffers until that
//! `write_all` parks part-way through a frame. A PLAY sent on the same
//! (full-duplex) connection then replaces the fanout. The server used to
//! `abort()` the parked task, which released the writer with the frame's
//! tail still owed; the `RTSP/1.0 200 OK` text landed where the client's
//! interleaved parser expected RTP payload, and the connection — which the
//! server keeps serving — was desynchronised from there on. Now the
//! replacement cancels via the token the fanout checks between frames, so
//! the parked task finishes its frame and exits before anything else is
//! written.
//!
//! The verdict is the wire parse (every frame whole, then the response),
//! not any timing: the parse accepts a complete frame or no frame before
//! the response, and rejects response bytes inside a frame's payload.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use tst_core::mpegts::common::Pts90khz;
use tst_rtp::{MountHandle, RtspServer, RtspServerBuilder};

use crate::fixtures::raw_rtsp::{header, make_muxer_cfg, request, session_id};

/// Connect with a deliberately tiny receive buffer so the kernel runs out
/// of room after a few KiB once the client stops reading. Set BEFORE
/// `connect` so the window is advertised small from the handshake on.
fn connect_with_small_rcvbuf(port: u16) -> TcpStream {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
    // Kernels clamp this to their minimum; any clamp is still far below
    // the ~6 MB the tests push, so the write parks either way.
    sock.set_recv_buffer_size(4096).unwrap();
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
    sock.connect(&addr.into()).unwrap();
    sock.into()
}

/// A raw TCP-interleaved peer through SETUP + PLAY that then stops
/// reading while the mount pushes far more RTP than the socket pair can
/// hold, so the server-side fanout parks inside a frame write.
///
/// Returns the stalled socket, the mount URL, the session id and the RTP
/// channel the server announced.
fn stalled_interleaved_peer(
    server: &RtspServer,
    mount: &MountHandle,
) -> (TcpStream, String, String, u8) {
    let port = server.local_addr().unwrap().port();
    let url = format!("rtsp://127.0.0.1:{port}/live");

    let mut tcp = connect_with_small_rcvbuf(port);
    // Per-read bound only; nothing in these tests asserts through a duration.
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let setup = request(
        &mut tcp,
        &format!(
            "SETUP {url} RTSP/1.0\r\nCSeq: 1\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    );
    assert!(setup.starts_with("RTSP/1.0 200"), "{setup}");
    let sid = session_id(&setup);
    // The server allocates the channel pair itself (it need not echo the
    // client's `0-1`); every RTP frame must ride the one it announced.
    let transport = header(&setup, "Transport");
    let rtp_channel: u8 = transport
        .split(';')
        .find_map(|p| p.strip_prefix("interleaved="))
        .and_then(|v| v.split('-').next())
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no interleaved=A-B in {transport}"));
    let play = request(
        &mut tcp,
        &format!("PLAY {url} RTSP/1.0\r\nCSeq: 2\r\nSession: {sid}\r\n\r\n"),
    );
    assert!(play.starts_with("RTSP/1.0 200"), "{play}");
    assert_eq!(mount.peer_count(), 1);

    // The peer stops reading. Push more RTP than the socket pair can
    // buffer, so the fanout's `write_all` parks on a full socket —
    // part-way through a frame, since the kernel accepts bytes at its own
    // granularity, not ours. 200 × 32 KiB ≈ 6 MB is sized against Linux's
    // `tcp_wmem` ceiling of 4 MiB (the receive side was pinned small
    // above; ~1.8 MB was measured buffered at the park point); macOS and
    // Windows stall earlier, and the verdict is the wire parse either way.
    // Kept small so a slow debug-mode runner finishes the push well inside
    // the session's idle bound. Pushes never block (broadcast drops for a
    // lagging peer), so this loop is bounded by muxing speed.
    let mut nal = vec![0u8; 32 * 1024];
    nal[..5].copy_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65]);
    let mut pts: i64 = 0;
    for _ in 0..200 {
        mount.push_video(&nal, Pts90khz::new(pts), true).unwrap();
        pts += 3600;
    }
    (tcp, url, sid, rtp_channel)
}

/// What one byte-stream parse step found.
enum Parsed {
    /// A whole `$` frame; the payload carried no RTSP text.
    Frame,
    /// A complete RTSP response head.
    Response(String),
    /// Not enough bytes yet to decide.
    NeedMore,
}

/// Parse one unit at `buf[pos..]`: an interleaved frame (RFC 7826 §14,
/// `$ <channel> <len u16-BE> <payload>`) or an RTSP response head.
/// Panics with the offset on anything else — that is the failure mode this
/// test exists to catch.
fn parse_one(buf: &[u8], pos: usize, frames_seen: usize, rtp_channel: u8) -> (Parsed, usize) {
    let rest = &buf[pos..];
    if rest.is_empty() {
        return (Parsed::NeedMore, pos);
    }
    if rest[0] == b'$' {
        if rest.len() < 4 {
            return (Parsed::NeedMore, pos);
        }
        let channel = rest[1];
        let len = u16::from_be_bytes([rest[2], rest[3]]) as usize;
        assert_eq!(
            channel, rtp_channel,
            "frame {frames_seen} at offset {pos} is not on the RTP channel SETUP allocated"
        );
        if rest.len() < 4 + len {
            return (Parsed::NeedMore, pos);
        }
        let payload = &rest[4..4 + len];
        assert!(
            !payload.windows(5).any(|w| w == b"RTSP/"),
            "RTSP response text inside the payload of frame {frames_seen} at offset {pos}: \
             the frame was cut mid-write and the response spliced into it"
        );
        return (Parsed::Frame, pos + 4 + len);
    }
    const HEAD: &[u8] = b"RTSP/1.0 ";
    if HEAD.starts_with(&rest[..rest.len().min(HEAD.len())]) {
        // A (prefix of a) response head: complete when CRLFCRLF arrives.
        match rest.windows(4).position(|w| w == b"\r\n\r\n") {
            Some(end) => {
                let text = String::from_utf8_lossy(&rest[..end + 4]).into_owned();
                (Parsed::Response(text), pos + end + 4)
            }
            None => (Parsed::NeedMore, pos),
        }
    } else {
        panic!(
            "byte 0x{:02x} at offset {pos} after {frames_seen} whole frames is neither a `$` \
             frame nor an RTSP response: the stream is desynchronised",
            rest[0]
        );
    }
}

/// Describe the unparsed tail for a failure message: how many payload
/// bytes the open frame still owes, and whether RTSP text sits inside it.
fn describe_stall(rest: &[u8]) -> String {
    if rest.len() >= 4 && rest[0] == b'$' {
        let promised = u16::from_be_bytes([rest[2], rest[3]]) as usize;
        let present = rest.len() - 4;
        let spliced = rest[4..].windows(5).any(|w| w == b"RTSP/");
        format!(
            "stalled inside a frame promising {promised} payload bytes with {present} present \
             (RTSP text inside the partial payload: {spliced})"
        )
    } else {
        format!(
            "{} unparsed bytes: {:?}",
            rest.len(),
            &rest[..rest.len().min(24)]
        )
    }
}

#[test]
fn a_second_play_on_a_backpressured_interleaved_peer_never_splits_a_frame() {
    let server = RtspServer::bind("rtsp://127.0.0.1:0").unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let (mut tcp, url, sid, rtp_channel) = stalled_interleaved_peer(&server, &mount);

    // A second PLAY without PAUSE replaces the fanout. The request travels
    // on the connection's other direction, which the full receive side
    // does not block.
    tcp.write_all(format!("PLAY {url} RTSP/1.0\r\nCSeq: 3\r\nSession: {sid}\r\n\r\n").as_bytes())
        .unwrap();
    // Latch: once the server has dispatched the PLAY the new fanout is
    // subscribed while the parked old one still holds its subscription,
    // so the mount counts two peers. Bounded wait, not an assertion: the
    // wire parse below is the verdict (a server that killed the old task
    // outright never shows two, and fails the parse instead).
    let deadline = Instant::now() + Duration::from_secs(2);
    while mount.peer_count() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }

    // Resume reading and parse the whole stream up to the PLAY response:
    // whole frames only, then `RTSP/1.0 200` with CSeq 3.
    let mut buf = Vec::new();
    let mut pos = 0usize;
    let mut frames = 0usize;
    let mut chunk = [0u8; 4096];
    let response = loop {
        match parse_one(&buf, pos, frames, rtp_channel) {
            (Parsed::Frame, next) => {
                frames += 1;
                pos = next;
            }
            (Parsed::Response(text), _) => break text,
            (Parsed::NeedMore, _) => {
                let n = match tcp.read(&mut chunk) {
                    Ok(n) => n,
                    Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                        // The client is stalled: it is owed bytes that never
                        // arrive. A mid-frame splice shows up this way — the
                        // response (~100 B) is shorter than the frame tail the
                        // header promised, so the parser waits forever.
                        panic!(
                            "no more bytes within the read timeout after {frames} whole frames \
                             and no PLAY response; {}",
                            describe_stall(&buf[pos..])
                        );
                    }
                    Err(e) => panic!("read failed after {frames} whole frames: {e}"),
                };
                assert!(
                    n > 0,
                    "connection closed after {frames} whole frames with no PLAY response; {}",
                    describe_stall(&buf[pos..])
                );
                buf.extend_from_slice(&chunk[..n]);
            }
        }
    };
    assert!(response.starts_with("RTSP/1.0 200"), "{response}");
    assert_eq!(header(&response, "CSeq"), "3", "{response}");
    assert!(
        frames > 0,
        "no RTP frame reached the peer before the second PLAY's response"
    );

    drop(tcp);
    server.stop().ok();
}

/// A fanout retired by PAUSE exits at its next frame boundary — which a
/// peer that never reads defers indefinitely: the task stays parked inside
/// its frame write, holding the session's TCP writer. The session must
/// still end at the idle bound and take that fanout with it: the PAUSE
/// response, parked behind the same writer, is abandoned at the bound, the
/// session's Drop aborts the retired task (this is the terminal path, FIN
/// follows), and the peer sees EOF once it drains.
///
/// Without the backstop the session task waits on the writer forever,
/// unreachable by the idle reaper and the server's cancel: `peer_count()`
/// stays 1 and no FIN ever arrives.
#[test]
fn a_fanout_retired_by_pause_on_a_stalled_peer_dies_with_the_reaped_session() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    // Advertised 4 s → idle bound 4 s + max(2 s, 2 s) = 6 s: short enough
    // to reap promptly, long enough that the push above cannot be raced
    // by the reap on a slow runner (the idle clock runs from the PLAY
    // response; a reap before the PAUSE would make this test vacuous).
    let session_timeout = Duration::from_secs(4);
    b.session_timeout(session_timeout);
    // The server's post-SETUP idle bound for that config:
    // `timeout + max(timeout / 2, 2 s)` (`post_setup_idle_bound` in the
    // server's session module) = 6 s. The drain below is sized from it.
    let idle_bound = session_timeout + (session_timeout / 2).max(Duration::from_secs(2));
    let server = b.build().unwrap();
    let mount = server.add_mount("/live", make_muxer_cfg()).unwrap();
    server.start().unwrap();
    let (mut tcp, url, sid, _rtp_channel) = stalled_interleaved_peer(&server, &mount);

    // PAUSE on the control direction. Its response parks behind the
    // retired fanout's writer lock, so do not wait for it here.
    tcp.write_all(format!("PAUSE {url} RTSP/1.0\r\nCSeq: 3\r\nSession: {sid}\r\n\r\n").as_bytes())
        .unwrap();

    // The idle bound (6 s) reaps the session; the retired fanout must go
    // with it. Latch-and-poll with a deadline far above the bound — a
    // failure here is "never", not "slow".
    let deadline = Instant::now() + Duration::from_secs(20);
    while mount.peer_count() != 0 {
        assert!(
            Instant::now() < deadline,
            "retired fanout outlived the reaped session (peer_count = {})",
            mount.peer_count()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Drain what the kernel buffered for the stalled peer; the stream must
    // then END. One overall deadline, short per-read timeout, draining
    // through every WouldBlock — the verdict is "stream ended / quiesced
    // within the bound", never a single blocking read's timeout.
    //
    // On Linux/macOS the session's exit path shuts the write half and the
    // FIN (or a reset) follows the drained bytes promptly. On Windows
    // tokio's `shutdown()` is `shutdown(SD_SEND)`; issued while the peer
    // still had unread data queued, the graceful FIN can be deferred until
    // the socket is fully closed (here at `server.stop()`, after this
    // loop), so the FIN is not observable in this window. There the
    // portable equivalent is a quiesced stream: every buffered byte
    // drained, then nothing more arrives — a live fanout would keep
    // pushing new frames forever. `peer_count() == 0` above already proved
    // the reap; this only corroborates it. So EOF/reset is required on
    // non-Windows and accepted-or-quiesced on Windows.
    //
    // The non-Windows quiet window is long on purpose. The exit path queues
    // the FIN as soon as it drops the session, but the FIN sits behind the
    // bytes the kernel still holds for a peer whose tiny receive window has
    // been closed for the whole idle bound; once the peer reads again,
    // delivery can pause for seconds (zero-window probe backoff, seen on
    // macOS) before the rest and the FIN arrive, so a window of a couple of
    // seconds can end the drain before the FIN. Three idle bounds of
    // silence is a stream that will not end; the overall deadline stays
    // above that.
    tcp.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let overall_bound = idle_bound * 5;
    let overall = Instant::now() + overall_bound;
    let quiet_window = if cfg!(windows) {
        Duration::from_secs(2)
    } else {
        idle_bound * 3
    };
    let mut chunk = [0u8; 65536];
    let mut drained = 0usize;
    let mut quiet_since: Option<Instant> = None;
    // `true` = saw FIN or a reset; `false` = the stream went quiet without one.
    let ended_cleanly = loop {
        match tcp.read(&mut chunk) {
            Ok(0) => break true,
            Ok(n) => {
                drained += n;
                quiet_since = None;
                // A fanout that never stopped keeps this arm busy forever;
                // the overall deadline bounds that path too.
                assert!(
                    Instant::now() < overall,
                    "bytes still arriving {overall_bound:?} after the reap ({drained} drained): \
                     the retired fanout is still writing"
                );
            }
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe
                ) =>
            {
                break true;
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                let since = *quiet_since.get_or_insert_with(Instant::now);
                assert!(
                    Instant::now() < overall,
                    "stream neither ended nor quiesced within {overall_bound:?} after \
                     draining {drained} bytes: the session was never reaped / its write half \
                     never shut"
                );
                // Drained something, then no more bytes for the quiet window:
                // the fanout has stopped and the stream is quiescent.
                if drained > 0 && since.elapsed() >= quiet_window {
                    break false;
                }
            }
            Err(e) => panic!("read failed after {drained} bytes: {e}"),
        }
    };
    assert!(drained > 0, "nothing was buffered for the stalled peer");
    #[cfg(not(windows))]
    assert!(
        ended_cleanly,
        "stream quiesced after {drained} bytes but sent no FIN/reset: on Linux/macOS the \
         reaped session's exit path must shut the write half"
    );
    #[cfg(windows)]
    if !ended_cleanly {
        // Accepted: SD_SEND-with-unread-data defers the FIN to close (at
        // server.stop()); the reap (peer_count == 0) and the quiesced
        // stream are the portable verdict here.
        eprintln!(
            "windows: {drained} bytes drained then quiesced; graceful FIN deferred to close (accepted)"
        );
    }

    drop(tcp);
    server.stop().ok();
}
