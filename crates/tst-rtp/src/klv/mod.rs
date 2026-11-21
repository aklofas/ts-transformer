//! RFC 6597 KLV (SMPTE 336M) packetization for the RTSP publish direction:
//! [`packetizer`].
//!
//! **Stability: Provisional** — see the
//! [API stability reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).
//!
//! The inverse is the RTSP server's KLV depacketizer.

pub(crate) mod align;
pub(crate) mod depacketizer;
pub mod packetizer;
