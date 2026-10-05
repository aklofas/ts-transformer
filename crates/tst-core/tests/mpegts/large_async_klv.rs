//! An async KLV PES whose first five bytes `06 0E 2B 34 02` ALSO parse as an
//! H.222.0 Metadata_AU_cell header (`cfi = Middle`,
//! `AU_cell_data_length = 0x3402 = 13 314`) was classified as a sync cell once
//! the payload reached 5 + 13 314 = 13 319 bytes, routed to the AU-cell
//! reassembler as an orphan Middle cell, and DROPPED — our own `Muxer` →
//! `Demuxer` round trip lost every ST 0601 record above ~13 KB (VMTI-sized
//! sets). Below the threshold the same bytes round-tripped.

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, MetadataKind, SamplePayload};

use crate::psi_builders::{build_pat_section, build_pmt_section, psi_packet};
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

/// Mux one `total_len`-byte UL-prefixed async KLV packet (plus a 40-byte
/// follower so the demuxer sees the first PES end), demux it, and return
/// `(matching Metadata events, NonConformant events as text)`.
fn round_trip(total_len: usize) -> (usize, Vec<String>) {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x100, VideoCodec::H264);
        prog.add_klv(0x200, KlvStreamType::PrivateData, true);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut muxer = Muxer::new(cfg).unwrap();
    let klv_handle = muxer.klv_handles()[0];
    // UL (16) + BER long-form length (3) + body.
    let body_len = total_len - 16 - 3;
    let mut klv = vec![
        0x06,
        0x0E,
        0x2B,
        0x34,
        0x02,
        0x0B,
        0x01,
        0x01,
        0x0E,
        0x01,
        0x03,
        0x01,
        0x01,
        0x00,
        0x00,
        0x00,
        0x82,
        (body_len >> 8) as u8,
        body_len as u8,
    ];
    klv.extend(core::iter::repeat_n(0x5A, body_len));
    assert_eq!(klv.len(), total_len);
    muxer
        .push_klv_to(klv_handle, &klv, Pts90khz::new(90_000), 0)
        .unwrap();
    muxer
        .push_klv_to(klv_handle, &klv[..40], Pts90khz::new(93_600), 0)
        .unwrap();
    let mut buf = vec![0u8; 188 * 2048];
    let n = muxer.pull(&mut buf);
    let mut d = Demuxer::new();
    d.feed(&buf[..n]).unwrap();
    d.flush();
    let mut meta = 0usize;
    let mut other = Vec::new();
    while let Some(ev) = d.next_event() {
        match ev {
            DemuxEvent::Metadata { kind, payload, .. } => {
                if payload.len() == total_len && matches!(kind, MetadataKind::KlvAsync) {
                    assert_eq!(payload, klv, "payload bytes must round-trip verbatim");
                    meta += 1;
                }
            }
            DemuxEvent::NonConformant { .. } => {
                other.push(format!("{ev:?}").chars().take(160).collect())
            }
            _ => {}
        }
    }
    (meta, other)
}

#[test]
fn async_klv_below_the_cell_threshold_round_trips() {
    let (meta, other) = round_trip(13_000);
    assert_eq!(meta, 1, "13 000-byte async KLV: nonconformant={other:?}");
    assert!(
        other.is_empty(),
        "a clean round trip emits no NonConformant: {other:?}"
    );
}

#[test]
fn async_klv_at_the_cell_threshold_round_trips() {
    // Exactly 5 + 0x3402: the first length that let the cell parse succeed.
    let (meta, other) = round_trip(13_319);
    assert_eq!(
        meta, 1,
        "13 319-byte async KLV must emit one Metadata event; nonconformant={other:?}"
    );
    assert!(other.is_empty(), "{other:?}");
}

#[test]
fn async_klv_well_above_the_cell_threshold_round_trips() {
    let (meta, other) = round_trip(20_000);
    assert_eq!(
        meta, 1,
        "20 000-byte async KLV must emit one Metadata event; nonconformant={other:?}"
    );
    assert!(other.is_empty(), "{other:?}");
}

/// Muxer-free: a sync-metadata PES (stream_type 0x15) whose first
/// `Metadata_AU_cell` flags byte is `0xC0` (reserved nibble `0000`, not the
/// required `1111`) is not parsed as a cell. It must still surface — as one
/// raw `SamplePayload::Unknown` sample carrying the PES payload verbatim —
/// never vanish.
#[test]
fn sync_pes_with_a_cleared_reserved_nibble_surfaces_as_unknown() {
    const PMT_PID: u16 = 0x1000;
    const KLV_PID: u16 = 0x200;
    // Cell header [service_id 0x00][seq 0x01][flags 0xC0][length 0x0004]
    // followed by its 4-byte body.
    let cell = [0x00, 0x01, 0xC0, 0x00, 0x04, 0xDE, 0xAD, 0xBE, 0xEF];

    // Bounded PES: stream_id 0xFC (metadata_stream), PTS = 90 000.
    let pts: u64 = 90_000;
    let mut pes = vec![0x00, 0x00, 0x01, 0xFC, 0x00, 0x00, 0x80, 0x80, 0x05];
    pes.extend_from_slice(&[
        0x21 | (((pts >> 30) as u8) << 1) & 0x0E,
        ((pts >> 22) & 0xFF) as u8,
        (((pts >> 14) & 0xFE) as u8) | 0x01,
        ((pts >> 7) & 0xFF) as u8,
        (((pts << 1) & 0xFE) as u8) | 0x01,
    ]);
    pes.extend_from_slice(&cell);
    let pes_len = (pes.len() - 6) as u16;
    pes[4..6].copy_from_slice(&pes_len.to_be_bytes());

    // One TS packet: PUSI, adaptation field of pure stuffing so the PES
    // ends exactly at byte 188.
    let mut pkt = vec![0xFFu8; 188];
    pkt[0] = 0x47;
    pkt[1] = 0x40 | (KLV_PID >> 8) as u8;
    pkt[2] = (KLV_PID & 0xFF) as u8;
    pkt[3] = 0x30; // adaptation field + payload, cc 0
    let af_len = 188 - 5 - pes.len();
    pkt[4] = af_len as u8;
    pkt[5] = 0x00; // no AF flags; the rest of the AF is 0xFF stuffing
    pkt[188 - pes.len()..].copy_from_slice(&pes);

    let mut stream = Vec::new();
    stream.extend(psi_packet(
        0x0000,
        &build_pat_section(0, &[(1, PMT_PID)]),
        0,
    ));
    stream.extend(psi_packet(
        PMT_PID,
        &build_pmt_section(1, KLV_PID, 0, &[(0x15, KLV_PID, &[])]),
        0,
    ));
    stream.extend(pkt);

    let mut d = Demuxer::new();
    d.feed(&stream).unwrap();
    d.flush();
    let mut unknown = Vec::new();
    let mut metadata = 0;
    while let Some(ev) = d.next_event() {
        match ev {
            DemuxEvent::Sample {
                stream,
                payload: SamplePayload::Unknown { raw, .. },
                ..
            } if stream.pid == KLV_PID => unknown.push(raw.as_slice().to_vec()),
            DemuxEvent::Metadata { .. } => metadata += 1,
            _ => {}
        }
    }
    assert_eq!(metadata, 0, "a non-cell must not be parsed as KLV");
    assert_eq!(
        unknown,
        vec![cell.to_vec()],
        "the PES payload must surface once as a raw Unknown sample"
    );
}
