# tstrans

Python bindings (via PyO3) for the [ts-transformer](https://github.com/aklofas/ts-transformer) Rust workspace.

> **Status:** `tstrans` covers file inspection + construction (`Demuxer` / `Muxer` / `MuxerFileSink`), typed KLV decode + encode for ST 0601 / ST 0102 / ST 0605 / ST 0903 (with `VTargetPack`), codec parsers for H.264 / H.265 / H.266 / AV1 / AAC / MPEG-2 audio, optional pandas DataFrame adapters + NumPy snapshot views via `pip install tstrans[pandas]`, and live transports: **SRT** (`tstrans.srt` — Sender / Receiver / Builder / Socket / Listener / MuxSender / DemuxReceiver + Managed* auto-reconnect + ReconnectPolicy), **RTP + RTSP** (`tstrans.rtp` — Sender / Receiver / MuxSender / DemuxReceiver / RtspClient / RtspServer / MountHandle), raw **UDP / TCP / RIST** (`tstrans.udp` / `tstrans.tcp` / `tstrans.rist`), and **HLS** publishing (`tstrans.hls`). Minimum Python 3.10.

## Install

```
pip install tstrans
```

Optional extras for DataFrame / NumPy integration:

```
pip install 'tstrans[pandas]'
```

## Quickstart

Inspect a `.ts` file:

```python
from tstrans.io import parse_file, probe
from tstrans.mpegts import DemuxEvent

# Quick summary
r = probe("capture.ts")
print(r.video_codecs, r.audio_codecs, r.has_klv)

# Full event stream
for event in parse_file("capture.ts"):
    match event:
        case DemuxEvent.Video(pts=p, codec=c, raw=b):
            print(f"Video {c.name} pts={p.ms}ms len={len(b)}")
        case DemuxEvent.Metadata(pts=p, payload=b):
            print(f"KLV pts={p.ms}ms len={len(b)}")
```

Build a `.ts` file (single-program H.264):

```python
from tstrans.mpegts import (
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)

prog = (
    MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x100)
    .add_video(0x101, VideoCodec.H264)
    .build()
)
cfg = MuxerConfigBuilder().add_program(prog).build()
m = Muxer(cfg)

with m.write_file("out.ts") as proxy:
    proxy.push_video(nal_bytes, pts=Pts90khz.from_raw(900_000), key_frame=True)
```

## RTP + RTSP transport

`tstrans.rtp` ships RTP-over-UDP and a full RTSP client + server. Available
by default in published wheels — no extra needed (`pip install tstrans`
already includes it). The Rust-side `rtp` cargo feature is on by default;
source builds that don't need it can opt out with
`maturin develop --no-default-features` for a smaller binary.

Connect to a camera and iterate demuxed events:

```python
from tstrans.rtp import RtspClient, RtspClientConfig
from tstrans.mpegts import DemuxEvent

cfg = RtspClientConfig(url="rtsp://camera.local/live")
with RtspClient.connect(cfg) as session:
    demux = session.into_demux_receiver()
    for event in demux:
        if isinstance(event, DemuxEvent.Video):
            handle(event.raw)
```

Publish a stream from your own RTSP server:

```python
from tstrans.rtp import RtspServer, RtspServerConfig
from tstrans.mpegts import MuxerProgramConfigBuilder, Pts90khz, VideoCodec

program = (
    MuxerProgramConfigBuilder(1, 0x100)
    .add_video(0x101, VideoCodec.H264)
    .build()
)
with RtspServer.start(RtspServerConfig(bind_addr="0.0.0.0:8554")) as server:
    mount = server.add_unicast_mount("/live", program)
    mount.push_video(nal_bytes, pts=Pts90khz.from_raw(0), key_frame=True)
```

The full surface — `Sender` / `Receiver` for raw RTP, `MuxSender` /
`DemuxReceiver` for one-call mux/demux convenience, `RtspClient` /
`RtspSession` with Basic + Digest auth and TCP-interleaved fallback,
`RtspServer` + `MountHandle` with five push methods (each with a `_to`
multi-stream twin) and multicast mounts — is documented in the per-class
docstrings and the `python/tstrans/rtp.pyi` type stubs (checked against the
runtime by `mypy.stubtest` in CI).

## SRT transport

`tstrans.srt` ships full SRT (Secure Reliable Transport) bindings on top
of libsrt 1.5.7 — classes spanning low-level `Builder` / `Socket` /
`Listener` primitives, high-level `Sender` / `Receiver` for raw bytes,
`MuxSender` / `DemuxReceiver` for one-call mux/demux convenience, and
`ManagedSender` / `ManagedReceiver` / `ManagedMuxSender` /
`ManagedDemuxReceiver` for auto-reconnect-with-gap-buffer ergonomics.
Available by default in published wheels — no extra needed
(`pip install tstrans` already includes it). The Rust-side `srt` cargo
feature is on by default; source builds that don't need SRT can opt
out with `maturin develop --no-default-features` for a smaller binary
that drops the libsrt + mbedTLS link.

Hello world — caller-side send + listener-side receive:

```python
import threading
import tstrans.srt

# Listener side (run on one host / thread):
def listen_side() -> None:
    with tstrans.srt.Receiver.from_url("srt://0.0.0.0:9000?mode=listener") as rx:
        chunk = rx.recv_bytes(max_len=1500)
        print(f"received {len(chunk)} bytes; sync byte = {chunk[0]:#04x}")

threading.Thread(target=listen_side, daemon=True).start()

# Caller side (the other host / thread):
with tstrans.srt.Sender.from_url("srt://127.0.0.1:9000?mode=caller") as tx:
    payload = (b"\x47" + b"\x00" * 187) * 7  # one 1316-byte TS bundle
    tx.send_bytes(payload)
```

Full mux + demux example — push H.264 NALs + KLV blobs from the caller,
iterate `DemuxEvent`s on the listener:

```python
import threading
import tstrans.srt
from tstrans.mpegts import (
    DemuxEvent, KlvStreamType, MuxerProgramConfigBuilder, Pts90khz, VideoCodec,
)

program = (
    MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x100)
    .add_video(0x101, VideoCodec.H264)
    .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
    .build()
)

def consumer() -> None:
    with tstrans.srt.DemuxReceiver.from_url("srt://:9001?mode=listener") as rx:
        for event in rx:
            if isinstance(event, DemuxEvent.Video):
                print(f"video pts={event.pts.ms}ms nals={len(event.parse())}")
            elif isinstance(event, DemuxEvent.Metadata):
                print(f"klv pts={event.pts.ms}ms len={len(event.payload)}")

threading.Thread(target=consumer, daemon=True).start()

with tstrans.srt.MuxSender.from_url(
    "srt://127.0.0.1:9001?mode=caller", program
) as tx:
    # tx.send_video / tx.send_klv handle bundling, PSI emission, and
    # ts-packet framing transparently.
    tx.send_video(nal_bytes, pts=Pts90khz.from_raw(0), key_frame=True)
    tx.send_klv(klv_ls_bytes, pts=Pts90khz.from_raw(0))
```

Builder + `Socket` promotion for fine-grained control (passphrase,
custom latency, stream id, congestion mode):

```python
import tstrans.srt

# Caller side with encryption + a custom SRT latency.
sock = (
    tstrans.srt.Builder("srt://127.0.0.1:9000?mode=caller")
    .passphrase("hunter-too-long-thanks!!")  # >= 10 chars
    .latency_ms(200)
    .stream_id("cam-01")
    .caller()
    .connect()
)
sender = sock.into_sender()  # consumes the Socket
sender.send_bytes(b"...")
sender.close()
```

Auto-reconnect with `ManagedReceiver` — wraps the underlying SRT
transport in a `tst_pipeline::ManagedRecvTransport` that catches
connection breaks and re-binds + re-accepts per a `ReconnectPolicy`;
`reconnect_attempts()` counts the reconnects. (`ManagedDemuxReceiver`
also surfaces each reconnect as a `DemuxEvent.ReconnectDiscontinuity`.)

```python
import tstrans.srt
from tstrans.srt import BackoffStrategy, ReconnectPolicy

policy = ReconnectPolicy(
    max_attempts=5,
    backoff=BackoffStrategy.exponential(base_ms=100, max_ms=10_000),
)

with tstrans.srt.ManagedReceiver.from_url(
    "srt://:9000?mode=listener", policy=policy
) as rx:
    while True:
        try:
            chunk = rx.recv_bytes(max_len=1500)
            print(f"received {len(chunk)} bytes; "
                  f"reconnects so far = {rx.reconnect_attempts()}")
        except KeyboardInterrupt:
            break
```

The full surface — `Sender` / `Receiver` for raw bytes, `MuxSender` /
`DemuxReceiver` for one-call mux/demux, `Builder` / `Socket` /
`Listener` for low-level construction, `ManagedSender` /
`ManagedReceiver` / `ManagedMuxSender` / `ManagedDemuxReceiver` for
auto-reconnect, plus `ReconnectPolicy` / `BackoffStrategy` /
`OverflowPolicy` policy types and `SocketStats` / `SrtStats` /
`CancelHandle` — is documented in the per-class docstrings and the
`python/tstrans/srt.pyi` type stubs (checked against the runtime by
`mypy.stubtest` in CI).

## See also

[docs/languages/python.md](https://github.com/aklofas/ts-transformer/blob/main/docs/languages/python.md)
is the full Python guide, including the DataFrame / NumPy integration.
