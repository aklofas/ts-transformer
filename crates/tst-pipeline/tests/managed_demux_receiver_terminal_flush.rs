//! `ManagedDemuxReceiver` must flush pending PES reassembly state on the
//! `Closed` (cancel / cross-thread close) path exactly as the plain
//! `DemuxReceiver` does (`demux_receiver_terminal_flush.rs`). Since the
//! managed Python/JVM receivers close cancel-first, every managed close
//! used to drop the partial final video AU that the plain shell surfaces.
//!
//! Helpers mirror `demux_receiver_terminal_flush.rs` (each tst-pipeline
//! test binary carries its own small fixture set).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, SamplePayload};
use tst_core::mpegts::mux::{Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_core::transport::{RecvTransport, TransportCancel};
use tst_core::{BrokenCause, TransportError};
use tst_pipeline::{
    BackoffStrategy, DemuxReceiverErrorSource, ManagedDemuxReceiver, ManagedDemuxReceiverConfig,
    ManagedRecvTransport, ReconnectPolicy, RecvEndReason, ShellErrorKind,
};

/// Minimal valid Annex-B H.264 AU (AUD + IDR slice), 14 bytes; `marker`
/// makes each AU distinguishable.
fn build_h264_au(marker: u8) -> Vec<u8> {
    vec![
        0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x65, marker, 0xBB, 0xCC,
    ]
}

fn drain_mux(mux: &mut Muxer) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1316];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

fn to_ts_packets(bytes: &[u8]) -> Vec<[u8; 188]> {
    assert_eq!(
        bytes.len() % 188,
        0,
        "muxer output must be TS-packet aligned"
    );
    bytes
        .chunks_exact(188)
        .map(|c| {
            let mut a = [0u8; 188];
            a.copy_from_slice(c);
            a
        })
        .collect()
}

/// Three video AUs; the last one's PES is length-0 (unbounded) and only a
/// flush can complete it.
fn three_au_stream() -> Vec<[u8; 188]> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let mut bytes = Vec::new();
    for marker in [10u8, 11, 12] {
        mux.push_video(&build_h264_au(marker), Pts90khz::new(90_000), true)
            .unwrap();
        bytes.extend_from_slice(&drain_mux(&mut mux));
    }
    to_ts_packets(&bytes)
}

fn never_reconnect() -> Box<dyn FnMut() -> Result<CancelThenBroken, TransportError> + Send> {
    Box::new(|| {
        Err(TransportError::Broken {
            msg: "factory never reconnects (test)".into(),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        })
    })
}

fn fast_policy(max_attempts: Option<u32>) -> ReconnectPolicy {
    ReconnectPolicy {
        max_attempts,
        backoff: BackoffStrategy::Constant(Duration::from_millis(0)),
        ..Default::default()
    }
}

/// Delivers one 188-byte packet per `recv_bytes`, then — once exhausted —
/// fires the MANAGED receiver's cancel handle (handed in through a cell,
/// it only exists after construction) and returns `Broken`, the way a
/// socket fails once a cross-thread cancel has closed it. The managed
/// wrapper tears the inner down, re-checks its `cancelled` latch and
/// returns `ExplicitClose` (→ `ShellErrorKind::Closed`) without calling
/// the factory.
struct CancelThenBroken {
    packets: Vec<[u8; 188]>,
    pos: usize,
    cancel: Arc<Mutex<Option<Arc<dyn TransportCancel + Send + Sync>>>>,
}

impl RecvTransport for CancelThenBroken {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if self.pos >= self.packets.len() {
            if let Some(h) = self.cancel.lock().unwrap().as_ref() {
                h.cancel();
            }
            return Err(TransportError::Broken {
                msg: "socket closed by the cancel (test)".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            });
        }
        buf[..188].copy_from_slice(&self.packets[self.pos]);
        self.pos += 1;
        Ok(188)
    }

    fn max_payload(&self) -> usize {
        188
    }

    fn is_alive(&self) -> bool {
        self.pos < self.packets.len()
    }
}

/// CORR-08: the managed shell's `Closed` arm returned without flushing —
/// AU 12 (bounded only by a flush) was lost on every cancel-first close.
#[test]
fn managed_demux_receiver_flushes_final_video_au_on_cancel() {
    let cancel_cell: Arc<Mutex<Option<Arc<dyn TransportCancel + Send + Sync>>>> =
        Arc::new(Mutex::new(None));
    let source = CancelThenBroken {
        packets: three_au_stream(),
        pos: 0,
        cancel: Arc::clone(&cancel_cell),
    };
    let managed = ManagedRecvTransport::new(source, never_reconnect(), fast_policy(Some(5)));
    let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());
    let end_reason = rx.end_reason_handle();
    *cancel_cell.lock().unwrap() = rx.cancel_handle();

    let mut recovered_aus: Vec<Vec<u8>> = Vec::new();
    let final_err = loop {
        match rx.recv_event() {
            Ok(Some(DemuxEvent::Sample {
                payload: SamplePayload::Video { raw, .. },
                ..
            })) => recovered_aus.push(raw.to_vec()),
            Ok(Some(_)) => {}
            Ok(None) => panic!("the cancel path must surface Err(Closed), not a clean EOF"),
            Err(e) => break e,
        }
    };

    assert_eq!(
        final_err.kind,
        ShellErrorKind::Closed,
        "a managed cancel surfaces as Closed, got: {:?}",
        final_err.kind
    );
    assert!(
        matches!(
            final_err.source,
            DemuxReceiverErrorSource::Transport(TransportError::ExplicitClose)
        ),
        "source must be Transport(ExplicitClose), got: {:?}",
        final_err.source
    );
    assert_eq!(
        recovered_aus,
        vec![build_h264_au(10), build_h264_au(11), build_h264_au(12)],
        "all 3 AUs, including the final one (only bounded by a flush), must be recovered \
         before the managed shell surfaces Closed — the plain DemuxReceiver already does this"
    );
    assert_eq!(
        end_reason.get(),
        Some(RecvEndReason::Cancelled),
        "the end reason is still recorded on the cancel path after the flush"
    );
}

/// Byte sinks were plain-shell-only (`rg byte_sink managed_demux_receiver.rs`
/// → 0 hits). After the fold every aligned packet the managed shell parses
/// is fanned out first, in registration order, exactly as on `DemuxReceiver`.
#[test]
fn managed_demux_receiver_byte_sink_sees_every_packet() {
    let packets = three_au_stream();
    let expected = packets.len();
    // No cancel in the cell: the source exhausts with Broken, the factory
    // never reconnects, `max_attempts: Some(0)` exhausts the budget at
    // once → clean `Ok(None)` after the flush.
    let source = CancelThenBroken {
        packets,
        pos: 0,
        cancel: Arc::new(Mutex::new(None)),
    };
    let managed = ManagedRecvTransport::new(source, never_reconnect(), fast_policy(Some(0)));
    let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());

    let seen = Arc::new(AtomicUsize::new(0));
    let seen_cl = Arc::clone(&seen);
    rx.add_byte_sink(Box::new(move |pkt: &[u8]| {
        assert_eq!(pkt.len(), 188, "sinks see whole TS packets");
        assert_eq!(pkt[0], 0x47, "sinks see aligned packets");
        seen_cl.fetch_add(1, Ordering::SeqCst);
    }));

    let mut samples = 0usize;
    loop {
        match rx.recv_event() {
            Ok(Some(DemuxEvent::Sample { .. })) => samples += 1,
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => panic!("budget exhaustion is a clean EOF on the managed shell, got {e:?}"),
        }
    }
    assert_eq!(
        samples, 3,
        "all three AUs demuxed (last one via the EOS flush)"
    );
    assert_eq!(
        seen.load(Ordering::SeqCst),
        expected,
        "the sink must see every packet the managed shell parsed (no reconnect → nothing dropped)"
    );
    assert_eq!(
        rx.end_reason_handle().get(),
        Some(RecvEndReason::ReconnectExhausted)
    );
}
