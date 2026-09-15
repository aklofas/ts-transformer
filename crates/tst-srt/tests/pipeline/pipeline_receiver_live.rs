//! Live-socket end-to-end roundtrip — `pipeline::MuxSender` → real SRT loopback
//! → `pipeline::DemuxReceiver` in one process.
//!
//! Why a separate test from `pipeline_receiver.rs` (canned-transport) and
//! `pipeline_sender.rs` (live socket, raw byte counter)?
//!
//! - `pipeline_receiver.rs` exercises the receiver composition with
//!   `CannedTransport`, which replays in-memory chunks and never goes near
//!   the SRT handshake or the wire.
//! - `pipeline_sender.rs` exercises the sender composition over a real
//!   `srt::Listener` ↔ `srt::Socket` pair, but the receive side just counts
//!   raw bytes — it never demuxes.
//!
//! This test wires the two halves together. It validates that the sender
//! pipeline's wire format survives a full SRT handshake, transit, and
//! reassembly, and that the receiver pipeline produces semantically
//! correct events on the other side.
//!
//! Linux x86_64 only — same gate as the existing live-socket tests.

#![cfg(target_os = "linux")]

use std::thread;
use std::time::Duration;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::DemuxEvent;
use tst_core::mpegts::mux::{
    KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};
use tst_pipeline::{DemuxReceiver, DemuxReceiverErrorSource, MuxSender, TransportError};
use tst_srt::SrtTransport;
use tst_srt::{ListenerBuilder, SocketBuilder};
use tst_test_helpers::synthetic_nal;

/// Minimal KLV blob with a valid SMPTE UL prefix so the demuxer classifies
/// it as `MetadataKind::KlvAsync`. Mirrors `minimal_klv()` in
/// `tests/pipeline_receiver.rs`. A bare ASCII placeholder would land in
/// `MetadataKind::Unknown` instead — counts toward `metas` only because we
/// don't filter by kind, but we want the right kind to flow through.
fn minimal_klv() -> Vec<u8> {
    // 16-byte SMPTE UL for ST 0601 + BER-short length + minimal body
    // (UDS tag 2 = "UAS LS Version Number" at 8 zero bytes — content is
    // semantically nonsense but the wrapper is well-formed).
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

/// Number of video AUs and KLV blobs the sender pushes. We assert
/// `samples >= EXPECT` and `metas >= EXPECT` — sender pushes a few extra
/// frames so the early-break condition trips well before the close.
///
/// Why the buffer? When libsrt signals peer close as `Broken` (not the
/// graceful `Closed`), `DemuxReceiver` propagates the error before calling
/// `Demuxer::flush()`. The last in-flight video AU only completes when
/// the *next* PUSI arrives (H.264 PES with length=0 sentinel), so without
/// flush the final AU is lost. Sending `SEND - EXPECT = 5` extra frames
/// makes sure we comfortably cross the assertion threshold while events
/// are still streaming through the queue.
const SEND: usize = 15;
const EXPECT: usize = 10;

#[test]
fn end_to_end_sender_to_receiver() {
    require_loopback!();
    // Listener side: bind to ephemeral port. `recv_latency` budget gives
    // libsrt's TSBPD path time to reorder + emit packets even on a busy CI
    // box; matches the sender side's `latency` for symmetry.
    let mut builder = ListenerBuilder::new();
    builder.recv_latency(Duration::from_millis(120));
    let lb = crate::common::Loopback::bind_with(builder);
    let port = lb.port;

    // Receiver runs in the accept-thread closure: wrap the accepted Socket
    // in SrtTransport, drive DemuxReceiver, count events, return a summary
    // tuple via accept.join() at the end.
    let accept = lb.spawn_accept(|server_socket| {
        let mut rx = DemuxReceiver::new(SrtTransport::new(server_socket));

        let mut samples = 0usize;
        let mut metas = 0usize;
        let mut got_pmap = false;

        // Drain events. Two valid termination paths:
        //   1. Iterator returns `None` — clean EOF after `Closed` triggered the
        //      demuxer's tail flush.
        //   2. Iterator returns `Some(Err(_))` where source is
        //      `DemuxReceiverErrorSource::Transport(Broken(_))` — peer hangup.
        //      libsrt typically signals sender-side close as a Broken receive on
        //      the peer (see `SrtTransport::recv_bytes` in `transport.rs`). Treat that as a
        //      clean stream end here: the sender did its job and any events
        //      already queued in the demuxer have been delivered.
        //
        // Early-exit once we've counted enough — covers both paths. A `Demux`
        // error or any non-Broken transport error is a real bug; fail loudly.
        for item in &mut rx {
            let event = match item {
                Ok(e) => e,
                Err(ref err)
                    if matches!(
                        err.source,
                        DemuxReceiverErrorSource::Transport(TransportError::Broken { .. })
                    ) =>
                {
                    break;
                }
                Err(other) => panic!("unexpected receiver error: {other:?}"),
            };
            match event {
                DemuxEvent::ProgramMap(_) => got_pmap = true,
                DemuxEvent::Sample { .. } => samples += 1,
                DemuxEvent::Metadata { .. } => metas += 1,
                // Discontinuity / NonConformant aren't expected on a clean
                // loopback round-trip but aren't fatal — let them pass.
                _ => {}
            }
            if samples >= EXPECT && metas >= EXPECT {
                break;
            }
        }

        (got_pmap, samples, metas)
    });
    accept.wait_ready();

    // MuxSender on the main thread: connect, build the pipeline, push N
    // video + N KLV frames at 30 fps PTS spacing (3000 ticks ≈ 33 ms at
    // 90 kHz), then brief sleep + close to let bytes drain on the wire.
    let socket = SocketBuilder::new()
        .latency(Duration::from_millis(120))
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");

    // Two-stream PMT: H.264 video on PID 0x100, async KLV on PID 0x101.
    // `add_klv(.., PrivateData, false)` matches the demuxer's async-KLV
    // recognition path — `false` means the muxer doesn't emit a PTS on
    // the KLV PES (typical for low-rate metadata).
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, MuxVideoCodec::H264);
        prog.add_klv(0x101, KlvStreamType::PrivateData, false);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().expect("build mux config")
    };
    let sender = MuxSender::new(SrtTransport::new(socket), cfg).expect("sender");

    let klv = minimal_klv();
    for i in 0..SEND as i64 {
        // Realistic AU body (500 bytes) with synthetic NAL header.
        // Frame 0 is a key frame; rest are P-frames. The mux treats
        // bytes opaquely so the only thing that matters wire-side is
        // start-code framing, which `synthetic_nal::h264_au` produces.
        let key = i == 0;
        let nal = synthetic_nal::h264_au(500, key);
        let pts = i * 3_000;
        sender
            .send_video(&nal, Pts90khz::new(pts), key)
            .expect("send_video");
        sender
            .send_klv(&klv, Pts90khz::new(pts), 0x00)
            .expect("send_klv");
    }

    // Drain pause before close — SRT's send queue is async w.r.t.
    // close. 1 s comfortably covers SRT's 120 ms latency budget plus
    // loopback scheduling jitter on every platform.
    //
    // Bumped from 200 ms in plan #66 — Darwin scheduling on Apple
    // Silicon (macOS arm64) pushes event emission past the previous
    // window. Linux loopback tolerates the smaller value but the
    // extra headroom is platform-stable.
    thread::sleep(Duration::from_secs(1));
    sender.close();

    let (got_pmap, samples, metas) = accept.join();

    assert!(got_pmap, "receiver should have observed PMT");
    assert!(
        samples >= EXPECT,
        "expected ≥ {EXPECT} video samples; got {samples}"
    );
    assert!(
        metas >= EXPECT,
        "expected ≥ {EXPECT} metadata events; got {metas}"
    );
}
