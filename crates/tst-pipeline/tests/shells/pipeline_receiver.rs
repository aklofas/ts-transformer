//! Pipeline integration: drive `DemuxReceiver` with an in-memory `ScriptedRecv`.
//!
//! These tests verify the composition layer rather than the mux or demux
//! internals in isolation. `ScriptedRecv` replays pre-muxed TS bytes
//! as if they arrived over a live SRT connection.

use crate::common::ScriptedRecv;

use std::collections::VecDeque;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::DemuxEvent;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};
use tst_core::transport::RecvTransport;
use tst_core::transport::TransportError;
use tst_pipeline::DemuxReceiver;

// ---------------------------------------------------------------------------
// Helper: mux one H.264 AU and drain all output chunks.
// ---------------------------------------------------------------------------

/// Build a minimal H.264 Annex-B AU: AUD (nal_type=9) byte.
fn minimal_h264_au() -> Vec<u8> {
    vec![0x00, 0x00, 0x00, 0x01, 0x09, 0x10]
}

/// Build a minimal KLV blob with a valid SMPTE UL prefix so the demuxer
/// recognizes it as ST 0601 async KLV. Matches the pattern used in
/// `mpegts_demux.rs`.
fn minimal_klv() -> Vec<u8> {
    // 16-byte SMPTE UL + BER-short length + minimal body (tag 2, 8 zero bytes).
    let body: &[u8] = &[2u8, 8, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut out = Vec::with_capacity(17 + body.len());
    out.extend_from_slice(&[
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00,
    ]);
    out.push(body.len() as u8);
    out.extend_from_slice(body);
    out
}

/// Drain all output from a `Muxer` into a queue of chunks, each at most
/// `chunk_size` bytes, and return both the queue and the total byte count.
fn drain_to_chunks(mux: &mut Muxer, chunk_size: usize) -> (VecDeque<Vec<u8>>, usize) {
    let mut chunks = VecDeque::new();
    let mut buf = vec![0u8; chunk_size];
    let mut total = 0;
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        total += n;
        chunks.push_back(buf[..n].to_vec());
    }
    (chunks, total)
}

/// Build a muxer preloaded with enough frames to guarantee the Receiver sync
/// state machine can lock.
///
/// The Receiver `Syncer` requires 4 back-to-back 0x47 sync bytes spaced
/// 188 bytes apart before it will emit packets — that means it needs at least
/// 5 * 188 + 1 = 941 bytes of contiguous TS data in its internal buffer before
/// transitioning from VERIFY to LOCKED.
///
/// Strategy: use `psi_interval_ms(10)` (the minimum allowed, 900 ticks at
/// 90 kHz) so PAT + PMT are re-emitted on every push. Then push 3 video + KLV
/// frames. Each PSI interval boundary emits 2 more packets, so after 3 pushes
/// we get PAT+PMT (×3) + 3 video + 3 KLV ≥ 9 packets = 1692 bytes — well
/// above the 941-byte lock threshold. A single 1316-byte pull chunk then
/// contains ≥ 7 packets (or two pulls totalling ≥ 9 packets), letting the
/// syncer lock and emit events.
fn build_and_preload_muxer() -> Muxer {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, MuxVideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.psi_interval_ms(10);
        b.build().unwrap()
    };
    let mut m = Muxer::new(cfg).unwrap();
    let au = minimal_h264_au();
    let klv = minimal_klv();
    // Push 3 frames at PTS 0 / 9001 / 18002 (each ≥ 9000 ticks = 10 ms apart
    // at 90 kHz) so the PSI re-emission threshold is crossed on every push.
    for i in 0i64..3 {
        let pts = i * 9001;
        m.push_video(&au, Pts90khz::new(pts), true).unwrap();
        m.push_klv(&klv, Pts90khz::new(pts), 0x00).unwrap();
    }
    m
}

// ---------------------------------------------------------------------------
// Test 1: DemuxReceiver emits at least a ProgramMap event for a well-formed stream.
// ---------------------------------------------------------------------------

/// Mux several H.264 frames, replay them through `ScriptedRecv`, and confirm
/// the `DemuxReceiver` emits a `ProgramMap` event. This exercises the full
/// composition path: transport → Receiver sync → Demuxer → event queue.
///
/// Multiple frames are required because the `Receiver` sync state machine
/// needs at least 941 bytes of contiguous TS data before it will lock and emit
/// packets. See `build_and_preload_muxer` for the exact strategy.
#[test]
fn receiver_emits_events_through_canned_transport() {
    let mut m = build_and_preload_muxer();

    let (chunks, _) = drain_to_chunks(&mut m, 1316);

    let mut rx = DemuxReceiver::new(ScriptedRecv::new(chunks, TransportError::Closed));
    let mut saw_pmap = false;
    for item in &mut rx {
        let e = item.unwrap();
        if matches!(e, DemuxEvent::ProgramMap(_)) {
            saw_pmap = true;
        }
    }
    assert!(saw_pmap, "expected at least one ProgramMap event");
}

// ---------------------------------------------------------------------------
// Test 2: the counter covers every aligned packet; Receiver + Demuxer composes.
// ---------------------------------------------------------------------------

/// `ts_bytes_demuxed` counts every aligned TS packet the shell hands to its
/// demuxer. On a clean stream the syncer's confirmation is peek-only (see
/// the `Receiver` "Sync lock and the trailing window" rule), so nothing is
/// withheld and the count equals the muxer output exactly, and equals the
/// receiver's own `bytes_received`.
#[test]
fn ts_bytes_demuxed_counts_every_aligned_packet() {
    let mut m = build_and_preload_muxer();

    let (chunks, total) = drain_to_chunks(&mut m, 1316);

    let mut rx = DemuxReceiver::new(ScriptedRecv::new(chunks, TransportError::Closed));

    // ScriptedRecv here only emits Closed (never Broken), so any Err would be
    // a strict-mode rejection, none of which are expected on a clean stream.
    while rx.next().is_some() {}

    assert_eq!(rx.ts_bytes_demuxed(), total as u64);
    assert_eq!(rx.ts_bytes_demuxed(), rx.stats().bytes_received);
}

/// `(variant name, pid, pts)` per event: the comparison key of the trace test.
fn event_key(e: &DemuxEvent) -> (String, Option<u16>, Option<i64>) {
    let dbg = format!("{e:?}");
    let name = dbg
        .split(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap_or_default()
        .to_string();
    match e {
        DemuxEvent::Sample { stream, pts, .. } | DemuxEvent::Metadata { stream, pts, .. } => {
            (name, Some(stream.pid), Some(pts.as_ticks()))
        }
        DemuxEvent::Discontinuity { stream, .. } | DemuxEvent::NonConformant { stream, .. } => {
            (name, Some(stream.pid), None)
        }
        _ => (name, None, None),
    }
}

/// The documented composition (`Receiver` for the raw packets, a `Demuxer`
/// fed with each) observes exactly what `DemuxReceiver` observes: the packets
/// are whole, aligned, concatenate to the muxer output byte for byte, and the
/// events are the same sequence.
#[test]
fn receiver_demuxer_composition_is_an_exact_trace() {
    use tst_core::mpegts::demux::{Demuxer, DemuxerConfig};
    use tst_pipeline::{Receiver, ReceiverConfig};

    let mut m = build_and_preload_muxer();
    let (chunks, total) = drain_to_chunks(&mut m, 1316);
    let written: Vec<u8> = chunks.iter().flatten().copied().collect();
    assert_eq!(written.len(), total);

    // Reference: the shell's own event sequence.
    let mut shell = DemuxReceiver::new(ScriptedRecv::new(chunks.clone(), TransportError::Closed));
    let expected: Vec<_> = (&mut shell).map(|i| event_key(&i.unwrap())).collect();
    assert!(!expected.is_empty());

    // Composition.
    let mut rx = Receiver::new(
        ScriptedRecv::new(chunks, TransportError::Closed),
        ReceiverConfig::default(),
    );
    let mut demux = Demuxer::with_config(DemuxerConfig::default());
    let mut bytes = Vec::new();
    let mut got = Vec::new();
    while let Ok(pkt) = rx.next_packet() {
        assert_eq!(pkt.len(), 188);
        assert_eq!(pkt[0], 0x47);
        bytes.extend_from_slice(&pkt);
        demux.feed_aligned(&pkt).unwrap();
        while let Some(e) = demux.next_event() {
            got.push(event_key(&e));
        }
    }
    demux.flush();
    while let Some(e) = demux.next_event() {
        got.push(event_key(&e));
    }

    assert_eq!(bytes, written);
    assert_eq!(got, expected);
}

// ---------------------------------------------------------------------------
// Test 3: ManagedRecvTransport invokes the factory on broken inner.
// ---------------------------------------------------------------------------

/// Confirms that `ManagedRecvTransport` rebuilds via the supplied factory
/// when its inner transport breaks. The initial inner is constructed empty
/// (so the very first `recv_bytes` returns `Closed`), driving the decorator
/// straight into the reconnect path; the factory then supplies a fresh
/// transport with one well-formed chunk that the next `recv_bytes` returns.
#[test]
fn managed_receive_reconnects_through_factory() {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tst_pipeline::ManagedRecvTransport;
    use tst_pipeline::reconnect::{BackoffStrategy, ReconnectPolicy};

    let attempts = Arc::new(Mutex::new(0u32));
    let attempts_cl = attempts.clone();
    let factory = Box::new(move || {
        *attempts_cl.lock().unwrap() += 1;
        Ok(ScriptedRecv::new(
            VecDeque::from(vec![vec![0x47; 188]]),
            TransportError::Closed,
        ))
    });

    // Initial transport is empty — first recv_bytes gets Closed and the
    // decorator falls into the reconnect path.
    let initial = ScriptedRecv::new(VecDeque::new(), TransportError::Closed);

    // Zero-delay constant backoff so the test doesn't actually sleep.
    let policy = ReconnectPolicy::default()
        .with_max_attempts(Some(3))
        .with_backoff(BackoffStrategy::Constant(Duration::from_millis(0)));

    let mut managed =
        ManagedRecvTransport::new(initial, factory, policy).expect("the policy validates");

    let mut buf = [0u8; 188];
    let n = managed.recv_bytes(&mut buf).unwrap();
    assert_eq!(n, 188);
    assert_eq!(buf[0], 0x47);
    assert!(*attempts.lock().unwrap() >= 1);
}

// ---------------------------------------------------------------------------
// The counter is readable while a `recv_event` is parked.
// ---------------------------------------------------------------------------

/// Serves its chunks, then blocks until the shared flag is set (by the
/// cancel handle) and reports `Closed`.
struct ParkingRecv {
    chunks: VecDeque<Vec<u8>>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl RecvTransport for ParkingRecv {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if let Some(v) = self.chunks.pop_front() {
            let n = v.len().min(buf.len());
            buf[..n].copy_from_slice(&v[..n]);
            return Ok(n);
        }
        while !self.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Err(TransportError::Closed)
    }

    fn max_payload(&self) -> usize {
        1316
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn close(&mut self) {}

    fn cancel_handle(
        &self,
    ) -> Option<std::sync::Arc<dyn tst_core::transport::TransportCancel + Send + Sync>> {
        Some(std::sync::Arc::new(crate::common::FlagCancel(
            self.cancelled.clone(),
        )))
    }
}

/// A reader of the counter never takes the shell slot: it finishes while
/// another thread is parked inside `recv_event`.
#[test]
fn ts_bytes_demuxed_reads_while_a_recv_event_is_parked() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    let mut m = build_and_preload_muxer();
    let (chunks, _) = drain_to_chunks(&mut m, 1316);
    let all: Vec<u8> = chunks.into_iter().flatten().collect();
    // The syncer locks on 4 aligned packets, then releases every packet it
    // verified, so five served packets are all counted.
    let served = all[..188 * 5].to_vec();

    let cancelled = Arc::new(AtomicBool::new(false));
    let rx = DemuxReceiver::new(ParkingRecv {
        chunks: VecDeque::from([served]),
        cancelled: cancelled.clone(),
    });
    let handle = rx.ts_bytes_demuxed_handle();
    let cancel: Arc<dyn tst_core::transport::TransportCancel> =
        Arc::new(crate::common::FlagCancel(cancelled));
    let owned = Arc::new(tst_pipeline::binding::Owned::new(
        rx,
        cancel,
        handle.clone(),
    ));

    let o = owned.clone();
    let a = std::thread::spawn(move || {
        let _ = o.with_mut(|rx| while rx.recv_event().is_ok_and(|e| e.is_some()) {});
    });

    assert!(
        crate::common::wait_for(Duration::from_secs(10), || owned.snapshot().get()
            == 5 * 188),
        "counter = {}",
        owned.snapshot().get()
    );
    assert!(!a.is_finished(), "thread A must still be parked");

    let o = owned.clone();
    let reader = std::thread::spawn(move || o.snapshot().get());
    assert!(crate::common::wait_for(Duration::from_secs(10), || reader.is_finished()));
    assert_eq!(reader.join().unwrap(), 5 * 188);

    owned.cancel();
    a.join().unwrap();
}

// ---------------------------------------------------------------------------
// The end of a stream: the whole packets still held
// ---------------------------------------------------------------------------

/// Answers in order, one per read, then `end` for ever.
struct Answers {
    answers: VecDeque<Result<Vec<u8>, TransportError>>,
    end: TransportError,
}

impl Answers {
    fn new(
        answers: impl IntoIterator<Item = Result<Vec<u8>, TransportError>>,
        end: TransportError,
    ) -> Self {
        Self {
            answers: answers.into_iter().collect(),
            end,
        }
    }
}

impl RecvTransport for Answers {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        match self.answers.pop_front() {
            Some(Ok(bytes)) => {
                buf[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            Some(Err(e)) => Err(e),
            None => Err(self.end.clone()),
        }
    }

    fn max_payload(&self) -> usize {
        4096
    }

    fn is_alive(&self) -> bool {
        true
    }
}

fn backpressure() -> TransportError {
    TransportError::Backpressure {
        msg: "nothing yet".into(),
        errno_code: None,
    }
}

/// One access unit per marker, each a single TS packet behind its own PAT
/// and PMT: three packets a unit. The unit's payload is 20 copies of the
/// marker.
fn short_units(markers: &[u8]) -> Vec<u8> {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, MuxVideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.psi_interval_ms(10);
    let mut m = Muxer::new(b.build().unwrap()).unwrap();
    let mut out = Vec::new();
    for (i, &marker) in markers.iter().enumerate() {
        let mut au = vec![0x00, 0x00, 0x00, 0x01, 0x65];
        au.resize(25, marker);
        m.push_video(&au, Pts90khz::new(i as i64 * 3003), true)
            .unwrap();
        out.extend(crate::common::drain_mux(&mut m));
    }
    assert_eq!(out.len(), markers.len() * 3 * 188, "three packets a unit");
    out
}

/// What a `DemuxReceiver` yields until its first terminal result: a sample
/// as the marker byte that fills it, any other event by its variant, a
/// `Backpressure` and the end by name.
fn outcomes<R: RecvTransport>(rx: &mut DemuxReceiver<R>) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        match rx.recv_event() {
            Ok(Some(DemuxEvent::Sample {
                payload: tst_core::mpegts::demux::SamplePayload::Video { raw, .. },
                ..
            })) => out.push(format!("sample {:02X}", raw[raw.len() - 1])),
            Ok(Some(e)) => out.push(event_key(&e).0),
            Ok(None) => {
                out.push("end".into());
                return out;
            }
            Err(e) if e.kind == tst_pipeline::ShellErrorKind::Backpressure => {
                out.push("backpressure".into());
            }
            Err(e) => {
                out.push(format!("{:?}", e.kind));
                return out;
            }
        }
        assert!(out.len() < 1000, "the receiver never ended");
    }
}

/// A stream of one, two or three packets reaches the caller: the syncer's
/// four-packet rule has nothing more to wait for once the transport has
/// ended, and every packet boundary present reads the sync byte.
#[test]
fn a_one_packet_stream_arrives() {
    use tst_pipeline::{Receiver, ReceiverConfig, ShellErrorKind};

    let stream = short_units(&[0xA1]);
    for n in 1..=3 {
        let sent = &stream[..n * 188];
        let mut rx = Receiver::new(
            ScriptedRecv::new([sent.to_vec()], TransportError::Closed),
            ReceiverConfig::default(),
        );
        let mut got = Vec::new();
        let end = loop {
            match rx.next_packet() {
                Ok(p) => got.extend_from_slice(&p),
                Err(e) => break e,
            }
        };
        assert_eq!(got, sent, "{n} packets");
        assert_eq!(end.kind, ShellErrorKind::EndOfStream);
        let st = rx.stats();
        assert_eq!(
            (st.packets_received, st.bytes_received, st.resync_events),
            (n as u64, n as u64 * 188, 1)
        );
    }

    // Through the demuxer: its table and its one access unit.
    let mut rx = DemuxReceiver::new(ScriptedRecv::new([stream], TransportError::Closed));
    assert_eq!(outcomes(&mut rx), ["ProgramMap", "sample A1", "end"]);
    assert_eq!(
        (rx.ts_bytes_demuxed(), rx.stats().packets_received),
        (3 * 188, 3)
    );
}

/// After damage the syncer needs four packets again. A stream that ends two
/// or three packets after the damage used to lose them.
#[test]
fn the_tail_after_a_resync_arrives() {
    let stream = short_units(&[0xA1, 0xB2, 0xC3]);
    // Six packets lock the syncer; then bytes that are not TS; then unit C.
    let mut wire = stream[..6 * 188].to_vec();
    wire.extend_from_slice(&[0xAA; 150]);
    wire.extend_from_slice(&stream[6 * 188..]);
    let reads: Vec<Vec<u8>> = wire.chunks(1316).map(<[u8]>::to_vec).collect();

    let mut rx = DemuxReceiver::new(ScriptedRecv::new(reads, TransportError::Closed));
    assert_eq!(
        outcomes(&mut rx),
        ["ProgramMap", "sample A1", "sample B2", "sample C3", "end"]
    );
    assert_eq!(rx.stats().packets_received, 9);
}

/// `Backpressure` with three packets held is not an end: nothing is
/// released and nothing flushed, and the bytes that follow continue the
/// same wait for a fourth packet.
#[test]
fn backpressure_releases_nothing_and_flushes_nothing() {
    let stream = short_units(&[0xA1, 0xB2]);
    let answers = [
        Ok(stream[..3 * 188].to_vec()),
        Err(backpressure()),
        Ok(stream[3 * 188..].to_vec()),
    ];
    let mut rx = DemuxReceiver::new(Answers::new(answers, TransportError::Closed));

    let e = rx.recv_event().expect_err("the pause is handed on");
    assert_eq!(e.kind, tst_pipeline::ShellErrorKind::Backpressure);
    assert_eq!(
        (rx.ts_bytes_demuxed(), rx.stats().packets_received),
        (0, 0),
        "three packets are still waiting for their fourth"
    );
    assert_eq!(
        outcomes(&mut rx),
        ["ProgramMap", "sample A1", "sample B2", "end"]
    );
    assert_eq!(rx.stats().packets_received, 6);
}

/// A failure or a cancel releases the held packets like a clean end does,
/// and is reported after them.
#[test]
fn the_error_comes_after_the_released_packets() {
    use tst_pipeline::{Receiver, ReceiverConfig, ShellErrorKind};

    let stream = short_units(&[0xA1]);
    let mut rx = Receiver::new(
        ScriptedRecv::new([stream[..2 * 188].to_vec()], crate::common::broken("gone")),
        ReceiverConfig::default(),
    );
    assert_eq!(rx.next_packet().unwrap()[..], stream[..188]);
    assert_eq!(rx.next_packet().unwrap()[..], stream[188..376]);
    let e = rx.next_packet().expect_err("then the failure");
    assert_eq!(e.kind, ShellErrorKind::TransportBroken);
    assert!(e.to_string().contains("gone"), "{e}");

    for (end, kind) in [
        (crate::common::broken("gone"), "TransportBroken"),
        (TransportError::ExplicitClose, "Closed"),
    ] {
        let mut rx = DemuxReceiver::new(ScriptedRecv::new([stream.clone()], end));
        assert_eq!(outcomes(&mut rx), ["ProgramMap", "sample A1", kind]);
    }
}

/// The one boundary an end can still refuse on is the first byte of a
/// trailing partial packet. When it is not a sync byte the whole packets
/// before it are not delivered: before the first lock, and after a resync.
#[test]
fn a_junk_tail_with_a_failing_stride_is_not_delivered() {
    use tst_pipeline::{Receiver, ReceiverConfig};

    let mut junk = vec![0x5A; 188 + 60];
    junk[0] = 0x47;
    let good = short_units(&[0xA1, 0xB2]);
    let mut after_a_resync = good.clone();
    after_a_resync.extend_from_slice(&[0xAA; 150]);
    after_a_resync.extend_from_slice(&junk);

    for (wire, delivered) in [(junk.clone(), 0), (after_a_resync, 6)] {
        let reads: Vec<Vec<u8>> = wire.chunks(1316).map(<[u8]>::to_vec).collect();
        let mut rx = Receiver::new(
            ScriptedRecv::new(reads, TransportError::Closed),
            ReceiverConfig::default(),
        );
        while rx.next_packet().is_ok() {}
        assert_eq!(rx.stats().packets_received, delivered);
    }
}

/// Exactly 188 bytes that are not TS but start with the sync byte, then the
/// end: before the first lock nothing can refute them, so one packet reaches
/// the demuxer, which reports what it cannot use and makes no sample of it.
#[test]
fn a_junk_packet_released_at_the_end_is_a_nonconformant_event_or_nothing() {
    for second_byte in [0x5A, 0xDA] {
        let mut junk = vec![0x5A; 188];
        junk[0] = 0x47;
        // 0xDA sets the transport error indicator.
        junk[1] = second_byte;
        let mut rx = DemuxReceiver::new(ScriptedRecv::new([junk], TransportError::Closed));
        let seen = outcomes(&mut rx);
        assert_eq!(
            (rx.stats().packets_received, rx.ts_bytes_demuxed()),
            (1, 188),
            "{seen:?}"
        );
        assert_eq!(seen.last().map(String::as_str), Some("end"));
        assert!(
            seen[..seen.len() - 1].iter().all(|o| o == "NonConformant"),
            "{seen:?}"
        );
    }
}

/// A strict demuxer rejects a packet the end released: the rejection is
/// reported once, the packet stays counted, and the end still comes.
#[test]
fn strict_mode_reports_a_rejected_tail_packet_then_the_end() {
    use tst_core::mpegts::demux::{DemuxerConfig, StrictMode};

    let mut packet = short_units(&[0xA1])[2 * 188..].to_vec();
    packet[1] |= 0x80;
    let mut cfg = DemuxerConfig::default();
    cfg.strict = StrictMode::Full;
    let mut rx =
        DemuxReceiver::with_demux_options(ScriptedRecv::new([packet], TransportError::Closed), cfg);

    let e = rx.recv_event().expect_err("the tail packet is rejected");
    assert_eq!(e.kind, tst_pipeline::ShellErrorKind::InputMalformed);
    assert_eq!(
        (rx.stats().packets_received, rx.ts_bytes_demuxed()),
        (1, 188)
    );
    let rest = outcomes(&mut rx);
    assert_eq!(rest.last().map(String::as_str), Some("end"), "{rest:?}");
    assert!(
        rest[..rest.len() - 1].iter().all(|o| o == "NonConformant"),
        "{rest:?}"
    );
}

/// `reset_sync` between two released packets drops the ones not yet handed
/// out and keeps the end they came with: the next call reports it without
/// another read.
#[test]
fn reset_sync_drops_the_released_packets_and_keeps_the_end() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tst_pipeline::{Receiver, ReceiverConfig, ShellErrorKind};

    let reads = Arc::new(AtomicU32::new(0));
    let stream = short_units(&[0xA1]);
    let mut rx = Receiver::new(
        ScriptedRecv::new([stream], crate::common::broken("gone")).with_counter(reads.clone()),
        ReceiverConfig::default(),
    );
    rx.next_packet()
        .expect("the first of three released packets");
    let reads_at_the_end = reads.load(Ordering::SeqCst);
    rx.reset_sync();
    let e = rx.next_packet().expect_err("the other two are dropped");
    assert_eq!(e.kind, ShellErrorKind::TransportBroken);
    assert_eq!(reads.load(Ordering::SeqCst), reads_at_the_end);
    assert_eq!(rx.stats().packets_received, 1);
    // The receiver has no latch of its own: asked again, it reads again.
    assert!(rx.next_packet().is_err());
    assert_eq!(reads.load(Ordering::SeqCst), reads_at_the_end + 1);
}
