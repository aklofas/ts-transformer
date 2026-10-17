//! DemuxReceiver-side MPEG-TS demuxer.
//!
//! Drive a [`Demuxer`] with 188-byte TS packets (or arbitrary bytes that
//! contain TS packets — the demuxer handles sync recovery). Pull events
//! out via `next_event`. The event stream is decoupled-pairing: the
//! demuxer extracts and timestamps streams independently and never pairs
//! sync-KLV with video AUs. See the cookbook for canonical pairing recipes.
//!
//! Lenient by default — non-conformance surfaces as
//! `DemuxEvent::NonConformant` events so the receive loop keeps running.
//! `DemuxerConfig::builder().strict(StrictMode::*)` opts in to hard-fail
//! categories for compliance / ingest workflows.

pub(crate) mod au_reassemble;
pub mod demuxer;
pub mod event;
pub(crate) mod payload;
mod pes;
mod pid_state;
mod timestamp;
mod psi;
pub(crate) mod psi_assembler;
pub mod strict;
// pub(super): the demuxer's sibling modules in this directory call parse_ts_packet.
pub(super) mod ts;
mod types;
mod sync_ingress;
pub(crate) mod pmt_classify;
mod pat_reassemble;
mod psi_topology;
#[cfg(test)]
mod test_support;
mod pes_emit;
mod stats_recorder;

pub mod low_level;

pub use crate::codec::{AudioCodec, NalUnit, Obu, ObuExtension, VideoCodec};
pub use demuxer::Demuxer;
pub use event::{
    AdaptationFieldKind, Av1ObuHeaderKind, DemuxEvent, DiscontinuityKind, KlvLink, LinkSource,
    MetadataKind, NalHeaderKind, NonConformantIssue, PcrMalformedKind, PesHeaderMalformedKind,
    ProgramMap, PsiSyntaxKind, SamplePayload, StreamId, StreamInfo, StreamKind, StreamKindTag,
    SubtitleCodec, VideoPayload, pts_to_duration,
};
pub use payload::{split_video, split_video_strict};
pub use strict::StrictMode;
pub use types::{DemuxerConfig, DemuxerConfigBuilder, DemuxerStats};
