//! Client-side interleaved producer thread — reads `$`-prefixed binary
//! frames + RTSP responses off the client's `Stream` read half
//! (post-PLAY when `transport_kind == TcpInterleaved`), demuxes by
//! channel, and routes:
//!
//! - Binary on `rtp_channel`: validate the RTP header (dropping
//!   structurally malformed frames), then push the **whole RTP packet**
//!   (header intact) to `data_tx` (the `mpsc::Sender<Bytes>` paired with
//!   the `mpsc::Receiver<Bytes>` that `RtpRecvTransport::from_mpsc_with_rtcp`
//!   consumes). PT policy and header stripping are the consumer's
//!   responsibility (`RtpRecvTransport::recv_bytes`).
//! - Binary on `rtcp_channel`: push payload to `rtcp_tx` (the RTCP
//!   ingest sink).
//! - RTSP responses (`CRLFCRLF`-framed + `Content-Length` body): push to
//!   `ctrl_tx`, which the main thread polls instead of reading the
//!   `Stream` directly once interleaved-PLAY mode is active — EXCEPT
//!   responses to keepalive pings (CSeq ≥ `KEEPALIVE_CSEQ_BASE`), which
//!   the pump consumes itself via
//!   `keepalive::handle_keepalive_response`: nothing drains `ctrl_tx`
//!   between main-thread requests, so queuing them would overflow the
//!   bounded queue on any long receive-only session and fail it.
//!
//! There is no server-side counterpart: the RTSP server never expects
//! client→server `$`-frames (no ANNOUNCE/RECORD support), so it reads
//! plain RTSP request bytes off the TCP stream directly
//! (`rtsp::server::session`).
//!
//! # Wire-up status
//!
//! The pump is wired into the
//! TCP-interleaved SETUP path: `RtspClient::activate_interleaved_pump`
//! (crate-private) spawns the pump as soon as a TCP-interleaved SETUP
//! succeeds (so the pump is draining the wire before PLAY is sent), and
//! [`RtspSession::into_recv_transport`](crate::rtsp::client::session::RtspSession::into_recv_transport)
//! consumes the data-side `mpsc::Receiver<Bytes>` plumbed through
//! `RtpRecvTransport::from_mpsc_with_rtcp` (crate-private). Once the
//! pump is active, subsequent RTSP request/response exchanges
//! (`RtspClient::send_and_read`, crate-private) write under the stream
//! mutex but read the response from the pump's `ctrl_rx` (matched by
//! CSeq), so reads don't race against the pump.

use std::io::Read;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use bytes::Bytes;

use crate::rtsp::client::Stream;
use crate::rtsp::client::end_reason::{EndReasonSlot, StreamEndReason};
use crate::rtsp::client::keepalive::KEEPALIVE_CSEQ_BASE;
use crate::rtsp::message::{RtspResponse, pump_accumulation_exceeded};

/// Bounded depth of the media (RTP/data) hand-off queue.
///
/// An interleaved frame body is capped at 65535 bytes by the
/// `$<ch><len_be16>` framing, but a normal TS bundle is `DEFAULT_PKT_SIZE`
/// (1316) bytes. A depth of 1024 gives a generous jitter buffer for a
/// live consumer while bounding retained memory to ~1.3 MiB in the normal
/// case (and capping the pathological all-max-frames case at ~64 MiB vs.
/// the unbounded growth this replaces). On overflow the pump drops the
/// newest frame (live-stream convention) and ticks `media_frames_dropped`
/// — it never blocks, so a slow consumer cannot wedge the pump thread.
pub(crate) const DATA_QUEUE_BOUND: usize = 1024;

/// Bounded depth of the RTCP hand-off queue.
///
/// RTCP is intrinsically low-rate (one compound report per few seconds per
/// RFC 3550 §6.2). 64 deep absorbs any legitimate burst; exceeding it
/// means the peer is flooding the control-adjacent RTCP channel, which is
/// abnormal/hostile — the pump fails the session rather than silently
/// dropping control-plane traffic.
pub(crate) const RTCP_QUEUE_BOUND: usize = 64;

/// Bounded depth of the RTSP control-response hand-off queue.
///
/// RTSP is not pipelined — at most one response is in flight at a time, so
/// the main thread drains this almost immediately. 32 deep tolerates any
/// legitimate interleaving slack; exceeding it means the peer is flooding
/// unsolicited control responses, which is hostile — the pump fails the
/// session. (Keepalive-ping responses never enter this queue — the pump
/// consumes them — so the flood policy cannot be tripped by the client's
/// own 30 s ping cadence on a long receive-only session.)
pub(crate) const CTRL_QUEUE_BOUND: usize = 32;

/// Stats observable from outside the pump thread.
#[derive(Debug, Default)]
pub(crate) struct PumpStats {
    /// Count of complete RTSP responses (header + body) routed to
    /// `ctrl_tx`.
    pub(crate) rtsp_messages_received: AtomicU64,
    /// Count of `$<rtp_channel>` binary frames whose RTP header was
    /// validated + whole packet pushed to `data_tx`.
    pub(crate) rtp_frames_received: AtomicU64,
    /// Count of `$<rtcp_channel>` binary frames whose payload was pushed
    /// to `rtcp_tx`.
    pub(crate) rtcp_frames_received: AtomicU64,
    /// Count of frames dropped because they were on an unknown channel,
    /// too small to contain an RTP header, or had a non-UTF8 RTSP
    /// header line.
    pub(crate) malformed_frames: AtomicU64,
    /// Count of RTP/media payloads dropped because the bounded `data_tx`
    /// queue was full (a slow or absent consumer). Drop-newest policy:
    /// the just-arrived frame is discarded so the pump never blocks (a
    /// blocking send would wedge the pump thread, which also carries the
    /// control channel — a self-DoS). Bounds retained memory regardless
    /// of consumer speed. Not yet surfaced through a public accessor; read
    /// by the bounded-queue test today.
    pub(crate) media_frames_dropped: AtomicU64,
    /// Count of read attempts the pump actually performed (i.e. loop
    /// iterations where it was not yielding the lock to a control-path
    /// writer). Used by the back-off regression test to assert the pump
    /// stops reading while `write_gate` is set.
    pub(crate) reads_attempted: AtomicU64,
    /// Count of keepalive-ping responses (CSeq ≥
    /// [`KEEPALIVE_CSEQ_BASE`](crate::rtsp::client::keepalive::KEEPALIVE_CSEQ_BASE))
    /// consumed by the pump instead of being routed to `ctrl_tx`.
    pub(crate) keepalive_responses: AtomicU64,
}

/// SETUP-allocated channel pair. The SETUP handler assigns these (the
/// client requests `interleaved=N-M` in its Transport header; the server
/// echoes or reassigns).
#[derive(Debug, Clone, Copy)]
pub(crate) struct InterleavedChannels {
    /// Channel number for RTP data frames.
    pub(crate) rtp: u8,
    /// Channel number for RTCP frames (conventionally `rtp + 1`).
    pub(crate) rtcp: u8,
}

// Framing rules live in `crate::rtsp::framing` (shared with the server's
// publisher direction). Re-exported here so the fuzz target and any
// external user of the old `#[doc(hidden)]` path keep compiling.
#[doc(hidden)]
pub use crate::rtsp::framing::{
    RtspFrameBoundary, parse_binary_frame_header, scan_rtsp_message_boundary,
};

/// Spawn the sync interleaved pump thread.
///
/// `reader` is any sync [`Read`] implementor. For plain TCP the caller
/// passes a [`std::net::TcpStream::try_clone`]'d half. For TLS the
/// caller passes a lock-and-read shim over the rustls session.
///
/// - `data_tx`: where whole RTP packets go (paired with
///   `RtpRecvTransport::from_mpsc_with_rtcp`). Bounded
///   ([`DATA_QUEUE_BOUND`]); on overflow the newest frame is dropped and
///   `media_frames_dropped` ticks (non-blocking `try_send`, never wedges
///   the pump).
/// - `rtcp_tx`: where RTCP `$<rtcp_channel>` frame payloads go. Bounded
///   ([`RTCP_QUEUE_BOUND`]); an overflow is treated as a hostile flood and
///   the pump exits (fails the session) rather than dropping silently.
/// - `ctrl_tx`: where RTSP response bytes go — main thread polls. Bounded
///   ([`CTRL_QUEUE_BOUND`]); an overflow is a hostile control-response
///   flood and the pump exits (fails the session). Keepalive-ping
///   responses (CSeq ≥ [`KEEPALIVE_CSEQ_BASE`]) never enter this queue —
///   the pump consumes them (see `auth` / `session_dead` below).
/// - `channels`: SETUP-allocated channel pair.
/// - `cancel`: flipped by `RtspClient::drop` or
///   [`crate::rtsp::client::RtspCancelHandle::cancel`].
/// - `write_gate`: count of writers (control-path requests, keepalive
///   pings) currently waiting for or holding the stream mutex — see
///   [`crate::rtsp::client::RtspClient::write_gate`] for the protocol.
///   While it is nonzero, the pump skips its read (and so does not
///   re-acquire the mutex), letting the writer in promptly. Without this
///   the pump — which holds the mutex across each blocking ~100 ms read
///   — monopolizes the lock and starves in-session writes on contended
///   runners.
/// - `auth`: shared challenge cache — a 401 answering a keepalive ping
///   refreshes it so the next ping signs against the fresh challenge.
/// - `session_dead`: keepalive death flag (`Some` iff the keepalive was
///   spawned) — flipped when a keepalive ping is answered `454`.
/// - `stats`: observable counters.
///
/// The pump exits cleanly when:
/// - `reader.read()` returns `Ok(0)` (EOF).
/// - `cancel` is set and the next `read()` returns
///   [`std::io::ErrorKind::TimedOut`] / [`std::io::ErrorKind::WouldBlock`].
/// - `data_tx.try_send()` fails with `Disconnected` (the receiver was
///   dropped). A `Full` on `data_tx` is NOT fatal — it drops the newest
///   media frame and ticks `media_frames_dropped`.
/// - `rtcp_tx` / `ctrl_tx` `try_send()` returns `Full` — a control-plane
///   flood is hostile, so the pump fails the session by exiting.
/// - `reader.read()` returns a non-timeout error (logged at WARN).
///
/// Returns `Err` if the OS refuses to spawn the thread (resource
/// exhaustion). This MUST be propagated rather than `.expect()`'d — the
/// pump runs on the RTSP connect path and the JVM/C bindings do not catch
/// unwinds across the FFI boundary, so a panic here would abort the host
/// process. The caller maps the `io::Error` to a typed `RtspError`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_client_pump<R: Read + Send + 'static>(
    mut reader: R,
    data_tx: mpsc::SyncSender<Bytes>,
    rtcp_tx: mpsc::SyncSender<Bytes>,
    ctrl_tx: mpsc::SyncSender<Bytes>,
    channels: InterleavedChannels,
    cancel: Arc<AtomicBool>,
    write_gate: Arc<AtomicUsize>,
    auth: Arc<Mutex<crate::rtsp::client::AuthState>>,
    session_dead: Option<Arc<AtomicBool>>,
    stats: Arc<PumpStats>,
    end_reason: EndReasonSlot,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("rtsp-interleaved-pump".to_string())
        .spawn(move || {
            let mut buf: Vec<u8> = Vec::with_capacity(16384);
            let mut chunk = [0u8; 4096];
            loop {
                if cancel.load(Ordering::Relaxed) {
                    // The most common deliberate-shutdown path: this
                    // flag is flipped by `RtspClient::Drop` (before the
                    // best-effort TEARDOWN) and by a replacement pump
                    // spawn reaping its predecessor — never by a wire
                    // event. Record it here so a client that's simply
                    // done (the overwhelmingly common case) reports
                    // `Cancelled` rather than leaving `end_reason()` at
                    // `None` forever.
                    end_reason.record(StreamEndReason::Cancelled);
                    return;
                }
                // Yield the stream lock to a control-path writer that is
                // about to (or is) waiting for it. We hold the mutex across
                // the blocking read below, so re-acquiring it every cycle
                // would starve the writer; skipping the read for one cycle
                // (~1 ms) lets it acquire promptly. Bounds an in-session
                // PLAY/PAUSE write to at most one in-flight read cycle.
                if write_gate.load(Ordering::Relaxed) > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                }
                stats.reads_attempted.fetch_add(1, Ordering::Relaxed);
                let n = match reader.read(&mut chunk) {
                    Ok(0) => {
                        // Clean EOF — the peer closed the connection in
                        // an orderly way (TCP FIN), not a wire error.
                        end_reason.record(StreamEndReason::CleanTeardown);
                        return;
                    }
                    Ok(n) => n,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::TimedOut
                            || e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        // The TcpStream is configured with a 100 ms read
                        // timeout in RtspClient::connect_with — loop back
                        // to recheck the cancel flag.
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "tst_rtp::client::pump",
                            error = %e,
                            "TCP read failed; pump exiting"
                        );
                        end_reason.record(StreamEndReason::TransportFailed {
                            msg: format!("TCP read failed: {e}"),
                        });
                        return;
                    }
                };
                buf.extend_from_slice(&chunk[..n]);

                // Buffer cap: coherent with the server pump and the session
                // loop (shared `pump_accumulation_exceeded` + the same
                // MAX_RTSP_MESSAGE_BYTES / MAX_RTSP_BODY_BYTES constants). This
                // channel interleaves RTSP text frames with binary `$`-frames. A
                // full u16 binary frame (65535-byte payload + 4-byte framing =
                // 65539 B) and an RTSP response with a body up to 1 MiB are BOTH
                // legitimate and never falsely rejected; only an
                // unterminated header run > 64 KiB or a bad/over-cap
                // Content-Length closes the pump. An uncapped header buffer
                // would let a peer grow `buf` without bound.
                if pump_accumulation_exceeded(&buf) {
                    tracing::warn!(
                        target: "tst_rtp::client::pump",
                        buf_len = buf.len(),
                        "pump buffer exceeded RTSP header/body caps; closing"
                    );
                    stats.malformed_frames.fetch_add(1, Ordering::Relaxed);
                    end_reason.record(StreamEndReason::ProtocolError {
                        msg: format!(
                            "pump buffer exceeded RTSP header/body caps ({} bytes buffered)",
                            buf.len()
                        ),
                    });
                    return;
                }

                // Parse as many complete frames as possible.
                loop {
                    if buf.is_empty() {
                        break;
                    }
                    if buf[0] == b'$' {
                        // Binary interleaved frame: `$<channel><len_be16><payload>`.
                        let Some((channel, total_len)) = parse_binary_frame_header(&buf) else {
                            break; // Need more bytes.
                        };
                        let payload_bytes = &buf[4..total_len];
                        if channel == channels.rtp {
                            stats.rtp_frames_received.fetch_add(1, Ordering::Relaxed);
                            // Decode the RTP header as a structural validity gate:
                            // a truncated or bad-version header is dropped +
                            // counter-ticked here. On success the whole RTP packet
                            // (header intact) is pushed to the consumer
                            // (`RtpRecvTransport::recv_bytes`), which applies PT
                            // policy and strips the header — so CSRC/extension/
                            // padding handling and PT enforcement live at one site.
                            match crate::packet::RtpHeader::decode(payload_bytes) {
                                Err(parse_err) => {
                                    stats.malformed_frames.fetch_add(1, Ordering::Relaxed);
                                    tracing::debug!(
                                        target: "tst_rtp::client::pump",
                                        error = ?parse_err,
                                        "interleaved RTP frame rejected; counter ticked",
                                    );
                                }
                                Ok(_parsed) => {
                                    let whole_packet = Bytes::copy_from_slice(payload_bytes);
                                    // Bounded, drop-newest, non-blocking. A `Full`
                                    // queue means a slow/absent consumer — drop the
                                    // newest frame (live-stream convention) and
                                    // counter-tick so the pump never blocks (a
                                    // blocking send would wedge this thread, which
                                    // also carries the control channel — a self-DoS).
                                    // Only a dropped RECEIVER is fatal.
                                    match data_tx.try_send(whole_packet) {
                                        Ok(()) => {}
                                        Err(mpsc::TrySendError::Full(_)) => {
                                            stats
                                                .media_frames_dropped
                                                .fetch_add(1, Ordering::Relaxed);
                                        }
                                        Err(mpsc::TrySendError::Disconnected(_)) => {
                                            // Receiver dropped — pump exits.
                                            return;
                                        }
                                    }
                                }
                            }
                        } else if channel == channels.rtcp {
                            stats.rtcp_frames_received.fetch_add(1, Ordering::Relaxed);
                            // RTCP is low-rate; a full bounded queue means the
                            // peer is flooding the control-adjacent RTCP channel.
                            // That is abnormal/hostile — fail the session (exit)
                            // rather than silently dropping control-plane frames.
                            // A dropped receiver also exits.
                            match rtcp_tx.try_send(Bytes::copy_from_slice(payload_bytes)) {
                                Ok(()) => {}
                                Err(mpsc::TrySendError::Full(_)) => {
                                    tracing::warn!(
                                        target: "tst_rtp::client::pump",
                                        "RTCP queue flooded ({} deep); failing session",
                                        RTCP_QUEUE_BOUND
                                    );
                                    stats.malformed_frames.fetch_add(1, Ordering::Relaxed);
                                    end_reason.record(StreamEndReason::ProtocolError {
                                        msg: format!(
                                            "RTCP queue flooded ({RTCP_QUEUE_BOUND} deep)"
                                        ),
                                    });
                                    return;
                                }
                                Err(mpsc::TrySendError::Disconnected(_)) => return,
                            }
                        } else {
                            stats.malformed_frames.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(
                                target: "tst_rtp::client::pump",
                                channel = channel,
                                "frame on unknown channel; dropping"
                            );
                        }
                        buf.drain(..total_len);
                    } else {
                        // RTSP response. Frame on `CRLFCRLF` + body of
                        // `Content-Length` bytes. Strict Content-Length:
                        // unparseable, oversized (> MAX_RTSP_BODY_BYTES), or
                        // duplicate are all hostile. Silently coercing to 0
                        // would desync framing, and an uncapped length would
                        // let a peer drive unbounded buffering — close the
                        // pump instead.
                        let msg_end = match scan_rtsp_message_boundary(&buf) {
                            RtspFrameBoundary::Incomplete => break, // Need more bytes.
                            RtspFrameBoundary::NonUtf8Headers { skip } => {
                                stats.malformed_frames.fetch_add(1, Ordering::Relaxed);
                                buf.drain(..skip);
                                continue;
                            }
                            RtspFrameBoundary::BadContentLength { detail } => {
                                tracing::warn!(
                                    target: "tst_rtp::client::pump",
                                    detail,
                                    "malformed RTSP Content-Length; pump exiting"
                                );
                                stats.malformed_frames.fetch_add(1, Ordering::Relaxed);
                                end_reason.record(StreamEndReason::ProtocolError {
                                    msg: format!("malformed RTSP Content-Length: {detail}"),
                                });
                                return;
                            }
                            RtspFrameBoundary::LengthOverflow => {
                                stats.malformed_frames.fetch_add(1, Ordering::Relaxed);
                                return;
                            }
                            RtspFrameBoundary::Complete { len } => len,
                        };
                        // Keepalive-ping responses are consumed HERE, never
                        // queued: nothing drains ctrl_rx between main-thread
                        // requests, so on a receive-only session (SETUP/PLAY
                        // then only data) queued keepalive 200s would fill the
                        // bounded queue and the flood policy below would fail
                        // the session — at the default 30 s ping cadence that
                        // killed every session at exactly 16.5 minutes
                        // ((CTRL_QUEUE_BOUND + 1) × 30 s), surfacing as a
                        // clean EOS. A parse failure falls through to the
                        // normal routing (e.g. a server→client REQUEST is not
                        // an `RtspResponse` and keeps its current handling).
                        if let Ok((resp, _)) = RtspResponse::parse(&buf[..msg_end]) {
                            if resp.cseq().is_some_and(|c| c >= KEEPALIVE_CSEQ_BASE) {
                                stats.keepalive_responses.fetch_add(1, Ordering::Relaxed);
                                crate::rtsp::client::keepalive::handle_keepalive_response(
                                    &resp,
                                    &auth,
                                    session_dead.as_deref(),
                                    &end_reason,
                                );
                                buf.drain(..msg_end);
                                continue;
                            }
                        }
                        stats.rtsp_messages_received.fetch_add(1, Ordering::Relaxed);
                        let msg = Bytes::copy_from_slice(&buf[..msg_end]);
                        // RTSP is not pipelined — the main thread drains ctrl_rx
                        // almost immediately. A full bounded queue means the peer
                        // is flooding unsolicited control responses, which is
                        // hostile — fail the session (exit) rather than buffer.
                        // `Disconnected` (main thread gone) also exits.
                        match ctrl_tx.try_send(msg) {
                            Ok(()) => {}
                            Err(mpsc::TrySendError::Full(_)) => {
                                tracing::warn!(
                                    target: "tst_rtp::client::pump",
                                    "RTSP control queue flooded ({} deep); failing session",
                                    CTRL_QUEUE_BOUND
                                );
                                stats.malformed_frames.fetch_add(1, Ordering::Relaxed);
                                end_reason.record(StreamEndReason::ProtocolError {
                                    msg: format!(
                                        "RTSP control queue flooded ({CTRL_QUEUE_BOUND} deep)"
                                    ),
                                });
                                return;
                            }
                            Err(mpsc::TrySendError::Disconnected(_)) => {
                                tracing::warn!(
                                    target: "tst_rtp::client::pump",
                                    "ctrl_rx closed; pump exiting"
                                );
                                // `ctrl_rx` is exclusively owned by
                                // `InterleavedPumpState`, which only drops
                                // it from `RtspClient::Drop` or a
                                // replacement pump spawn — both cancel/drop
                                // paths (and both set this pump's own
                                // `cancel` flag first, so this branch is a
                                // narrow race with the loop-top cancel
                                // check, not the common exit path). Record
                                // Cancelled rather than ProtocolError: there
                                // is no scenario where this queue closes
                                // because of a peer/protocol issue.
                                end_reason.record(StreamEndReason::Cancelled);
                                return;
                            }
                        }
                        buf.drain(..msg_end);
                    }
                }
            }
        })
}

/// `Read` adapter that locks `Arc<Mutex<Stream>>` per call.
///
/// The pump runs in a background thread and shares the control TCP /
/// TLS stream with the main thread (request writes) and the keepalive
/// thread (OPTIONS writes). To avoid `try_clone` — which doesn't work
/// for rustls `ClientConnection` — we lock the same `Arc<Mutex<Stream>>`
/// once per `read()` call. Each call:
///
/// 1. Acquires the mutex.
/// 2. Calls `Stream::read` (which has a ~100 ms underlying read timeout
///    set by [`crate::rtsp::client::RtspClient::connect_with`]).
/// 3. Releases the mutex.
/// 4. Returns whatever the read returned (`Ok(n)`, `Ok(0)` for EOF, or
///    `Err(WouldBlock|TimedOut)` for the timeout — the pump loops back).
///
/// Holding the mutex for at most ~100 ms per call is fine: RTSP is not
/// pipelined, so the main thread only contends when it's actively
/// sending/receiving a single request, and a 100 ms wait there is
/// invisible compared to typical RTSP round-trip times.
pub(crate) struct SharedStreamReader {
    stream: Arc<Mutex<Stream>>,
}

impl SharedStreamReader {
    pub(crate) fn new(stream: Arc<Mutex<Stream>>) -> Self {
        Self { stream }
    }
}

impl Read for SharedStreamReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut g = self
            .stream
            .lock()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "stream mutex poisoned"))?;
        g.read(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{RTP_HEADER_LEN, RTP_PT_MP2T, RtpHeader};
    use std::io::Cursor;

    use crate::rtsp::client::AuthState;

    /// [`spawn_client_pump`] with a default (empty) auth state and no
    /// `session_dead` flag — most pump tests don't exercise the
    /// keepalive-response path; the ones that do call the real spawn.
    #[allow(clippy::too_many_arguments)]
    fn spawn_test_pump<R: Read + Send + 'static>(
        reader: R,
        data_tx: mpsc::SyncSender<Bytes>,
        rtcp_tx: mpsc::SyncSender<Bytes>,
        ctrl_tx: mpsc::SyncSender<Bytes>,
        channels: InterleavedChannels,
        cancel: Arc<AtomicBool>,
        write_gate: Arc<AtomicUsize>,
        stats: Arc<PumpStats>,
    ) -> std::io::Result<JoinHandle<()>> {
        spawn_client_pump(
            reader,
            data_tx,
            rtcp_tx,
            ctrl_tx,
            channels,
            cancel,
            write_gate,
            Arc::new(Mutex::new(AuthState::default())),
            None,
            stats,
            EndReasonSlot::default(),
        )
    }

    #[allow(clippy::type_complexity)]
    fn make_args() -> (
        mpsc::SyncSender<Bytes>,
        mpsc::Receiver<Bytes>,
        mpsc::SyncSender<Bytes>,
        mpsc::Receiver<Bytes>,
        mpsc::SyncSender<Bytes>,
        mpsc::Receiver<Bytes>,
        Arc<AtomicBool>,
        Arc<PumpStats>,
    ) {
        let (dt, dr) = mpsc::sync_channel(DATA_QUEUE_BOUND);
        let (rt, rr) = mpsc::sync_channel(RTCP_QUEUE_BOUND);
        let (ct, cr) = mpsc::sync_channel(CTRL_QUEUE_BOUND);
        let cancel = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(PumpStats::default());
        (dt, dr, rt, rr, ct, cr, cancel, stats)
    }

    /// The happy path returns `Ok(JoinHandle)` — the spawn signature is now
    /// fallible (it propagates an `io::Error` instead of panicking) so a
    /// thread-spawn failure surfaces as a clean `RtspError` rather than
    /// aborting the host process across the FFI boundary. The failure path
    /// itself (OS thread-spawn refusal under resource exhaustion) can't be
    /// injected deterministically without OS-level limit manipulation, so
    /// this asserts the contract on the only path we can exercise: success.
    #[test]
    fn spawn_returns_ok_on_happy_path() {
        let raw: Vec<u8> = Vec::new(); // empty → immediate EOF, pump exits.
        let (dt, _dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        let result = spawn_test_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            stats.clone(),
        );
        let handle = result.expect("spawn must succeed on the happy path");
        let _ = handle.join();
    }

    /// Feed an RTP frame (channel=0, 12-byte header + 8-byte payload).
    /// Pump should validate the header and push the whole RTP packet
    /// (header + payload) onto data_rx — stripping happens at the
    /// transport recv site.
    #[test]
    fn rtp_whole_packet_delivered() {
        let header = valid_rtp_header();
        let mut raw = vec![b'$', 0u8, 0x00, 20]; // interleaved frame: length = 12 + 8 = 20
        raw.extend_from_slice(&header);
        raw.extend_from_slice(b"PAYLOAD!"); // 8 bytes.
        let (dt, dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        let handle = spawn_test_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            stats.clone(),
        )
        .unwrap();
        // Pump should deliver the whole RTP packet (header + payload).
        let packet = dr.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let mut expected = header.to_vec();
        expected.extend_from_slice(b"PAYLOAD!");
        assert_eq!(packet.as_ref(), expected.as_slice());
        let _ = handle.join();
        assert_eq!(stats.rtp_frames_received.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn rtcp_frame_routed_to_rtcp_rx() {
        let mut raw = vec![b'$', 1u8, 0x00, 4];
        raw.extend_from_slice(b"\xDE\xAD\xBE\xEF");
        let (dt, _dr, rt, rr, ct, _cr, cancel, stats) = make_args();
        let handle = spawn_test_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            stats.clone(),
        )
        .unwrap();
        let payload = rr.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(payload.as_ref(), &[0xDE, 0xAD, 0xBE, 0xEF]);
        let _ = handle.join();
        assert_eq!(stats.rtcp_frames_received.load(Ordering::Relaxed), 1);
    }

    /// A FULL u16 binary interleaved frame — 65535-byte
    /// payload + 4-byte framing = 65539 B — must be ACCEPTED, not falsely
    /// rejected. A client pump that capped `buf` at 64 KiB would close the
    /// pump on any frame > ~65532 B. Mirrors the server pump's full-u16 test.
    #[test]
    fn pump_accepts_full_u16_binary_frame() {
        // $<channel=1 (rtcp)><length=65535 BE><65535 payload bytes>.
        let mut raw = vec![b'$', 1u8, 0xFF, 0xFF];
        raw.extend(std::iter::repeat_n(0xABu8, 65535));
        let (dt, _dr, rt, rr, ct, _cr, cancel, stats) = make_args();
        let handle = spawn_test_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            stats.clone(),
        )
        .unwrap();
        let payload = rr.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(payload.len(), 65535, "full payload must round-trip intact");
        assert!(payload.iter().all(|&b| b == 0xAB));
        let _ = handle.join();
        assert_eq!(stats.rtcp_frames_received.load(Ordering::Relaxed), 1);
        assert_eq!(stats.malformed_frames.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn rtsp_response_routed_to_ctrl_rx() {
        let raw = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n".to_vec();
        let (dt, _dr, rt, _rr, ct, cr, cancel, stats) = make_args();
        let handle = spawn_test_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            stats.clone(),
        )
        .unwrap();
        let msg = cr.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let text = std::str::from_utf8(&msg).unwrap();
        assert!(text.starts_with("RTSP/1.0 200 OK"));
        let _ = handle.join();
        assert_eq!(stats.rtsp_messages_received.load(Ordering::Relaxed), 1);
    }

    /// Regression: responses to keepalive
    /// OPTIONS pings (CSeq ≥ [`KEEPALIVE_CSEQ_BASE`]) must be consumed by
    /// the pump, NOT routed to the bounded ctrl queue — nothing drains
    /// that queue between main-thread requests, so on a receive-only
    /// session queued pings would overflow it after `CTRL_QUEUE_BOUND + 1`
    /// responses (16.5 minutes at the default 30 s cadence) and the flood
    /// policy below would kill the session, surfacing as a clean EOS. Feeding
    /// more responses than the queue can hold with NO consumer draining it
    /// proves the pump does not queue them.
    #[test]
    fn keepalive_responses_consumed_not_queued() {
        let n = CTRL_QUEUE_BOUND + 8;
        let mut raw = Vec::new();
        for i in 0..n {
            raw.extend_from_slice(
                format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n",
                    KEEPALIVE_CSEQ_BASE as usize + 1 + i
                )
                .as_bytes(),
            );
        }
        let (dt, _dr, rt, _rr, ct, cr, cancel, stats) = make_args();
        let handle = spawn_test_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            stats.clone(),
        )
        .unwrap();
        // Pre-fix the pump exited via the flood policy at response #33;
        // post-fix it consumes all of them and exits at EOF.
        let _ = handle.join();
        assert_eq!(
            cr.try_iter().count(),
            0,
            "keepalive responses must not reach ctrl_rx"
        );
        assert_eq!(stats.keepalive_responses.load(Ordering::Relaxed), n as u64);
        assert_eq!(stats.rtsp_messages_received.load(Ordering::Relaxed), 0);
        assert_eq!(stats.malformed_frames.load(Ordering::Relaxed), 0);
    }

    /// A mid-session 401 on a keepalive ping (nonce rotated or expired —
    /// RFC 7616 §3.3 `stale=true` and friends) must refresh the SHARED
    /// challenge cache so the next ping, at most one interval later,
    /// signs against the fresh challenge. Before the fix nothing read
    /// keepalive responses at all, so every subsequent ping re-signed
    /// the dead nonce and the session silently died at the server
    /// timeout with no client-side signal.
    #[test]
    fn keepalive_401_refreshes_shared_auth_challenge() {
        let raw = format!(
            "RTSP/1.0 401 Unauthorized\r\nCSeq: {}\r\nWWW-Authenticate: \
             Digest realm=\"cam\", nonce=\"rotated\", stale=true\r\n\r\n",
            KEEPALIVE_CSEQ_BASE + 7
        )
        .into_bytes();
        let auth = Arc::new(Mutex::new(AuthState::default()));
        {
            let mut g = auth.lock().unwrap();
            g.challenge = Some(r#"Digest realm="cam", nonce="old""#.into());
            g.nc = 41;
        }
        let (dt, _dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        let handle = spawn_client_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            auth.clone(),
            None,
            stats.clone(),
            EndReasonSlot::default(),
        )
        .unwrap();
        let _ = handle.join();
        let g = auth.lock().unwrap();
        assert!(
            g.challenge
                .as_deref()
                .is_some_and(|c| c.contains("rotated")),
            "shared challenge must be refreshed from the keepalive 401, got {:?}",
            g.challenge
        );
        assert_eq!(g.nc, 0, "nonce-count must reset for the new challenge");
    }

    /// 454 Session Not Found answering a keepalive ping means the server
    /// no longer honors the session — the pump must flip `session_dead`
    /// so `RtspClient::is_session_alive` surfaces the expiry (the flag
    /// previously only tracked control-TCP write failures).
    #[test]
    fn keepalive_454_flips_session_dead() {
        let raw = format!(
            "RTSP/1.0 454 Session Not Found\r\nCSeq: {}\r\n\r\n",
            KEEPALIVE_CSEQ_BASE + 3
        )
        .into_bytes();
        let session_dead = Arc::new(AtomicBool::new(false));
        let end_reason = EndReasonSlot::default();
        let (dt, _dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        let handle = spawn_client_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(Mutex::new(AuthState::default())),
            Some(session_dead.clone()),
            stats.clone(),
            end_reason.clone(),
        )
        .unwrap();
        let _ = handle.join();
        assert!(
            session_dead.load(Ordering::Relaxed),
            "a 454 keepalive response must flip session_dead"
        );
        assert!(
            matches!(end_reason.get(), Some(StreamEndReason::SessionExpired)),
            "a 454 keepalive response must record StreamEndReason::SessionExpired, got {:?}",
            end_reason.get()
        );
    }

    /// RTSP response with a `Content-Length: N` body — the pump must
    /// wait for the body bytes before considering the message complete.
    #[test]
    fn rtsp_response_with_body_routed_to_ctrl_rx() {
        let raw = b"RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: 5\r\n\r\nHELLO".to_vec();
        let (dt, _dr, rt, _rr, ct, cr, cancel, stats) = make_args();
        let handle = spawn_test_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            stats.clone(),
        )
        .unwrap();
        let msg = cr.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert!(msg.ends_with(b"\r\n\r\nHELLO"));
        let _ = handle.join();
        assert_eq!(stats.rtsp_messages_received.load(Ordering::Relaxed), 1);
    }

    /// An RTSP response on the interleaved channel that
    /// declares a malformed/oversized Content-Length must CLOSE the pump (not
    /// silently coerce to a 0-length body and desync, nor buffer toward an
    /// uncapped body). Mirrors the server pump's close-on-bad-CL policy at this
    /// call site. Covers both unparseable and over-cap CLs.
    #[test]
    fn rtsp_response_closes_pump_on_malformed_content_length() {
        for header in [
            "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: nope\r\n\r\n",
            "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: 2000000000\r\n\r\n",
        ] {
            let raw = header.as_bytes().to_vec();
            let (dt, _dr, rt, _rr, ct, cr, cancel, stats) = make_args();
            let handle = spawn_test_pump(
                Cursor::new(raw),
                dt,
                rt,
                ct,
                InterleavedChannels { rtp: 0, rtcp: 1 },
                cancel.clone(),
                Arc::new(AtomicUsize::new(0)),
                stats.clone(),
            )
            .unwrap();
            // The pump must exit on its own (bad CL is fatal) — join returns.
            let _ = handle.join();
            // No RTSP message should have been routed.
            assert!(
                cr.try_recv().is_err(),
                "malformed Content-Length must not produce an RTSP message"
            );
            assert!(
                stats.malformed_frames.load(Ordering::Relaxed) >= 1,
                "malformed Content-Length should be counted as a malformed frame"
            );
            assert_eq!(stats.rtsp_messages_received.load(Ordering::Relaxed), 0);
        }
    }

    /// An RTSP response that never terminates (no `CRLFCRLF`) must NOT
    /// drive the pump to buffer unboundedly — the buffer cap closes the pump
    /// once accumulation exceeds `MAX_RTSP_MESSAGE_BYTES`. With an uncapped
    /// header buffer a peer that never sends CRLFCRLF could grow `buf`
    /// without bound.
    #[test]
    fn pump_closes_on_unterminated_header_flood() {
        // 128 KiB of header-junk with no CRLFCRLF terminator — well over the
        // 64 KiB cap. Starts with a non-`$` byte so it's parsed as RTSP text.
        let raw = vec![b'A'; 128 * 1024];
        let (dt, _dr, rt, _rr, ct, cr, cancel, stats) = make_args();
        let handle = spawn_test_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            stats.clone(),
        )
        .unwrap();
        // The pump must exit on its own once the cap is exceeded — join returns.
        let _ = handle.join();
        assert!(
            cr.try_recv().is_err(),
            "unterminated flood must not produce an RTSP message"
        );
        assert!(
            stats.malformed_frames.load(Ordering::Relaxed) >= 1,
            "over-cap buffer should be counted as a malformed frame"
        );
        assert_eq!(stats.rtsp_messages_received.load(Ordering::Relaxed), 0);
    }

    /// A frame on an unknown channel (not rtp, not rtcp) should be
    /// counter-ticked + dropped, but not crash the pump.
    #[test]
    fn unknown_channel_frame_counted_and_dropped() {
        let raw = vec![b'$', 7u8, 0x00, 2, 0xAB, 0xCD];
        let (dt, _dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        let handle = spawn_test_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            stats.clone(),
        )
        .unwrap();
        let _ = handle.join();
        assert_eq!(stats.malformed_frames.load(Ordering::Relaxed), 1);
        assert_eq!(stats.rtp_frames_received.load(Ordering::Relaxed), 0);
        assert_eq!(stats.rtcp_frames_received.load(Ordering::Relaxed), 0);
    }

    /// An RTP frame whose body is smaller than the 12-byte RTP header
    /// should be counter-ticked and dropped (not panic on slice).
    #[test]
    fn undersized_rtp_frame_counted_and_dropped() {
        let raw = vec![b'$', 0u8, 0x00, 4, 0x00, 0x00, 0x00, 0x00];
        let (dt, _dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        let handle = spawn_test_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            stats.clone(),
        )
        .unwrap();
        let _ = handle.join();
        assert_eq!(stats.rtp_frames_received.load(Ordering::Relaxed), 1);
        assert_eq!(stats.malformed_frames.load(Ordering::Relaxed), 1);
    }

    /// Build a minimal *valid* fixed RTP header (V=2, P=0, X=0, CC=0,
    /// PT=33) so the body parses as MP2T-over-RTP. Used by the fixtures
    /// that exercise the delivery path (they care about payload bytes,
    /// not the header fields).
    fn valid_rtp_header() -> [u8; RTP_HEADER_LEN] {
        let mut h = [0u8; RTP_HEADER_LEN];
        RtpHeader::new(0, 0, 0).encode_into(&mut h);
        h
    }

    /// Wrap a raw RTP packet (`rtp`) in one interleaved frame on `channel`.
    fn interleaved_frame(channel: u8, rtp: &[u8]) -> Vec<u8> {
        let mut f = vec![b'$', channel];
        f.extend_from_slice(&(rtp.len() as u16).to_be_bytes());
        f.extend_from_slice(rtp);
        f
    }

    /// An interleaved RTP frame carrying a CSRC list
    /// (CC>0), a header extension (X=1), and trailing padding (P=1) must
    /// decode successfully at the pump (structural gate) and be delivered
    /// as a whole packet. CSRC/extension skipping and padding trimming
    /// are the consumer's responsibility; see the transport-level test
    /// `interleaved_rtp_csrc_extension_padding_stripped_at_recv_site`.
    #[test]
    fn interleaved_rtp_complex_header_delivered_as_whole_packet() {
        // Build a packet with CC=1, X=1, P=1.
        //   Octet 0: V=2 | P=1 | X=1 | CC=1 = 0b10_1_1_0001 = 0xB1
        //   Octet 1: PT=33
        //   Octets 2..12 : seq/ts/ssrc
        //   Octets 12..16: 1 CSRC entry (4 bytes)
        //   Octets 16..20: extension header (profile 0xBEDE + length=1 word)
        //   Octets 20..24: 1 word (4 bytes) of extension data
        //   Octets 24..28: the 4 real payload bytes
        //   Octets 28..30: 2 padding bytes (last byte = pad count = 2)
        let mut rtp = vec![0xB1, RTP_PT_MP2T];
        rtp.extend_from_slice(&[0u8; 10]); // seq/ts/ssrc
        rtp.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // CSRC[0]
        rtp.extend_from_slice(&[0xBE, 0xDE, 0x00, 0x01]); // ext header, len=1 word
        rtp.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]); // ext data word
        rtp.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // real payload
        rtp.extend_from_slice(&[0x00, 0x02]); // 2 padding bytes (count=2)

        let raw = interleaved_frame(0, &rtp);
        let (dt, dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        let handle = spawn_test_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            stats.clone(),
        )
        .unwrap();
        // Pump delivers the whole RTP packet intact (header + CSRC + ext + payload + pad).
        let packet = dr.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(
            packet.as_ref(),
            rtp.as_slice(),
            "whole RTP packet (including CSRC/extension/padding) must be delivered intact"
        );
        let _ = handle.join();
        assert_eq!(stats.rtp_frames_received.load(Ordering::Relaxed), 1);
        assert_eq!(stats.malformed_frames.load(Ordering::Relaxed), 0);
    }

    /// An interleaved RTP frame with a structurally malformed RTP header
    /// (here: a truncated extension — X=1 but no extension bytes present)
    /// must be dropped and counter-ticked, never fed to the demuxer.
    #[test]
    fn interleaved_malformed_rtp_dropped_and_counted() {
        // V=2, X=1, CC=0, PT=33, but only the 12-byte fixed header — the
        // 4-byte extension header X=1 promises is missing → decode rejects.
        let rtp = vec![0x90, RTP_PT_MP2T, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let raw = interleaved_frame(0, &rtp);
        let (dt, dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        let handle = spawn_test_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            stats.clone(),
        )
        .unwrap();
        let _ = handle.join();
        assert!(
            dr.try_recv().is_err(),
            "malformed RTP must not produce a TS payload"
        );
        assert_eq!(stats.rtp_frames_received.load(Ordering::Relaxed), 1);
        assert_eq!(stats.malformed_frames.load(Ordering::Relaxed), 1);
    }

    /// Build one interleaved RTP frame on `channel` carrying a valid 12-byte
    /// RTP header + `payload_len` payload bytes. The frame body length
    /// (`RTP_HEADER_LEN + payload_len`) must fit a u16.
    fn rtp_frame(channel: u8, payload_len: usize) -> Vec<u8> {
        let mut rtp = valid_rtp_header().to_vec();
        rtp.extend(std::iter::repeat_n(0xAB, payload_len));
        interleaved_frame(channel, &rtp)
    }

    /// Adversarial: a fast producer flooding the
    /// media (RTP) channel while the consumer never drains must NOT grow
    /// memory without bound. The bounded `data_tx` caps retained frames at
    /// `DATA_QUEUE_BOUND`; everything beyond is dropped-newest and ticks
    /// `media_frames_dropped`. The pump stays alive (media drop is NOT
    /// fatal) and processes the whole input.
    #[test]
    fn media_queue_bounded_drops_newest_on_slow_consumer() {
        const EXTRA: usize = 256;
        let total = DATA_QUEUE_BOUND + EXTRA;
        let mut raw = Vec::new();
        for _ in 0..total {
            raw.extend(rtp_frame(0, 8));
        }
        let (dt, dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        let handle = spawn_test_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            stats.clone(),
        )
        .unwrap();
        // Consumer never drains until the pump has finished (EOF → exit).
        let _ = handle.join();

        // The pump saw every frame...
        assert_eq!(
            stats.rtp_frames_received.load(Ordering::Relaxed),
            total as u64
        );
        // ...but retained memory is bounded: at most DATA_QUEUE_BOUND frames
        // are still queued; the EXTRA were dropped-newest + counted.
        assert_eq!(
            stats.media_frames_dropped.load(Ordering::Relaxed),
            EXTRA as u64,
            "exactly the over-cap frames must be dropped"
        );
        let mut queued = 0usize;
        while dr.try_recv().is_ok() {
            queued += 1;
        }
        assert_eq!(
            queued, DATA_QUEUE_BOUND,
            "the bounded queue must retain at most DATA_QUEUE_BOUND frames"
        );
    }

    /// Adversarial: an RTCP flood with an absent
    /// consumer must FAIL the session (control-plane traffic is never
    /// silently dropped). Once the bounded RTCP queue fills, the pump
    /// exits and ticks `malformed_frames`. Note: `rr` is kept alive (so
    /// the channel isn't Disconnected) but never drained, forcing `Full`.
    #[test]
    fn rtcp_flood_fails_session() {
        let mut raw = Vec::new();
        // One more than the queue can hold guarantees a `Full`.
        for _ in 0..(RTCP_QUEUE_BOUND + 1) {
            // RTCP frame on channel 1: $<1><len=4><DEADBEEF>.
            raw.extend_from_slice(&[b'$', 1u8, 0x00, 0x04, 0xDE, 0xAD, 0xBE, 0xEF]);
        }
        // Trailing extra frames the pump should never reach (it exits first).
        for _ in 0..10 {
            raw.extend_from_slice(&[b'$', 1u8, 0x00, 0x04, 0xDE, 0xAD, 0xBE, 0xEF]);
        }
        let (dt, _dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        // _rr kept alive → channel not Disconnected → overflow is `Full`.
        let handle = spawn_test_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            stats.clone(),
        )
        .unwrap();
        let _ = handle.join();
        // The pump must have exited on the flood, not processed all frames.
        assert!(
            stats.rtcp_frames_received.load(Ordering::Relaxed) <= (RTCP_QUEUE_BOUND + 1) as u64,
            "pump should fail the session at the first RTCP overflow"
        );
        assert!(
            stats.malformed_frames.load(Ordering::Relaxed) >= 1,
            "an RTCP flood must counter-tick + fail the session"
        );
    }

    /// Adversarial: an RTSP control-response flood
    /// with an absent main thread must FAIL the session rather than buffer
    /// unbounded. Once the bounded ctrl queue fills, the pump exits.
    #[test]
    fn ctrl_flood_fails_session() {
        let mut raw = Vec::new();
        // Minimal complete RTSP responses (header + CRLFCRLF, no body).
        // CTRL_QUEUE_BOUND + 1 guarantees a `Full`.
        for i in 0..(CTRL_QUEUE_BOUND + 1) {
            raw.extend_from_slice(format!("RTSP/1.0 200 OK\r\nCSeq: {i}\r\n\r\n").as_bytes());
        }
        let (dt, _dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        // _cr kept alive but never drained → overflow is `Full`, not closed.
        let handle = spawn_test_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            stats.clone(),
        )
        .unwrap();
        let _ = handle.join();
        assert!(
            stats.malformed_frames.load(Ordering::Relaxed) >= 1,
            "a control-response flood must counter-tick + fail the session"
        );
        // The queue retained at most CTRL_QUEUE_BOUND responses (bounded).
        let mut queued = 0usize;
        while _cr.try_recv().is_ok() {
            queued += 1;
        }
        assert!(
            queued <= CTRL_QUEUE_BOUND,
            "ctrl queue must be bounded by CTRL_QUEUE_BOUND"
        );
    }

    #[test]
    fn cancel_flag_exits_pump() {
        // Use an empty Cursor — gives EOF immediately. We can't easily
        // simulate a never-EOF stream with Cursor, so we set cancel
        // before the pump starts; the test verifies pump joins cleanly
        // when cancel is set.
        let raw = Vec::<u8>::new();
        let (dt, _dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        cancel.store(true, Ordering::Relaxed);
        let handle = spawn_test_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            stats.clone(),
        )
        .unwrap();
        let _ = handle.join();
    }

    /// Regression guard for a hang in
    /// `client_setup_with_transport_tcp_round_trips_ts`: the pump holds the
    /// stream mutex across each blocking ~100 ms read, so without yielding
    /// to `write_gate` an
    /// in-session control write (PLAY/PAUSE) is starved indefinitely on a
    /// contended runner. This asserts the pump STOPS reading while the gate
    /// is set — the precondition that lets a control writer acquire the
    /// lock promptly. Deterministic on any kernel: an un-gated pump keeps
    /// reading (counter climbs); a gated pump freezes.
    #[test]
    fn pump_stops_reading_while_write_gate_set() {
        use std::net::{TcpListener, TcpStream};
        use std::time::{Duration, Instant};

        // Loopback pair whose server side never sends, so every pump read
        // blocks for the full 100 ms read timeout while holding the lock —
        // the idle-control-connection case that monopolizes the mutex.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let done_srv = done.clone();
        let server = std::thread::spawn(move || {
            let _conn = listener.accept().unwrap();
            // Hold the connection open (never send) until the test is done.
            while !done_srv.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let stream = Arc::new(Mutex::new(Stream::Plain(client)));

        let (dt, _dr) = mpsc::sync_channel(DATA_QUEUE_BOUND);
        let (rt, _rr) = mpsc::sync_channel(RTCP_QUEUE_BOUND);
        let (ct, _cr) = mpsc::sync_channel(CTRL_QUEUE_BOUND);
        let cancel = Arc::new(AtomicBool::new(false));
        let write_gate = Arc::new(AtomicUsize::new(0));
        let stats = Arc::new(PumpStats::default());
        let reader = SharedStreamReader::new(stream.clone());
        let handle = spawn_test_pump(
            reader,
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            write_gate.clone(),
            stats.clone(),
        )
        .unwrap();

        // Let the pump take a few read cycles.
        std::thread::sleep(Duration::from_millis(50));
        // Ask it to yield, then wait past one in-flight read (100 ms) so any
        // read in progress when we set the gate has drained.
        write_gate.fetch_add(1, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(200));
        let baseline = stats.reads_attempted.load(Ordering::Relaxed);
        // Over the next 300 ms (~3 read cycles) the pump must not read.
        std::thread::sleep(Duration::from_millis(300));
        let after = stats.reads_attempted.load(Ordering::Relaxed);

        // And a control writer must acquire the lock promptly while gated.
        let t0 = Instant::now();
        {
            let _g = stream.lock().unwrap();
        }
        let lock_wait = t0.elapsed();

        cancel.store(true, Ordering::Relaxed);
        write_gate.fetch_sub(1, Ordering::Relaxed);
        done.store(true, Ordering::Relaxed);
        let _ = handle.join();
        let _ = server.join();

        assert_eq!(
            after,
            baseline,
            "pump performed {} reads while write_gate was set — it must yield the lock",
            after - baseline
        );
        assert!(
            lock_wait < Duration::from_millis(500),
            "control writer waited {lock_wait:?} for the stream lock while the gate was set"
        );
    }

    // ── StreamEndReason recording at each pump exit site ───────────────

    /// A `Read` impl that yields nothing then returns a hard (non-timeout)
    /// error on its first call — the shape the pump's `:235 TCP read
    /// failed` site handles. `Cursor` can't produce this (it only ever
    /// returns `Ok` or reaches EOF), so a tiny custom reader is needed to
    /// exercise the `TransportFailed` mapping deterministically.
    struct ErroringReader;

    impl Read for ErroringReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("simulated hard read failure"))
        }
    }

    /// The pump's loop-top cancel check (hit by `RtspClient::Drop` or a
    /// replacement pump spawn reaping its predecessor — the most common
    /// deliberate-shutdown path) must record `Cancelled`, distinct from
    /// the `Ok(0)` clean-EOF branch below. Cancel is set BEFORE spawn so
    /// the pump takes the loop-top exit on its very first iteration,
    /// before ever touching the (empty, EOF-on-first-read) reader.
    #[test]
    fn cancel_flag_exit_records_cancelled() {
        let raw = Vec::<u8>::new();
        let (dt, _dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        cancel.store(true, Ordering::Relaxed);
        let end_reason = EndReasonSlot::default();
        let handle = spawn_client_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(Mutex::new(AuthState::default())),
            None,
            stats.clone(),
            end_reason.clone(),
        )
        .unwrap();
        let _ = handle.join();
        assert!(
            matches!(end_reason.get(), Some(StreamEndReason::Cancelled)),
            "the loop-top cancel exit must record Cancelled, got {:?}",
            end_reason.get()
        );
    }

    #[test]
    fn clean_eof_records_clean_teardown() {
        let (dt, _dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        let end_reason = EndReasonSlot::default();
        let handle = spawn_client_pump(
            Cursor::new(Vec::<u8>::new()), // empty -> immediate Ok(0) EOF.
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(Mutex::new(AuthState::default())),
            None,
            stats.clone(),
            end_reason.clone(),
        )
        .unwrap();
        let _ = handle.join();
        assert!(
            matches!(end_reason.get(), Some(StreamEndReason::CleanTeardown)),
            "clean EOF must record CleanTeardown, got {:?}",
            end_reason.get()
        );
    }

    #[test]
    fn hard_read_error_records_transport_failed() {
        let (dt, _dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        let end_reason = EndReasonSlot::default();
        let handle = spawn_client_pump(
            ErroringReader,
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(Mutex::new(AuthState::default())),
            None,
            stats.clone(),
            end_reason.clone(),
        )
        .unwrap();
        let _ = handle.join();
        assert!(
            matches!(
                end_reason.get(),
                Some(StreamEndReason::TransportFailed { .. })
            ),
            "a hard TCP read error must record TransportFailed, got {:?}",
            end_reason.get()
        );
    }

    #[test]
    fn unterminated_header_flood_records_protocol_error() {
        let raw = vec![b'A'; 128 * 1024]; // over MAX_RTSP_MESSAGE_BYTES, no CRLFCRLF.
        let (dt, _dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        let end_reason = EndReasonSlot::default();
        let handle = spawn_client_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(Mutex::new(AuthState::default())),
            None,
            stats.clone(),
            end_reason.clone(),
        )
        .unwrap();
        let _ = handle.join();
        assert!(
            matches!(
                end_reason.get(),
                Some(StreamEndReason::ProtocolError { .. })
            ),
            "an unterminated header flood must record ProtocolError, got {:?}",
            end_reason.get()
        );
    }

    #[test]
    fn rtcp_flood_records_protocol_error() {
        let mut raw = Vec::new();
        for _ in 0..(RTCP_QUEUE_BOUND + 1) {
            raw.extend_from_slice(&[b'$', 1u8, 0x00, 0x04, 0xDE, 0xAD, 0xBE, 0xEF]);
        }
        let (dt, _dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        let end_reason = EndReasonSlot::default();
        let handle = spawn_client_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(Mutex::new(AuthState::default())),
            None,
            stats.clone(),
            end_reason.clone(),
        )
        .unwrap();
        let _ = handle.join();
        assert!(
            matches!(
                end_reason.get(),
                Some(StreamEndReason::ProtocolError { .. })
            ),
            "an RTCP queue flood must record ProtocolError, got {:?}",
            end_reason.get()
        );
    }

    #[test]
    fn ctrl_flood_records_protocol_error() {
        let mut raw = Vec::new();
        for i in 0..(CTRL_QUEUE_BOUND + 1) {
            raw.extend_from_slice(format!("RTSP/1.0 200 OK\r\nCSeq: {i}\r\n\r\n").as_bytes());
        }
        let (dt, _dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        let end_reason = EndReasonSlot::default();
        let handle = spawn_client_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(Mutex::new(AuthState::default())),
            None,
            stats.clone(),
            end_reason.clone(),
        )
        .unwrap();
        let _ = handle.join();
        assert!(
            matches!(
                end_reason.get(),
                Some(StreamEndReason::ProtocolError { .. })
            ),
            "a control-response flood must record ProtocolError, got {:?}",
            end_reason.get()
        );
    }

    #[test]
    fn malformed_content_length_records_protocol_error() {
        let raw = b"RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Length: nope\r\n\r\n".to_vec();
        let (dt, _dr, rt, _rr, ct, _cr, cancel, stats) = make_args();
        let end_reason = EndReasonSlot::default();
        let handle = spawn_client_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(Mutex::new(AuthState::default())),
            None,
            stats.clone(),
            end_reason.clone(),
        )
        .unwrap();
        let _ = handle.join();
        assert!(
            matches!(
                end_reason.get(),
                Some(StreamEndReason::ProtocolError { .. })
            ),
            "a malformed Content-Length must record ProtocolError, got {:?}",
            end_reason.get()
        );
    }

    /// The `ctrl_rx` (consumer of the control-response queue) is dropped
    /// while the pump still has a response to deliver — this is the
    /// `Disconnected` arm of the `:440` exit site, distinct from the
    /// `Full` flood arm covered above. Per the site's context (`ctrl_rx`
    /// is only ever dropped alongside this pump's own `cancel` flag, from
    /// `RtspClient::Drop` or a replacement pump spawn), this is classified
    /// `Cancelled`, not `ProtocolError`.
    #[test]
    fn ctrl_rx_disconnected_records_cancelled() {
        let raw = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n".to_vec();
        let (dt, _dr, rt, _rr, ct, cr, cancel, stats) = make_args();
        drop(cr); // ctrl_rx gone before the pump ever parses the response.
        let end_reason = EndReasonSlot::default();
        let handle = spawn_client_pump(
            Cursor::new(raw),
            dt,
            rt,
            ct,
            InterleavedChannels { rtp: 0, rtcp: 1 },
            cancel.clone(),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(Mutex::new(AuthState::default())),
            None,
            stats.clone(),
            end_reason.clone(),
        )
        .unwrap();
        let _ = handle.join();
        assert!(
            matches!(end_reason.get(), Some(StreamEndReason::Cancelled)),
            "a dropped ctrl_rx must record Cancelled, got {:?}",
            end_reason.get()
        );
    }
}
