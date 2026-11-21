# JVM bindings (`org.tstrans`)
<!-- compile-prelude: java
static byte[] annexBNal, klvBytes, bodyBytes, wireBytes, adtsBytes, tsBytes, miisIdBytes;
static byte[] rbsp;
static org.tstrans.klv.UasDatalinkLs ls;
static org.tstrans.klv.VmtiLs.Builder vmtiLsBuilder;
static org.tstrans.mpegts.Demuxer demuxer;
static long generatedUs, pts;
static int port;
static java.util.Set<String> blocked;
static java.util.List<byte[]> accessUnits, klvUnits;
static byte[] firstKeyFrame;
static org.tstrans.srt.ReconnectPolicy policy;
static org.tstrans.mpegts.MuxerConfig program;
static java.util.Map<Integer, Object> caches;
static java.io.OutputStream recorder;
static org.tstrans.mpegts.DemuxEvent e;
static byte[] st0601Record, annexBAccessUnit;
static org.tstrans.mpegts.MuxerConfig cfg;
-->


> **Who this is for:** You write Java (or any JVM language — Kotlin, Scala,
> Clojure) and want to demux MPEG-TS + KLV streams into typed events — or mux
> them back into a transport stream — on JDK 17+.

> **You will learn:**
> - How to install the binding from Maven Central (or build it from source)
> - How to read a `.ts` file and dispatch typed `DemuxEvent` items
> - How to mux a single-program `.ts` offline with the `Muxer` + config builder (video / KLV / audio / subtitle / private-data streams)
> - How to configure the demuxer with a fluent `DemuxerConfig` builder
> - How to decode / encode typed KLV sets (ST 0601 / 0102 / 0605 / 0903) under `org.tstrans.klv`, plus ST 0806 RVT, ST 1010 SDCC error covariance, and the ST 0805 KLV → CoT conversion layer
> - How to use the file I/O helpers (`Io.parseFile`, `probe`, `extractKlv`, `Muxer.writeFile`)
> - How to send and receive MPEG-TS over RTP and SRT (`org.tstrans.shells` / `org.tstrans.rtp` / `org.tstrans.srt`) — pre-muxed bytes or the `MuxSender`/`DemuxReceiver` shells — and how to drive RTSP (client + server)
> - How to take UDP, TCP and RIST ingest on ports you reserve: `localPort()` on the receiving shells and the TCP listener in `org.tstrans.tcp`
> - How to pair video with KLV metadata by PTS using `org.tstrans.pipeline.Pairer`
> - The JVM-specific gotchas: heap-copied `ByteBuffer` payloads, nullable `Long` DTS
> - How this binding differs from the Rust core

Start with installation and the first examples below. For a specific task,
jump to:

- [Read or write files](#file-io-orgtstransio).
- [Encode or decode KLV](#typed-klv-orgtstransklv).
- [Parse video and audio headers](#codec-parsing-orgtstranscodec).
- [Send or receive over SRT](#srt-convenience-muxsender--demuxreceiver),
  [add reconnect](#srt-managed-reconnect-managed), or
  [use RTP](#rtp-transport-orgtstransrtp).
- [Take UDP, TCP or RIST ingest on your own ports](#udp-tcp-and-rist-ingest-on-your-own-ports).
- [Connect to an RTSP source](#rtsp-client-orgtstransrtp),
  [run an RTSP server](#rtsp-server-orgtstransrtp), or
  [accept RTSP publishers](#rtsp-publisher-ingest-orgtstransrtp).
- [Publish HLS](#hls-publishing-orgtstranshls).
- [Pair video with metadata](#pipeline-pairing-orgtstranspipelinepairer).

The JVM binding exposes SRT, RTP/RTSP, and HLS publishing. UDP, TCP,
and RIST are reached through the `org.tstrans.shells` classes
(`MuxSender.open("udp://…")`, `Receiver.open("rist://@…")`); a `udp://` or
`rist://@…` receiver reports the port it bound with `localPort()`, and
`org.tstrans.tcp.Listener` accepts TCP connections and turns each into a
shell. The raw datagram/stream transports have no JVM class. The JAR ships
RIST. See [binding differences](#where-this-binding-differs-from-the-rust-core).

## Install

The JVM binding is published to Maven Central as
`org.tstrans:tstrans-jvm` (JDK 17+):

```xml
<dependency>
  <groupId>org.tstrans</groupId>
  <artifactId>tstrans-jvm</artifactId>
  <version>0.10.0</version>
</dependency>
```

To build from source instead. Besides the Rust toolchain and a JDK, the
build needs `meson`, `ninja` and `pkg-config` (`apt install meson
ninja-build pkg-config`): the native library links the vendored librist,
which builds with meson.

```bash
# From the workspace, build the binding and run its JUnit5 tests:
cd bindings/jvm
./gradlew test
```

The Gradle build (JDK 17 toolchain, wrapper 9.5.1) drives
`cargo build -p tst-jni` to produce the native library
(`libtstjni.so` / `.dylib` / `.dll`), copies it into JAR resources under
`native/<triple>/`, then compiles and tests the Java surface. A
`NativeLoader` extracts the right native library for the running platform
at runtime. The library is cached in a per-user
`<tmpdir>/tstrans-native-<user>/<sha256>/` directory that the loader creates
owner-only (`rwx------`) and reuses only when it is a real directory owned by
the current user (checked on every file system with an owner view) and, on
POSIX, has exactly that mode. The `<sha256>` directory and a reused library
must be owned by the current user too. On Windows the loader sets no ACL: the
owner check refuses planted entries, but if the temp directory's ACL lets other
accounts write to files there, point `java.io.tmpdir` at a private directory. If the per-user directory exists but
fails those checks, the loader leaves it alone and extracts into a fresh private
temp directory instead, logging a warning. Each JVM stages its own file, reuses
a cached file only when it is a regular file whose size and SHA-256 match, and
never deletes cache entries. Old versions are left for the operating system's
temp cleaner. To skip extraction entirely, start the JVM with
`-Dtstrans.native.lib=<absolute path to libtstjni>`.

**Minimum JDK is 17.** The native code is delivered as a single fat JAR
bundling the per-platform native libraries (linux-x86_64 / linux-aarch64 /
macos-arm64 / windows-x86_64); the consumer picks no classifier.

The jar is also a named module, `org.tstrans`. On the module path, add
`requires org.tstrans;` to your `module-info.java` and run with
`--module-path tstrans-jvm-<version>.jar:<your classes>` (use `;` as the
separator on Windows). Every public package
is exported, and CI runs a small named module on the module path against the jar it
builds, on each platform.

## Hello world

The smallest thing that proves the native library loads and the JNI bridge
works — print the version string:

<!-- compile: java-statements -->
```java
import org.tstrans.Version;

System.out.println(Version.versionString());  // e.g. "0.10.0"
```

## First send

Build a single-program H.264 transport stream offline: configure the muxer,
push one access unit, then drain assembled TS packets with `pull`. The muxer
is deterministic — identical inputs produce byte-identical output across the
Rust, Python, and JVM bindings.

Supply `annexBNal` as a `byte[]` containing one complete Annex-B H.264
access unit from your encoder. Put the executable statements below in a
method that handles or declares the resulting exceptions.

<!-- compile: java-statements -->
```java
import org.tstrans.mpegts.*;

MuxerConfig cfg = MuxerConfig.builder()
    .programNumber(1).pmtPid(0x1000)
    .addVideo(0x1011, VideoCodec.H264)
    .build();

byte[] out = new byte[8192];
try (Muxer m = new Muxer(cfg);
     var sink = java.nio.file.Files.newOutputStream(java.nio.file.Path.of("out.ts"))) {
    // pts is a 90 kHz tick count; keyFrame marks a random-access point.
    m.pushVideo(annexBNal, /*pts=*/ 0L, /*keyFrame=*/ true);
    int n;
    while ((n = m.pull(out)) > 0) {   // drain in a loop until pull returns 0
        sink.write(out, 0, n);        // n is always a multiple of 188
    }
}
```

`Muxer implements AutoCloseable` — the native allocation is reclaimed by
`close()`, so use try-with-resources. The push family mirrors the Rust core:

- `pushVideo(byte[] nal, long pts, boolean keyFrame)` — Annex-B H.264/H.265/H.266 (or AV1 OBU bitstream).
- `pushKlv(byte[] klv, long pts, int metadataServiceId)` — raw KLV LS bytes; for a `SYNCHRONOUS_METADATA` stream the muxer auto-prepends the 5-byte AU-cell header (do **not** pre-wrap).
- `pushAudio(byte[] frames, long pts)` — codec-native audio frames (ADTS for AAC, raw for MP2 / AC-3 / LATM).
- `pushSubtitle(long pts, byte[] payload)` — note the `(pts, payload)` argument order.
- `pushData(byte[] data, long pts)` — raw private/application data bytes,
  passed through verbatim (no AU-cell wrap, unlike KLV); one push = one PES
  packet on `private_stream_1` (0xBD). The PTS is written into the PES only
  when the stream was configured with `carriesPts=true` (a sample pushed to a
  `carriesPts=false` stream re-demuxes with `pts == 0`); either way the `pts`
  argument drives PSI/PCR pacing. Payload ceiling: 65 527 bytes with a PTS,
  65 532 without — larger throws `MuxException(INPUT_MALFORMED)`.
- `pushDataTo(DataStreamHandle h, byte[] data, long pts)` — handle-targeted
  variant for multi-data-stream configs; obtain handles from
  `Muxer.dataHandles()` / `dataStreamHandle(int)`.

The full handle-targeted push family for the typed stream kinds:

- `pushVideoTo(VideoStreamHandle handle, byte[] nal, long pts, boolean keyFrame)` — push a NAL to a specific video stream; required when more than one video stream is configured.
- `pushVideoWireTo(VideoStreamHandle handle, byte[] wire, long pts, boolean keyFrame)` — push an on-wire video AU verbatim (no Annex-B validation; for byte-faithful transmux where `raw()` is fed back directly).
- `pushKlvTo(KlvStreamHandle handle, byte[] klv, long pts, int metadataServiceId)` — push KLV to a specific stream.
- `pushAudioTo(AudioStreamHandle handle, byte[] frames, long pts)` — push audio to a specific stream.
- `pushSubtitleTo(SubtitleStreamHandle handle, long pts, byte[] payload)` — push subtitle to a specific stream (note `(pts, payload)` argument order, matching `pushSubtitle`).

DTS-aware push variants for B-frame-reordered video streams:

- `pushVideoToWithDts(VideoStreamHandle handle, byte[] nal, long pts, long dts, boolean keyFrame)` — Annex-B NAL with explicit DTS; writes `PTS_DTS_flags = '11'` (ISO/IEC 13818-1 §2.4.3.6) in the PES header.
- `pushVideoWireToWithDts(VideoStreamHandle handle, byte[] wire, long pts, long dts, boolean keyFrame)` — on-wire AU with explicit DTS.

`hls.MuxPublisher` and `rtp.MountHandle` take no stream handle for a DTS push: each has a single-stream `pushVideoWithDts(byte[] nal, long pts, long dts, boolean keyFrame)` for a config with exactly one video stream (several streams fail with `MuxException` / `RtspException(MOUNT)`). `dts <= pts` is not enforced.

Per-stream handle accessors — obtain handles at mux time and use them with the
`*To` variants above:

- `List<VideoStreamHandle> videoHandles()` — all configured video stream handles (across all programs).
- `Optional<VideoStreamHandle> videoStreamHandle(int index)` — get the video handle by zero-based position in the config.
- `List<AudioStreamHandle> audioHandles()` / `Optional<AudioStreamHandle> audioStreamHandle(int index)` — same shape for audio.
- `List<KlvStreamHandle> klvHandles()` / `Optional<KlvStreamHandle> klvStreamHandle(int index)` — same shape for KLV.
- `List<SubtitleStreamHandle> subtitleHandles()` / `Optional<SubtitleStreamHandle> subtitleStreamHandle(int index)` — same shape for subtitle.
- `List<DataStreamHandle> dataHandles()` / `Optional<DataStreamHandle> dataStreamHandle(int index)` — same shape for data streams.

Each `push*` targets the lone stream of that kind; a muxer configured with
zero or more than one stream of the kind throws `MuxException(INVALID_USAGE)`.
Build the `MuxerConfig` with `addVideo` / `addKlv` / `addAudio` / `addSubtitle`
/ `addData(pid, streamType, carriesPts)` on `MuxerConfig.builder()`; the
builder is single-program. `programDescriptors(byte[][])` sets the program
info loop, and `streamDescriptorsForVideo` / `ForKlv` / `ForAudio` /
`ForSubtitle` / `ForData(int kindIndex, byte[][])` set one stream's PMT
descriptor TLVs, the index counting that kind's `add*` calls in order;
`streamDescriptorsForStream(int streamIndex, byte[][])` counts every `add*`
call. A later call naming the same stream replaces an earlier one. The muxer
never auto-emits a descriptor on a data stream. Deep config validation (PID collisions,
PMT-size budget, sync-KLV-without-PTS, a data `streamType` that would classify
as a typed kind, …) runs in the native `Muxer` constructor and surfaces as
`MuxException(CONFIG_INVALID)`. The muxer's PCR runs 100 ms behind each access
unit's decode time; `MuxerConfig.Builder.pcrLeadMs(ms)` sets another lead
(0 to 1000).

`Muxer.reconfigure(MuxerConfig)` replaces the running program in place: streams
match by PID, and one whose stream type and codec are unchanged keeps its handle,
its continuity counter and the program's PCR and PSI cadence; a removed stream's
handle is refused with `INVALID_USAGE` until a later `reconfigure` reuses its
slot (a new stream takes the slot that last carried its PID, else any freed
one), so re-read the handles from the `*Handles()` getters after each call. A config that moves a PMT PID, changes a
PID's stream type or codec, moves a PID to another program number, or changes a
muxer-wide setting (`pcrIntervalMs`, `pcrLeadMs`, `psiIntervalMs`,
`bufferPackets`, `av1Carriage`) throws `MuxException(CONFIG_INVALID)` and
changes nothing. The single-program config means the program list itself cannot
grow here; the Rust and Python muxers take several programs.

The muxing shells take the same call: `MuxSender.reconfigure(MuxerConfig)` and
`ManagedMuxSender.reconfigure(MuxerConfig)` (also on the `MuxSender` that
`RtspPushSession.intoMuxSender` returns) apply these rules to the running
sender, and the next send puts the new PAT and PMT on the wire. Their
`videoHandle()` … `dataHandle()` getters then return the handles of the new
configuration, and never wait for a send or a reconfigure on another thread.
A reconfigure holds the sender as a send does: it first sends what an earlier
refused send left, so it can wait on a slow transport, and `close()` from
another thread ends that wait as it ends a send's (`CLOSED`).

<!-- compile: java-statements -->
```java
import org.tstrans.mpegts.*;

MuxerConfig base = MuxerConfig.builder().programNumber(1).pmtPid(0x100)
    .addVideo(0x101, VideoCodec.H264).build();
try (Muxer m = new Muxer(base)) {
    VideoStreamHandle video = m.videoHandles().get(0);
    // Add a KLV stream; the video keeps its handle and its counters.
    m.reconfigure(MuxerConfig.builder().programNumber(1).pmtPid(0x100)
        .addVideo(0x101, VideoCodec.H264)
        .addKlv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, true).build());
    KlvStreamHandle klv = m.klvHandles().get(0);
}
```

> **Scope.** This binding's `MuxerConfig` is single-program; multi-program
> configs are deferred. DVB-subtitle codec configuration is deferred;
> `addSubtitle` accepts the no-config codecs (`CEA708_STANDALONE` /
> `WEBVTT_IN_TS`).

> Every `SYNCHRONOUS_METADATA` KLV stream declares `metadata_service_id` 0
> unless its descriptors carry a `metadata_descriptor` (tag 0x26), and two
> streams declaring the same id in one transport stream fail with
> `CONFIG_INVALID`. Give the second stream its own id with
> `streamDescriptorsForKlv(1, new byte[][] {{0x26, 0x09, 0x01, 0x00, (byte) 0xFF,
> 'K', 'L', 'V', 'A', 1, 0x0F}})` (service id 1). The per-push
> `metadataServiceId` argument does not change the PMT descriptors.

## First receive

Demux a `.ts` file and dispatch on typed events. The JVM binding's
baseline is **JDK 17**, where `instanceof` pattern matching is the portable
idiom for a sealed hierarchy:

<!-- compile: java-statements -->
```java
import org.tstrans.mpegts.*;

byte[] ts = java.nio.file.Files.readAllBytes(java.nio.file.Path.of("capture.ts"));
// Feed bounded chunks and drain after each: one feed() larger than the
// 4 MiB ingress ceiling throws DemuxException (kind SYNC_BUF_EXHAUSTED).
// The last pass flushes.
final int chunk = 188 * 1024;
try (Demuxer d = new Demuxer()) {
    for (int off = 0; ; off += chunk) {
        boolean end = off >= ts.length;
        if (end) {
            d.flush();
        } else {
            d.feed(java.util.Arrays.copyOfRange(ts, off, Math.min(ts.length, off + chunk)));
        }
        for (DemuxEvent e : d) {
            if (e instanceof DemuxEvent.ProgramMap pm) {
                System.out.println("PSI: program " + pm.programNumber() + ", " + pm.elementaryPids().size() + " streams");
            } else if (e instanceof DemuxEvent.Video v) {
                System.out.println("Video pid=" + v.stream().pid() + " pts=" + v.pts() + " len=" + v.raw().remaining());
            } else if (e instanceof DemuxEvent.Metadata m) {
                System.out.println("KLV pid=" + m.stream().pid() + " kind=" + m.kind() + " len=" + m.payload().remaining());
            } else if (e instanceof DemuxEvent.NonConformant nc) {
                System.out.println("non-conformant: " + nc.kind() + " — " + nc.issue());
            }
            // Audio / Subtitle / UnknownSample / Discontinuity / ReconnectDiscontinuity handled similarly.
        }
        if (end) break;
    }
}
```

`Demuxer` `implements AutoCloseable, Iterable<DemuxEvent>`. The shape is:
`feed(byte[])` enqueues parsed events, `flush()` drains any buffered
partial PES, and iterating (or calling `nextEvent()`) pulls the
currently-queued events. `nextEvent()` returns `null` when the queue is
empty; the `for`-each loop stops at the same point. Call `feed` / `flush`
again to enqueue more, then iterate again.

On **JDK 21+** you can `switch` on the sealed `DemuxEvent` hierarchy with
pattern matching, but this binding targets JDK 17 where `instanceof`
patterns are the portable form — the examples here stay on 17.

### Configured demuxer

Pass a `DemuxerConfig` built with the fluent builder to tighten parsing
behavior — for example, enable full strict mode and turn off CFI
tolerance:

<!-- compile: java-statements -->
```java
import org.tstrans.mpegts.*;

DemuxerConfig cfg = DemuxerConfig.builder()
    .strictMode(StrictMode.FULL)
    .cfiTolerance(false)
    .pesCapPerPid(4_000_000)
    .build();

try (Demuxer d = new Demuxer(cfg)) {
    // feed in chunks, flush and iterate as in the loop above
}
```

The 9 config knobs:

| Knob | Type | Default | Effect |
|---|---|---|---|
| `strictMode` | `StrictMode` | `OFF` | Strictness ladder: `OFF` / `TIMING_ONLY` / `PSI_ONLY` / `FULL`. |
| `cfiTolerance` | `boolean` | `true` | Tolerate cell-fragment-indication producer bugs. |
| `pesCapPerPid` | `long` | `0` (Rust default) | Per-PID PES reassembly byte cap. |
| `pesCapTotal` | `long` | `0` (Rust default) | Total PES reassembly byte cap. |
| `auCellCapPerPid` | `long` | `0` (Rust default) | Per-PID AU-cell reassembly byte cap. |
| `av1Carriage` | `Av1CarriageMode` | `MPEG2_TS_BINDING` | AV1 carriage: `MPEG2_TS_BINDING` or `INTEROP_RAW_OBU`. |
| `lenientPsiReassembly` | `boolean` | `false` | Relax PSI section reassembly. |
| `syncBufCap` | `long` | `0` (Rust default, 4 MiB) | Pre-sync ingress buffer ceiling in bytes; a `feed()` call larger than this throws `DemuxException` (kind `SYNC_BUF_EXHAUSTED`) before consuming any bytes. |
| `unwrapTimestamps` | `boolean` | `false` | Unwrap the raw 33-bit 90 kHz PTS/DTS onto a continuous per-program timeline that carries across the ~26.5 h rollover, instead of emitting the raw wire value. |

A `long` knob of `0` means "use the Rust core's default cap."

**Continuous timestamps across the rollover.** With `unwrapTimestamps(true)`,
each sample's signed wrap-aware delta accumulates onto the previous
unwrapped value for its PID, so a genuine out-of-order arrival steps back
by its true distance instead of being mistaken for another wrap. PIDs of
the same program share one running clock and stay directly comparable
across the rollover — this is what lets a consumer pair KLV to video by
PTS on a long-running stream — while independent programs are never
cross-anchored. The accumulators reset alongside the rest of the per-PID
parse state on a sync reset, so a `ManagedDemuxReceiver` reconnect (see
below) restarts the unwrap timeline from the next `ProgramMap`.

### The `DemuxEvent` hierarchy

`DemuxEvent` is a JDK-17 `sealed interface` whose variants are `record`s:

- `ProgramMap(int programNumber, int pcrPid, int pmtPid, List<Integer> elementaryPids)` — PSI / PMT.
- `Video(StreamId stream, long pts, Long dts, VideoCodec codec, ByteBuffer raw, boolean randomAccessIndicator, Av1CarriageMode av1Carriage)` — the raw encoded access unit; call `parse()` to obtain typed `List<VideoUnit>` on demand (see [Typed sample payloads](#typed-sample-payloads)).
- `Audio(StreamId stream, long pts, Long dts, AudioCodec codec, ByteBuffer raw)` — the raw encoded audio ES; call `parse()` to obtain typed `List<AudioFrame>` on demand (see [Typed sample payloads](#typed-sample-payloads)).
- `Subtitle(StreamId stream, long pts, Long dts, SubtitleCodec codec, ByteBuffer payload)`
- `UnknownSample(StreamId stream, long pts, Long dts, int streamType, ByteBuffer payload)`
- `Metadata(StreamId stream, long pts, MetadataKind kind, ByteBuffer payload, boolean wasReassembled, int cellCount)` — KLV.
- `NonConformant(StreamId stream, String issue, NonConformantKind kind, MultiCellAuReason multiCellAuReason, CellFragmentIndication observedCfi, CellFragmentIndication treatedAs)`
- `Discontinuity(StreamId stream, DiscontinuityKind kind)`
- `ReconnectDiscontinuity()`

`dts` is a nullable boxed `Long` (null when the PES carried no DTS). On
`NonConformant`, the trailing three fields are `null` except for the
relevant kind: `multiCellAuReason` is non-null only when
`kind == MULTI_CELL_AU`, and `observedCfi` / `treatedAs` are non-null only
when `kind == CFI_TOLERATED`.

`StreamId(int pid, StreamKind kind, int programNumber)` carries the source
PID, the typed stream kind, and the owning program number. `StreamKind` is
itself a sealed interface: `Video(VideoCodec codec)`,
`Audio(AudioCodec codec)`, `Subtitle(SubtitleCodec codec)`,
`KlvSync(Integer declaredLink)`, `KlvAsync()`, and
`Unknown(int streamTypeByte)`.

**Enums:**

- `VideoCodec` — `H264`, `H265`, `H266`, `AV1`
- `AudioCodec` — `MP2`, `AAC`, `AAC_LATM`, `AC3`
- `SubtitleCodec` — `DVB_SUBTITLING`, `DVB_TELETEXT`, `CEA708_STANDALONE`, `WEBVTT_IN_TS`
- `MetadataKind` — `KLV_SYNC_AU_CELL`, `KLV_ASYNC`, `UNKNOWN`
- `DiscontinuityKind` — `CONTINUITY_JUMP`, `PES_OVERSIZE`, `PES_TOTAL_OVERSIZE`, `ADAPTATION_FIELD_FLAG`
- `CellFragmentIndication` — `MIDDLE`, `LAST`, `FIRST`, `COMPLETE`
- `MultiCellAuReason` — `ORPHAN`, `SEQUENCE_GAP`, `CONCURRENT_FIRST`, `OVERFLOW`, `OVERFLOW_TOTAL`, `TOO_MANY_PIDS`
- `StrictMode` — `OFF`, `TIMING_ONLY`, `PSI_ONLY`, `FULL`
- `Av1CarriageMode` — `MPEG2_TS_BINDING`, `INTEROP_RAW_OBU`
- `NonConformantKind` — a collapsed discriminant; the `issue` String carries the detail (see the gotcha below).

## Typed KLV (`org.tstrans.klv`)

The `org.tstrans.klv` package exposes fully typed decode and encode for the
four MISB KLV set families that the demuxer surfaces on `DemuxEvent.Metadata`
payloads. All types are immutable Java `record`s; all decode / encode goes
through the static `Klv` façade.

### Decode an ST 0601 UAS Datalink LS

Pass the raw `ByteBuffer` payload bytes from a `DemuxEvent.Metadata` event
directly to `Klv.decodeUasDatalink`. The buffer includes the 16-byte SMPTE
Universal Label — the decoder reads it as part of its verification.

<!-- compile: java-statements -->
```java
import org.tstrans.klv.*;
import org.tstrans.mpegts.DemuxEvent;

// Inside a demux loop where `e` is a DemuxEvent.Metadata:
if (e instanceof DemuxEvent.Metadata m) {
    // Copy the heap ByteBuffer to a byte[] for Klv.decodeUasDatalink.
    java.nio.ByteBuffer view = m.payload().duplicate();
    byte[] klvBytes = new byte[view.remaining()];
    view.get(klvBytes);

    if (Klv.isSt0601Family(klvBytes)) {
        UasDatalinkLs ls = Klv.decodeUasDatalink(klvBytes);  // throws KlvDecodeException

        // Composite accessor: sensor GPS position (lat/lon/alt).
        ls.sensorPosition().ifPresent(pos ->
            System.out.printf("sensor: %.6f, %.6f, %.1fm%n",
                pos.latDeg(), pos.lonDeg(), pos.altM()));

        // Composite accessor: frame-center coordinates (falls back to
        // offset calculations when absolute coordinates are absent).
        ls.frameCenter().ifPresent(fc ->
            System.out.printf("frame center: %.6f, %.6f%n",
                fc.latDeg(), fc.lonDeg()));

        // Non-fatal field errors: tags that decoded partially.
        for (KlvFieldError fe : ls.fieldErrors()) {
            System.out.println("field error tag=" + fe.tag() + " " + fe.kind());
        }
    }
}
```

`decodeUasDatalink` is **lenient by default**: it accepts any 16-byte UL,
verifies the Tag-1 checksum, and collects per-field parse failures in
`fieldErrors()` rather than throwing. Pass `strict=true` / `compliance=true`
to the three-argument overload for stricter behaviour:

```java
// Strict: requires the ST 0601 family UL pattern.
UasDatalinkLs ls = Klv.decodeUasDatalink(klvBytes, /*strict=*/ true, /*compliance=*/ false);

// Compliance: also enforces Tag-2 first / Tag-1 last / Tag-65 present.
UasDatalinkLs ls = Klv.decodeUasDatalink(klvBytes, /*strict=*/ true, /*compliance=*/ true);
```

### Encode an ST 0601 UAS Datalink LS

Build an `UasDatalinkLs` with its `Builder`, push only the fields you want,
then call `Klv.encodeUasDatalink`. Encoding is lenient (no mandatory-tag
enforcement); use `encodeUasDatalinkStrictCompliance` to enforce the full
compliance rules.

<!-- compile: java-statements -->
```java
import org.tstrans.klv.*;

// Build a minimal record: timestamp + version (required by strict compliance).
UasDatalinkLs ls = new UasDatalinkLs.Builder()
    .timestampUs(1_700_000_000_000_000L)  // microseconds (Tag 2)
    .declaredVersion(17)                   // MISB ST 0601.17 (Tag 65)
    .build();

// Lenient encode — emits only populated fields.
byte[] wire = Klv.encodeUasDatalink(ls);  // throws KlvEncodeException

// Strict-compliance encode — enforces mandatory tags (Tags 1/2/65).
byte[] strictWire = Klv.encodeUasDatalinkStrictCompliance(ls);
```

The encode round-trip is byte-identical across the Rust, Python, and JVM
bindings for the same input record.

### Universal-label dispatcher (`parseUniversal`)

`Klv.parseUniversal(byte[])` inspects the first 16 bytes (the SMPTE UL) and
routes to the correct typed decoder. It returns `Optional<KlvSet>` — empty
for an unrecognised UL, or a concrete `KlvSet` implementer for a known one.
Use `instanceof` on JDK 17 to dispatch:

<!-- compile: java-statements -->
```java
import org.tstrans.klv.*;
import java.util.Optional;

Optional<KlvSet> result = Klv.parseUniversal(klvBytes);  // throws KlvDecodeException
if (result.isPresent()) {
    KlvSet set = result.get();
    if (set instanceof UasDatalinkLs ls) {
        System.out.println("ST 0601: sensorPos=" + ls.sensorPosition());
    } else if (set instanceof SecurityLs sec) {
        System.out.println("ST 0102: class=" + sec.securityClassification());
    } else if (set instanceof PrecisionTimeStampPack ptp) {
        System.out.println("ST 0605: ts=" + ptp.timestampUs() + " µs");
    } else if (set instanceof VmtiLs vmti) {
        System.out.println("ST 0903: " + vmti.targets().size() + " targets");
    }
} else {
    System.out.println("unrecognised UL");
}
```

For body-only sets (ST 0102 / ST 0903), `parseUniversal` peels the 16-byte UL
and the outer BER length before calling the per-set decoder. For the others
(ST 0601 / ST 0605), the full buffer is passed through.

### Other typed-set families

**ST 0102 — Security Metadata LS** (body-only — no UL / outer BER wrapper):

<!-- compile: java-statements -->
```java
// Decode body bytes (no UL / outer BER).
SecurityLs secLenient = Klv.decodeSecurity(bodyBytes);        // lenient
SecurityLs secStrict = Klv.decodeSecurity(bodyBytes, true);   // strict (rejects missing required tags)

// Encode back to body bytes.
byte[] body = Klv.encodeSecurity(secLenient);  // throws KlvEncodeException

// Enum accessors: typed + raw codepoint preserved for unknown values.
secLenient.securityClassification();         // Optional<SecurityClassification>
secLenient.securityClassificationCode();     // Integer (raw code, or null if tag absent)
```

**ST 0605 — Precision Time Stamp Pack** (full 26-byte framing):

<!-- compile: java-statements -->
```java
PrecisionTimeStampPack pack = Klv.decodePrecisionTimestamp(wireBytes);  // throws KlvDecodeException
System.out.println(pack.timestampUs() + " µs, locked=" + pack.timeStatus().isLocked());

byte[] wire = Klv.encodePrecisionTimestamp(pack);  // infallible; always 26 bytes
```

**ST 0903 — VMTI LS** (body-only for decode; two encode forms):

<!-- compile: java-statements -->
```java
VmtiLs vmtiLenient = Klv.decodeVmti(bodyBytes);        // lenient
VmtiLs vmtiStrict = Klv.decodeVmti(bodyBytes, true);   // strict

System.out.println(vmtiLenient.targets().size() + " targets");

byte[] body = Klv.encodeVmti(vmtiLenient);               // body only (no UL / BER / checksum)
byte[] framed = Klv.encodeVmtiStandalone(vmtiLenient);   // full [UL][BER][body][Tag1 checksum]
```

**ST 0806 — RVT (Remote Video Terminal) Local Set** (body bytes carried in
`UasDatalinkLs.rvt()`, ST 0601 Tag 73):

<!-- compile: java-statements -->
```java
ByteBuffer rvtBytes = ls.rvt();
if (rvtBytes != null) {
    ByteBuffer view = rvtBytes.duplicate();
    byte[] buf = new byte[view.remaining()];
    view.get(buf);
    RvtLs rvt = Klv.decodeRvt(buf);  // throws org.tstrans.KlvDecodeException
    System.out.println("airspeed: " + rvt.platformTrueAirspeed() + " m/s");
    for (RvtPoi poi : rvt.pointsOfInterest()) {
        System.out.println("  POI #" + poi.number() + ": " + poi.text());
    }
}
```

RVT is also standalone-capable — `Klv.decodeRvtStandalone` parses the
16-byte UL + BER length + body and verifies the Tag 1 CRC-32/MPEG-2
checksum when present; the embedded (Tag 73) form is not required to
carry it.

**ST 0805 — KLV → Cursor-on-Target (CoT) conversion** (one-way; not a KLV
wire format):

<!-- compile: java-statements -->
```java
String platformXml = Klv.platformPositionXml(ls, generatedUs);         // CotConfig.defaults()
String spiXml = Klv.sensorPointOfInterestXml(ls, generatedUs);

CotConfig cfg = CotConfig.builder().platformType("a-f-A-M-H").build();
String customXml = Klv.platformPositionXml(ls, cfg, generatedUs);      // explicit CotConfig
```

`generatedUs` (POSIX epoch microseconds) is a required argument, not
sampled internally — a replayed-file CoT run must be byte-identical to a
live one (ST 0805.1 §1). Both conversions throw unchecked
`IllegalArgumentException` naming the missing KLV tag when a
mapping-required field is absent from `ls`. `CotConfig.producer` is an
XML attribute *name* stamped verbatim into `<detail><_flow-tags_ .../>` —
a Name production (an attribute name, not a value): neither validated
nor escaped, so an invalid value produces malformed XML.

**ST 1010 — SDCC-FLP error covariance** (general-purpose; carried inside
ST 0601 Tag 102, but not ST 0601-specific):

<!-- compile: java-statements -->
```java
for (SdccFlpField f : ls.sdccFlps()) {
    ByteBuffer view = f.bytes().duplicate();
    byte[] buf = new byte[view.remaining()];
    view.get(buf);
    SdccFlp m = Klv.decodeSdccFlp(buf);  // throws org.tstrans.KlvDecodeException
    for (int i = 0; i < m.matrixSize(); i++) {
        System.out.println("sigma[" + i + "] = " + m.correlation(i, i));
    }
}
```

### Field-error model

Lenient decode is non-throwing for per-field problems. Errors that the Rust
core can recover from (malformed tag value, unsupported IMAPB length, invalid
codepoint, …) are collected in the set's `fieldErrors()` list as
`KlvFieldError(KlvFieldErrorKind, long tag, String message)`. Tags that fail
are skipped; all other tags decode normally.

<!-- compile: java-statements -->
```java
UasDatalinkLs ls = Klv.decodeUasDatalink(klvBytes);
for (KlvFieldError fe : ls.fieldErrors()) {
    // KlvFieldErrorKind: OUT_OF_RANGE, INVALID_UTF8, INVALID_LENGTH, ...
    System.out.printf("  tag %d: %s — %s%n", fe.tag(), fe.kind(), fe.message());
}
```

`fieldErrors()` returns an empty list when decoding succeeds without any
per-field problem. A non-empty list is advisory — the set is still usable;
the affected tags are missing from the typed fields.

### KLV byte fields are heap `ByteBuffer` copies

Fields typed `ByteBuffer` in the KLV records (for example `UasDatalinkLs`'s
`vmti()` or `securityLocalSet()`, `VTargetPack`'s `vmask()`, etc.) are
heap-`ByteBuffer.wrap(byte[])` copies — the same JDK-17 safety rule that
governs `DemuxEvent` payloads. Direct buffers over Rust memory are not
offered on this baseline; safe zero-copy is deferred to a JDK-22+ Foreign
Function & Memory API path. When hand-constructing a `ByteBuffer` to pass to
a builder setter, always use `ByteBuffer.wrap(byte[])`:

<!-- compile: java-statements -->
```java
// Correct: heap copy, safe on JDK 17.
vmtiLsBuilder.miisId(java.nio.ByteBuffer.wrap(miisIdBytes));

// Wrong: direct buffer would be rejected (and unsafe on JDK < 22).
// ByteBuffer.allocateDirect(16).put(miisIdBytes)  — do NOT do this
```

## Codec parsing (`org.tstrans.codec`)

The `org.tstrans.codec` package wraps `tst_core::codec::*` — the elementary-stream
parsers for H.264 / H.265 / H.266 / AV1 / AAC / MPEG-2 audio. The static
`Codec` facade is the entry point; every parser takes raw RBSP / payload bytes
and returns an immutable record (or a `List` of them), throwing
`CodecParseException` on malformed input.

<!-- compile: java-statements -->
```java
import org.tstrans.codec.Codec;
import org.tstrans.codec.H264Sps;
import org.tstrans.codec.AdtsFrame;
import org.tstrans.CodecParseException;
import java.util.List;

// Parse an H.264 sequence parameter set from its RBSP body
// (Annex-B start code already stripped).
H264Sps sps = Codec.parseH264Sps(rbsp);
System.out.printf("%dx%d profile_idc=%d%n",
    sps.codedWidth(), sps.codedHeight(), sps.profileIdc());

// Parse a run of ADTS AAC frames out of an audio elementary stream.
List<AdtsFrame> frames = Codec.parseAacFrames(adtsBytes);
for (AdtsFrame f : frames) {
    System.out.printf("AAC %d Hz, channel_config=%d%n",
        f.sampleRateHz(), f.channelConfiguration());
}
```

`CodecParseException` is a checked exception carrying a `kind()` discriminant
(e.g. `TRUNCATED_RBSP`, `INVALID_GOLOMB`, `UNSUPPORTED_PROFILE`, `BAD_SYNC_WORD`)
and a human-readable message. The
H.265 / H.266 parsers mirror the H.264 method names
(`parseH265Sps`, `parseH266Vps`, …); AV1 has `parseAv1SequenceHeader` /
`parseAv1FrameHeaderLight` / `parseAv1ObuStream`; MPEG-2 audio has
`parseMpeg2AudioFrames`. The `*WithResync` audio variants tolerate leading
garbage by scanning for the next sync word instead of throwing.

## Typed sample payloads

The demuxer hands back **typed** elementary units on
demand. `DemuxEvent.Video.parse()` returns a `List<VideoUnit>` — `NalUnit`s for
H.264 / H.265 / H.266, `Obu`s for AV1 — by calling the native `split_video`
only when the caller opts in. `DemuxEvent.Audio.parse()` likewise returns a
`List<AudioFrame>` — `AdtsFrame`s for AAC, `Mpeg2AudioFrame`s for MPEG-2 audio —
parsing the raw ES bytes only when the caller opts in. The `codec()` accessor on
each event tags the discriminant (`VideoCodec` / `AudioCodec`). Downcast with
`instanceof`:

<!-- compile: java-statements -->
```java
// parse() throws the checked DemuxException — declare it on the
// enclosing method (as nextEvent()-driven code usually already does).
for (DemuxEvent e : demuxer) {
    if (e instanceof DemuxEvent.Video v && v.codec() == VideoCodec.H264) {
        for (VideoUnit u : v.parse()) {       // opt-in: calls split_video
            NalUnit nal = (NalUnit) u;        // H.264 -> always NalUnit
            int nalType = nal.nalType();      // 5 == IDR slice
            ByteBuffer rbsp = nal.payload();  // RBSP body, start code stripped
        }
    }
}
```

**Video raw bytes (always populated).** `Video.raw()` carries the exact
encoded access unit — Annex-B byte stream for H.264/H.265/H.266, on-wire PES
payload for AV1 — as a heap `ByteBuffer`. Call `parse()` when you need the
typed unit list; it mirrors tst-py's `.parse()`. Feed `raw()` back to
`Muxer.pushVideo` for byte-faithful transmux; it mirrors tst-py's `.raw`.

**Audio raw bytes (always populated).** `Audio.raw()` carries the exact encoded
audio elementary-stream bytes as a heap `ByteBuffer`. Call `parse()` (or
`parse(strict)`) when you need the typed frame list; it mirrors tst-py's
`.parse()`. `parse()` is **lenient** — it skips past corruption to the next
valid frame and never throws `CodecParseException`; `parse(true)` is **strict**
— it throws `CodecParseException` on the first malformed frame. Both methods
can also throw `DemuxException` on an internal binding failure (e.g. enum
drift). Both are opt-in, so the demuxer never pays the parse cost for audio
you don't inspect.

**Codecs with no typed parser (AAC-LATM, AC-3).** These codecs are carried
(AC-3 is additionally sync-validated by the demuxer), but per-frame typed
parsing isn't implemented yet, so `parse()` returns an **empty list** in both
modes — read `raw()` directly for the encoded bytes.

<!-- compile: java-statements -->
```java
// parse() throws checked CodecParseException (strict mode) and DemuxException
// (internal failure) — declare both on the enclosing method (or catch them).
// Use a plain for-loop, not a forEach lambda, since a lambda cannot propagate
// checked exceptions.
for (DemuxEvent e : demuxer) {
    if (e instanceof DemuxEvent.Audio a && a.codec() == AudioCodec.AAC) {
        for (AudioFrame f : a.parse()) {          // opt-in: parses the raw ES
            AdtsFrame adts = (AdtsFrame) f;       // AAC -> always AdtsFrame
            long sampleRate = adts.sampleRateHz();
        }
    }
}
```

## File I/O (`org.tstrans.io`)

The `org.tstrans.io` package wraps the file read-path in three convenience
helpers — `parseFile`, `probe`, and `extractKlv` — and the write-path lives on
`Muxer.writeFile`. All helpers are pure-Java orchestration over `Demuxer`; there
is no native code in the package.

### Parse a file

`Io.parseFile` opens a `.ts` file, feeds it to a `Demuxer` in 64 KiB chunks,
and yields `DemuxEvent` items lazily as a `Stream`. The stream is
`AutoCloseable` — always wrap it in try-with-resources so the backing demuxer
and file handle are released:

<!-- compile: java-statements -->
```java
import org.tstrans.io.Io;
import org.tstrans.mpegts.*;
import java.nio.file.Path;

Path path = Path.of("capture.ts");
try (var events = Io.parseFile(path)) {
    events.forEach(e -> {
        if (e instanceof DemuxEvent.ProgramMap pm) {
            System.out.println("PMT program=" + pm.programNumber()
                + " streams=" + pm.elementaryPids().size());
        } else if (e instanceof DemuxEvent.Video v) {
            // raw() is exception-free; parse() throws the checked
            // DemuxException, which a forEach lambda cannot propagate —
            // use a plain for-loop when you need the typed units here.
            System.out.println("Video pts=" + v.pts()
                + " rawBytes=" + v.raw().remaining());
        } else if (e instanceof DemuxEvent.Metadata m) {
            System.out.println("KLV kind=" + m.kind()
                + " len=" + m.payload().remaining());
        } else if (e instanceof DemuxEvent.Audio a) {
            System.out.println("Audio codec=" + a.codec()
                + " pts=" + a.pts());
        }
        // NonConformant / Discontinuity / ReconnectDiscontinuity handled similarly.
    });
}
```

On **JDK 21+** you can replace the `instanceof` chain with a `switch` on the
sealed `DemuxEvent` hierarchy — the examples here stay on JDK 17 (`instanceof`
patterns) as that is the baseline. Pass a `DemuxerConfig` to the two-argument
overload to tighten parsing (see [Configured demuxer](#configured-demuxer)):

```java
DemuxerConfig cfg = DemuxerConfig.builder().strictMode(StrictMode.FULL).build();
try (var events = Io.parseFile(path, cfg)) { ... }
```

**Error contract.** A demux error mid-stream surfaces as a `RuntimeException`
wrapping `DemuxException` (Java streams cannot propagate checked exceptions
during iteration). An I/O read error surfaces as `UncheckedIOException`.
Truncation is a clean end — the stream terminates normally with no error.

> **Truncation note.** The file path never reports truncation as an error:
> it is treated as a clean EOF, and read failures surface as
> `UncheckedIOException`. (The parity-only `DemuxException.Kind.UNEXPECTED_EOF`
> was retired in 0.7.0 — it had no producer in any binding.)

### Probe a file

`Io.probe` scans the first 5 MiB and returns a `ProbeResult` record
summarising what the file contains — without reading the entire file:

<!-- compile: java-statements -->
```java
import org.tstrans.io.Io;
import org.tstrans.io.ProbeResult;
import java.nio.file.Path;

ProbeResult r = Io.probe(Path.of("capture.ts"));

System.out.println("size:         " + r.sizeBytes() + " bytes");
System.out.println("packets:      " + r.packetCount());
System.out.println("programs:     " + r.programs().size());
System.out.println("video codecs: " + r.videoCodecs());
System.out.println("audio codecs: " + r.audioCodecs());
System.out.println("has KLV:      " + r.hasKlv());
System.out.println("pids:         " + r.pids());
```

`ProbeResult` fields:

| Field | Type | Notes |
|---|---|---|
| `sizeBytes()` | `long` | Full file size in bytes (not capped at the probe window). |
| `packetCount()` | `long` | Number of 188-byte TS packets read in the probe window. |
| `programs()` | `List<DemuxEvent.ProgramMap>` | One entry per PMT seen in the probe window. |
| `pids()` | `List<Integer>` | Elementary-stream PIDs seen across all programs. |
| `videoCodecs()` | `List<VideoCodec>` | Distinct video codecs observed (sorted by name). |
| `audioCodecs()` | `List<AudioCodec>` | Distinct audio codecs observed (sorted by name). |
| `subtitleCodecs()` | `List<SubtitleCodec>` | Distinct subtitle codecs observed (sorted by name). |
| `hasKlv()` | `boolean` | Whether any `Metadata` event was seen. |

**Classification source.** Codec and KLV presence are derived from the
`Video` / `Audio` / `Subtitle` / `Metadata` events observed during the scan, not
from the PMT. This is a small, documented divergence from tst-py, where
`ProgramMap.streams` carries per-stream type info: the JVM `DemuxEvent.ProgramMap`
exposes only `elementaryPids`, so per-stream classification is event-derived.
For any file that has samples within the probe window the results are equivalent.

### Extract KLV

`Io.extractKlv` streams only the KLV payloads from a file, optionally
attaching 90 kHz timestamps and dispatching through the typed-KLV decoder:

<!-- compile: java-statements -->
```java
import org.tstrans.io.Io;
import org.tstrans.io.ExtractKlvOptions;
import org.tstrans.io.KlvEntry;
import org.tstrans.klv.*;
import java.nio.file.Path;

// Parsed + with PTS: each entry carries a typed KlvSet and its 90 kHz timestamp.
ExtractKlvOptions opts = ExtractKlvOptions.builder()
    .withPts(true)          // include the 90 kHz PTS in each KlvEntry
    .parsed(true)           // run Klv.parseUniversal on each payload
    .skipUnknown(true)      // drop payloads whose UL is unrecognised (default)
    .skipMalformed(false)   // propagate KlvDecodeException on a recognised bad payload (default)
    .build();

try (var entries = Io.extractKlv(Path.of("capture.ts"), opts)) {
    entries.forEach(entry -> {
        Long pts = entry.pts();                // null when withPts=false
        KlvSet typed = entry.parsed();         // null when parsed=false
        // byte[] entry.raw() is null when parsed=true; non-null when parsed=false

        if (typed instanceof UasDatalinkLs ls) {
            ls.sensorPosition().ifPresent(pos ->
                System.out.printf("pts=%d sensor=%.6f,%.6f%n",
                    pts, pos.latDeg(), pos.lonDeg()));
        }
    });
}
```

**`KlvEntry` shape.** All three fields are nullable depending on the options:

| Field | Non-null when |
|---|---|
| `pts()` (`Long`) | `withPts(true)` |
| `raw()` (`byte[]`) | `parsed(false)` |
| `parsed()` (`KlvSet`) | `parsed(true)` AND the UL was recognised |

**Error contract for `extractKlv`.** With `parsed=true` and a recognised UL
that fails to decode, `skipMalformed=false` (the default) re-throws the
`KlvDecodeException` wrapped in a `RuntimeException` — corruption is surfaced,
not silently lost. Set `skipMalformed(true)` to silently drop the malformed
entry and continue. Unrecognised ULs are dropped when `skipUnknown=true`
(default) and surfaced (as a `KlvEntry` with `parsed=null`) when
`skipUnknown=false`.

The defaults (`withPts=false, parsed=false, skipUnknown=true, skipMalformed=false`)
yield raw `byte[] raw` entries with no PTS, matching the tst-py
`extract_klv` default call.

### Write a file

`Muxer.writeFile` returns a `MuxerFileSink` that auto-drains pending TS
packets to disk after each `push*` call and on `close`. The muxer is
**borrowed**, not consumed — it remains usable after the sink closes.

**Non-atomic write** (normal case):

<!-- compile: java-statements -->
```java
import org.tstrans.mpegts.*;
import java.nio.file.Path;
// annexBNal and klvBytes are byte[] values prepared beforehand.

MuxerConfig cfg = MuxerConfig.builder()
    .addVideo(0x1011, VideoCodec.H264)
    .addKlv(0x1012, KlvStreamType.SYNCHRONOUS_METADATA, /*carriesPts=*/ true)
    .build();

try (Muxer m = new Muxer(cfg);
     var sink = m.writeFile(Path.of("out.ts"))) {
    sink.pushVideo(annexBNal, /*pts=*/ 0L, /*keyFrame=*/ true);
    sink.pushKlv(klvBytes, /*pts=*/ 0L, /*metadataServiceId=*/ 0);
}
// out.ts now contains whatever was pushed, even if an exception was thrown
// partway through (non-atomic: partial output is preserved).
```

**Atomic write** — the destination is only promoted from a `.partial` temp
on explicit success:

<!-- compile: java-statements -->
```java
try (Muxer m = new Muxer(cfg);
     var sink = m.writeFile(Path.of("out.ts"), /*atomic=*/ true)) {
    sink.pushVideo(annexBNal, 0L, true);
    sink.pushKlv(klvBytes, 0L, 0);
    sink.commit();  // mark success — close() will now promote the temp
}
// Without commit(), close() discards the .partial temp; out.ts is never written.
```

**Why `commit()` exists.** Python's `with`-statement gets `exc_type` in
`__exit__`, so tst-py can infer success automatically. Java's
`AutoCloseable.close()` has no exception hook, so atomic mode requires an
explicit `commit()` call on the success path: a committed sink promotes the
`.partial` temp file to the destination on `close()`; a sink that closes
without `commit()` — whether because of an exception or a missing call —
discards the temp and leaves the destination untouched.

The `MuxerFileSink` push family mirrors the `Muxer` push family exactly —
`pushVideo`, `pushVideoWire`, `pushKlv`, `pushAudio`, `pushSubtitle`,
`pushData`, `pushDataTo`, plus all handle-targeted variants (`pushVideoTo`,
`pushVideoWireTo`, `pushKlvTo`, `pushAudioTo`, `pushSubtitleTo`) and DTS
variants (`pushVideoToWithDts`, `pushVideoWireToWithDts`) — and also declares
`IOException` (each call drains packets to disk). Argument shapes and PTS units
(90 kHz) are identical; the `(pts, payload)` argument order on
`pushSubtitle`/`pushSubtitleTo` matches `Muxer`.

## SRT transport (`org.tstrans.srt`)

The shells (`Sender`, `Receiver`, `MuxSender`, `DemuxReceiver`) live in
`org.tstrans.shells` and open any transport from a URL (`Class.open(url, ...)`);
the `org.tstrans.srt` package holds the low-level `Builder`/`Socket`/`Listener`,
the `Managed*` reconnect family and the SRT policy types. SRT is default-on — no
feature flag is needed.

### Sender hello

The simplest sender: connect to a peer in caller mode and stream bytes. A
passphrase must be 10 to 80 bytes, and the receiver must be configured
with the same one (`example-shared-secret` here).

<!-- compile: java-statements -->
```java
import org.tstrans.shells.Sender;
import org.tstrans.SrtException;

// mode=caller is the default when ?mode= is omitted.
try (var tx = Sender.open(
        "srt://host:9000?mode=caller&passphrase=example-shared-secret")) {
    tx.sendBytes(tsBytes);  // push pre-muxed TS bytes (any length)
    tx.flush();             // emit any buffered partial bundle
}
```

`sendBytes` accepts raw TS bytes of any length; the sender internally frames
them into 7-packet (1316-byte) SRT bundles. Call `flush()` after the last push
to emit any partial bundle, or `finish()` to end the stream: it flushes, then
closes the transport without cancelling it first (so a transport that queues,
an RTSP push, writes its queue), throws the first flush error, and leaves the
handle for `close()`, which then does nothing more. A stream of one or two
aligned TS packets is not sent by either, and nothing reports it: the sender
forwards bytes only once three aligned packets have confirmed the TS
alignment; three or more are sent.

### RecvTransport hello

The simplest receiver: bind in listener mode, accept one connection, and drain
packets.

<!-- compile: java-statements -->
```java
import org.tstrans.shells.Receiver;
import org.tstrans.SrtException;

// Receiver.open does a one-shot bind + accept.
try (var rx = Receiver.open("srt://:9000?mode=listener")) {
    while (true) {
        byte[] pkt = rx.recvBytes();  // one 188-byte TS packet per call
        // process pkt ...
    }
}
```

`recvBytes()` returns one 188-byte TS packet per call (the SRT live-mode unit).
Break the loop on three kinds, which together cover every way a receive ends:
`SrtException(END_OF_STREAM)` — the peer closed the session cleanly (what a
C-ABI sender, a `ManagedSender` or `srt-live-transmit` produces, and what a
`ManagedReceiver` reports once its reconnect budget is exhausted; on a plain
`srt://` shell libsrt reports the close as a broken link first, so expect
`BROKEN` before `END_OF_STREAM`);
`SrtException(CLOSED)` — you closed or cancelled this receiver;
`SrtException(BROKEN)` — the link failed. Catching only `CLOSED` and `BROKEN`
(the pre-0.7.0 advice) rethrows at a clean hang-up.

### Builder → Socket → intoReceiver (low-level path)

Use the `Builder` → `Listener` → `accept` → `Socket` → `intoReceiver()` path
when you need the listener's bound port (for example, when binding to an
ephemeral `:0` port and reporting it to the sender):

<!-- compile: java-statements -->
```java
import org.tstrans.srt.*;
import org.tstrans.shells.Receiver;
import org.tstrans.SrtException;

// Bind to a kernel-assigned ephemeral port.
try (Listener listener = new Builder("srt://127.0.0.1:0")
        .listener()
        .listen()) {

    int port = listener.localAddr().getPort();   // get the assigned port
    System.out.println("listening on port " + port);

    // Accept the first incoming peer (infinite wait).
    // Use accept(null) rather than accept(timeoutMs) for reliable
    // TSBPD-wakeup on the accepted socket (see Gotchas below).
    Socket sock = listener.accept(null);
    try (Receiver rx = sock.intoReceiver()) { // consumes the Socket
        byte[] pkt = rx.recvBytes();
        // ...
    }
}
```

`Listener.localAddr()`, `Socket.localAddr()` and `Socket.peerAddr()` return an
`InetSocketAddress` built from the native address: no name lookup, an IPv6
scope id kept, and an IPv4 peer reached through an IPv6 socket reported as an
`Inet4Address`. The `HostPort` record they returned before is deprecated and
removed in 0.12.0.

### Listen rules

`listener.rules()` returns a `ListenRules` view that decides each caller
during the SRT handshake, keyed by the exact stream ID. The default rule is
accept, so a listener you never touch behaves as before. Edits are safe from
any thread, including while another thread is parked in `accept`.

<!-- compile: java-statements -->
```java
import org.tstrans.srt.*;

try (Listener listener = new Builder("srt://127.0.0.1:9000")
        .listener()
        .listen()) {
    ListenRules rules = listener.rules();
    rules.set("publish:amber", ListenRule.acceptWith("amber-secret-1"));
    rules.setDefault(ListenRule.reject(RejectReason.NOT_FOUND));
    // The accepted Socket is AutoCloseable; the listener's close() does not close it.
    try (Socket sock = listener.accept(null)) {
        // The accepted peer is "publish:amber" (the only rule that admits one):
        // refuse the next caller that uses the same name.
        rules.set("publish:amber", ListenRule.reject(RejectReason.CONFLICT));
    }
}
```

`remove(String)` returns whether a rule existed, `clear()` drops every
explicit rule and `size()` counts them. `RejectReason` has constants such as
`FORBIDDEN`, `CONFLICT` and `OVERLOAD`, constants for libsrt's internal
codes 0 to 17 (`BAD_SECRET`, `UNSECURE`, …), `userDefined(int)` for
application codes of 2000 or more, and `ofCode(int)`. `ListenRule.reject`
takes codes of 1000 or more. `set` throws `SrtException(CONFIG_INVALID)` for a bad
passphrase, a reject code above 2147482647 (which libsrt cannot send), or a
NUL byte in a stream ID or rule key. A stream ID, rule key or passphrase with an unpaired UTF-16 surrogate is refused with `IllegalArgumentException` before the listener is consulted (JNI would otherwise decode it lossily and two distinct strings could share one rule). On a closed listener every other `set`/`setDefault` throws `SrtException(CLOSED)` even when the rule itself is invalid. After `Listener.close()` every `ListenRules` method throws
`SrtException(CLOSED)` and `Listener.rules()` throws `IllegalStateException`.
After `cancelHandle().cancel()` without a `close()`, `Listener.rules()` still
returns a view, and every method of that view throws `SrtException(CLOSED)`.
A rule installed after `accept` returns does not cover a caller whose
handshake already completed, so keep your own check after `accept` when it
matters. A refused caller's connect (`Builder.caller().connect()` or an
`srt://` shell's open) throws `SrtException(CONNECT_FAILED)` with the reason
name in the message and the reason itself in `rejectReason()`: the rule's
`RejectReason` constant, a reason named `OTHER` that keeps an application
code, or an internal one such as `BAD_SECRET` for a wrong passphrase
(`UNSECURE` when only one side has one). `rejectReason()` is empty for every
other error. The listener's `accept` never sees a caller it refused, and the
managed shells retry a refused connect, so neither reports a reason. `AccessControl.parse(streamId)` (throws
`IllegalArgumentException` for any other form) reads
the `#!::` stream-ID form into `user()`, `resource()`, `host()`, `session()`
(each an `Optional<String>`), `kind()`, `mode()`, `get(key)` and `all()`.
The semantics, including the passphrase and key-length rules, are those of
the Rust listener in the [SRT guide](/docs/guides/srt.md#listen-rules).

The accepted `Socket` is consumed by `intoReceiver()` — the `Socket` handle
is zeroed immediately after the JNI call. Attempting to call any `Socket`
method after `intoReceiver()` throws `IllegalStateException`. To produce a
sender from an accepted socket use `sock.intoSender()` instead.

`Builder` also exposes `connect()` for the caller side:

<!-- compile: java-statements -->
```java
import org.tstrans.srt.Builder;
import org.tstrans.srt.Socket;

try (Socket sock = new Builder("srt://host:9000")
        .caller()
        .latencyMs(200)
        .passphrase("hunter2hunter2")
        .connect()) {
    try (org.tstrans.shells.Sender tx = sock.intoSender()) {
        tx.sendBytes(tsBytes);
        tx.flush();
    }
}
```

`Builder.connectTimeoutMs(ms)` (URL key `conntimeo`) is the budget for the
whole connect, shared across the addresses the host resolves to.
`Builder.peerIdleTimeoutMs(ms)` (URL key `peeridletimeo`) is how long a
connected peer may stay silent; libsrt does not detect a silent peer sooner
than about 5 s, whatever the value. The [SRT guide](/docs/guides/srt.md)
has the detail.

### Cancellation

The library never runs your code on its threads: no callback of yours is ever invoked from a thread the library owns. See [Architecture](/docs/reference/architecture.md) for the rule.

Obtain a `org.tstrans.shells.CancelHandle` from an open `Sender`, `Receiver`,
`MuxSender` or `DemuxReceiver` (and their `Managed*` twins) and call `cancel()` from another
thread to unblock a parked `sendBytes` / `send*` / `recvBytes` / `next()` call.
After a cancel, every receive, send, `flush()` and `finish()` on these shells
throws `CLOSED`, buffered data included. `org.tstrans.rtp.H264Receiver` is the
exception: after a cancel its `recvAu()` still returns the access units it
holds, and the one it can flush, then throws `RtpException(CLOSED)`; a cancelled
for-each throws a `RuntimeException` with that cause:

<!-- compile: java-statements -->
```java
import org.tstrans.shells.Sender;

var tx = Sender.open("srt://host:9000?mode=caller");
var cancel = tx.cancelHandle();

// On another thread:
cancel.cancel();  // wakes tx.sendBytes() → throws SrtException(CLOSED)
```

`CancelHandle` is safe to share across threads. On every shell, plain or
managed, the parked call ends with `CLOSED` (message `cancelled from another
thread`): the plain shells close the underlying libsrt socket and report the
cancel they observed — the shell is dead afterwards — and the `Managed*`
shells latch their close flag and wake whatever the reconnect loop is parked
in (a live send/receive, the backoff wait, a re-dial or re-accept).
A second `cancel()` is a no-op; every later send or receive on the shell
raises `CLOSED`. Obtaining the handle is
itself safe from another thread at any time — `cancelHandle()` returns
promptly even while `sendBytes` / `send*` / `recvBytes` / `next()` / `accept()`
is parked on the same object, so it need not be taken before iterating.

Since 0.7.0 `isCancelled()` reports the SHELL's one cancel state, not the
handle's own: it is `true` once `cancel()` was called on ANY handle of that
shell, or the shell was `close()`d (close cancels first). Every handle of one
shell therefore agrees, and a handle that outlives its shell's `close()`
answers `true` rather than `false`. Before 0.7.0 each handle carried a private
flag, so a second handle read `false` after the first cancelled and `close()`
set nothing.

#### Cancelling an open

An open that waits for a peer has no shell, and so no `cancelHandle()`, until
it returns: a listener-mode `srt://` `Receiver.open`, `DemuxReceiver.open`,
`ManagedReceiver.fromUrl` or `ManagedDemuxReceiver.fromUrl` blocks in its
first accept until a caller connects. To cancel one, make an
`org.tstrans.shells.OpenCancel` first, pass it as the last argument of the
open and call its `cancel()` from another thread:

<!-- compile: java-statements -->
```java
import org.tstrans.shells.OpenCancel;
import org.tstrans.shells.Receiver;

try (var oc = new OpenCancel()) {
    // On the opening thread:
    var rx = Receiver.open("srt://:9000?mode=listener", oc);
    // On another thread:
    oc.cancel();  // the open throws SrtException(CLOSED) and releases the port
}
```

The overloads are `Receiver.open(url, cancel)`, `Sender.open(url, cancel)`,
`DemuxReceiver.open(url, demuxConfig, cancel)`,
`MuxSender.open(url, programConfig, cancel)`,
`ManagedReceiver.fromUrl(url, policy, cancel)`,
`ManagedSender.fromUrl(url, policy, cancel)`,
`ManagedDemuxReceiver.fromUrl(url, demuxConfig, policy, cancel)` and
`ManagedMuxSender.fromUrl(url, programConfig, policy, cancel)`. The rules:

- One object may be given to any number of opens, one after another or at the
  same time; one `cancel()` ends every open in progress with it.
- `cancel()` is sticky: every later open given the object opens nothing. With
  an already-cancelled object an open is refused with `CLOSED` before its
  configuration and its URL are looked at. The shell
  opens throw the URL's transport exception (a URL whose scheme no shell opens
  still throws `IllegalArgumentException`: there is no transport to name). The
  managed `fromUrl`s are SRT only and throw `SrtException(CLOSED)` whatever
  the URL.
- It ends a listener-mode first accept at any time. It does not interrupt a
  caller-mode connect, but an open that finishes after the cancel throws
  `CLOSED` whatever it did, and a connection it had just made is closed
  again: nothing is handed out. The senders' opens take the object for this
  rule and for the refusal of an object already cancelled.
- It is not the shell's cancel handle. An open that returned before the
  cancel is untouched by it, and so are a managed receiver's later
  reconnects; from then on the shell's `cancelHandle()` is its cancel.
- `close()` frees the Java object only: an open parked with it keeps waiting
  for its peer. After `close()`, `cancel()`, `isCancelled()` and an open
  given the object throw `IllegalStateException`.
- `cancel()` and `isCancelled()` are safe from any thread, any number of
  times; `isCancelled()` and `close()` do not wait for a `cancel()` that is
  still closing listening sockets on another thread.

A `null` object is the same as the open without it.

### What answers while another thread is parked

A blocking call holds its object for as long as it is parked, so what a
second thread (a watchdog, a UI, a shutdown path) may ask of that object
falls into two groups.

**Answers at once, whatever the other thread is doing:**

| Accessor | While a call is parked | Once closed |
|---|---|---|
| `close()` (every class but `RtspSession`, `RtspServer` and `OpenCancel`: see below), `CancelHandle.cancel()` / `isCancelled()` | cancel first, then free | quiet |
| `cancelHandle()` | a new handle | throws `IllegalStateException` |
| `isAlive()` | `true` — a parked call means the object is open | `false` |
| `localAddr()` (`srt.Listener`, `H264Receiver`) | the value captured when the object was built | throws `IllegalStateException` |
| `localPort()` (`Receiver`, `DemuxReceiver`, `tcp.Listener`) | the port captured at open (`tcp.Listener` answers while `accept()` is parked) | throws `IllegalStateException` |
| `localAddr()`, `peerAddr()`, `sourceRejected()` (`Receiver`, `DemuxReceiver`) | the bound address captured at open; the publisher's address and the count of what the `source` pin turned away, as they stand | throws `IllegalStateException` |
| `rtpStats()` (`Receiver`, `DemuxReceiver`) | the RTP counters as they stand; empty off RTP | the counters as the close left them |
| `tsBytesDemuxed()` (`DemuxReceiver`, `ManagedDemuxReceiver`) | the count as it stands | the count at close |
| `peerAddr()` (`tcp.Transport`) | the address captured at accept | throws `IllegalStateException`, also once the connection is converted into a shell |
| `videoHandle()` … `dataHandle()` on `MuxSender`, `ManagedMuxSender` and `MountHandle` | the handles of the configuration in force: the one the sender was built from, or the last one `reconfigure` accepted (`MountHandle` has no `reconfigure`) | throws `IllegalStateException` |
| `reconnectAttempts()`, `reconnectStats()` on the managed shells | live counters kept outside the object | throws `IllegalStateException` |
| `stats()`, `endReason()`, `endDetail()` (`RtspPushSession`) | read outside the session, so a waiting send does not hold them up | the values the session ended with |
| `endReason()` / `endDetail()` | the recorded reason, or empty; `H264Receiver` answers empty while a `recvAu()` is in flight | the reason recorded at close |

`RtspServer.close()` runs the graceful `stop()` before it frees the server,
so it takes as long as `stop()` (a parked `nextPublisher` does not hold it
up). `RtspServer.stop(long)` ignores its argument (`gracefulShutdownDrainMs`
sets the wait): it is deprecated and removed in 0.12.0. `OpenCancel.close()`
frees the object only.

One rule covers a closed object: every call the table above does not list
as answering once closed throws `IllegalStateException("<Class> is closed")`,
also when another thread's `close()` lands between the call's own open check
and its native work. Only a call that was already parked when the close or a
cancel landed ends with the transport's `CLOSED` instead. These calls answer
a closed object too, although they are not in the table (while the object is
open, `finish()` waits behind a parked send):

- `finish()` on `Sender`, `MuxSender` and `ManagedMuxSender` returns quietly;
- `RtspSession.isTornDown()` returns `true`;
- `RtspPushSession.isSessionAlive()`, like `isAlive()`, returns `false`;
- `HlsServerHandle.shutdown()`, which is its `close()`, returns quietly;
- `toString()` answers on every class;
- `iterator()` returns an iterator, whose first `hasNext()` throws
  `IllegalStateException`.

`RtspSession` differs. Its `close()` cancels only a `pause()` / `play()` /
`teardown()` in progress on another thread (that call throws `CLOSED`, and
the session then ends as a cancel: a live receiver ends
`RtpException(CLOSED)`); an idle session is not cancelled first, so a
TEARDOWN the server accepts still ends a live receiver with end of stream.
It, and the `close()` of an `H264Receiver` made from a session, then end the
RTSP client: up to 500 ms for the TEARDOWN's reply and up to 2 s for the
client's thread (see [RTSP client](#rtsp-client-orgtstransrtp)).
`RtspSession.cancelHandle()` returns at once while a control call is in
progress; the control calls themselves run one at a time.

**Waits for the parked call to return:** `stats()` (but not
`RtspPushSession.stats()`), `socketStats()`,
`srtStats()`, `depayStats()`, `H264Receiver.rtpStats()` and `lastSeen()`. These
read counters that live inside the object and have nothing to report
without it. Poll them from the thread that drives the send or receive
loop, or give the receiver a receive deadline (`?recv_timeout=<ms>` on
`rtp://` / `rtsp(s)://`, `?x-recvtimeout=<ms>` on `srt://`) so its blocking
call returns on a cadence (the [`lastSeen` caveat](#per-stream-staleness-lastseen)
spells this out). `finish()` is a blocking operation in its own right.

Through 0.6.x `isAlive()` on every class except the two `DemuxReceiver`s,
the `MuxSender` handle getters, and `H264Receiver.endReason()` /
`endDetail()` were in the second group.

### Exiting with a call still parked

`close()` every shell (try-with-resources) and join your threads before the
JVM exits — that is still the shape to write. If a thread is nevertheless
inside an SRT native call when the JVM shuts down (`System.exit`, or `main`
returning with only daemon threads left), the native library gets it out: its
exit handler refuses every SRT call that has not started yet, closes every SRT
socket that is still open, waits for the native calls in flight to return —
the whole call, not only the part that blocks — and only then runs libsrt's
own cleanup. Before 0.7.0 a thread left in an accept made that cleanup wait
forever: the JVM ran its shutdown hooks and then never terminated.

Two limits. The wait is bounded at 2 s; if a call is still in flight after
that, libsrt's cleanup runs anyway and that thread may crash or hang the
exiting JVM, which is what every such program risked before the guard
existed. And the 2 s does not bound exit as a whole: closing a connected
sender that still has unsent data takes up to that socket's linger time
(`?linger=` on the URL; 5 s by default on the sender opens), and the handler
closes the open sockets one after the other before the wait starts.

This matters most for the listener-mode opens — `DemuxReceiver.open`,
`Receiver.open`, `ManagedReceiver.fromUrl` and `ManagedDemuxReceiver.fromUrl`
with `?mode=listener` block in their first accept BEFORE returning an object,
so there is no shell to `close()` while they wait for a peer (an
`OpenCancel` given to the open can end that wait: see
[Cancelling an open](#cancelling-an-open)). The unparked thread does not get
to observe an exception: the JVM is already past the point where Java code
runs. A program that closed everything pays nothing at exit.

**Lifetimes are explicit.** The JVM binding releases native resources only on
`close()` (try-with-resources): there is no `Cleaner` and no finalizer. A
handle you never close keeps its native resources until the process exits, and
the binding supports one class loader per VM.

### SRT-specific Gotchas

- **One-shot accept on `Receiver.open`.** `Receiver.open` binds,
  listens, and accepts exactly one connection — it is not a multi-client
  server. For a server that accepts many peers, use `Builder.listener().listen()`
  and iterate or call `listener.accept(null)` in a loop.
- **`accept(null)` vs `accept(timeoutMs)`.** On the accepted socket,
  `recvBytes()` uses libsrt's edge-triggered epoll internally. If you use
  `accept(timeoutMs)` (epoll-based accept), there is a subtle interaction
  where the accept epoll subscription can prevent `srt_recv` from waking on
  TSBPD delivery when data arrives before `recvBytes()` is called. Prefer
  `accept(null)` (direct `srt_accept`) for the accepted socket's receiver path.
  `Receiver.open` always uses `srt_accept` directly and is unaffected.
- **Cancel wakes with `CLOSED`.** `CancelHandle.cancel()` wakes a thread
  parked in `sendBytes` or `recvBytes`; that call throws
  `SrtException(CLOSED)`. (Through 0.6.x the plain shells threw `BROKEN`.)
- **`Receiver.close()` / `DemuxReceiver.close()` cancel first.** Calling
  either from another thread while `recvBytes()` / `next()` is parked wakes
  that call — it throws `SrtException(CLOSED)` — and `close()` returns
  promptly. The managed pair (`ManagedReceiver` / `ManagedDemuxReceiver`) does
  the same and records `EndReason.CANCELLED`. A `close()` with nothing
  parked simply closes.
- **Every srt sender's `close()` cancels first too.** `MuxSender.close()` /
  `Sender.close()` from another thread while a `send*` / `sendBytes()` is
  parked (libsrt blocked on a full send buffer) wakes that call — it throws
  `SrtException(CLOSED)` — and `close()` returns promptly.
  `ManagedMuxSender.close()` / `ManagedSender.close()` do the same for a send
  parked anywhere in the reconnect loop (a live send, the backoff wait, a
  re-dial), so a cross-thread close mid-outage no longer waits out the whole
  reconnect budget. `close()` is the prompt, lossy shutdown: bytes a prior
  transient error left pending are abandoned. One binding, one contract: this
  is what the rtp senders and the C ABI's `tst_*_sender_close` already do.
- **JDK-17 byte-copy posture.** `sendBytes` copies the supplied array across
  the JNI boundary; `recvBytes` returns a heap `byte[]` copy. A zero-copy
  path using FFM `MemorySegment` is deferred to a JDK-22+ release.
- **`recvBytes()` returns one packet quantum.** Each `recvBytes()` call
  returns exactly one 188-byte TS packet. Accumulate packets to reconstruct
  a larger frame.
- **`.listener()` supplies the URL's `mode=listener`.** When the URL has no
  `mode=`, `new Builder("srt://:9000").listener().listen()` binds a listener.
  A URL that says `mode=caller` is refused with `CONFIG_INVALID`. Without
  `.listener()`, `listen()` needs `?mode=listener` in the URL.
- **Negative knob values throw `IllegalArgumentException`.** All non-negative
  knob setters (`latencyMs`, `connectTimeoutMs`, etc.) reject negative values
  at construction time, mirroring tst-py's `u32`-typed API.

## SRT convenience (`MuxSender` / `DemuxReceiver`)

The `org.tstrans.shells` package also exposes the high-level convenience shells that
own a muxer / demuxer and a transport in one object, so you never touch raw TS
bytes: `MuxSender` (send elementary streams → it muxes + sends) and
`DemuxReceiver` (it recvs + demuxes → you iterate typed `DemuxEvent`s). Each
opens the transport its URL names: `srt://`, `rtp://`, `udp://`, `tcp(s)://`
or `rist://`. A transport failure throws that transport's exception,
each a `BindingException`:

| URL scheme | Exception |
|---|---|
| `srt://` | `SrtException` |
| `rtp://` | `RtpException` |
| `udp://` | `UdpException` |
| `tcp://`, `tcps://` | `TcpException` (also `org.tstrans.tcp.Listener` and `Transport`) |
| `rist://` | `RistException` |

Every checked exception of this binding extends `BindingException`, whose
`kind()` returns the concrete class's `Kind` member, so a handler that
catches the base reads it without a cast. Keep or compare `kind().name()`,
not the ordinal: members are added in later releases.

<!-- compile: java-statements -->
```java
import org.tstrans.BindingException;
import org.tstrans.shells.Sender;

try (Sender tx = Sender.open("srt://media.example:9000")) {
    tx.sendBytes(tsBytes);
} catch (BindingException e) {
    // e.g. "CONNECT_FAILED: …" when no listener answers
    System.err.println(e.kind().name() + ": " + e.getMessage());
}
```

Every one carries a `Kind` enum; `CLOSED` is the kind a cancelled or closed
call raises.

### Send: `MuxSender`

Build a `MuxerConfig`, connect in caller mode, and send elementary streams:

<!-- compile: java-statements -->
```java
import org.tstrans.shells.MuxSender;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

MuxerConfig program = MuxerConfig.builder()
    .programNumber(1).pmtPid(0x1000)
    .addVideo(0x1011, VideoCodec.H264)
    .build();

try (MuxSender s = MuxSender.open(
        "srt://host:9000?mode=caller&latency=120", program)) {
    long pts = 0;
    for (byte[] annexBNal : accessUnits) {
        s.sendVideo(annexBNal, pts, /*keyFrame=*/ true);
        pts += 3000;  // 90 kHz PTS; advance per frame
    }
}
// MuxSender has NO flush(): bytes flush per-send and again on close().
// close() cancels first, then drains best-effort; finish() is the lossless path.
```

`MuxSender.cancelHandle()` returns a cross-thread `CancelHandle` (see
[Cancellation](#cancellation)); `close()` from another thread cancels first,
so a parked `send*` ends with `SrtException(CLOSED)` instead of blocking the
close.

**Lossless shutdown.** `close()` cancels first and abandons pending bytes;
`finish()` drains them, throws the first drain error as `SrtException`, then
closes. `finish()` does not cancel a parked `send*` first, so it waits behind
one; a second `finish()` is a no-op unless the sender has been cancelled
(after a cancel every `finish()` throws `CLOSED`, a repeated one included),
a `finish()` after `close()` is a no-op too, and the handle still takes a
`close()` afterwards. `ManagedMuxSender` has the same method.

`sendKlv`, `sendAudio`, `sendSubtitle`, and `sendData` (raw private-data
bytes, passed through verbatim — same PTS / ceiling semantics as
`Muxer.pushData`) cover the other elementary-stream kinds; the handle-targeted
`send*To` variants (including `sendDataTo` with the handle from `dataHandle()`)
address a specific stream in a multi-stream program.

### Receive: `DemuxReceiver`

Bind in listener mode and iterate the same sealed `DemuxEvent` hierarchy the
offline `org.tstrans.mpegts.Demuxer` produces:

<!-- compile: java-statements -->
```java
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.codec.VideoUnit;
import java.util.List;

try (DemuxReceiver rx = DemuxReceiver.open("srt://:9000?mode=listener")) {
    for (DemuxEvent e : rx) {
        if (e instanceof DemuxEvent.Video v) {
            // Opt-in typed units; parse() throws the checked DemuxException,
            // so declare it on the enclosing method.
            List<VideoUnit> units = v.parse();
        }
    }
}
// Iteration ends on clean EOF; a CLOSED/BROKEN SrtException surfaces (wrapped in
// a RuntimeException — the Iterator contract forbids checked exceptions).
```

### Raw stream access: `tsBytesDemuxed` and `Receiver` + `Demuxer`

`rx.tsBytesDemuxed()` returns the aligned TS bytes the receiver fed its
demuxer. It answers while `next()` is parked and never waits on the receive
slot. Once `close()` has returned, on any thread, it returns the count at
close.

To record or forward the raw packets, pull them from a `Receiver` and feed a
`Demuxer` yourself. Each chunk is a whole number of 188-byte packets:

<!-- compile: java-statements -->
```java
import org.tstrans.TcpException;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.Demuxer;
import org.tstrans.shells.Receiver;

try (Receiver rx = Receiver.open("tcp://127.0.0.1:9000");
     Demuxer demux = new Demuxer()) {
    try {
        while (true) {
            byte[] chunk;
            try {
                chunk = rx.recvBytes();
            } catch (TcpException e) {
                if (e.kind() == TcpException.Kind.END_OF_STREAM) break;
                throw e;
            }
            recorder.write(chunk);                  // your consumer
            demux.feed(chunk);
            for (DemuxEvent ev; (ev = demux.nextEvent()) != null; ) {
                // ... normal event handling ...
            }
        }
    } finally {
        demux.flush();                              // the final unbounded PES
        for (DemuxEvent ev; (ev = demux.nextEvent()) != null; ) { /* ... */ }
    }
}
```

Gotchas:

- **Flush at the end, also after an error.** Without `demux.flush()` the last
  access unit is lost.
- **Reconnects are yours.** A `DemuxReceiver` resets its demux state across a
  reconnect; this loop does not, and cannot: the binding `Receiver` (one
  188-byte packet per `recvBytes()`) and `Demuxer` expose no `resetSync()`,
  so neither can be reset at a reconnect the way the Rust recipe in the
  [pipeline guide](/docs/guides/pipeline.md) does. `reconnectAttempts()`
  counts factory attempts, failures included. Over a reconnecting source use
  `ManagedDemuxReceiver`, which drops the spliced packet itself and exposes
  `tsBytesDemuxed()`, or accept that one spliced packet may reach the
  demuxer.
- **No user code on the library's thread.** Your loop owns the recorder and
  every error it raises.

### Per-stream staleness: `lastSeen`

`rx.lastSeen(pid)` returns an `Optional<Instant>`: when this receiver last
emitted an item for `pid`, to the microsecond, or empty if `pid` was never
seen — a cheap staleness check with no `stats()` snapshot-diffing required.
`lastSeenMicros(pid)` (the same instant as a `Long` of Unix-epoch
microseconds, `null` for never) is deprecated and is removed in 0.12.0.
See the watchdog recipe under [RTP convenience](#rtp-convenience-muxsender--demuxreceiver)
below (the method is identical here). The same method also exists on
`org.tstrans.srt.ManagedDemuxReceiver`.

## SRT managed reconnect (`Managed*`)

The four `Managed*` shells add **automatic reconnect** to the plain SRT
shells: on a Broken/Closed transport they re-dial (caller) or re-bind+re-accept
(listener) under a `ReconnectPolicy` and resume, replaying buffered gap data.
They mirror `tstrans.srt.Managed{Sender,Receiver,MuxSender,DemuxReceiver}` and
wrap `tst_pipeline`'s `ManagedTransport` / `ManagedRecvTransport`.

### The reconnect policy

<!-- compile: java-statements -->
```java
import org.tstrans.srt.ReconnectPolicy;
import org.tstrans.srt.BackoffStrategy;
import org.tstrans.srt.OverflowPolicy;

ReconnectPolicy policy = ReconnectPolicy.builder()
    .maxAttempts(null)  // null = retry forever; an Integer caps the attempts
    .backoff(BackoffStrategy.exponential(/*baseMs=*/ 100, /*maxMs=*/ 10_000))
    .gapBufferCapacity(256)
    .overflowPolicy(OverflowPolicy.DROP_OLDEST)
    .build();
```

`maxAttempts(null)` means retry forever; pass an `Integer` to cap the attempts.
`gapBufferCapacity <= 0` throws `IllegalArgumentException`. The all-defaults
policy (`maxAttempts=10`, `exponential(100, 10_000)`, capacity 256,
`DROP_OLDEST`) is `ReconnectPolicy.defaults()` — or just pass `null` for the
policy argument.

### Send with auto-reconnect: `ManagedMuxSender`

Same `send*` API as the plain `MuxSender`, but the transport silently
rebuilds on a peer drop:

<!-- compile: java-statements -->
```java
import org.tstrans.srt.ManagedMuxSender;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

MuxerConfig program = MuxerConfig.builder()
    .programNumber(1).pmtPid(0x1000)
    .addVideo(0x1011, VideoCodec.H264)
    .build();

try (ManagedMuxSender s = ManagedMuxSender.fromUrl(
        "srt://host:9000?mode=caller&latency=120", program, policy)) {
    long pts = 0;
    for (byte[] annexBNal : accessUnits) {
        s.sendVideo(annexBNal, pts, /*keyFrame=*/ true);  // auto-reconnects on Broken/Closed
        pts += 3000;
    }
}
```

`ManagedMuxSender.cancelHandle()` reaches every phase of the reconnect loop —
a live send, the backoff wait between attempts, and a re-dial — and the parked
`send*` ends with `SrtException(CLOSED)` promptly in all three. `close()` from
another thread does the same before tearing down, so it never waits out the
reconnect budget.

### Receive with auto-reconnect: `ManagedDemuxReceiver`

Iterate the same sealed `DemuxEvent` hierarchy. Each transport reconnect emits
exactly one `DemuxEvent.ReconnectDiscontinuity` before the post-reconnect events
— **drop your per-stream caches on receipt** and rebuild from the next
`DemuxEvent.ProgramMap`:

<!-- compile: java-statements -->
```java
import org.tstrans.srt.ManagedDemuxReceiver;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.codec.VideoUnit;
import java.util.List;

try (ManagedDemuxReceiver rx = ManagedDemuxReceiver.fromUrl(
        "srt://host:9000?mode=caller&latency=120", policy)) {
    for (DemuxEvent e : rx) {
        if (e instanceof DemuxEvent.ReconnectDiscontinuity) {
            caches.clear();  // transport rebuilt — re-derive from the next ProgramMap
        } else if (e instanceof DemuxEvent.Video v) {
            List<VideoUnit> units = v.parse();  // checked: enclosing method throws DemuxException
            // ... handle video ...
        }
    }
}
```

Unlike the plain `DemuxReceiver` (listener only), `ManagedDemuxReceiver` accepts
`mode=caller` too — in caller mode it re-dials on each reconnect; in listener
mode it re-binds and re-accepts. `cancelHandle().cancel()` reaches every phase
of that reconnect — a live receive, the backoff wait between attempts, and a
re-accept parked with no peer in sight — and the iterator ends with
`SrtException(CLOSED)` promptly in all three.

**Why did a managed session end? (`endReason()`)** Once the receive loop has
stopped for good, `rx.endReason()` (on `ManagedDemuxReceiver` only — none of
the other three managed shells expose it) answers with an `org.tstrans.shells.EndReason`
member in an `Optional` — empty before any ending, including while a reconnect is still
in progress. Two of its three members are reachable on the managed-SRT path
today: `RECONNECT_EXHAUSTED` (the `ReconnectPolicy`'s `maxAttempts` budget
ran out — also what a plain peer close reports under a zero-retry policy,
since a peer FIN reaches the reconnect decorator as a retryable break) and
`CANCELLED` (`cancelHandle().cancel()` fired, or `close()` was called from
another thread while `next()` was parked — `close()` cancels first, so the
parked iteration ends with `SrtException(CLOSED)`; a `close()` with no
iteration in flight ends nothing and records nothing). `EndReason.END_OF_STREAM`
is reserved for a future receive transport that can signal a clean end
distinct from budget exhaustion — do not confuse it with the unrelated
`SrtException.Kind.END_OF_STREAM`, which 0.7.0 added for the ERROR a receive
raises at end of stream (see "Error kinds" below). The surface exists
specifically to tell a caller-initiated cancel apart from a budget-exhausted
give-up. It reads a lock-free cell captured when the receiver is opened, so
it answers while another thread is parked in `next()` and keeps answering
after `close()`.

### Stats drifts on the managed shells

The managed shells inherit the same `srtStats()` drifts as tst-py — **read these
carefully, they are not uniform across the four shells:**

- **`ManagedSender.srtStats()` and `ManagedReceiver.srtStats()` always throw
  `SrtException(IO)`** (the managed send/recv transports expose no
  SRT-rich shape); both are deprecated and removed in 0.12.0. Use
  `socketStats()` instead — it returns the scheme-neutral 16-field
  `org.tstrans.shells.SocketStats`.
- **`ManagedMuxSender` has no `srtStats()` at all.** It exposes a combined
  `stats()` returning a `TransportStats` (socket + muxer); use that.
- **`ManagedDemuxReceiver.srtStats()` returns a `SocketStats`** (NOT `SrtStats`)
  and does NOT throw — it returns the same value as its `socketStats()`.

### `reconnectAttempts()` semantics differ too

- **`ManagedReceiver.reconnectAttempts()` is a SUCCESS count** — the number of
  completed reconnects (it excludes the initial bind+accept).
- **`ManagedMuxSender.reconnectAttempts()` and
  `ManagedDemuxReceiver.reconnectAttempts()` are ATTEMPT counts** — every
  reconnect-factory invocation since construction (a failed-and-retried rebuild
  still bumps the counter).
- **`ManagedSender` has no `reconnectAttempts()`.**

### Background reconnect (`ReconnectMode`)

Set `.mode(ReconnectMode.BACKGROUND)` on the `ReconnectPolicy.Builder` handed to
`ManagedSender` / `ManagedMuxSender` to move the reconnect loop off the
caller's thread — a dedicated per-outage worker owns backoff + the factory
call, and while that worker is active (or the gap buffer still holds a
backlog) `send`/`sendVideo`/… enqueue into the gap buffer under
`overflowPolicy` instead of blocking. With no worker and an empty gap buffer
a send goes straight to the transport and can still block on network I/O:

<!-- compile: java-statements -->
```java
import org.tstrans.srt.ManagedMuxSender;
import org.tstrans.srt.ReconnectPolicy;
import org.tstrans.srt.ReconnectMode;
import org.tstrans.srt.ManagedTransportStats;

ReconnectPolicy policy = ReconnectPolicy.builder()
    .mode(ReconnectMode.BACKGROUND)  // default is BLOCKING
    .build();

try (ManagedMuxSender s = ManagedMuxSender.fromUrl(
        "srt://host:9000?mode=caller&latency=120", program, policy)) {
    // ...
    ManagedTransportStats stats = s.reconnectStats();
    if (stats.reconnecting()) {
        System.out.println("outage in progress, " + stats.gapLen() + " messages queued");
    }
}
```

`reconnectStats()` (also on `ManagedSender`) returns a frozen
`ManagedTransportStats` (`reconnectAttempts`, `reconnectSuccesses`,
`gapLen`, `gapMessagesDropped`, `gapBytesDropped`, `reconnecting`) regardless
of `mode` — `reconnecting()` just stays `false` under the default `BLOCKING`
mode, since that mode's reconnect runs synchronously inside the call that
observed the break rather than on a separate worker. `BACKGROUND` is
send-side only: handing it to `ManagedReceiver` / `ManagedDemuxReceiver` logs
a warning and those classes reconnect on the caller's thread anyway.

## UDP, TCP and RIST ingest on your own ports

A server that takes ingest on ports it reserves needs answers the URL
alone does not give: which port a receiver bound when it was opened on port
0, and who is publishing. The receiving shells of `org.tstrans.shells`
answer the first with `localPort()` and `localAddr()`, and, on `udp://` and
`rist://`, the second with `peerAddr()`; a `source` key in the URL keeps a
UDP or RIST receiver to one publisher. `org.tstrans.tcp.Listener` accepts
TCP connections, reports each peer and turns the connection into a shell.

### `localPort()` on UDP and RIST receivers

<!-- compile: java-statements -->
```java
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.shells.Receiver;

try (Receiver udp = Receiver.open("udp://0.0.0.0:0");
     DemuxReceiver rist = DemuxReceiver.open("rist://@0.0.0.0:0")) {
    int udpPort = udp.localPort().getAsInt();
    int ristPort = rist.localPort().getAsInt();
    // hand udpPort and ristPort to the publishers
}
```

`Receiver.localPort()` and `DemuxReceiver.localPort()` return an
`OptionalInt`: the port a `udp://` or `rist://@host:port` receiver bound,
the kernel's pick when the URL said port 0, and so does an `rtp://` receiver.
An `srt://` listener reports the port the accepted connection arrived on;
its open returns only once a caller connected, so port 0 in an `srt://` URL
cannot tell a caller where to dial. Every other shell answers empty: a
caller, a shell made from a connection a listener object accepted, a
receiver taken from an RTSP session or a publish mount. The value is the
one captured at open, so it answers while a receive is parked on another
thread; after `close()` it throws
`IllegalStateException`. `localAddr()` returns the bound address as an
`Optional<InetSocketAddress>`, present exactly where `localPort()` is, with
the same port.

`recvBytes()` returns aligned TS packets (one 188-byte packet per call),
whatever datagram size the publisher sends. `Receiver.recvBytes(int)` and
`srt.ManagedReceiver.recvBytes(int)` ignore their argument: they are
deprecated and removed in 0.12.0. A receiving shell returns its
first packet only once it has found four sync bytes 188 bytes apart (the
starts of four aligned TS packets); a stream shorter than that yields
nothing. A `udp://` receive blocks until a packet is ready or the
shell is cancelled or closed. A `rist://` receive throws
`RistException(BACKPRESSURE)` about every 100 ms while nothing arrives; the
receiver stays open, so call it again:

<!-- compile: java-statements -->
```java
import org.tstrans.RistException;
import org.tstrans.shells.Receiver;

try (Receiver rx = Receiver.open("rist://@0.0.0.0:0")) {
    while (true) {
        byte[] ts;
        try {
            ts = rx.recvBytes();
        } catch (RistException e) {
            // nothing arrived for ~100 ms
            if (e.kind() == RistException.Kind.BACKPRESSURE) continue;
            throw e;
        }
        recorder.write(ts);                         // your consumer
    }
}
```

`DemuxReceiver.recvEvent()` throws the same; use it rather than the
`Iterator`, which wraps the exception in an unchecked `RuntimeException`.
The publisher opens `rist://host:port`, without the `@`. Its first packets
can be lost while the RIST session comes up.

**RIST profiles and ports.** The Main profile, the default, carries its
control traffic (RTCP) on the data port, so one port is all it needs: it
binds port 0 and reports the port. The Simple profile (`?profile=simple`)
sends RTCP on a second port, the data port + 1, and needs an even data
port; name an even port whose odd neighbour is free too. A Simple-profile
receiver on port 0 is refused at open with
`RistException(INVALID_CONFIG)`, because librist misreads a port the
kernel picked and would put RTCP on the wrong port. A URL with `?secret=`
(encryption) uses the Main profile, also when it says `?profile=simple`.

### The publisher's address: `peerAddr()` and the `source` pin

`Receiver.peerAddr()` and `DemuxReceiver.peerAddr()` return an
`Optional<InetSocketAddress>`: the address of the sender of the datagram
(`udp://`) or block (`rist://`) the transport delivered last, empty until
the first one is delivered. A `source` key in the URL keeps the receiver to
one IP: `source=<ip>`, or `source=first`, which takes the IP of the first
datagram or block delivered. Neither is proof of who is sending. The
address is the source address the datagrams carry, and nothing proves the
sender can receive there: it can be forged (a TCP peer address is proven by
the handshake, a UDP or RIST one is not). On `udp://`, `source=first` is
taken by the first datagram with a payload that reaches the port, whoever
sent it; on a `rist://` receiver without a key, by the first block
delivered, whoever sent it; with a pre-shared key, only by a sender whose
data decrypts, and that includes a recording of the publisher's earlier
data replayed from any address. Either way it holds until the receiver is
closed: there is no release. Use the address for logging and per-address
rules, not as an identity.

A server that gives each stream name its own port can let the first
publisher take the port and then decide whether to keep it. The wait for
that first publisher needs a limit, as any idle port does, and here it
matters more: the receive returns only aligned TS packets (four sync bytes
188 bytes apart), so a first datagram with a payload that is not TS (a probe, a stray
byte) takes the lock and the receive then returns nothing until the
receiver is closed, unless that sender goes on to send TS. The example
closes the receiver when its idle limit passes and opens the port again:

<!-- compile: java-class -->
```java
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.time.Duration;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.Predicate;
import org.tstrans.BindingException;
import org.tstrans.shells.Receiver;

/** One stream name's UDP port: the first publisher to send takes it. */
final class UdpNameIngest implements AutoCloseable {
    private final Receiver rx;

    UdpNameIngest(int port) throws BindingException {
        // source=first: the IP of the first datagram with a payload takes
        // the receiver until close(), whoever sent it; any other IP is
        // dropped and counted in sourceRejected(), and its sender is not told.
        rx = Receiver.open("udp://0.0.0.0:" + port + "?source=first");
    }

    /**
     * Waits up to {@code idleLimit} for the first TS packet and judges its
     * sender. Returns the packet, or null when the sender is refused or
     * nothing came in time; the receiver is then closed, and the caller
     * opens a new one to wait again.
     */
    byte[] admitFirst(Predicate<InetAddress> allowed, Duration idleLimit)
            throws BindingException {
        // The server's ordinary rule for an idle port. It also ends the wait
        // when a datagram with a payload that is not TS took the lock: the
        // receive returns only aligned TS packets, so it then returns nothing
        // until the receiver is closed, unless that sender goes on to send TS.
        // Whichever sets `decided` first wins: the receive or the limit.
        AtomicBoolean decided = new AtomicBoolean();
        ScheduledExecutorService timer = Executors.newSingleThreadScheduledExecutor();
        timer.schedule(() -> {
            if (decided.compareAndSet(false, true)) rx.close();
        }, idleLimit.toMillis(), TimeUnit.MILLISECONDS);
        byte[] first;
        try {
            first = rx.recvBytes();              // parks until a packet or close()
        } catch (BindingException | IllegalStateException e) {
            if (!decided.compareAndSet(false, true)) return null;  // the limit closed it
            rx.close();
            throw e;
        } finally {
            timer.shutdownNow();                 // drops a pending limit: no thread lingers
        }
        if (!decided.compareAndSet(false, true)) {
            rx.close();                          // the limit fired as data came
            return null;
        }

        // A packet was delivered, so the sender is known. The address is
        // what the datagrams carry, not proof of who sent them.
        InetSocketAddress publisher = rx.peerAddr().orElseThrow();
        if (!allowed.test(publisher.getAddress())) {
            // The lock is released only by closing. A new receiver on this
            // port is taken by the next datagram, whoever sends it.
            rx.close();
            return null;
        }
        return first;
    }

    byte[] next() throws BindingException { return rx.recvBytes(); }

    long turnedAway() { return rx.sourceRejected(); }

    /** From any thread: a recvBytes() parked on another ends with CLOSED. */
    @Override public void close() { rx.close(); }
}
```

<!-- compile: java-statements -->
```java
while (true) {
    try (UdpNameIngest ingest = new UdpNameIngest(port)) {  // the name's port
        byte[] ts = ingest.admitFirst(ip -> !blocked.contains(ip), Duration.ofSeconds(30));
        if (ts == null) continue;                // refused or idle: closed, open again
        while (true) {
            recorder.write(ts);                  // your consumer
            ts = ingest.next();
        }
    }
}
```

After the first packet the data loop has no limit of its own: a `udp://`
receive blocks until a packet is ready or the shell is closed, so a server
that must not wait forever closes the receiver from another thread when the
stream stops progressing.

A refused sender who keeps sending takes every reopened receiver again, so
refusing by `first` works only against a sender that stops.

- **Without the key** every sender that reaches the receiver is delivered,
  interleaved, and `peerAddr()` names whichever delivered last.
- **The pin compares the IP address, never the port**, so a publisher that
  restarts, or whose NAT changes its port, continues. If its IP changes,
  its datagrams are turned away and counted, and the receiver delivers
  nothing until it is closed and reopened.
- **`sourceRejected()`** counts what the pin turned away: datagrams on
  `udp://`; on `rist://`, datagrams from a new sender whose IP the pin
  refuses, datagrams whose source address could not be read, and delivered
  blocks refused on the read path (from an IP other than the one `first`
  latched, or from a sender the receiver holds no address for). Neither
  sender is told. On `udp://` a datagram turned away is in no received
  counter of `socketStats()`. On `rist://` a block refused on the read path
  surfaces from the receive as the same `RistException(BACKPRESSURE)` a
  quiet link gives, so the retry loop above carries on.
- **IPv4 on a dual-stack bind.** An IPv4 sender on a `[::]` bind is reported
  as an `Inet4Address` and compared in IPv4 form. The address is built from
  its bytes, with no name lookup.
- **Refusals.** A sender URL with the key is refused at open, and so is a
  value that is neither an IP literal nor `first`; so is an IPv6 pin on a
  `udp://` receiver bound to an IPv4 address. A misspelt key (`?sourse=`) is
  ignored like any unknown key on `udp://` and `rist://`, which leaves the
  receiver without a pin.
- **Other transports.** `peerAddr()` is empty and `sourceRejected()` reads 0
  on every other shell, a shell made from an accepted TCP connection among
  them (its `tcp.Transport.peerAddr()` names the peer before the
  conversion). What an `rtp://` URL's own `?source=` pin turns away is not
  counted by `sourceRejected()`.
- Both getters never wait behind a receive parked on another thread, and
  throw `IllegalStateException` after `close()`.

### TCP: a listener, one publisher at a time

A small class serves one publisher at a time on its own thread until
`close()`:

<!-- compile: java-class -->
```java
import java.net.InetSocketAddress;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicReference;
import org.tstrans.BindingException;
import org.tstrans.TcpException;
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.shells.CancelHandle;
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.tcp.Listener;
import org.tstrans.tcp.Transport;

/** Serves one TCP publisher at a time until {@link #close()}. */
final class TcpIngest implements AutoCloseable {
    private final Listener listener;
    private final CancelHandle stop;          // taken once, closed last
    private final AtomicReference<DemuxReceiver> serving =
        new AtomicReference<>();
    private final AtomicBoolean closed = new AtomicBoolean();
    private final Thread loop = new Thread(this::run, "tcp-ingest");

    TcpIngest(String url) throws TcpException {  // "tcp://0.0.0.0:0?listen=1"
        listener = Listener.open(url);
        stop = listener.cancelHandle();
    }

    int port() throws TcpException { return listener.localPort(); }

    void start() { loop.start(); }

    private void run() {
        while (true) {
            Transport conn;
            try {
                conn = listener.accept();
            } catch (TcpException e) {
                if (e.kind() == TcpException.Kind.CLOSED) return;  // close() ran
                // One connection failed, or no descriptor is free: the
                // listener stays open. Pause so a lasting failure does
                // not spin, then accept again.
                System.err.println("accept: " + e.getMessage());
                try {
                    Thread.sleep(100);
                } catch (InterruptedException ie) {
                    Thread.currentThread().interrupt();
                    return;
                }
                continue;
            }
            InetSocketAddress publisher = conn.peerAddr();  // read before the conversion
            try (DemuxReceiver rx = conn.intoDemuxReceiver()) {
                serving.set(rx);
                if (stop.isCancelled()) return;   // close() during accept()
                for (DemuxEvent ev; (ev = rx.recvEvent()) != null; ) {
                    // ... handle the event ...
                }
            } catch (BindingException | IllegalStateException e) {
                // This publisher failed, or close() closed its shell:
                // accept the next.
                if (!stop.isCancelled()) {
                    System.err.println(publisher.getHostString() + " port "
                        + publisher.getPort() + ": " + e.getMessage());
                }
            } finally {
                serving.set(null);
            }
        }
    }

    @Override public void close() {
        if (!closed.compareAndSet(false, true)) return;
        stop.cancel();               // a parked accept() ends with CLOSED
        DemuxReceiver rx = serving.get();
        if (rx != null) rx.close();  // a parked recvEvent() ends: CLOSED
        boolean interrupted = false;
        while (true) {               // the loop ends: the cancel is latched
            try {
                loop.join();
                break;
            } catch (InterruptedException e) {
                interrupted = true;  // finish the cleanup first
            }
        }
        stop.close();                // no thread can use it now
        listener.close();
        if (interrupted) Thread.currentThread().interrupt();
    }
}
```

<!-- compile: java-statements -->
```java
TcpIngest ingest = new TcpIngest("tcp://0.0.0.0:0?listen=1");
int port = ingest.port();          // hand it to the publisher
ingest.start();
// ... later, from any thread but the loop's own:
ingest.close();                    // throws nothing; an interrupt stays set
```

The `catch` around the shell keeps the server up when one publisher fails:
a reset connection, a TLS failure or malformed TS ends that connection, not
the listener. The `catch` around `accept()` returns only on `CLOSED`, the
kind a cancel or `close()` gives; any other kind leaves the listener open,
so the loop logs it and accepts again after a 100 ms pause. The pause adds
nothing to shutdown beyond its own length: the cancel is latched, so the
`accept()` after it throws `CLOSED` at once. The loop uses `recvEvent()`,
which throws the checked exception, rather than the `Iterator`, which wraps
it in an unchecked `RuntimeException`.

The listener's cancel handle is taken once because every `cancelHandle()`
call creates a native handle that stays allocated until its own `close()`,
and a closed handle's `cancel()` throws `IllegalStateException`. `close()`
closes it only after `join()`, so no thread can still be using it. An
interrupt of the closing thread does not cut that short: `close()` keeps
joining (the loop thread ends regardless, since the cancel is latched and the
served shell is closed), closes both handles, and only then sets the thread's
interrupt status again, so the listener is never left bound and the
interrupt is not lost.

`close()` ends the publisher being served by closing its shell, not through
a cancel handle of that shell: `DemuxReceiver.close()` is documented as
memory-safe against any concurrent native call, and it cancels first, so a
`recvEvent()` parked on the loop thread ends promptly with the transport's
`CLOSED` (a call that enters just after the close throws
`IllegalStateException`; the `catch` takes both). Closing the shell is
idempotent, so the loop's own try-with-resources close is then a no-op, and
there is no per-connection handle to leak. Cancelling the listener alone
would not end a connection it has already handed out. The `isCancelled()`
check covers a `close()` that lands between `accept()` returning and
`serving` being set.

`Listener.open` takes `tcp://ip:port?listen=1`. The host must be an IP
literal (`0.0.0.0`, `::`, `127.0.0.1`, ...); a host name, or a URL without
`?listen=1`, is `TcpException(URL)`, and a port already in use is
`TcpException(IO)`. `localPort()` reports the port bound, port 0 included,
and answers while `accept()` is parked on another thread.

Each `accept()` returns one connection as a `tcp.Transport`. A publisher
that connects while another is being served waits in the kernel's queue
until the next `accept()`; closing the listener drops whatever is still
queued, and such a connection is never handed out. `peerAddr()` is the
peer's `InetSocketAddress`, the TCP source address: behind a proxy, the
proxy's. An IPv4 client of a listener bound to `::` is reported as an
`Inet4Address`. The address is built from its bytes, with no name lookup.

A `Transport` becomes a shell with `intoReceiver()`, `intoDemuxReceiver()`,
`intoDemuxReceiver(DemuxerConfig)`, `intoSender()` or
`intoMuxSender(MuxerConfig)`. The conversion
consumes it: afterwards every call on the `Transport` but `close()` throws
`IllegalStateException`. `intoReceiver()`, `intoDemuxReceiver()` and
`intoSender()` consume it even when they throw; a `DemuxerConfig` the native
layer refuses, or a `MuxerConfig` the muxer refuses
(`MuxException(CONFIG_INVALID)`), leaves the `Transport` open, so a corrected
retry can convert it. The shell throws `TcpException` and reports no `localPort()`. An
accepted `Transport`, and a shell made from it, stay open after the listener
closes.

**A silent publisher.** A TCP receive has no deadline: a peer that connects
and sends nothing parks the receive until the shell's cancel handle fires or
the shell is closed. That is the only bound, so a server that must not
wait forever runs a watchdog that closes the shell (as `TcpIngest.close()`
does) when the stream stops progressing. `tsBytesDemuxed()` on the
`DemuxReceiver` answers while a receive is parked and makes a progress
counter for that watchdog.

**A peer that resets at once.** A peer can connect and reset the connection
before the listener takes it. On some platforms the accept itself, or setting
the socket options of the new connection, then fails, and `accept()` throws
`TcpException(IO)`; elsewhere `accept()` returns the connection and its
first receive fails. Either way only that connection is lost: the listener
stays open and the next `accept()` waits for the next peer, so a loop that
ends on any kind but `CLOSED` lets one stranger stop the server.

**Descriptors running out.** When the process has no file descriptor left,
`accept()` throws `TcpException(IO)` at once on every call until one is
freed. A loop that retries without a pause spins a core for that time; the
100 ms pause in `TcpIngest` bounds it.

**Stopping the listener.** `cancelHandle().cancel()`, from any thread, ends a
parked `accept()` with `TcpException(CLOSED)`, and every later `accept()`
throws the same until `close()`; it does not free the listener.
`close()` cancels first, then frees it; after `close()` every call throws
`IllegalStateException`.

**TLS.** `tcps://ip:port?listen=1&cert=<PEM file>&key=<PEM file>` serves
TLS; without `cert` and `key` the open is `TcpException(INVALID_CONFIG)`.
`accept()` runs no handshake: it runs on the shell's first receive or send,
and that call throws its failure. A client that does not speak TLS fails the
first receive with `TcpException(BROKEN)`. While a client has not answered
the handshake, the two directions differ: a receive parks until the shell's
cancel handle fires or the shell is closed, as above; a send throws
`TcpException(BACKPRESSURE)` after about 100 ms, and the sender stays
usable, so retry or close it.

### `socketStats()` on these transports

`socketStats()` returns the same `SocketStats` record on every shell. On
these three schemes it carries:

| Scheme | Bytes and packets | `packetsDroppedSend` / `packetsDroppedRecv` | Other fields |
|---|---|---|---|
| `udp://` | bytes; a packet is one datagram | socket send / receive errors | none |
| `tcp://`, `tcps://` | bytes; a packet is one `send` / `recv` call | send / receive errors | none |
| `rist://` | bytes and packets | `packetsDroppedRecv`: librist's lost-packet count plus oversize or malformed blocks the receiver dropped; `packetsDroppedSend` is 0 | `packetsRetransmitted` (a sender's count; a receiver reads 0), `linkBandwidthBps` (librist's estimate), `rttUs` |

Every field the table does not name reads 0. On a receiver the send-side
counters read 0, and on a sender the receive-side ones do.

- **librist reports once a second.** On `rist://` the lost count,
  `packetsRetransmitted`, `linkBandwidthBps` and `rttUs` lag behind the
  byte and packet counts, which the wrapper keeps itself.
  `linkBandwidthBps` can read 0 for a long time (it stayed 0 on a loopback
  link in testing), and librist measures the round trip in whole
  milliseconds, so `rttUs` reads 0 for a round trip under 1 ms; neither 0
  means there is no link.
- **Receive counters count what the transport read.** A receiver's
  `bytesReceived` and `packetsReceived` count whole reads (a datagram on
  `udp://`, a read on `tcp://`, a block on `rist://`, 1316 bytes from this
  library's senders), so they can run ahead of the packets `recvBytes()`
  has returned so far.
- **It waits behind a parked call.** `socketStats()` reads counters inside
  the shell, so it returns only when a receive or send parked on another
  thread does; see
  [What answers while another thread is parked](#what-answers-while-another-thread-is-parked).

## RTP transport (`org.tstrans.rtp`)

The `org.tstrans.rtp` package covers two RTP receive shapes:

- **MPEG-TS-over-RTP (RFC 2250, PT=33):** `org.tstrans.rtp.Transport` /
  `RecvTransport` send/receive pre-muxed MPEG-TS bytes over RTP/UDP (the raw
  transports; the TS shells over `rtp://` are `org.tstrans.shells.Sender` /
  `Receiver`). Each `send` produces one
  datagram (12-byte RTP header + TS payload); each `recv` returns one datagram's
  TS payload with the header stripped.
- **H.264-over-RTP (RFC 6184):** `H264Receiver` and `RtspClient.connectH264`
  ingest bare H.264 access units from an RTSP camera. See the dedicated section
  below.

RTP is default-on — no feature flag is needed.

### Transport hello

The simplest sender: bind to a destination and push TS bytes. Each call sends
one datagram.

<!-- compile: java-statements -->
```java
import org.tstrans.rtp.Transport;
import org.tstrans.RtpException;

// Send pre-muxed TS bytes over RTP-over-UDP:
try (Transport s = Transport.fromUrl("rtp://239.0.0.1:5004")) {
    s.send(tsBytes);  // one UDP datagram, framed with an RTP header
}
```

`send(byte[])` accepts a TS payload up to the configured packet size **minus the
12-byte RTP header** prepended to every datagram — i.e. up to
`Transport.DEFAULT_PKT_SIZE - 12` = 1304 bytes at the default `pkt_size` of 1316. A
payload that exceeds the cap throws `RtpException(TOO_LARGE)`.

### Receiver hello

The simplest receiver: bind to the same group/port and read one datagram per
`recv()` call. For multicast URLs the group is joined automatically.

<!-- compile: java-statements -->
```java
import org.tstrans.rtp.RecvTransport;
import org.tstrans.RtpException;

// Receive on the same group/port:
try (RecvTransport r = RecvTransport.fromUrl("rtp://239.0.0.1:5004")) {
    byte[] payload = r.recv(); // one datagram's TS payload (RTP header stripped)
    // process payload ...
}
```

`recv()` blocks until a datagram arrives or a cancel fires. Because `input.ts`
in the cross-binding scenario is 752 bytes (< the 1316 packet size), one
`send(input)` produces exactly one datagram and one `recv()` returns all 752
bytes — no accumulation needed for that case. For larger frames, call `recv()`
in a loop and concatenate.

### Cancellation

Obtain a `org.tstrans.shells.CancelHandle` from an open `Transport` or `RecvTransport` and call `cancel()`
from another thread to unblock a parked `send` / `recv` call:

<!-- compile: java-statements -->
```java
import org.tstrans.rtp.RecvTransport;

var rx = RecvTransport.fromUrl("rtp://127.0.0.1:5004");
var cancel = rx.cancelHandle();

// On another thread:
cancel.cancel();  // wakes rx.recv() → throws RtpException(CLOSED)
```

`CancelHandle` is safe to share across threads; `cancel()` and `close()` are
`synchronized`. The cancel takes effect at the next ~100 ms cancel-poll tick.

### RTP-specific gotchas

- **`Transport` / `RecvTransport` are not thread-safe.** Use one per thread. A
  cross-thread stop goes through `cancelHandle().cancel()`, which wakes a parked
  `send`/`recv` with `RtpException(CLOSED)` (detail `cancelled from another
  thread`).
- **One `org.tstrans.shells.SocketStats` for every transport.** The 16-field
  record is the same type the srt and rtp shells return. The RTCP-derived
  fields (`rttUs`, `packetsLost*`) stay zero until RTCP ingest is wired;
  `Transport` populates the send-side counters, `RecvTransport` the
  receive-side.
- **One `org.tstrans.shells.CancelHandle`** for every transport; it has
  `cancel()` and `isCancelled()`.
- **Closed handle → `IllegalStateException`.** Calling `send` / `recv` /
  `socketStats` / `cancelHandle` after `close()` throws `IllegalStateException`,
  the established JVM idiom (tst-py raises `RtpError` instead).
- **Negative `pktSize` / out-of-range `ssrc` throw `IllegalArgumentException`.**
  Validated at construction, mirroring tst-py's `u32`-typed API.

See [`/docs/languages/python.md`](/docs/languages/python.md) for the canonical
`tstrans.rtp` Python surface this binding mirrors.

### RTP convenience: MuxSender / DemuxReceiver

`org.tstrans.shells.MuxSender` over an `rtp://` URL bundles a `Muxer` + an RTP
transport — send encoded video/KLV/audio/subtitle/private-data and it muxes to
MPEG-TS and sends over RTP/UDP in one call: the per-kind `send*` shorthands
(including `sendData`), the handle-targeted `send*To` variants (including
`sendDataTo`), and the per-kind handle accessors (including `dataHandle()`).
`org.tstrans.shells.DemuxReceiver` bundles a `Demuxer` + an RTP recv transport
and iterates `DemuxEvent`s.

<!-- compile: java-statements -->
```java
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.shells.MuxSender;

MuxerConfig program = MuxerConfig.builder()
    .programNumber(1).pmtPid(0x1000)
    .addVideo(0x1011, VideoCodec.H264)
    .build();
try (MuxSender s = MuxSender.open("rtp://127.0.0.1:5004", program)) {
    s.sendVideo(annexBNal, /*pts*/ 0L, /*keyFrame*/ true);
}

try (DemuxReceiver rx = DemuxReceiver.open("rtp://0.0.0.0:5004")) {
    for (DemuxEvent e : rx) {
        if (e instanceof DemuxEvent.Video v) { /* ... */ }
    }
}
```

**Gotchas:**

- The datagram size is the `?pkt_size=` URL key (default 1316); a negative
  value is rejected with `RtpException(URL)`. The payload cap per push is
  `pkt_size − 12` (the RTP header is prepended by the transport).
- Every shell has `cancelHandle()` and `socketStats()`; `stats()` is the
  receive counters (`DemuxReceiverStats` on a `DemuxReceiver`, `ReceiverStats`
  on a `Receiver`), and an `rtp://` shell's `rtpStats()` counts the datagrams
  dropped as malformed and those its `source` key turned away, without
  waiting for a parked receive. To stop a `DemuxReceiver` iteration that is parked waiting for the
  next datagram, call `cancelHandle().cancel()` or `close()` from another
  thread; `close()` cancels the in-flight recv first, then frees the receiver
  (safe cross-thread).
- RTP/UDP is connectionless: a remote sender closing does **not** end a
  `DemuxReceiver` iteration. Break out of the loop on a sentinel event (or close
  the receiver) rather than waiting for end-of-stream.
- Sample payloads are heap `byte[]` copies.

`rx.lastSeen(pid)` returns when this receiver last emitted an item for
`pid` (an `Optional<Instant>`, to the microsecond), or empty if that PID has
never been seen — a per-stream staleness check that needs no `stats()`
snapshot-diffing. A watchdog thread can poll it directly:

```java
Duration staleAfter = Duration.ofSeconds(5);

void watchdog(DemuxReceiver rx, int pid, AtomicBoolean stop) {
    while (!stop.get()) {
        Optional<Instant> seen = rx.lastSeen(pid);
        if (seen.isPresent() && seen.get().plus(staleAfter).isBefore(Instant.now())) {
            System.out.println("pid " + Integer.toHexString(pid) + " has gone quiet");
        }
        try {
            Thread.sleep(1_000);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            return;
        }
    }
}
```

**Blocking caveat:** `lastSeen` takes the same internal resource lock a
parked `next()` call holds, so on a receiver whose consumer thread is
currently blocked in `next()` waiting on a fully-quiet stream, the watchdog's
call blocks too — indefinitely, until an item finally arrives (or the
consumer's own deadline, if any, expires). A separate watchdog thread is
therefore not truly independent of the consumer's recv cadence. Two ways to
keep it responsive: poll `lastSeen` from the *consumer* thread itself,
between `next()` calls, rather than from a separate thread; or pair it with
a receive deadline on the receiver URL (`?recv_timeout=<ms>` on `rtp://` /
`rtsp(s)://`, `?x-recvtimeout=<ms>` on `srt://`; see [Recv deadlines](#recv-deadlines-recv_timeout--per-call-overloads)
below) so `next()` returns — and releases the lock — on its own cadence even
when the stream is silent, letting a separate watchdog thread's poll go
through between those returns.

The same method exists on `org.tstrans.shells.DemuxReceiver` and
`org.tstrans.srt.ManagedDemuxReceiver` (same blocking caveat).

### Recv deadlines (`?recv_timeout=` / per-call overloads)

Both `RecvTransport.recv()` and `DemuxReceiver` iteration block indefinitely by
default — fine for a live camera, less fine for a quiet socket you want to
notice going quiet. `?recv_timeout=<ms>` on a `rtp://` (or `rtsp(s)://`) URL
arms a persistent receive deadline: a `recv()` / `recvEvent()` call that
would otherwise block forever instead throws `RtpException(BACKPRESSURE)` after
`<ms>` milliseconds of silence, and the receiver stays open — call again to
keep waiting:

<!-- compile: java-statements -->
```java
import org.tstrans.RtpException;
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.mpegts.DemuxEvent;

try (DemuxReceiver rx = DemuxReceiver.open("rtp://0.0.0.0:5004?recv_timeout=5000")) {
    while (true) {
        DemuxEvent event;
        try {
            event = rx.recvEvent();
        } catch (RtpException e) {
            if (e.kind() == RtpException.Kind.BACKPRESSURE) {
                System.out.println("quiet for 5s — still connected, just nothing to say");
                continue;
            }
            throw e;
        }
        if (event == null) {
            break;  // clean end of stream
        }
        // ... process event ...
    }
}
```

`Receiver.recv(Integer timeoutMs)` and `H264Receiver.recvAu(Integer timeoutMs)`
take a per-call override instead (or in addition — the explicit argument
always wins over a configured URL deadline for that one call; pass `null` to
fall back to the configured deadline, or block indefinitely if none is
configured). `RtspClientConfig`'s URL accepts the same `?recv_timeout=` key;
it carries through `intoDemuxReceiver()` / `intoH264Receiver()` automatically.
`BACKPRESSURE` is retryable — the transport and session are both still
alive, unlike `CLOSED` or `BROKEN`. On `srt://` the same deadline is
`?x-recvtimeout=<ms>` (a `ts-transformer` extension key, see
[SRT extension keys](/docs/guides/srt.md#ts-transformer-extension-keys-group-2));
the SRT parser rejects `?recv_timeout=` as an unknown key, and a silent SRT
receiver throws `SrtException(BACKPRESSURE)` per deadline. Use `recvEvent()`, not the `Iterator`
(`for (var event : rx)`), when you need to catch it as a checked exception —
the iterator wraps it in an unchecked `RuntimeException`.

> **0.7.0 kind changes.** The rtp deadline was `RtpException(TIMEOUT)`, a
> cancel was `CANCELLED`, an oversized payload `MALFORMED_PACKET`, and every
> other transport or connect failure the catch-all `TRANSPORT`. All four
> members are gone: a deadline is `BACKPRESSURE`, a cancel or close is
> `CLOSED`, an oversized payload is `TOO_LARGE`, a dead transport is `BROKEN`,
> and each `ConnectError` has its own member (`URL`,
> `PAYLOAD_TYPE_PARAM`, `MISSING_PAYLOAD_TYPE_PARAM`, `HOST_NOT_LITERAL`,
> `IO`, `IFACE_UNSUPPORTED`). On the srt side `WOULD_BLOCK` became
> `BACKPRESSURE`, a TS-sync loss on `sendBytes` became `INPUT_MALFORMED`, an
> oversized payload `TOO_LARGE`, and a receive that reaches end of stream now
> raises the new `SrtException.Kind.END_OF_STREAM` where it used to say
> `CLOSED`. Every kind name is now the shared `BindingErrorKind` member, and
> `NativeLoader.load()` verifies at load time that the JAR's enums carry every
> kind the native library can raise.

An `rtp://` receive URL also takes `?rcvbuf=` (kernel receive buffer, e.g. `8M` for high-bitrate streams) and `?source=<ip>` (accept datagrams from one sending host only); see [Performance and reliability](/docs/troubleshooting.md#performance-and-reliability).

## RTSP client (`org.tstrans.rtp`)

Connect to an RTSP server, drive OPTIONS/DESCRIBE/SETUP/PLAY, and demux the RTP
data plane. Mirrors tst-py's `tstrans.rtp` RTSP-client surface.

<!-- compile: java-statements -->
```java
import org.tstrans.rtp.*;
import org.tstrans.shells.DemuxReceiver;
import org.tstrans.RtspException;

var cfg = RtspClientConfig.builder("rtsp://cam.local:554/stream1")
    .auth(new DigestAuth("admin", "secret"))   // BasicAuth | DigestAuth | (none)
    .transportPref(TransportPref.AUTO)         // UDP-first, TCP fallback on 461
    .build();

try (RtspSession session = RtspClient.connect(cfg);
     DemuxReceiver rx = session.intoDemuxReceiver()) {
    for (var event : rx) {
        // handle DemuxEvent.Video / .Metadata / ...
    }
}
```

- **Auth secrecy.** `BasicAuth`/`DigestAuth` hold the password but expose no public
  `password` accessor; `toString()` redacts it. URL secrets and stream-ID query values
  are redacted in every `toString()` and exception message. The redaction is a
  heuristic with one documented gap: a secret holding a quote followed by
  whitespace or `, ; . : ) ] }` can be read as the end of a quoted URL, leaving
  the rest of it visible (see [Secrets in string forms](/docs/reference/binding-authors.md#secrets-in-string-forms)).
  The credential is handed to the native connect and wrapped in Rust `secrecy::SecretString`.
- **URL credentials.** Without `auth` the URL's userinfo answers a challenge;
  a user name without a password (`rtsp://admin@cam/…`) answers with an empty
  password, as `rtsp://admin:@cam/…` does. `auth` overrides it. An empty user
  name (`new BasicAuth("", pw)`, `new DigestAuth("", pw)` or `rtsp://:pw@cam/…`)
  is no credentials: a challenge then throws an `RtspException` of kind
  `AUTH_FAILED`.
- **`auth` is `Object`.** Java has no union type; `RtspClientConfig.auth()` returns
  `Optional<Object>` that is a `BasicAuth` or `DigestAuth` — match with `instanceof`.
- **`DigestAuth` never falls back to Basic.** A server that offers no Digest
  challenge fails the connect with an auth-unsupported `RtspException` rather
  than receiving the password in Basic form. `BasicAuth` answers Digest when it
  is offered and Basic otherwise.
- **Threads and cancellation.** `session.cancelHandle()` returns a
  `RtspCancelHandle` at once from any thread, and `close()` may be called
  from any thread: it cancels a control call in progress there and closes
  once that call has let go. The control calls (`pause()`, `play()`,
  `teardown()`) run one at a time: a second one waits for the call in
  progress, up to that call's request timeout (10 s). Flip `cancel()` from another thread to break a control call
  in progress out; it throws `RtspException(CLOSED)`. A `close()` that
  overlaps a control call in progress on another thread cancels it the same
  way, so the session then ends as a cancel, not as a clean teardown: a live
  receiver ends `RtpException(CLOSED)`. A `pause()` / `play()` that reached the
  session before a `close()` began and loses it to that close throws
  `CLOSED`; a `teardown()` in that place returns normally (the session is torn
  down) unless the close cancelled it; a call that starts after the close
  began throws `IllegalStateException`. A `close()` made while another
  `close()` is running returns at once without waiting for it.
- **`teardown()` reports what the server answered.** A 2xx reply, or
  `454 Session Not Found` (the session is already gone), returns normally. A
  TEARDOWN the server refuses throws `RtspException` of the status's kind:
  `PROTOCOL`, or `NOT_FOUND` for a 404. A first 401 is answered once with
  the credentials, unless it carries no `WWW-Authenticate` header
  (`PROTOCOL`), the client has no credentials (`AUTH_FAILED`) or its
  challenge offers no scheme the client may answer (`AUTH_REQUIRED`); a
  401 to that retry throws `AUTH_FAILED`. The session counts as torn down either way, so
  a second `teardown()` is a no-op.
- **`teardown()` or `close()`.** `teardown()` waits for the reply up to the
  request timeout (10 s), reports it, and keeps the session object for
  `close()`. `close()` (and so try-with-resources) is silent and bounded: it
  sends TEARDOWN with at most 500 ms for the reply (none after `teardown()`),
  then waits up to 2 s for the client's thread, which closes the control
  connection; a refused or unanswered TEARDOWN is not reported. A caller who
  needs a slow server's reply awaited calls `teardown()` first.
  `cancelHandle()` returns a handle after `teardown()`; after `close()` it
  throws `IllegalStateException`, and handles taken before read cancelled.
- **A panic poisons the session.** After a panic inside an earlier call (a
  bug in the library), `pause()`, `play()`, `teardown()`,
  `intoDemuxReceiver()` and `intoH264Receiver()` throw
  `RtspException(CLOSED)`; `close()` still ends the session.
- **TLS (`rtsps://`) is supported.** The binding links rustls; an `rtsps://`
  URL negotiates TLS on connect. `tlsRootCertsPem` supplies a PEM bundle of
  custom trust anchors (private-CA cameras) — without it the handshake
  verifies against platform native roots. Invalid PEM, a rejected anchor, or
  an empty bundle throw `RtspException` of kind `TLS` before any network I/O.
- **`transportPref` and `rtspVersion` apply.** A non-default value overrides the URL's
  `?transport=udp|tcp` / `?rtsp_version=` query (`AUTO` and `V1_0` leave the URL in
  charge), as in tst-py and the C builder. `DigestAuth.algorithm` is
  retained for introspection only; the server's
  `WWW-Authenticate` challenge selects the actual digest algorithm.
- **`stats()`** returns a zeroed `RtspStats` for now (RTCP counters wire in later).
- **Why did the stream end?** When a `for (var event : rx)` loop exits or a
  call throws, `rx.endReason()` (on `Receiver`, `DemuxReceiver`, and
  `H264Receiver` alike) answers with an `org.tstrans.shells.EndReason` member —
  `CLEAN_TEARDOWN`, `SESSION_EXPIRED`, `KEEPALIVE_FAILED`,
  `TRANSPORT_FAILED`, `PROTOCOL_ERROR`, or `CANCELLED` in an `Optional` —
  empty if the session hasn't ended yet (or ended through a path that records no
  reason, e.g. a plain `rtp://` receiver with no owning `RtspClient`).
  `rx.endDetail()` carries the free-text message for the three failure
  variants (empty for the others). Both stay readable after `close()` — the receiver snapshots
  them at close time, before the underlying native handle is freed.
  Neither ever waits behind a receive parked on another thread; on
  `H264Receiver` they answer empty while a `recvAu()` is in flight. Set
  `TSTRANS_LOG=tst_rtp=debug` before the first `System.load` of the
  native library (i.e. before touching any `org.tstrans.*` class) to
  also see the RTSP client's `tracing` events (targets
  `tst_rtp::client::*`) on stderr.
  Full recipe + the Rust-side rationale:
  [Why did my RTSP stream end?](/docs/troubleshooting.md#why-did-my-rtsp-stream-end).

## RTSP server (`org.tstrans.rtp`)

Host an RTSP server, register unicast or multicast mounts, and push elementary
streams to all connected clients via the `MountHandle` push family. Mirrors
tst-py's `tstrans.rtp.RtspServer`.

<!-- compile: java-statements -->
```java
import org.tstrans.rtp.*;
import org.tstrans.RtspException;
import org.tstrans.mpegts.KlvStreamType;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

MuxerConfig program = MuxerConfig.builder()
    .programNumber(1).pmtPid(0x1000)
    .addVideo(0x1011, VideoCodec.H264)
    .addKlv(0x1012, KlvStreamType.SYNCHRONOUS_METADATA, true)
    .build();

var cfg = RtspServerConfig.of("0.0.0.0:8554");   // defaults: maxSessions=100, etc.

try (RtspServer server = RtspServer.start(cfg);
     MountHandle mount = server.addUnicastMount("/live", program)) {

    // Push frames to all connected RTSP clients.
    // pts is a 90 kHz tick count; keyFrame marks a random-access point.
    mount.pushVideo(annexBNal, /*pts=*/ 0L, /*keyFrame=*/ true);
    mount.pushKlv(klvBytes, /*pts=*/ 0L, /*metadataServiceId=*/ 1);

    // server.localAddr() returns the bound address, an Optional<InetSocketAddress>
    // (getPort() is the kernel's port when port 0 is used).
}
// close() performs a graceful Notice 5402 teardown of active sessions (it takes
// as long as stop(), the drain included), then frees the native server.
// MountHandle.close() frees only the handle wrapper — the mount continues to
// serve until the server itself is closed.
```

- **`RtspServerConfig.of(bindAddr)`** is the one-liner constructor (all other
  fields take tst-py defaults: `maxSessions=100`, `sessionTimeoutSecs=60`,
  `fanoutCapacity=256`, `gracefulShutdownDrainMs=2000`,
  `acceptUnregisteredPublishers=false`). Use `RtspServerConfig.builder()` to
  tune individual fields: `bindAddr`, `auth`, `maxSessions`,
  `sessionTimeoutSecs`, `fanoutCapacity`, `gracefulShutdownDrainMs`,
  `tlsCert`, `tlsKey`, `acceptUnregisteredPublishers`,
  `endPublishMountOnPublisherExit`, `publishIdleTimeoutMs`.
- **Mount errors are `RtspException`.** All `pushVideo`/`pushKlv`/
  `pushAudio`/`pushSubtitle`/`pushData` calls on `MountHandle` throw `RtspException` of kind
  `MOUNT` when the muxer refuses the frame (e.g. a stream the config does not
  have), and of kind `CLOSED` once the server ends (`stop()` or `close()`,
  from the moment it begins, or its hard cancel), whether or not the mount
  was removed first; `flush()` and `resetStats()` are not refused after it.
  This differs from `MuxSender`, which throws `MuxException`.
- **`MountHandle` carries private data too.** `pushData` / `pushDataTo`
  have the same pass-through and PTS semantics as `Muxer.pushData`.
- **`MountHandle` is `Arc`-backed and thread-safe** on the push path (`&self`
  internally). Multiple producer threads may call `push*` concurrently. Do not
  race `close()` against a concurrent push — coordinate closes at the producer
  boundary.
- **`MountHandle.close()` unregisters only the handle wrapper.** The mount
  itself stays live in the server (still accepts new connections and fans out to
  existing ones) until `RtspServer.removeMount(path)`, `RtspServer.close()` or
  `stop()`. `removeMount` sends its sessions the Notice 5402 teardown and frees
  the path; the `MountHandle` keeps accepting pushes that reach nobody until the
  server ends.
- **Hard-cancel.** `server.cancelHandle()` returns a cross-thread
  `RtspServerCancelHandle`; `cancel()` ends the server without the Notice
  5402 teardown and without the drain. From that moment mount pushes, mount
  registration, `removeMount`, `MountRules` edits and a new `nextPublisher`
  throw `RtspException(CLOSED)`; within about 100 ms the server notices the
  cancel and ends what runs: a parked `nextPublisher` throws `CLOSED`,
  every session's connection is closed and every publish mount's receiver
  reads end of stream. A `stop()` or `close()` after it is not graceful
  either. A cancel fired while a `stop()` or `close()` runs does not shorten
  that call (its Notice writes, its drain wait and its connection shutdowns
  do not read the cancel), and `cancelHandle()` throws `IllegalStateException` once
  `close()` has begun: for an end without the Notice, take the handle and
  cancel before `close()`. A second `close()` made while the first runs
  returns at once, before the first has finished.
- **Auth.** Pass `new BasicAuth("user", "pass", "realm")` or
  `new DigestAuth("user", "pass", DigestAlgorithm.SHA256, "realm")` to
  `RtspServerConfig.builder().auth(...)`. The realm is required for server-side
  auth; omitting it throws `IllegalArgumentException` at `start()`.
- **TLS (`rtsps://`) is supported.** Bind with an explicit `rtsps://` address
  and set `tlsCert`/`tlsKey` on the config — PEM certificate-chain and
  private-key **file paths** (both or neither; `build()` enforces). The
  native server loads and validates them synchronously inside `start()`:
  a missing or malformed file throws `RtspException` of kind `TLS` from
  `RtspServer.start(config)` — never a server that looks started but can't
  complete a handshake.

## RTSP publisher ingest (`org.tstrans.rtp`)

The server's publisher role lets an encoder push into it with ANNOUNCE /
SETUP `mode=record` / RECORD. Each pushed stream reaches the application as
MPEG-TS through an ordinary `org.tstrans.shells.DemuxReceiver`, and the same mount
keeps serving PLAY readers from the published bytes. A publish mount accepts
one publisher at a time, in one of two shapes (`PublishShape`):

- `MP2T`: one MPEG-TS-over-RTP track. The bytes pass through.
- `ELEMENTARY`: H.264 with an optional KLV track. The server re-muxes them
  into one program (video PID 0x100, KLV PID 0x101).

<!-- compile: java-statements -->
```java
import org.tstrans.mpegts.DemuxEvent;
import org.tstrans.rtp.*;
import org.tstrans.shells.DemuxReceiver;

var cfg = RtspServerConfig.of("0.0.0.0:8554");
try (RtspServer server = RtspServer.start(cfg);
     PublishMount mount = server.addPublishMount("/cam1");
     DemuxReceiver rx = mount.intoDemuxReceiver()) {          // take-once
    // Quiet until a publisher records. recvEvent() returns null (end of
    // stream) once another thread calls server.removeMount("/cam1"),
    // server.stop() or server.close().
    DemuxEvent ev;
    while ((ev = rx.recvEvent()) != null) {
        if (ev instanceof DemuxEvent.Video v) {
            // ...
        }
    }
}
```

The surface:

- `RtspServer.addPublishMount(path) -> PublishMount` registers a path up
  front. An ANNOUNCE to any other path answers `404`.
  `addPublishMount(path, PublishMountConfig.builder().endOnPublisherExit(true).build())`
  ends that mount's transport when a publisher that reached RECORD leaves;
  remove the mount, then add it again, to accept a new publisher. An option
  left unset inherits the builder's `endPublishMountOnPublisherExit` (false
  by default), and an explicit `false` keeps the mount open even when the
  server default is true. A closed on-demand mount stays in the table, and
  counts toward the on-demand cap, until removed.
- `RtspServerConfig.builder().publishIdleTimeoutMs(n)` ends a recording
  publisher's session after n ms without a valid RTP packet (a decodable
  header with the announced payload type), timed from RECORD; RTSP keepalives
  and RTCP do not reset it. Zero leaves it off (the default); a negative
  value makes `build()` throw `IllegalArgumentException`.
- `RtspServerConfig.builder().acceptUnregisteredPublishers(true)` lets an
  ANNOUNCE to an unregistered path create a publish mount on demand, and
  `RtspServer.nextPublisher(timeoutMs) -> Optional<PublishMount>` hands each
  one out, in ANNOUNCE order, to exactly one caller. Up to 64 handles wait in
  that queue and the server holds up to 256 on-demand mounts; an ANNOUNCE
  past either bound answers `503`. Anyone who can reach the port can create
  mounts, so set `auth` where that matters.
- `RtspServer.mountRules() -> MountRules` is a per-path table, editable while
  the server runs: `set(path, rule)`, `remove(path)`, `setDefault(rule)`,
  `clear()`, `setChallenge(scheme, realm)`, `size()`, `isClosed()` and
  `close()`. `MountRule.of(publish, read)` takes a `MountPolicy`
  (`inherit()`, `open()`, `require(MountCredential...)` or
  `refuse(RefuseStatus.NOT_FOUND)`); `RtspServerConfig.builder().defaultMountRule(...)`
  guards the first ANNOUNCE. A path rule's `Inherit` side uses the table's default rule; the default's `Inherit` uses the server-wide builder setting (`auth_*`). A bad path throws
  `RtspException` of kind `MOUNT`; `MountPolicy.require()` with no credentials
  throws `IllegalArgumentException` before the server is consulted; a
  credential with an empty username is refused by the server with kind
  `MOUNT`. From the moment `stop()` or `close()` begins, and after the hard
  cancel, an edit throws kind `CLOSED`; `size()` still answers. A server nobody edits behaves as before; see the
  [publish-ingest recipe](/docs/cookbook/receiving/rtsp-publish-ingest.md#per-name-credentials-and-refusing-a-publisher)
  for when a change takes effect.
- `RtspServer.removeMount(path)` removes a mount of any kind. A live
  publisher gets the RTSP Notice 5402 and is disconnected. On-demand mounts
  stay registered after their publisher leaves until this removes them.
- `PublishMount` methods: `mountPath()`, `peerCount()` (PLAY readers),
  `generation()` (publishers that have ended on the mount), `publisher() ->
  Optional<PublisherInfo>`, `stats() -> PublishMountStats`, `cancel()`,
  `intoDemuxReceiver()` / `intoDemuxReceiver(DemuxerConfig)`. The object stays
  usable after the transport is taken, after `removeMount` and after the
  server stops. `close()` frees only the wrapper; the mount stays registered.
- `PublisherInfo` record: `peer` (an `InetSocketAddress`), `shape`, `klv`,
  `since` (an `Instant`), `generation`.
- `PublishMountStats` record: every counter of the Rust snapshot in the same
  order, including `sourceRejected`, `ausReordered`, `ssrcChanges`, and
  `alignment` (a `ClockAlignment`: `NOT_APPLICABLE`, `PENDING`, `PROVISIONAL`
  or `SENDER_REPORT`).
- `ServerStats` adds `activePublishers`, `totalRtpPacketsReceived` and
  `totalRtpBytesReceived`.

How each call ends:

| Call | Outcome | Meaning |
|---|---|---|
| `nextPublisher(timeoutMs)` | `Optional.empty()` | No mount arrived in time. Always the result when on-demand publishers are off. |
| `nextPublisher`, `addPublishMount`, `removeMount` | `RtspException(CLOSED)` | The server is stopped (from the moment `stop()` or `close()` begins) or hard-cancelled. A `nextPublisher` parked on another thread wakes with it when `stop()` or `close()` runs. After `close()` the object itself refuses calls with `IllegalStateException`. |
| `addPublishMount`, `removeMount` | `RtspException(MOUNT)` | Duplicate or invalid path, or no mount registered at the path. |
| `intoDemuxReceiver` | `RtspException(CLOSED)` | The mount's transport was already taken, through this or any other handle to the mount. |
| `DemuxReceiver.recvEvent()` | `null` | End of stream: the mount was removed, the server stopped or was hard-cancelled, or its recording publisher left with end-on-exit on. What was already queued is delivered first. `endReason()` reads `CLEAN_TEARDOWN`. |
| `DemuxReceiver.recvEvent()` | `RtpException(CLOSED)` | The application cancelled it: `PublishMount.cancel()` or `DemuxReceiver.close()`. `endReason()` reads `CANCELLED`. |

Rules the types do not enforce:

- **Receiver across publishers.** With end-on-exit off (the default), the
  receiver outlives publisher churn: between publishers it stays open and
  silent, and the next publisher's bytes arrive as ordinary continuity
  discontinuities.
- **Every shutdown ends the mounts.** `stop()`, `close()` and
  `cancelHandle().cancel()` end every publish mount's receiver and wake a
  parked `nextPublisher` with `CLOSED`; only the hard cancel skips the
  Notice 5402 teardown and the drain (and a `stop()` or `close()` after it
  skips them too).
- **`nextPublisher` blocking.** The wait holds no lock on the `RtspServer`
  object, so `stats()`, `close()` and the other methods answer from other
  threads while it is parked. `timeoutMs` must be `>= 0` (`0` polls).
  Concurrent callers are served one at a time, so a call made while another
  waits can return later than its own `timeoutMs`.
- **Expired handles.** `nextPublisher` can return a mount that `removeMount`
  removed while its handle waited in the queue. Its receiver reads end of
  stream at once.

ffmpeg pushes elementary tracks and cannot push KLV over RTSP; GStreamer's
`rtspclientsink` can push MPEG-TS or H.264 + KLV. The
[publisher ingest recipe](/docs/cookbook/receiving/rtsp-publish-ingest.md)
covers the same flow from Rust.

## RTSP push client (`org.tstrans.rtp`)

`RtspClient.push(config, description)` publishes to an RTSP server: it
connects, sends ANNOUNCE, SETUP `mode=record` for each track and RECORD,
and returns an `RtspPushSession` that sends the media interleaved on the
control connection. The server may be mediamtx, this library's
`RtspServer` (the [publisher ingest](#rtsp-publisher-ingest-orgtstransrtp)
above) or any server that takes RTSP 1.0 RECORD over TCP.

<!-- compile: java-statements -->
```java
import org.tstrans.rtp.*;

var cfg = RtspClientConfig.builder("rtsp://media.example:8554/cam1")
    .auth(new BasicAuth("publisher", "secret"))
    .build();
var desc = PushDescription.h264FromKeyFrame(firstKeyFrame, true);   // klv = true
try (RtspPushSession push = RtspClient.push(cfg, desc)) {
    for (int i = 0; i < accessUnits.size(); i++) {
        long pts = i * 3000L;
        push.sendVideo(accessUnits.get(i), pts);   // Annex B, decoding order
        push.sendKlv(klvUnits.get(i), pts);        // one whole KLV unit
    }
}   // close(): writes what is queued, then TEARDOWN
```

The surface:

- `PushDescription.mp2t()`, `h264(List<byte[]> parameterSets, boolean klv)`,
  `h264FromKeyFrame(byte[] annexB, boolean klv)`, `h265(vps, sps, pps,
  klv)`, `h265FromKeyFrame(annexB, klv)`. The `*FromKeyFrame` factories
  throw `IllegalArgumentException` when the access unit lacks a parameter
  set; a description without one is refused by `push` with
  `RtspException(PROTOCOL)` before the ANNOUNCE is written. Every factory
  copies its arrays.
- `RtspPushSession.sendTs(byte[])` (whole TS packets, at most 1 316 bytes,
  on an MPEG-TS session), `sendVideo(byte[] annexB, long pts)`,
  `sendKlv(byte[] klv, long pts)`, with `pts` in 90 kHz ticks.
- `pause()`, `resume()`, `teardown()`, `close()`, `cancelHandle()`,
  `stats() -> RtspPushStats`, `endReason() -> Optional<EndReason>`,
  `endDetail() -> Optional<String>`, `isSessionAlive()`, `sessionId()`.
- `intoMuxSender(MuxerConfig)` and `intoSender()` move an MPEG-TS session
  into a `org.tstrans.shells.MuxSender` or `Sender`; this session is closed
  afterwards. A session announced with a video track throws
  `IllegalArgumentException`, and `intoMuxSender` throws
  `MuxException(CONFIG_INVALID)` for a refused config, each leaving the
  session open. Neither waits for a send, `pause()` or `resume()` running on
  another thread: they throw `IllegalStateException`, consume nothing and
  leave the session usable.
- `RtspPushStats` record: `packetsSent`, `bytesSent`, `senderReportsSent`,
  `receiverReports`, `fractionLostQ8`, `cumulativeLost`,
  `framesDroppedInbound`, `framesDroppedAtClose` (media a close
  discarded; `teardown()` is the call that reports such a loss).
  `stats()`, `endReason()` and `endDetail()` never wait and never throw:
  they answer while a send is waiting, after `close()` and `teardown()`
  with the values the session ended with, and after a conversion they
  follow the push the sender owns until this session's `close()`: close the
  converted session object too (a try-with-resources around it does).

How each call ends:

| Call | Outcome | Meaning |
|---|---|---|
| `RtspClient.push` | `RtspException` | A step failed; the connection is closed. The kind, in the order a push meets them: `UNSUPPORTED_TRANSPORT` for `TransportPref.UDP` or a URL carrying `?transport=udp` (refused before any SETUP is written); `PROTOCOL` for `RtspVersion.V2_0` (no ANNOUNCE written), for a URL that does not parse and for a description that lacks a parameter set; `IO`, `TLS` and `TIMEOUT` for the connection, its TLS handshake and a request left unanswered; for a `401`, which the client answers once with the configured credential: `PROTOCOL` when the `401` carries no `WWW-Authenticate` header, `AUTH_FAILED` when no credential is configured or the retry is answered `401` again (the credential was refused), and `AUTH_REQUIRED` when the challenge offers no scheme the credential may answer (a Basic-only challenge under `DigestAuth`); `NOT_FOUND` for a `404` (no such path) and `PROTOCOL` for any other refused status (a server that does not take the codec answers `415`); for a SETUP reply the client cannot use, `PROTOCOL` (no `Transport` header, a transport that is not interleaved, no session, another session, or a channel pair it cannot take) and `UNSUPPORTED_TRANSPORT` (a `Transport` header whose port or channel pair does not parse). |
| a data call | `RtpException(CLOSED)` | The cancel handle fired; also a call that was waiting for the session when a `close()`, `teardown()` or conversion on another thread took it (it sends nothing). |
| a data call | `RtpException(BROKEN)` | The server closed the connection, the connection failed or this machine's kernel took no byte of it for 5 s, a keepalive ping was answered `454`, or the server broke the framing. Also a TS payload that is empty or not whole packets, which leaves the session open. |
| `sendTs` | `RtpException(TOO_LARGE)` | More than 1 316 bytes; nothing sent. |
| a data call | `IllegalArgumentException` | A track the session did not announce, or an empty access unit or KLV unit; nothing sent. |
| any call but `isSessionAlive()`, `stats()`, `endReason()`, `endDetail()` | `IllegalStateException` | The session is closed. Also `intoMuxSender` / `intoSender` while a send, `pause()` or `resume()` runs on another thread (nothing consumed). |
| a data, control or conversion call | `RtspException(CLOSED)` | A panic inside an earlier call (a bug in the library) poisoned the session; `close()` and `teardown()` still end it, and `isSessionAlive()` reads `false`. |
| `teardown()` | `RtspException` | The first of these that applies: `CLOSED` on a cancelled session; `PROTOCOL` with status `454` when the server had forgotten the session and queued media was dropped, whatever the TEARDOWN is answered; for the TEARDOWN itself, `PROTOCOL` (or `NOT_FOUND` for a 404) when the server refused it, `TIMEOUT` when no reply came within the 500 ms and `IO` when the connection had ended. `CLOSED` also when a conversion on another thread took the session first: nothing was torn down, the converted shell owns the push, and `stats()` / `endReason()` keep following it. A normal return means the queued media was written and the server accepted the TEARDOWN. The session is closed on every outcome. |
| `close()`, try-with-resources | returns | Whatever the server answered. |

Rules the types do not enforce:

- **A send waits only for the queue.** It returns once its packets are
  queued for the client's thread. Against a server that reads slowly it
  waits until room frees, the cancel handle fires (within about 100 ms),
  the session ends, or this machine's kernel has taken no byte of the
  connection for 5 s. `pause()`, `teardown()` and `close()` first wait
  for the queue the same way.
  Keepalive pings go out one at a time, so what waits on such a server's
  connection stays bounded.
- **One call at a time.** A call made while another is in progress on
  another thread waits for it. `cancelHandle()`, `isSessionAlive()`,
  `sessionId()`, `stats()`, `endReason()` and `endDetail()` answer at
  once; `cancelHandle()` is the way to end a
  waiting send. `isSessionAlive()` never throws: a call in progress counts
  as live, and it reads `false` once the session is closed, torn down or
  converted. `close()` and `teardown()` fire the cancel first only when a
  send, `pause()` or `resume()` is running on another thread: that call
  throws `CLOSED`, what is queued is dropped, and the TEARDOWN is written
  without the wait. A send, `pause()` or `resume()` only waiting for the
  session is not cancelled: it runs nothing and throws `CLOSED`. Each
  `cancelHandle()` is a new handle to close when done.
- **Ending a push through a shell.** `MuxSender.finish()` cancels nothing:
  it writes what the sender holds, then, while the session is up, the
  session writes its queue and sends TEARDOWN; the handle still needs its
  `close()` afterwards (try-with-resources does it). `finish()` cannot tell
  you whether the queue was written: a session that ended first drops it
  and `finish()` still returns normally. `Sender.finish()` after
  `intoSender()` does the same for pre-muxed bytes. `close()` alone cancels
  first and can drop media still queued, on either shell. When you
  must know that everything was written, send through `sendTs` and call
  `teardown()`, which throws unless the queued media was written and the
  server accepted the TEARDOWN.
- **Keepalive pings.** `RtspClientConfig.builder(url).keepalive(...)`
  (on by default) sends pings at half the session timeout the server
  advertises, or every `keepaliveIntervalMs(ms)` when that is set. mediamtx
  1.21.4 ends a recording publisher after 10 s without a byte while it
  advertises 55 s, so keep media flowing or set an interval below 10 s. The
  same builder sets `connectTimeoutMs(ms)` (TCP connect plus TLS handshake;
  expiry `RtspException(IO)`), `requestTimeoutMs(ms)` (each request's reply;
  expiry `TIMEOUT`) and `userAgent(text)` (one holding a CR or LF is refused
  with `PROTOCOL` before anything is written); unset, each keeps the client's
  default (10 s, 10 s, `tst-rtp/0.1`). `build()` refuses a value of 0 or
  below, and an interval with `keepalive(false)`, with
  `IllegalArgumentException`. They apply to `connect`, `connectH264` and
  `push` alike.
- **After `pause()` the data calls still send**, and some servers end the
  session for that (mediamtx 1.21.4 closes the connection). `resume()` does
  not wait for queued media.

The [push recipe](/docs/cookbook/sending/rtsp-push.md) has the MPEG-TS
form through `intoMuxSender`, the Rust and Python twins, and what mediamtx
and ffmpeg do with a push.

## H.264-over-RTP ingest (RFC 6184) (`org.tstrans.rtp`)

`org.tstrans.rtp.H264Receiver` ingests bare H.264 elementary streams over
RTP/RTSP (RFC 6184 — single-NALU, STAP-A, FU-A; packetization modes 0 and 1).
The canonical path for an RTSP camera uses `RtspClient.connectH264`:

<!-- compile: java-statements -->
```java
import org.tstrans.rtp.*;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;
import org.tstrans.RtspException;

var cfg = RtspClientConfig.builder("rtsp://cam.local/h264")
    .auth(new DigestAuth("admin", "secret"))
    .build();

try (RtspSession session = RtspClient.connectH264(cfg)) {
    // intoH264Receiver() CONSUMES the session handle — session is closed on
    // return (success or failure). pause()/play() are unavailable afterward.
    // See the "JVM divergence" note below.
    try (H264Receiver rx = session.intoH264Receiver()) {
        var muxCfg = MuxerConfig.builder()
            .programNumber(1).pmtPid(0x1000)
            .addVideo(0x1011, VideoCodec.H264)
            .build();
        try (var mux = new org.tstrans.mpegts.Muxer(muxCfg)) {
            byte[] drain = new byte[1316];
            H264AccessUnit au;
            while ((au = rx.recvAu()) != null) {
                // au.pts() is a 90 kHz presentation timestamp; AUs arrive in decode order.
                // MPEG-TS PTS uses the same clock, so no rescaling is needed.
                mux.pushVideo(au.annexb(), au.pts(), au.keyFrame());
                int n;
                while ((n = mux.pull(drain)) > 0) {
                    // write drain[0..n] to file / SRT / etc.
                }
            }
        }
    }
}
```

### Key types

| Type | Notes |
|---|---|
| `H264Receiver` | `listen(String url)` / `listen(String url, H264DepayConfig cfg)` for direct UDP. `recvAu()` → `H264AccessUnit \| null`; `null` at end of stream (the server ended the session; a failed control connection also ends it with `null`, and `endReason()` names it); after a cancel it throws `RtpException(CLOSED)` once the access units it holds are handed out. A `close()` on another thread never makes it return `null`: a call that reaches the receiver after the `close()` began throws `IllegalStateException`, one that reached it first but loses it to the `close()` before receiving throws `RtpException(CLOSED)`. `implements AutoCloseable, Iterable<H264AccessUnit>`. |
| `H264AccessUnit` | `annexb(): byte[]`, `pts(): long` (90 kHz ticks, i64), `keyFrame(): boolean`, `rtpTimestamp(): long`. |
| `H264DepayConfig` | Immutable; build with `H264DepayConfig.builder()`. Defaults: `payloadType=96`, `parameterSetInjection=BEFORE_IDR`, `initialParameterSets=[]`, `maxAuBytes=8388608`. |
| `ParameterSetInjection` | `NONE` — pass through as received; `BEFORE_IDR` (default) — prepend cached SPS/PPS before each IDR. |
| `H264DepayStats` | `ausEmitted()`, `ausDropped()`, `seqGaps()`, `parameterSetUpdates()`, … (9 counters). |
| `RtpStats` | `malformedPackets()`. |

### JVM divergence from Python

**`intoH264Receiver()` consumes the session.** Unlike the Python binding
(where `session.into_h264_receiver()` leaves the session wrapper usable for
`pause()`/`play()`), the JVM `intoH264Receiver()` zeroes the session handle
via `consumeHandle()` before the fallible native call (NativeHandle contract
item 3 — double-free safety). On return — success **or** failure — this
`RtspSession` wrapper is closed: subsequent `pause()` / `play()` / `teardown()`
calls throw `IllegalStateException`. The `H264Receiver` takes over the full
session (control connection, keepalives, and its close on `close()`, as
`RtspSession.close()` does it; `endReason()` then reads `CLEAN_TEARDOWN` only
when the server accepted the TEARDOWN within the close's 500 ms and no
`recvAu()` was in progress, otherwise `CANCELLED`). By contrast,
`intoDemuxReceiver()` leases the session handle (a sanctioned borrow — only the
data-plane transport moves into the receiver), so the control plane remains
usable after the call. That `DemuxReceiver` does not own the RTSP client:
closing it does not end the session, and only the `RtspSession`'s own
`close()` does. The JVM has no cleaner, so a session object dropped without
`close()` is never closed: `RtspClient.connect(cfg).intoDemuxReceiver()` with
the session not kept leaves the RTSP session, its connection and its
keepalives running until the process exits, with nothing left to close it.
Keep the session and close it (the example above closes both). Python
differs: a receiver keeps the client alive after the session object is
collected, and the client ends once the session and the receiver are both
gone.

**Failure path note.** If `intoDemuxReceiver()` previously consumed the data
plane, a subsequent `intoH264Receiver()` still consumes the session (the native
throws `RtspException(CLOSED)`): it closes the session as `close()` does
(TEARDOWN with at most 500 ms for the reply, then up to 2 s for the client's
thread). A live TCP-interleaved
`DemuxReceiver` on it then ends after the events already queued: with end of
stream (`endReason()` is `CLEAN_TEARDOWN`) when the server accepted the
TEARDOWN within those 500 ms on a session whose cancel handle had not fired, and with
`RtpException(CLOSED)` when it refused it or did not answer in time, or when
the session's cancel handle had fired before the TEARDOWN was sent. Ensure only one
`into*` call is made per session.

**End of stream or CLOSED.** On a receiver from an RTSP session, end of stream
(`null` from `recvAu()` / `recvEvent()`, the end of iteration) means the peer
ended the session in an orderly way, or the server accepted (`2xx` or `454`)
the TEARDOWN that `teardown()` or `close()` sent on a session whose cancel
handle had not fired (`close()` waits 500 ms for that answer, `teardown()`
the request timeout). `RtpException(CLOSED)` means a cancel: the receiver was
cancelled or closed; or its session was closed without the server accepting
that TEARDOWN in time; or the session's cancel handle had fired before the TEARDOWN
was sent, so `teardown()`, `close()` and a failed `intoH264Receiver()` on a
cancelled session are a cancel whatever the server then does. A session that
had already ended for its own reason (a keepalive answered `454`, a wire
failure, the peer's close) keeps that reason and that outcome. A session
keeps one end reason: open a new session for each stream. A receiver on a
UDP session has its own socket and is not ended by any of this. Keep the
`RtspSession` open while a `DemuxReceiver` from `intoDemuxReceiver()`
receives, and call `teardown()` then `close()` to end the stream cleanly
(`close()` alone waits at most 500 ms for the server's answer).

**`socket_stats()` is not Optional.** `H264Receiver.socketStats()` returns a
bare `SocketStats` (never null). This matches the Rust
`H264Receiver::socket_stats()` → `SocketStats` signature directly, which has
no "no socket" code path once constructed.

### Integration notes

- **`sprop-parameter-sets` handled automatically.** `connectH264` decodes the
  SDP `a=fmtp` attribute and stores SPS/PPS NALUs in the `H264DepayConfig`
  stashed inside the session. With `ParameterSetInjection.BEFORE_IDR` (the
  default), the depacketizer prepends them before every IDR frame.

- **B-frame / DTS limitation.** `au.pts()` is derived from the RTP timestamp
  (presentation time; AUs arrive in decode order). For live cameras without
  B-frames, PTS = DTS. For
  B-frame content, supply DTS separately to `mux.pushVideoToWithDts`.

- **Loss behavior.** A sequence-number gap drops the open AU and increments
  `rx.depayStats().ausDropped()` and `.seqGaps()`. Loss is whole-AU only.

- **KLV pairing slot.** For a STANAG 4609 gateway, push KLV using
  `mux.pushKlv(klvBytes, /*pts=*/ au.pts(), /*metadataServiceId=*/ 0x00)`.

- **RTCP is not processed on the H.264 path.** No RR/SR is sent, and
  received RTCP is discarded.

## HLS publishing (`org.tstrans.hls`)

`HlsPublisher` segments pushed MPEG-TS to `.ts` files plus a rolling
`playlist.m3u8` and serves them over a built-in HTTP(S) server;
`MuxPublisher` owns a muxer and an `HlsPublisher` so you push elementary
streams instead of TS bytes. Mirrors `tstrans.hls`.

<!-- compile: java-statements -->
```java
import java.net.InetSocketAddress;
import org.tstrans.hls.HlsMode;
import org.tstrans.hls.HlsPublisher;
import org.tstrans.hls.MuxPublisher;
import org.tstrans.mpegts.KlvStreamType;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

HlsPublisher publisher = HlsPublisher.builder()
    // .noServer() instead of .bind(...) writes the files and starts no HTTP server.
    .bind("127.0.0.1:8080")            // loopback by default; front with a proxy to expose
    .outputDir("/var/cache/hls")
    .segmentDurationMs(4000)
    .playlistWindow(6)
    .mode(HlsMode.LIVE)
    .build();
InetSocketAddress at = publisher.localAddr().orElseThrow(); // empty after noServer()
System.out.println("serving http://" + at.getAddress().getHostAddress() + ":" + at.getPort()
    + "/playlist.m3u8");

MuxerConfig program = MuxerConfig.builder()
    .programNumber(1).pmtPid(0x100)
    .addVideo(0x101, VideoCodec.H264)
    .addKlv(0x102, KlvStreamType.SYNCHRONOUS_METADATA, true)
    .build();

MuxPublisher shell = MuxPublisher.withConfigHls(publisher, program); // consumes `publisher`
shell.pushVideo(annexBNal, pts, /*keyFrame=*/ true);   // past segmentDuration, a key frame begins a segment
// shell.pushVideoWithDts(annexBNal, pts, dts, true);  // same, with a decode timestamp (one video stream)
shell.pushKlv(klvBytes, pts, 0);                        // raw LS bytes; AU-cell added by the muxer

HlsPublisher back = shell.finishIntoPublisher();        // consumes the shell
back.finish();                                          // or back.finishServing() to keep serving
```

`MuxPublisher` also has `pushVideoMisp(nal, pts, keyFrame, mispTimestamp)`
(a MISB ST 0604 MISP timestamp SEI spliced before the first VCL NAL) and
`pushData(data, pts)` (a data stream's PES pass-through); like
`pushVideoWithDts`, each needs exactly one stream of its kind in the config
(else `MuxException(INVALID_USAGE)`).

Pre-muxed TS goes straight in with `publisher.pushTs(bytes)` (whole 188-byte
packets, else `HlsException(UNALIGNED_PUSH_TS)`) and `cutSegment()` /
`cutSegmentWithDuration(us)`. `publisher.localPort()` is an `OptionalInt`,
empty after `noServer()` like `localAddr()`. `finishServing()` returns an
`HlsServerHandle` that keeps the terminal playlist fetchable until
`shutdown()`; its `localAddr()` is an `InetSocketAddress`.
`HlsPublisher.builder().fromUrl("hls://127.0.0.1:8080?segment_duration=4")`
seeds a builder from a URL; later setters overlay it. Basic auth
(`basicAuth(user, pass)`) and HTTPS (`enableTls(certPath, keyPath)`) are on the
builder. Errors are `HlsException` with `Kind` {`IO`, `INVALID_CONFIG`,
`FINISHED`, `TLS`, `URL`, `BIND_FAILED`, `UNALIGNED_PUSH_TS`, `TLS_DISABLED`,
`CLOSED`, `INTERNAL`}; a muxer rejection from `MuxPublisher` is a `MuxException`.

## WebRTC playback (`org.tstrans.webrtc`)

`WhepServer` lets browsers play a mount over WebRTC: a viewer POSTs an SDP
offer to the mount's path (WHEP) and gets H.264 or H.265 video with the KLV
frame-locked to it, in an SEI message inside each access unit and on a `klv`
data channel. Mirrors the Rust `tst_webrtc::WhepServer`.

<!-- compile: java-statements -->
```java
import org.tstrans.WebrtcException;
import org.tstrans.webrtc.*;

// advertise= is the address viewers send media to (see the gotcha below).
try (WhepServer server = WhepServer.bind("whep://0.0.0.0:8889?advertise=192.0.2.10");
     MountHandle mount = server.addMount("/cam", MountConfig.defaults())) {
    // A browser POSTs its offer to http://192.0.2.10:8889/cam. pts is in 90 kHz ticks; a KLV
    // record rides the first frame pushed at or after its PTS.
    mount.pushKlv(st0601Record, pts, /*metadataServiceId=*/ 0);
    mount.pushVideo(annexBAccessUnit, pts, /*keyFrame=*/ true);
    // localAddr() and mediaAddr() are InetSocketAddresses: the kernel's port for :0.
    System.out.println("port " + server.localAddr().getPort() + " viewers=" + mount.stats().viewers());
}
```

- **The URL carries every setting.** `WhepServer.bind(String)` is
  `bind(new WhepServerConfig(url))`; the host is a literal IP and the query
  keys are `media_port`, `advertise`, `max_viewers` (1 to 64, default 10),
  `token`, `cors_origin`, `cert` and `key` (the
  [URL key table](/docs/reference/conventions.md)).
- **`wheps://` for HTTPS pages.** A page served over HTTPS may only POST its
  offer to an HTTPS endpoint: bind
  `wheps://0.0.0.0:8443?cert=server.crt&key=server.key` (PEM file paths). A
  missing or unreadable file throws `WebrtcException(TLS)` from `bind`, and no
  port is bound.
- **`advertise=` behind NAT.** Viewers send media to the addresses the
  answer offers: the bind address, plus `advertise` when set. A bind on
  `0.0.0.0` or `::` with no `advertise` offers each viewer the local address
  its request arrived on (and loopback), so a server on a LAN needs no address in its URL
  and follows an address change without a rebind. Behind NAT, or behind a
  reverse proxy on the same host, set `advertise=` to the address viewers
  reach (see [Network](/docs/guides/webrtc.md#network-ice-ports-tls)). One
  UDP port (`media_port`, default the signalling port) carries every viewer.
- **Mounts.** `MountConfig.defaults()` is an elementary H.264 mount with
  audio dropped and KLV in both carriages; build a `MountConfig` record to
  choose `MountKind.MP2T` (feed MPEG-TS with `pushTs`), H.265,
  `AudioPolicy.PASSTHROUGH_OPUS` (elementary mounts only),
  `KlvCarriage.SEI_ONLY` / `DATA_CHANNEL_ONLY`, the per-frame SEI budget or
  the per-viewer queue. Pushes never block: a full queue drops the frame and
  counts it in `MountHandle.stats()`.
- **An MP2T mount runs one frame behind.** Its demuxer emits an access
  unit when the next one starts, so the last access unit of the bytes
  pushed waits for more, and a file's last access unit is never played.
- **Close semantics.** `stop()` closes the server, and is idempotent: every
  mount's pushes then throw `WebrtcException(CLOSED)` and the sessions end.
  `close()` is `stop()` plus freeing the native server, after which every
  method throws `IllegalStateException`. `MountHandle.close()` frees only
  the handle object; the mount keeps serving until `removeMount(path)`,
  `stop()` or `close()`.
- **Errors** are `WebrtcException` with `Kind` {`URL`, `IO`, `TLS`, `MOUNT`,
  `SERVER`, `PUSH`, `INTERNAL`, `CLOSED`}: `URL` for an unparsable bind URL,
  `IO` for a port that cannot be bound, `MOUNT` for a refused path or
  configuration, `PUSH` for a refused input (the other push family, an
  access unit without a NAL unit, a KLV record over 256 KiB), `CLOSED` after
  the mount or server closed.
- **Stats.** `WhepServer.stats()`, `MountHandle.stats()` and
  `MountHandle.sessions()` (one `SessionStats` per viewer) are snapshots;
  every counter is the bit pattern of an unsigned 64-bit value. A session's
  `remoteAddr()` (`Optional<InetSocketAddress>`) and `connectedFor()`
  (`Optional<Duration>`) are empty until ICE has connected the viewer.
  `WhepServer.stats()` and `MountHandle.sessions()` ask the media thread and
  can wait up to 2 s for its reply when it is busy, so call them from a
  monitoring thread, not the thread that paces pushes; `MountHandle.stats()`
  never waits.

### Example: serve a `.ts` file to browsers

`ServeWhep` feeds an MPEG-TS file to an MP2T mount at the speed its PTS
says and prints the counters once a second until the end of the file or
Ctrl-C. The source is `bindings/jvm/src/examples/java/ServeWhep.java`; the
Gradle build compiles it before the tests run, and runs it with
`./gradlew runServeWhep --args='input.ts'` from `bindings/jvm`.

<!-- compile: java-class -->
```java
// ServeWhep: play an MPEG-TS file in a browser over WebRTC (WHEP).
//
// Usage (from bindings/jvm):
//   ./gradlew runServeWhep --args='input.ts'
//   ./gradlew runServeWhep --args='input.ts whep://0.0.0.0:8080?advertise=192.0.2.10'
//
// A WHEP player then POSTs its SDP offer to http://127.0.0.1:8080/live and
// plays the file's H.264 video (set the mount's video codec for H.265), with
// each KLV record frame-locked to its access unit (in an SEI message and on
// the "klv" data channel).
//
// The shape: one WhepServer, one MP2T mount, and a loop that feeds the file's
// bytes to the mount at the speed its timestamps say. The mount does the
// demultiplexing, so this program never parses more of the TS than the PTS it
// paces on.
import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import org.tstrans.WebrtcException;
import org.tstrans.webrtc.MountConfig;
import org.tstrans.webrtc.MountHandle;
import org.tstrans.webrtc.MountKind;
import org.tstrans.webrtc.MountStats;
import org.tstrans.webrtc.ServerStats;
import org.tstrans.webrtc.WhepServer;

public final class ServeWhep {
    private static final int TS_PACKET = 188;

    // PTS is a 33-bit count of 90 kHz ticks: 90 ticks per millisecond.
    private static final long PTS_MODULUS = 1L << 33;
    private static final long TICKS_PER_MS = 90;

    // Two PTS steps are discontinuities (a spliced or looped file), not real
    // time, and make the pacer re-anchor: forward by more than 5 s, which it
    // must not sleep through, and backward by more than 1 s. A backward step
    // up to 1 s is B-frame reordering (a few frames on the video PID) and does
    // not move the pacer. The mount's RTP clock uses wider bounds (10 s back,
    // 1 h ahead) because it only has to keep RTP timestamps moving forward; a
    // pacer that let a 3 s file loop back 3 s would push the second pass at
    // disk speed.
    private static final long MAX_FORWARD_TICKS = 5 * 90_000;
    private static final long MAX_REORDER_TICKS = 90_000;

    // Set by the Ctrl-C hook; the main loop checks it between packets.
    private static volatile boolean running = true;
    // Set before System.exit on an error, so the hook does not wait for main.
    private static volatile boolean exiting = false;

    public static void main(String[] args) throws IOException, InterruptedException {
        if (args.length < 1 || args.length > 2) {
            System.err.println("usage: ServeWhep <input.ts> [whep://ip:port[?key=value...]]");
            System.exit(2);
        }
        Path input = Path.of(args[0]);
        // The URL carries every server setting. 127.0.0.1 keeps the example on
        // this machine. To serve other machines, bind 0.0.0.0: each viewer is
        // offered the address its request arrived on. Behind NAT, add
        // advertise=<the public address>. A page on another origin also needs
        // cors_origin=<origin>.
        String url = args.length == 2 ? args[1] : "whep://127.0.0.1:8080";

        // Ctrl-C runs shutdown hooks, not finally blocks: the hook asks the main
        // loop to stop and waits for it, so the try-with-resources below still
        // closes the server (which ends every viewer's session cleanly).
        Thread mainThread = Thread.currentThread();
        Runtime.getRuntime().addShutdownHook(new Thread(() -> {
            running = false;
            if (exiting) {
                return; // main itself called System.exit and will not return
            }
            try {
                mainThread.join(5_000);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }));

        // try-with-resources closes the mount handle, then the server: close()
        // stops the server's two threads and frees its native memory. Closing
        // the handle alone would leave the mount serving.
        try (WhepServer server = WhepServer.bind(url);
             MountHandle mount = server.addMount("/live", mp2tMount());
             InputStream in = Files.newInputStream(input)) {
            System.out.println("serving " + input + " on port " + server.localAddr().getPort()
                + " at /live (media on udp port " + server.mediaAddr().getPort() + ")");
            serve(in, server, mount);
            // The mount's demuxer emits an access unit when the next one
            // starts, so it runs one frame behind the bytes pushed and the
            // file's last access unit is never played: a 150-frame file
            // reports frames=149.
            printStats(server, mount);
            System.out.println(running ? "end of file" : "interrupted");
        } catch (WebrtcException e) {
            // URL for a bad bind URL, IO for a port in use, MOUNT for a refused
            // mount, PUSH or CLOSED from a push.
            System.err.println("webrtc error (" + e.kind() + "): " + e.getMessage());
            exiting = true;
            System.exit(1);
        }
    }

    // MountConfig.defaults() is an elementary mount (pushVideo / pushKlv);
    // this program has muxed TS, so it keeps the other defaults and swaps the
    // kind. The video codec must match the file's: H.264 here.
    private static MountConfig mp2tMount() {
        MountConfig d = MountConfig.defaults();
        return new MountConfig(MountKind.MP2T, d.video(), d.audio(), d.klv(),
            d.seiBudgetBytes(), d.viewerQueuePackets());
    }

    // Reads the file in whole TS packets and pushes them so that a packet
    // carrying PTS p goes out (p - firstPts) / 90 ms after the first one.
    //
    // Why pace at all: the mount sends each access unit to the viewers as soon
    // as it is complete, and a browser plays frames as they arrive. Pushing the
    // file at disk speed would send minutes of video in a second; the viewers'
    // queues would overflow (counted in viewerPacketsDroppedQueueFull) and the
    // browser would show a burst, then nothing.
    //
    // Why PTS rather than PCR: PTS is the clock the mount and the browser use
    // for the frames themselves, and it is in every PES header, so a file
    // without PCRs still paces. Only the first video PID's PTS is used: KLV
    // and audio PES run on their own PIDs, and a stream that runs behind the
    // video would otherwise look like a jump back.
    private static void serve(InputStream in, WhepServer server, MountHandle mount)
            throws IOException, InterruptedException, WebrtcException {
        // Push in batches: everything read since the last PTS step goes in one
        // pushTs call. The mount accepts any chunking, so a batch boundary in
        // the middle of a PES packet is fine.
        byte[] chunk = new byte[TS_PACKET * 64];
        byte[] batch = new byte[TS_PACKET * 4096];
        int batchLen = 0;

        long startNanos = System.nanoTime();
        long lastStats = startNanos;
        int pacingPid = -1;    // the video PID paced on, once seen
        long lastPts = -1;     // the 33-bit PTS last paced on
        long mediaTicks = 0;   // ticks elapsed since the first PTS, unwrapped
        long anchorTicks = 0;  // mediaTicks at the last re-anchor
        long anchorNanos = startNanos;

        int n;
        while (running && (n = in.readNBytes(chunk, 0, chunk.length)) > 0) {
            // A trailing partial packet is not TS; leave it out.
            n -= n % TS_PACKET;
            for (int off = 0; off < n && running; off += TS_PACKET) {
                long pts = videoPts(chunk, off);
                int pid = ((chunk[off + 1] & 0x1F) << 8) | (chunk[off + 2] & 0xFF);
                if (pts >= 0 && pacingPid < 0) {
                    pacingPid = pid;
                    lastPts = pts;
                }
                if (pts >= 0 && pid == pacingPid && pts != lastPts) {
                    // Unwrap the 33-bit clock: a step below 2^32 is forward
                    // (it may cross the wrap), a larger one is backward.
                    long step = Math.floorMod(pts - lastPts, PTS_MODULUS);
                    boolean forward = step < PTS_MODULUS / 2;
                    long back = PTS_MODULUS - step;
                    if (forward || back > MAX_REORDER_TICKS) {
                        lastPts = pts;
                        // Everything before this packet is due by now.
                        if (batchLen > 0) {
                            mount.pushTs(Arrays.copyOf(batch, batchLen));
                            batchLen = 0;
                        }
                        if (!forward || step > MAX_FORWARD_TICKS) {
                            // A discontinuity: restart the timeline here.
                            anchorTicks = mediaTicks;
                            anchorNanos = System.nanoTime();
                        } else {
                            // Wait for this packet's time.
                            mediaTicks += step;
                            long dueNanos = anchorNanos
                                + (mediaTicks - anchorTicks) * 1_000_000 / TICKS_PER_MS;
                            long waitNanos = dueNanos - System.nanoTime();
                            if (waitNanos > 0) {
                                Thread.sleep(waitNanos / 1_000_000,
                                    (int) (waitNanos % 1_000_000));
                            }
                        }
                    }
                    // else: a reordered B-frame PTS; the pacer stays put.
                }
                if (batchLen == batch.length) {
                    mount.pushTs(Arrays.copyOf(batch, batchLen));
                    batchLen = 0;
                }
                System.arraycopy(chunk, off, batch, batchLen, TS_PACKET);
                batchLen += TS_PACKET;

                long now = System.nanoTime();
                if (now - lastStats >= 1_000_000_000L) {
                    lastStats = now;
                    printStats(server, mount);
                }
            }
        }
        if (batchLen > 0) {
            mount.pushTs(Arrays.copyOf(batch, batchLen));
        }
    }

    // The PTS of the video PES packet (stream_id 0xE0 to 0xEF) that starts in
    // the TS packet at buf[off], or -1 when none starts there. Only the fields
    // pacing needs are read: the TS header, the adaptation field's length, and
    // the PES header's stream_id and PTS.
    private static long videoPts(byte[] buf, int off) {
        if (buf[off] != 0x47) {
            return -1; // lost sync; the mount's demuxer resynchronises itself
        }
        boolean payloadStart = (buf[off + 1] & 0x40) != 0;
        int adaptation = (buf[off + 3] >> 4) & 0x3;
        if (!payloadStart || (adaptation & 0x1) == 0) {
            return -1; // no PES header begins in this packet
        }
        int p = off + 4;
        if ((adaptation & 0x2) != 0) {
            p += 1 + (buf[p] & 0xFF); // skip the adaptation field
        }
        int end = off + TS_PACKET;
        // PES header: 00 00 01, stream_id, length (2), flags (2), header
        // length (1), then the 5-byte PTS when PTS_DTS_flags has its top bit.
        if (p + 14 > end || buf[p] != 0 || buf[p + 1] != 0 || buf[p + 2] != 1) {
            return -1; // a PSI section (PAT, PMT) or a header that is cut short
        }
        if ((buf[p + 3] & 0xF0) != 0xE0 || (buf[p + 7] & 0x80) == 0) {
            return -1; // not video, or no PTS
        }
        int t = p + 9;
        // PTS[32..30] in bits 3..1 of the first byte, then two 15-bit halves,
        // each followed by a marker bit.
        return ((long) (buf[t] & 0x0E) << 29)
            | ((long) (buf[t + 1] & 0xFF) << 22)
            | ((long) (buf[t + 2] & 0xFE) << 14)
            | ((long) (buf[t + 3] & 0xFF) << 7)
            | ((long) (buf[t + 4] & 0xFF) >> 1);
    }

    // One line per second. server.stats() asks the media thread and can wait
    // up to 2 s for its reply; calling it inline from the pushing loop is
    // acceptable here because it runs once a second and the wait is bounded.
    // mount.stats() never waits. The counters are unsigned 64-bit values
    // carried in a long.
    private static void printStats(WhepServer server, MountHandle mount) {
        ServerStats s = server.stats();
        MountStats m = mount.stats();
        System.out.printf(
            "viewers=%d sessions=%d frames=%d keyframes=%d klv=%d klv_sei=%d klv_dc=%d"
                + " rtp_packets=%d queue_drops=%d nonconformant=%d%n",
            m.viewers(), s.sessionsTotal(), m.framesPushed(), m.keyframesPushed(),
            m.klvRecordsPushed(), m.klvSeiInserted(), m.klvDcMessagesSent(),
            s.rtpPacketsSent(), m.viewerPacketsDroppedQueueFull(), m.demuxNonconformant());
    }
}
```

## Pipeline pairing (`org.tstrans.pipeline.Pairer`)

MPEG-TS programs that carry synchronized KLV metadata (e.g. MISB ST 0601 UAS
Datalink) multiplex video on one PID and KLV on another, both timestamped against
the same 90 kHz clock but arriving in separate PES packets.
`Pairer` — wrapping the Rust core `tst_pipeline::ext::pairing::PairingDemuxer` —
correlates the two streams by PTS without exposing any demux events across the FFI
boundary: you feed raw TS bytes, and for each video access unit the pairer searches
for the nearest-PTS KLV sample within the configured tolerance (default 300 ms). A
successful match produces a `PairerOutput.Paired`; a sample with no counterpart in
the window becomes `PairerOutput.UnpairedVideo` or `PairerOutput.UnpairedKlv`;
every other demux event — PMT, off-PID samples, discontinuity notices — surfaces as
`PairerOutput.PassThrough`.

<!-- compile: java-statements -->
```java
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import org.tstrans.DemuxException;
import org.tstrans.pipeline.Pairer;
import org.tstrans.pipeline.PairerConfig;
import org.tstrans.pipeline.PairerMode;
import org.tstrans.pipeline.PairerOutput;
import org.tstrans.pipeline.PairerStats;
import org.tstrans.pipeline.PairingDemuxerConfig;

int videoPid = 0x101, klvPid = 0x102;

// Realtime nearest-PTS pairing, 100 ms tolerance. Pass null for demuxer defaults.
PairingDemuxerConfig cfg = new PairingDemuxerConfig(
    new PairerConfig(new PairerMode.Realtime(), Duration.ofMillis(100), 32, 32),
    null);

try (Pairer pairer = new Pairer(videoPid, klvPid, cfg)) {
    List<PairerOutput> outs = new ArrayList<>(pairer.feed(tsBytes));
    outs.addAll(pairer.flush());              // drain end-of-stream (trailing UnpairedKlv; buffered video too)
    for (PairerOutput out : outs) {
        if (out instanceof PairerOutput.Paired p) {
            // p.video().codec() (e.g. VideoCodec.H264)
            // p.video().parse() is List<VideoUnit> (NalUnit / Obu units, opt-in)
            // p.klv().payload() is a ByteBuffer of raw KLV LS bytes
        } else if (out instanceof PairerOutput.PassThrough pt) {
            // pt.event() is a DemuxEvent (ProgramMap, off-PID samples, ...)
        }
    }
    PairerStats s = pairer.stats();           // s.paired(), s.unpairedVideo(), ...
}
```

The simplest form — `new Pairer(videoPid, klvPid)` — uses `PairerConfig.defaults()`
(Realtime mode, 300 ms tolerance, 32/32 buffers) with
demuxer defaults. To tolerate arrival skew, switch to Buffered mode: pass
`new PairerMode.Buffered(Duration.ofMillis(200))` as the mode. Buffered mode
holds samples until the lag window closes; call `flush()` at end-of-stream
to release any samples still waiting.

`feed` returns a `List<PairerOutput>` containing all outputs produced from the
supplied bytes; match each element with `instanceof` on the four sealed records:
`Paired` (video + KLV matched within tolerance), `UnpairedVideo` (video with no KLV
counterpart), `UnpairedKlv` (KLV with no video counterpart), and `PassThrough` (any
other demux event, including `DemuxEvent.ProgramMap`). `feed` throws the checked
`DemuxException` on non-conformant input (same exception kind as
`org.tstrans.mpegts.Demuxer`). `stats()` returns a frozen `PairerStats` snapshot
with `paired`, `unpairedVideo`, `unpairedKlv`, and `passThrough` counters;
`demuxerStats()` returns the underlying `DemuxerStats`; `resetStats()` zeroes the
pairer counters without touching demuxer stats.

Gotchas:

- **`KlvSample.payload()` is a heap-copied, JVM-owned `ByteBuffer`** — the KLV LS
  bytes are copied from Rust on each event (consistent with the page-wide
  `ByteBuffer` policy). Safe to retain indefinitely.
- **Single-threaded contract.** `Pairer` is not thread-safe — the consumer owns
  concurrency, the same as `org.tstrans.mpegts.Demuxer`.
- **Always call `flush()` at end-of-stream — in either mode.** It drains any
  unused KLV history as trailing `UnpairedKlv` (in both Realtime and Buffered —
  e.g. metadata that arrived after the last video access unit), and in Buffered
  mode it additionally force-drains the buffered video AUs (best-effort matched).
  Skipping it can therefore lose buffered video or trailing metadata.
- **`feed` throws checked `DemuxException`.** Declare it in `throws` or wrap it.
- **Closed `Pairer` → `IllegalStateException`.** All methods (`feed`, `flush`,
  `stats`, `demuxerStats`, `resetStats`) throw `IllegalStateException` after
  `close()`.

## Language-specific gotchas

- **`payload` is a heap-copied, JVM-owned `ByteBuffer`.** Each
  sample / metadata payload is a **copy** of the demuxed bytes, not a view
  over Rust memory. That makes it safe to retain indefinitely — the buffer
  stays readable after the next `nextEvent()` pull and after the `Demuxer`
  is `close()`d. True zero-copy (a direct `ByteBuffer` over native memory)
  is deferred to a future JDK-22+ path built on the Foreign Function &
  Memory API (`Arena` / `MemorySegment`), where the buffer's lifetime can
  be tied to a confined arena. On the JDK-17 baseline this binding copies —
  a direct buffer over Rust-owned memory would be a use-after-free
  foot-gun, so it is deliberately not offered here.
- **`dts` is a nullable `Long`** — boxed, not a primitive `long`. It is
  `null` when the PES carried no DTS. Null-check before unboxing.
- **`Demuxer` is single-threaded** — the consumer owns concurrency. Don't
  share one `Demuxer` across threads without external synchronization.
  Iterating drains the currently-queued events; call `feed` / `flush` to
  enqueue more.
- **`NonConformant` collapses** the Rust core's 30+-variant issue set into
  a single `NonConformantKind` enum plus a human-readable `issue` String
  (and the optional CFI / multi-cell-reason fields). Match on `kind` for
  programmatic dispatch; read `issue` for the human-facing detail.
- **`TSTRANS_LOG` bridges the Rust core's `tracing` events to stderr.**
  Set it in the process environment before the JVM loads `libtstjni`
  (`EnvFilter` syntax, same as `RUST_LOG` — e.g.
  `TSTRANS_LOG=tst_rtp=debug`) to see diagnostics (keepalive failures,
  RTSP client warnings, …) that otherwise vanish silently — nothing installs a
  subscriber by default. The bridge installs from `JNI_OnLoad`, the JVM's
  one guaranteed one-time native-library entry point (called during
  `System.load`, before any `org.tstrans.*` native method is reachable).
  Unset means zero overhead beyond the one env lookup at load time; if
  the host process already installed its own `tracing` subscriber, this
  bridge never displaces it.

## Where this binding differs from the Rust core

- **Surface.** The JVM binding surfaces the `org.tstrans.mpegts.Demuxer` receive path
  (feed bytes → typed `DemuxEvent`s with typed NAL / OBU / ADTS payloads),
  the offline `org.tstrans.mpegts.Muxer` send path (config builder → push
  family → `pull` / `writeFile`), the full `org.tstrans.klv` typed-KLV
  surface (ST 0601 / 0102 / 0605 / 0903 decode + encode + `parseUniversal`
  dispatcher, plus ST 0806 RVT, ST 1010 SDCC-FLP, and the ST 0805
  KLV → CoT conversion layer), the `org.tstrans.codec` elementary-stream parsers (H.264 /
  H.265 / H.266 / AV1 / AAC / MPEG-2 audio), the `org.tstrans.io` file
  helpers (`parseFile`, `probe`, `extractKlv`), the `org.tstrans.srt` SRT
  transport surface (the low-level `Builder`/`Socket`/`Listener` + the
  `Managed*` reconnect family), the `org.tstrans.shells` pipeline shells
  (`Sender`/`Receiver`/`MuxSender`/`DemuxReceiver` over every transport URL +
  `CancelHandle`/`SocketStats`/`TransportStats`/`EndReason`), the
  `org.tstrans.tcp` TCP listener (`Listener`/`Transport`, each accepted
  connection converted into a shell), the
  `org.tstrans.rtp` RTP transport surface (`Transport`/`RecvTransport` +
  RTSP client/server), the `org.tstrans.hls`
  HLS publisher family (`HlsPublisher`/`HlsPublisherBuilder`/
  `HlsServerHandle` + the `MuxPublisher` shell), the
  `org.tstrans.pipeline` pairing shell (`Pairer`), and the
  `org.tstrans.Version` bootstrap.
- **JDK 17 baseline.** The examples use `instanceof` pattern matching, not
  `switch`-on-sealed (which needs JDK 21+). `switch` patterns work on
  21+, but `instanceof` is the portable form on the 17 baseline.
- **`payload` is a heap-copied `ByteBuffer`**, not a direct buffer over
  native memory. Safe-zero-copy is deferred to a JDK-22+ Foreign Function &
  Memory API (`Arena`) path — see the gotcha above.
- **Single fat JAR** bundles the per-platform native library
  (`.so` / `.dylib` / `.dll`); the `NativeLoader` extracts the correct one
  at runtime. No per-platform classifier.
- **`endDetail()` reads the Rust enum field directly, not a
  last-error channel.** The C ABI's `TstStreamEndReason` accessors read
  a recorded `KeepaliveFailed` / `TransportFailed` / `ProtocolError`
  message through the shared thread-local last-error channel
  (`tst_get_last_error_str`), resetting it on every call. This binding
  has no such channel — `endDetail()` reads the `msg` field straight
  off the Rust `EndReason` value (same approach as Python). A
  recorded end reason is data, not a failure, so it never touches
  `RtpException`.
- **`lastSeen(pid)` returns an empty `Optional`, not `0`, for "never
  seen".** The C ABI has no nullable type in its getter shape, so it
  uses a `0` sentinel; this binding answers with an `Optional<Instant>`
  (Python answers `None`).
- **A finished or consumed HLS handle throws `IllegalStateException`**, like
  every other closed handle here; Python raises `HlsError(FINISHED)` for the
  same state. `HlsException.Kind.FINISHED` is still declared because the
  Rust library can raise it. `close()` on `HlsPublisher` / `MuxPublisher` is
  quiet (finish, errors dropped); `finish()` throws.

The Rust page's "Where this binding differs from the Rust core" section
treats Rust as the canonical surface; everything here is a subset of it.
See [`/docs/languages/rust.md`](/docs/languages/rust.md) for the full
surface and [`/docs/languages/python.md`](/docs/languages/python.md) for the
Python binding's gaps.

### Moving from 0.8

Deprecated in 0.9.0 and removed in 0.11.0. In 0.9 and 0.10 each old shell
class was a `@Deprecated(forRemoval = true)` subclass of its replacement that
kept its 0.8 `fromUrl` factories; from 0.11.0 the classes in the left column
do not exist.

The `org.tstrans.shells` classes accept every transport's scheme in `open`.
`open` and the instance methods (`sendBytes`, `sendVideo`, `recvBytes`,
`recvEvent`, ...) declare `throws BindingException` (plus `MuxException` on
`MuxSender`, `DemuxException` on `recvEvent`), not `SrtException` /
`RtpException`. A 0.8 call site that catches only `SrtException` or
`RtpException` around them does not compile: catch `BindingException`, which
every transport exception extends.

| 0.8 (removed in 0.11.0) | 0.9 and later |
|---|---|
| `org.tstrans.srt.{Sender, Receiver, MuxSender, DemuxReceiver}`, `org.tstrans.rtp.{MuxSender, DemuxReceiver}` | `org.tstrans.shells.*`, opened with `Class.open(url, ...)` |
| `org.tstrans.rtp.Sender` / `Receiver` | `org.tstrans.rtp.Transport` / `RecvTransport` |
| the `pktSize` argument of the rtp shell factories | the `?pkt_size=` URL key (`rtp.Transport.fromUrl` still takes `pktSize`) |

Removed in 0.9.0 with no deprecated alias: the per-package `CancelHandle`,
`SocketStats`, `TransportStats`, `RecvEndReason` and `StreamEndReason` of
`org.tstrans.srt` and `org.tstrans.rtp`. Use the one
`org.tstrans.shells.{CancelHandle, SocketStats, TransportStats, EndReason}`.

`ManagedSender` / `ManagedReceiver` / `ManagedMuxSender` /
`ManagedDemuxReceiver` keep `fromUrl`: they are SRT-specific.

## Surface by package

- **`org.tstrans.Version`** — the bootstrap; proves the
  cargo → cdylib → Gradle → Java → JNI build pipeline and native loader.
- **mpegts demux** — `org.tstrans.mpegts.Demuxer` + `DemuxEvent` + `DemuxerConfig`.
- **mpegts mux** — `org.tstrans.mpegts.Muxer` + `MuxerConfig` + push family + `pull`.
- **klv** — typed KLV decode/encode (ST 0601 / 0102 / 0605 / 0903, plus ST 0806 RVT, ST 1010 SDCC-FLP, and the ST 0805 KLV → CoT conversion layer) under `org.tstrans.klv`.
- **codec** — H.264 / H.265 / H.266 / AV1 + audio parsers under
  `org.tstrans.codec`; typed elementary-stream payloads (NAL / OBU / ADTS).
- **io** — file inspection helpers (`Io.parseFile`, `probe`, `extractKlv`, `Muxer.writeFile`).
- **shells** — `Sender` / `Receiver` / `MuxSender` / `DemuxReceiver` over every
  transport URL, with `CancelHandle` / `SocketStats` / `TransportStats` /
  `EndReason`, the `DemuxReceiver.tsBytesDemuxed` counter and the
  receivers' `localPort()`, `localAddr()`, `peerAddr()` and
  `sourceRejected()`.
- **tcp** — `Listener` + `Transport`: a `tcp://` / `tcps://` listener whose
  accepted connections become `shells` classes.
- **srt** — `Builder` / `Socket` / `Listener` / `SrtStats`; the
  `ReconnectPolicy` / `BackoffStrategy` / `OverflowPolicy` types; the
  `Managed*` reconnect wrappers.
- **rtp** — MPEG-TS-over-RTP transports (`Transport` / `RecvTransport`) +
  RTSP client / server (incl. the publisher role: `PublishMount`) +
  `H264Receiver`.
- **hls** — `HlsPublisher` + `MuxPublisher` HLS segmenter/server under
  `org.tstrans.hls`.
- **webrtc** — `WhepServer` + `MountHandle` WebRTC (WHEP) playback under
  `org.tstrans.webrtc`.
- **pipeline** — `org.tstrans.pipeline.Pairer` pairing shell.
- **Packaging** — one fat JAR bundling the
  linux-x86_64 / linux-aarch64 / macos-arm64 / windows-x86_64
  native libraries, published to Maven Central as `org.tstrans:tstrans-jvm`.

## Where to go next

- [`/docs/start/concepts.md`](/docs/start/concepts.md) — the conceptual
  model (mux/demux, KLV, transport) before any code.
- [`/docs/guides/mpegts-demux.md`](/docs/guides/mpegts-demux.md) — the full
  demuxer contract: strict-mode ladder, AU-cell unwrap behavior,
  non-conformant handling. The JVM `Demuxer` is a thin wrap over this.
- [`/docs/guides/klv.md`](/docs/guides/klv.md) — the KLV substrate the
  `Metadata` event payloads carry; the `org.tstrans.klv` typed-decode
  surface mirrors this guide module-for-module.
- [`/docs/languages/rust.md`](/docs/languages/rust.md) — the canonical Rust
  surface this binding mirrors.
