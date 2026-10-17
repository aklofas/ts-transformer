# tst-srt

Safe Rust wrapper around libsrt — `SrtSocket`/`SrtListener`/`SrtSocketBuilder`,
SRT URL parsing, and `Transport`/`RecvTransport` implementations for
encrypted, congestion-controlled SRT streaming.

Built on `tst-core`'s transport traits; pair with `tst-pipeline`'s
`MuxSender`/`DemuxReceiver` shells for the full mux-to-wire path. See
the [docs landing
page](https://github.com/aklofas/ts-transformer/blob/main/docs/index.md)
for the full guide set.

## Quick start

```rust,no_run
use tst_srt::SrtSocketBuilder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut b = SrtSocketBuilder::new();
    b.latency_ms(120);
    let mut socket = b.connect("127.0.0.1:1234")?;

    socket.send(b"hello")?;
    Ok(())
}
```

**Build prerequisites:** a C/C++ toolchain and CMake (for the bundled
libsrt build), plus libclang (for bindgen) — see `tstrans-srt-sys`'s
README for detail.

**Stability: Stable** — the primary transport of this project's
scope; only the `shells` module (URL → managed-shell open path used by
the bindings) is Provisional. See the [API stability
reference](https://github.com/aklofas/ts-transformer/blob/main/docs/reference/api-stability.md).

**License:** MIT OR Apache-2.0. This crate depends on
`tstrans-srt-sys`, which bundles libsrt (MPL-2.0) — see that crate's
README.
