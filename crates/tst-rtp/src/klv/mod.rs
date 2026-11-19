//! RFC 6597 KLV (SMPTE 336M) packetization for the RTSP publish direction.
//! The inverse is the RTSP server's KLV depacketizer.

mod packetizer;

pub use packetizer::{MIN_MAX_PAYLOAD, packetize};
