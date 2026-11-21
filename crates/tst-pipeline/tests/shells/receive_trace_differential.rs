//! The receive shells' event trace against a hand-written model.
//!
//! A case is one muxed stream (video and KLV, access units that span several
//! packets), optional damage, a chunking, zero or more reconnects and one way
//! the transport ends. The trace of a case is every `recv_event` result up to
//! and including the first terminal one, each with the two packet counters
//! read right after it.
//!
//! Two oracles judge the shell. The model is the receive loop written out
//! rule by rule, with its own syncer over a plain byte buffer and the public
//! `Demuxer`; it says what the trace must be. The recorded digests, one per
//! seed, say that the trace did not move. What a shell answers after its
//! first terminal result is outside both.

use std::collections::VecDeque;

use crate::common::{broken, drain_mux, fast_policy};

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, SamplePayload};
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};
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

/// Serves its steps, then repeats its end for ever.
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
            None => Err(self.end.clone()),
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
            return trace;
        }
        assert!(trace.len() < MAX_RESULTS, "the shell never ended");
    }
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
/// first byte that is not one costs the lock.
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
    packets_received: u64,
    ts_bytes_demuxed: u64,
}

impl<'a> Model<'a> {
    fn new(case: &'a Case) -> Self {
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
            packets_received: 0,
            ts_bytes_demuxed: 0,
        }
    }

    fn recv_event(&mut self) -> Outcome {
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
            // 4. Then the next aligned packet, reading until there is one.
            let pkt = loop {
                if let Some(pkt) = self.syncer.next_packet() {
                    self.packets_received += 1;
                    break pkt;
                }
                match self.source.read() {
                    Read::Bytes(b) => self.syncer.buf.extend_from_slice(b),
                    // 5. Backpressure is handed on as it is: nothing is
                    //    flushed and the next call reads again.
                    Read::Backpressure => return Outcome::Backpressure,
                    // 6. A clean end flushes the demuxer; `Ok(None)` comes
                    //    once the flushed events are out.
                    Read::End(Ending::Clean | Ending::ZeroRead) => {
                        self.demux.flush();
                        return match self.demux.next_event() {
                            Some(e) => Outcome::Event(event_key(&e)),
                            None => Outcome::End,
                        };
                    }
                    // 7. A failure flushes too and comes after the flushed
                    //    events.
                    Read::End(ending) => {
                        self.demux.flush();
                        let failure = Outcome::Failed(
                            if ending == Ending::Broken {
                                "TransportBroken"
                            } else {
                                "Closed"
                            }
                            .into(),
                        );
                        return match self.demux.next_event() {
                            Some(e) => {
                                self.deferred = Some(failure);
                                Outcome::Event(event_key(&e))
                            }
                            None => failure,
                        };
                    }
                }
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

fn model_trace(case: &Case) -> Vec<Entry> {
    let mut model = Model::new(case);
    let mut trace = Vec::new();
    loop {
        let outcome = model.recv_event();
        let done = outcome.is_terminal();
        trace.push((outcome, model.ts_bytes_demuxed, model.packets_received));
        if done {
            return trace;
        }
        assert!(trace.len() < MAX_RESULTS, "the model never ended");
    }
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

/// With `TST_RECEIVE_TRACE_RECORD` set, the digests are printed as the table
/// at the end of this file instead of being compared with it.
#[test]
fn the_shell_yields_the_models_trace_and_the_recorded_one() {
    let record = std::env::var_os("TST_RECEIVE_TRACE_RECORD").is_some();
    let mut printed = Vec::new();
    for seed in 0..seeds() {
        let case = Case::generate(seed);
        let shell = shell_trace(&case);
        assert_same_trace(seed, &case, &shell, &model_trace(&case));
        let Some(recorded) = RECORDED.get(seed as usize) else {
            continue;
        };
        if record {
            printed.push(format!("0x{:016X},", digest(&shell)));
        } else {
            assert_eq!(
                digest(&shell),
                *recorded,
                "seed {seed} ({}): the trace is not the recorded one",
                case.describe()
            );
        }
    }
    if record {
        println!("const RECORDED: [u64; {}] = [", RECORDED.len());
        for row in printed.chunks(4) {
            println!("    {}", row.join(" "));
        }
        println!("];");
    }
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
        let last = trace.last().expect("a trace has its terminal result");
        nothing_delivered += usize::from(last.2 == 0);
        // A failure that came after events its own flush produced.
        let n = trace.len();
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

/// One digest per seed, the seed being the index.
#[rustfmt::skip]
const RECORDED: [u64; 512] = [
    0x3D28A5EDD55BEC5C, 0x9CD190F83ECFCE12, 0x44669A5E6A5172B8, 0xBBF64AC0778487A2,
    0x7A0CDA7B53462B84, 0x14DC3DC989531C41, 0xC9730BE9E1F75A55, 0x28BDF27C97B28847,
    0x93C7BD9011CC6025, 0xCC67AB9EF4C77E18, 0x5855D28D577D767B, 0x38D234D4BA2A4E0E,
    0x4B55AC83F4B80B3B, 0xD8C6CEBF3410C55A, 0x2BC568E98BBBBEA8, 0xF625301A8460E08F,
    0xEC1ADE68146B5452, 0x3E9EC6E248AC02DF, 0x9CD190F83ECFCE12, 0x428BBB9E519ACBE2,
    0xF352C6FF5210A94A, 0x00D6A654D3E19960, 0x662E9A31231DC125, 0xCC75FCCE1D257312,
    0x5BF726830D643992, 0x12F52EDCC47686B1, 0x91BB97CEE67E4C32, 0xC582C904F3B46B7D,
    0xBCD1E43A6D20DA52, 0xA5496E7188A29B28, 0xD78875073A66E8A3, 0x9CD190F83ECFCE12,
    0x24C3525C870D5BE0, 0x735AEC9ACE55D7B2, 0x91BB97CEE67E4C32, 0x7CD5E13F490696CA,
    0xB865B45FC56DC67A, 0xC439E956119E3C9B, 0x44669A5E6A5172B8, 0x9DA57C509B5CECBD,
    0xA30019489582093F, 0x0C42F685E3BD491C, 0xEFB1BDE71C14BF19, 0xD51797980D8EC5FA,
    0x1115990AF37BB492, 0xF207263D6B540436, 0x9CD190F83ECFCE12, 0x827046BDB275819F,
    0x9B84A14CDEAB40FA, 0x5C804A314652A3E2, 0x9DA205A5BE9DA942, 0x91BB97CEE67E4C32,
    0x1115990AF37BB492, 0x5AC7A15011D17698, 0x496E8BA653C73D58, 0x9CD190F83ECFCE12,
    0x92D83D76086F9ADE, 0x86BDC43915B50596, 0x012EBDB615334DE9, 0xB13C85B15A33B3AC,
    0x9CD190F83ECFCE12, 0x86BDC43915B50596, 0x678B6D93C2F2E49E, 0x86BDC43915B50596,
    0x9CD190F83ECFCE12, 0xF18BEBAC872D82CF, 0x078C0871776B0231, 0xFAE7151BC6F3C794,
    0x5202519E38E68F1C, 0x8BD7E7E858AD312C, 0x77787C3C8F865295, 0xE0EBC172F3341A54,
    0x496E8BA653C73D58, 0xF1A62923BC797E51, 0x91BB97CEE67E4C32, 0x16E1395C8F087A2C,
    0xF5C3A2D80745BCEA, 0xF207263D6B540436, 0x29E740046E2F0E75, 0xBFFFA3950E6975BC,
    0x44669A5E6A5172B8, 0x2EFFAA0E67B5EA48, 0xCC56D106F9BBFD73, 0x91BB97CEE67E4C32,
    0xA1B9240EFF64FBDD, 0xEDD7AF2A810584D8, 0x2665FB1FA251D30B, 0x18B81CED74573F38,
    0x811B3A3408FC5A74, 0xCCA6BE4970B08BE8, 0x44669A5E6A5172B8, 0x86BDC43915B50596,
    0x86BDC43915B50596, 0x1115990AF37BB492, 0xBA5386969DF2B1A6, 0x496E8BA653C73D58,
    0x9CD190F83ECFCE12, 0x4E3A6AD3B5FC09DD, 0xECD5ABA58B4D9520, 0x88FD454886D54A3C,
    0xF207263D6B540436, 0x4295B0ADC06067C0, 0xD2DE8888BD39C452, 0x9C3ACBF89B0EF949,
    0x4983D2E639A0DFD8, 0x7FDC77F7A29D18DA, 0xC2ADC68FDF0F7B2C, 0x4983D2E639A0DFD8,
    0x47C4D212FDE02B8A, 0xC6B29687EADA69F3, 0x65BBE7395926D81F, 0xB9FD51A282A7C498,
    0x98753C130837B763, 0x496E8BA653C73D58, 0x537203DE95386EF3, 0x55E273D2BEF814B5,
    0x7FF930D0E330978C, 0x9CD190F83ECFCE12, 0x4BAC9FA55450EC6B, 0xD195DBFC22C693E6,
    0x91BB97CEE67E4C32, 0x899C7B2D84CB0372, 0x1946697D52981C9F, 0xA549D6B7A009F793,
    0x733CD8E1CF2CE177, 0x86BDC43915B50596, 0x8762A83ABB0C8068, 0x44669A5E6A5172B8,
    0x168C84AD19235F6F, 0x86BDC43915B50596, 0xF3C57D37390F3769, 0x44669A5E6A5172B8,
    0x9AA9130D5F24D87E, 0x9CD190F83ECFCE12, 0x9CD190F83ECFCE12, 0xBBFECAFD7C8E66FD,
    0x8D82910709993160, 0x91BB97CEE67E4C32, 0x23F4AC2138F576DF, 0x91BB97CEE67E4C32,
    0x1946697D52981C9F, 0xEF86990B26D9E671, 0x2678CA5621C30E6A, 0xE16916DC8DE82508,
    0x7D4F26350754B9A6, 0xDC650A8784A4B983, 0xD3C5C182EC91672A, 0xF207263D6B540436,
    0xBE6C9FCE32E2753C, 0x9CD190F83ECFCE12, 0x91BB97CEE67E4C32, 0x202157D2F85D7109,
    0x91BB97CEE67E4C32, 0x92E6F02A3CA0C935, 0x91BB97CEE67E4C32, 0x91BB97CEE67E4C32,
    0xAEADF97115B61361, 0xC31FDBD2CB32E3FC, 0x05312EF67A885523, 0xC6C8939920A45F81,
    0x44669A5E6A5172B8, 0xF42A5E34E2DB3C38, 0x44669A5E6A5172B8, 0x38B13553613842CA,
    0xBC4CDF3D57494B72, 0xEC1ADE68146B5452, 0xD5C968C1B2A1BA44, 0xEF7DFBFDA9E3DFDD,
    0x74169CAA3288F164, 0x9CD190F83ECFCE12, 0x5953227B2CD1FBEC, 0xEA394144C9E9D7E6,
    0x4983D2E639A0DFD8, 0x5CD5FBE7C17BEF9D, 0x65155081D9ABE5D1, 0x8F12C6479A889A60,
    0x3C12D3A31254ECE5, 0xCAC3ED380B9E1964, 0xE0A16E2B7B15F855, 0x496E8BA653C73D58,
    0x20F8F1155A64B303, 0xE86748B67D06D17F, 0x1115990AF37BB492, 0x496E8BA653C73D58,
    0xB55A2AA0E6FADE73, 0x96927F18A77B2841, 0xF3ABC3D54E0C33F9, 0xDF0C9FEF87D00F18,
    0x1115990AF37BB492, 0x88707B004C012716, 0xD033FF1B87F1AE84, 0xADE61F2DC3C3481F,
    0xABB4509D4D385D75, 0x10E0416ECB824B6B, 0xF3E83F1CA4128440, 0x07A6CCFBC8C82A2C,
    0x5C1BFB8AAD102E74, 0xABDA855E7527C49A, 0xAA967104D6B255FF, 0x9CEF13A60C09580C,
    0x6ACB2FDDC15EBE22, 0x98C8DDE93A71FA29, 0xED95E40F4F59395C, 0x91BB97CEE67E4C32,
    0x44669A5E6A5172B8, 0xDD55B72983097D68, 0x9CD190F83ECFCE12, 0x94524069050100DE,
    0xC19FA893263FB326, 0x44669A5E6A5172B8, 0x91BB97CEE67E4C32, 0x9E4A770472CA8D43,
    0x44669A5E6A5172B8, 0x44669A5E6A5172B8, 0x3052D10D97C5E3CD, 0xD9F82D5E07743442,
    0x1115990AF37BB492, 0xBBF1DC94907BDF50, 0x91BB97CEE67E4C32, 0x509290B0C5994475,
    0x0DE114A39F750FF3, 0x64CF82EE86FC5B5E, 0x91BB97CEE67E4C32, 0x6B402F9551D4BED6,
    0x7FFA9A50507165A2, 0x9CD190F83ECFCE12, 0x4AF7D9D89E199835, 0x40C6F839048828B2,
    0x5146A2FEF05F8549, 0x1115990AF37BB492, 0x1A1ADD876A583699, 0x1EB07943F57EA2E6,
    0x91BB97CEE67E4C32, 0xEC1ADE68146B5452, 0x046BDACDEF761921, 0xBCF8187B1D72C9E6,
    0xC2133E2B66F0BA51, 0x8437D6FC1BA0D8BE, 0x9CD190F83ECFCE12, 0xE04E5F0ABB04D255,
    0x91BB97CEE67E4C32, 0x641BBCFB8F93FAED, 0x533BFFE5A85FCB0E, 0x9CD190F83ECFCE12,
    0x4ED7EDB407742213, 0x5C5103AA5E6DE141, 0x30F72EC878D43631, 0x86BDC43915B50596,
    0x328DC2F3124592F1, 0x86BDC43915B50596, 0x862F205D3E565F1D, 0x5FFE6BC34F174B4F,
    0x86BDC43915B50596, 0xA3DAD47EC1816764, 0x91BB97CEE67E4C32, 0x4F288BDAC2685F9E,
    0x966659DCDD6B7332, 0x9CD190F83ECFCE12, 0x9CD190F83ECFCE12, 0x187FC2FD9F2BE06E,
    0x1115990AF37BB492, 0x44669A5E6A5172B8, 0xE0EBE45990E5F9A6, 0x8759157E7B2B5962,
    0x95A7C1B1A3401916, 0x91BB97CEE67E4C32, 0x44669A5E6A5172B8, 0xF207263D6B540436,
    0xB498FBD4989FBACB, 0x4983D2E639A0DFD8, 0x57B6C54B74F6FD07, 0x980FC003607855D1,
    0x1B65922EE48287F7, 0x47E91DD254C51B72, 0x9CD190F83ECFCE12, 0x44669A5E6A5172B8,
    0xEA1F9B669943C081, 0x53663786506FA5E1, 0x496E8BA653C73D58, 0x13AE5FF0860A5313,
    0xB0C2C85465A94095, 0x32D69EDBBB41BCF6, 0x78F096B155DC5E63, 0x5F3EED2DA7C480FB,
    0x9CD190F83ECFCE12, 0x9CD190F83ECFCE12, 0xD05FCFA06474287A, 0x3BC92D99A2EE1DFA,
    0x6F3BF711B78097FB, 0x4BE124F664A56062, 0xEB32A0B33805FA3D, 0x4E3F3FEDC7FCB7E7,
    0xB32120310531BF69, 0x1115990AF37BB492, 0x91BB97CEE67E4C32, 0x9CD190F83ECFCE12,
    0xF207263D6B540436, 0xF207263D6B540436, 0x756F185180E85860, 0x91BB97CEE67E4C32,
    0x44669A5E6A5172B8, 0xF207263D6B540436, 0x058D402770CFAF28, 0x8545C89D9582B10D,
    0xD8AD440D1C64FE3F, 0x9CD190F83ECFCE12, 0x91BB97CEE67E4C32, 0x05E5585A16447B66,
    0x078C0871776B0231, 0xA13FD3AB65A175C9, 0xD62549A90860F36B, 0x694141067B35EEE5,
    0xFC051937B77C620A, 0x19272763B3642831, 0x0E319DD4872A097F, 0x583774A7600840BC,
    0x86BDC43915B50596, 0xDC145D383630E374, 0xE3D57EE92371D880, 0x194382D460B49B4D,
    0xD9A1A9634D86EEF8, 0xADDF85150EBEA047, 0x1946697D52981C9F, 0xC99B7E4879D967C6,
    0xB7419DF10571416F, 0x1828C87F66728969, 0x04CBBB2DF5263ACB, 0x912A5B10C9DC8BC0,
    0x2C75D515A38F5329, 0x4E5F0747BEB39D11, 0x496E8BA653C73D58, 0x2CAA82C48197947C,
    0xBE78F0CA90B5EA79, 0x56E91FFDE37192F3, 0x1ECC01DFE83FC4B4, 0xB6BA17817EDBFE9A,
    0x4983D2E639A0DFD8, 0x44669A5E6A5172B8, 0x0813C9DB4DF2A2E6, 0xEF6D272DF33F65E6,
    0x1115990AF37BB492, 0x28DF99B708046CAB, 0x9CD190F83ECFCE12, 0x2E81CFC2BA6F8686,
    0xA8444C3975258516, 0x7D1A5AE5D9957ED0, 0x0ED864DACF0B2CC4, 0x690128FC6A2390FB,
    0xE65AE1A08C563441, 0x4E91AC3CE73AB95E, 0x31BBB83138D573B0, 0x8A72DCEF70DC7555,
    0x278D204C8E30FE9F, 0xE7D79E6889461F60, 0xF424A29CC7E378D6, 0x3B74960E6DDD9227,
    0x496E8BA653C73D58, 0x078C0871776B0231, 0xC52E6299E7F139EC, 0x32CC82571A952D06,
    0x71353D6981FB2AF2, 0x4D96B2F837B5500B, 0xC70128042552280A, 0x00ADA4B20D0FB6AE,
    0xDB3C9843EEDCE3AB, 0x496E8BA653C73D58, 0x23E9420A6EEF8EE7, 0x496E8BA653C73D58,
    0x86E7220D9E273B18, 0xB284C064D6708121, 0x078C0871776B0231, 0x1115990AF37BB492,
    0x0D7C2F773BA6B8EF, 0x44669A5E6A5172B8, 0x719C1A69D0375EA1, 0x91BB97CEE67E4C32,
    0x91BB97CEE67E4C32, 0x9CD190F83ECFCE12, 0x91BB97CEE67E4C32, 0xF7AB9F76BA01671E,
    0xFDDD13372C3E86F3, 0xF050DF03939BE1C7, 0x72E1B5F36DABD26D, 0x4983D2E639A0DFD8,
    0xF207263D6B540436, 0x91BB97CEE67E4C32, 0x548D84A9C21CB897, 0xE4ECA2F0F3330717,
    0xD24006E0D0FECEA4, 0x91BB97CEE67E4C32, 0x616376C649A54790, 0x078C0871776B0231,
    0x758BE1D2C24254F0, 0x5FAF70FC46E26550, 0x46C4D572918CAFA2, 0x33ED3735450AE3EE,
    0xC3D6A3EA7F47FE23, 0x23000BBC357C23CE, 0x1EC901C6C07994BE, 0x0E5E7786C53EF7B1,
    0x91BB97CEE67E4C32, 0xFC612B7E23C94FA8, 0x09AA53C4C9B5209F, 0x496E8BA653C73D58,
    0x1115990AF37BB492, 0x44669A5E6A5172B8, 0xFFD2AF7F509E113D, 0x9976A23FDACBE62D,
    0x569F6350D4EAFA25, 0x44669A5E6A5172B8, 0x9CD190F83ECFCE12, 0x1ABBF3B57B246794,
    0xCFB79FDA17941701, 0x4ED289ED19CDD154, 0x9B8EA8B030E0CA5D, 0x1115990AF37BB492,
    0x9CD190F83ECFCE12, 0xF207263D6B540436, 0xD21960FC111A6B11, 0x496E8BA653C73D58,
    0xEBEE04C0E4B81057, 0x0147E4611E398A1C, 0x91BB97CEE67E4C32, 0x4983D2E639A0DFD8,
    0x4909AFC31CEFC6D1, 0x44669A5E6A5172B8, 0x1946697D52981C9F, 0x91BB97CEE67E4C32,
    0x95A7C1B1A3401916, 0x226D82279FA23D70, 0x91BB97CEE67E4C32, 0xFD60AFF52DFECACB,
    0xFDDD2864E2B03F9E, 0xAD885D6838F65C46, 0x496E8BA653C73D58, 0x60D3655D55ECCF47,
    0x91BB97CEE67E4C32, 0xAA8EE8658897CAE2, 0x1E0FE2951A404C49, 0x3BD2338C62BA0E69,
    0x2E97EE8CF7CC7EC2, 0x73263F0B3A4A0631, 0xBCD05FEB8F8801FA, 0x3E0C458D788C12CA,
    0x29E0A090814AD0CA, 0x329299296765A893, 0x47FEDFC259EFB231, 0x39F8F06CD3FA409C,
    0x44669A5E6A5172B8, 0x86BDC43915B50596, 0x9CD190F83ECFCE12, 0x66A7953F3B78C771,
    0x91BB97CEE67E4C32, 0x8AC9BF44C0FE90C3, 0x8FC2CEBDC9DD451C, 0x496E8BA653C73D58,
    0x91BB97CEE67E4C32, 0x2CF000B323E46975, 0x883A2612FBBF8355, 0x27725572E8A45253,
    0x9CD190F83ECFCE12, 0xB18252EAD299B5C3, 0x91BB97CEE67E4C32, 0x8C11DD42BEF906FF,
    0x496E8BA653C73D58, 0x868423055BF22190, 0x9CD190F83ECFCE12, 0xFB65C616CFAA7545,
    0x9CD190F83ECFCE12, 0x1EE29EEDB781F405, 0x4983D2E639A0DFD8, 0x9CD190F83ECFCE12,
    0x496E8BA653C73D58, 0xF207263D6B540436, 0x91BB97CEE67E4C32, 0x81C98EFE9BDF52C5,
    0x2F0DBAFF26A73CC2, 0x1C13695F47D2E951, 0xA8822F8FBB8940E4, 0x94CEF0ACF075C394,
    0x9CD190F83ECFCE12, 0x8286CAFEB3C3E875, 0xD356E84BF564692C, 0x735AEC9ACE55D7B2,
    0x4A89323880994D7B, 0x86BDC43915B50596, 0x9377D01E5DB1EECB, 0x9CD190F83ECFCE12,
    0x1115990AF37BB492, 0x5CD109CD131E429E, 0x3B3204EF033CD5B8, 0xAA0C1FAE31A5EFEC,
    0x171A217CA1E891D2, 0x44669A5E6A5172B8, 0x5F6440DC1A2AB325, 0xE07397DFBE4CA109,
    0x980FC003607855D1, 0x9CD190F83ECFCE12, 0xC761B95EA18E3A2E, 0x496E8BA653C73D58,
    0x9CD190F83ECFCE12, 0x3B252DE561060D89, 0x9CD190F83ECFCE12, 0x9CD190F83ECFCE12,
    0x28A2C35923F721A2, 0x5FFE6BC34F174B4F, 0x7A12C1EDCDF295B7, 0x527CDE026EF8BCC2,
    0x91BB97CEE67E4C32, 0x735AEC9ACE55D7B2, 0x686A74465CA67CA4, 0x078C0871776B0231,
];
