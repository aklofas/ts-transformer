# tst-core

Pure MPEG-TS mux/demux, KLV (MISB ST 0601 and friends), codec
parameter-set parsers (H.264/H.265/H.266/AV1), and the
`Transport`/`RecvTransport` trait contracts. No network I/O, no threads,
no transport implementations; `no_std` + `alloc` with
`default-features = false` (the default `std` feature adds file I/O and
the cancel plumbing).

This is the substrate every other `tst-*` crate builds on — transport
implementations (`tst-srt`, `tst-udp`, `tst-tcp`, `tst-rtp`, `tst-hls`,
`tst-rist`) and the pipeline shells (`tst-pipeline`) depend on it, not
the other way around. See the [docs landing
page](https://github.com/aklofas/ts-transformer/blob/main/docs/index.md)
for the full guide set.

## Quick start — round-trip a ST 0601 record

```rust
use tst_core::klv::st0601;
use tst_core::UasDatalinkLs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut ls = UasDatalinkLs::default();
    ls.timestamp_us = Some(1_700_000_000_000_000);

    let bytes = st0601::encode_to_vec(&ls)?;
    let decoded = st0601::decode(&bytes)?;
    assert_eq!(decoded.timestamp_us, Some(1_700_000_000_000_000));
    Ok(())
}
```

**Stability: Stable**, with several Provisional modules: the newer typed
KLV sets (`klv::st0102`, `klv::st0605`, `klv::st0903`, `klv::st0805`,
`klv::st0806`, `klv::st1010`, `klv::st1204`), the newer codec parsers
(`codec::h266`, `codec::av1`, `codec::misp_time`, `codec::nal_framing`),
the audio parsers (`codec::aac`, `codec::ac3`, `codec::mpegaudio`), the
`transport::conformance` table, and the HLS-adjacent `publisher` trait. This list highlights the carve-outs; the [API
stability
reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md)
is the authoritative full per-module table.

**License:** MIT OR Apache-2.0.
