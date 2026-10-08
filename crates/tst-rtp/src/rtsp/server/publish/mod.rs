//! Publisher direction of [`RtspServer`](crate::rtsp::server::RtspServer):
//! ANNOUNCE / SETUP `mode=record` / RECORD (RFC 2326 §10.3, §10.11, §12.39).
//! A published mount re-serves PLAY readers and hands the application an
//! [`RtpRecvTransport`](crate::transport::RtpRecvTransport).

pub(crate) mod adapter;
pub(crate) mod adapter_es;
pub(crate) mod align;
pub(crate) mod handlers;
pub(crate) mod klv_depacketizer;
pub(crate) mod mount;
pub(crate) mod session;
pub(crate) mod shape;
pub(crate) mod udp_ingest;

pub use mount::{ClockAlignment, PublishMountHandle, PublishMountStats, PublisherInfo};

/// Wire shape a publisher announced (§1 classification table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PublishShape {
    /// One MPEG-TS-over-RTP track (RFC 2250, PT 33 or a dynamic PT mapped to `MP2T/90000`).
    Mp2t,
    /// Elementary tracks: one H.264 (RFC 6184) video track, optionally one KLV (RFC 6597) track.
    Elementary {
        /// The announce carries a KLV (RFC 6597, `smpte336m`) track beside
        /// the video.
        klv: bool,
    },
}
