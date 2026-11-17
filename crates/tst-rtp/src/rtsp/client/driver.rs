//! The RTSP client's one I/O thread. It owns the control socket and the
//! engine, and sleeps until the socket can be read or written, a command
//! arrives or the engine's next deadline, so a request is never queued
//! behind a blocked read or a stalled write.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use mio::{Events, Interest, Poll, Token, Waker};
use tst_core::cancel::FlagCancel;
use tst_core::transport::{EndReason, EndReasonHandle};

use super::CONTROL_WRITE_TIMEOUT;
use super::engine::{ClientEngine, Event, InterleavedChannels, RequestSpec, StreamEnd, TxnId};
use super::stream::{ReadState, Stream};
use crate::error::RtspError;
use crate::rtcp::stats::RtcpStats;
use crate::rtsp::message::RtspResponse;
use crate::transport::{ingest_rtcp_packet, record_end};

const SOCKET: Token = Token(0);
const WAKE: Token = Token(1);

const COMMAND_QUEUE_BOUND: usize = 8;

/// Bounded depth of the media (RTP) queue the facade makes at an
/// interleaved SETUP.
///
/// An interleaved frame body is capped at 65535 bytes by the
/// `$<ch><len_be16>` framing, but a normal TS bundle is `DEFAULT_PKT_SIZE`
/// (1316) bytes. A depth of 1024 gives a generous jitter buffer for a
/// live consumer while bounding retained memory to ~1.3 MiB in the normal
/// case (and capping the pathological all-max-frames case at ~64 MiB). On
/// overflow the thread drops the newest frame (live-stream convention) and
/// counts it in `media_dropped`: it never blocks, so a slow consumer
/// cannot wedge the control connection.
pub(crate) const DATA_QUEUE_BOUND: usize = 1024;

const THREAD_NAME: &str = "tst-rtsp-client";

/// Where RTCP reports from the interleaved RTCP channel are counted.
pub(crate) struct RtcpSink {
    pub stats: Arc<Mutex<RtcpStats>>,
    pub ssrc: u32,
}

pub(crate) enum Command {
    /// A caller request. It replaces a request still waiting, whose
    /// `progress` is dropped without a `Done`.
    Request {
        spec: RequestSpec,
        reply_deadline: Option<Instant>,
        progress: mpsc::Sender<Progress>,
    },
    SetInterleaved {
        channels: InterleavedChannels,
        data_tx: mpsc::SyncSender<Bytes>,
        rtcp: RtcpSink,
    },
    EnableKeepalive(Option<Duration>),
}

/// A caller request's progress. `Written` comes at most once and never
/// after `Done`; `Done` comes exactly once unless the request is replaced
/// by a later one (then the channel closes without it). `Done` can come
/// with no `Written` before it: an early reply, on TLS a request handed to
/// rustls together with a ping, or a failure before the write. A caller
/// waiting for `Written` must treat `Done` as the end of that wait.
#[derive(Debug)]
pub(crate) enum Progress {
    /// Every byte of the request's first attempt is in the kernel.
    Written,
    Done(Result<RtspResponse, RtspError>),
}

/// State the thread shares with the client and its receiver.
pub(crate) struct Shared {
    pub cancel: FlagCancel,
    pub session_dead: Arc<AtomicBool>,
    pub end_reason: EndReasonHandle,
    pub media_dropped: Arc<AtomicU64>,
    /// Holds the thread once it has left its loop, before it closes the
    /// socket: it sends on the first and waits on the second.
    #[cfg(test)]
    pub exit_gate: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
}

#[derive(Debug)]
pub(crate) struct DriverHandle {
    commands: mpsc::SyncSender<Command>,
    waker: Arc<Waker>,
    stop: Arc<AtomicBool>,
    done: mpsc::Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl DriverHandle {
    /// Queue `cmd` for the thread. A full queue or an ended thread is
    /// `Io(UnexpectedEof)`.
    pub(crate) fn send(&self, cmd: Command) -> Result<(), RtspError> {
        self.commands
            .try_send(cmd)
            .map_err(|_| RtspError::Io(io::ErrorKind::UnexpectedEof))?;
        self.wake();
        Ok(())
    }

    /// Ask the thread to end the stream and return. Never blocks: the
    /// request is a flag, so a full command queue cannot lose it.
    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.wake();
    }

    /// Wait up to `bound` for the thread to finish, then join it. `false`:
    /// it is still running and stays unjoined.
    pub(crate) fn join_within(&mut self, bound: Duration) -> bool {
        let Some(thread) = self.thread.take() else {
            return true;
        };
        match self.done.recv_timeout(bound) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                true
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.thread = Some(thread);
                false
            }
        }
    }

    /// The thread's pthread id, for a test that signals it.
    #[cfg(all(test, unix))]
    pub(crate) fn pthread(&self) -> Option<std::os::unix::thread::RawPthread> {
        use std::os::unix::thread::JoinHandleExt;
        self.thread.as_ref().map(|t| t.as_pthread_t())
    }

    fn wake(&self) {
        if let Err(e) = self.waker.wake() {
            tracing::warn!(target: "tst_rtp::client::driver", error = %e, "waking the client thread failed");
        }
    }
}

// Without it a dropped handle would leave the thread waiting on its socket.
impl Drop for DriverHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the thread with the write-stall time [`CONTROL_WRITE_TIMEOUT`].
pub(crate) fn spawn(
    stream: Stream,
    engine: ClientEngine,
    shared: Shared,
) -> io::Result<DriverHandle> {
    spawn_with(stream, engine, shared, CONTROL_WRITE_TIMEOUT)
}

/// Start the thread. Queued bytes the kernel takes none of for
/// `write_stall` end the stream.
pub(crate) fn spawn_with(
    mut stream: Stream,
    engine: ClientEngine,
    shared: Shared,
    write_stall: Duration,
) -> io::Result<DriverHandle> {
    let poll = Poll::new()?;
    // Both interests for good: readiness is edge-triggered, and a writable
    // edge with nothing queued costs one empty flush.
    poll.registry().register(
        stream.source(),
        SOCKET,
        Interest::READABLE | Interest::WRITABLE,
    )?;
    let waker = Arc::new(Waker::new(poll.registry(), WAKE)?);
    let (commands, command_rx) = mpsc::sync_channel(COMMAND_QUEUE_BOUND);
    let (done_tx, done) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let driver = Driver {
        poll,
        events: Events::with_capacity(8),
        stream: Some(stream),
        engine,
        shared,
        commands: command_rx,
        stop: stop.clone(),
        _waker: waker.clone(),
        write_stall,
        caller: None,
        segments: VecDeque::new(),
        queued: 0,
        stall_from: None,
        media: None,
        rtcp: None,
        read_more: false,
        inbuf: Vec::new(),
    };
    let thread = std::thread::Builder::new()
        .name(THREAD_NAME.to_string())
        .spawn(move || {
            driver.run();
            let _ = done_tx.send(());
        })?;
    Ok(DriverHandle {
        commands,
        waker,
        stop,
        done,
        thread: Some(thread),
    })
}

struct Caller {
    txn: TxnId,
    progress: mpsc::Sender<Progress>,
    written: bool,
}

/// One transmit's place in the write queue.
struct Segment {
    /// Queue offset just past its last byte.
    end: u64,
    txn: Option<TxnId>,
    is_ping: bool,
}

enum Flow {
    Go,
    Stop,
}

struct Driver {
    poll: Poll,
    events: Events,
    /// `None` once the stream has ended.
    stream: Option<Stream>,
    engine: ClientEngine,
    shared: Shared,
    commands: mpsc::Receiver<Command>,
    stop: Arc<AtomicBool>,
    /// Keeps the wake's eventfd open while the thread runs. Closing it
    /// discards a wake the thread has not read yet, such as the one
    /// `stop()` sends just before a dropped handle releases its copy.
    _waker: Arc<Waker>,
    write_stall: Duration,
    caller: Option<Caller>,
    /// Transmits not yet wholly in the kernel, oldest first.
    segments: VecDeque<Segment>,
    /// Bytes queued on the stream, counted from the start.
    queued: u64,
    /// Since when queued bytes have made no progress.
    stall_from: Option<Instant>,
    media: Option<mpsc::SyncSender<Bytes>>,
    rtcp: Option<RtcpSink>,
    /// The last read stopped at its budget with bytes perhaps left.
    read_more: bool,
    inbuf: Vec<u8>,
}

impl Driver {
    fn run(mut self) {
        // Bytes can be waiting before the first event: rustls may hold
        // plaintext it decrypted during the handshake.
        let mut readable = true;
        loop {
            if matches!(self.drain_commands(), Flow::Stop) {
                break;
            }
            self.engine
                .handle_timeout(Instant::now(), self.shared.cancel.is_cancelled());
            if readable || self.read_more {
                self.read();
            }
            self.write();
            self.deliver();
            match self.wait() {
                Ok(r) => readable = r,
                Err(e) => {
                    tracing::warn!(target: "tst_rtp::client::driver", error = %e, "waiting on the control socket failed; the client thread ends");
                    self.end_stream(StreamEnd::ReadFailed(e));
                    break;
                }
            }
        }
        #[cfg(test)]
        if let Some((held, release)) = self.shared.exit_gate.take() {
            let _ = held.send(());
            let _ = release.recv();
        }
        self.engine.handle_closed(StreamEnd::Stopped);
        self.deliver();
        self.drop_stream();
    }

    fn drain_commands(&mut self) -> Flow {
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Flow::Stop;
            }
            let cmd = match self.commands.try_recv() {
                Ok(cmd) => cmd,
                Err(mpsc::TryRecvError::Empty) => return Flow::Go,
                Err(mpsc::TryRecvError::Disconnected) => return Flow::Stop,
            };
            match cmd {
                Command::Request {
                    spec,
                    reply_deadline,
                    progress,
                } => {
                    // The engine forgets a replaced request: its caller
                    // sees the channel close.
                    self.caller = None;
                    match self.engine.submit(spec, reply_deadline, Instant::now()) {
                        Ok(txn) => {
                            self.caller = Some(Caller {
                                txn,
                                progress,
                                written: false,
                            });
                        }
                        Err(e) => {
                            let _ = progress.send(Progress::Done(Err(e)));
                        }
                    }
                }
                Command::SetInterleaved {
                    channels,
                    data_tx,
                    rtcp,
                } => {
                    self.engine.set_interleaved(channels);
                    // After the stream's end the receiver sees its queue
                    // close at once; the reason is already recorded.
                    self.media = (!self.engine.is_closed()).then_some(data_tx);
                    self.rtcp = Some(rtcp);
                }
                Command::EnableKeepalive(interval) => {
                    self.engine.enable_keepalive(interval, Instant::now());
                }
            }
        }
    }

    fn read(&mut self) {
        self.read_more = false;
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        self.inbuf.clear();
        let r = stream.read_available(&mut self.inbuf);
        if !self.inbuf.is_empty() {
            self.engine.handle_input(&self.inbuf, Instant::now());
            // Deliver what these bytes produced before a close read with
            // them ends the stream: a TEARDOWN's reply is then seen with
            // the engine still open.
            self.deliver();
        }
        match r {
            Ok(ReadState::Open) => {}
            Ok(ReadState::More) => self.read_more = true,
            Ok(ReadState::Eof) => self.end_stream(StreamEnd::PeerClosed),
            Err(e) => {
                tracing::warn!(target: "tst_rtp::client::driver", error = %e, "control stream read failed");
                self.end_stream(StreamEnd::ReadFailed(e));
            }
        }
        // A framing violation closed the engine: the socket goes with it.
        if self.engine.is_closed() {
            self.drop_stream();
        }
    }

    fn write(&mut self) {
        let Some(stream) = self.stream.as_mut() else {
            // A closed engine queues nothing; drained all the same.
            while self.engine.poll_transmit().is_some() {}
            return;
        };
        while let Some(t) = self.engine.poll_transmit() {
            stream.queue(&t.bytes);
            self.queued += t.bytes.len() as u64;
            self.segments.push_back(Segment {
                end: self.queued,
                txn: t.txn,
                is_ping: t.is_ping,
            });
        }
        let wire_before = stream.wire_bytes();
        let flushed = stream.flush();
        let progressed = stream.wire_bytes() != wire_before;
        let written = stream.written();
        while self.segments.front().is_some_and(|s| s.end <= written) {
            if let Some(Segment { txn: Some(txn), .. }) = self.segments.pop_front() {
                self.mark_written(txn);
            }
        }
        let now = Instant::now();
        match flushed {
            Ok(true) => self.stall_from = None,
            Ok(false) => match self.stall_from {
                Some(since) if !progressed => {
                    if now.saturating_duration_since(since) >= self.write_stall {
                        let ping = self.segments.front().is_some_and(|s| s.is_ping);
                        tracing::warn!(target: "tst_rtp::client::driver", "control stream write made no progress for the write timeout");
                        self.end_stream(StreamEnd::WriteStalled { ping });
                    }
                }
                _ => self.stall_from = Some(now),
            },
            Err(e) => {
                tracing::warn!(target: "tst_rtp::client::driver", error = %e, "control stream write failed");
                self.end_stream(StreamEnd::WriteFailed(e));
            }
        }
    }

    fn mark_written(&mut self, txn: TxnId) {
        if let Some(c) = self.caller.as_mut()
            && c.txn == txn
            && !c.written
        {
            c.written = true;
            let _ = c.progress.send(Progress::Written);
        }
    }

    fn deliver(&mut self) {
        while let Some(event) = self.engine.poll_event() {
            match event {
                Event::Reply { txn, result } => {
                    if self.caller.as_ref().is_some_and(|c| c.txn == txn)
                        && let Some(c) = self.caller.take()
                    {
                        let _ = c.progress.send(Progress::Done(result));
                    }
                }
                Event::Media(packet) => {
                    if let Some(tx) = &self.media
                        && let Err(mpsc::TrySendError::Full(_)) = tx.try_send(packet)
                    {
                        self.shared.media_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Event::Rtcp(packet) => {
                    if let Some(sink) = &self.rtcp {
                        ingest_rtcp_packet(&sink.stats, sink.ssrc, &packet);
                    }
                }
                Event::SessionDead => self.shared.session_dead.store(true, Ordering::Release),
                Event::Ended(reason) => {
                    // A `454` to a ping with the stream still open ends the
                    // session, not the media: the queue stays.
                    let close_media = reason == EndReason::CleanTeardown || self.engine.is_closed();
                    // A TEARDOWN accepted on a cancelled client (a dropped
                    // one sends it after setting the cancel) is the cancel;
                    // the peer's own close, with the engine closed, is not.
                    let reason = if reason == EndReason::CleanTeardown
                        && !self.engine.is_closed()
                        && self.shared.cancel.is_cancelled()
                    {
                        EndReason::Cancelled
                    } else {
                        reason
                    };
                    // The reason first: a receiver that sees its queue
                    // close reads it at once.
                    record_end(&self.shared.end_reason, reason);
                    if close_media {
                        self.media = None;
                    }
                }
            }
        }
    }

    fn end_stream(&mut self, end: StreamEnd) {
        self.engine.handle_closed(end);
        self.drop_stream();
    }

    fn drop_stream(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            let _ = self.poll.registry().deregister(stream.source());
        }
        self.segments.clear();
        self.stall_from = None;
        self.read_more = false;
        self.inbuf = Vec::new();
    }

    /// Sleep until an event or the next deadline; `true` when the socket
    /// had one.
    fn wait(&mut self) -> io::Result<bool> {
        loop {
            let timeout = if self.read_more {
                Some(Duration::ZERO)
            } else {
                let stall = self
                    .stall_from
                    .and_then(|since| since.checked_add(self.write_stall));
                let deadline = match (self.engine.poll_timeout(), stall) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                deadline.map(|d| d.saturating_duration_since(Instant::now()))
            };
            match self.poll.poll(&mut self.events, timeout) {
                Ok(()) => return Ok(self.events.iter().any(|e| e.token() == SOCKET)),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests;
