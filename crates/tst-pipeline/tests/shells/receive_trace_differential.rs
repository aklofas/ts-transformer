//! The receive shells' event trace against a hand-written model.
//!
//! A case is one muxed stream (video and KLV, access units that span several
//! packets), optional damage, a chunking, zero or more reconnects and one way
//! the transport ends. The trace of a case is every `recv_event` result up to
//! the first terminal one and the three calls after it, each with the two
//! packet counters read right after it.
//!
//! Two oracles judge the shell. The model is the receive loop written out
//! rule by rule, with its own syncer over a plain byte buffer and the public
//! `Demuxer`; it says what the trace must be. The recorded digests say that
//! the trace did not move: two per seed, from before the shells delivered
//! the packets held at an end and from now, so that the cases that rule
//! changed can be counted and checked to be the ones it is for.

use std::collections::VecDeque;

use crate::common::{broken, drain_mux, fast_policy};

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, SamplePayload};
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};
use tst_core::mpegts::sync::aligned_packets_at_end;
use tst_core::transport::{RecvTransport, TransportError};
use tst_pipeline::{
    DemuxReceiver, DemuxReceiverError, ManagedDemuxReceiver, ManagedDemuxReceiverConfig,
    ManagedRecvTransport, ShellErrorKind,
};

const PACKET: usize = 188;
const SYNC: u8 = 0x47;
/// The largest read a scripted transport serves, and its `max_payload`.
const MAX_READ: usize = 2048;

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

/// SplitMix64: the cases must be the same on every platform and toolchain,
/// because their digests are recorded in this file.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n`.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// A value in `lo..=hi`.
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    fn chance(&mut self, num: usize, den: usize) -> bool {
        self.below(den) < num
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// How the last connection of a case ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// `TransportError::Closed`.
    Clean,
    /// `TransportError::Broken`.
    Broken,
    /// `TransportError::ExplicitClose`.
    Cancel,
    /// A read of zero bytes, for ever. Plain shell only.
    ZeroRead,
}

/// One `recv_bytes` answer before the connection's end.
#[derive(Clone, Debug)]
enum Step {
    Bytes(Vec<u8>),
    Backpressure,
}

struct Case {
    /// One script per connection. A plain shell has exactly one; a managed
    /// shell reconnects from each to the next and the stream goes on at the
    /// byte where the last connection stopped.
    connections: Vec<Vec<Step>>,
    managed: bool,
    ending: Ending,
}

impl Case {
    fn generate(seed: u64) -> Self {
        let mut rng = Rng(seed);
        let mut bytes = muxed_stream(&mut rng);
        damage(&mut rng, &mut bytes);
        let managed = rng.chance(1, 2);
        let reconnects = if managed { rng.below(3) } else { 0 };
        let ending = match rng.below(if managed { 3 } else { 4 }) {
            0 => Ending::Clean,
            1 => Ending::Broken,
            2 => Ending::Cancel,
            _ => Ending::ZeroRead,
        };
        let mut cuts: Vec<usize> = (0..reconnects)
            .map(|_| rng.below(bytes.len() + 1))
            .collect();
        cuts.sort_unstable();
        // One reconnecting case in three ends on a connection that delivers
        // nothing: the reconnect and the end come out of one read.
        if reconnects > 0 && rng.chance(1, 3) {
            cuts[reconnects - 1] = bytes.len();
        }
        cuts.push(bytes.len());
        let mut connections = Vec::new();
        let mut from = 0;
        for to in cuts {
            connections.push(chunked(&mut rng, &bytes[from..to]));
            from = to;
        }
        Self {
            connections,
            managed,
            ending,
        }
    }

    /// The error connection `i` repeats once its steps are served. Every
    /// connection but the last breaks or closes, which a managed transport
    /// answers with a reconnect.
    fn end_of(&self, i: usize) -> TransportError {
        if i + 1 < self.connections.len() {
            return if i.is_multiple_of(2) {
                broken("the link died")
            } else {
                TransportError::Closed
            };
        }
        match self.ending {
            Ending::Clean | Ending::ZeroRead => TransportError::Closed,
            Ending::Broken => broken("the link died"),
            Ending::Cancel => TransportError::ExplicitClose,
        }
    }

    fn describe(&self) -> String {
        let links: Vec<String> = self
            .connections
            .iter()
            .map(|steps| {
                let bytes: usize = steps
                    .iter()
                    .map(|s| match s {
                        Step::Bytes(b) => b.len(),
                        Step::Backpressure => 0,
                    })
                    .sum();
                format!("{bytes} bytes in {} steps", steps.len())
            })
            .collect();
        format!(
            "managed={} ending={:?} connections=[{}]",
            self.managed,
            self.ending,
            links.join("; ")
        )
    }
}

/// Zero to six video access units of 1 to 1 500 bytes, most followed by a KLV
/// unit, from the core muxer: a PES is in flight at most byte offsets.
fn muxed_stream(rng: &mut Rng) -> Vec<u8> {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264);
    prog.add_klv(0x101, KlvStreamType::PrivateData, false);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    if rng.chance(1, 2) {
        // PAT and PMT before every access unit instead of once.
        b.psi_interval_ms(10);
    }
    let mut mux = Muxer::new(b.build().unwrap()).unwrap();
    let mut out = Vec::new();
    for i in 0..rng.below(7) {
        let pts = Pts90khz::new(i as i64 * 3003);
        let len = if rng.chance(1, 2) {
            rng.range(1, 150)
        } else {
            rng.range(150, 1500)
        };
        let mut au = vec![0x00, 0x00, 0x00, 0x01, if i == 0 { 0x65 } else { 0x41 }];
        // No zero byte: the payload holds no further start code.
        au.extend(rng.bytes(len).into_iter().map(|b| b.max(1)));
        mux.push_video(&au, pts, i == 0).unwrap();
        if rng.chance(2, 3) {
            let body = rng.range(1, 100);
            let mut klv = vec![
                0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00,
                0x00, 0x00,
            ];
            klv.push(body as u8);
            klv.extend(rng.bytes(body));
            mux.push_klv(&klv, pts, 0x00).unwrap();
        }
        out.extend(drain_mux(&mut mux));
    }
    out
}

/// What a real link does to a stream: it ends early, loses its alignment, or
/// carries bytes that are not TS. Each kind is applied to a share of the
/// cases, the loss of alignment more often near the end than not.
fn damage(rng: &mut Rng, bytes: &mut Vec<u8>) {
    if rng.chance(1, 4) {
        let partial = if rng.chance(1, 2) {
            rng.below(PACKET)
        } else {
            0
        };
        bytes.truncate(rng.below(5) * PACKET + partial);
    }
    let packets = bytes.len() / PACKET;
    if packets > 0 && rng.chance(1, 3) {
        let p = if rng.chance(2, 3) {
            packets - 1 - rng.below(packets.min(5))
        } else {
            rng.below(packets)
        };
        let at = p * PACKET;
        match rng.below(3) {
            0 => bytes[at] = 0x00,
            1 => {
                let n = rng.range(1, 400);
                let junk = rng.bytes(n);
                bytes.splice(at..at, junk);
            }
            _ => {
                let n = rng.range(1, PACKET - 1);
                bytes.drain(at..at + n);
            }
        }
    }
    if rng.chance(1, 8) {
        let n = rng.range(1, 300);
        let junk = rng.bytes(n);
        bytes.splice(0..0, junk);
    }
    if rng.chance(1, 5) {
        let n = rng.range(1, 600);
        let mut junk = rng.bytes(n);
        if rng.chance(1, 2) {
            junk[0] = SYNC;
        }
        if rng.chance(1, 2) {
            junk.iter_mut().step_by(PACKET).for_each(|b| *b = SYNC);
        }
        bytes.extend(junk);
    }
}

/// One connection's bytes as reads of one size (1, 188 or 1 316 bytes) or of
/// sizes that change from read to read, with up to two `Backpressure` answers
/// anywhere among them, before the first read and after the last included.
fn chunked(rng: &mut Rng, bytes: &[u8]) -> Vec<Step> {
    let mode = rng.below(5);
    let mut steps = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let want = match mode {
            0 => 1,
            1 => PACKET,
            2 => 1316,
            3 => rng.range(1, 400),
            _ => rng.range(1, MAX_READ),
        };
        let n = want.min(bytes.len() - at);
        steps.push(Step::Bytes(bytes[at..at + n].to_vec()));
        at += n;
    }
    for _ in 0..2 {
        if rng.chance(1, 4) {
            let i = rng.below(steps.len() + 1);
            steps.insert(i, Step::Backpressure);
        }
    }
    steps
}

// ---------------------------------------------------------------------------
// Traces
// ---------------------------------------------------------------------------

/// What identifies an event: its variant, its PID and PTS where it has them,
/// and a hash of its payload bytes, so that an access unit assembled from
/// the wrong packets is a different key.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EventKey {
    name: String,
    pid: Option<u16>,
    pts: Option<i64>,
    payload: u64,
}

fn event_key(e: &DemuxEvent) -> EventKey {
    let dbg = format!("{e:?}");
    let name = dbg
        .split(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap_or_default()
        .to_string();
    let (pid, pts, payload) = match e {
        DemuxEvent::Sample {
            stream,
            pts,
            payload,
            ..
        } => {
            let bytes: &[u8] = match payload {
                SamplePayload::Video { raw, .. } => raw,
                SamplePayload::Audio { frames, .. } => frames,
                SamplePayload::Subtitle { payload, .. } => payload,
                SamplePayload::Unknown { raw, .. } => raw,
            };
            (Some(stream.pid), Some(pts.as_ticks()), fnv1a(bytes))
        }
        DemuxEvent::Metadata {
            stream,
            pts,
            payload,
            ..
        } => (Some(stream.pid), Some(pts.as_ticks()), fnv1a(payload)),
        DemuxEvent::Discontinuity { stream, .. } | DemuxEvent::NonConformant { stream, .. } => {
            (Some(stream.pid), None, 0)
        }
        _ => (None, None, 0),
    };
    EventKey {
        name,
        pid,
        pts,
        payload,
    }
}

/// One `recv_event` result.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    Event(EventKey),
    /// `Err` of kind `Backpressure`: not an end.
    Backpressure,
    /// `Ok(None)`.
    End,
    /// `Err` of any other kind, by the kind's name.
    Failed(String),
}

impl Outcome {
    fn is_terminal(&self) -> bool {
        matches!(self, Outcome::End | Outcome::Failed(_))
    }
}

/// A result with `ts_bytes_demuxed` and `packets_received` read after it.
type Entry = (Outcome, u64, u64);

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    for &b in bytes {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

fn digest(trace: &[Entry]) -> u64 {
    let mut flat = Vec::new();
    for (outcome, demuxed, packets) in trace {
        match outcome {
            Outcome::Event(k) => {
                flat.push(1);
                flat.extend_from_slice(k.name.as_bytes());
                flat.push(0xFF);
                flat.extend_from_slice(&k.pid.map_or(u32::MAX, u32::from).to_le_bytes());
                flat.extend_from_slice(&k.pts.unwrap_or(i64::MIN).to_le_bytes());
                flat.extend_from_slice(&k.payload.to_le_bytes());
            }
            Outcome::Backpressure => flat.push(2),
            Outcome::End => flat.push(3),
            Outcome::Failed(kind) => {
                flat.push(4);
                flat.extend_from_slice(kind.as_bytes());
                flat.push(0xFF);
            }
        }
        flat.extend_from_slice(&demuxed.to_le_bytes());
        flat.extend_from_slice(&packets.to_le_bytes());
    }
    fnv1a(&flat)
}

// ---------------------------------------------------------------------------
// The shell
// ---------------------------------------------------------------------------

/// Serves its steps, then its end, as the socket transports give theirs: a
/// cancel and a clean end for ever, a failure once and `Closed` after it.
struct Scripted {
    steps: VecDeque<Step>,
    end: TransportError,
    zero_read: bool,
}

impl RecvTransport for Scripted {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        match self.steps.pop_front() {
            Some(Step::Bytes(b)) => {
                buf[..b.len()].copy_from_slice(&b);
                Ok(b.len())
            }
            Some(Step::Backpressure) => Err(TransportError::Backpressure {
                msg: "nothing yet".into(),
                errno_code: None,
            }),
            None if self.zero_read => Ok(0),
            None => {
                let end = self.end.clone();
                if matches!(end, TransportError::Broken { .. }) {
                    self.end = TransportError::Closed;
                }
                Err(end)
            }
        }
    }

    fn max_payload(&self) -> usize {
        MAX_READ
    }

    fn is_alive(&self) -> bool {
        true
    }
}

trait Shell {
    fn recv(&mut self) -> Result<Option<DemuxEvent>, DemuxReceiverError>;
    fn counters(&self) -> (u64, u64);
}

impl Shell for DemuxReceiver<Scripted> {
    fn recv(&mut self) -> Result<Option<DemuxEvent>, DemuxReceiverError> {
        self.recv_event()
    }
    fn counters(&self) -> (u64, u64) {
        (self.ts_bytes_demuxed(), self.stats().packets_received)
    }
}

impl Shell for ManagedDemuxReceiver<Scripted> {
    fn recv(&mut self) -> Result<Option<DemuxEvent>, DemuxReceiverError> {
        self.recv_event()
    }
    fn counters(&self) -> (u64, u64) {
        (self.ts_bytes_demuxed(), self.stats().packets_received)
    }
}

/// More results than any case can produce: a shell that never ends fails
/// here instead of hanging the test.
const MAX_RESULTS: usize = 100_000;
/// The calls made after the first terminal result.
const CALLS_AFTER_THE_END: usize = 3;

fn drive(shell: &mut dyn Shell) -> Vec<Entry> {
    let mut trace = Vec::new();
    loop {
        let outcome = match shell.recv() {
            Ok(Some(e)) => Outcome::Event(event_key(&e)),
            Ok(None) => Outcome::End,
            Err(e) if e.kind == ShellErrorKind::Backpressure => Outcome::Backpressure,
            Err(e) => Outcome::Failed(format!("{:?}", e.kind)),
        };
        let done = outcome.is_terminal();
        let (demuxed, packets) = shell.counters();
        trace.push((outcome, demuxed, packets));
        if done {
            break;
        }
        assert!(trace.len() < MAX_RESULTS, "the shell never ended");
    }
    for _ in 0..CALLS_AFTER_THE_END {
        let outcome = match shell.recv() {
            Ok(Some(e)) => Outcome::Event(event_key(&e)),
            Ok(None) => Outcome::End,
            Err(e) if e.kind == ShellErrorKind::Backpressure => Outcome::Backpressure,
            Err(e) => Outcome::Failed(format!("{:?}", e.kind)),
        };
        let (demuxed, packets) = shell.counters();
        trace.push((outcome, demuxed, packets));
    }
    trace
}

fn shell_trace(case: &Case) -> Vec<Entry> {
    let mut links: VecDeque<Scripted> = case
        .connections
        .iter()
        .enumerate()
        .map(|(i, steps)| Scripted {
            steps: steps.iter().cloned().collect(),
            end: case.end_of(i),
            zero_read: case.ending == Ending::ZeroRead,
        })
        .collect();
    let first = links.pop_front().expect("a case has a connection");
    if !case.managed {
        return drive(&mut DemuxReceiver::new(first));
    }
    // When the connections are used up the factory fails every time, so the
    // managed transport gives up whatever its budget is.
    let factory = Box::new(move || links.pop_front().ok_or_else(|| broken("nobody answers")));
    let transport = ManagedRecvTransport::new(first, factory, fast_policy(Some(8)))
        .expect("the policy validates");
    drive(&mut ManagedDemuxReceiver::new(
        transport,
        ManagedDemuxReceiverConfig::default(),
    ))
}

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

/// What the model reads: the case's steps as a managed transport would hand
/// them on, with the count of reconnects so far.
struct ModelSource<'a> {
    case: &'a Case,
    connection: usize,
    step: usize,
    reconnects: u64,
}

enum Read<'a> {
    Bytes(&'a [u8]),
    Backpressure,
    End(Ending),
}

impl<'a> ModelSource<'a> {
    fn read(&mut self) -> Read<'a> {
        loop {
            if let Some(step) = self.case.connections[self.connection].get(self.step) {
                self.step += 1;
                return match step {
                    Step::Bytes(b) => Read::Bytes(b),
                    Step::Backpressure => Read::Backpressure,
                };
            }
            // A connection that is not the last was lost: the next one is
            // there before this read returns.
            if self.connection + 1 < self.case.connections.len() {
                self.connection += 1;
                self.step = 0;
                self.reconnects += 1;
                continue;
            }
            // A managed transport retries a broken or closed link until it
            // gives up, and reports that as a clean end.
            return Read::End(match (self.case.managed, self.case.ending) {
                (true, Ending::Broken) => Ending::Clean,
                (_, ending) => ending,
            });
        }
    }
}

/// The lock rule over a plain buffer: skip to a sync byte; while fewer than
/// four whole packets follow it, wait as long as each one present starts
/// with a sync byte and drop one byte when one does not; with four, lock.
/// Locked, every 188 bytes that start with a sync byte are a packet, and a
/// first byte that is not one costs the lock. At the end of the input the
/// packets still waiting are taken on the bytes that are there.
#[derive(Default)]
struct ModelSyncer {
    buf: Vec<u8>,
    locked: bool,
}

impl ModelSyncer {
    fn next_packet(&mut self) -> Option<[u8; PACKET]> {
        loop {
            if self.locked {
                if self.buf.len() < PACKET {
                    return None;
                }
                if self.buf[0] != SYNC {
                    self.locked = false;
                    continue;
                }
                let mut pkt = [0u8; PACKET];
                pkt.copy_from_slice(&self.buf[..PACKET]);
                self.buf.drain(..PACKET);
                return Some(pkt);
            }
            let Some(candidate) = self.buf.iter().position(|&b| b == SYNC) else {
                self.buf.clear();
                return None;
            };
            self.buf.drain(..candidate);
            let whole = self.buf.len() / PACKET;
            if (0..whole.min(4)).any(|k| self.buf[k * PACKET] != SYNC) {
                self.buf.drain(..1);
                continue;
            }
            if whole < 4 {
                return None;
            }
            self.locked = true;
        }
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.locked = false;
    }

    /// The input has ended. Whole packets waiting behind the candidate are
    /// accepted when every packet boundary present reads the sync byte.
    fn end_of_input(&mut self) {
        if !self.locked && aligned_packets_at_end(&self.buf) > 0 {
            self.locked = true;
        }
    }
}

struct Model<'a> {
    source: ModelSource<'a>,
    syncer: ModelSyncer,
    demux: Demuxer,
    /// The reconnect count the loop has acted on.
    seen_reconnects: u64,
    /// A reconnect was seen and its marker is owed.
    marker: bool,
    /// A failure whose flushed events are still being handed out.
    deferred: Option<Outcome>,
    /// An end whose released packets are still being handed out.
    held_end: Option<Ending>,
    /// The first terminal result has been given; what every later call
    /// answers.
    after_the_end: Option<Outcome>,
    /// Whether an end releases the packets the syncer holds. Without it
    /// the model is the shell as it was before that rule.
    end_rule: bool,
    /// At the first end: whether whole packets were waiting in the syncer
    /// for the end to release, and whether they were there but held across
    /// a reconnect, which releases nothing.
    waiting_at_the_end: Option<(bool, bool)>,
    packets_received: u64,
    ts_bytes_demuxed: u64,
}

impl<'a> Model<'a> {
    fn new(case: &'a Case, end_rule: bool) -> Self {
        Self {
            source: ModelSource {
                case,
                connection: 0,
                step: 0,
                reconnects: 0,
            },
            syncer: ModelSyncer::default(),
            demux: Demuxer::new(),
            seen_reconnects: 0,
            marker: false,
            deferred: None,
            held_end: None,
            after_the_end: None,
            end_rule,
            waiting_at_the_end: None,
            packets_received: 0,
            ts_bytes_demuxed: 0,
        }
    }

    fn recv_event(&mut self) -> Outcome {
        // 0. After the first terminal result nothing is read or flushed any
        //    more. A clean end and a cancel are repeated; a failure was
        //    reported once and reads as a clean end from then on.
        if let Some(answer) = &self.after_the_end {
            return answer.clone();
        }
        let outcome = self.before_the_end();
        self.after_the_end = match &outcome {
            Outcome::End => Some(Outcome::End),
            Outcome::Failed(kind) if kind == "Closed" => Some(outcome.clone()),
            Outcome::Failed(_) => Some(Outcome::End),
            _ => None,
        };
        outcome
    }

    fn before_the_end(&mut self) -> Outcome {
        loop {
            // 1. The reconnect marker comes before anything else.
            if std::mem::take(&mut self.marker) {
                return Outcome::Event(event_key(&DemuxEvent::ReconnectDiscontinuity));
            }
            // 2. Then what the demuxer has queued.
            if let Some(e) = self.demux.next_event() {
                return Outcome::Event(event_key(&e));
            }
            // 3. Then a failure whose flushed events have all been handed out.
            if let Some(failure) = self.deferred.take() {
                return failure;
            }
            // 4. Then the next aligned packet, reading until there is one or
            //    the stream has ended and what the end released is out.
            let pkt = loop {
                if let Some(pkt) = self.syncer.next_packet() {
                    self.packets_received += 1;
                    break pkt;
                }
                let ending = match self.held_end.take() {
                    Some(ending) => ending,
                    None => match self.source.read() {
                        Read::Bytes(b) => {
                            self.syncer.buf.extend_from_slice(b);
                            continue;
                        }
                        // 5. Backpressure is handed on as it is: nothing is
                        //    released or flushed and the next call reads
                        //    again.
                        Read::Backpressure => return Outcome::Backpressure,
                        // 6. An end first releases the whole packets the
                        //    syncer still holds, and waits behind them.
                        //    Behind a reconnect that no packet was read
                        //    after, it releases nothing: they were held
                        //    across the reconnect and are dropped.
                        Read::End(ending) => {
                            let whole = !self.syncer.locked && self.syncer.buf.len() >= PACKET;
                            let reconnected = self.source.reconnects > self.seen_reconnects;
                            self.waiting_at_the_end
                                .get_or_insert((whole && !reconnected, whole && reconnected));
                            if reconnected {
                                self.syncer.reset();
                            } else if self.end_rule {
                                self.syncer.end_of_input();
                            }
                            self.held_end = Some(ending);
                            continue;
                        }
                    },
                };
                // 7. Then the demuxer is flushed. A clean end is `Ok(None)`
                //    once the flushed events are out; a failure comes after
                //    them.
                self.demux.flush();
                let end = match ending {
                    Ending::Clean | Ending::ZeroRead => Outcome::End,
                    Ending::Broken => Outcome::Failed("TransportBroken".into()),
                    Ending::Cancel => Outcome::Failed("Closed".into()),
                };
                return match self.demux.next_event() {
                    Some(e) => {
                        if end != Outcome::End {
                            self.deferred = Some(end);
                        }
                        Outcome::Event(event_key(&e))
                    }
                    None => end,
                };
            };
            // 8. A packet read across a reconnect is dropped, uncounted by
            //    the demuxed-bytes counter; what the syncer and the demuxer
            //    held goes with it and the marker is owed.
            if self.source.reconnects > self.seen_reconnects {
                self.seen_reconnects = self.source.reconnects;
                self.marker = true;
                self.syncer.reset();
                self.demux.reset_sync();
                continue;
            }
            // 9. Any other packet is counted, then fed.
            self.ts_bytes_demuxed += PACKET as u64;
            self.demux
                .feed_aligned(&pkt)
                .expect("a lenient demuxer takes every aligned packet");
        }
    }
}

/// What the model saw at the first end of a case.
struct AtTheEnd {
    /// Whole packets were waiting in the syncer for the end to release.
    waiting: bool,
    /// Whole packets were in the syncer, held across a reconnect that no
    /// packet was read after: nothing is released.
    held_across_a_reconnect: bool,
}

/// The model's trace and what it saw at the end. `end_rule` off gives the
/// shell as it was before an end released anything.
fn model_run(case: &Case, end_rule: bool) -> (Vec<Entry>, AtTheEnd) {
    let mut model = Model::new(case, end_rule);
    let mut trace = Vec::new();
    loop {
        let outcome = model.recv_event();
        let done = outcome.is_terminal();
        trace.push((outcome, model.ts_bytes_demuxed, model.packets_received));
        if done {
            break;
        }
        assert!(trace.len() < MAX_RESULTS, "the model never ended");
    }
    for _ in 0..CALLS_AFTER_THE_END {
        let outcome = model.recv_event();
        trace.push((outcome, model.ts_bytes_demuxed, model.packets_received));
    }
    let (waiting, held_across_a_reconnect) =
        model.waiting_at_the_end.expect("a trace ends at an end");
    (
        trace,
        AtTheEnd {
            waiting,
            held_across_a_reconnect,
        },
    )
}

fn model_trace(case: &Case) -> Vec<Entry> {
    model_run(case, true).0
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

fn assert_same_trace(seed: u64, case: &Case, shell: &[Entry], model: &[Entry]) {
    if shell == model {
        return;
    }
    let at = shell
        .iter()
        .zip(model)
        .position(|(s, m)| s != m)
        .unwrap_or(shell.len().min(model.len()));
    panic!(
        "seed {seed} ({}): the traces differ at result {at} of {} (shell) / {} (model)\n shell: {:?}\n model: {:?}",
        case.describe(),
        shell.len(),
        model.len(),
        shell.get(at),
        model.get(at),
    );
}

/// The seeds compared with the model: the recorded ones, or more when
/// `TST_RECEIVE_TRACE_CASES` says so (the long run by hand is 10 000).
fn seeds() -> u64 {
    let recorded = RECORDED.len() as u64;
    std::env::var("TST_RECEIVE_TRACE_CASES")
        .ok()
        .and_then(|n| n.parse().ok())
        .map_or(recorded, |n: u64| n.max(recorded))
}

/// `TST_RECEIVE_TRACE_RECORD=now` prints the table at the end of this file
/// with its second column taken from the shell as it is, instead of
/// comparing with it. `=before` is for a checkout of the shell from before
/// an end released anything: it compares that shell with the model without
/// the rule and prints the table with its first column taken from it.
#[test]
fn the_shell_yields_the_models_trace_and_the_recorded_one() {
    let record = std::env::var("TST_RECEIVE_TRACE_RECORD").ok();
    let end_rule = record.as_deref() != Some("before");
    let mut printed = Vec::new();
    for seed in 0..seeds() {
        let case = Case::generate(seed);
        let shell = shell_trace(&case);
        assert_same_trace(seed, &case, &shell, &model_run(&case, end_rule).0);
        let Some(&(before, now)) = RECORDED.get(seed as usize) else {
            continue;
        };
        match record.as_deref() {
            Some("before") => printed.push((digest(&shell), now)),
            Some(_) => printed.push((before, digest(&shell))),
            None => assert_eq!(
                digest(&shell),
                now,
                "seed {seed} ({}): the trace is not the recorded one",
                case.describe()
            ),
        }
    }
    if record.is_some() {
        println!("const RECORDED: [(u64, u64); {}] = [", RECORDED.len());
        for row in printed.chunks(2) {
            let row: Vec<String> = row
                .iter()
                .map(|(before, now)| format!("(0x{before:016X}, 0x{now:016X}),"))
                .collect();
            println!("    {}", row.join(" "));
        }
        println!("];");
    }
}

/// Delivering the packets held at an end may change a trace only where
/// whole packets were waiting in the syncer for the end to release. In
/// particular a stream that ends behind a reconnect, with packets held
/// across it and an access unit of the old connection still to be flushed,
/// must read exactly as it did. The counts per ending are printed.
#[test]
fn the_end_rule_moved_only_the_traces_with_packets_waiting_at_the_end() {
    let mut moved = [0usize; 4];
    let (mut waiting_unmoved, mut held_across_a_reconnect) = (0, 0);
    for (seed, &(before, now)) in RECORDED.iter().enumerate() {
        let case = Case::generate(seed as u64);
        let (_, at_the_end) = model_run(&case, true);
        held_across_a_reconnect += usize::from(at_the_end.held_across_a_reconnect);
        if before != now {
            assert!(
                at_the_end.waiting,
                "seed {seed} ({}): the trace moved with nothing waiting to be released",
                case.describe()
            );
            moved[case.ending as usize] += 1;
        } else if at_the_end.waiting {
            waiting_unmoved += 1;
        }
    }
    println!(
        "moved, by ending [clean, broken, cancel, zero read]: {moved:?}; waiting but unmoved: \
         {waiting_unmoved}; held across a reconnect: {held_across_a_reconnect}"
    );
    assert!(moved.iter().all(|&n| n >= 5), "{moved:?}");
    assert!(held_across_a_reconnect >= 5, "{held_across_a_reconnect}");
}

/// The model's syncer against the public one: the same packets after every
/// read of every case.
#[test]
fn the_models_syncer_agrees_with_the_public_one() {
    use tst_pipeline::receiver::sync::Syncer;
    for seed in 0..RECORDED.len() as u64 {
        let case = Case::generate(seed);
        let (mut model, mut public) = (ModelSyncer::default(), Syncer::new());
        for step in case.connections.iter().flatten() {
            let Step::Bytes(bytes) = step else { continue };
            model.buf.extend_from_slice(bytes);
            public.push(bytes);
            loop {
                let (m, p) = (model.next_packet(), public.next_packet());
                assert_eq!(m, p, "seed {seed}");
                if m.is_none() {
                    break;
                }
            }
        }
    }
}

/// The comparison is only as good as the cases: each class a rule of the
/// loop depends on must be among the recorded seeds, many times over.
#[test]
fn the_recorded_seeds_cover_every_class_of_case() {
    let mut endings = [0usize; 4];
    let (mut managed, mut reconnected, mut backpressure, mut samples) = (0, 0, 0, 0);
    let (mut nothing_delivered, mut deferred_failure, mut silent_last_connection) = (0, 0, 0);
    for seed in 0..RECORDED.len() as u64 {
        let case = Case::generate(seed);
        let trace = model_trace(&case);
        let has = |name: &str| {
            trace
                .iter()
                .any(|(o, _, _)| matches!(o, Outcome::Event(k) if k.name == name))
        };
        endings[case.ending as usize] += 1;
        managed += usize::from(case.managed);
        silent_last_connection += usize::from(
            case.connections.len() > 1
                && case.connections.last().is_some_and(|steps| {
                    steps.iter().all(|step| matches!(step, Step::Backpressure))
                }),
        );
        reconnected += usize::from(has("ReconnectDiscontinuity"));
        samples += usize::from(has("Sample"));
        backpressure += usize::from(trace.iter().any(|(o, _, _)| *o == Outcome::Backpressure));
        // The first terminal result; the calls after it follow.
        let n = trace.len() - CALLS_AFTER_THE_END;
        let last = &trace[n - 1];
        assert!(last.0.is_terminal());
        nothing_delivered += usize::from(last.2 == 0);
        // A failure that came after events its own flush produced.
        deferred_failure += usize::from(
            n >= 2
                && matches!(last.0, Outcome::Failed(_))
                && matches!(&trace[n - 2].0, Outcome::Event(k) if k.name == "Sample"),
        );
    }
    let counts = format!(
        "endings {endings:?}, managed {managed}, reconnected {reconnected}, backpressure \
         {backpressure}, samples {samples}, nothing delivered {nothing_delivered}, deferred \
         failure {deferred_failure}, silent last connection {silent_last_connection}"
    );
    println!("{counts}");
    assert!(endings.iter().all(|&n| n >= 40), "{counts}");
    for n in [
        managed,
        reconnected,
        backpressure,
        samples,
        nothing_delivered,
        deferred_failure,
        silent_last_connection,
    ] {
        assert!(n >= 40, "{counts}");
    }
}

/// Two digests per seed, the seed being the index: the trace before the
/// shells delivered the packets held at an end, and the trace now.
#[rustfmt::skip]
const RECORDED: [(u64, u64); 512] = [
    (0xCBA96314C2D05806, 0xCBA96314C2D05806), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D), (0x5CD8AE4E964AAD41, 0x5CD8AE4E964AAD41),
    (0xFDC8EE9C87CD97E1, 0xFDC8EE9C87CD97E1), (0x5B0486CB1686A80B, 0x5B0486CB1686A80B),
    (0xE50B7D6914C4BB34, 0xE50B7D6914C4BB34), (0x28245F95B39CCE08, 0x3F17B2B06A129D14),
    (0x98CA8DEF0B7EB161, 0x98CA8DEF0B7EB161), (0x780902C5B23BBCF6, 0x780902C5B23BBCF6),
    (0x87AF15B7E11A9C40, 0x87AF15B7E11A9C40), (0xFBEA71ADCD150EA1, 0xFBEA71ADCD150EA1),
    (0x6EA395ADCF74A408, 0x6EA395ADCF74A408), (0x3C6B4BE9E7A61B41, 0xBA68B807DBBDB7B8),
    (0x864B525D19AAA887, 0x864B525D19AAA887), (0xDDC3B4C8CA60BC21, 0xDDC3B4C8CA60BC21),
    (0xAB1E9DBC311F6A82, 0xAB1E9DBC311F6A82), (0xCECAC93FA77BAE84, 0xCECAC93FA77BAE84),
    (0xBF6049797E872BF9, 0xBF6049797E872BF9), (0x79793BB00AAFEC88, 0x79793BB00AAFEC88),
    (0xCE14268B1672E84E, 0xCE14268B1672E84E), (0xF6491ACC0C4179F2, 0x14BCE8D45990E3C4),
    (0x5F5238A71DEA124F, 0x5F5238A71DEA124F), (0xC64AE7EE98F74DBF, 0x75E2E2EC7113F10E),
    (0x37725E019A255DEE, 0x9C0184E080534CFE), (0x2304EEF79287F3AD, 0x2304EEF79287F3AD),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0xEA412C6EF8E9893B, 0xEA412C6EF8E9893B),
    (0xB39ABAFAF78CBF28, 0xFCEBDD1EA0B914F2), (0x07DA3A7F208A89CE, 0x07DA3A7F208A89CE),
    (0xCF5EF191768960BD, 0xCF5EF191768960BD), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0x7F0034B644879C52, 0x7F0034B644879C52), (0x1D96E1FE8C7FC1D9, 0x1D96E1FE8C7FC1D9),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0xB2050D364E9E055B, 0xB2050D364E9E055B),
    (0x5CBB3EFFF786CAC5, 0x5CBB3EFFF786CAC5), (0x8DA2763405FD0428, 0x8DA2763405FD0428),
    (0x8F9B5030C46BF52D, 0x629A23F124D569AD), (0xE3E6A1E88CDEC9D6, 0xE3E6A1E88CDEC9D6),
    (0x2187D7CFAB5F132C, 0x2187D7CFAB5F132C), (0x3C3CBDF439C9E19F, 0x3C3CBDF439C9E19F),
    (0x591F2EDA2969FF73, 0x591F2EDA2969FF73), (0xF66DBC2DA3E1D2FD, 0xF66DBC2DA3E1D2FD),
    (0x823677677E8AC379, 0x823677677E8AC379), (0x8105482367BE3705, 0x8105482367BE3705),
    (0xBF6049797E872BF9, 0xBF6049797E872BF9), (0xF0154346056F4214, 0x0EE1F3846E54A423),
    (0x4AF9F899B5F26537, 0x4AF9F899B5F26537), (0xADB43C93D5D64916, 0xADB43C93D5D64916),
    (0xC1C2C32D04347F0F, 0x58C50F8839CA9FEF), (0x103D4121F4EDB359, 0x103D4121F4EDB359),
    (0x823677677E8AC379, 0xC669C20526BF936E), (0xD7EFC1299056C37F, 0xD7EFC1299056C37F),
    (0x29DC7B2B1DA17D4D, 0x4C4A877E9120043E), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0x3A949B76C6AAB118, 0x3A949B76C6AAB118), (0xCBF4D523687EBA25, 0xDB33F356BDBEDB5D),
    (0x323201DAD3ED2D8F, 0x323201DAD3ED2D8F), (0xBAFB25A11E54273B, 0xBAFB25A11E54273B),
    (0xBF6049797E872BF9, 0xBF6049797E872BF9), (0xCBF4D523687EBA25, 0xCBF4D523687EBA25),
    (0xE45A0190F9BC8BBA, 0xE45A0190F9BC8BBA), (0xCBF4D523687EBA25, 0x658BFA51725BDAAD),
    (0xBF6049797E872BF9, 0xC818B2BA4C7F7AED), (0x074AD8DFF66C3E19, 0x074AD8DFF66C3E19),
    (0x6AA947C697E2931B, 0xF55C6FE1DBA9B90E), (0x7FAEBF63AC281ECB, 0x7FAEBF63AC281ECB),
    (0x9F8CEF4116E56BB9, 0x9F8CEF4116E56BB9), (0xE2F851852146F8F4, 0xE2F851852146F8F4),
    (0xBE8CB7CDCA3CAC0E, 0xBE8CB7CDCA3CAC0E), (0x6FC71EAF3DCFF2E7, 0x6FC71EAF3DCFF2E7),
    (0x29DC7B2B1DA17D4D, 0x29DC7B2B1DA17D4D), (0x849076301D1B447B, 0x849076301D1B447B),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0xE87585D1FC73426E, 0xE87585D1FC73426E),
    (0x7F8CACFB73339B37, 0x7F8CACFB73339B37), (0x8105482367BE3705, 0x8105482367BE3705),
    (0x748E80AE25DD7184, 0x748E80AE25DD7184), (0xEFB095A4663DF615, 0xEFB095A4663DF615),
    (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D), (0x4B3EE6AFC3CE5A03, 0x4B3EE6AFC3CE5A03),
    (0x6DE2AFF694ACE946, 0x6DE2AFF694ACE946), (0x103D4121F4EDB359, 0x103D4121F4EDB359),
    (0x4B0DEE2B85615320, 0x4B0DEE2B85615320), (0xB5B3FD72BCC1E18A, 0xB5B3FD72BCC1E18A),
    (0xEAE5FAFDC2CBDCC6, 0xEAE5FAFDC2CBDCC6), (0x069CE379851C9F17, 0x069CE379851C9F17),
    (0x03E1C2FBF51E800D, 0x03E1C2FBF51E800D), (0xF405570C2717B0A6, 0xF405570C2717B0A6),
    (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D), (0xCBF4D523687EBA25, 0xCBF4D523687EBA25),
    (0xCBF4D523687EBA25, 0xCBF4D523687EBA25), (0x823677677E8AC379, 0x823677677E8AC379),
    (0x608AC577E1051C9B, 0x608AC577E1051C9B), (0x29DC7B2B1DA17D4D, 0xC3ECF56F61E4152D),
    (0xBF6049797E872BF9, 0xBF6049797E872BF9), (0x8CC8717B172E4576, 0x8CC8717B172E4576),
    (0xE2B36D90089B7FFE, 0xE2B36D90089B7FFE), (0xE3E0B4450D0F8BDE, 0xE3E0B4450D0F8BDE),
    (0x8105482367BE3705, 0x8105482367BE3705), (0x8983A433CE68F8B6, 0x8983A433CE68F8B6),
    (0x7E998A68606FE4F7, 0x7E998A68606FE4F7), (0x991CC6A2BF1444F8, 0x959F68DD591856CE),
    (0x1CC79B35CF6682CD, 0x1CC79B35CF6682CD), (0xF6EB02F905C0C771, 0xF6EB02F905C0C771),
    (0x20F703845C7CE6F4, 0x20F703845C7CE6F4), (0x1CC79B35CF6682CD, 0xCC71F5463CDFDD78),
    (0xBD12E52B7E44B638, 0xF179C5D614B8D5A0), (0xB088F9D816CA9131, 0x0D1C1252C37E8649),
    (0x22D3B8F2AB4D4028, 0x22D3B8F2AB4D4028), (0xADB955CA44606E33, 0xADB955CA44606E33),
    (0xA427DA4663142D74, 0xF9F0A021D98E4861), (0x29DC7B2B1DA17D4D, 0xD056A3C70083E062),
    (0x665630C9E61B5C1B, 0x665630C9E61B5C1B), (0x5CBF84CDBDCD52A2, 0x5CBF84CDBDCD52A2),
    (0x5A3172CA3AF1CA2D, 0x5A3172CA3AF1CA2D), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0x8A4EA3292C03EDEE, 0x8A4EA3292C03EDEE), (0xD4A3FBF9F6E2F195, 0xD4A3FBF9F6E2F195),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0x004CBFBF4373ECE2, 0x004CBFBF4373ECE2),
    (0x6BD45A09C95DC483, 0x6BD45A09C95DC483), (0xED76DD66888F836D, 0xED76DD66888F836D),
    (0x64690003551DBA50, 0xDE868AFCF76B2D08), (0xCBF4D523687EBA25, 0xE40538D9459894A7),
    (0x05CEC3172020ADBF, 0x10DB8B9484014E2F), (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D),
    (0x047AE916B188D424, 0xC1AC83497F26793F), (0xCBF4D523687EBA25, 0xCBF4D523687EBA25),
    (0xB99AC0BAEC5282AD, 0xB99AC0BAEC5282AD), (0x8F9B5030C46BF52D, 0x1E365DB6D185EF85),
    (0xAE726F362256DB07, 0xAE726F362256DB07), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0xBF6049797E872BF9, 0xC818B2BA4C7F7AED), (0x966E1930FDD18744, 0x966E1930FDD18744),
    (0x541867C78164EDF0, 0x58B45AA1966A7C21), (0x103D4121F4EDB359, 0x103D4121F4EDB359),
    (0x86B43078D597C1B6, 0x86B43078D597C1B6), (0x103D4121F4EDB359, 0x103D4121F4EDB359),
    (0x6BD45A09C95DC483, 0xA295B335498969CB), (0x52C1177FB62CAC2A, 0x52C1177FB62CAC2A),
    (0x7B3608FDA4D414F5, 0x7B3608FDA4D414F5), (0x67BA0CA185C22685, 0x67BA0CA185C22685),
    (0x12229BE7C505EFE9, 0x12229BE7C505EFE9), (0x640CB8C0605EE036, 0x640CB8C0605EE036),
    (0x78B78C0B12E78E5F, 0x78B78C0B12E78E5F), (0x8105482367BE3705, 0x90613EE8CA7116F9),
    (0xFC24CF873FF49ABA, 0xFC24CF873FF49ABA), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0x103D4121F4EDB359, 0x3297C6EC5566304D), (0x3FBBE536E52DCF90, 0x3FBBE536E52DCF90),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0x4FBDDA6C8EE24BE1, 0x4FBDDA6C8EE24BE1),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0x103D4121F4EDB359, 0x103D4121F4EDB359),
    (0x1019D95B2B221CDD, 0xD678F6D056FFB6CB), (0xEAEC02883814CA2E, 0xEAEC02883814CA2E),
    (0xED4B03DD1AC3E38E, 0x1DC31C525B7F2C0B), (0xB87EE24E99E3313C, 0x4BA87723FF2182D7),
    (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D), (0x2832EA5E77061BAD, 0x2832EA5E77061BAD),
    (0x8F9B5030C46BF52D, 0xEA3AD6CAD55094E6), (0x7184F208BF036565, 0x7184F208BF036565),
    (0xF908C909B5A40CBD, 0xF908C909B5A40CBD), (0xAB1E9DBC311F6A82, 0xAB1E9DBC311F6A82),
    (0xCF216CB58C192C94, 0xCF216CB58C192C94), (0x38F76A20D39521D1, 0x38F76A20D39521D1),
    (0x997090B202FD2B29, 0x997090B202FD2B29), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0xC1E45ED86C4FDCBF, 0xC1E45ED86C4FDCBF), (0xC824A15E2B88D6C2, 0xC824A15E2B88D6C2),
    (0x1CC79B35CF6682CD, 0x1CC79B35CF6682CD), (0xBAC3E2E08B65AE78, 0xBAC3E2E08B65AE78),
    (0x09A0FE25946D6B7F, 0x09A0FE25946D6B7F), (0x024B5956B2F685D9, 0x024B5956B2F685D9),
    (0x6271B4211D36320E, 0x6271B4211D36320E), (0x5E96B5D24E8959F3, 0x5E96B5D24E8959F3),
    (0x37C83B5126A50216, 0x37C83B5126A50216), (0x29DC7B2B1DA17D4D, 0x29DC7B2B1DA17D4D),
    (0x3B88FEDBB17699A1, 0x3B88FEDBB17699A1), (0xC309648F12EEDF69, 0xC309648F12EEDF69),
    (0x823677677E8AC379, 0x823677677E8AC379), (0x29DC7B2B1DA17D4D, 0x29DC7B2B1DA17D4D),
    (0x215F6A89F999AE00, 0x215F6A89F999AE00), (0xD532812D542F2EFC, 0xD532812D542F2EFC),
    (0x9A17BCD7AC9FCA70, 0x9A17BCD7AC9FCA70), (0xD8D7E674668BD9A5, 0xD8D7E674668BD9A5),
    (0x823677677E8AC379, 0x823677677E8AC379), (0x08D9811C32EADBF7, 0x08D9811C32EADBF7),
    (0x621ADB6E4750847F, 0xF50EEC806701363B), (0xE8ACB8F286041E8E, 0xE8ACB8F286041E8E),
    (0xF33E6FAF30371625, 0xF33E6FAF30371625), (0x46D28F45910521A8, 0x46D28F45910521A8),
    (0xE89817C9DA1B150D, 0xC00544973A515995), (0x02DAB43B3CD008CB, 0x02DAB43B3CD008CB),
    (0xFAFEE1807016EF23, 0xFAFEE1807016EF23), (0x1B714F25DE8829B4, 0x1B714F25DE8829B4),
    (0xFBC7D6FF97168D53, 0xFBC7D6FF97168D53), (0x4DBC6C33AF96D286, 0x4DBC6C33AF96D286),
    (0xED57D762D0834507, 0xED57D762D0834507), (0xA50888183B0F9BC2, 0xA50888183B0F9BC2),
    (0xEB5F851A9BC40706, 0xEB5F851A9BC40706), (0x103D4121F4EDB359, 0x103D4121F4EDB359),
    (0x8F9B5030C46BF52D, 0x956FE4B782D842EE), (0xAA35EC523CCC26BB, 0xAA35EC523CCC26BB),
    (0xBF6049797E872BF9, 0xBF6049797E872BF9), (0x115B3BC7F58AD718, 0x115B3BC7F58AD718),
    (0xAD7A557F2F7293DE, 0xAD7A557F2F7293DE), (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D),
    (0x103D4121F4EDB359, 0x3297C6EC5566304D), (0xB571D72063AEB22E, 0xB571D72063AEB22E),
    (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D), (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D),
    (0x86DDF78240A2EA26, 0x86DDF78240A2EA26), (0xE620CBF2AA171912, 0x2AE6884650F6A54A),
    (0x823677677E8AC379, 0x823677677E8AC379), (0x957681C35A3FFFF1, 0x957681C35A3FFFF1),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0x83A220CFE96D0941, 0x83A220CFE96D0941),
    (0x506BA21172AA5E1A, 0x506BA21172AA5E1A), (0xFBEF8EF02E334985, 0xFBEF8EF02E334985),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0x57C47AF031C0E959, 0x57C47AF031C0E959),
    (0x8331F0887BB0931B, 0x8331F0887BB0931B), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0x94C2DF7A6C387418, 0x94C2DF7A6C387418), (0x0D6CFD72AAE6F9A2, 0x0D6CFD72AAE6F9A2),
    (0x147FDA430D852AA1, 0x147FDA430D852AA1), (0x823677677E8AC379, 0x2DB7C05ED969986D),
    (0xB77914AFE4BC80E2, 0xB77914AFE4BC80E2), (0x3C633BA4EAD7E960, 0x3C633BA4EAD7E960),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0xAB1E9DBC311F6A82, 0xAB1E9DBC311F6A82),
    (0x7FDA340E795D9C8B, 0x7FDA340E795D9C8B), (0x81F18DFE17F5703C, 0x81F18DFE17F5703C),
    (0xCCBC232048C66356, 0xCCBC232048C66356), (0xD85F9AA11B761F8B, 0xD85F9AA11B761F8B),
    (0xBF6049797E872BF9, 0xBF6049797E872BF9), (0xEB9A9DB8C793494E, 0xEB9A9DB8C793494E),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0x2BAC6B7F1DFD7599, 0x2BAC6B7F1DFD7599),
    (0x0538D92456A9156C, 0x0538D92456A9156C), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0x8DFAF18CA4E4A620, 0x8DFAF18CA4E4A620), (0x3C47BF8FD0C112CA, 0x3C47BF8FD0C112CA),
    (0xF2BB3E1F752F4291, 0xE6D8C68D450F6BA5), (0xCBF4D523687EBA25, 0x6406622D4EF45266),
    (0x49FE2049F4141125, 0x49FE2049F4141125), (0xCBF4D523687EBA25, 0x725235273D4BEA21),
    (0x3FB4A6CA384F9618, 0x3FB4A6CA384F9618), (0x76BEC3B8D814D938, 0x76BEC3B8D814D938),
    (0xCBF4D523687EBA25, 0xCEFA85B666965F99), (0x8E4F083605CA0965, 0x8E4F083605CA0965),
    (0x103D4121F4EDB359, 0xCF3536E8A2F750A9), (0xAADE33BFE83B22BB, 0xAADE33BFE83B22BB),
    (0x6FBB6036367F2520, 0x6FBB6036367F2520), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0xBF6049797E872BF9, 0xBF6049797E872BF9), (0xB33B820009D8D8FD, 0xB33B820009D8D8FD),
    (0x823677677E8AC379, 0x823677677E8AC379), (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D),
    (0xF399307BD1DDAE2B, 0x5D2A44A326E12BC6), (0xEDCC29554B17C8E8, 0xEDCC29554B17C8E8),
    (0xDA79F1CD620EA0A5, 0x80D751D136DBD0A1), (0x103D4121F4EDB359, 0x103D4121F4EDB359),
    (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D), (0x8105482367BE3705, 0x8105482367BE3705),
    (0x13AF45768150A135, 0x13AF45768150A135), (0x1CC79B35CF6682CD, 0x1CC79B35CF6682CD),
    (0x80599409C85625C6, 0x80599409C85625C6), (0x85A786B5320816FB, 0x85A786B5320816FB),
    (0x4A8A03C0360A1104, 0x4A8A03C0360A1104), (0xB85C63B0C430F99B, 0xB85C63B0C430F99B),
    (0xBF6049797E872BF9, 0xBF6049797E872BF9), (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D),
    (0x155A95F29D3C244E, 0x155A95F29D3C244E), (0xD37FBA9DB7DF9376, 0xD37FBA9DB7DF9376),
    (0x29DC7B2B1DA17D4D, 0xC3629B48B3F82133), (0x35313A98DD3CD5C5, 0x35313A98DD3CD5C5),
    (0x1951BF495675A893, 0x1951BF495675A893), (0x646DB24970EEDC19, 0x646DB24970EEDC19),
    (0xF8BAC8DA63335D14, 0xF8BAC8DA63335D14), (0x725F24C9FFD2BB44, 0x725F24C9FFD2BB44),
    (0xBF6049797E872BF9, 0xBF6049797E872BF9), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0x5DD46A908B94C72E, 0x5DD46A908B94C72E), (0x6C487B9353187A45, 0x6C487B9353187A45),
    (0xCE720BAAB30931DA, 0xCE720BAAB30931DA), (0x799D386AD54EB2AC, 0x799D386AD54EB2AC),
    (0xB88CCE05867F4B7C, 0xB88CCE05867F4B7C), (0xA649DB4E8436CE59, 0xA649DB4E8436CE59),
    (0xFBBD37CDEFE00246, 0xFBBD37CDEFE00246), (0x823677677E8AC379, 0x823677677E8AC379),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0xBF6049797E872BF9, 0x8BE6E665BFB9DC0D),
    (0x8105482367BE3705, 0x8105482367BE3705), (0x8105482367BE3705, 0x8105482367BE3705),
    (0xB793099E19EFFA69, 0xB793099E19EFFA69), (0x103D4121F4EDB359, 0xF7A722D64F853B1F),
    (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D), (0x8105482367BE3705, 0x8105482367BE3705),
    (0x5CFEFFCA897B3799, 0x5CFEFFCA897B3799), (0xE36B737D9CB3F648, 0xE36B737D9CB3F648),
    (0x51E8DAB7D5F3AFA3, 0x51E8DAB7D5F3AFA3), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0x103D4121F4EDB359, 0xA17ACA682D23AC95), (0x23D45E9E45BE5627, 0x23D45E9E45BE5627),
    (0x6AA947C697E2931B, 0x6AA947C697E2931B), (0x0D2299BC3253C123, 0x0D2299BC3253C123),
    (0xBB9A5178EEFAD0B2, 0xBB9A5178EEFAD0B2), (0xE51414903A889E28, 0xE51414903A889E28),
    (0x29B29B5A231D907C, 0x29B29B5A231D907C), (0x5C43F69D0CDD4821, 0x5C43F69D0CDD4821),
    (0xFC73FF4BC5D8299C, 0xFC73FF4BC5D8299C), (0x7FFFEDC0DD24431D, 0x7FFFEDC0DD24431D),
    (0xCBF4D523687EBA25, 0xCBF4D523687EBA25), (0xFF4B938DB3825B31, 0xFF4B938DB3825B31),
    (0x874403522E22FA8E, 0x874403522E22FA8E), (0x101AE258297601C1, 0x101AE258297601C1),
    (0x09233F556954321B, 0x09233F556954321B), (0xEEFDC3C0F8B3112E, 0xEEFDC3C0F8B3112E),
    (0x6BD45A09C95DC483, 0x6BD45A09C95DC483), (0xF1175A996BB8C30F, 0xF1175A996BB8C30F),
    (0xB166C41C44AA6DDF, 0xB166C41C44AA6DDF), (0xC02FCDEE1DE0CC43, 0xC02FCDEE1DE0CC43),
    (0x031591FD0B75EA75, 0x031591FD0B75EA75), (0x8857673535B6F943, 0x8857673535B6F943),
    (0x1AF5BA9596C2DF94, 0x1AF5BA9596C2DF94), (0x03825DD7CA8DFA06, 0x03825DD7CA8DFA06),
    (0x29DC7B2B1DA17D4D, 0x3D88AE0766E9E38E), (0xB8926B4EED7E0AEE, 0xB8926B4EED7E0AEE),
    (0xE644E7AD1C81519F, 0xE644E7AD1C81519F), (0xCEC292DDDF531A2D, 0xCEC292DDDF531A2D),
    (0x64108F2898EF2B16, 0x64108F2898EF2B16), (0x475D9D260C17D376, 0x475D9D260C17D376),
    (0x1CC79B35CF6682CD, 0x1CC79B35CF6682CD), (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D),
    (0x4B80F6FBD260B974, 0x4B80F6FBD260B974), (0xBC48E8C951F5BC9B, 0xBC48E8C951F5BC9B),
    (0x823677677E8AC379, 0x954AC44DE142C6D2), (0xD71D1A140BB67B88, 0x996085CF428BEAB6),
    (0xBF6049797E872BF9, 0xECB62118A3798C9A), (0xDE3D1DCB493E4435, 0xDE3D1DCB493E4435),
    (0x4EB3DACC27552735, 0x4EB3DACC27552735), (0x6ACC0D2C04930A46, 0x6ACC0D2C04930A46),
    (0x0BD65130451FC20D, 0x0BD65130451FC20D), (0xE86F1B2A39C24DC2, 0xE86F1B2A39C24DC2),
    (0x9170FA6F47509D33, 0x9170FA6F47509D33), (0x111F0774C8F184E2, 0x111F0774C8F184E2),
    (0xDFE4393005997189, 0xDFE4393005997189), (0xB3A699A25043D80A, 0xB3A699A25043D80A),
    (0xADF8208969E31833, 0xADF8208969E31833), (0x440007C2BF8204AD, 0x440007C2BF8204AD),
    (0x487D77F209A3D9C0, 0x487D77F209A3D9C0), (0xC0867BDB0D2937FF, 0xC4DEE0AF7A132CEF),
    (0x29DC7B2B1DA17D4D, 0x8686B02240B42E25), (0x6AA947C697E2931B, 0x6AA947C697E2931B),
    (0x5CA0CF76F038B489, 0x5CA0CF76F038B489), (0xAD0C99BAA3FC9DBB, 0xAD0C99BAA3FC9DBB),
    (0xED7D19C7B5FC3195, 0xE570C9E63D9DEFC1), (0x0045D2AB313E9973, 0x0045D2AB313E9973),
    (0xECD5DEF2EB453731, 0xECD5DEF2EB453731), (0x882B0EEF65E0E053, 0x882B0EEF65E0E053),
    (0x8836A3DA02F4B85B, 0x8836A3DA02F4B85B), (0x29DC7B2B1DA17D4D, 0xACA6957761589ED9),
    (0x53B6C99393FF9F76, 0x53B6C99393FF9F76), (0x29DC7B2B1DA17D4D, 0x29DC7B2B1DA17D4D),
    (0x492AFDF4A6C483CE, 0x492AFDF4A6C483CE), (0xCC96374543897D31, 0xCC96374543897D31),
    (0x6AA947C697E2931B, 0xE0061D07EE96570F), (0x823677677E8AC379, 0x823677677E8AC379),
    (0xE5728AC14663BED3, 0xE5728AC14663BED3), (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D),
    (0x947901104C15DA8C, 0x947901104C15DA8C), (0x103D4121F4EDB359, 0x7ABCD21195327006),
    (0x103D4121F4EDB359, 0x9E949CC133E52F6D), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0xDA69727694BE0358, 0xDA69727694BE0358),
    (0x58CE5F2880B4FCB3, 0x58CE5F2880B4FCB3), (0x3364E652ACBA0842, 0x3364E652ACBA0842),
    (0xAC59C882043EE326, 0xAC59C882043EE326), (0x1CC79B35CF6682CD, 0x1CC79B35CF6682CD),
    (0x8105482367BE3705, 0x8105482367BE3705), (0x103D4121F4EDB359, 0x103D4121F4EDB359),
    (0x6D71F48A0C4F3213, 0x6D71F48A0C4F3213), (0x4DE89F3CA76E4D4F, 0x4DE89F3CA76E4D4F),
    (0xD370CFC6955AFC5B, 0xD370CFC6955AFC5B), (0x103D4121F4EDB359, 0x103D4121F4EDB359),
    (0x144064DC81100B40, 0x144064DC81100B40), (0x6AA947C697E2931B, 0x6AA947C697E2931B),
    (0x3A2C5E8B77BCF61D, 0x3A2C5E8B77BCF61D), (0x6E11F3C89E2BD512, 0x6E11F3C89E2BD512),
    (0x33E590C171D7A8D3, 0x33E590C171D7A8D3), (0xE8EA05168582AE13, 0xE8EA05168582AE13),
    (0xC436FB9FDD567260, 0xC436FB9FDD567260), (0xCF70565BA082E7F3, 0xCF70565BA082E7F3),
    (0xA217B6AF9F2783F0, 0xA217B6AF9F2783F0), (0x067E4BB6EB99C776, 0x067E4BB6EB99C776),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0x349393944E8B44D5, 0xEB0885B300F992A1),
    (0xB94134825A4EB257, 0xB94134825A4EB257), (0x29DC7B2B1DA17D4D, 0x29DC7B2B1DA17D4D),
    (0x823677677E8AC379, 0x823677677E8AC379), (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D),
    (0x47787CED1618A9A4, 0xDDE07B9B8CE88214), (0x499A4436E8A65157, 0x499A4436E8A65157),
    (0xA01952C94C3F679C, 0xA01952C94C3F679C), (0x8F9B5030C46BF52D, 0x44B3D02408559A45),
    (0xBF6049797E872BF9, 0x0087A71B5CFB2D7B), (0xD9103FE904EDC1CC, 0xD9103FE904EDC1CC),
    (0xFA9711851C6BF4DF, 0xFA9711851C6BF4DF), (0x0A3F23148D48D11D, 0x0A3F23148D48D11D),
    (0x74B56718C3C11FF6, 0x74B56718C3C11FF6), (0x823677677E8AC379, 0x823677677E8AC379),
    (0xBF6049797E872BF9, 0xBF6049797E872BF9), (0x8105482367BE3705, 0xFFA58C300EC5A501),
    (0xC9F9421444CE3A96, 0xC9F9421444CE3A96), (0x29DC7B2B1DA17D4D, 0x29DC7B2B1DA17D4D),
    (0x6328D1376060EF4D, 0xF61FFA593724230F), (0x901B54C0A8D4B1DA, 0x901B54C0A8D4B1DA),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0x1CC79B35CF6682CD, 0x1CC79B35CF6682CD),
    (0x26CD5B963B1C44CB, 0xD0113599FC20B867), (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D),
    (0x6BD45A09C95DC483, 0x6BD45A09C95DC483), (0x103D4121F4EDB359, 0x103D4121F4EDB359),
    (0xDA79F1CD620EA0A5, 0x3AB6C2A9D33FC019), (0x0AECD24C64AC8ED0, 0x0AECD24C64AC8ED0),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0x266224A4642CD394, 0x266224A4642CD394),
    (0x82B429C15D95B797, 0x82B429C15D95B797), (0x486DE680E859CD63, 0x486DE680E859CD63),
    (0x29DC7B2B1DA17D4D, 0x29DC7B2B1DA17D4D), (0x2889C05482ACBEA1, 0x2889C05482ACBEA1),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0x3D6ECE4FE025A069, 0x3D6ECE4FE025A069),
    (0x71BE1E2898F4065B, 0x71BE1E2898F4065B), (0x667CEFCF4DD8DDB4, 0x667CEFCF4DD8DDB4),
    (0x1FE5ED23AB86DB9A, 0x1FE5ED23AB86DB9A), (0x96E54904F85B3C25, 0x96E54904F85B3C25),
    (0x3B2FC1FE3F4FB96F, 0x3B2FC1FE3F4FB96F), (0xFA94D2DA4098D77E, 0xFA94D2DA4098D77E),
    (0x1EDFDF86EA976721, 0x1EDFDF86EA976721), (0xC9D9F19D9C4A0C8F, 0xC9D9F19D9C4A0C8F),
    (0xAFA5E6F9D87DBACE, 0xAFA5E6F9D87DBACE), (0x858277627A1F4311, 0x858277627A1F4311),
    (0x8F9B5030C46BF52D, 0x1E365DB6D185EF85), (0xCBF4D523687EBA25, 0xCBF4D523687EBA25),
    (0xBF6049797E872BF9, 0xD74621A3075C1335), (0x828F5FCA58E220F5, 0x828F5FCA58E220F5),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0x8F01E9364DC0D2B4, 0xB9F757B307F99E08),
    (0xD424EC2C62563536, 0xD424EC2C62563536), (0x29DC7B2B1DA17D4D, 0x29DC7B2B1DA17D4D),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0x7300E40876E58494, 0xEB960B7A2F710F4C),
    (0x23879C434B68BB41, 0x23879C434B68BB41), (0xB3945AA293120653, 0xB3945AA293120653),
    (0xBF6049797E872BF9, 0xD74621A3075C1335), (0x3CF5431AEEA813D6, 0x3CF5431AEEA813D6),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0x75A3BEA16BC69103, 0x75A3BEA16BC69103),
    (0x29DC7B2B1DA17D4D, 0x6D15C8C4EE1D034B), (0x97093E88E464EF50, 0x97093E88E464EF50),
    (0xBF6049797E872BF9, 0xBF6049797E872BF9), (0xE320821C62AD5F3E, 0xE320821C62AD5F3E),
    (0xBF6049797E872BF9, 0xD74621A3075C1335), (0x0A242F8B66C70499, 0x0A242F8B66C70499),
    (0x1CC79B35CF6682CD, 0x1CC79B35CF6682CD), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0x29DC7B2B1DA17D4D, 0x8686B02240B42E25), (0x8105482367BE3705, 0x8105482367BE3705),
    (0x103D4121F4EDB359, 0xF07D7EEE27BF5C44), (0xFE03DA23E622D1F1, 0xFE03DA23E622D1F1),
    (0xEC4AC171A1BFC049, 0x5AAB19C021567B2D), (0x9FE0E0F19E47F68E, 0x9FE0E0F19E47F68E),
    (0xBECF849D754D2532, 0xBECF849D754D2532), (0xD57B82E11B85B403, 0xD57B82E11B85B403),
    (0xBF6049797E872BF9, 0xBF6049797E872BF9), (0xB7BAD93E62E49D4D, 0xB7BAD93E62E49D4D),
    (0xEF6DD4F08FDCA38E, 0xEF6DD4F08FDCA38E), (0x1D96E1FE8C7FC1D9, 0x1D96E1FE8C7FC1D9),
    (0xF014DD0F78A270F8, 0xF014DD0F78A270F8), (0xCBF4D523687EBA25, 0xCBF4D523687EBA25),
    (0xED42F65BBE5DD2A0, 0xED42F65BBE5DD2A0), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0x823677677E8AC379, 0x823677677E8AC379), (0xF809C8499F016D03, 0xF809C8499F016D03),
    (0xFD85BA098CA9764C, 0xFD85BA098CA9764C), (0x108C16627A37256C, 0x108C16627A37256C),
    (0xF05D6AB0500A0BBD, 0xF05D6AB0500A0BBD), (0x8F9B5030C46BF52D, 0x8F9B5030C46BF52D),
    (0x8C298519B8802CEE, 0x8C298519B8802CEE), (0xBB5C325716391ECD, 0x4FE80ECCBAB2F0A4),
    (0x85A786B5320816FB, 0x85A786B5320816FB), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0x5364A3696DAA0680, 0x5364A3696DAA0680), (0x29DC7B2B1DA17D4D, 0x29DC7B2B1DA17D4D),
    (0xBF6049797E872BF9, 0xBF6049797E872BF9), (0xA485F88823131832, 0xA485F88823131832),
    (0xBF6049797E872BF9, 0xD32CE0E03C63C7EE), (0xBF6049797E872BF9, 0xBF6049797E872BF9),
    (0x100DE850C8C630FA, 0xBC027404D5248B07), (0x76BEC3B8D814D938, 0x76BEC3B8D814D938),
    (0x63037D30FE0CCEBD, 0x63037D30FE0CCEBD), (0x734BA4A11A1A4AF3, 0x734BA4A11A1A4AF3),
    (0x103D4121F4EDB359, 0x103D4121F4EDB359), (0x1D96E1FE8C7FC1D9, 0x1D96E1FE8C7FC1D9),
    (0xB92EADB2BCCE4256, 0x8D86C081BFEB3947), (0x6AA947C697E2931B, 0x6AA947C697E2931B),
];
