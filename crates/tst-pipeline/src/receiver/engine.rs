//! The demux receiver's state without its transport: it is handed aligned
//! packets, reconnects and the end of the stream, and hands back events and
//! that end. It opens no socket, reads no transport and takes no clock.
//! After an end it asks for nothing more: the transport is not read again
//! and the demuxer not flushed again.
//!
//! One clock read remains beneath it. The [`Demuxer`] it owns stamps each
//! stream's `last_seen` with the wall clock on a `std` build
//! (`tst_core::mpegts::stats::StreamStats`). Moving that stamp out of the
//! demuxer changes the public stats records and is not this module's to do.
//!
//! The driver is [`crate::DemuxReceiver::recv_event`]: it reads the
//! transport, classifies how a read ended, watches the reconnect counter
//! and counts the packets it hands in.

use tst_core::error::DemuxError;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer};
use tst_core::transport::TransportError;

/// How the driver's read ended the stream.
pub(crate) enum End {
    /// The peer ended the stream.
    Clean,
    /// The caller cancelled or closed the receive
    /// (`TransportError::ExplicitClose`).
    Cancelled,
    /// The transport broke.
    Failed(TransportError),
}

/// What [`ReceiveEngine::poll`] has for the driver.
pub(crate) enum Step {
    Event(DemuxEvent),
    /// Nothing is queued and the stream has not ended: hand in a packet, a
    /// reconnect or the end.
    NeedInput,
    /// The stream has ended; what the caller is told now. The engine wants
    /// no further input.
    Ended(Result<(), TransportError>),
}

/// The end the engine was handed, and what is still to be said about it.
enum Ended {
    Clean,
    Cancelled,
    /// `Some` until the failure has been reported.
    Failed(Option<TransportError>),
}

pub(crate) struct ReceiveEngine {
    demux: Demuxer,
    /// A reconnect was handed in and its marker has not been handed out. A
    /// flag, not a queued event: the reconnect clears the demuxer's queue.
    marker_owed: bool,
    /// The first end handed in.
    ended: Option<Ended>,
}

impl ReceiveEngine {
    pub(crate) fn new(demux: Demuxer) -> Self {
        Self {
            demux,
            marker_owed: false,
            ended: None,
        }
    }

    /// The next thing to hand to the caller. The reconnect marker comes
    /// first, so no event of the new connection precedes it; then what the
    /// demuxer has queued, the events of the final flush included; then the
    /// end.
    ///
    /// After an end the answer is `Ended` for good, and it is what the
    /// socket transports themselves answer when asked again: a clean end
    /// every time; a cancel every time; a failure once, and a clean end
    /// from then on. A transport that would have repeated its failure for
    /// ever is therefore heard once.
    #[inline]
    pub(crate) fn poll(&mut self) -> Step {
        if self.marker_owed {
            self.marker_owed = false;
            return Step::Event(DemuxEvent::ReconnectDiscontinuity);
        }
        if let Some(event) = self.demux.next_event() {
            return Step::Event(event);
        }
        match &mut self.ended {
            None => Step::NeedInput,
            Some(Ended::Clean) => Step::Ended(Ok(())),
            Some(Ended::Cancelled) => Step::Ended(Err(TransportError::ExplicitClose)),
            Some(Ended::Failed(unreported)) => Step::Ended(match unreported.take() {
                Some(e) => Err(e),
                None => Ok(()),
            }),
        }
    }

    /// Feed one aligned packet. An error is the demuxer's refusal of this
    /// packet (a strict mode's, or a missing sync byte) and does not end
    /// the stream.
    #[inline]
    pub(crate) fn handle_packet(&mut self, packet: &[u8; 188]) -> Result<(), DemuxError> {
        if self.ended.is_some() {
            return Ok(());
        }
        self.demux.feed_aligned(packet)
    }

    /// The transport was rebuilt: drop the demuxer's PSI, PES and continuity
    /// state, so nothing is assembled from both connections, and owe the
    /// caller the marker.
    pub(crate) fn handle_reconnect(&mut self) {
        if self.ended.is_some() {
            return;
        }
        self.demux.reset_sync();
        self.marker_owed = true;
    }

    /// The stream ended. The demuxer is flushed once, so an access unit that
    /// only its successor or the end can close comes out, and the end is
    /// kept. A second end changes nothing, with one exception: a cancel
    /// that comes after a clean end or a failure is from then on what the
    /// caller is told, as it is by a transport asked after its cancel.
    pub(crate) fn handle_end(&mut self, end: End) {
        match (&self.ended, end) {
            (None, end) => {
                self.demux.flush();
                self.ended = Some(match end {
                    End::Clean => Ended::Clean,
                    End::Cancelled => Ended::Cancelled,
                    End::Failed(e) => Ended::Failed(Some(e)),
                });
            }
            (Some(_), End::Cancelled) => self.ended = Some(Ended::Cancelled),
            (Some(_), _) => {}
        }
    }

    pub(crate) fn demuxer(&self) -> &Demuxer {
        &self.demux
    }

    pub(crate) fn demuxer_mut(&mut self) -> &mut Demuxer {
        &mut self.demux
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use tst_core::mpegts::common::Pts90khz;
    use tst_core::mpegts::demux::{DemuxerConfig, SamplePayload, StrictMode};
    use tst_core::mpegts::mux::{Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
    use tst_core::transport::BrokenCause;

    /// Payload bytes of one access unit: four TS packets.
    const AU_LEN: usize = 600;

    /// PAT, PMT and one four-packet access unit per marker, each unit
    /// `AU_LEN` copies of its marker, from a muxer of its own.
    fn stream(markers: &[u8]) -> Vec<[u8; 188]> {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        let mut mux = Muxer::new(b.build().unwrap()).unwrap();
        let mut packets = Vec::new();
        for (i, &marker) in markers.iter().enumerate() {
            let mut au = vec![0x00, 0x00, 0x00, 0x01, 0x65];
            au.resize(5 + AU_LEN, marker);
            mux.push_video(&au, Pts90khz::new(i as i64 * 3003), true)
                .unwrap();
            let mut buf = [0u8; 188];
            while mux.pull(&mut buf) == 188 {
                packets.push(buf);
            }
        }
        assert_eq!(packets.len(), 2 + 4 * markers.len());
        packets
    }

    fn engine() -> ReceiveEngine {
        ReceiveEngine::new(Demuxer::new())
    }

    fn feed(engine: &mut ReceiveEngine, packets: &[[u8; 188]]) {
        for p in packets {
            engine.handle_packet(p).unwrap();
        }
    }

    fn broken(msg: &str) -> TransportError {
        TransportError::Broken {
            msg: msg.into(),
            errno_code: None,
            cause: BrokenCause::Unspecified,
        }
    }

    /// One `poll` answer in a form that compares: a sample is the number of
    /// bytes it holds of each of the two markers the tests use.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Polled {
        Marker,
        ProgramMap,
        Sample { a1: usize, c3: usize },
        Other,
        NeedInput,
        Ended(Result<(), TransportError>),
    }

    fn poll(engine: &mut ReceiveEngine) -> Polled {
        match engine.poll() {
            Step::Event(DemuxEvent::ReconnectDiscontinuity) => Polled::Marker,
            Step::Event(DemuxEvent::ProgramMap(_)) => Polled::ProgramMap,
            Step::Event(DemuxEvent::Sample {
                payload: SamplePayload::Video { raw, .. },
                ..
            }) => Polled::Sample {
                a1: raw.iter().filter(|&&b| b == 0xA1).count(),
                c3: raw.iter().filter(|&&b| b == 0xC3).count(),
            },
            Step::Event(_) => Polled::Other,
            Step::NeedInput => Polled::NeedInput,
            Step::Ended(end) => Polled::Ended(end),
        }
    }

    /// Every answer up to and including the first that is not an event.
    fn drain(engine: &mut ReceiveEngine) -> Vec<Polled> {
        let mut out = Vec::new();
        loop {
            let p = poll(engine);
            let done = matches!(p, Polled::NeedInput | Polled::Ended(_));
            out.push(p);
            if done {
                return out;
            }
        }
    }

    #[test]
    fn the_marker_comes_out_before_the_queue() {
        let mut e = engine();
        e.handle_reconnect();
        // The new connection's first table is queued behind the owed marker.
        feed(&mut e, &stream(&[0xC3])[..2]);
        assert_eq!(
            drain(&mut e),
            [Polled::Marker, Polled::ProgramMap, Polled::NeedInput]
        );
    }

    #[test]
    fn a_reconnect_drops_the_demuxers_partial_state() {
        let mut e = engine();
        // The old connection: tables, then half of an access unit.
        feed(&mut e, &stream(&[0xA1])[..4]);
        assert_eq!(drain(&mut e), [Polled::ProgramMap, Polled::NeedInput]);

        e.handle_reconnect();
        // The new connection: tables and a whole unit, then the end. Had the
        // half unit survived, the new unit's start would have closed it and
        // handed it out; had the program survived, its PMT would be a repeat.
        feed(&mut e, &stream(&[0xC3]));
        e.handle_end(End::Clean);
        assert_eq!(
            drain(&mut e),
            [
                Polled::Marker,
                Polled::ProgramMap,
                Polled::Sample { a1: 0, c3: AU_LEN },
                Polled::Ended(Ok(())),
            ]
        );
    }

    #[test]
    fn a_clean_end_flushes_once_then_ends() {
        let mut e = engine();
        // A video PES has no length: the unit is only closed by a flush.
        feed(&mut e, &stream(&[0xA1]));
        assert_eq!(drain(&mut e), [Polled::ProgramMap, Polled::NeedInput]);
        e.handle_end(End::Clean);
        assert_eq!(
            drain(&mut e),
            [Polled::Sample { a1: AU_LEN, c3: 0 }, Polled::Ended(Ok(()))]
        );
    }

    #[test]
    fn a_failed_end_hands_out_the_flushed_events_before_the_error() {
        let mut e = engine();
        feed(&mut e, &stream(&[0xA1]));
        assert_eq!(drain(&mut e), [Polled::ProgramMap, Polled::NeedInput]);
        e.handle_end(End::Failed(broken("the link died")));
        assert_eq!(
            drain(&mut e),
            [
                Polled::Sample { a1: AU_LEN, c3: 0 },
                Polled::Ended(Err(broken("the link died"))),
            ]
        );
    }

    #[test]
    fn a_strict_mode_rejection_is_returned_at_once() {
        let mut cfg = DemuxerConfig::default();
        cfg.strict = StrictMode::Full;
        let mut e = ReceiveEngine::new(Demuxer::with_config(cfg));
        let packets = stream(&[0xA1]);
        feed(&mut e, &packets[..2]);
        // A packet with the transport error indicator set.
        let mut damaged = packets[2];
        damaged[1] |= 0x80;
        assert!(matches!(
            e.handle_packet(&damaged),
            Err(DemuxError::StrictRejection(_))
        ));
        // The rejection is the caller's to report; it is not an end.
        assert!(!drain(&mut e).iter().any(|p| matches!(p, Polled::Ended(_))));
        e.handle_packet(&packets[3]).unwrap();
    }

    #[test]
    fn a_second_end_does_not_flush_again() {
        let mut e = engine();
        feed(&mut e, &stream(&[0xA1]));
        e.handle_end(End::Clean);
        assert_eq!(
            drain(&mut e),
            [
                Polled::ProgramMap,
                Polled::Sample { a1: AU_LEN, c3: 0 },
                Polled::Ended(Ok(())),
            ]
        );
        // Neither a new outcome nor a new event.
        e.handle_end(End::Failed(broken("too late")));
        assert_eq!(drain(&mut e), [Polled::Ended(Ok(()))]);
        e.handle_end(End::Clean);
        assert_eq!(drain(&mut e), [Polled::Ended(Ok(()))]);
    }

    #[test]
    fn the_latch_repeats_a_clean_end() {
        let mut e = engine();
        e.handle_end(End::Clean);
        for _ in 0..3 {
            assert_eq!(poll(&mut e), Polled::Ended(Ok(())));
        }
    }

    /// What a socket transport answers when asked again after it broke: the
    /// failure once, then its own end.
    #[test]
    fn the_latch_reports_a_failure_once_and_then_the_end() {
        let mut e = engine();
        e.handle_end(End::Failed(broken("the link died")));
        assert_eq!(poll(&mut e), Polled::Ended(Err(broken("the link died"))));
        for _ in 0..3 {
            assert_eq!(poll(&mut e), Polled::Ended(Ok(())));
        }
    }

    #[test]
    fn the_latch_repeats_a_cancel() {
        let mut e = engine();
        e.handle_end(End::Cancelled);
        for _ in 0..3 {
            assert_eq!(
                poll(&mut e),
                Polled::Ended(Err(TransportError::ExplicitClose))
            );
        }
    }

    /// A cancel is reported from the moment it is handed in, also behind a
    /// clean end and behind a failure, reported or not.
    #[test]
    fn a_cancel_after_the_end_is_reported_from_then_on() {
        let cancelled = Polled::Ended(Err(TransportError::ExplicitClose));

        let mut e = engine();
        e.handle_end(End::Clean);
        assert_eq!(poll(&mut e), Polled::Ended(Ok(())));
        e.handle_end(End::Cancelled);
        assert_eq!(
            [poll(&mut e), poll(&mut e)],
            [cancelled.clone(), cancelled.clone()]
        );

        let mut e = engine();
        e.handle_end(End::Failed(broken("the link died")));
        e.handle_end(End::Cancelled);
        assert_eq!(
            [poll(&mut e), poll(&mut e)],
            [cancelled.clone(), cancelled.clone()]
        );

        // The cancel does not flush again or drop what the flush queued.
        let mut e = engine();
        feed(&mut e, &stream(&[0xA1]));
        e.handle_end(End::Failed(broken("the link died")));
        e.handle_end(End::Cancelled);
        assert_eq!(
            drain(&mut e),
            [
                Polled::ProgramMap,
                Polled::Sample { a1: AU_LEN, c3: 0 },
                cancelled,
            ]
        );
    }

    #[test]
    fn nothing_handed_in_after_the_end_changes_the_answer() {
        let mut e = engine();
        e.handle_end(End::Clean);
        e.handle_reconnect();
        feed(&mut e, &stream(&[0xA1]));
        assert_eq!(poll(&mut e), Polled::Ended(Ok(())));
    }
}
