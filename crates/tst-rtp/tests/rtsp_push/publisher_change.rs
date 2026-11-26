//! A mount whose publisher changes: the mount's receiver yields the
//! publishers' streams one after the other. A demuxer reading it sees a
//! second publisher with other tracks as a second program map, with the
//! first publisher's units on the PIDs that leave handed out before it, and
//! a second publisher with the same tracks as one continuity discontinuity
//! per PID under the one program map.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tst_core::mpegts::demux::{DemuxEvent, SamplePayload};
use tst_core::mpegts::mux::{KlvStreamType, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_pipeline::{DemuxReceiver, MuxSender, ShellErrorKind};
use tst_rtp::PushDescription;

use crate::common::*;
use crate::fixtures::publish_mount::{KLV_PID, VIDEO_PID};
use crate::fixtures::raw_rtsp::scaled;
use crate::fixtures::raw_rtsp_publisher::klv_unit;

/// Access units, each with one KLV unit, that every publisher sends.
const UNITS: usize = 60;

/// The video and KLV PIDs of an MPEG-TS publisher (its PMT is on 0x1000).
const TS_VIDEO_PID: u16 = 0x1011;
const TS_KLV_PID: u16 = 0x1031;

/// An MPEG-TS publisher: a `MuxSender` over the push, one program with an
/// H.264 stream and, with `klv`, a KLV stream. Returns once the mount has
/// let it go, which is after the mount has passed on all it sent.
fn publish_ts(s: &Served, klv: bool) {
    let mut program = MuxerProgramConfigBuilder::new(1, 0x1000);
    program.add_video(TS_VIDEO_PID, VideoCodec::H264);
    if klv {
        program.add_klv(TS_KLV_PID, KlvStreamType::PrivateData, true);
    }
    let mut config = MuxerConfig::builder();
    config.add_program(program.build());
    let push = s.push(&PushDescription::Mp2t);
    let sender = MuxSender::new(push, config.build().unwrap()).unwrap();
    for i in 0..UNITS {
        let at = pts(i as i64 * AU_STEP);
        let mut au = Vec::new();
        if i == 0 {
            for ps in [sps(), pps()] {
                au.extend_from_slice(&[0, 0, 0, 1]);
                au.extend_from_slice(&ps);
            }
        }
        au.extend_from_slice(&access_unit(i));
        sender.send_video(&au, at, i == 0).unwrap();
        if klv {
            sender.send_klv(&klv_unit(i as u32), at, 0).unwrap();
        }
    }
    sender.finish().unwrap();
    s.until_no_publisher();
}

/// An elementary publisher: H.264 and KLV through the push, re-muxed by the
/// mount on [`VIDEO_PID`] and [`KLV_PID`]. Returns once the mount has let it
/// go: the mount writes the access unit it still holds before that.
fn publish_elementary(s: &Served) {
    let mut push = s.push(&elementary(true));
    for i in 0..UNITS {
        let at = pts(i as i64 * AU_STEP);
        push.send_video(&access_unit(i), at).unwrap();
        push.send_klv(&klv_unit(1000 + i as u32), at).unwrap();
    }
    push.close().unwrap();
    s.until_no_publisher();
}

/// The mount's receiver read through one `DemuxReceiver` on a thread of its
/// own, from before the first publisher until the server stops.
struct MountReader {
    events: Arc<Mutex<Vec<DemuxEvent>>>,
    thread: JoinHandle<()>,
}

impl MountReader {
    fn start(s: &Served) -> Self {
        let mut app = s.app();
        // Between publishers the mount is idle: each receive that waits out
        // this bound reads as `Backpressure` and the loop goes on.
        app.set_recv_timeout(Some(Duration::from_millis(100)));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let thread = std::thread::spawn(move || {
            let mut rx = DemuxReceiver::new(app);
            let give_up = Instant::now() + scaled(Duration::from_secs(10));
            loop {
                match rx.recv_event() {
                    Ok(Some(ev)) => sink.lock().unwrap().push(ev),
                    Ok(None) => return,
                    Err(e) if e.kind == ShellErrorKind::Backpressure => {
                        assert!(Instant::now() < give_up, "the mount's receiver never ended");
                    }
                    Err(e) => panic!("the mount's receiver failed: {e:?}"),
                }
            }
        });
        Self { events, thread }
    }

    /// Stop the server and return every event the receiver yielded. The
    /// stop ends the receiver only after it has read every frame already
    /// queued for it; its end hands out the unit still in flight.
    fn finish(self, s: &Served) -> Vec<DemuxEvent> {
        assert_eq!(s.mount.stats().frames_dropped_app, 0);
        s.server.stop().unwrap();
        self.thread.join().expect("the mount's reader panicked");
        std::mem::take(&mut *self.events.lock().unwrap())
    }
}

/// Program maps, and video samples, KLV units and discontinuities per PID.
#[derive(Debug, Default, PartialEq)]
struct Seen {
    program_maps: usize,
    video: BTreeMap<u16, usize>,
    klv: BTreeMap<u16, usize>,
    discontinuities: BTreeMap<u16, usize>,
}

fn seen(events: &[DemuxEvent]) -> Seen {
    let mut seen = Seen::default();
    for ev in events {
        match ev {
            DemuxEvent::ProgramMap(_) => seen.program_maps += 1,
            DemuxEvent::Sample {
                stream,
                payload: SamplePayload::Video { .. },
                ..
            } => *seen.video.entry(stream.pid).or_default() += 1,
            DemuxEvent::Metadata { stream, .. } => *seen.klv.entry(stream.pid).or_default() += 1,
            DemuxEvent::Discontinuity { stream, .. } => {
                *seen.discontinuities.entry(stream.pid).or_default() += 1;
            }
            _ => {}
        }
    }
    seen
}

/// Where the second `ProgramMap` is among `events`, and the PIDs it names.
fn second_program_map(events: &[DemuxEvent]) -> (usize, Vec<u16>) {
    events
        .iter()
        .enumerate()
        .filter_map(|(at, ev)| match ev {
            DemuxEvent::ProgramMap(map) => {
                let mut pids: Vec<u16> = map.streams.iter().map(|st| st.pid).collect();
                pids.sort_unstable();
                Some((at, pids))
            }
            _ => None,
        })
        .nth(1)
        .unwrap_or_else(|| panic!("no second program map: {:?}", seen(events)))
}

fn per_pid(counts: &[(u16, usize)]) -> BTreeMap<u16, usize> {
    counts.iter().copied().collect()
}

#[test]
fn a_second_publisher_with_other_tracks_reaches_the_mounts_demuxer() {
    let s = Served::new();
    let reader = MountReader::start(&s);
    publish_ts(&s, true);
    publish_elementary(&s);
    let events = reader.finish(&s);

    assert_eq!(
        seen(&events),
        Seen {
            program_maps: 2,
            video: per_pid(&[(VIDEO_PID, UNITS), (TS_VIDEO_PID, UNITS)]),
            klv: per_pid(&[(KLV_PID, UNITS), (TS_KLV_PID, UNITS)]),
            discontinuities: BTreeMap::new(),
        }
    );
    // Every unit of the first publisher, its last access unit included,
    // comes before the program map of the second, and none of the second's
    // before it.
    let (at, pids) = second_program_map(&events);
    assert_eq!(pids, [VIDEO_PID, KLV_PID]);
    let before = seen(&events[..at]);
    assert_eq!(before.video, per_pid(&[(TS_VIDEO_PID, UNITS)]));
    assert_eq!(before.klv, per_pid(&[(TS_KLV_PID, UNITS)]));
}

#[test]
fn two_mpegts_publishers_with_different_track_lists_are_both_seen() {
    let s = Served::new();
    let reader = MountReader::start(&s);
    publish_ts(&s, false);
    publish_ts(&s, true);
    let events = reader.finish(&s);

    // The second publisher's muxer restarts the video PID's continuity
    // counter: one discontinuity there.
    assert_eq!(
        seen(&events),
        Seen {
            program_maps: 2,
            video: per_pid(&[(TS_VIDEO_PID, 2 * UNITS)]),
            klv: per_pid(&[(TS_KLV_PID, UNITS)]),
            discontinuities: per_pid(&[(TS_VIDEO_PID, 1)]),
        }
    );
    // The KLV stream exists from the second program map on. The video PID
    // stays in the program, so its unit in flight is not handed out at the
    // change: it ends where the second publisher's first unit starts.
    let (at, pids) = second_program_map(&events);
    assert_eq!(pids, [TS_VIDEO_PID, TS_KLV_PID]);
    let before = seen(&events[..at]);
    assert_eq!(before.video, per_pid(&[(TS_VIDEO_PID, UNITS - 1)]));
    assert!(before.klv.is_empty(), "{before:?}");
}

#[test]
fn a_second_publisher_with_the_same_tracks_shows_as_discontinuities() {
    let s = Served::new();
    let reader = MountReader::start(&s);
    publish_elementary(&s);
    publish_elementary(&s);
    let events = reader.finish(&s);

    // The mount re-muxes each publisher with the same tables, so the one
    // program map stands, and each PID's restarted continuity counter is one
    // discontinuity.
    assert_eq!(
        seen(&events),
        Seen {
            program_maps: 1,
            video: per_pid(&[(VIDEO_PID, 2 * UNITS)]),
            klv: per_pid(&[(KLV_PID, 2 * UNITS)]),
            discontinuities: per_pid(&[(VIDEO_PID, 1), (KLV_PID, 1)]),
        }
    );
}
