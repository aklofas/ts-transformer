# Python bindings (`tstrans`)

> **Who this is for:** You write Python and want to inspect, build,
> stream, or receive MPEG-TS + KLV — offline files *and* live transports
> (UDP, TCP, RTP/RTSP, SRT, RIST).

> **You will learn:**
> - How to install `tstrans` (with or without the pandas extra)
> - How to read a `.ts` file and inspect typed `DemuxEvent` items in ~5 lines
> - How to build a `.ts` file by pushing video + KLV through the `Muxer`
> - How to encode and decode all 4 MISB KLV sets (ST 0601 / 0102 / 0605 / 0903), plus ST 0806 RVT, ST 1010 SDCC error covariance, and the ST 0805 KLV → CoT conversion layer
> - How to send and receive live MPEG-TS over UDP, TCP, RTP/RTSP, SRT, and RIST
> - How to pair video with synchronized KLV via `tstrans.pipeline.Pairer`
> - How to drive bulk KLV → pandas DataFrame ETL with the optional `[pandas]` extra
> - The Python-specific gotchas: GIL release, dataclass strictness, optional extras
> - How this binding differs from the Rust core

## Install

> **On PyPI.** `pip install tstrans` (v0.2.0+). To work from a source checkout
> instead, build the wheel with `maturin` or run `maturin develop` in
> `bindings/python/`.

```bash
pip install tstrans
```

Optional extras:

```bash
pip install 'tstrans[pandas]'   # pandas DataFrame adapters + NumPy snapshot views
```

**Minimum Python is 3.10** (bumped from 3.9 mid-Phase-2 to enable PEP 604
union syntax and `match` statements without compat hacks).

The compiled extension is imported as `tstrans._native`. Public API lives
on `tstrans` and its topic submodules — `tstrans.io`, `tstrans.mpegts`,
`tstrans.klv`, `tstrans.codec`, the live-transport modules `tstrans.srt`,
`tstrans.rtp`, `tstrans.udp`, `tstrans.tcp`, `tstrans.rist`, the
`tstrans.pipeline` pairer, and the optional `tstrans.pandas`. Don't reach
into `tstrans._native` directly; it may reorganize between versions.

Wheels ship the UDP, TCP, RTP/RTSP, SRT, RIST, and HLS surfaces
on by default — with one caveat: **RIST is excluded from the Windows
wheel** (the Linux and macOS wheels include it). The HLS publisher
(`tstrans.hls`) imports out of the box from a published wheel (see
[HLS publisher](#hls-publisher-tstranshls) below); it is unavailable only
in a `--no-default-features` source build that omits `hls`.

> **Status:** `tstrans` ships the full surface — offline file inspection
> + construction (`Demuxer` / `Muxer` / `MuxerFileSink`), typed KLV
> decode + encode for ST 0601 / ST 0102 / ST 0605 / ST 0903 (with
> `VTargetPack`), ST 0806 RVT, ST 1010 SDCC-FLP, and the ST 0805
> KLV → CoT conversion layer, codec parsers for H.264 / H.265 / H.266 / AV1 / AAC /
> MPEG-2 audio, optional pandas DataFrame adapters + NumPy snapshot views
> via `pip install tstrans[pandas]`, the live transports
> `tstrans.{srt,rtp,udp,tcp,rist}` (with RTSP client + server and SRT
> auto-reconnect), the HLS publisher (`tstrans.hls`), and
> `tstrans.pipeline.Pairer`. ~1240 pytest tests.

## Hello world

Read a `.ts` file and print the type of each event in five lines:

```python
import tstrans

for event in tstrans.io.parse_file("capture.ts"):
    print(type(event).__name__)
```

## First send

Build a single-program H.264 `.ts` file by pushing one access unit through
the `Muxer`:

```python
from tstrans.mpegts import (
    KlvStreamType,
    Muxer,
    MuxerConfigBuilder,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)

prog = (
    MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x100)
    .add_video(0x101, VideoCodec.H264)
    .add_klv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, carries_pts=True)
    .build()
)
cfg = MuxerConfigBuilder().add_program(prog).build()
m = Muxer(cfg)

with m.write_file("out.ts") as proxy:
    proxy.push_video(nal_bytes, pts=Pts90khz.from_raw(900_000))
    proxy.push_klv(klv_bytes, pts=Pts90khz.from_raw(900_000))
```

`MuxerFileSink` (the object returned by `write_file`) is a context
manager — `__exit__` flushes and finalizes the file; no explicit
`close()` ceremony is needed. Note the pushes go through `proxy` (the
object the `with` statement yields), not through `m` — only proxy
pushes drain to the file as they go.

When you need the TS bytes in memory rather than writing to a file, use
`Muxer.pull()` directly. `pull` fills a preallocated `bytearray` and
returns the number of bytes written; drain it in a loop until `pull`
returns 0:

```python
buf = bytearray(188 * 64)   # preallocate one chunk

m.push_video(nal_bytes, pts=Pts90khz.from_raw(900_000))
m.push_klv(klv_bytes, pts=Pts90khz.from_raw(900_000))

packets: list[bytes] = []
while (n := m.pull(buf)) > 0:
    packets.append(bytes(buf[:n]))
```

For the common file-write case, `write_file` (returning a `MuxerFileSink`)
is the canonical path — no manual `pull` loop needed.

## First receive

Demux a file and dispatch on typed events with a `match` statement:

```python
from tstrans.io import parse_file
from tstrans.mpegts import DemuxEvent

for event in parse_file("capture.ts"):
    match event:
        case DemuxEvent.ProgramMap(programs=pms):
            print(f"PSI: {len(pms)} programs")
        case DemuxEvent.Video(pts=p, codec=c, raw=b) as ev:
            # raw-first: `raw` is the exact encoded access unit. Splitting
            # it into typed NAL/OBU units is opt-in via `ev.parse()`.
            print(f"Video {c.name} pts={p.ms}ms len={len(b)} units={len(ev.parse())}")
        case DemuxEvent.Metadata(pts=p, payload=b):
            print(f"KLV pts={p.ms}ms len={len(b)} (use tstrans.klv to decode)")
```

For a quick summary without iterating every event, use `probe`:

```python
from tstrans.io import probe

r = probe("capture.ts")
print(r.video_codecs, r.audio_codecs, r.has_klv)
```

To pull typed KLV records directly:

```python
from tstrans.io import extract_klv
from tstrans.klv import UasDatalinkLs, parse_klv_universal

# Iterate typed KLV records from a .ts file
for pts, record in extract_klv("capture.ts", parsed=True, with_pts=True):
    if isinstance(record, UasDatalinkLs):
        pos = record.sensor_position()
        if pos is not None:
            print(
                f"{pts.ms}ms platform={record.platform_designation} "
                f"@ {pos.lat_deg:.5f},{pos.lon_deg:.5f} alt={pos.alt_m:.1f}m"
            )

# Or dispatch a single record by UL
record = parse_klv_universal(raw_klv_bytes)
# record is UasDatalinkLs | SecurityLs | PrecisionTimeStampPack | VmtiLs | None
```

For the common ST-0601-only case there is a dedicated iterator that
also carries each record's file-order KLV index (it counts every KLV
event, so indices line up with a later re-mux pass over the same
file), and the typed sets support copy-update via `with_()` — they
are frozen dataclasses, so attribute assignment raises
`FrozenInstanceError`:

```python
from tstrans.io import iter_uas_datalink

for pts, klv_index, record in iter_uas_datalink("capture.ts"):
    corrected = record.with_(sensor_lat_deg=33.5)  # frozen → copy-update
```

KLV demux events decode in place too: `ev.parse()` on a
`DemuxEvent.Metadata` dispatches by universal label — the KLV counterpart
of the raw-first `Video.parse()` / `Audio.parse()`.

All 4 MISB typed sets (ST 0601 UAS Datalink, ST 0102 Security,
ST 0605 Precision Time Stamp, ST 0903 VMTI) decode with the same
semantics as the Rust crate: lenient mode tolerates broken input and
accumulates per-field errors on `.field_errors`; strict mode raises
`tstrans.exceptions.KlvError`. Symmetric encoders (`encode_*` (lenient default) /
`encode_*_strict_compliance` (opt-in strict)) round-trip parsed records
back to wire bytes.
See the `tstrans.klv` module docstring for the full type listing.

### RVT, SDCC error covariance, and KLV → CoT (ST 0806 / ST 1010 / ST 0805)

Three more typed layers ship alongside the core four: `decode_rvt` /
`decode_rvt_standalone` (+ `encode_rvt` / `encode_rvt_standalone`) for the
ST 0806.4 Remote Video Terminal Local Set (`UasDatalinkLs.rvt` carries the
pass-through Tag 73 bytes); `decode_sdcc_flp` / `encode_sdcc_flp_mode2` for
the general-purpose ST 1010.3 SDCC-FLP error-covariance pack (ST 0601 Tag
102 embeds one occurrence per `UasDatalinkLs.sdcc_flps` entry); and the
one-way `platform_position_xml` / `sensor_point_of_interest_xml` ST 0805.1
KLV → Cursor-on-Target conversion, configured via the `CotConfig` dataclass
(a plain `dataclasses.dataclass`, not a PyO3 type — same pattern as
`DemuxerConfig`). `CotConfig.producer` is an XML attribute *name* stamped
verbatim into `<detail><_flow-tags_ .../>` — a Name production (an
attribute name, not a value), neither validated nor escaped, so an
invalid value produces malformed XML.

```python
from tstrans.klv import decode_rvt, decode_sdcc_flp, platform_position_xml

if record.rvt is not None:
    rvt = decode_rvt(record.rvt)  # ST 0601 Tag 73 pass-through bytes
    for poi in rvt.points_of_interest:
        print(poi.number, poi.text)

for field in record.sdcc_flps:
    m = decode_sdcc_flp(field.bytes)
    print(m.matrix_size, m.std_devs)

# generated_us is keyword-only and required — no wall-clock sampling,
# see the determinism note in guides/klv.md.
xml = platform_position_xml(record, generated_us=generated_us)
```

`decode_rvt` / `decode_sdcc_flp` raise `tstrans.exceptions.KlvError` on
malformed bytes, same as the core four. `platform_position_xml` /
`sensor_point_of_interest_xml` raise `ValueError` (not `KlvError`) when a
required source KLV tag is absent from `record`.

### DemuxEvent variant reference

The full `DemuxEvent` class hierarchy for `match`/`isinstance` dispatch:

```python
from tstrans.mpegts import Demuxer, DemuxerConfig, DemuxEvent

with open("capture.ts", "rb") as f:
    ts = f.read()

# For files larger than 4 MiB, use the chunk-and-drain loop (see below)
# rather than a single feed() call.
d = Demuxer()
CHUNK = 188 * 1024
for i in range(0, len(ts), CHUNK):
    d.feed(ts[i : i + CHUNK])
    while (ev := d.next_event()) is not None:
        match ev:
            case DemuxEvent.ProgramMap():
                pass  # topology; build config from this if re-muxing
            case DemuxEvent.Video():
                pass  # SamplePayload::Video — raw AU; ev.parse() → NALs/OBUs
            case DemuxEvent.Audio():
                pass  # SamplePayload::Audio — raw frames; ev.parse() → typed
            case DemuxEvent.Subtitle():
                pass  # SamplePayload::Subtitle
            case DemuxEvent.Metadata():
                pass  # KLV — both sync AU-cell and async bare-LS land here
                      # DemuxEvent.Klv is a deprecated alias; prefer .Metadata
            case DemuxEvent.UnknownSample():
                pass  # unrecognized stream_type; verbatim PES payload
            case DemuxEvent.Discontinuity():
                pass  # CC jump or PES overflow on a PID
            case DemuxEvent.NonConformant():
                pass  # spec violation tolerated in lenient mode
            case DemuxEvent.ReconnectDiscontinuity():
                pass  # injected only by ManagedDemuxReceiver after reconnect
d.flush()
while (ev := d.next_event()) is not None:
    pass  # drain trailing AU (unbounded-PES_packet_length)
```

**Sync-ingress ceiling.** `Demuxer.feed()` buffers incoming bytes
in a pre-sync scan window capped at **4 MiB** by default. A single
`feed()` call with a file larger than that cap will raise
`DemuxError` before any events appear. The chunk-and-drain loop above
avoids this regardless of file size. To raise the cap instead (for
small files where a single-call approach is preferred):

```python
from tstrans.mpegts import Demuxer, DemuxerConfig

d = Demuxer(DemuxerConfig(sync_buf_cap=64 * 1024 * 1024))  # 64 MiB
```

**Continuous timestamps across the 33-bit rollover.** `DemuxerConfig.unwrap_timestamps`
(default `False`) opts a `Demuxer` into unwrapping the raw 33-bit 90 kHz
PTS/DTS onto a continuous per-program timeline instead of emitting the raw
wire value (which wraps every ~26.5 h):

```python
from tstrans.mpegts import Demuxer, DemuxerConfig

d = Demuxer(DemuxerConfig(unwrap_timestamps=True))
```

Each sample's signed wrap-aware delta accumulates onto the previous
unwrapped value for its PID, so a genuine out-of-order arrival steps back
by its true distance instead of being mistaken for another wrap. PIDs of
the same program share one running clock and stay directly comparable
across the rollover — this is what lets a consumer pair KLV to video by
PTS on a long-running stream — while independent programs are never
cross-anchored. The accumulators reset alongside the rest of the per-PID
parse state on a sync reset, so a `ManagedDemuxReceiver` reconnect (see
below) restarts the unwrap timeline from the next `ProgramMap`.

## Transmux: edit metadata, copy everything else

`tstrans.io.transmux` bridges a demuxer and a muxer: iterate the source's
events and write back the ones to keep. Video/audio are copied
byte-for-byte via their raw encoded AUs; KLV can be substituted — pair
with `tstrans.klv.patch_uas_datalink` for byte-faithful tag edits. The
output muxer is built lazily from the first `ProgramMap`, so the
source's program topology (PIDs, codecs, program number) is reproduced.

```python
import tstrans.io as tio
from tstrans import klv
from tstrans.mpegts import DemuxEvent

with tio.transmux("in.ts", "out.ts", atomic=True) as tx:
    for ev in tx:
        if isinstance(ev, DemuxEvent.Metadata):
            patched = klv.patch_uas_datalink(
                ev.payload, {"frame_center_lat_deg": 37.7749}
            )
            tx.write_klv(ev, patched)
        else:
            tx.write(ev)  # video/audio copied byte-for-byte
```

Strict by default: streams the muxer cannot represent (DVB
subtitling/teletext) raise `MuxError` naming the offenders.
Private/application data streams (unknown stream types) pass through
byte-faithfully: `MuxerConfig.from_program_map` reproduces their PMT
entry (raw stream_type byte + descriptor loop verbatim) and each
`DemuxEvent.UnknownSample` payload is re-emitted as-is via
`push_data_to`. Re-muxed data streams always carry PTS and the
demuxer substitutes 0 for a PTS-less source PES, so a source sample
with no PTS re-emerges with a literal PTS of 0.
Pass kinds in `drop=` (e.g. `drop=(StreamKindTag.UNKNOWN,)`) to
exclude streams instead; their events are then skipped by `write`. v1
supports single-program sources (a second program raises
`ValueError`).
`atomic=True` writes through a same-directory `*.partial` temp file and
`os.replace`s it into place only on clean exit, so no partial output can
appear at the destination.

## SRT transport (`tstrans.srt`)

`tstrans.srt` wraps `tst_pipeline`'s `Sender` / `Receiver` over an SRT
transport plus the low-level `tst_srt` `Builder` / `Socket` / `Listener`,
for streaming pre-muxed MPEG-TS bytes over SRT. Use the raw `Sender` /
`Receiver` when you already hold TS bytes (e.g. from a `Muxer` or a file);
use the `MuxSender` / `DemuxReceiver` convenience shells (next section) to
push elementary streams directly. SRT ships in published wheels
(default-on).

`SrtError` and `SrtErrorKind` live in `tstrans.exceptions`, **not** in
`tstrans.srt` (the same pattern as `tstrans.rtp`):

```python
from tstrans.exceptions import SrtError, SrtErrorKind
```

### Sender hello

Caller mode is the default when `?mode=` is omitted. URL query parameters
(`passphrase`, `latency`, `streamid`, `mss`, `payloadsize`, …) apply
through the SRT URL overlay:

```python
from tstrans.srt import Sender

# mode=caller is the default when ?mode= is omitted.
with Sender.from_url("srt://host:9000?mode=caller&passphrase=secret") as tx:
    tx.send_bytes(ts_bytes)   # pre-muxed TS bytes, any length
    tx.flush()                # emit any buffered partial bundle
```

`send_bytes` accepts a bytes-like payload of any length; the sender frames
it into 7-packet (1316-byte) SRT bundles internally. Call `flush()` after
the last push to emit a partial bundle. `socket_stats()` returns the
scheme-neutral 16-field `SocketStats`; `srt_stats()` returns the
libsrt-rich `SrtStats` (estimated bandwidth, symmetric send/recv-side loss
split).

### Receiver hello

`Receiver.from_url` does a one-shot bind + accept (listener mode). Each
`recv_bytes()` returns one 188-byte TS packet:

```python
from tstrans.srt import Receiver
from tstrans.exceptions import SrtError, SrtErrorKind

with Receiver.from_url("srt://:9000?mode=listener") as rx:
    try:
        while True:
            pkt = rx.recv_bytes()   # one 188-byte TS packet per call
            ...                     # process pkt
    except SrtError as e:
        # CLOSED / BROKEN both signal end of stream.
        if e.kind not in (SrtErrorKind.CLOSED, SrtErrorKind.BROKEN):
            raise
```

### Builder → Socket low-level path

When you need the listener's bound port (e.g. binding to an ephemeral
`:0`), drive the `Builder` → `Listener` → `Socket` path. A `Socket` is
consumed by exactly one of `into_sender()` / `into_receiver()` /
`into_mux_sender(program_config)` / `into_demux_receiver(demux_config=None)`:

```python
from tstrans.srt import Builder

# Caller side: connect, then turn the socket into a Sender.
with Builder("srt://host:9000").caller().latency_ms(200).connect() as sock:
    with sock.into_sender() as tx:
        tx.send_bytes(ts_bytes)
        tx.flush()

# Listener side: bind an ephemeral port, read it back, accept one peer.
with Builder("srt://127.0.0.1:0?mode=listener").listener().listen() as listener:
    host, port = listener.local_addr()
    print("listening on", port)
    sock = listener.accept()          # blocks for the first peer
    with sock.into_receiver() as rx:
        pkt = rx.recv_bytes()
```

The `Builder` mode setters (`.caller()` / `.listener()`) set only the
Python-side mode; `.listen()` still requires `?mode=listener` in the URL.
URL-provided values win over kwargs / setters. A `Listener` is iterable —
iterating yields accepted `Socket`s until `cancel_handle().cancel()` stops
it.

### Cancellation

`Sender` / `Receiver` / `Listener` (and the `DemuxReceiver` shell) expose
`cancel_handle()`; calling `.cancel()` from another thread wakes a thread
parked in `send_bytes` / `recv_bytes` / `accept` within ~3–10 ms,
surfacing `SrtError(BROKEN)` or `SrtError(CLOSED)`:

```python
tx = Sender.from_url("srt://host:9000?mode=caller")
cancel = tx.cancel_handle()
# On another thread:
cancel.cancel()   # wakes tx.send_bytes() → SrtError(BROKEN | CLOSED)
```

`is_cancelled()` is per-clone, but `cancel()` on any clone wakes the shared
socket.

### SRT convenience (`MuxSender` / `DemuxReceiver`)

`MuxSender` bundles a `Muxer` + an SRT `Sender`: send encoded elementary
streams and it muxes + sends in one call. `DemuxReceiver` bundles a
`Receiver` + a `Demuxer`: iterate to get the same
`tstrans.mpegts.DemuxEvent` subclass instances the offline `Demuxer`
produces. Both release the GIL around the muxer / transport work, and both
are context managers.

```python
from tstrans.srt import MuxSender
from tstrans.mpegts import MuxerProgramConfigBuilder, Pts90khz, VideoCodec

program = (
    MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x1000)
    .add_video(0x1011, VideoCodec.H264)
    .build()
)

with MuxSender.from_url(
    "srt://host:9000?mode=caller&latency=120", program
) as s:
    pts = 0
    for nal in access_units:
        s.send_video(nal, pts=Pts90khz.from_raw(pts), key_frame=True)
        pts += 3000   # 90 kHz ticks per frame
```

`pts` is keyword-only on every send method. The send surface: `send_video`
/ `send_klv` / `send_audio` / `send_subtitle` / `send_data` (raw
private-data bytes, passed through verbatim), plus the handle-targeted
`send_*_to` variants for multi-stream programs and the first-of-kind handle
accessors (`video_handle()`, `klv_handle()`, …, `data_handle()`). `stats()`
returns a `(SocketStats, MuxerStats)` tuple. There is no `flush()` — bytes
flush per-send and again on `close()`.

Receiving:

```python
from tstrans.srt import DemuxReceiver
from tstrans.mpegts import DemuxEvent

with DemuxReceiver.from_url("srt://:9000?mode=listener") as rx:
    for event in rx:
        match event:
            case DemuxEvent.Video(pts=p, raw=b):
                ...   # one encoded access unit
            case DemuxEvent.Metadata(payload=b):
                ...
```

`add_byte_sink(callback)` fans out every 188-byte TS packet (as `bytes`)
to a callback BEFORE demuxing — register it before iterating. If the
callback raises, the error re-raises from the next iteration step and
iteration stops. Keep the sink cheap (it runs on the recv-loop thread) and
never re-enter the receiver from it.

`rx.last_seen_micros(pid)` returns the epoch-microsecond timestamp of
the most recent item this receiver emitted for `pid`, or `None` if
`pid` was never seen — a cheap staleness check with no snapshot-diffing
required. See the watchdog recipe under [RTP convenience](#rtp-convenience-muxsender--demuxreceiver)
below (the method is identical here).

### SRT managed reconnect

The four `Managed*` shells add automatic reconnect: on a Broken/Closed
transport they re-dial (caller) or re-bind + re-accept (listener) under a
`ReconnectPolicy` and resume, replaying buffered gap data. `ManagedSender`
/ `ManagedReceiver` are the raw-bytes pair; `ManagedMuxSender` /
`ManagedDemuxReceiver` are the convenience pair.

```python
from tstrans.srt import (
    ManagedMuxSender,
    ReconnectPolicy,
    BackoffStrategy,
    OverflowPolicy,
)
from tstrans.mpegts import MuxerProgramConfigBuilder, Pts90khz, VideoCodec

policy = ReconnectPolicy(
    max_attempts=None,                  # None = retry forever; an int caps attempts
    backoff=BackoffStrategy.exponential(base_ms=100, max_ms=10_000),
    gap_buffer_capacity=256,
    overflow_policy=OverflowPolicy.DROP_OLDEST,
)

program = (
    MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x1000)
    .add_video(0x1011, VideoCodec.H264)
    .build()
)

with ManagedMuxSender.from_url(
    "srt://host:9000?mode=caller&latency=120", program, policy=policy
) as s:
    for i, nal in enumerate(access_units):
        s.send_video(nal, pts=Pts90khz.from_raw(i * 3000), key_frame=True)
```

Passing `policy=None` (or omitting it) uses the defaults
(`max_attempts=10`, `BackoffStrategy.exponential(base_ms=100,
max_ms=10_000)`, `gap_buffer_capacity=256`, `OverflowPolicy.DROP_OLDEST`).
`BackoffStrategy.constant(ms)` is the fixed-wait alternative;
`OverflowPolicy` is `DROP_OLDEST` or `REJECT`; `gap_buffer_capacity == 0`
raises `ValueError`.

On the receive side, each reconnect emits exactly one
`DemuxEvent.ReconnectDiscontinuity` before the post-reconnect events —
drop your per-stream caches on receipt and rebuild from the next
`ProgramMap`:

```python
from tstrans.srt import ManagedDemuxReceiver
from tstrans.mpegts import DemuxEvent

with ManagedDemuxReceiver.from_url(
    "srt://host:9000?mode=caller&latency=120", policy=policy
) as rx:
    for event in rx:
        if isinstance(event, DemuxEvent.ReconnectDiscontinuity):
            caches.clear()     # transport rebuilt — re-derive from next ProgramMap
        elif isinstance(event, DemuxEvent.Video):
            ...
```

Unlike the plain `DemuxReceiver` (listener only), `ManagedDemuxReceiver`
accepts `mode=caller` too — it re-dials in caller mode and re-binds +
re-accepts in listener mode. `cancel_handle().cancel()` reaches every
phase of that reconnect: a live receive, the backoff wait between
attempts, and a re-accept parked with no peer in sight (the iterator
raises `SrtError(CLOSED)` promptly in all three). `ManagedReceiver`, the
raw-bytes sibling, covers the same three phases — a parked `recv_bytes`
raises `SrtError(CLOSED)` just as promptly. The one accept neither class
can cancel is the *first* one, inside `from_url` itself: the handle that
would fire it does not exist until the constructor returns. Take
`ManagedReceiver.cancel_handle()` *before* the first `recv_bytes` and
hand it to the thread that will do the cancelling — `recv_bytes` holds
the object's mutable borrow for the whole blocking call, so asking for
the handle while one is parked raises `RuntimeError: Already borrowed`.

**Why did a managed session end?** `end_reason()` (on `ManagedDemuxReceiver`
only — none of the other three managed shells expose it) answers with a
`RecvEndReason` member once the receive loop has stopped for good:
`END_OF_STREAM` (the peer sent a clean SRT end-of-stream),
`RECONNECT_EXHAUSTED` (the `ReconnectPolicy`'s `max_attempts` budget ran
out), or `CANCELLED` (`cancel_handle().cancel()` fired). Returns `None`
before any of those — including while a reconnect is still in progress,
which is not itself an ending. This is the recv-side analogue of the RTP
`StreamEndReason` covered above: it exists specifically to tell a
caller-initiated cancel apart from a budget-exhausted give-up, which
otherwise both surface identically as `SrtError(CLOSED)` from the
iterator.

**Stats drift on the managed shells** (mirrors the JVM binding):
`ManagedSender.srt_stats()` and `ManagedReceiver.srt_stats()` raise
`SrtError(IO)` today — the managed transport exposes no SRT-rich shape, so
use `socket_stats()` (the 16-field `SocketStats`) instead.
`ManagedDemuxReceiver.srt_stats()` does NOT throw — it returns a
`SocketStats` (not `SrtStats`). `ManagedReceiver.reconnect_attempts()` is a
success count (excludes the initial accept); `ManagedMuxSender` and
`ManagedDemuxReceiver` `reconnect_attempts()` count every reconnect-factory
invocation. `ManagedDemuxReceiver.last_seen_micros(pid)` works the same
as the plain `DemuxReceiver` above.

**Why did the managed stream end?** `ManagedDemuxReceiver.end_reason()`
returns a `tstrans.srt.RecvEndReason` once the receive session has ended,
or `None` while it is still live:

```python
from tstrans.srt import ManagedDemuxReceiver, RecvEndReason

with ManagedDemuxReceiver.from_url("srt://:7000?mode=listener") as rx:
    for event in rx:
        ...
    if rx.end_reason() is RecvEndReason.RECONNECT_EXHAUSTED:
        print("peer never came back within the policy budget")
```

Two of the three members are reachable on the managed-SRT path today:
`RECONNECT_EXHAUSTED` (the reconnect budget ran out — also what a plain
peer close reports under a zero-retry policy, because a peer FIN reaches
the reconnect decorator as a retryable break) and `CANCELLED` (you fired
`cancel_handle()` or `close()`). `END_OF_STREAM` is reserved for a future
transport that can signal a clean end-of-stream distinct from budget
exhaustion. The value is recorded first-writer-wins and read from a
lock-free cell captured at construction, so `end_reason()` still answers
after `close()` and is safe to poll from a watchdog thread while another
thread iterates. This is a dedicated enum, not `tstrans.rtp.StreamEndReason`
— the RTP and SRT reasons are different types with different members.

**Background reconnect** (`ReconnectMode`): pass `mode=ReconnectMode.BACKGROUND`
on the `ReconnectPolicy` handed to `ManagedSender` / `ManagedMuxSender` to
move the reconnect loop off the caller's thread — a dedicated per-outage
worker owns backoff + the factory call, and `send`/`send_video`/… enqueue
into the gap buffer under `overflow_policy` instead of blocking:

```python
from tstrans.srt import ManagedMuxSender, ReconnectPolicy, ReconnectMode

policy = ReconnectPolicy(mode=ReconnectMode.BACKGROUND)  # default is BLOCKING

with ManagedMuxSender.from_url(
    "srt://host:9000?mode=caller&latency=120", program, policy=policy
) as s:
    ...
    stats = s.reconnect_stats()   # ManagedTransportStats
    if stats.reconnecting:
        print(f"outage in progress, {stats.gap_len} messages queued")
```

`reconnect_stats()` (also on `ManagedSender`) returns a frozen
`ManagedTransportStats` (`reconnect_attempts`, `reconnect_successes`,
`gap_len`, `gap_messages_dropped`, `gap_bytes_dropped`, `reconnecting`)
regardless of `mode` — `reconnecting` just stays `False` under the
default `BLOCKING` mode, since that mode's reconnect runs synchronously
inside the call that observed the break rather than on a separate
worker. `BACKGROUND` is send-side only: handing it to `ManagedReceiver`
/ `ManagedDemuxReceiver` logs a warning and those classes reconnect on
the caller's thread anyway.

## RTP transport (`tstrans.rtp`)

`tstrans.rtp` wraps `tst_rtp` for two RTP receive shapes: **MPEG-TS-over-RTP
(RFC 2250, PT=33)** via the `Sender` / `Receiver` / `MuxSender` /
`DemuxReceiver` shells and the RTSP client/server, and **H.264-over-RTP
(RFC 6184)** via `H264Receiver` and `RtspClient.connect_h264` (see the
dedicated sub-section below). For the MPEG-TS path: each `send` produces one
RTP datagram (12-byte header + TS payload); each `recv` returns one
datagram's TS payload with the header stripped. Unlike SRT, the raw `Sender`
/ `Receiver` are constructed with `__init__`, not `from_url`. RTP ships in
published wheels (default-on). `RtpError` / `RtpErrorKind` and `RtspError` /
`RtspErrorKind` live in `tstrans.exceptions`.

### Sender / Receiver hello

```python
from tstrans.rtp import Sender, Receiver

# pkt_size caps the datagram (RTP header + TS payload); ssrc pins the SSRC.
with Sender("rtp://239.0.0.1:5004", pkt_size=1316) as tx:
    tx.send(ts_bytes)        # one UDP datagram per call

# Multicast bind URLs auto-join the group.
with Receiver("rtp://239.0.0.1:5004") as rx:
    payload = rx.recv()      # one datagram's TS payload (RTP header stripped)
```

`send` accepts a TS payload up to `pkt_size − 12`; oversize raises
`RtpError(MALFORMED_PACKET)`. `recv()` blocks until a datagram arrives or a
cancel fires. RTP/UDP is connectionless — a remote sender closing does NOT
end a `recv()` loop; stop on a sentinel or via `cancel_handle().cancel()`,
which wakes a parked `send` / `recv` with `RtpError(CANCELLED)` within
~100 ms. The RTP `CancelHandle` has only `cancel()` — no `is_cancelled()`
(differs from SRT). A literal `rtp://host:0` receiver binds (the kernel
picks an ephemeral port; RTCP is off by default on this surface), but it
is impractical: the raw receiver exposes no local-address getter, so
there is no way to learn which port the kernel chose and no sender can
be pointed at it — bind a concrete port. (The H.264 receiver surface
differs: `H264Receiver.local_addr()` reports its bound address.)

### RTP convenience (`MuxSender` / `DemuxReceiver`)

`MuxSender` takes the same elementary-stream send surface as the SRT
shell, constructed with `MuxSender(url, program_config, *,
pkt_size=1316)`; `DemuxReceiver` iterates `DemuxEvent`s and is constructed
with `DemuxReceiver(url, *, demux_config=None)`. Both expose **no
`cancel_handle()` and no `socket_stats()`** — only `stats()` (a
`(SocketStats, MuxerStats)` tuple) and `close()`:

```python
from tstrans.rtp import MuxSender, DemuxReceiver
from tstrans.mpegts import (
    DemuxEvent,
    MuxerProgramConfigBuilder,
    Pts90khz,
    VideoCodec,
)

program = (
    MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x1000)
    .add_video(0x1011, VideoCodec.H264)
    .build()
)
with MuxSender("rtp://127.0.0.1:5004", program) as s:
    s.send_video(nal, pts=Pts90khz.from_raw(0), key_frame=True)

with DemuxReceiver("rtp://0.0.0.0:5004") as rx:
    rx.add_byte_sink(lambda pkt: record(pkt))   # each raw 188-byte TS packet
    for event in rx:
        if isinstance(event, DemuxEvent.Video):
            ...
```

To stop a `DemuxReceiver` iteration parked on the next datagram, call
`close()` from another thread — it cancels the in-flight recv first, then
frees the receiver (safe cross-thread).

`rx.last_seen_micros(pid)` returns the Unix-epoch microsecond timestamp
of the most recent item this receiver emitted for `pid`, or `None` if
that PID has never been seen — a per-stream staleness check that needs
no `stats()` snapshot-diffing. A watchdog thread can poll it directly:

```python
import time

STALE_AFTER_US = 5_000_000  # 5 s

def watchdog(rx, pid, stop_event):
    while not stop_event.is_set():
        seen = rx.last_seen_micros(pid)
        if seen is not None and time.time() * 1_000_000 - seen > STALE_AFTER_US:
            print(f"pid {pid:#x} has gone quiet")
        time.sleep(1.0)
```

The same method exists on `tstrans.srt.DemuxReceiver` and
`tstrans.srt.ManagedDemuxReceiver`.

### Recv deadlines (`?recv_timeout=` / `timeout_ms`)

Both `Receiver.recv()` and `DemuxReceiver` iteration block indefinitely
by default — fine for a live camera, less fine for a quiet socket you
want to notice going quiet. `?recv_timeout=<ms>` on a `rtp://` (or
`rtsp(s)://`) URL arms a persistent receive deadline: a `recv()` /
`next()` call that would otherwise block forever instead raises
`RtpError(TIMEOUT)` after `<ms>` milliseconds of silence, and the
receiver stays open — call again to keep waiting:

```python
from tstrans.exceptions import RtpError, RtpErrorKind
from tstrans.rtp import DemuxReceiver

with DemuxReceiver("rtp://0.0.0.0:5004?recv_timeout=5000") as rx:
    it = iter(rx)
    while True:
        try:
            event = next(it)
        except RtpError as e:
            if e.kind == RtpErrorKind.TIMEOUT:
                print("quiet for 5s — still connected, just nothing to say")
                continue
            raise
        ...  # process event
```

`Receiver.recv(timeout_ms=...)` and `H264Receiver.recv_au(timeout_ms=...)`
take a per-call override instead (or in addition — the explicit argument
always wins over a configured URL deadline for that one call). `RtspClientConfig`'s
URL accepts the same `?recv_timeout=` key; it carries through
`into_demux_receiver()` / `into_h264_receiver()` automatically. `TIMEOUT`
is retryable — the transport and session are both still alive, unlike
`CANCELLED` or `TRANSPORT`.

### RTSP client

`RtspClient.connect(config)` runs OPTIONS / DESCRIBE / SETUP / PLAY and
returns a live `RtspSession`; `RtspSession.into_demux_receiver()` hands you
the RTP data plane as a `DemuxReceiver`. Auth is `BasicAuth(user,
password, realm=None)` or `DigestAuth(user, password, algorithm=...,
realm=None)`:

```python
from tstrans.rtp import (
    RtspClient,
    RtspClientConfig,
    DigestAuth,
    TransportPref,
)

cfg = RtspClientConfig(
    "rtsp://cam.local:554/stream1",
    auth=DigestAuth("admin", "secret"),     # BasicAuth | DigestAuth | None
    transport_pref=TransportPref.AUTO,      # UDP-first, TCP fallback on 461
)

with RtspClient.connect(cfg) as session:
    with session.into_demux_receiver() as rx:
        for event in rx:
            ...     # DemuxEvent.Video / .Metadata / ...
    # session.play() / session.pause() / session.teardown() drive RTSP state.
```

- **Passwords stay Rust-side.** `BasicAuth` / `DigestAuth` hold the
  password in Rust memory (never re-exposed); only `user`, `realm`, and
  `algorithm` are readable. `repr()` redacts the secret.
- **TLS (`rtsps://`) is compiled in** (wheels ship the `tls` feature).
  Server certificates verify against platform native trust roots by
  default; pass `tls_root_certs_pem=<PEM bytes>` to trust a private CA /
  self-signed camera cert instead.
- **Cancellation.** Obtain a `RtspCancelHandle` from
  `session.cancel_handle()` *before* a blocking control call, then flip
  `cancel()` from another thread to break it out.
- **Why did the stream end?** When `for event in rx` exits or a call
  raises, `rx.end_reason()` (on `DemuxReceiver`, `Receiver`, and
  `H264Receiver` alike) answers with a `StreamEndReason` member —
  `CLEAN_TEARDOWN`, `SESSION_EXPIRED`, `KEEPALIVE_FAILED`,
  `TRANSPORT_FAILED`, `PROTOCOL_ERROR`, or `CANCELLED` — or `None` if
  the session hasn't ended yet (or ended through a path this arc
  doesn't instrument, e.g. a plain `rtp://` receiver with no owning
  `RtspClient`). `rx.end_detail()` carries the free-text message for
  the three failure variants. Both stay readable after `close()`. Set
  `TSTRANS_LOG=tst_rtp=debug` before `import tstrans` to also see the
  underlying pump/keepalive `tracing` events on stderr. Full recipe +
  the Rust-side rationale: [Why did my RTSP stream end?](/docs/troubleshooting.md#why-did-my-rtsp-stream-end).

### RTSP server

`RtspServer.start(config)` hosts a server; `add_unicast_mount(path,
program_config)` / `add_multicast_mount(path, group, port, *,
program_config=...)` register mounts, and the returned `MountHandle` exposes an
elementary-stream push family (`push_video` / `push_klv` / … / `push_data`,
plus handle-targeted `push_*_to` variants and per-kind handle accessors).
Note that `MountHandle` retains the `push_*` naming convention (buffer op),
while `MuxSender` uses `send_*` (transport-bound op). The `MountHandle` is
`Arc`-shared, so multiple producer threads may push concurrently.

```python
from tstrans.rtp import RtspServer, RtspServerConfig
from tstrans.mpegts import MuxerProgramConfigBuilder, Pts90khz, VideoCodec

program = (
    MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x1000)
    .add_video(0x1011, VideoCodec.H264)
    .build()
)

cfg = RtspServerConfig(bind_addr="0.0.0.0:8554")   # defaults: max_sessions=100, …
with RtspServer.start(cfg) as server:
    mount = server.add_unicast_mount("/live", program)
    mount.push_video(nal, pts=Pts90khz.from_raw(0), key_frame=True)
    print(server.local_addr())     # bound "ip:port" (useful with port 0)
# __exit__ sends a graceful Notice-5402 teardown to active sessions.
```

`RtspServerConfig` is a frozen dataclass (`bind_addr`, `auth`,
`max_sessions`, `session_timeout_secs`, `fanout_capacity`,
`graceful_shutdown_drain_ms`, `tls_cert`, `tls_key`). For an `rtsps://`
server, bind `bind_addr="rtsps://host:port"` and set `tls_cert` /
`tls_key` to PEM certificate-chain / private-key file paths (set
together, or `__post_init__` raises `ValueError`; unreadable paths raise
`RtspError(TLS)` at `start()`). For a credentialed server pass
`auth=BasicAuth("user", "pass", "realm")` — the realm is required
server-side. `server.cancel_handle()` returns an `RtspServerCancelHandle`
for an immediate hard teardown that bypasses the drain window.

### H.264-over-RTP ingest (RFC 6184)

`tstrans.rtp` ships `H264Receiver` and its companions for ingesting bare
H.264 elementary streams over RTP/RTSP (RFC 6184 — single-NALU, STAP-A,
FU-A; packetization modes 0 and 1). This covers cameras that expose H.264
directly rather than wrapping video in MPEG-TS first. The gateway pattern
— receive AUs and re-mux them into MPEG-TS — is the headline use case:

```python
from tstrans.rtp import RtspClient, RtspClientConfig
from tstrans.mpegts import (
    Muxer, MuxerConfigBuilder, MuxerProgramConfigBuilder, VideoCodec, Pts90khz,
)

cfg = RtspClientConfig("rtsp://cam.local/h264")
# connect_h264 runs OPTIONS/DESCRIBE/SETUP/PLAY and stashes the negotiated
# H264DepayConfig (payload type + sprop-parameter-sets NALUs from the SDP).
with RtspClient.connect_h264(cfg) as session:
    # into_h264_receiver() keeps the session alive (pause/play still work).
    with session.into_h264_receiver() as rx:
        prog = (
            MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x100)
            .add_video(0x101, VideoCodec.H264)
            .build()
        )
        mux = Muxer(MuxerConfigBuilder().add_program(prog).build())
        buf = bytearray(1316)
        for au in rx:                              # recv_au(), GIL released
            # au.pts is a 90 kHz decode-order timestamp — same clock as
            # MPEG-TS PTS, no rescaling needed.
            mux.push_video(au.annexb, pts=Pts90khz.from_raw(au.pts), key_frame=au.key_frame)
            while (n := mux.pull(buf)) > 0:
                pass   # write buf[:n] to file / SRT / etc.
```

**Types:**

| Name | Notes |
|---|---|
| `H264Receiver` | `listen(url, config=None)` for direct UDP; `into_h264_receiver()` from an RTSP session. `recv_au()` → `H264AccessUnit \| None`. Also iterable: `for au in rx`. |
| `H264AccessUnit` | `annexb: bytes`, `pts: int` (90 kHz ticks, i64), `key_frame: bool`, `rtp_timestamp: int`. |
| `H264DepayConfig` | `payload_type=96`, `parameter_set_injection=ParameterSetInjection.BEFORE_IDR`, `initial_parameter_sets=[]`, `max_au_bytes=8388608`. All kwargs. |
| `ParameterSetInjection` | `.NONE` — pass through as received; `.BEFORE_IDR` (default) — prepend cached SPS/PPS before each IDR. |
| `H264DepayStats` | `aus_emitted`, `aus_dropped`, `seq_gaps`, `parameter_set_updates`, `ssrc_changes`, … (9 counters). |
| `RtpStats` | `malformed_packets`. |

**Integration notes:**

- **`sprop-parameter-sets` handled automatically.** `connect_h264` decodes
  the SDP `a=fmtp` attribute and stores the SPS/PPS NALUs in the
  `H264DepayConfig` stashed on the session. With `BeforeIdr` (the default),
  the depacketizer prepends them before every IDR frame — a decoder gets a
  clean start even when the camera omits in-band parameter sets.

- **`into_h264_receiver()` — session stays usable.** Unlike the JVM binding,
  the Python `into_h264_receiver()` does **not** consume the session object;
  `session.pause()` / `session.play()` remain available after the receiver is
  created and while it is live.

- **B-frame / DTS limitation.** `au.pts` is derived from the RTP timestamp
  (decode order). Most live cameras do not use B-frames — PTS = DTS. If the
  source uses B-frames, pass `au.pts` as the PTS and supply a separately-
  derived DTS via the relevant muxer overload.

- **Loss behavior.** A sequence-number gap drops the open AU and increments
  `rx.depay_stats().aus_dropped` and `.seq_gaps`. Loss is whole-AU only.

- **KLV pairing slot.** For a STANAG 4609 gateway, push KLV (from a
  separate UDP feed or a second RTSP m-line) to the same muxer using
  `mux.push_klv(klv_bytes, pts=Pts90khz.from_raw(au.pts), metadata_service_id=0x00)`.
  Pair by PTS using `Pairer` if the feeds are asynchronous.

- **RTCP is not implemented on the H.264 path (v1 decision).** No RTCP
  socket is bound; no RR/SR is sent or received.

- **Stats accessors:** `rx.depay_stats()`, `rx.rtp_stats()`,
  `rx.socket_stats()` (returns a `SocketStats` — not Optional, unlike the
  SRT path).

- **Deadlines and end-of-stream diagnosis.** `rx.recv_au(timeout_ms=...)`
  bounds a single call — see [Recv deadlines](#recv-deadlines-recv_timeout--timeout_ms)
  above. `rx.end_reason()` / `rx.end_detail()` work the same as on
  `DemuxReceiver` / `Receiver` — see
  [Why did the stream end?](#rtsp-client) under RTSP client above.

**Rust twin:** [`examples/receiving/recv_rtsp_h264.rs`](/examples/receiving/recv_rtsp_h264.rs)
and the cookbook recipe [Ingest H.264 from an RTSP camera and remux to MPEG-TS](/docs/cookbook/receiving/recv-rtsp-h264-to-ts.md).

## UDP / TCP / RIST transports

These three are the raw datagram / stream transports — lighter than SRT or
RTP (no muxer shells), used to move pre-muxed TS bytes. Each exposes a
fluent builder. All three ship in published wheels (RIST on Linux + macOS
only — see the caveat under [Install](#install)). Their error types
(`UdpError`, `TcpError`, `RistError` and the matching `*ErrorKind`
`IntEnum`s) live in `tstrans.exceptions` and are also re-exported from each
submodule.

### UDP (`tstrans.udp`)

```python
from tstrans.udp import Transport, RecvTransport

# Sender — fixed peer.
with Transport.builder().url("udp://239.0.0.1:5000").ttl(8).build() as tx:
    tx.send(ts_bytes)                       # one datagram; default cap 1316 bytes

# Receiver — bind, then recv (timeout_ms=None blocks).
with RecvTransport.builder().bind_url("udp://@239.0.0.1:5000").build() as rx:
    payload, _addr = rx.recv(timeout_ms=1000)   # raises UdpError(IO) on timeout
```

`recv()` returns `(payload, sender_addr)` — the address string is
currently always `""`. Use `udp://@group:port` (the `@` prefix) on
`bind_url` for a multicast join; `RecvTransport.local_addr_port()` reads
back an ephemeral port when you bind `:0`.

### TCP (`tstrans.tcp`)

`Transport` is full-duplex (send + recv on one handle); a `Listener`
accepts inbound connections:

```python
from tstrans.tcp import Transport, Listener

# Client.
with Transport.builder().url("tcp://host:5001").nodelay(True).build() as conn:
    conn.send(ts_bytes)
    buf = bytearray(64 * 1024)
    n = conn.recv(buf)                       # bytes read into buf

# Server.
with Listener.builder().bind("0.0.0.0:5001").build() as listener:
    conn = listener.accept_blocking()        # -> Transport
    with conn:
        n = conn.recv(buf := bytearray(64 * 1024))
```

`tcps://` is compiled into the published wheels: callers verify against
native trust roots, or a custom CA with `tcps://host:port?ca=<pem path>`;
listeners serve TLS via `Listener.builder().bind("host:0").tls(cert,
key)` (PEM file paths). `Listener.builder().bind("host:0")` plus
`local_port()` gives you an ephemeral port.

### RIST (`tstrans.rist`)

```python
from tstrans.rist import Transport, RecvTransport, EncryptionKey

# Sender.
with Transport.builder().url("rist://host:5004").build() as tx:
    tx.send(ts_bytes)

# Receiver — the @ prefix is REQUIRED on bind_url.
with RecvTransport.builder().bind_url("rist://@0.0.0.0:5004").build() as rx:
    payload = rx.recv(timeout_ms=1000)       # raises RistError(RECV_TIMEOUT)

# Encryption forces RistProfile.MAIN.
enc = EncryptionKey.aes256("pre-shared-secret")
tx = Transport.builder().url("rist://host:5004").encryption(enc).build()
```

The `@` prefix on `bind_url` is mandatory (per librist / ffmpeg
convention); omit it on the sender's `url`. Mixing them up raises
`RistError(INVALID_CONFIG)`. `EncryptionKey.aes128` / `aes192` / `aes256`
each force `RistProfile.MAIN` (the profile that carries encryption); the
default is `RistProfile.SIMPLE`. Both transports return a `RistStats`
snapshot from `stats()`.

## HLS publisher (`tstrans.hls`)

> **Ships in the wheels.** `tstrans.hls` imports out of the box from a
> published wheel (the `hls` feature is default-on). It is unavailable only
> in a `--no-default-features` source build that omits `hls`, where
> `import tstrans.hls` raises `ImportError`. See the
> [HLS guide](/docs/guides/hls.md) for serving guidance, the KLV
> ride-along carriage modes, and latency tuning.

`HlsPublisher` segments pre-muxed MPEG-TS to disk and serves an HTTP
playlist; `MuxPublisher` is the pipeline shell that owns a muxer + an
`HlsPublisher` so you can push elementary streams instead of TS bytes:

```python
from tstrans.hls import HlsPublisher, MuxPublisher
from tstrans.mpegts import MuxerProgramConfigBuilder, Pts90khz, VideoCodec

publisher = (
    HlsPublisher.builder()
    .bind("127.0.0.1:0")               # 0 = ephemeral; read back via local_addr()
    .output_dir("/tmp/hls")
    .segment_duration_ms(2000)
    .build()
)

program = (
    MuxerProgramConfigBuilder(program_number=1, pmt_pid=0x1000)
    .add_video(0x1011, VideoCodec.H264)
    .build()
)

# with_config_hls CONSUMES the publisher.
mp = MuxPublisher.with_config_hls(publisher, program)
mp.send_video(nal, pts=Pts90khz.from_raw(0), key_frame=True)  # auto-cuts on key_frame
pub = mp.finish_into_publisher()   # recover the publisher to finish() it cleanly
pub.finish()
```

`push_ts` (on the publisher) requires a 188-multiple and raises
`HlsError(UNALIGNED_PUSH_TS)` otherwise; once consumed (by `finish()` or
`with_config_hls`) further calls raise `HlsError(FINISHED)`.
`HlsMode.LIVE` / `EVENT` / `VOD` selects playlist behavior.

### Keeping a completed VOD / EVENT playlist served

`finish()` writes the terminal playlist and tears the server down, so a VOD
stops being fetchable when the stream ends. Call `finish_serving()` instead
to finalize the playlist but keep the built-in HTTP server up — it returns
an `HlsServerHandle` with `local_addr()` and `shutdown()`:

```python
pub = mp.finish_into_publisher()
handle = pub.finish_serving()      # #EXT-X-ENDLIST written; server keeps serving
print("VOD at http://%s/playlist.m3u8" % handle.local_addr())
# ... hold the handle while clients should be able to fetch ...
handle.shutdown()
```

The full `Publisher` ABC surface exposed through `tstrans.hls`:

| Method | Type | Notes |
|---|---|---|
| `push_ts(data: bytes) -> None` | `HlsPublisher` | 188-multiple buffer; raises `HlsError(UNALIGNED_PUSH_TS)` otherwise |
| `cut_segment() -> None` | `HlsPublisher` | Wall-clock segment cut hint; call on keyframe boundaries |
| `cut_segment_with_duration(media_duration_us: int) -> None` | `HlsPublisher` | Media-presentation-time-derived cut; `media_duration_us` is the PTS span of the completed segment in µs (same unit as `PublisherStats.*_us`); used by `MuxPublisher` internally |
| `finish() -> None` | `HlsPublisher` | Flush pending segment + write `#EXT-X-ENDLIST`; terminal |
| `stats() -> PublisherStats` | `HlsPublisher` / `MuxPublisher` | Cross-publisher stats snapshot |

## Pipeline pairing (`tstrans.pipeline.Pairer`)

MPEG-TS programs that carry synchronized KLV (e.g. MISB ST 0601 UAS
Datalink) multiplex video on one PID and KLV on another, both timestamped
against the same 90 kHz clock but arriving in separate PES packets.
`Pairer` — wrapping the Rust core `PairingDemuxer` — correlates the two by
PTS without exposing demux events across the FFI boundary: you feed raw TS
bytes and get back `PairerOutput`s. `Pairer` is **not** a context manager
and is single-threaded (the consumer owns concurrency).

```python
from datetime import timedelta
from tstrans.pipeline import (
    Pairer,
    PairerConfig,
    PairerMode,
    PairerOutput,
    PairingDemuxerConfig,
)

video_pid, klv_pid = 0x101, 0x102

# Realtime nearest-PTS pairing, 100 ms tolerance. config=None → defaults
# (Realtime, 300 ms tolerance, 32/32 buffers).
cfg = PairingDemuxerConfig(
    pairer=PairerConfig(
        mode=PairerMode.Realtime,
        tolerance=timedelta(milliseconds=100),
    ),
)

pairer = Pairer(video_pid, klv_pid, cfg)
outputs = pairer.feed(ts_bytes)
outputs += pairer.flush()              # drain end-of-stream (trailing UnpairedKlv; buffered video too)

for out in outputs:
    match out:
        case PairerOutput.Paired(video=v, klv=k):
            # v.codec, v.raw (bytes); v.parse() returns list[NalUnit] | list[Obu]; k.payload (bytes)
            ...
        case PairerOutput.UnpairedVideo(video=v):
            ...
        case PairerOutput.UnpairedKlv(klv=k):
            ...
        case PairerOutput.PassThrough(event=ev):
            ...   # a tstrans.mpegts.DemuxEvent.* instance (ProgramMap, off-PID, …)

print(pairer.stats())   # {'paired': N, 'unpaired_video': N, 'unpaired_klv': N, 'pass_through': N}
```

The simplest form — `Pairer(video_pid, klv_pid)` — uses all defaults. To
tolerate arrival skew, switch to Buffered mode by passing
`mode=PairerMode.Buffered(max_lag=timedelta(milliseconds=200))` to
`PairerConfig`; `flush()` is most load-bearing in Buffered mode, where
buffered samples are held until the lag window closes. Call `flush()` at
end-of-stream in **either** mode, though: it drains any unused KLV history as
trailing `UnpairedKlv` (e.g. metadata that arrived after the last video access
unit), so skipping it can drop tail metadata. `feed` and `flush`
each return a `list[PairerOutput]`; `feed` raises
`tstrans.exceptions.DemuxError` on non-conformant input. `stats()` and
`demuxer_stats()` return dicts; `reset_stats()` zeroes the pairing
counters without touching demuxer stats.

## Language-specific gotchas

- **GIL released in `push_*` methods.** Long-running CPU work (large NAL
  parses, big KLV blobs) doesn't block other Python threads. The
  `add_subtitle()` and `push_subtitle*()` methods also release the GIL
  (added in plan #96 Wave C).
- **Subtitle config dataclasses reject `bool`-as-`int`.** PyO3 strictness
  means `True` is not silently coerced to integer `1` for fields that
  expect an integer codec selector. Same with `bytearray` vs `bytes`.
  (Came from plan #96 validation pass.)
- **`MuxerFileSink` is a context manager — push on the proxy it
  yields.** Use `with m.write_file("out.ts") as proxy: ...` and route
  every `push_*` through `proxy`. Only proxy pushes drain to the file;
  pushing on the original Muxer (`m.push_video(...)`) inside the block
  bypasses the per-push drain and raises `MuxError(BACKPRESSURE)` once
  `buffer_packets` (default 10 000) accumulate — a footgun that only
  fires in long push loops. The `__exit__` flushes + finalizes the
  file. No explicit `close()` ceremony is needed (and a double-close on
  the underlying handle would panic).
- **Video / Audio events are raw-first; parsing is opt-in.** A
  `DemuxEvent.Video` / `DemuxEvent.Audio` carries `.raw` (the exact encoded
  bytes). Call `ev.parse()` to get typed units: for H.264 / H.265 / H.266
  video it's `list[NalUnit]`; for AV1 it's `list[Obu]`; for AAC ADTS it's
  `list[AdtsFrame]`; for MPEG-2 Audio it's `list[Mpeg2AudioFrame]`. For
  subtitles + AAC-LATM + AC-3 there's no typed frame parser yet — use `.raw`
  directly (AC-3 carriage is still sync-validated on demux). The
  free functions `tstrans.codec.split_units(raw, codec)` and
  `tstrans.codec.parse_audio(raw, codec)` do the same split and additionally
  return the conformance-issue list.
- **abi3 build limitation.** `bytes`-like extraction uses a two-path
  approach (one for true `bytes`, one for `memoryview` / `bytearray`)
  because PyO3's abi3 doesn't expose a unified buffer protocol. The
  Python API is uniform — you can pass `bytes`, `bytearray`, or a
  `memoryview` and it works.
- **`tstrans._native` is private.** Use `tstrans.X` (or `tstrans.mpegts.X`
  / `tstrans.klv.X` / ...) — never `tstrans._native.X`. The `_native`
  submodule may reorganize between versions.
- **`TSTRANS_LOG` bridges the Rust core's `tracing` events to stderr.**
  Set it before `import tstrans` (`EnvFilter` syntax, same as
  `RUST_LOG` — e.g. `TSTRANS_LOG=tst_rtp=debug`) to see diagnostics
  (keepalive failures, pump warnings, …) that otherwise vanish silently
  — nothing installs a subscriber inside the extension by default.
  Unset means zero overhead beyond the one env lookup at import time;
  if the embedding process already installed its own `tracing`
  subscriber, this bridge never displaces it.

### Pandas + NumPy adapters

Optional pandas DataFrame adapters and NumPy snapshot views (one
Rust-to-Python `bytes` copy per access; see [Snapshot vs zero-copy](#snapshot-vs-zero-copy)
below) for the `tstrans` Python package. Requires the `[pandas]` extra:

```bash
pip install 'tstrans[pandas]'
```

Without the extra, `tstrans` works as documented in the core modules above. Calling any pandas adapter or any
NumPy `.payload_np` / `.raw_rbsp_np` / `.raw_np` accessor without the
extra raises:

```
ImportError: tstrans pandas adapters require: pip install 'tstrans[pandas]'
```

#### Quick start

```python
import tstrans.io
import tstrans.pandas

# Parse a .ts file into events
events = list(tstrans.io.parse_file("capture.ts"))

# Convert to DataFrame for analysis
df = tstrans.pandas.events_to_dataframe(events)
print(df.kind.value_counts())
#  Sample                  1234
#  Metadata                  56
#  ProgramMap                12
```

#### DataFrame adapters

##### KLV records — `klv_to_dataframe`

```python
from tstrans.io import extract_klv

records = list(extract_klv("capture.ts", parsed=True))
df = tstrans.pandas.klv_to_dataframe(records)
df.head()
```

`klv_to_dataframe` is polymorphic — it dispatches on the record type
and produces a per-set schema. Input must be homogeneous (one set type
per call); mixed input raises `TypeError`. Supported types: `UasDatalinkLs`
(ST 0601), `SecurityLs` (ST 0102), `PrecisionTimeStampPack` (ST 0605),
`VmtiLs` (ST 0903).

KLV DataFrames are indexed by `pd.DatetimeIndex` (with `tz="UTC"`,
named `pts`) derived from the per-record timestamp where present:
ST 0601 / ST 0903 use the `timestamp_us` field (microseconds since
the 1970 UTC epoch), ST 0605 uses its own precision timestamp. If a
record lacks a timestamp the row's index entry is `pd.NaT`; if NO
record in the batch has one the DataFrame falls back to
`pd.RangeIndex`.

**Column shape.** ST 0601 (UasDatalinkLs) flattens to its full set of
~50 scalar fields — fields like `frame_center_lat_deg`,
`frame_center_lon_deg`, `frame_center_elev_m`, `sensor_lat_deg`,
`sensor_lon_deg`, `sensor_alt_m`, `platform_heading_deg`,
`platform_pitch_deg`, `platform_roll_deg` are direct top-level columns
(no dotted composite namespacing). Enum-valued fields collapse to their
variant name string (e.g. `"FullyEncrypted"`). Per-field parse errors
(Phase 3 `KlvFieldError`) collapse to a single string `field_errors`
column using a `|` joiner with the per-error format
`tag<N>:<kind>:<message>` — the `|` (not `,`) joiner keeps the column
parseable even when an error `message` contains commas.

**ST 0903 (VmtiLs) supports two modes:**

- `mode="summary"` (default): one row per VMTI record, with a
  `num_targets` column counting `VTargetPack` entries. Indexed by
  `pd.DatetimeIndex` of record timestamps.
- `mode="targets"`: one row per `VTargetPack`, indexed by
  `pd.MultiIndex` with levels `[pts, target_id]`.

```python
# Aggregate targets across the full capture
targets = tstrans.pandas.klv_to_dataframe(vmti_records, mode="targets")
```

##### DemuxEvents — `events_to_dataframe`

```python
df = tstrans.pandas.events_to_dataframe(events)
```

Union schema across all event kinds. Video / Audio / Subtitle events
collapse to `kind="Sample"`; KLV events collapse to `kind="Metadata"`;
ProgramMap / NonConformant / Discontinuity / ReconnectDiscontinuity
keep their own labels. (`Pat` is folded into `ProgramMap` by the
demuxer; it never appears as a separate kind.)

| Column | Type | Description |
|---|---|---|
| kind | str | `Sample` / `Metadata` / `ProgramMap` / `NonConformant` / `Discontinuity` / `ReconnectDiscontinuity` |
| pts_raw | u64 | `Pts90khz.raw` ticks |
| pts_ms | float | `Pts90khz.ms` (PTS in milliseconds) |
| dts_ms | float | DTS in ms (Sample events that carry it; otherwise NaN) |
| pid | u16 | Source PID (NaN for global events) |
| stream_type | str | `StreamKind` variant name (`Video` / `Audio` / `Klv` / `Subtitle`) |
| codec | str | Codec tag (`H264` / `H265` / `H266` / `Av1` / `Aac` / `Mpeg2Audio` / `WebVtt` / ...) |
| payload_len | int | byte length of the event payload — `len(raw)` for video / audio rows, `len(payload)` for KLV / subtitle rows |
| nal_count | int | Video-only — the per-AU unit count, obtained by running the opt-in `event.parse()` on each `_VideoEvent` row (NAL units, or OBUs for AV1). NaN on audio rows and on non-Sample rows |
| random_access | bool | TS adaptation-field RAI bit (video samples) |
| has_codec_parse_error | bool | Vestigial column, always `None` under the raw-first surface — the eager `codec_parse_error` field was dropped; conformance issues now surface via `event.parse(strict=True)` / `tstrans.codec.split_units`. Kept for schema stability. |
| issue | str | `NonConformant` event's issue text |
| issue_kind | str | `NonConformant` event's `.kind` enum variant name |

Payloads themselves stay on the original event objects — they're not
materialised in the DataFrame.

##### NAL / OBU lists — `nals_to_dataframe` / `obus_to_dataframe`

```python
# Extract NALs from a single video Sample (opt-in split via `.parse()`)
sample = next(e for e in events if type(e).__name__ == "_VideoEvent")
df = tstrans.pandas.nals_to_dataframe(sample.parse(), pts=sample.pts.ms)
df.nal_type_name.value_counts()
```

NAL type names are decoded via H.264 §Table 7-1 / H.265 §Table 7-1 /
H.266 V4 §Table 5 lookup keyed on `nal.kind`. Unknown types fall back
to `unknown_{n}`.

Columns: `kind`, `nal_type`, `nal_type_name`, `ref_idc` (H.264 only;
NaN elsewhere), `layer_id` (H.265/H.266 only; NaN on H.264),
`temporal_id_plus1`, `payload_len`, and `pts_ms` if the optional `pts`
argument was supplied.

```python
# AV1 sample (`.parse()` returns the OBU list)
df = tstrans.pandas.obus_to_dataframe(sample.parse(), pts=sample.pts.ms)
```

OBU schema: `obu_type`, `obu_type_name`, `temporal_id`, `spatial_id`
(both from the optional OBU extension; NaN when absent), `payload_len`,
and `pts_ms` if supplied.

##### Audio frames — `audio_frames_to_dataframe`

```python
from tstrans.codec import parse_aac_frames

frames = parse_aac_frames(buf)
df = tstrans.pandas.audio_frames_to_dataframe(frames)
df.plot(x="byte_offset", y="frame_length_bytes")
```

Polymorphic — detects `AdtsFrame` vs `Mpeg2AudioFrame` from the first
element. Mixed-type input raises `TypeError`. Enum-valued fields
collapse to their bare variant name (e.g. `"LC"`, not `"AacProfile.LC"`;
`"III"` for MPEG-2 Audio Layer III; `"JOINT_STEREO"` for the channel
mode). Struct-valued `AacChannelLayout` is kept as its `repr`.

`byte_offset` is the running cumulative offset of each parsed frame
inside the input buffer, computed by summing `frame_length_bytes`
from zero. For inputs produced by `parse_*_frames_with_resync`, this
does NOT account for skipped (garbage) bytes between recovered frames
— if you need absolute offsets across a resync boundary, pre-compute
them from the resync output itself.

#### NumPy snapshot views

Every byte-bearing class (NalUnit, Obu, AdtsFrame, Mpeg2AudioFrame, all
H.264/H.265/H.266 SPS/PPS/VPS/SliceHeaderLight, AV1 sequence/frame
headers) carries a `.payload_np` / `.raw_rbsp_np` / `.raw_np` accessor
that returns a `numpy.ndarray(dtype=uint8)` snapshot — each access
copies from Rust-owned storage into a fresh Python `bytes`, which
NumPy then views without further copy:

```python
import numpy as np
from tstrans.codec import parse_h264_sps

sps = parse_h264_sps(rbsp_bytes)
arr = sps.raw_rbsp_np   # snapshot np.ndarray(dtype=np.uint8)
```

Mapping:

- `.payload_np` — `NalUnit`, `Obu`, `AdtsFrame`, `Mpeg2AudioFrame`
- `.raw_rbsp_np` — H.264 / H.265 / H.266 `Sps` / `Pps` / `Vps` /
  `SliceHeaderLight`
- `.raw_np` — `Av1SequenceHeader`, `Av1FrameHeaderLight` (the field is
  named `raw`, not `raw_rbsp`)

These accessors are **read-only** views — `np.frombuffer` sets
`writeable=False` on Python `bytes`. Mutating attempts raise
`ValueError: assignment destination is read-only` by design.

##### Snapshot vs zero-copy

Each `.payload_np` / `.raw_rbsp_np` / `.raw_np` access materializes a
fresh Python `bytes` from Rust-owned storage (one copy), then NumPy
views that bytes object with no further copy. Per-access cost is
`O(payload_length)`; the view itself is a true zero-copy view over the
bytes object, but the bytes object is freshly allocated each time. For
repeated access on the same frame/NAL, cache the result manually:

```python
arr = nal.payload_np  # one copy from Rust
# use `arr` repeatedly — no further copy
```

A future plan may implement the Python buffer protocol directly on the
Rust types, eliminating the bytes copy. This is non-trivial because
each of the ~15 PyClass types would need `__getbuffer__` /
`__releasebuffer__` magic methods over stable Rust-owned storage.
Tracked as a v2 optimization.

For users who don't want the `.payload_np` indirection, the snapshot
is one line of stdlib NumPy:

```python
import numpy as np
arr = np.frombuffer(nal.payload, dtype=np.uint8)
```

Both forms are equivalent.

#### Common recipes

##### Plot platform altitude over time

```python
df = tstrans.pandas.klv_to_dataframe(uas_records)
df["sensor_alt_m"].plot()
# Or, if you want the framed-scene centre instead of the sensor itself:
df["frame_center_elev_m"].plot()
```

##### Filter Sample events by codec

```python
df = tstrans.pandas.events_to_dataframe(events)
h264_samples = df[(df.kind == "Sample") & (df.codec == "H264")]
```

##### NAL type histogram across an entire capture

```python
all_nals = []
for ev in events:
    if type(ev).__name__ == "_VideoEvent":
        all_nals.extend(ev.parse())  # opt-in split of each AU into NAL units
df = tstrans.pandas.nals_to_dataframe(all_nals)
df.nal_type_name.value_counts().plot.bar()
```

##### Audio frame-length over byte offset

```python
frames = list(parse_aac_frames(buf))
df = tstrans.pandas.audio_frames_to_dataframe(frames)
df.set_index("byte_offset")["frame_length_bytes"].plot()
```

#### Troubleshooting

**`TypeError: klv_to_dataframe requires homogeneous record types`** — your
input mixes ST sets (e.g. `UasDatalinkLs` + `SecurityLs`). Split into
per-set lists:

```python
from tstrans.klv import UasDatalinkLs, SecurityLs
uas = [r for r in records if isinstance(r, UasDatalinkLs)]
sec = [r for r in records if isinstance(r, SecurityLs)]
df_uas = tstrans.pandas.klv_to_dataframe(uas)
df_sec = tstrans.pandas.klv_to_dataframe(sec)
```

**KLV DataFrame falls back to `RangeIndex` instead of `DatetimeIndex`** —
none of your records had a populated timestamp. Common with legacy
ST 0102 SecurityLs (no internal timestamp) or partial captures whose
records pre-date the precision-timestamp tag.

**`field_errors` looks empty / non-empty unexpectedly** — lenient KLV
decode (the default) keeps a per-record `field_errors` list of
`KlvFieldError` entries for tags that failed to parse. The DataFrame
collapses these to a `|`-joined string. Empty `field_errors` becomes
the empty string `""`, not `NaN`. If you need a boolean instead, use
`df.field_errors.astype(bool)`.

**`nal_count` is `NaN` on audio rows** — by design.
`audio_frames_to_dataframe` is the audio-frame adapter; `nal_count` is
populated only on video Sample rows. See the column table above.

**`byte_offset` doesn't match the absolute byte position I expected** —
the cumulative offset is a running sum of `frame_length_bytes` starting
at zero, so it represents the offset within the contiguous-frame slice
the adapter saw. For `*_with_resync` flows, gaps caused by skipped
garbage bytes between frames are NOT reflected. Use the resync API
output directly when absolute byte offsets matter.

## Where this binding differs from the Rust core

- **Pipeline-shell naming follows the C-ABI convention, not the Rust
  crate's.** The TS-bytes-through-transport shells are `Sender` /
  `Receiver` (e.g. `tstrans.srt.Sender`) and the raw transports are
  `Transport` / `RecvTransport` (e.g. `tstrans.udp.Transport`) — there is
  no `RawSender` / `RawReceiver`. The composite shells keep the qualified
  `MuxSender` / `DemuxReceiver` names.
- **HLS ships in the wheels.** `tstrans.hls` imports out of the box from a
  published wheel (the `hls` feature is default-on); it raises
  `ImportError` only in a `--no-default-features` source build that omits
  `hls`.
- **RIST is excluded from the Windows wheel.** The Linux and macOS wheels
  bundle librist; the Windows wheel does not. (UDP / TCP / RTP / SRT ship
  on every platform.)
- **RTSP passwords never round-trip to Python.** `BasicAuth` /
  `DigestAuth` hold the password in Rust memory; only `user` / `realm`
  (and `DigestAuth.algorithm`) are readable. `RtspServerConfig.tls_cert` /
  `tls_key` (PEM file paths, set together) light up an `rtsps://` bind.
- **Subtitle Mux API is dataclass-driven.** Rust uses struct-variant
  enums for subtitle codec config; Python wraps each variant as a
  separate dataclass (`DvbSubtitlingConfig`, `DvbTeletextConfig`,
  `Cea708StandaloneConfig`, `WebVttInTsConfig`).
- **`add_subtitle()` and `push_subtitle*()` release the GIL.** Added in
  plan #96 Wave C.
- **`end_detail()` reads the Rust enum field directly, not a
  last-error channel.** The C ABI's `TstStreamEndReason` accessors read
  a recorded `KeepaliveFailed` / `TransportFailed` / `ProtocolError`
  message through the shared thread-local last-error channel
  (`tst_get_last_error_str`), resetting it on every call. Python has no
  such channel — `end_detail()` reads the `msg` field straight off the
  Rust `StreamEndReason` value. A recorded end reason is data, not a
  failure, so it never touches `tstrans.exceptions`.
- **`last_seen_micros(pid)` returns `None`, not `0`, for "never
  seen".** The C ABI has no `Option` in its getter shape, so it uses a
  `0` sentinel; Python's `None` is the honest absent value.
- **No bindings for low-level `mpegts::demux::low_level::*`.** The Rust
  core exposes extension points there; the Python wrap omits them
  (would need PyO3 wrapping of trait objects).
- **Optional `[pandas]` extra is opt-in.** The `tstrans.pandas`
  submodule is only available if `pip install tstrans[pandas]` was
  used. Importing without the extra raises `ImportError` with a clear
  message.

(For pandas + NumPy specifics, see the
[Pandas + NumPy adapters](#pandas--numpy-adapters) sub-section under
"Language-specific gotchas" above.)

## Roadmap

The full surface has shipped — offline file I/O, typed KLV decode /
encode, codec parsers, pandas / NumPy adapters, the UDP / TCP / RTP / RTSP /
SRT / RIST transports, the HLS publisher, and `tstrans.pipeline.Pairer`.
Wheels publish to PyPI on tagged releases. The remaining item is
incremental: a zero-copy Python-buffer-protocol path for the NumPy accessors
(today each access copies once — see
[Snapshot vs zero-copy](#snapshot-vs-zero-copy)).
