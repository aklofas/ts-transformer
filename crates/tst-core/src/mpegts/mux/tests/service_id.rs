//! The synchronous-KLV service id a push names against the ones the PMT declares.

use super::*;
use crate::mpegts::common::Pts90khz;
use crate::mpegts::descriptors::metadata_klva;
use crate::test_prelude::*;

const KLV: &[u8] = &[0x06, 0x0E, 0x2B, 0x34, 0x01, 0x00];

/// One program: H.264 on 0x0100, KLV of `stream_type` on 0x0101 with the
/// caller descriptors `descs` (empty: the automatic ones).
fn one_klv_config(stream_type: KlvStreamType, descs: Vec<Vec<u8>>) -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x0100, VideoCodec::H264);
    prog.add_klv(0x0101, stream_type, true);
    if !descs.is_empty() {
        prog.stream_descriptors_for_klv(0, descs).unwrap();
    }
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().unwrap()
}

fn refused(pid: u16, service_id: u8) -> Result<(), MuxError> {
    Err(MuxError::UndeclaredMetadataService { pid, service_id })
}

#[test]
fn sync_klv_rejects_undeclared_service_id() {
    let mut mux = Muxer::new(one_klv_config(KlvStreamType::SynchronousMetadata, vec![])).unwrap();
    mux.push_klv(KLV, Pts90khz::new(90_000), 0).unwrap();

    let pending = mux.pending_packets();
    let (clock, psi) = (mux.slots[0].clock, mux.slots[0].psi_last);
    assert_eq!(
        mux.push_klv(KLV, Pts90khz::new(900_000), 3),
        refused(0x0101, 3)
    );
    assert_eq!(
        mux.pending_packets(),
        pending,
        "a refused push queued packets"
    );
    assert_eq!(mux.slots[0].clock, clock, "a refused push moved the clock");
    assert_eq!(
        mux.slots[0].psi_last, psi,
        "a refused push moved the PSI anchor"
    );
}

#[test]
fn sync_klv_accepts_every_declared_service() {
    let cfg = one_klv_config(
        KlvStreamType::SynchronousMetadata,
        vec![metadata_klva(3), metadata_klva(4)],
    );
    let mut mux = Muxer::new(cfg).unwrap();
    for id in [3, 4] {
        mux.push_klv(KLV, Pts90khz::new(90_000), id).unwrap();
    }
    for id in [0, 5] {
        assert_eq!(
            mux.push_klv(KLV, Pts90khz::new(90_000), id),
            refused(0x0101, id)
        );
    }
}

#[test]
fn async_klv_ignores_service_id() {
    let mut mux = Muxer::new(one_klv_config(KlvStreamType::PrivateData, vec![])).unwrap();
    for id in [0, 3, 255] {
        mux.push_klv(KLV, Pts90khz::new(90_000), id).unwrap();
    }
}

#[test]
fn the_declared_services_follow_a_reconfigure() {
    let mut mux = Muxer::new(one_klv_config(KlvStreamType::SynchronousMetadata, vec![])).unwrap();
    mux.push_klv(KLV, Pts90khz::new(90_000), 0).unwrap();

    // The surviving stream now declares service 5 only.
    mux.reconfigure(one_klv_config(
        KlvStreamType::SynchronousMetadata,
        vec![metadata_klva(5)],
    ))
    .unwrap();
    assert_eq!(
        mux.push_klv(KLV, Pts90khz::new(93_000), 0),
        refused(0x0101, 0)
    );
    mux.push_klv(KLV, Pts90khz::new(93_000), 5).unwrap();

    // A second synchronous stream joins and declares service 7.
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x0100, VideoCodec::H264);
    prog.add_klv(0x0101, KlvStreamType::SynchronousMetadata, true);
    prog.add_klv(0x0102, KlvStreamType::SynchronousMetadata, true);
    prog.stream_descriptors_for_klv(0, vec![metadata_klva(5)])
        .unwrap();
    prog.stream_descriptors_for_klv(1, vec![metadata_klva(7)])
        .unwrap();
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    mux.reconfigure(b.build().unwrap()).unwrap();

    let handles = mux.klv_handles_for_program(1).unwrap();
    assert_eq!(handles.len(), 2);
    mux.push_klv_to(handles[0], KLV, Pts90khz::new(96_000), 5)
        .unwrap();
    mux.push_klv_to(handles[1], KLV, Pts90khz::new(96_000), 7)
        .unwrap();
    assert_eq!(
        mux.push_klv_to(handles[1], KLV, Pts90khz::new(96_000), 5),
        refused(0x0102, 5)
    );
}

#[test]
fn the_same_id_in_another_program_is_not_enough() {
    // Program 1 declares service 3; program 2 keeps the automatic service 0.
    let mut p1 = MuxerProgramConfigBuilder::new(1, 0x1000);
    p1.add_video(0x0100, VideoCodec::H264);
    p1.add_klv(0x0101, KlvStreamType::SynchronousMetadata, true);
    p1.stream_descriptors_for_klv(0, vec![metadata_klva(3)])
        .unwrap();
    let mut p2 = MuxerProgramConfigBuilder::new(2, 0x1100);
    p2.add_video(0x0200, VideoCodec::H264);
    p2.add_klv(0x0201, KlvStreamType::SynchronousMetadata, true);
    let mut b = MuxerConfig::builder();
    b.add_program(p1.build());
    b.add_program(p2.build());
    let mut mux = Muxer::new(b.build().unwrap()).unwrap();

    let second = mux.klv_handles_for_program(2).unwrap()[0];
    assert_eq!(
        mux.push_klv_to(second, KLV, Pts90khz::new(90_000), 3),
        refused(0x0201, 3)
    );
    mux.push_klv_to(second, KLV, Pts90khz::new(90_000), 0)
        .unwrap();
    let first = mux.klv_handles_for_program(1).unwrap()[0];
    mux.push_klv_to(first, KLV, Pts90khz::new(90_000), 3)
        .unwrap();
}

#[test]
fn an_undeclared_service_is_an_invalid_usage_error() {
    let e = MuxError::UndeclaredMetadataService {
        pid: 0x0101,
        service_id: 3,
    };
    assert_eq!(e.kind(), crate::error::MuxErrorKind::InvalidUsage);
    let text = e.to_string();
    assert!(text.contains("0x0101"), "{text}");
    assert!(text.contains("metadata_service_id 3"), "{text}");
}
