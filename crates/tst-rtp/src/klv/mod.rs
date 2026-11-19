//! RFC 6597 KLV (SMPTE 336M) packetization for the RTSP publish direction.
//!
//! **Stability: Stable** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! The inverse is the RTSP server's KLV depacketizer.

mod packetizer;

pub use packetizer::{MIN_MAX_PAYLOAD, packetize};
