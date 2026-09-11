//! Regression coverage: `DemuxReceiver` must flush pending PES reassembly
//! state on a *terminal* transport error, not only on the clean
//! `TransportError::Closed` (peer-EOS) path.
//!
//! # The bug
//!
//! H.264 video PES packets are muxed with `PES_packet_length = 0`
//! ("unbounded" per ITU-T H.222.0 V9 §2.4.3.7) because the encoder doesn't
//! know the AU's length up front. The demuxer can only recognize such a PES
//! is complete when either the next PES on that PID starts (PUSI) or the
//! caller explicitly flushes. Before this fix, `DemuxReceiver::recv_event`
//! only called `Demuxer::flush` on the clean EOF path
//! (`TransportError::Closed` → `ShellErrorKind::EndOfStream`) — so every
//! live receive session that ended via a broken socket
//! (`ShellErrorKind::TransportBroken`) or a caller-initiated cross-thread
//! cancel (`ShellErrorKind::Closed`, from `TransportError::ExplicitClose`)
//! silently dropped the final buffered video AU, even though the bytes had
//! fully arrived. Reproduced live over SRT (150 sent → 149 demuxed) and
//! over an unimpaired loopback (90 → 89); the same bytes through an offline
//! `Demuxer` (which the caller flushes manually) always recover the full
//! count.
//!
//! # Sibling coverage
//!
//! `Demuxer::flush` (`crates/tst-core/src/mpegts/demux/demuxer.rs`) drains
//! **every** PID with buffered partial-PES state
//! (`PesReassembler::drain_partial` iterates the whole `by_pid` map), not
//! just video. `demux_receiver_flushes_pending_audio_pid_on_transport_broken`
//! below proves a non-video PID benefits identically, and additionally
//! documents the accepted trade-off: unlike video (always "pending" by
//! design until the next PUSI or a flush), a length-known stream (audio/KLV)
//! only has PES bytes still buffered when the transport broke while that PES
//! was mid-flight — so the flushed sample can be a genuinely **truncated**
//! access unit. That risk already existed on the pre-fix `EndOfStream` path
//! (a sender that stops mid-PES still looks like a clean TCP-style close to
//! some transports) — this fix does not introduce a new risk category, it
//! just applies the same accepted behavior to more termination reasons.

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, SamplePayload};
use tst_core::mpegts::mux::{
    AudioCodec, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};
use tst_core::transport::RecvTransport;
use tst_core::{BrokenCause, TransportError};
use tst_pipeline::{DemuxReceiver, DemuxReceiverErrorSource, ShellErrorKind};

/// Minimal valid Annex-B H.264 AU (AUD + IDR slice), 14 bytes. `marker`
/// occupies the byte after the slice NAL header so each pushed AU has
/// distinguishable content for the assertion.
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

/// Delivers one 188-byte TS packet per `recv_bytes` call, then signals a
/// *terminal, non-clean* transport error once exhausted — simulating a live
/// socket dying (`Broken`) or a caller cancelling from another thread
/// (`ExplicitClose`) instead of the peer closing cleanly (`Closed`, already
/// covered by `demux_receiver_malformed_pes_recovery.rs`'s `PacketSource`).
struct TerminalAfterPackets {
    packets: Vec<[u8; 188]>,
    pos: usize,
    terminal: TransportError,
}

impl TerminalAfterPackets {
    fn broken(packets: Vec<[u8; 188]>) -> Self {
        Self {
            packets,
            pos: 0,
            terminal: TransportError::Broken {
                msg: "simulated socket break".into(),
                errno_code: None,
                cause: BrokenCause::Unspecified,
            },
        }
    }

    fn explicit_close(packets: Vec<[u8; 188]>) -> Self {
        Self {
            packets,
            pos: 0,
            terminal: TransportError::ExplicitClose,
        }
    }
}

impl RecvTransport for TerminalAfterPackets {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if self.pos >= self.packets.len() {
            return Err(match &self.terminal {
                TransportError::Broken {
                    msg,
                    errno_code,
                    cause,
                } => TransportError::Broken {
                    msg: msg.clone(),
                    errno_code: *errno_code,
                    cause: *cause,
                },
                TransportError::ExplicitClose => TransportError::ExplicitClose,
                other => panic!("unexpected terminal fixture: {other:?}"),
            });
        }
        let pkt = self.packets[self.pos];
        buf[..188].copy_from_slice(&pkt);
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

/// The mandatory RED→GREEN regression: 3 video AUs muxed and delivered in
/// full, then the transport breaks (no clean `Closed`). All 3 AUs — including
/// the final one, which is only bounded by a flush since video PES length is
/// unbounded — must be recovered before the `TransportBroken` error surfaces.
#[test]
fn demux_receiver_flushes_final_video_au_on_transport_broken() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    let mut bytes = Vec::new();
    for (i, marker) in [1u8, 2, 3].into_iter().enumerate() {
        mux.push_video(
            &build_h264_au(marker),
            Pts90khz::new(90_000 * (i as i64 + 1)),
            true,
        )
        .unwrap();
        bytes.extend_from_slice(&drain_mux(&mut mux));
    }

    let source = TerminalAfterPackets::broken(to_ts_packets(&bytes));
    let mut rx = DemuxReceiver::new(source);

    let mut recovered_aus: Vec<Vec<u8>> = Vec::new();
    let final_err = loop {
        match rx.recv_event() {
            Ok(Some(DemuxEvent::Sample {
                payload: SamplePayload::Video { raw, .. },
                ..
            })) => recovered_aus.push(raw.to_vec()),
            Ok(Some(_)) => {}
            Ok(None) => panic!(
                "recv_event returned clean EOF (Ok(None)) — the fixture's transport breaks, \
                 it never signals TransportError::Closed"
            ),
            Err(e) => break e,
        }
    };

    assert_eq!(
        final_err.kind,
        ShellErrorKind::TransportBroken,
        "fixture must surface TransportBroken, got: {:?}",
        final_err.kind
    );
    assert!(
        matches!(
            final_err.source,
            DemuxReceiverErrorSource::Transport(TransportError::Broken { .. })
        ),
        "source must be Transport(Broken(_)), got: {:?}",
        final_err.source
    );

    assert_eq!(
        recovered_aus,
        vec![build_h264_au(1), build_h264_au(2), build_h264_au(3),],
        "all 3 pushed AUs, including the final one (only bounded by a flush — video \
         PES_packet_length is 0/unbounded), must be recovered even though the transport \
         broke instead of closing cleanly"
    );
}

/// Sibling terminal kind: a caller-initiated cross-thread cancel
/// (`TransportError::ExplicitClose` → `ShellErrorKind::Closed`) is just as
/// terminal as a broken socket — the receive loop is over either way — so it
/// must flush too. Same-thread `close()` already routed through the
/// `EndOfStream` flush path before this fix; this closes the analogous gap
/// for the cross-thread cancel path.
#[test]
fn demux_receiver_flushes_final_video_au_on_explicit_close() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    // Three AUs, mirroring the primary test above: the syncer's HUNT->LOCKED
    // transition needs 4 confirming sync bytes (>=5 TS packets buffered)
    // before it emits anything at all, so a single tiny AU's worth of
    // packets (plus PAT/PMT) isn't reliably enough to prove the *final* AU
    // specifically was flush-recovered rather than just never having
    // reached the demuxer. AUs 10-11 complete normally (each bounded by the
    // next AU's PES start); AU 12 is only bounded by the flush.
    let mut bytes = Vec::new();
    for marker in [10u8, 11, 12] {
        mux.push_video(&build_h264_au(marker), Pts90khz::new(90_000), true)
            .unwrap();
        bytes.extend_from_slice(&drain_mux(&mut mux));
    }

    let source = TerminalAfterPackets::explicit_close(to_ts_packets(&bytes));
    let mut rx = DemuxReceiver::new(source);

    let mut recovered_aus: Vec<Vec<u8>> = Vec::new();
    let final_err = loop {
        match rx.recv_event() {
            Ok(Some(DemuxEvent::Sample {
                payload: SamplePayload::Video { raw, .. },
                ..
            })) => recovered_aus.push(raw.to_vec()),
            Ok(Some(_)) => {}
            Ok(None) => panic!("fixture never signals a clean Closed"),
            Err(e) => break e,
        }
    };

    assert_eq!(
        final_err.kind,
        ShellErrorKind::Closed,
        "fixture must surface Closed (ExplicitClose maps to Closed kind), got: {:?}",
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
        "all 3 AUs, including the final one, must be recovered on a cross-thread \
         ExplicitClose, mirroring the same-thread close() -> EndOfStream flush path"
    );
}

/// Sibling coverage: a non-video PID's buffered partial PES is drained by
/// the same flush call. Unlike video, audio PES declares a real
/// `PES_packet_length`, so its bytes are only left "pending" when the
/// transport broke mid-PES — the recovered sample is a genuine truncation of
/// the original frame. This documents the accepted trade-off from the module
/// docs rather than hiding it: recovering a truncated frame is strictly
/// better than losing it outright, but callers of hostile/interrupted wire
/// data must already tolerate arbitrary AU content.
#[test]
fn demux_receiver_flushes_pending_audio_pid_on_transport_broken() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_audio(0x101, AudioCodec::Aac);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();

    // Video AU: only recoverable via flush (same mechanism as the primary
    // test above), proving the two PIDs are drained by the one flush call.
    mux.push_video(&build_h264_au(7), Pts90khz::new(90_000), true)
        .unwrap();
    let video_bytes = drain_mux(&mut mux);

    // Audio frame large enough to span multiple 188-byte TS packets so it
    // can be truncated mid-PES (a single-packet frame would always complete
    // via ordinary length-driven reassembly and wouldn't exercise the flush
    // path at all).
    let full_frame: Vec<u8> = (0u8..=255).cycle().take(1000).collect();
    mux.push_audio(&full_frame, Pts90khz::new(90_000)).unwrap();
    let audio_bytes = drain_mux(&mut mux);
    let audio_packets = to_ts_packets(&audio_bytes);
    assert!(
        audio_packets.len() >= 2,
        "test setup: audio PES must span >=2 TS packets to allow a mid-PES truncation"
    );
    // Withhold the trailing half of the audio PES's packets — the transport
    // breaks before the audio PES's declared length is ever reached.
    let deliver_n = audio_packets.len() / 2;
    assert!(
        deliver_n >= 1,
        "test setup: need at least 1 audio packet delivered"
    );

    let mut packets = to_ts_packets(&video_bytes);
    packets.extend_from_slice(&audio_packets[..deliver_n]);

    let source = TerminalAfterPackets::broken(packets);
    let mut rx = DemuxReceiver::new(source);

    let mut saw_video = false;
    let mut recovered_audio: Option<Vec<u8>> = None;
    let final_err = loop {
        match rx.recv_event() {
            Ok(Some(DemuxEvent::Sample {
                payload: SamplePayload::Video { raw, .. },
                ..
            })) => {
                assert_eq!(raw.to_vec(), build_h264_au(7));
                saw_video = true;
            }
            Ok(Some(DemuxEvent::Sample {
                payload: SamplePayload::Audio { frames, .. },
                ..
            })) => recovered_audio = Some(frames.to_vec()),
            Ok(Some(_)) => {}
            Ok(None) => panic!("fixture never signals a clean Closed"),
            Err(e) => break e,
        }
    };

    assert_eq!(final_err.kind, ShellErrorKind::TransportBroken);
    assert!(
        saw_video,
        "the video PID's pending AU must also be flushed alongside the audio PID"
    );

    let recovered_audio = recovered_audio
        .expect("the partially-buffered audio PES must still be flushed, not silently dropped");
    assert!(
        !recovered_audio.is_empty(),
        "flush must recover the bytes that did arrive"
    );
    assert!(
        recovered_audio.len() < full_frame.len(),
        "the recovered audio frame must be a genuine (documented) truncation — fewer bytes \
         than were originally pushed, since the transport broke mid-PES: got {} of {} bytes",
        recovered_audio.len(),
        full_frame.len()
    );
    assert_eq!(
        &recovered_audio[..],
        &full_frame[..recovered_audio.len()],
        "the recovered bytes must be an exact prefix of the original frame (truncated, not \
         corrupted)"
    );
}
