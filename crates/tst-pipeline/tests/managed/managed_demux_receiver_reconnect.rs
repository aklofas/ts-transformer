//! Integration tests for [`ManagedDemuxReceiver`] reconnect-discontinuity
//! semantics. Covers three behaviours:
//!
//! 1. Connection drops mid-TS-packet; reconnect; verify next packet
//!    starts with a fresh sync and no spliced bytes.
//! 2. Reconnect discontinuity event IS surfaced to the caller.
//! 3. Reconnect during partial PES (multi-packet sample) — verify the
//!    half-assembled sample is dropped, not corrupted into the next
//!    sample.
//!
//! Uses `ScriptedRecv` (from `common`) for deterministic reconnect
//! injection — no SRT loopback, no temp files, no sleep beyond the
//! policy backoff (zero in tests).

use crate::common::{ScriptedRecv, broken, drain_mux, fast_policy, to_ts_packets};

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, SamplePayload};
use tst_core::mpegts::mux::{Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};
use tst_core::transport::{EndReason, TransportError};
use tst_pipeline::{ManagedDemuxReceiver, ManagedDemuxReceiverConfig, ManagedRecvTransport};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a 188-byte aligned TS packet on `pid` with CC `cc`.
fn ts_packet(pid: u16, cc: u8) -> [u8; 188] {
    let mut buf = [0xFFu8; 188];
    buf[0] = 0x47;
    buf[1] = 0x40 | ((pid >> 8) as u8 & 0x1F);
    buf[2] = (pid & 0xFF) as u8;
    buf[3] = 0x10 | (cc & 0x0F);
    buf
}

/// Build a 188-byte aligned TS packet with the `transport_error_indicator`
/// bit (byte 1, mask 0x80) set. Per ISO/IEC 13818-1 §2.4.3.2 each such
/// packet is dropped by the demuxer with a single
/// `DemuxEvent::NonConformant { issue: TransportErrorPacket { pid } }`
/// event. We use TEI packets as a unit-counting discriminator: each
/// packet that REACHES the demuxer produces exactly one observable
/// event, so the count of `NonConformant` events on the receiver
/// equals the count of packets that survived sync + reconnect-drop.
fn ts_packet_tei(pid: u16, cc: u8) -> [u8; 188] {
    let mut buf = [0xFFu8; 188];
    buf[0] = 0x47;
    // TEI=1 (0x80) + PUSI=1 (0x40) + pid_high
    buf[1] = 0xC0 | ((pid >> 8) as u8 & 0x1F);
    buf[2] = (pid & 0xFF) as u8;
    buf[3] = 0x10 | (cc & 0x0F);
    buf
}

fn pack_chunk(packets: &[[u8; 188]]) -> Vec<u8> {
    let mut v = Vec::with_capacity(packets.len() * 188);
    for p in packets {
        v.extend_from_slice(p);
    }
    v
}

// ---------------------------------------------------------------------------
// Test 1 — Connection drops mid-TS-packet
// ---------------------------------------------------------------------------

/// Phase-1 inner serves an aligned chunk plus a TRUNCATED final packet
/// (only the first 50 bytes of the 6th 188-byte slot). On reconnect,
/// phase-2 inner serves 5 fresh aligned packets on a different PID.
///
/// Acceptance: the post-reconnect packets must be parsed cleanly. If
/// the syncer had carried the truncated tail across the reset, the
/// first post-reconnect bytes would mis-align and the demuxer would
/// either error or emit garbage NonConformant events from bogus stream
/// IDs.
#[test]
fn connection_drops_mid_ts_packet_reconnect_resyncs_cleanly() {
    let p1_packets: Vec<[u8; 188]> = (0..5).map(|i| ts_packet(0x0100, i as u8)).collect();
    let mut p1 = pack_chunk(&p1_packets);
    // Append 50 bytes — the start of a fictional 6th packet, abruptly
    // truncated by the dead connection.
    p1.extend_from_slice(&[0x47, 0xAA, 0xBB, 0xCC]);
    p1.extend_from_slice(&[0xDE; 46]);
    assert_eq!(p1.len() - p1_packets.len() * 188, 50);

    let inner = ScriptedRecv::new(vec![p1], broken("phase 1 ended (test)")).with_max_payload(4096);

    let calls = Arc::new(AtomicU32::new(0));
    let calls_cl = calls.clone();
    let factory = Box::new(move || -> Result<ScriptedRecv, TransportError> {
        let n = calls_cl.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // Second connection: clean aligned packets on a different PID.
            let p2_packets: Vec<[u8; 188]> = (0..5).map(|i| ts_packet(0x0200, i as u8)).collect();
            Ok(ScriptedRecv::new(
                vec![pack_chunk(&p2_packets)],
                broken("phase 2 ended (test)"),
            )
            .with_max_payload(4096))
        } else {
            // Force budget exhaust on second factory attempt.
            Err(broken("no more rebuilds"))
        }
    });

    let managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(2)))
        .expect("the policy validates");
    let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());

    let mut saw_reconnect = false;
    let mut iters = 0;
    loop {
        iters += 1;
        assert!(iters < 1000, "test loop should bound");
        match rx.recv_event() {
            Ok(Some(DemuxEvent::ReconnectDiscontinuity)) => saw_reconnect = true,
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(saw_reconnect, "should surface ReconnectDiscontinuity");
    assert_eq!(rx.reconnects_count(), 1);
}

/// The reconnect splice drop is not counted: after one reconnect the
/// transport-layer `bytes_received` exceeds the demuxer-fed counter by
/// exactly the one dropped packet.
#[test]
fn managed_demux_receiver_reconnect_splice_is_not_counted() {
    let p1_packets: Vec<[u8; 188]> = (0..5).map(|i| ts_packet_tei(0x0100, i as u8)).collect();
    let mut p1 = pack_chunk(&p1_packets);
    p1.extend_from_slice(&[0x47, 0xAA, 0xBB, 0xCC]);
    p1.extend_from_slice(&[0xDE; 46]);

    let inner = ScriptedRecv::new(vec![p1], broken("phase 1 ended (test)")).with_max_payload(4096);
    let calls = Arc::new(AtomicU32::new(0));
    let calls_cl = calls.clone();
    let factory = Box::new(move || -> Result<ScriptedRecv, TransportError> {
        let n = calls_cl.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            let p2: Vec<[u8; 188]> = (0..5).map(|i| ts_packet_tei(0x0200, i as u8)).collect();
            Ok(
                ScriptedRecv::new(vec![pack_chunk(&p2)], broken("phase 2 ended (test)"))
                    .with_max_payload(4096),
            )
        } else {
            Err(broken("no more rebuilds"))
        }
    });
    let managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(2)))
        .expect("the policy validates");
    let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());
    let h = rx.ts_bytes_demuxed_handle();

    let mut fed = 0u64;
    let mut iters = 0;
    loop {
        iters += 1;
        assert!(iters < 1000, "test loop should bound");
        match rx.recv_event() {
            Ok(Some(DemuxEvent::NonConformant {
                issue: tst_core::mpegts::demux::NonConformantIssue::TransportErrorPacket { .. },
                ..
            })) => fed += 1,
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    assert!(fed >= 5, "fed = {fed}");
    assert_eq!(rx.reconnects_count(), 1);
    assert_eq!(rx.ts_bytes_demuxed(), 188 * fed);
    assert_eq!(rx.stats().bytes_received - rx.ts_bytes_demuxed(), 188);
    assert_eq!(h.get(), rx.ts_bytes_demuxed());
}

// ---------------------------------------------------------------------------
// Test 2 — Reconnect discontinuity event IS surfaced to caller
// ---------------------------------------------------------------------------

/// Equivalent assertion to the unit-test in lib, exposed at the
/// integration boundary so external consumers of `tst_pipeline`'s
/// public types see this guarantee verified.
#[test]
fn reconnect_event_is_observable_via_public_api() {
    let phase1 = pack_chunk(
        &(0..5)
            .map(|i| ts_packet(0x0300, i as u8))
            .collect::<Vec<_>>(),
    );
    let inner = ScriptedRecv::new(vec![phase1], broken("phase 1 ended")).with_max_payload(4096);

    let calls = Arc::new(AtomicU32::new(0));
    let calls_cl = calls.clone();
    let factory = Box::new(move || -> Result<ScriptedRecv, TransportError> {
        let n = calls_cl.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            let phase2 = pack_chunk(
                &(0..3)
                    .map(|i| ts_packet(0x0301, i as u8))
                    .collect::<Vec<_>>(),
            );
            Ok(ScriptedRecv::new(vec![phase2], broken("phase 2 ended")).with_max_payload(4096))
        } else {
            Err(broken("stop"))
        }
    });

    let managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(2)))
        .expect("the policy validates");
    let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());

    let mut events: Vec<&'static str> = Vec::new();
    let mut iters = 0;
    loop {
        iters += 1;
        assert!(iters < 1000);
        match rx.recv_event() {
            Ok(Some(DemuxEvent::ReconnectDiscontinuity)) => events.push("reconnect"),
            Ok(Some(DemuxEvent::ProgramMap(_))) => events.push("program_map"),
            Ok(Some(DemuxEvent::Sample { .. })) => events.push("sample"),
            Ok(Some(DemuxEvent::Metadata { .. })) => events.push("metadata"),
            Ok(Some(DemuxEvent::Discontinuity { .. })) => events.push("discontinuity"),
            Ok(Some(DemuxEvent::NonConformant { .. })) => events.push("nonconformant"),
            Ok(Some(_)) => events.push("other"),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    // The "reconnect" marker MUST appear in the event sequence at
    // least once. Exact position isn't asserted because the
    // synthetic stream has no PMT (so no Sample events) and the
    // event count is dominated by the reconnect itself.
    assert!(
        events.contains(&"reconnect"),
        "ReconnectDiscontinuity event was not surfaced: events = {events:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — A reconnect inside a PES
// ---------------------------------------------------------------------------

/// Payload bytes of one video access unit: four TS packets.
const AU_LEN: usize = 600;

/// An access unit whose payload is `AU_LEN` copies of `marker`, so a sample
/// says which unit, and so which connection, each of its bytes came from.
fn marked_au(marker: u8) -> Vec<u8> {
    let mut au = vec![0x00, 0x00, 0x00, 0x01, 0x65];
    au.resize(5 + AU_LEN, marker);
    au
}

/// One connection's stream from its own muxer: PAT and PMT before every
/// access unit, one unit per marker.
fn marked_stream(markers: &[u8]) -> Vec<[u8; 188]> {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.psi_interval_ms(10);
    let mut mux = Muxer::new(b.build().unwrap()).unwrap();
    let mut out = Vec::new();
    for (i, &marker) in markers.iter().enumerate() {
        mux.push_video(&marked_au(marker), Pts90khz::new(i as i64 * 3003), true)
            .unwrap();
        out.extend(to_ts_packets(&drain_mux(&mut mux)));
    }
    out
}

/// What the tests below read off an event: a sample's distinct marker bytes
/// and how many marker bytes it holds, or the event's kind.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    ProgramMap,
    Reconnect,
    Sample {
        markers: Vec<u8>,
        marker_bytes: usize,
    },
    Other,
}

const MARKERS: [u8; 6] = [0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6];

fn seen(e: &DemuxEvent) -> Seen {
    match e {
        DemuxEvent::ProgramMap(_) => Seen::ProgramMap,
        DemuxEvent::ReconnectDiscontinuity => Seen::Reconnect,
        DemuxEvent::Sample {
            payload: SamplePayload::Video { raw, .. },
            ..
        } => {
            let mut markers: Vec<u8> = raw
                .iter()
                .copied()
                .filter(|b| MARKERS.contains(b))
                .collect();
            let marker_bytes = markers.len();
            markers.sort_unstable();
            markers.dedup();
            Seen::Sample {
                markers,
                marker_bytes,
            }
        }
        _ => Seen::Other,
    }
}

/// A whole access unit of one marker.
fn whole(marker: u8) -> Seen {
    Seen::Sample {
        markers: vec![marker],
        marker_bytes: AU_LEN,
    }
}

/// Every event of a managed receiver over `first` and then `second`, until
/// the reconnect budget gives up.
fn events_over_two_connections(first: ScriptedRecv, second: ScriptedRecv) -> Vec<Seen> {
    let mut second = Some(second);
    let factory = Box::new(move || second.take().ok_or_else(|| broken("nobody answers")));
    let managed = ManagedRecvTransport::new(first, factory, fast_policy(Some(2)))
        .expect("the policy validates");
    let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());
    let mut events = Vec::new();
    while let Some(e) = rx.recv_event().expect("a give-up is a clean end") {
        events.push(seen(&e));
        assert!(events.len() < 1000, "the receiver never ended");
    }
    assert_eq!(rx.reconnects_count(), 1);
    events
}

/// The connection dies with an access unit half received: two of unit B's
/// four packets, behind a real PAT and PMT and a whole unit A. The next
/// connection brings units C, D and E from a muxer of its own.
///
/// Nothing may be built from bytes of both connections. The half of B is
/// dropped: it is neither delivered nor closed by the next connection's
/// first unit start. The program of the old connection is forgotten, so the
/// new connection's PMT is announced again, and C, which arrives before the
/// first PAT that survives the reconnect, is the documented loss.
#[test]
fn a_reconnect_inside_a_pes_delivers_no_unit_built_from_both_connections() {
    let mut old = marked_stream(&[0xA1, 0xB2]);
    let b_starts = old.len() - 4;
    assert!(
        old[b_starts][1] & 0x40 != 0 && old[b_starts + 1..].iter().all(|p| p[1] & 0x40 == 0),
        "test setup: unit B is the last four packets"
    );
    old.truncate(b_starts + 2);
    let new = marked_stream(&[0xC3, 0xD4, 0xE5]);

    let events = events_over_two_connections(
        ScriptedRecv::packets(old, broken("the link died")),
        ScriptedRecv::packets(new, broken("the link died")),
    );

    assert_eq!(
        events,
        [
            Seen::ProgramMap,
            whole(0xA1),
            Seen::Reconnect,
            Seen::ProgramMap,
            whole(0xD4),
            whole(0xE5),
        ]
    );
}

/// With reads of seven packets and a connection that dies inside a TS
/// packet: every sample before the marker holds bytes of the old connection
/// only, every sample after it bytes of the new one only, and each side has
/// one. A media event of the new connection ahead of the marker, or a unit
/// spliced across it, fails here.
#[test]
fn no_media_event_precedes_the_reconnect_marker() {
    let mut old = pack_chunk(&marked_stream(&[0xA1, 0xB2]));
    // The start of one more packet: the dead tail the syncer is left with.
    old.extend_from_slice(&marked_stream(&[0xF6])[2][..100]);
    let new = pack_chunk(&marked_stream(&[0xC3, 0xD4, 0xE5, 0xF6]));
    let reads = |bytes: &[u8]| -> Vec<Vec<u8>> { bytes.chunks(1316).map(<[u8]>::to_vec).collect() };

    let events = events_over_two_connections(
        ScriptedRecv::new(reads(&old), broken("the link died")),
        ScriptedRecv::new(reads(&new), broken("the link died")),
    );

    let markers: Vec<usize> = (0..events.len())
        .filter(|&i| events[i] == Seen::Reconnect)
        .collect();
    assert_eq!(markers.len(), 1, "{events:?}");
    let samples = |side: &[Seen]| -> Vec<Vec<u8>> {
        side.iter()
            .filter_map(|e| match e {
                Seen::Sample { markers, .. } => Some(markers.clone()),
                _ => None,
            })
            .collect()
    };
    let (before, after) = (
        samples(&events[..markers[0]]),
        samples(&events[markers[0]..]),
    );
    assert!(!before.is_empty() && !after.is_empty(), "{events:?}");
    assert!(
        before.iter().flatten().all(|m| [0xA1, 0xB2].contains(m)),
        "a sample ahead of the marker holds bytes of the new connection: {events:?}"
    );
    assert!(
        after
            .iter()
            .flatten()
            .all(|m| [0xC3, 0xD4, 0xE5, 0xF6].contains(m)),
        "a sample behind the marker holds bytes of the old connection: {events:?}"
    );
    assert!(
        before.iter().chain(&after).all(|m| m.len() == 1),
        "a sample holds bytes of two units: {events:?}"
    );
}

/// Which TEI packets reached the demuxer, by PID, and whether the marker
/// came: everything else a test below sees.
fn tei_pids_and_markers(events: &[DemuxEvent]) -> Vec<String> {
    events
        .iter()
        .map(|e| match e {
            DemuxEvent::NonConformant {
                issue: tst_core::mpegts::demux::NonConformantIssue::TransportErrorPacket { pid },
                ..
            } => format!("tei {pid:#06x}"),
            DemuxEvent::ReconnectDiscontinuity => "reconnect".to_string(),
            other => format!("{other:?}"),
        })
        .collect()
}

/// A reconnect is not an end. Three packets of the old connection are held
/// by the syncer when it dies; the reconnect succeeds, and they are dropped
/// with the first packet read across it, never released.
#[test]
fn a_reconnect_drops_the_held_packets_and_releases_nothing() {
    let old: Vec<[u8; 188]> = (0..3).map(|i| ts_packet_tei(0x0100, i)).collect();
    let new: Vec<[u8; 188]> = (0..7).map(|i| ts_packet_tei(0x0200, i)).collect();
    let inner =
        ScriptedRecv::new([pack_chunk(&old)], broken("the link died")).with_max_payload(4096);
    let mut second = Some(
        ScriptedRecv::new(
            [pack_chunk(&new[..2]), pack_chunk(&new[2..])],
            broken("the link died"),
        )
        .with_max_payload(4096),
    );
    let factory = Box::new(move || second.take().ok_or_else(|| broken("nobody answers")));
    let managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(2)))
        .expect("the policy validates");
    let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());

    let mut events = Vec::new();
    while let Some(e) = rx.recv_event().expect("a give-up is a clean end") {
        events.push(e);
    }
    // The first read of the new connection completes a lock over old and new
    // bytes; the packet it yields is dropped with everything buffered. The
    // second read's five packets are the new connection's alone.
    let mut expected = vec!["reconnect".to_string()];
    expected.extend(std::iter::repeat_n("tei 0x0200".to_string(), 5));
    assert_eq!(tei_pids_and_markers(&events), expected);
}

/// A reconnect and the end of the stream with no delivered packet between
/// them, in two shapes.
///
/// The new connection completes one packet and ends: that packet is the one
/// read across the reconnect, so it is dropped, with the half unit the old
/// connection left; the marker, then the end.
///
/// The new connection delivers nothing at all (the reconnect and the end
/// come out of one read) while the syncer holds two packets of the old one
/// behind damage: the end releases them, but they were held across a
/// reconnect, so they are dropped like any packet read across one. Nothing
/// of a new connection was read: no marker, and the unit the old connection
/// had completed is still flushed, as it was before the end released
/// anything.
#[test]
fn a_reconnect_then_an_end_in_one_read_delivers_nothing_spliced() {
    let run = |old: Vec<u8>, new: Vec<Vec<u8>>| {
        let inner = ScriptedRecv::new([old], broken("the link died")).with_max_payload(4096);
        let mut second =
            Some(ScriptedRecv::new(new, broken("the link died")).with_max_payload(4096));
        let factory = Box::new(move || second.take().ok_or_else(|| broken("nobody answers")));
        let managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(2)))
            .expect("the policy validates");
        let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());
        let mut events = Vec::new();
        while let Some(e) = rx.recv_event().expect("a give-up is a clean end") {
            events.push(seen(&e));
        }
        assert_eq!(rx.reconnects_count(), 1);
        events
    };
    let unit_a = pack_chunk(&marked_stream(&[0xA1]));

    let one_packet = pack_chunk(&[ts_packet_tei(0x0200, 0)]);
    assert_eq!(
        run(unit_a.clone(), vec![one_packet]),
        [Seen::ProgramMap, Seen::Reconnect]
    );

    let mut old = unit_a;
    old.extend_from_slice(&[0xAA; 10]);
    old.extend_from_slice(&pack_chunk(&[
        ts_packet_tei(0x0300, 0),
        ts_packet_tei(0x0300, 1),
    ]));
    assert_eq!(run(old, vec![]), [Seen::ProgramMap, whole(0xA1)]);
}

// ---------------------------------------------------------------------------
// Test 4 — Clean reconnect still drops the first post-reconnect packet
// ---------------------------------------------------------------------------

/// Documents the `# Data-loss budget on reconnect` contract on
/// [`ManagedDemuxReceiver`]: at least one packet is unconditionally
/// dropped on every reconnect, EVEN WHEN the boundary falls cleanly on
/// a 188-byte packet edge with no dead-tail bytes left in the syncer's
/// ring buffer.
///
/// Counting mechanism: every input packet has the
/// `transport_error_indicator` bit set (TEI). Per ISO/IEC 13818-1
/// §2.4.3.2 the demuxer drops TEI packets with a single
/// `DemuxEvent::NonConformant { issue: TransportErrorPacket { .. } }`
/// event per packet — so the count of `NonConformant` events on the
/// receiver-side equals the count of packets that survived sync +
/// reconnect-drop and reached the demuxer.
///
/// Setup: N TEI packets in phase 1 (chunk ends EXACTLY on a packet
/// boundary — no dead-tail bytes). Inner exhausts with `Broken`,
/// triggering reconnect. The second connection serves a long sequence of more TEI
/// packets across multiple `recv_bytes` chunks so the receiver has
/// enough bytes to (a) lock once, get the first packet dropped by the
/// shell, then (b) re-lock cleanly on subsequent bytes and emit a
/// stream of events.
///
/// Acceptance: a `ReconnectDiscontinuity` event surfaces AND the count
/// of phase-2 events is STRICTLY LESS than the number of phase-2
/// packets fed. The strict inequality is the contract: if the shell
/// ever stopped dropping the first post-reconnect packet on clean
/// boundaries, the phase-2 event count would equal the phase-2 packet
/// count and this assertion would fail.
///
/// Why a strict inequality rather than an exact count: when the
/// reconnect is detected the syncer's reset clears not only the
/// just-emitted post-reconnect packet but also any bytes the syncer
/// had pulled from the transport but not yet drained as aligned
/// packets (per the `Data-loss budget on reconnect` rustdoc section
/// on [`ManagedDemuxReceiver`]). The exact number of dropped packets
/// depends on `recv_bytes` chunking which is transport-specific
/// (typically one SRT payload = ~7 TS packets). Asserting strict
/// less-than is robust across both single-chunk and multi-chunk
/// recv_bytes patterns.
#[test]
fn clean_reconnect_drops_first_post_reconnect_packet() {
    // First connection: 8 TEI packets on PID 0x100. Chunk length is exactly
    // 8 * 188 = 1504 bytes — no fractional / dead-tail bytes appended.
    let n: usize = 8;
    let p1_packets: Vec<[u8; 188]> = (0..n).map(|i| ts_packet_tei(0x100, i as u8)).collect();
    let p1_chunk = pack_chunk(&p1_packets);
    assert_eq!(
        p1_chunk.len(),
        n * 188,
        "phase 1 chunk must end on a packet boundary (no dead-tail)"
    );

    let inner = ScriptedRecv::new(vec![p1_chunk], broken("phase 1 ended (clean boundary)"))
        .with_max_payload(4096);

    // Second connection: M TEI packets delivered as TWO clean-aligned chunks. The
    // shell's reset_sync clears the syncer buffer at reconnect time so
    // bytes already pulled-but-not-emitted in the first chunk are also
    // lost; the second chunk lets the syncer re-lock and drain a
    // measurable tail of events.
    let m: usize = 16;
    let calls = Arc::new(AtomicU32::new(0));
    let calls_cl = calls.clone();
    let factory = Box::new(move || -> Result<ScriptedRecv, TransportError> {
        let n = calls_cl.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            let p2_first: Vec<[u8; 188]> = (0..8).map(|i| ts_packet_tei(0x200, i as u8)).collect();
            let p2_second: Vec<[u8; 188]> = (0..8)
                .map(|i| ts_packet_tei(0x200, (i + 8) as u8))
                .collect();
            let chunk_a = pack_chunk(&p2_first);
            let chunk_b = pack_chunk(&p2_second);
            assert_eq!(chunk_a.len() % 188, 0);
            assert_eq!(chunk_b.len() % 188, 0);
            Ok(
                ScriptedRecv::new(vec![chunk_a, chunk_b], broken("phase 2 ended"))
                    .with_max_payload(4096),
            )
        } else {
            Err(broken("no more rebuilds"))
        }
    });

    let managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(2)))
        .expect("the policy validates");
    let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());

    let mut saw_reconnect = false;
    // Track NonConformant events by PID so phase-1 (PID 0x100) and
    // phase-2 (PID 0x200) counts stay separable — the contract under
    // test concerns the phase-2 count specifically.
    let mut phase1_count = 0usize;
    let mut phase2_count = 0usize;
    let mut iters = 0;
    loop {
        iters += 1;
        assert!(iters < 1000, "test loop should bound");
        match rx.recv_event() {
            Ok(Some(DemuxEvent::ReconnectDiscontinuity)) => {
                saw_reconnect = true;
            }
            Ok(Some(DemuxEvent::NonConformant {
                issue: tst_core::mpegts::demux::NonConformantIssue::TransportErrorPacket { pid },
                ..
            })) => match pid {
                0x100 => phase1_count += 1,
                0x200 => phase2_count += 1,
                _ => {}
            },
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }
    assert!(saw_reconnect, "ReconnectDiscontinuity must surface");
    assert_eq!(rx.reconnects_count(), 1);

    // Sanity: phase-1 packets all reached the demuxer (no drop applies
    // before the first reconnect).
    assert_eq!(
        phase1_count, n,
        "phase 1 events should match all {} input packets",
        n
    );

    // The contract: even with a clean (no dead-tail) reconnect boundary,
    // at least one phase-2 packet is dropped by the shell. If the shell
    // ever stopped dropping on clean boundaries, phase2_count would
    // equal `m` and this assertion would fail — surfacing the contract
    // change for review.
    assert!(
        phase2_count < m,
        "expected fewer than {} phase-2 events (some dropped by reconnect handler); got {}",
        m,
        phase2_count
    );
    // Sanity: at least one phase-2 packet survived (so we know we're
    // measuring the drop, not a total post-reconnect stall).
    assert!(
        phase2_count > 0,
        "phase 2 should have emitted some events after re-lock; got 0"
    );
}

// ---------------------------------------------------------------------------
// Test 5 — EndReason: reconnect-budget exhaustion, cancel, live, and
// first-writer-wins under repeated terminal polling.
// ---------------------------------------------------------------------------

/// A peer that dies and never comes back (factory always fails,
/// `max_attempts=Some(1)`) budget-exhausts. On the managed-SRT path this
/// is the ONLY way `recv_event` reaches `Ok(None)` — record accordingly.
#[test]
fn end_reason_reconnect_exhausted_on_budget_giveup() {
    let packets: Vec<[u8; 188]> = (0..5).map(|i| ts_packet(0x0100, i as u8)).collect();
    let inner = ScriptedRecv::new(vec![pack_chunk(&packets)], broken("peer gone (test)"))
        .with_max_payload(4096);
    // Factory never succeeds — with max_attempts=1 the budget exhausts on
    // the first (and only) reconnect attempt.
    let factory =
        Box::new(|| -> Result<ScriptedRecv, TransportError> { Err(broken("peer never returns")) });
    let managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(1)))
        .expect("the policy validates");
    let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());

    assert_eq!(
        rx.end_reason_handle().get(),
        None,
        "must be unset before the stream ends"
    );

    let mut iters = 0;
    loop {
        iters += 1;
        assert!(iters < 1000, "test loop should bound");
        match rx.recv_event() {
            Ok(None) => break,
            Ok(Some(_)) => {}
            Err(_) => break,
        }
    }
    assert_eq!(
        rx.end_reason_handle().get(),
        Some(EndReason::ReconnectExhausted),
        "budget give-up must record ReconnectExhausted"
    );
}

/// A caller-initiated cancel (via the cross-thread cancel handle) surfaces
/// as a `Closed`-kind error and must record `Cancelled` — not
/// `ReconnectExhausted`, even though both terminate the receiver.
#[test]
fn end_reason_cancelled_on_caller_cancel() {
    let packets: Vec<[u8; 188]> = (0..5).map(|i| ts_packet(0x0100, i as u8)).collect();
    let inner = ScriptedRecv::new(vec![pack_chunk(&packets)], broken("unused (test)"))
        .with_max_payload(4096);
    let factory =
        Box::new(|| -> Result<ScriptedRecv, TransportError> { Err(broken("unused (test)")) });
    let managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(5)))
        .expect("the policy validates");
    let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());

    // Obtain-before-move pattern: grab both handles up front, exactly as
    // a C binding would before boxing the receiver.
    let end_reason = rx.end_reason_handle();
    let cancel = rx
        .cancel_handle()
        .expect("managed transport is cancellable");

    // Cancel immediately, before any bytes are pulled — the very first
    // recv_bytes call inside recv_event must observe ExplicitClose.
    cancel.cancel();

    let mut iters = 0;
    loop {
        iters += 1;
        assert!(iters < 1000, "test loop should bound");
        match rx.recv_event() {
            Ok(None) | Err(_) => break,
            Ok(Some(_)) => {}
        }
    }
    assert_eq!(
        end_reason.get(),
        Some(EndReason::Cancelled),
        "caller-initiated cancel must record Cancelled"
    );
}

/// While the stream is live and flowing, `end_reason_handle().get()` must
/// stay `None` — no terminal condition has been observed yet.
#[test]
fn end_reason_none_while_live() {
    // TEI packets so each one guarantees a NonConformant event without
    // needing a PAT/PMT fixture (see `ts_packet_tei`'s doc comment above).
    // At least 5 packets: the syncer needs 4 confirming sync bytes to
    // lock before it emits anything (shorter streams yield a silent
    // clean EOF with zero events).
    let packets: Vec<[u8; 188]> = (0..6).map(|i| ts_packet_tei(0x0100, i as u8)).collect();
    let inner = ScriptedRecv::new(vec![pack_chunk(&packets)], broken("unused (test)"))
        .with_max_payload(4096);
    let factory =
        Box::new(|| -> Result<ScriptedRecv, TransportError> { Err(broken("unused (test)")) });
    let managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(5)))
        .expect("the policy validates");
    let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());

    match rx.recv_event() {
        Ok(Some(_)) => {}
        other => panic!("expected a live event, got {other:?}"),
    }
    assert_eq!(
        rx.end_reason_handle().get(),
        None,
        "no terminal condition has occurred yet"
    );
}

/// First-writer-wins under repeated terminal polling: once
/// `ReconnectExhausted` is latched, further `recv_event` calls on the
/// already-terminal receiver (each of which re-observes the same
/// `EndOfStream`-kind condition and attempts to record again) must not
/// change the recorded reason.
#[test]
fn end_reason_first_writer_wins_across_repeated_terminal_calls() {
    let packets: Vec<[u8; 188]> = (0..5).map(|i| ts_packet(0x0100, i as u8)).collect();
    let inner = ScriptedRecv::new(vec![pack_chunk(&packets)], broken("peer gone (test)"))
        .with_max_payload(4096);
    let factory =
        Box::new(|| -> Result<ScriptedRecv, TransportError> { Err(broken("peer never returns")) });
    let managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(1)))
        .expect("the policy validates");
    let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());

    // Drive to the first terminal Ok(None).
    let mut iters = 0;
    loop {
        iters += 1;
        assert!(iters < 1000, "test loop should bound");
        match rx.recv_event() {
            Ok(None) => break,
            Ok(Some(_)) => {}
            Err(_) => break,
        }
    }
    assert_eq!(
        rx.end_reason_handle().get(),
        Some(EndReason::ReconnectExhausted)
    );

    // Poll several more times on the now-terminally-closed receiver. Each
    // call re-enters the same EndOfStream-kind branch and calls
    // `record()` again — the value must stay exactly what it was.
    for _ in 0..3 {
        let _ = rx.recv_event();
        assert_eq!(
            rx.end_reason_handle().get(),
            Some(EndReason::ReconnectExhausted),
            "repeated terminal observation must not change the latched reason"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 6 — `reconnecting()` is TRUE during an outage, FALSE once the fresh
// inner is installed, and latches TRUE again after the budget is exhausted.
// ---------------------------------------------------------------------------

/// Observes the transient `reconnecting == true` state that
/// [`ManagedRecvTransport`] holds while its inner is absent.
///
/// Why a signaling harness: the reconnect loop runs on the caller's
/// thread inside `recv_bytes`, and the synchronous factories used by
/// the tests above return immediately — the flag flips true and back
/// to false inside ONE `recv_event` call, so no other thread can catch
/// it set. Here the factory BLOCKS on a channel until a watcher thread
/// has seen the flag through `reconnecting_handle()`, then releases it.
/// That turns a timing race into a handshake: the window cannot close
/// until the watcher says it has looked.
///
/// Sequence:
/// 1. main drives `recv_event`; the phase-1 inner exhausts `Broken`,
///    the decorator drops it, stores `reconnecting = true`, and calls
///    the factory — which blocks on `gate_rx` (still on main's thread).
/// 2. the watcher polls the handle until it reads `true`, snapshots
///    `reconnects_handle()` at that instant (must still be 0 — nothing
///    has been rebuilt yet), then sends on `gate_tx`.
/// 3. the factory returns the phase-2 inner; the decorator stores
///    `reconnecting = false` and bumps the counter. The next
///    `recv_event` surfaces `ReconnectDiscontinuity` after exactly one
///    `recv_bytes` on the fresh inner (its first chunk locks the syncer
///    outright), so the phase-2 inner cannot have exhausted yet and
///    main asserts `reconnecting() == false` deterministically.
/// 4. phase 2 exhausts; the factory fails on every further call; the
///    budget gives up → `Ok(None)`. The flag stays latched `true` with
///    `is_alive() == false` — the documented give-up contract.
///
/// The watcher's poll loop has a deadline and ALWAYS releases the gate
/// on exit, so a regression that never sets the flag fails the
/// `observed` assertion instead of wedging the test.
#[test]
fn reconnecting_flag_true_during_outage_false_after_rebuild_latched_after_giveup() {
    let phase1: Vec<[u8; 188]> = (0..8).map(|i| ts_packet_tei(0x100, i as u8)).collect();
    let inner = ScriptedRecv::new(vec![pack_chunk(&phase1)], broken("phase 1 ended (test)"))
        .with_max_payload(4096);

    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    let calls = Arc::new(AtomicU32::new(0));
    let calls_cl = calls.clone();
    let factory = Box::new(move || -> Result<ScriptedRecv, TransportError> {
        let n = calls_cl.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // Hold the outage open until the watcher has seen
            // `reconnecting == true`. A dropped sender (watcher gave
            // up) also returns here — the receiver loop never wedges.
            let _ = gate_rx.recv();
            let a: Vec<[u8; 188]> = (0..8).map(|i| ts_packet_tei(0x200, i as u8)).collect();
            let b: Vec<[u8; 188]> = (0..8)
                .map(|i| ts_packet_tei(0x200, (i + 8) as u8))
                .collect();
            Ok(ScriptedRecv::new(
                vec![pack_chunk(&a), pack_chunk(&b)],
                broken("phase 2 ended (test)"),
            )
            .with_max_payload(4096))
        } else {
            Err(broken("peer gone for good (test)"))
        }
    });

    let managed = ManagedRecvTransport::new(inner, factory, fast_policy(Some(2)))
        .expect("the policy validates");
    let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());

    // Obtain-before-drive: the same shape a binding uses before boxing
    // the receiver, and the only way another thread can read the flag
    // while this thread is blocked inside `recv_event`.
    let reconnecting = rx.reconnecting_handle();
    let reconnects = rx.reconnects_handle();
    assert!(
        !rx.reconnecting(),
        "fresh receiver must not be reconnecting"
    );

    let watcher = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut observed = None;
        while std::time::Instant::now() < deadline {
            if reconnecting.load(Ordering::Acquire) {
                observed = Some(reconnects.load(Ordering::Acquire));
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        // Release the gate whether or not the flag was seen so the
        // receiver loop always terminates; `observed` carries the verdict.
        let _ = gate_tx.send(());
        observed
    });

    let mut saw_reconnect = false;
    let mut iters = 0;
    loop {
        iters += 1;
        assert!(iters < 1000, "test loop should bound");
        match rx.recv_event() {
            Ok(Some(DemuxEvent::ReconnectDiscontinuity)) => {
                saw_reconnect = true;
                // Step 3: the fresh inner is installed and has served
                // exactly one chunk — the flag must already be clear.
                assert!(
                    !rx.reconnecting(),
                    "flag must be clear once the fresh inner is installed"
                );
                assert_eq!(rx.reconnects_count(), 1);
                assert!(rx.is_alive());
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => panic!("unexpected receiver error: {e:?}"),
        }
    }
    let observed = watcher.join().expect("watcher thread panicked");

    // Step 2: the watcher saw the transient true state, and at that
    // instant no rebuild had succeeded yet.
    assert_eq!(
        observed,
        Some(0),
        "watcher must observe reconnecting == true before any rebuild succeeds"
    );
    assert!(saw_reconnect, "ReconnectDiscontinuity must surface");

    // Step 4: budget exhausted — the flag latches true and stays there.
    assert!(
        rx.reconnecting(),
        "flag must stay latched after the reconnect budget is exhausted"
    );
    assert!(!rx.is_alive(), "budget exhaustion closes the receiver");
    assert_eq!(
        rx.end_reason_handle().get(),
        Some(EndReason::ReconnectExhausted)
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "one gated rebuild + two failed attempts before give-up"
    );
}

/// A managed transport that has already reconnected once when the shell
/// takes it over (its owner read from it first): the shell starts from the
/// counter's current value, so it emits no `ReconnectDiscontinuity` and
/// drops nothing.
#[test]
fn a_transport_that_reconnected_before_it_was_wrapped_starts_clean() {
    use tst_core::transport::RecvTransport;

    const PACKETS: usize = 12;
    let calls = Arc::new(AtomicU32::new(0));
    let factory = {
        let calls = Arc::clone(&calls);
        Box::new(move || -> Result<ScriptedRecv, TransportError> {
            if calls.fetch_add(1, Ordering::SeqCst) > 0 {
                return Err(broken("no second rebuild"));
            }
            // The rebuilt connection: four bytes the owner reads itself,
            // then the stream, one packet per read.
            let mut chunks = vec![b"peek".to_vec()];
            chunks.extend((0..PACKETS).map(|i| ts_packet_tei(0x100, i as u8).to_vec()));
            Ok(ScriptedRecv::new(chunks, TransportError::Closed).with_max_payload(188))
        })
    };
    let mut managed = ManagedRecvTransport::new(
        ScriptedRecv::failing(broken("the first connection is dead")).with_alive(true),
        factory,
        fast_policy(Some(1)),
    )
    .expect("the policy validates");

    let mut peek = [0u8; 188];
    assert_eq!(managed.recv_bytes(&mut peek), Ok(4));
    assert_eq!(managed.reconnects_count(), 1, "one rebuild before the wrap");

    let mut rx = ManagedDemuxReceiver::new(managed, ManagedDemuxReceiverConfig::default());
    let mut discontinuities = 0usize;
    let mut packets = 0usize;
    for _ in 0..1000 {
        match rx.recv_event() {
            Ok(Some(DemuxEvent::ReconnectDiscontinuity)) => discontinuities += 1,
            Ok(Some(DemuxEvent::NonConformant {
                issue: tst_core::mpegts::demux::NonConformantIssue::TransportErrorPacket { .. },
                ..
            })) => packets += 1,
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    assert_eq!(
        (discontinuities, packets),
        (0, PACKETS),
        "no reconnect happened under the shell: no marker, no packet dropped"
    );
}
