//! Off-route demuxer events the unit tests of both pairing strategies feed.

use tst_core::mpegts::common::StreamTypeCode;
use tst_core::mpegts::demux::{
    DemuxEvent, DiscontinuityKind, ProgramMap, StreamId, StreamInfo, StreamKind, VideoCodec,
};

pub(super) fn af_discontinuity(pid: u16) -> DemuxEvent {
    DemuxEvent::Discontinuity {
        stream: StreamId {
            pid,
            kind: StreamKind::Video(VideoCodec::H264),
            program_number: 1,
        },
        kind: DiscontinuityKind::AdaptationFieldFlag,
    }
}

pub(super) fn cc_jump(pid: u16) -> DemuxEvent {
    DemuxEvent::Discontinuity {
        stream: StreamId {
            pid,
            kind: StreamKind::Video(VideoCodec::H264),
            program_number: 1,
        },
        kind: DiscontinuityKind::ContinuityJump {
            expected: 1,
            observed: 5,
        },
    }
}

pub(super) fn program_map(program_number: u16, pcr_pid: u16, pids: &[u16]) -> DemuxEvent {
    DemuxEvent::ProgramMap(ProgramMap {
        program_number,
        pcr_pid,
        pmt_pid: 0x20,
        streams: pids
            .iter()
            .map(|&pid| StreamInfo {
                pid,
                stream_type: StreamTypeCode::from_byte(0x1B),
                kind: StreamKind::Video(VideoCodec::H264),
                program_number,
                raw_descriptors: Vec::new(),
            })
            .collect(),
        klv_links: Vec::new(),
    })
}
