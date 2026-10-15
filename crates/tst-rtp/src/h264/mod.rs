//! RFC 6184 H.264 RTP payload format support.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! This module provides SDP attribute parsing, the depacketizer state
//! machine for reassembling H.264 NALUs from RTP packets, and the
//! blocking I/O receiver shell.
//!
//! # SDP parsing
//!
//! `RtspClient::setup_h264_auto` discovers the dynamic payload type assigned
//! to H.264 from the `a=rtpmap` lines, and extracts packetization mode,
//! out-of-band parameter sets (SPS/PPS), and profile information from
//! `a=fmtp`.
//!
//! # Depacketizer
//!
//! Use [`H264Depacketizer`] to reassemble Access Units from RTP packets:
//! call [`H264Depacketizer::feed`] for each received packet then drain
//! [`H264Depacketizer::next_au`] until it returns `None`.
//!
//! # Packetizer
//!
//! [`packetizer::packetize`] is the inverse of the depacketizer: an access
//! unit's NAL units to RTP payloads.
//!
//! # Receiver
//!
//! Use [`H264Receiver`] for a blocking I/O shell that wraps a UDP socket
//! or TCP-interleaved mpsc channel, drives the depacketizer, and returns
//! [`H264Au`]s via [`H264Receiver::recv_au`].

pub mod depacketizer;
pub(crate) mod fmtp;
pub mod packetizer;
pub mod receiver;

pub use crate::depay::ParameterSetInjection;
pub use depacketizer::{H264Au, H264Depacketizer, H264DepayConfig, H264DepayStats};
pub use receiver::H264Receiver;
