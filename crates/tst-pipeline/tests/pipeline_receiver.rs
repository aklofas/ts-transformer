//! Pipeline integration: drive `DemuxReceiver` with an in-memory `ScriptedRecv`.
//!
//! These tests verify the composition layer rather than the mux or demux
//! internals in isolation. `ScriptedRecv` replays pre-muxed TS bytes
//! as if they arrived over a live SRT connection.

mod common;

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
// Test 2: Byte sinks see every TS byte pulled from the transport.
// ---------------------------------------------------------------------------

/// Confirms that `add_byte_sink` receives every TS packet byte before the
/// demuxer processes it. The total bytes seen by the sink must equal the total
/// bytes produced by the muxer (within ±188 bytes to account for the
/// Receiver syncer's internal packet alignment buffer, which may withhold
/// up to one packet until sync is confirmed).
///
/// The ±188 window is intentionally loose: `Receiver` may buffer one packet
/// internally during VERIFY-phase sync before emitting it, so the sink can
/// see up to one packet fewer than `total`. The upper bound guards against
/// double-delivery bugs.
#[test]
fn byte_sinks_see_every_chunk() {
    use std::sync::{Arc, Mutex};

    let mut m = build_and_preload_muxer();

    let (chunks, total) = drain_to_chunks(&mut m, 1316);

    let mut rx = DemuxReceiver::new(ScriptedRecv::new(chunks, TransportError::Closed));

    // Shared counter: the sink accumulates bytes across all packets.
    let captured = Arc::new(Mutex::new(0usize));
    let cap = captured.clone();
    rx.add_byte_sink(Box::new(move |b| {
        *cap.lock().unwrap() += b.len();
    }));

    // Drive the receiver to EOF. ScriptedRecv here only emits Closed (never
    // Broken), so any Err here would be a Demux strict-mode rejection — none
    // are expected on a clean stream. The `is_some` discard pattern is safe
    // *here*; production code should match each `Item` and surface errors.
    while rx.next().is_some() {}

    let saw = *captured.lock().unwrap();
    assert!(
        saw >= total.saturating_sub(188) && saw <= total + 188,
        "byte sink saw {saw} bytes; expected {total} ± 188"
    );
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
    let policy = ReconnectPolicy {
        max_attempts: Some(3),
        backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
        ..Default::default()
    };

    let mut managed = ManagedRecvTransport::new(initial, factory, policy);

    let mut buf = [0u8; 188];
    let n = managed.recv_bytes(&mut buf).unwrap();
    assert_eq!(n, 188);
    assert_eq!(buf[0], 0x47);
    assert!(*attempts.lock().unwrap() >= 1);
}
