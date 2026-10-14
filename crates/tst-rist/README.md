# tst-rist

Safe Rust wrapper around librist — RIST Simple and Main profile
sender/receiver `Transport` implementations, with optional AES-128/
192/256 PSK encryption.

Built on `tst-core`'s transport traits; pair with `tst-pipeline`'s
`MuxSender`/`DemuxReceiver` shells for the full mux-to-wire path. See
the [docs landing
page](https://github.com/aklofas/ts-transformer/blob/main/docs/index.md)
for the full guide set.

## Quick start

```rust,no_run
use tst_core::transport::Transport;
use tst_rist::RistTransport;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut tx = RistTransport::connect("rist://127.0.0.1:9000")?;

    let ts_packet = [0u8; 188]; // caller-supplied MPEG-TS packet
    tx.send_bytes(&ts_packet)?;
    Ok(())
}
```

**Build prerequisites:** a C toolchain, meson, and ninja (for the
bundled librist build — Debian/Ubuntu: `apt install meson
ninja-build`), plus libclang (for bindgen) — see `tstrans-rist-sys`'s
README for detail.

**Stability: Provisional** — profile coverage (Simple/Main) is still
evolving. See the [API stability
reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

**License:** MIT OR Apache-2.0. This crate depends on
`tstrans-rist-sys`, which bundles librist (BSD-2-Clause) — see that
crate's README.
