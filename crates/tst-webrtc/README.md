# tst-webrtc

WebRTC playback for tstrans: a WHEP (WebRTC-HTTP Egress Protocol) server
with RTSP-style mounts. Browsers pull a mount over WebRTC; H.264 and H.265
pass through untouched, and KLV metadata rides frame-locked in SEI and on a
`klv` data channel.

The crate is new and its surface is still settling. See the [docs landing
page](https://github.com/aklofas/ts-transformer/blob/main/docs/index.md)
for the full guide set.

**Stability: Provisional** — new in 0.9.0. See the [API stability
reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

**License:** MIT OR Apache-2.0.
