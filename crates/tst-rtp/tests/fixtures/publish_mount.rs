//! What a test reads back from a publish mount: the server re-muxes an
//! elementary publisher's video on PID 0x100 and its KLV on PID 0x101.

use tst_core::mpegts::demux::{DemuxEvent, SamplePayload};

/// The PID of an elementary publisher's video in the mount's stream.
pub const VIDEO_PID: u16 = 0x100;
/// The PID of an elementary publisher's KLV in the mount's stream.
pub const KLV_PID: u16 = 0x101;

/// A demuxed video sample: `(pts ticks, Annex-B bytes, random access)`.
pub type VideoSample = (i64, Vec<u8>, bool);

/// Every video sample on [`VIDEO_PID`] among `events`.
pub fn video_samples(events: &[DemuxEvent]) -> Vec<VideoSample> {
    events
        .iter()
        .filter_map(|ev| match ev {
            DemuxEvent::Sample {
                stream,
                pts,
                payload:
                    SamplePayload::Video {
                        raw,
                        random_access_indicator,
                        ..
                    },
                ..
            } if stream.pid == VIDEO_PID => {
                Some((pts.as_ticks(), raw.to_vec(), *random_access_indicator))
            }
            _ => None,
        })
        .collect()
}

/// `(pts ticks, payload)` of every KLV `Metadata` event on [`KLV_PID`].
pub fn klv_events(events: &[DemuxEvent]) -> Vec<(i64, Vec<u8>)> {
    events
        .iter()
        .filter_map(|ev| match ev {
            DemuxEvent::Metadata {
                stream,
                pts,
                payload,
                ..
            } if stream.pid == KLV_PID => Some((pts.as_ticks(), payload.clone())),
            _ => None,
        })
        .collect()
}
