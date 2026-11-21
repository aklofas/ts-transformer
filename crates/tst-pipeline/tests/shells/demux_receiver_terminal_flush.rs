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

use crate::common::{ScriptedRecv, broken, build_h264_au, drain_mux, to_ts_packets};

use tst_core::TransportError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, SamplePayload};
use tst_core::mpegts::mux::{
    AudioCodec, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};
use tst_pipeline::{DemuxReceiver, DemuxReceiverErrorSource, ShellErrorKind};

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

    let source = ScriptedRecv::packets(to_ts_packets(&bytes), broken("simulated socket break"));
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

    let source = ScriptedRecv::packets(to_ts_packets(&bytes), TransportError::ExplicitClose);
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

    let source = ScriptedRecv::packets(packets, broken("simulated socket break"));
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

// ---------------------------------------------------------------------------
// What is not an end, and what follows one
// ---------------------------------------------------------------------------

/// Serves its answers in order, one per read, then `Closed` for ever.
/// `reads` counts every call.
struct Answers {
    answers: std::collections::VecDeque<Result<[u8; 188], TransportError>>,
    reads: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl Answers {
    fn new(answers: impl IntoIterator<Item = Result<[u8; 188], TransportError>>) -> Self {
        Self {
            answers: answers.into_iter().collect(),
            reads: std::sync::Arc::default(),
        }
    }
}

impl tst_core::transport::RecvTransport for Answers {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match self.answers.pop_front() {
            Some(Ok(packet)) => {
                buf[..188].copy_from_slice(&packet);
                Ok(188)
            }
            Some(Err(e)) => Err(e),
            None => Err(TransportError::Closed),
        }
    }

    fn max_payload(&self) -> usize {
        188
    }

    fn is_alive(&self) -> bool {
        true
    }
}

/// Payload bytes of one marked access unit: four TS packets.
const AU_LEN: usize = 600;

/// One H.264 program; each access unit is `AU_LEN` copies of its marker.
fn marked_stream(markers: &[u8]) -> Vec<[u8; 188]> {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    let mut mux = Muxer::new(b.build().unwrap()).unwrap();
    let mut bytes = Vec::new();
    for (i, &marker) in markers.iter().enumerate() {
        let mut au = vec![0x00, 0x00, 0x00, 0x01, 0x65];
        au.resize(5 + AU_LEN, marker);
        mux.push_video(&au, Pts90khz::new(i as i64 * 3003), true)
            .unwrap();
        bytes.extend_from_slice(&drain_mux(&mut mux));
    }
    to_ts_packets(&bytes)
}

/// How many bytes of `raw` are `marker`.
fn count(raw: &[u8], marker: u8) -> usize {
    raw.iter().filter(|&&b| b == marker).count()
}

/// `Backpressure` arrives with half of an access unit received (two of its
/// four packets). It is handed to the caller and finalises nothing: the unit
/// comes out once, whole, when the rest has arrived. A flush at that point
/// would hand out the first half as one sample and the second as another.
#[test]
fn backpressure_inside_a_pes_finalises_nothing() {
    let packets = marked_stream(&[0xA1, 0xB2]);
    // PAT, PMT and the first two packets of unit A, then the pause.
    let pause_at = 4;
    assert!(
        packets[2][1] & 0x40 != 0 && packets[3..6].iter().all(|p| p[1] & 0x40 == 0),
        "test setup: unit A is packets 2 to 5"
    );
    let mut answers: Vec<Result<[u8; 188], TransportError>> =
        packets.iter().copied().map(Ok).collect();
    answers.insert(
        pause_at,
        Err(TransportError::Backpressure {
            msg: "nothing yet".into(),
            errno_code: None,
        }),
    );
    let mut rx = DemuxReceiver::new(Answers::new(answers));

    let mut trace: Vec<String> = Vec::new();
    loop {
        match rx.recv_event() {
            Ok(Some(DemuxEvent::Sample {
                payload: SamplePayload::Video { raw, .. },
                ..
            })) => trace.push(format!(
                "sample A1x{} B2x{}",
                count(&raw, 0xA1),
                count(&raw, 0xB2)
            )),
            Ok(Some(DemuxEvent::ProgramMap(_))) => trace.push("program map".into()),
            Ok(Some(other)) => trace.push(format!("{other:?}")),
            Ok(None) => break,
            Err(e) if e.kind == ShellErrorKind::Backpressure => trace.push("backpressure".into()),
            Err(e) => panic!("the stream ends cleanly: {e}"),
        }
    }

    assert_eq!(
        trace,
        [
            "program map",
            "backpressure",
            "sample A1x600 B2x0",
            "sample A1x0 B2x600",
        ]
    );
}

/// The first terminal result of each kind, then the same transport asked
/// again: one that would go on answering DATA after its end.
fn asked_again_after(end: TransportError) -> (Vec<String>, u32, u32) {
    let packets = marked_stream(&[0xA1, 0xB2, 0xC3, 0xD4]);
    let (head, tail) = packets.split_at(8);
    let answers = head
        .iter()
        .copied()
        .map(Ok)
        .chain([Err(end)])
        .chain(tail.iter().copied().map(Ok));
    let transport = Answers::new(answers);
    let reads = transport.reads.clone();
    let mut rx = DemuxReceiver::new(transport);

    let outcome = |r: Result<Option<DemuxEvent>, tst_pipeline::DemuxReceiverError>| match r {
        Ok(Some(_)) => "event".to_string(),
        Ok(None) => "end".to_string(),
        Err(e) => format!("{:?}: {}", e.kind, e.source),
    };
    let first = loop {
        let o = outcome(rx.recv_event());
        if o != "event" {
            break o;
        }
    };
    let reads_at_the_end = reads.load(std::sync::atomic::Ordering::SeqCst);
    let mut results = vec![first];
    for _ in 0..3 {
        results.push(outcome(rx.recv_event()));
    }
    (
        results,
        reads_at_the_end,
        reads.load(std::sync::atomic::Ordering::SeqCst),
    )
}

/// After its first terminal result a `DemuxReceiver` never reads its
/// transport again, whatever the transport would have said next, and
/// answers what a socket transport answers when asked again: a clean end
/// every time, a cancel every time, a failure once and then the end.
#[test]
fn the_latch_stops_reading_a_transport_that_would_go_on() {
    let broken_text = "TransportBroken: transport broken: the link died";
    let cancel_text = "Closed: transport explicitly closed by caller";
    for (end, expected) in [
        (TransportError::Closed, ["end"; 4]),
        (broken("the link died"), [broken_text, "end", "end", "end"]),
        (TransportError::ExplicitClose, [cancel_text; 4]),
    ] {
        let (results, reads_at_the_end, reads_after) = asked_again_after(end);
        assert_eq!(results, expected);
        assert_eq!(
            reads_after, reads_at_the_end,
            "{expected:?}: the transport was read after the end"
        );
    }
}

/// A transport that answers `Broken` for ever (none in this tree does: the
/// socket transports answer `Closed` once they have latched dead) is heard
/// once; the receiver does not ask it again.
#[test]
fn a_failure_is_reported_once_and_then_the_end() {
    struct AlwaysBroken(std::collections::VecDeque<[u8; 188]>);
    impl tst_core::transport::RecvTransport for AlwaysBroken {
        fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            match self.0.pop_front() {
                Some(packet) => {
                    buf[..188].copy_from_slice(&packet);
                    Ok(188)
                }
                None => Err(broken("the link died")),
            }
        }
        fn max_payload(&self) -> usize {
            188
        }
        fn is_alive(&self) -> bool {
            true
        }
    }

    let mut rx = DemuxReceiver::new(AlwaysBroken(marked_stream(&[0xA1, 0xB2]).into()));
    while let Ok(Some(_)) = rx.recv_event() {}
    // The loop above consumed the failure; from here on the end.
    for call in 0..3 {
        assert!(
            matches!(rx.recv_event(), Ok(None)),
            "call {call} after the failure"
        );
    }

    let mut rx = DemuxReceiver::new(AlwaysBroken(marked_stream(&[0xA1, 0xB2]).into()));
    let e = loop {
        match rx.recv_event() {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the stream failed; it did not end cleanly"),
            Err(e) => break e,
        }
    };
    assert_eq!(e.kind, ShellErrorKind::TransportBroken);
    assert!(
        matches!(
            &e.source,
            DemuxReceiverErrorSource::Transport(TransportError::Broken { msg, .. })
                if msg == "the link died"
        ),
        "{:?}",
        e.source
    );
}

/// Answers like a socket transport: its packets, then `end` once, then
/// `Closed`; and `ExplicitClose` at its entry once the cancel has fired.
/// Its own `close()` fires the cancel latch too and is told apart by a flag
/// of its own, as SRT's does.
struct Cancellable {
    packets: std::collections::VecDeque<[u8; 188]>,
    end: Option<TransportError>,
    cancel: std::sync::Arc<crate::common::FlagCancel>,
    closed: bool,
    /// Reads go on as if nothing had happened after a cancel: a transport
    /// that does not look at its own latch.
    deaf: bool,
    reads: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl tst_core::transport::RecvTransport for Cancellable {
    fn recv_bytes(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.closed {
            return Err(TransportError::Closed);
        }
        if !self.deaf && tst_core::transport::TransportCancel::is_cancelled(&*self.cancel) {
            return Err(TransportError::ExplicitClose);
        }
        match self.packets.pop_front() {
            Some(packet) => {
                buf[..188].copy_from_slice(&packet);
                Ok(188)
            }
            None => Err(self.end.take().unwrap_or(TransportError::Closed)),
        }
    }

    fn max_payload(&self) -> usize {
        188
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn close(&mut self) {
        self.closed = true;
        tst_core::transport::TransportCancel::cancel(&*self.cancel);
    }

    fn cancel_handle(
        &self,
    ) -> Option<std::sync::Arc<dyn tst_core::transport::TransportCancel + Send + Sync>> {
        Some(self.cancel.clone())
    }
}

/// The receiver's first terminal result and its next two, with `then` run
/// on the receiver after the first; and the transport reads `then` and the
/// two later calls cost.
fn after_the_end(
    end: TransportError,
    then: impl FnOnce(&mut DemuxReceiver<Cancellable>),
) -> (Vec<String>, u32) {
    let reads = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut rx = DemuxReceiver::new(Cancellable {
        packets: marked_stream(&[0xA1]).into(),
        end: Some(end),
        cancel: std::sync::Arc::new(crate::common::FlagCancel(crate::common::flag(false))),
        closed: false,
        deaf: false,
        reads: reads.clone(),
    });
    let outcome = |r: Result<Option<DemuxEvent>, tst_pipeline::DemuxReceiverError>| match r {
        Ok(Some(_)) => "event".to_string(),
        Ok(None) => "end".to_string(),
        Err(e) => format!("{:?}", e.kind),
    };
    let first = loop {
        let o = outcome(rx.recv_event());
        if o != "event" {
            break o;
        }
    };
    let reads_at_the_end = reads.load(std::sync::atomic::Ordering::SeqCst);
    then(&mut rx);
    let results = vec![first, outcome(rx.recv_event()), outcome(rx.recv_event())];
    (
        results,
        reads.load(std::sync::atomic::Ordering::SeqCst) - reads_at_the_end,
    )
}

fn cancel(rx: &mut DemuxReceiver<Cancellable>) {
    rx.cancel_handle().expect("the transport has one").cancel();
}

/// A cancel that fires after the stream has ended is still reported, as the
/// transport itself would report it, and without reading the transport.
#[test]
fn a_cancel_after_the_end_is_still_reported() {
    assert_eq!(
        after_the_end(TransportError::Closed, cancel),
        (vec!["end".into(), "Closed".into(), "Closed".into()], 0)
    );
    assert_eq!(
        after_the_end(broken("the link died"), cancel),
        (
            vec!["TransportBroken".into(), "Closed".into(), "Closed".into()],
            0
        )
    );
    // With no cancel, the same two streams go on answering the end.
    assert_eq!(
        after_the_end(TransportError::Closed, |_| {}),
        (vec!["end".into(); 3], 0)
    );
    assert_eq!(
        after_the_end(broken("the link died"), |_| {}),
        (
            vec!["TransportBroken".into(), "end".into(), "end".into()],
            0
        )
    );
}

/// The receiver's own `close()` after the end is not a cancel, although a
/// transport may fire its cancel latch when it is closed: the end goes on
/// being the end.
#[test]
fn the_receivers_own_close_after_the_end_is_not_a_cancel() {
    assert_eq!(
        after_the_end(TransportError::Closed, DemuxReceiver::close),
        (vec!["end".into(); 3], 0)
    );
    assert_eq!(
        after_the_end(broken("the link died"), DemuxReceiver::close),
        (
            vec!["TransportBroken".into(), "end".into(), "end".into()],
            0
        )
    );
}

/// Every result of a receiver over three access units and `end`, with a
/// cancel fired right after the first result that came once the transport's
/// end had been read: the caller is still taking the events the end made, and
/// the cancel is not hers to hear until they are out. Three calls after the
/// last event are kept; the events before are folded into one `event` each.
fn cancel_while_the_end_is_handed_out(end: TransportError) -> Vec<String> {
    let packets = marked_stream(&[0xA1, 0xB2, 0xC3]);
    let stream_len = packets.len() as u32;
    let reads = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut rx = DemuxReceiver::new(Cancellable {
        packets: packets.into(),
        end: Some(end),
        cancel: std::sync::Arc::new(crate::common::FlagCancel(crate::common::flag(false))),
        closed: false,
        deaf: false,
        reads: reads.clone(),
    });
    let outcome = |r: Result<Option<DemuxEvent>, tst_pipeline::DemuxReceiverError>| match r {
        Ok(Some(_)) => "event".to_string(),
        Ok(None) => "end".to_string(),
        Err(e) => format!("{:?}", e.kind),
    };
    let mut fired_after_an_event = false;
    let mut results = Vec::new();
    while results.len() < 40 {
        let o = outcome(rx.recv_event());
        let event = o == "event";
        results.push(o);
        if !fired_after_an_event && reads.load(std::sync::atomic::Ordering::SeqCst) > stream_len {
            assert!(event, "the end was read by a call that handed out no event");
            cancel(&mut rx);
            fired_after_an_event = true;
        }
        if !event && results.iter().filter(|r| *r != "event").count() == 3 {
            break;
        }
    }
    assert!(fired_after_an_event);
    results
}

/// A cancel fired while the caller is still taking the events the end
/// produced is reported after them, and from then on.
#[test]
fn a_cancel_fired_while_a_clean_ends_events_are_handed_out_is_reported_after_them() {
    let results = cancel_while_the_end_is_handed_out(TransportError::Closed);
    let (events, tail) = results.split_at(results.len() - 3);
    assert!(events.iter().all(|r| r == "event"), "{results:?}");
    assert!(events.len() > 1, "{results:?}");
    assert_eq!(tail, ["Closed"; 3], "{results:?}");
}

/// The same for a stream that ended in failure: the failure is reported
/// once, then the cancel.
#[test]
fn a_cancel_fired_while_a_failures_events_are_handed_out_is_reported_after_the_failure() {
    let results = cancel_while_the_end_is_handed_out(broken("the link died"));
    let (events, tail) = results.split_at(results.len() - 3);
    assert!(events.iter().all(|r| r == "event"), "{results:?}");
    assert!(events.len() > 1, "{results:?}");
    assert_eq!(tail, ["TransportBroken", "Closed", "Closed"], "{results:?}");
}

/// The first terminal result is the transport's own. A transport that ends
/// its stream cleanly although its cancel latch is already set (none of the
/// socket transports does) is taken at its word, then and on later calls:
/// only a cancel fired after the end is added to it.
#[test]
fn a_cancel_that_fired_before_a_clean_end_does_not_change_it() {
    let mut rx = DemuxReceiver::new(Cancellable {
        packets: marked_stream(&[0xA1]).into(),
        end: Some(TransportError::Closed),
        cancel: std::sync::Arc::new(crate::common::FlagCancel(crate::common::flag(true))),
        closed: false,
        deaf: true,
        reads: std::sync::Arc::default(),
    });
    while let Some(_event) = rx.recv_event().expect("a clean stream") {}
    for call in 0..3 {
        assert!(matches!(rx.recv_event(), Ok(None)), "call {call}");
    }
}
