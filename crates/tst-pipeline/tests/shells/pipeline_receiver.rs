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
