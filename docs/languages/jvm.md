# JVM bindings (`org.tstrans`)

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
> - How to send and receive MPEG-TS over RTP and SRT (`org.tstrans.rtp` / `org.tstrans.srt`) — pre-muxed bytes or the `MuxSender`/`DemuxReceiver` shells — and how to drive RTSP (client + server)
> - How to pair video with KLV metadata by PTS using `org.tstrans.pipeline.Pairer`
> - The JVM-specific gotchas: heap-copied `ByteBuffer` payloads, nullable `Long` DTS, codec on `StreamId`
> - How this binding differs from the Rust core

> **Status (mpegts demux + offline mux + typed KLV + codec parsers + file I/O +
> RTP + SRT transports shipped):** the JVM binding ships the bootstrap
> `org.tstrans.Version` hello-world; the complete `org.tstrans.mpegts` **demux**
> surface (`Demuxer`, `DemuxerConfig`, the sealed `DemuxEvent` hierarchy,
> `StreamId`, codec / kind enums); the offline **mux** surface (`Muxer`,
> `MuxerConfig`, push family + `pull` + `writeFile` / `MuxerFileSink`); the full
> **typed KLV** surface (`org.tstrans.klv` — decode/encode for ST 0601 / 0102 /
> 0605 / 0903, the `parseUniversal` dispatcher, and the field-error model); the
> **codec parsers** (`org.tstrans.codec` — H.264 / H.265 / H.266 / AV1 / AAC /
> MPEG-2 audio, typed NAL / OBU / ADTS payloads on demux events); the **file I/O
> helpers** (`org.tstrans.io` — `parseFile`, `probe`, `extractKlv`); the
> **SRT transport** (`org.tstrans.srt` — `Sender`/`Receiver` pipeline shells,
> the `Builder`/`Socket`/`Listener`/`CancelHandle` low-level surface, the
> `MuxSender`/`DemuxReceiver` convenience shells, and the `Managed*`
> auto-reconnect family); the **RTP transport** (`org.tstrans.rtp` —
> `Sender`/`Receiver` transports, `MuxSender`/`DemuxReceiver` convenience
> shells, and the RTSP client + server); and the **pipeline shells**
> (`org.tstrans.pipeline` — the `Pairer` pairing shell).
> This page documents only what exists today.

## Install

The JVM binding is published to Maven Central as
`org.tstrans:tstrans-jvm` (JDK 17+):

```xml
<dependency>
  <groupId>org.tstrans</groupId>
  <artifactId>tstrans-jvm</artifactId>
  <version>0.6.0</version>
</dependency>
```

To build from source instead:

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
at runtime.

**Minimum JDK is 17.** The native code is delivered as a single fat JAR
bundling the per-platform native libraries (linux-x86_64 / linux-aarch64 /
macos-arm64 / windows-x86_64); the consumer picks no classifier.

## Hello world

The smallest thing that proves the native library loads and the JNI bridge
works — print the version string:

```java
import org.tstrans.Version;

System.out.println(Version.versionString());  // e.g. "0.6.0"
```

## First send

Build a single-program H.264 transport stream offline: configure the muxer,
push one access unit, then drain assembled TS packets with `pull`. The muxer
is deterministic — identical inputs produce byte-identical output across the
Rust, Python, and JVM bindings.

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
builder is single-program. Data streams may also carry raw PMT descriptor TLVs
via `streamDescriptorsForData(dataIndex, byte[][])` (the muxer never auto-emits
a descriptor on a data stream). Deep config validation (PID collisions,
PMT-size budget, sync-KLV-without-PTS, a data `streamType` that would classify
as a typed kind, …) runs in the native `Muxer` constructor and surfaces as
`MuxException(CONFIG_INVALID)`.

> **Scope.** This binding's `MuxerConfig` is single-program; multi-program
> configs are deferred. DVB-subtitle codec configuration is deferred;
> `addSubtitle` accepts the no-config codecs (`CEA708_STANDALONE` /
> `WEBVTT_IN_TS`). Per-stream descriptors for the typed kinds (video / KLV /
> audio / subtitle) are deferred.

## First receive

Demux a `.ts` file and dispatch on typed events. The JVM binding's
baseline is **JDK 17**, where `instanceof` pattern matching is the portable
idiom for a sealed hierarchy:

```java
import org.tstrans.mpegts.*;

byte[] ts = java.nio.file.Files.readAllBytes(java.nio.file.Path.of("capture.ts"));
try (Demuxer d = new Demuxer()) {
    d.feed(ts);
    d.flush();
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

```java
import org.tstrans.mpegts.*;

DemuxerConfig cfg = DemuxerConfig.builder()
    .strictMode(StrictMode.FULL)
    .cfiTolerance(false)
    .pesCapPerPid(4_000_000)
    .build();

try (Demuxer d = new Demuxer(cfg)) {
    d.feed(ts);
    d.flush();
    for (DemuxEvent e : d) {
        // ...
    }
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
| `syncBufCap` | `long` | `0` (Rust default, 4 MiB) | Pre-sync ingress buffer ceiling in bytes; a `feed()` call larger than this throws `DemuxException` (kind `SYNC_LOSS`) before consuming any bytes. |
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
- `MultiCellAuReason` — `ORPHAN`, `SEQUENCE_GAP`, `CONCURRENT_FIRST`, `OVERFLOW`
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

```java
import org.tstrans.klv.*;

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

```java
PrecisionTimeStampPack pack = Klv.decodePrecisionTimestamp(wireBytes);  // throws KlvDecodeException
System.out.println(pack.timestampUs() + " µs, locked=" + pack.timeStatus().isLocked());

byte[] wire = Klv.encodePrecisionTimestamp(pack);  // infallible; always 26 bytes
```

**ST 0903 — VMTI LS** (body-only for decode; two encode forms):

```java
VmtiLs vmtiLenient = Klv.decodeVmti(bodyBytes);        // lenient
VmtiLs vmtiStrict = Klv.decodeVmti(bodyBytes, true);   // strict

System.out.println(vmtiLenient.targets().size() + " targets");

byte[] body = Klv.encodeVmti(vmtiLenient);               // body only (no UL / BER / checksum)
byte[] framed = Klv.encodeVmtiStandalone(vmtiLenient);   // full [UL][BER][body][Tag1 checksum]
```

**ST 0806 — RVT (Remote Video Terminal) Local Set** (body bytes carried in
`UasDatalinkLs.rvt()`, ST 0601 Tag 73):

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

```java
import org.tstrans.codec.Codec;
import org.tstrans.codec.H264Sps;
import org.tstrans.codec.AdtsFrame;
import org.tstrans.CodecParseException;

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

Since the codec wave, the demuxer hands back **typed** elementary units on
demand. `DemuxEvent.Video.parse()` returns a `List<VideoUnit>` — `NalUnit`s for
H.264 / H.265 / H.266, `Obu`s for AV1 — by calling the native `split_video`
only when the caller opts in. `DemuxEvent.Audio.parse()` likewise returns a
`List<AudioFrame>` — `AdtsFrame`s for AAC, `Mpeg2AudioFrame`s for MPEG-2 audio —
parsing the raw ES bytes only when the caller opts in. The `codec()` accessor on
each event tags the discriminant (`VideoCodec` / `AudioCodec`). Downcast with
`instanceof`:

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

> **`UNEXPECTED_EOF` note.** `DemuxException.Kind.UNEXPECTED_EOF` exists for
> tst-py parity but is never thrown by the file path: truncation is treated as
> clean EOF and read failures surface as `UncheckedIOException`.

### Probe a file

`Io.probe` scans the first 5 MiB and returns a `ProbeResult` record
summarising what the file contains — without reading the entire file:

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

The `org.tstrans.srt` package wraps `tst_pipeline::Sender`/`Receiver` and the
low-level `tst_srt::SocketBuilder`/`Socket`/`Listener` for sending and receiving
pre-muxed MPEG-TS bytes over SRT. SRT is default-on — no feature flag is needed.

### Sender hello

The simplest sender: connect to a peer in caller mode and stream bytes.

```java
import org.tstrans.srt.Sender;
import org.tstrans.SrtException;

// mode=caller is the default when ?mode= is omitted.
try (var tx = Sender.fromUrl(
        "srt://host:9000?mode=caller&passphrase=secret")) {
    tx.sendBytes(tsBytes);  // push pre-muxed TS bytes (any length)
    tx.flush();             // emit any buffered partial bundle
}
```

`sendBytes` accepts raw TS bytes of any length; the sender internally frames
them into 7-packet (1316-byte) SRT bundles. Call `flush()` after the last push
to emit any partial bundle.

### Receiver hello

The simplest receiver: bind in listener mode, accept one connection, and drain
packets.

```java
import org.tstrans.srt.Receiver;
import org.tstrans.SrtException;

// Receiver.fromUrl does a one-shot bind + accept.
try (var rx = Receiver.fromUrl("srt://:9000?mode=listener")) {
    while (true) {
        byte[] pkt = rx.recvBytes();  // one 188-byte TS packet per call
        // process pkt ...
    }
}
```

`recvBytes()` returns one 188-byte TS packet per call (the SRT live-mode unit).
Break the loop when `recvBytes()` throws `SrtException(CLOSED)` or
`SrtException(BROKEN)` — both signal end of stream.

### Builder → Socket → intoReceiver (low-level path)

Use the `Builder` → `Listener` → `accept` → `Socket` → `intoReceiver()` path
when you need the listener's bound port (for example, when binding to an
ephemeral `:0` port and reporting it to the sender):

```java
import org.tstrans.srt.*;
import org.tstrans.SrtException;

// Bind to a kernel-assigned ephemeral port.
try (Listener listener = new Builder("srt://127.0.0.1:0?mode=listener")
        .listener()
        .listen()) {

    int port = listener.localAddr().port();   // get the assigned port
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

The accepted `Socket` is consumed by `intoReceiver()` — the `Socket` handle
is zeroed immediately after the JNI call. Attempting to call any `Socket`
method after `intoReceiver()` throws `IllegalStateException`. To produce a
sender from an accepted socket use `sock.intoSender()` instead.

`Builder` also exposes `connect()` for the caller side:

```java
try (Socket sock = new Builder("srt://host:9000")
        .caller()
        .latencyMs(200)
        .passphrase("hunter2hunter2")
        .connect()) {
    try (Sender tx = sock.intoSender()) {
        tx.sendBytes(tsBytes);
        tx.flush();
    }
}
```

### Cancellation

Obtain a `CancelHandle` from an open `Sender` or `Receiver` and call
`cancel()` from another thread to unblock a parked `sendBytes` /
`recvBytes` call:

```java
var tx = Sender.fromUrl("srt://host:9000?mode=caller");
var cancel = tx.cancelHandle();

// On another thread:
cancel.cancel();  // wakes tx.sendBytes() → throws SrtException(BROKEN or CLOSED)
```

`CancelHandle` is safe to share across threads. The first `cancel()` call
closes the underlying libsrt socket; subsequent calls are no-ops. Obtaining
the handle is itself safe from another thread at any time — `cancelHandle()`
returns promptly even while `sendBytes` / `recvBytes` / `next()` / `accept()`
is parked on the same object, so it need not be taken before iterating.

### SRT-specific Gotchas

- **One-shot accept on `Receiver.fromUrl`.** `Receiver.fromUrl` binds,
  listens, and accepts exactly one connection — it is not a multi-client
  server. For a server that accepts many peers, use `Builder.listener().listen()`
  and iterate or call `listener.accept(null)` in a loop.
- **`accept(null)` vs `accept(timeoutMs)`.** On the accepted socket,
  `recvBytes()` uses libsrt's edge-triggered epoll internally. If you use
  `accept(timeoutMs)` (epoll-based accept), there is a subtle interaction
  where the accept epoll subscription can prevent `srt_recv` from waking on
  TSBPD delivery when data arrives before `recvBytes()` is called. Prefer
  `accept(null)` (direct `srt_accept`) for the accepted socket's receiver path.
  `Receiver.fromUrl` always uses `srt_accept` directly and is unaffected.
- **Cancel wakes with `BROKEN` or `CLOSED`.** `CancelHandle.cancel()` wakes a
  thread parked in `sendBytes` or `recvBytes`; that call throws
  `SrtException(BROKEN)` or `SrtException(CLOSED)`. Catch both if your code
  must distinguish a cancel from a peer hangup.
- **`Receiver.close()` / `DemuxReceiver.close()` cancel first.** Calling
  either from another thread while `recvBytes()` / `next()` is parked wakes
  that call — it throws `SrtException(BROKEN)`, because the plain cancel
  closes the libsrt socket under the parked receive — and `close()` returns
  promptly. The managed pair (`ManagedReceiver` / `ManagedDemuxReceiver`)
  does the same but surfaces `CLOSED` and records `RecvEndReason.CANCELLED`.
  A `close()` with nothing parked simply closes.
- **JDK-17 byte-copy posture.** `sendBytes` copies the supplied array across
  the JNI boundary; `recvBytes` returns a heap `byte[]` copy. A zero-copy
  path using FFM `MemorySegment` is deferred to a JDK-22+ release.
- **`recvBytes()` returns one packet quantum.** Each `recvBytes()` call
  returns exactly one 188-byte TS packet. Accumulate packets to reconstruct
  a larger frame.
- **`Builder.listen()` requires `?mode=listener` in the URL.** The `Builder`
  URL must carry `?mode=listener`; calling `.listener()` sets the Java-side
  mode but does NOT inject the URL parameter. Canonical form:
  `new Builder("srt://:9000?mode=listener").listener().listen()`.
- **Negative knob values throw `IllegalArgumentException`.** All non-negative
  knob setters (`latencyMs`, `connectTimeoutMs`, etc.) reject negative values
  at construction time, mirroring tst-py's `u32`-typed API.

## SRT convenience (`MuxSender` / `DemuxReceiver`)

The `org.tstrans.srt` package also exposes the high-level convenience shells that
own a muxer / demuxer and a transport in one object, so you never touch raw TS
bytes: `MuxSender` wraps `tst_pipeline::MuxSender<SrtTransport>` (send elementary
streams → it muxes + sends) and `DemuxReceiver` wraps
`tst_pipeline::DemuxReceiver<SrtTransport>` (it recvs + demuxes → you iterate
typed `DemuxEvent`s).

### Send: `MuxSender`

Build a `MuxerConfig`, connect in caller mode, and send elementary streams:

```java
import org.tstrans.srt.MuxSender;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

MuxerConfig program = MuxerConfig.builder()
    .programNumber(1).pmtPid(0x1000)
    .addVideo(0x1011, VideoCodec.H264)
    .build();

try (MuxSender s = MuxSender.fromUrl(
        "srt://host:9000?mode=caller&latency=120", program)) {
    long pts = 0;
    for (byte[] annexBNal : accessUnits) {
        s.sendVideo(annexBNal, pts, /*keyFrame=*/ true);
        pts += 3000;  // 90 kHz PTS; advance per frame
    }
}
// MuxSender has NO flush(): bytes flush per-send and again on close().
```

`sendKlv`, `sendAudio`, `sendSubtitle`, and `sendData` (raw private-data
bytes, passed through verbatim — same PTS / ceiling semantics as
`Muxer.pushData`) cover the other elementary-stream kinds; the handle-targeted
`send*To` variants (including `sendDataTo` with the handle from `dataHandle()`)
address a specific stream in a multi-stream program.

### Receive: `DemuxReceiver`

Bind in listener mode and iterate the same sealed `DemuxEvent` hierarchy the
offline `org.tstrans.mpegts.Demuxer` produces:

```java
import org.tstrans.srt.DemuxReceiver;
import org.tstrans.mpegts.DemuxEvent;

try (DemuxReceiver rx = DemuxReceiver.fromUrl("srt://:9000?mode=listener")) {
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

### Tee the raw stream: `addByteSink`

`DemuxReceiver.addByteSink` fans out every **188-byte TS packet** — as a fresh
`byte[]` — to a callback BEFORE the demuxer parses it. Useful for record-to-disk
or a parallel parser without consuming the event iterator:

```java
try (DemuxReceiver rx = DemuxReceiver.fromUrl("srt://:9000?mode=listener")) {
    rx.addByteSink(pkt -> recorder.write(pkt));  // pkt.length == 188
    for (DemuxEvent e : rx) {
        // ... normal event handling continues unaffected ...
    }
}
```

Gotchas:

- **Fires per 188-byte packet, ahead of demux.** Each callback gets exactly one
  raw TS packet (the SRT live-mode quantum), before parsing.
- **Keep it cheap.** The sink runs on the receiver's own recv-loop thread; a slow
  sink throttles the receiver.
- **Never re-enter the receiver.** Do NOT call `next()` / `close()` / `stats()`
  on the same `DemuxReceiver` from inside the sink.
- **Fail-loud.** If the sink throws, the first error wins and is re-raised from
  the **next** iteration step, which then stops iteration.
- **Register before iterating** (or between `next()` calls) — `addByteSink` is
  not safe to call concurrently with an in-flight `next()`.

### Per-stream staleness: `lastSeenMicros`

`rx.lastSeenMicros(pid)` returns the epoch-microsecond timestamp of the most
recent item this receiver emitted for `pid`, or `null` if `pid` was never
seen — a cheap staleness check with no `stats()` snapshot-diffing required.
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

### Receive with auto-reconnect: `ManagedDemuxReceiver`

Iterate the same sealed `DemuxEvent` hierarchy. Each transport reconnect emits
exactly one `DemuxEvent.ReconnectDiscontinuity` before the post-reconnect events
— **drop your per-stream caches on receipt** and rebuild from the next
`DemuxEvent.ProgramMap`:

```java
import org.tstrans.srt.ManagedDemuxReceiver;
import org.tstrans.mpegts.DemuxEvent;

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
the other three managed shells expose it) answers with a `RecvEndReason`
member — or `null` before any ending, including while a reconnect is still
in progress. Two of its three members are reachable on the managed-SRT path
today: `RECONNECT_EXHAUSTED` (the `ReconnectPolicy`'s `maxAttempts` budget
ran out — also what a plain peer close reports under a zero-retry policy,
since a peer FIN reaches the reconnect decorator as a retryable break) and
`CANCELLED` (`cancelHandle().cancel()` fired, or `close()` was called from
another thread while `next()` was parked — `close()` cancels first, so the
parked iteration ends with `SrtException(CLOSED)`; a `close()` with no
iteration in flight ends nothing and records nothing). `END_OF_STREAM` is
reserved for a future receive transport that can signal a clean end distinct
from budget exhaustion. The surface exists specifically to tell a
caller-initiated cancel apart from a budget-exhausted give-up, which
otherwise both surface identically as `SrtException(CLOSED)` from the
iterator. It reads a lock-free cell captured when the receiver is opened, so
it answers while another thread is parked in `next()` and keeps answering
after `close()`.

### Stats drifts on the managed shells

The managed shells inherit the same `srtStats()` drifts as tst-py — **read these
carefully, they are not uniform across the four shells:**

- **`ManagedSender.srtStats()` and `ManagedReceiver.srtStats()` always throw
  `SrtException(IO)`** today (the managed send/recv transports expose no
  SRT-rich shape). Use `socketStats()` instead — it returns the scheme-neutral
  16-field `SocketStats`.
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
call, and `send`/`sendVideo`/… enqueue into the gap buffer under
`overflowPolicy` instead of blocking:

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

## RTP transport (`org.tstrans.rtp`)

The `org.tstrans.rtp` package covers two RTP receive shapes:

- **MPEG-TS-over-RTP (RFC 2250, PT=33):** `RtpTransport` / `RtpRecvTransport`
  send/receive pre-muxed MPEG-TS bytes over RTP/UDP. Each `send` produces one
  datagram (12-byte RTP header + TS payload); each `recv` returns one datagram's
  TS payload with the header stripped.
- **H.264-over-RTP (RFC 6184):** `H264Receiver` and `RtspClient.connectH264`
  ingest bare H.264 access units from an RTSP camera. See the dedicated section
  below.

RTP is default-on — no feature flag is needed.

### Sender hello

The simplest sender: bind to a destination and push TS bytes. Each call sends
one datagram.

```java
import org.tstrans.rtp.Sender;
import org.tstrans.RtpException;

// Send pre-muxed TS bytes over RTP-over-UDP:
try (Sender s = Sender.fromUrl("rtp://239.0.0.1:5004")) {
    s.send(tsBytes);  // one UDP datagram, framed with an RTP header
}
```

`send(byte[])` accepts a TS payload up to the configured packet size **minus the
12-byte RTP header** prepended to every datagram — i.e. up to
`Sender.DEFAULT_PKT_SIZE - 12` = 1304 bytes at the default `pkt_size` of 1316. A
payload that exceeds the cap throws `RtpException(MALFORMED_PACKET)`.

### Receiver hello

The simplest receiver: bind to the same group/port and read one datagram per
`recv()` call. For multicast URLs the group is joined automatically.

```java
import org.tstrans.rtp.Receiver;
import org.tstrans.RtpException;

// Receive on the same group/port:
try (Receiver r = Receiver.fromUrl("rtp://239.0.0.1:5004")) {
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

Obtain a `CancelHandle` from an open `Sender` or `Receiver` and call `cancel()`
from another thread to unblock a parked `send` / `recv` call:

```java
var rx = Receiver.fromUrl("rtp://127.0.0.1:5004");
var cancel = rx.cancelHandle();

// On another thread:
cancel.cancel();  // wakes rx.recv() → throws RtpException(CANCELLED)
```

`CancelHandle` is safe to share across threads; `cancel()` and `close()` are
`synchronized`. The cancel takes effect at the next ~100 ms cancel-poll tick.

### RTP-specific gotchas

- **`Sender` / `Receiver` are not thread-safe.** Use one per thread. A
  cross-thread stop goes through `cancelHandle().cancel()`, which wakes a parked
  `send`/`recv` with `RtpException(CANCELLED)`.
- **`org.tstrans.rtp.SocketStats` is a distinct type from
  `org.tstrans.srt.SocketStats`.** Same 16-field shape, different package. The
  RTCP-derived fields (`rttUs`, `packetsLost*`) stay zero until RTCP ingest is
  wired; `RtpTransport` populates the send-side counters, `RtpRecvTransport` the
  receive-side.
- **`rtp` `CancelHandle` has no `isCancelled()`.** Unlike the srt
  `CancelHandle`, the RTP one exposes only `cancel()` (mirroring tst-py's
  `tstrans.rtp.CancelHandle`).
- **Closed handle → `IllegalStateException`.** Calling `send` / `recv` /
  `socketStats` / `cancelHandle` after `close()` throws `IllegalStateException`,
  the established JVM idiom (tst-py raises `RtpError` instead).
- **Negative `pktSize` / out-of-range `ssrc` throw `IllegalArgumentException`.**
  Validated at construction, mirroring tst-py's `u32`-typed API.

See [`/docs/languages/python.md`](/docs/languages/python.md) for the canonical
`tstrans.rtp` Python surface this binding mirrors.

### RTP convenience: MuxSender / DemuxReceiver

`org.tstrans.rtp.MuxSender` bundles a `Muxer` + an RTP transport — send encoded
video/KLV/audio/subtitle/private-data and it muxes to MPEG-TS and sends over
RTP/UDP in one call. The send surface matches the srt `MuxSender`: the per-kind
`send*` shorthands (including `sendData`), the handle-targeted `send*To`
variants (including `sendDataTo`), and the per-kind handle accessors
(including `dataHandle()`). `org.tstrans.rtp.DemuxReceiver` bundles a `Demuxer`
+ an RTP recv transport and iterates `DemuxEvent`s.

```java
MuxerConfig program = MuxerConfig.builder()
    .programNumber(1).pmtPid(0x1000)
    .addVideo(0x1011, VideoCodec.H264)
    .build();
try (MuxSender s = MuxSender.fromUrl("rtp://127.0.0.1:5004", program)) {
    s.sendVideo(annexBNal, /*pts*/ 0L, /*keyFrame*/ true);
}

try (DemuxReceiver rx = DemuxReceiver.fromUrl("rtp://0.0.0.0:5004")) {
    rx.addByteSink(pkt -> record(pkt)); // fans out each raw 188-byte TS packet
    for (DemuxEvent e : rx) {
        if (e instanceof DemuxEvent.Video v) { /* ... */ }
    }
}
```

**Gotchas:**

- `MuxSender.fromUrl` takes an optional `pktSize` (default 1316); a negative
  `pktSize` is rejected with `IllegalArgumentException`. The payload cap per push
  is `pktSize − 12` (the RTP header is prepended by the transport).
- The RTP `MuxSender` / `DemuxReceiver` expose **no `cancelHandle()` and no
  `socketStats()`** (matching the Python surface) — only `stats()` (a
  `(SocketStats, MuxerStats)` snapshot). To stop a `DemuxReceiver` iteration that
  is parked waiting for the next datagram, call `close()` from another thread; it
  cancels the in-flight recv first, then frees the receiver (safe cross-thread).
- RTP/UDP is connectionless: a remote sender closing does **not** end a
  `DemuxReceiver` iteration. Break out of the loop on a sentinel event (or close
  the receiver) rather than waiting for end-of-stream.
- `addByteSink` callbacks run on the receiver's own thread and must not re-enter
  the receiver. Sample payloads and byte-sink buffers are heap `byte[]` copies.

`rx.lastSeenMicros(pid)` returns the Unix-epoch microsecond timestamp of the
most recent item this receiver emitted for `pid`, or `null` if that PID has
never been seen — a per-stream staleness check that needs no `stats()`
snapshot-diffing. A watchdog thread can poll it directly:

```java
long staleAfterUs = 5_000_000;  // 5 s

void watchdog(DemuxReceiver rx, int pid, AtomicBoolean stop) {
    while (!stop.get()) {
        Long seen = rx.lastSeenMicros(pid);
        if (seen != null && System.currentTimeMillis() * 1_000 - seen > staleAfterUs) {
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

**Blocking caveat:** `lastSeenMicros` takes the same internal resource lock a
parked `next()` call holds, so on a receiver whose consumer thread is
currently blocked in `next()` waiting on a fully-quiet stream, the watchdog's
call blocks too — indefinitely, until an item finally arrives (or the
consumer's own deadline, if any, expires). A separate watchdog thread is
therefore not truly independent of the consumer's recv cadence. Two ways to
keep it responsive: poll `lastSeenMicros` from the *consumer* thread itself,
between `next()` calls, rather than from a separate thread; or pair it with
`?recv_timeout=<ms>` on the receiver URL (see [Recv deadlines](#recv-deadlines-recv_timeout--per-call-overloads)
below) so `next()` returns — and releases the lock — on its own cadence even
when the stream is silent, letting a separate watchdog thread's poll go
through between those returns.

The same method exists on `org.tstrans.srt.DemuxReceiver` and
`org.tstrans.srt.ManagedDemuxReceiver` (same blocking caveat).

### Recv deadlines (`?recv_timeout=` / per-call overloads)

Both `Receiver.recv()` and `DemuxReceiver` iteration block indefinitely by
default — fine for a live camera, less fine for a quiet socket you want to
notice going quiet. `?recv_timeout=<ms>` on a `rtp://` (or `rtsp(s)://`) URL
arms a persistent receive deadline: a `recv()` / `recvEvent()` call that
would otherwise block forever instead throws `RtpException(TIMEOUT)` after
`<ms>` milliseconds of silence, and the receiver stays open — call again to
keep waiting:

```java
import org.tstrans.RtpException;
import org.tstrans.rtp.DemuxReceiver;
import org.tstrans.mpegts.DemuxEvent;

try (DemuxReceiver rx = DemuxReceiver.fromUrl("rtp://0.0.0.0:5004?recv_timeout=5000")) {
    while (true) {
        DemuxEvent event;
        try {
            event = rx.recvEvent();
        } catch (RtpException e) {
            if (e.kind() == RtpException.Kind.TIMEOUT) {
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
`TIMEOUT` is retryable — the transport and session are both still alive,
unlike `CANCELLED` or `TRANSPORT`. Use `recvEvent()`, not the `Iterator`
(`for (var event : rx)`), when you need to catch `TIMEOUT` as a checked
exception — the iterator wraps it in an unchecked `RuntimeException`.

## RTSP client (`org.tstrans.rtp`)

Connect to an RTSP server, drive OPTIONS/DESCRIBE/SETUP/PLAY, and demux the RTP
data plane. Mirrors tst-py's `tstrans.rtp` RTSP-client surface.

```java
import org.tstrans.rtp.*;
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
  `password` accessor; `toString()` redacts it. The credential is handed to the
  native connect and wrapped in Rust `secrecy::SecretString`.
- **`auth` is `Object`.** Java has no union type; `RtspClientConfig.auth()` returns
  `Optional<Object>` that is a `BasicAuth` or `DigestAuth` — match with `instanceof`.
- **Cancellation.** Obtain a `RtspCancelHandle` from `session.cancelHandle()` BEFORE
  a blocking control call; flip `cancel()` from another thread to break it out.
  `close()` is a best-effort teardown, not a cross-thread interruptor.
- **TLS (`rtsps://`) is supported.** The binding links rustls; an `rtsps://`
  URL negotiates TLS on connect. `tlsRootCertsPem` supplies a PEM bundle of
  custom trust anchors (private-CA cameras) — without it the handshake
  verifies against platform native roots. Invalid PEM, a rejected anchor, or
  an empty bundle throw `RtspException` of kind `TLS` before any network I/O.
- **Pass-through config fields.** `transportPref` and `rtspVersion` are informational
  here — the underlying connect derives transport (from a `?transport=udp|tcp` query)
  and version (from the URL scheme) from the URL, not from these fields. Likewise
  `DigestAuth.algorithm` is retained for introspection only; the server's
  `WWW-Authenticate` challenge selects the actual digest algorithm.
- **`stats()`** returns a zeroed `RtspStats` for now (RTCP counters wire in later).
- **Why did the stream end?** When a `for (var event : rx)` loop exits or a
  call throws, `rx.endReason()` (on `Receiver`, `DemuxReceiver`, and
  `H264Receiver` alike) answers with a `StreamEndReason` member —
  `CLEAN_TEARDOWN`, `SESSION_EXPIRED`, `KEEPALIVE_FAILED`,
  `TRANSPORT_FAILED`, `PROTOCOL_ERROR`, or `CANCELLED` — or `null` if the
  session hasn't ended yet (or ended through a path this arc doesn't
  instrument, e.g. a plain `rtp://` receiver with no owning `RtspClient`).
  `rx.endDetail()` carries the free-text message for the three failure
  variants. Both stay readable after `close()` — the receiver snapshots
  them at close time, before the underlying native handle is freed. Set
  `TSTRANS_LOG=tst_rtp=debug` before the first `System.load` of the
  native library (i.e. before touching any `org.tstrans.*` class) to
  also see the underlying pump/keepalive `tracing` events on stderr.
  Full recipe + the Rust-side rationale:
  [Why did my RTSP stream end?](/docs/troubleshooting.md#why-did-my-rtsp-stream-end).

## RTSP server (`org.tstrans.rtp`)

Host an RTSP server, register unicast or multicast mounts, and push elementary
streams to all connected clients via the `MountHandle` push family. Mirrors
tst-py's `tstrans.rtp.RtspServer`.

```java
import org.tstrans.rtp.*;
import org.tstrans.RtspException;
import org.tstrans.mpegts.MuxerConfig;
import org.tstrans.mpegts.VideoCodec;

MuxerConfig program = MuxerConfig.builder()
    .programNumber(1).pmtPid(0x1000)
    .addVideo(0x1011, VideoCodec.H264)
    .build();

var cfg = RtspServerConfig.of("0.0.0.0:8554");   // defaults: maxSessions=100, etc.

try (RtspServer server = RtspServer.start(cfg);
     MountHandle mount = server.addUnicastMount("/live", program)) {

    // Push frames to all connected RTSP clients.
    // pts is a 90 kHz tick count; keyFrame marks a random-access point.
    mount.pushVideo(annexBNal, /*pts=*/ 0L, /*keyFrame=*/ true);
    mount.pushKlv(klvBytes, /*pts=*/ 0L, /*metadataServiceId=*/ 1);

    // server.localAddr() returns the bound "ip:port" (useful when port 0 is used).
}
// close() performs a graceful Notice 5402 teardown of active sessions, then
// frees the native server. MountHandle.close() frees only the handle wrapper —
// the mount continues to serve until the server itself is closed.
```

- **`RtspServerConfig.of(bindAddr)`** is the one-liner constructor (all other
  fields take tst-py defaults: `maxSessions=100`, `sessionTimeoutSecs=60`,
  `fanoutCapacity=256`, `gracefulShutdownDrainMs=2000`). Use
  `RtspServerConfig.builder()` to tune individual fields.
- **Mount errors are `RtspException(MOUNT)`.** All `pushVideo`/`pushKlv`/
  `pushAudio`/`pushSubtitle` calls on `MountHandle` throw `RtspException` of kind
  `MOUNT` on failure (e.g. invalid config, server already stopped). This differs
  from `MuxSender`, which throws `MuxException`.
- **No data push family yet on `MountHandle`.** `MountHandle` does not expose
  `pushData` / `pushDataTo` — a recorded follow-up. Private-data streams on
  the RTSP path currently push through the offline `Muxer` / `MuxerFileSink`.
  The srt `ManagedMuxSender` and the plain srt / rtp `MuxSender`s do expose
  the full `sendData` / `sendDataTo` family.
- **`MountHandle` is `Arc`-backed and thread-safe** on the push path (`&self`
  internally). Multiple producer threads may call `push*` concurrently. Do not
  race `close()` against a concurrent push — coordinate closes at the producer
  boundary.
- **`MountHandle.close()` unregisters only the handle wrapper.** The mount
  itself stays live in the server (still accepts new connections and fans out to
  existing ones) until `RtspServer.close()` / `stop()` is called. If you need to
  remove a mount while the server runs, stop pushing and let connected sessions
  drain naturally.
- **Hard-cancel.** `server.cancelHandle()` returns a cross-thread
  `RtspServerCancelHandle`; call `cancel()` to tear down the server immediately
  without the graceful drain window.
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

## H.264-over-RTP ingest (RFC 6184) (`org.tstrans.rtp`)

`org.tstrans.rtp.H264Receiver` ingests bare H.264 elementary streams over
RTP/RTSP (RFC 6184 — single-NALU, STAP-A, FU-A; packetization modes 0 and 1).
The canonical path for an RTSP camera uses `RtspClient.connectH264`:

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
                // au.pts() is a 90 kHz decode-order timestamp — same clock
                // as MPEG-TS PTS, no rescaling needed.
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
| `H264Receiver` | `listen(String url)` / `listen(String url, H264DepayConfig cfg)` for direct UDP. `recvAu()` → `H264AccessUnit \| null`. `implements AutoCloseable, Iterable<H264AccessUnit>`. |
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
session (control connection, keepalives, teardown on `close()`). By contrast,
`intoDemuxReceiver()` leases the session handle (a sanctioned borrow — only the
data-plane transport moves into the receiver), so the control plane remains
usable after the call.

**Failure path note.** If `intoDemuxReceiver()` previously consumed the data
plane, a subsequent `intoH264Receiver()` still consumes and tears down the
session (the native returns `PROTOCOL`) — a live `DemuxReceiver` on it will
reach EOS on its next iteration. Ensure only one `into*` call is made per
session.

**`socket_stats()` is not Optional.** `H264Receiver.socketStats()` returns a
bare `SocketStats` (never null), whereas the SRT `Receiver.socketStats()`
returns `Optional<SocketStats>`. This matches the Rust
`H264Receiver::socket_stats()` → `SocketStats` signature directly, which has
no "no socket" code path once constructed.

### Integration notes

- **`sprop-parameter-sets` handled automatically.** `connectH264` decodes the
  SDP `a=fmtp` attribute and stores SPS/PPS NALUs in the `H264DepayConfig`
  stashed inside the session. With `ParameterSetInjection.BEFORE_IDR` (the
  default), the depacketizer prepends them before every IDR frame.

- **B-frame / DTS limitation.** `au.pts()` is derived from the RTP timestamp
  (decode order). For live cameras without B-frames, PTS = DTS. For
  B-frame content, supply DTS separately to `mux.pushVideoToWithDts`.

- **Loss behavior.** A sequence-number gap drops the open AU and increments
  `rx.depayStats().ausDropped()` and `.seqGaps()`. Loss is whole-AU only.

- **KLV pairing slot.** For a STANAG 4609 gateway, push KLV using
  `mux.pushKlv(klvBytes, /*pts=*/ au.pts(), /*metadataServiceId=*/ 0x00)`.

- **RTCP is not implemented on the H.264 path (v1 decision).** No RTCP
  socket is bound; no RR/SR is sent or received.

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
    new PairerConfig(new PairerMode.Realtime(), Duration.ofMillis(100), 32, 32, true),
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
(Realtime mode, 300 ms tolerance, 32/32 buffers, `linkKlvToVideo = true`) with
demuxer defaults. To tolerate arrival skew, switch to Buffered mode: pass
`new PairerMode.Buffered(Duration.ofMillis(200))` as the mode; `flush()` becomes
load-bearing at end-of-stream because buffered samples are held until the lag window
closes.

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
  It is most load-bearing in Buffered mode, but skipping it in Realtime can still
  drop tail metadata.
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
- **`codec` lives on `StreamId.kind()`, not on the event record.** A
  `Video` event does not carry its codec directly; read it from the stream:
  `((StreamKind.Video) v.stream().kind()).codec()`. The event records
  intentionally don't duplicate the codec.
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
  pump warnings, …) that otherwise vanish silently — nothing installs a
  subscriber by default. The bridge installs from `JNI_OnLoad`, the JVM's
  one guaranteed one-time native-library entry point (called during
  `System.load`, before any `org.tstrans.*` native method is reachable).
  Unset means zero overhead beyond the one env lookup at load time; if
  the host process already installed its own `tracing` subscriber, this
  bridge never displaces it.

## Where this binding differs from the Rust core

- **Demux + offline mux + typed KLV + codec parsers + file I/O + SRT +
  RTP transports shipped.**
  The JVM binding surfaces the `org.tstrans.mpegts.Demuxer` receive path
  (feed bytes → typed `DemuxEvent`s with typed NAL / OBU / ADTS payloads),
  the offline `org.tstrans.mpegts.Muxer` send path (config builder → push
  family → `pull` / `writeFile`), the full `org.tstrans.klv` typed-KLV
  surface (ST 0601 / 0102 / 0605 / 0903 decode + encode + `parseUniversal`
  dispatcher, plus ST 0806 RVT, ST 1010 SDCC-FLP, and the ST 0805
  KLV → CoT conversion layer), the `org.tstrans.codec` elementary-stream parsers (H.264 /
  H.265 / H.266 / AV1 / AAC / MPEG-2 audio), the `org.tstrans.io` file
  helpers (`parseFile`, `probe`, `extractKlv`), the `org.tstrans.srt` SRT
  transport surface (`Sender`/`Receiver` pipeline shells + the low-level
  `Builder`/`Socket`/`Listener`/`CancelHandle` + the `MuxSender`/
  `DemuxReceiver` convenience shells + the `Managed*` reconnect family),
  the `org.tstrans.rtp` RTP transport surface (`Sender`/`Receiver` +
  `MuxSender`/`DemuxReceiver` + RTSP client/server), the
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
  off the Rust `StreamEndReason` value (same approach as Python). A
  recorded end reason is data, not a failure, so it never touches
  `RtpException`.
- **`lastSeenMicros(pid)` returns `null`, not `0`, for "never
  seen".** The C ABI has no nullable type in its getter shape, so it
  uses a `0` sentinel; this binding's boxed `Long` `null` is the honest
  absent value (same convention as Python's `None`).

The Rust page's "Where this binding differs from the Rust core" section
treats Rust as the canonical surface; everything here is a subset of it.
See [`/docs/languages/rust.md`](/docs/languages/rust.md) for the full
surface and [`/docs/languages/python.md`](/docs/languages/python.md) for the
Python binding's gaps.

## Roadmap

- **Bootstrap (`org.tstrans.Version`) — SHIPPED.** Proves the
  cargo → cdylib → Gradle → Java → JNI build pipeline and native loader.
- **mpegts demux (`org.tstrans.mpegts.Demuxer` + `DemuxEvent` + `DemuxerConfig`) — SHIPPED.**
- **mpegts mux (`org.tstrans.mpegts.Muxer` + `MuxerConfig` + push family + `pull`) — SHIPPED.**
- **klv** — typed KLV decode/encode (ST 0601 / 0102 / 0605 / 0903, plus ST 0806 RVT, ST 1010 SDCC-FLP, and the ST 0805 KLV → CoT conversion layer) under `org.tstrans.klv` — **SHIPPED.**
- **codec** — H.264 / H.265 / H.266 / AV1 + audio parsers under
  `org.tstrans.codec`; typed elementary-stream payloads (NAL / OBU / ADTS) — **SHIPPED.**
- **io** — file inspection helpers (`Io.parseFile`, `probe`, `extractKlv`, `Muxer.writeFile`) — **SHIPPED.**
- **srt (sub-wave A)** — `Sender` / `Receiver` pipeline shells + `Builder` /
  `Socket` / `Listener` / `CancelHandle` / `SocketStats` / `SrtStats` — **SHIPPED.**
- **srt (sub-wave B)** — `MuxSender` / `DemuxReceiver` high-level shells +
  `DemuxReceiver.addByteSink` fan-out + the `ReconnectPolicy` /
  `BackoffStrategy` / `OverflowPolicy` types — **SHIPPED.**
- **srt (sub-wave C)** — `Managed*` reconnect wrappers — **SHIPPED.**
- **rtp** — MPEG-TS-over-RTP transport + `MuxSender` / `DemuxReceiver` +
  RTSP client / server — **SHIPPED.**
- **pipeline** — `org.tstrans.pipeline.Pairer` pairing shell — **SHIPPED.**
- **multi-platform fat JAR + Maven Central publish** — single JAR bundling
  linux-x86_64 / linux-aarch64 / macos-arm64 / windows-x86_64
  native libraries, published as `org.tstrans:tstrans-jvm` — **SHIPPED
  (v0.1.0).**

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
