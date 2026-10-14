# tst-hls

HLS publisher — segments MPEG-TS to disk and writes the playlist, with
an optional built-in HTTP server (`hls://`/`hlss://`). KLV rides the
`.ts` segments transparently; no new wire format.

Built on `tst-core`'s `Publisher` trait; pair with `tst-pipeline`'s
`MuxPublisher` shell to feed it from encoded video/KLV/audio directly.
See the [docs landing
page](https://github.com/aklofas/ts-transformer/blob/main/docs/index.md)
for the full guide set.

## Quick start

```rust,no_run
use tst_core::publisher::Publisher;
use tst_hls::{HlsMode, HlsPublisherBuilder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut publisher = HlsPublisherBuilder::new()
        .bind("127.0.0.1:8080".parse()?)
        .output_dir(std::env::temp_dir().join("hls_out"))
        .mode(HlsMode::Live)
        .build()?;

    // Bytes must be a whole multiple of 188 (real callers mux with
    // `tst-pipeline`'s `MuxPublisher`, or their own muxer).
    let ts_packets = [0u8; 188 * 7];
    publisher.push_ts(&ts_packets)?;
    publisher.finish()?;
    Ok(())
}
```

**Stability: Provisional** — split out of `tst-tcp` in 0.3.0; the
feature surface is still settling. See the [API stability
reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

**License:** MIT OR Apache-2.0.
