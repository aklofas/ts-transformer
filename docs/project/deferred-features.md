# Deferred features

Things deliberately out of scope today with a clear path back if they
become load-bearing. Each entry records the reason it was deferred and
the trigger that would unblock it.

## HLS publisher — SUPPORTED

- **Status:** SUPPORTED. The HLS publisher ships in its own segmenter-first
  `tst-hls` crate. The Python wheels enable it by default (`tstrans.hls`
  imports out of the box); the C binding exposes it behind the opt-in `hls`
  Cargo feature (`TST_HAS_HLS`). It is no longer gated out of published
  artifacts. The security items that previously blocked promotion are
  closed:
  - **Path traversal (CWE-22)** is closed — the built-in HTTP server serves
    only files from a known set it wrote itself; request paths are not used
    to open arbitrary files under the output directory.
  - **VOD / EVENT serving** is closed — `HlsPublisher::finish_serving`
    returns an `HlsServerHandle` that keeps the built-in server up so a
    completed VOD or EVENT playlist and its segments stay fetchable after
    the stream ends.
  - **Bind default is loopback** (`127.0.0.1:8080`); binding all interfaces
    is now an explicit opt-in, and the guide points operators at fronting
    the output directory with a reverse proxy / CDN for exposure.
  - Segments open on a decodable boundary (PAT → PMT → IDR), so a joining
    player can decode the first segment it fetches.
- See the [HLS guide](/docs/guides/hls.md) for the full surface, serving
  guidance, KLV ride-along, and latency tuning. The JVM binding does not
  yet expose HLS (see below).

## HLS fMP4 / CMAF segments + emsg-v1 KLV (MISB ST 1910.1)

- **Status:** Not implemented. `tst-hls` emits MPEG-2 TS (`.ts`) segments
  only. Fragmented-MP4 / CMAF (`.m4s`) segmentation, and carrying KLV in
  CMAF `emsg` v1 boxes per MISB ST 1910.1 (scheme
  `urn:misb:KLV:bin:1910.1`), are not done.
- **Why deferred:** TS segments are the STANAG 4609 lingua franca and ride
  through the existing MPEG-TS muxer unchanged. CMAF/fMP4 is a distinct
  packaging path (init segment + moof/mdat framing) and the emsg-v1 KLV
  binding is a separate wire shape — real work with no current consumer
  driving it. TS-in-HLS covers the shipping use case, including KLV
  ride-along.
- **Trigger to revisit:** Low-latency / LL-HLS demand (CMAF is the natural
  carrier), or a consumer that needs Safari-native timed metadata (Safari
  surfaces `emsg` KLV where it does not surface TS private-data KLV).

## LL-HLS protocol (EXT-X-PART / blocking playlist reload / preload hints)

- **Status:** Not implemented. `tst-hls` writes standard RFC 8216 playlists
  (full-segment granularity). The LL-HLS additions — partial segments
  (`#EXT-X-PART`), blocking playlist reload (`_HLS_msn` / `_HLS_part`),
  preload hints (`#EXT-X-PRELOAD-HINT`), and rendition reports — are absent.
- **Why deferred:** LL-HLS meaningfully lowers glass-to-glass latency only
  with a CMAF/fMP4 packaging path (partial segments are typically CMAF
  chunks), so it couples to the fMP4 entry above. The internal
  renderer/dispatcher bones (segmenter, playlist renderer, HTTP dispatcher)
  are in place, so adding the LL-HLS machinery is bounded once the packaging
  path exists.
- **Trigger to revisit:** A consumer needs sub-two-second HLS latency, or
  the fMP4/CMAF path lands (LL-HLS rides with it).

## ID3v2-wrapped KLV (timed-metadata ID3 frames)

- **Status:** Not implemented. KLV rides HLS segments as an MPEG-TS
  elementary stream (`PrivateData` / stream_type 0x06 + KLVA, or
  `SynchronousMetadata` / 0x15). Wrapping KLV in ID3v2 `PRIV`/`GEOB` frames
  on a timed-metadata PID — the shape some web players expect via the
  ID3 event path — is not emitted.
- **Why deferred:** The native private-data KLV path already reaches
  hls.js (`enableEmsgKLVMetadata`) and STANAG toolchains; ID3-wrapping is
  an extra encoding with no current consumer. It is additive, not a
  replacement.
- **Trigger to revisit:** A target player exposes timed metadata only via
  ID3 frames (no raw private-data / emsg path).

## JVM HLS surface

- **Status:** Not implemented. The HLS publisher is exposed in Rust, C
  (`TST_HAS_HLS`), and Python (wheels ship it), but the JVM binding
  (`tst-jni`) carries no HLS dependency and exposes no `HlsPublisher` /
  `MuxPublisher` classes.
- **Why deferred:** No JVM consumer has asked. The publisher surface maps
  mechanically onto the JNI patterns already used for the mux-sender family
  (builder + handle + `finish_serving` handle), so the port is bounded when
  a consumer arrives.
- **Trigger to revisit:** A JVM consumer asks for HLS output.

## In-memory HLS `SegmentSink`

- **Status:** Not implemented. `tst-hls` writes segments and the playlist
  to a filesystem `output_dir`. There is no pluggable sink to receive
  segment bytes + playlist in memory (for an object store, an embedded
  no-filesystem target, or a custom uploader) instead of touching disk.
- **Why deferred:** The filesystem sink plus the built-in server (or a
  reverse-proxied `output_dir`) covers the shipping deployments. A
  `SegmentSink` trait is a clean extension point but carries no weight
  without a driving consumer.
- **Trigger to revisit:** An embedded / no-filesystem consumer, or one that
  wants to push segments straight to an object store / CDN origin without a
  disk round-trip.

## Other PMT entries / auxiliary services

- **Status:** SCTE-35 splice info, EMM/ECM (conditional access), and
  data carousels (DSM-CC) are not emitted as typed services. Arbitrary
  private-data PES streams (PMT entry with a caller-chosen
  `stream_type` + descriptors, pass-through payload) are now supported
  via `StreamSpec::Data` / `add_data`.
- **Why deferred:** No shipping consumer asks for any of them. Adding
  them speculatively risks the same wrong-abstraction trap as
  subtitles — each is its own descriptor + stream_type + PES shape.
- **Trigger to revisit:** A consumer asks for one specifically.
- **Scope when added:** Case-by-case; each carries its own
  descriptor + stream_type + PES framing.

## Async / reactor exposure

- **Status:** Not implemented; the public API is sync blocking.
- **Why deferred:** Consumers fit ten or fewer SRT connections per
  process; thread-per-connection is straightforward and matches
  `std::net::TcpStream` semantics. A reactor-backed surface is
  significantly more design work than the current consumer base
  justifies.
- **Trigger to revisit:** A consumer needing fifty or more concurrent
  connections, or a binding (UniFFI, JNI) that explicitly wants async
  at the FFI surface.

## Bonding / connection groups (`SRTO_GROUP*`)

- **Status:** Not implemented.
- **Why deferred:** SRT 1.5 supports caller-side bonding for redundant
  uplinks; no shipping consumer uses it today. The bonding API surface
  is large, and a half-finished wrap is worse than no wrap at all.
- **Trigger to revisit:** A dual-radio uplink that wants redundancy at
  the SRT layer rather than at VPN / MPUDP.

## Key rotation (`SRTO_KMREFRESHRATE`, `SRTO_KMPREANNOUNCE`)

- **Status:** Not implemented; AES-CTR uses the static key derived
  from the passphrase for the lifetime of the connection.
- **Why deferred:** Shipping streams are typically minutes to hours;
  static-key AES-CTR is fine across that duration.
- **Trigger to revisit:** A 24/7 unattended stream, or a compliance
  regime that requires periodic rekey.

## Protocol-version pinning (`SRTO_PEERVERSION`, `SRTO_MINVERSION`)

- **Status:** Not exposed.
- **Why deferred:** libsrt 1.5.7 negotiates with anything 1.3 or
  newer. No current consumer needs to refuse older peers.
- **Trigger to revisit:** Integration with a peer that has a known
  protocol bug below some version.

## Typed packet-filter / FEC builder

- **Status:** Not implemented; spec strings pass through verbatim via
  `PacketFilter::new("fec,cols:N,rows:M,arq:onreq")`.
- **Why deferred:** The raw-string surface is small, well-documented
  by libsrt, and unlikely to grow. A typed builder roughly doubles the
  surface area for marginal benefit over the string form.
- **Trigger to revisit:** libsrt adds a filter type that's hard to
  compose by string.

## Stream-ID filtering on `Listener`

- **Status:** Not implemented; `Listener::accept` returns every
  successful handshake.
- **Why deferred:** Filter shape is application policy (regex? exact
  list? signed token?), not transport policy. The library exposes
  `socket.stream_id()` post-`accept` so the caller's accept loop
  decides whether to keep the connection.
- **Trigger to revisit:** A common abstraction emerges across multiple
  consumers.

## Custom congestion controller selection

- **Status:** `Live` (default) and `File` only.
- **Why deferred:** libsrt's `Live` is the right answer for live
  video. Plugging custom controllers via libsrt's C-callback
  registration is awkward research-tier work.
- **Trigger to revisit:** A research collaboration produces a
  controller that empirically beats `Live` for our workload.

## ST 0102 universal-set form

- **Status:** Deferred. The Local Set form ships in `klv::st0102`
  (decode + decode_strict + encode); the parallel Universal Set form
  (16-byte UL per item, separate from the LS encoding) is not
  implemented.
- **Why deferred:** LS form is the only form on MPEG-TS+KLV streams.
  The Universal Set is for archival / file-based use cases the library
  does not target.
- **Trigger to revisit:** A consumer ingesting archival / file-based
  ST 0102-bearing streams that use the Universal Set encoding.

## ST 0102 country-code validation

- **Status:** Deferred. `klv::st0102` decodes the country coding
  method (Tags 2 / 12) as a typed enum but the country codes
  themselves (Tags 3 / 6 / 13) pass through as `String` verbatim. No
  validation against ISO 3166 / GENC / FIPS 10-4 / STANAG 1059 /
  CAPCO tables.
- **Why deferred:** Tables are large (GENC alone has 250+ codes plus
  admin subdivisions plus version dates plus deprecations),
  version-dependent, and a moving target across spec revisions.
  Pass-through strings sidestep the maintenance burden.
- **Trigger to revisit:** A compliance pipeline that requires
  validating codes against authoritative tables, AND a clear answer
  for which spec revision's table to bake in.

## Typed nested VMTI Local Sets (`VMask`, `VObject`, `VFeature`, `VTracker`, `VChip`)

- **Status:** Pass-through. The five LSes inside each `VTargetPack` —
  `vmask`, `vtracker`, `vchip`, `vchip_series`, `vobject_series` — are
  `Option<Vec<u8>>` raw bytes today.
- **Why deferred:** The structural per-target slice (target ID,
  centroid, bbox, lat/lon, dimensions, color, intensity, detection
  status, algorithm ID, etc.) covers the load-bearing analyst use
  case. Each nested LS is its own per-tag table to write and maintain
  — without a consumer asking for typed access, the table is carrying
  weight without paying for itself.
- **Trigger to revisit:** A consumer asks for typed access to per-
  target classification (VObjectSeries), feature vectors (deferred —
  ST 0903.6 deprecated the VFeature LS at Tag 103), track state
  (VTracker), pixel masks (VMask), or image cutouts (VChip /
  VChipSeries).

## Typed VMTI Algorithm + Ontology Series

- **Status:** Pass-through. `VmtiLs.algorithm_series` and
  `VmtiLs.ontology_series` are `Option<Vec<u8>>` raw bytes today.
- **Why deferred:** Same reasoning as the nested-LS entry above —
  per-tag tables without a driving consumer ask. Algorithm describes
  detector/tracker provenance; Ontology describes class label
  hierarchy.
- **Trigger to revisit:** A consumer asks for typed algorithm
  provenance or class-label hierarchy.

## VMTI standalone-PID demuxer dispatch (`MetadataKind::VmtiLs`)

- **Status:** Consumer-side dispatch. Consumers carrying VMTI on its
  own KLV PID match `data.starts_with(&klv::st0903::VMTI_LS_UL)`
  themselves and call `klv::st0903::decode` on the inner bytes (after
  stripping the 16-byte UL prefix and reading the BER outer length).
  The demuxer's `MetadataKind` enum has no VMTI-aware variant.
- **Why deferred:** Adding `MetadataKind::VmtiLs` to the demuxer event
  surface makes the demuxer typed-set-aware — and that's a slippery
  slope (do we then add `MetadataKind::SecurityLs`,
  `MetadataKind::Ais`, ...?). Today's pattern keeps the demuxer UL-
  agnostic and pushes dispatch to consumer code, which is where the
  typed-set decision naturally lives.
- **Trigger to revisit:** A consumer with VMTI on its own KLV PID
  asks for ergonomic dispatch, AND we're prepared to commit to a
  `MetadataKind::*` policy across all typed sets.

## VMTI Universal Set form

- **Status:** Local Set form ships in `klv::st0903` (decode +
  decode_strict + encode); the parallel Universal Set form (16-byte UL
  per item, separate from the LS encoding) is not implemented.
- **Why deferred:** LS form is the only form on MPEG-TS+KLV streams.
  The Universal Set is for archival / file-based use cases the library
  does not target.
- **Trigger to revisit:** A consumer ingesting archival / file-based
  VMTI-bearing streams that use the Universal Set encoding.

## Typed ST 0601 nested local sets (ST 1206 / ST 1002 / ST 1601 / ST 1602 / ST 1607 Segment + Amend)

- **Status:** Pass-through. The six remaining named-but-opaque nested
  Local Sets on `UasDatalinkLs` are `Option<Vec<u8>>` raw bytes today:
  tag 95 (`sar_mi_local_set`, ST 1206 SAR Motion Imagery), tag 97
  (`range_image_local_set`, ST 1002 Range Image), tag 98
  (`geo_registration_local_set`, ST 1601 Geo-Registration), tag 99
  (`composite_imaging_local_set`, ST 1602 Composite Imaging), and
  tags 100–101 (`segment_local_set` / `amend_local_set`, ST 1607
  Segment and Amend). Tag 73 (RVT / ST 0806) is no longer on this
  list — see the `klv::st0806` typed-layer entry below.
- **Why deferred:** Same reasoning as the VMTI nested-LS entry above —
  the substrate supports typing each one, but every set is its own
  per-tag table to write and maintain, and no consumer has asked for
  typed access to any of the six.
- **Trigger to revisit:** A consumer asks for typed access to SAR
  motion imagery, range imagery, geo-registration, composite imaging,
  or segment/amend metadata.

## `klv::st0806` RVT typed layer — SHIPPED

- **Status:** SHIPPED. `klv::st0806` types the MISB ST 0806.4 Remote
  Video Terminal (RVT) Local Set: the nested body form (carried via
  `UasDatalinkLs::rvt`, ST 0601 Tag 73) and the standalone independent
  form (own 16-byte UL, timestamp-first Tag 2, CRC-32/MPEG-2-last
  Tag 1). The repeatable nested POI (Tag 12), AOI (Tag 13), and User
  Defined (Tag 11) sub-sets are typed too. Decode via
  `klv::st0806::decode` (nested) or `klv::st0806::decode_standalone`
  (independent). See the `klv::st0806` module docs for the full
  surface.

## RVT standalone-PID demuxer dispatch (`MetadataKind::RvtLs`)

- **Status:** Consumer-side dispatch. Consumers carrying a standalone
  RVT LS on its own KLV PID (the ST 0806.4-01/-03 independent form)
  match `data.starts_with(&klv::st0806::RVT_LS_UL.0)` themselves and call
  `klv::st0806::decode_standalone` on the inner bytes. The demuxer's
  `MetadataKind` enum has no RVT-aware variant — same shape as the
  VMTI standalone-PID entry above.
- **Why deferred:** Same slippery-slope reasoning as the VMTI entry:
  adding `MetadataKind::RvtLs` commits the demuxer to a growing per-
  typed-set variant list. Today's pattern keeps the demuxer UL-agnostic
  and pushes dispatch to consumer code, which is where the typed-set
  decision naturally lives.
- **Trigger to revisit:** A consumer with standalone-RVT streams (own
  KLV PID, ST 0806.4-01/-03 form) asks for ergonomic dispatch, AND
  we're prepared to commit to a `MetadataKind::*` policy across all
  typed sets.

## KLV conformance cross-check vs. Python `klvdata`

- **Status:** Default test suite uses synthetic + MISB public fixtures
  as ground truth. No automated cross-decoder agreement check.
- **Why deferred:** Adding Python to CI is marginal value when MISB
  public test vectors are already authoritative ground truth.
- **Trigger to revisit:** A parsing bug ships that golden-file tests
  would have missed — i.e., the library and the spec disagreed without
  the test suite catching it.
- **Scope when added:** A `--features conformance` cargo feature plus
  a separate CI job that runs Python with `pip install klvdata`,
  parses each fixture with both decoders, and asserts typed-field
  agreement within tolerance.

## Streaming / chunked KLV decode

- **Status:** Buffer-in / buffer-out. The decoder consumes a complete
  KLV LS in one call.
- **Why deferred:** ST 0601 records are sub-1 KB typical and sub-10 KB
  worst case. A streaming decoder is implementation cost without a
  beneficiary.
- **Trigger to revisit:** Implausible — would require a consumer with
  records over 100 KB.

## `serde` integration for typed KLV records

- **Status:** Not implemented.
- **Why deferred:** Wire format and JSON aren't isomorphic — JSON
  would carry the typed shape but lose unknown-tag pass-through, which
  is the whole point of the ST 0107.5 future-proof skip rule.
- **Trigger to revisit:** A consumer wants typed records as JSON for
  external tooling, with an explicit decision on how unknown tags are
  represented.

## `no_std` support for `klv`

- **Status:** Shipped 2026-05-30 (tst-core no_std baseline). `pub mod
  klv;` (`crates/tst-core/src/lib.rs:73`) carries no `std` feature
  gate; `no-std-baremetal.sh` compiles it for both
  `thumbv7em-none-eabihf` and `riscv32imac-unknown-none-elf` alongside
  the rest of `tst-core`. Residual std-only corners: none — the only
  two `#[cfg(not(feature = "std"))]` sites under `klv/` (`imapb.rs`,
  `st0601/mapping.rs`) are the `no_std` *enabling* code itself (routing
  float ops through `float_ext::FloatExt`'s `libm` shim in place of
  `std::f64` methods), not a remaining std requirement.

## Multi-stream `mpegts::mux` — `tst-jni` / `tst-uniffi` binding surface

- **Status:** The `tst-c` C ABI fan-out shipped — `tst_video_stream_handle_t` /
  `tst_klv_stream_handle_t` typedefs, `tst_mux_config_add_video_stream` /
  `_add_klv_stream` returning handles, and `_video_to(handle, ...)` /
  `_klv_to(handle, ...)` siblings on `tst_muxer_t`, `tst_mux_sender_t`,
  and `tst_managed_mux_sender_t`. The single-target entry points keep
  their original signatures and surface `MuxError::AmbiguousTarget` as
  `TST_E_INVALID_USAGE` on multi-stream muxers. The same handle-aware
  shape has NOT yet landed in `tst-jni` or `tst-uniffi`.
- **Note on Sender / RawSender:** the original deferred-features entry
  said `tst_ts_sender_*` / `tst_managed_ts_sender_*` would also gain
  `_video_to` / `_klv_to` siblings. That was wrong: `tst_pipeline::Sender`
  exposes only `send_ts(bytes)` (pre-muxed TS bytes) and `tst_pipeline::RawSender`
  exposes only `send(bytes)`. Neither carries a `Muxer`, so handle-aware
  fan-out is meaningless on those variants. Only the three muxer-owning
  C variants (`tst_muxer_t`, `tst_mux_sender_t`, `tst_managed_mux_sender_t`)
  have the new `_to` surface.
- **Trigger to revisit:** First JNI or UniFFI consumer that actually wants
  multi-stream output. The pattern is mechanical — mirror the same
  handle-typedef + `_to(handle, ...)` fan-out across the JNI/UniFFI
  binding once each ships.

## Codec parameter set parsing at the C ABI

- **Status:** Deferred. The Rust core ships `tst_core::codec::h264` and
  `tst_core::codec::h265` with typed parsers for SPS / PPS (H.264) and
  VPS / SPS / PPS (H.265). The C ABI exposure is deferred.
- **Why deferred:** The receiver-surface C ABI plan is the natural carrier
  for all FFI parser exposure — consistent ownership and error semantics
  across receiver fields, parameter-set fields, and future audio / subtitle
  parser exposure are best designed in one pass rather than piecemeal. An
  interim send-only or codec-only C ABI shape would need reshaping when the
  receiver C ABI lands anyway.
- **Trigger to revisit:** The receiver-surface C ABI plan starts execution.
  At that point the codec parsers get C entry points alongside the receiver
  event surface, sharing the same error-reporting and lifetime conventions.

## AV1 full Frame Header parsing

- **Status:** Deferred. `codec::av1::parse_frame_header_light` ships
  surfacing `frame_type` (KEY / INTER / INTRA_ONLY / SWITCH),
  `show_frame`, and `show_existing_frame`. Full Frame Header parsing
  (reference frame management, segmentation, loop filter, film grain,
  per-frame display size) is not in scope.
- **Why deferred:** Crosses into "you want a decoder." The light
  scope covers keyframe detection — the load-bearing use case for
  metadata extraction — and the cookbook recipes route off
  `frame_type == KEY` + `show_frame` for keyframe gating today.
- **Trigger to revisit:** A consumer shipping per-frame display-time,
  per-frame aspect-ratio, or `film_grain_params` consumption.

## AV1 still-picture / AVIF detection helper

- **Status:** Deferred. `Av1SequenceHeader::still_picture` and
  `reduced_still_picture_header` are parsed and surfaced, but no
  consumer-facing helper for "is this an AVIF?" exists yet.
- **Why deferred:** AVIF-in-MPEG-TS isn't a common consumer pattern;
  no consumer ask. Callers that need it can read the two raw flags off
  the parsed Sequence Header today.
- **Trigger to revisit:** A consumer shipping AVIF over SRT.

## AV1 multi-operating-point streams

- **Status:** Deferred. Operating points beyond OP 0 are walked past
  in the Sequence Header parser but not surfaced — `Av1SequenceHeader::level`
  and `tier` reflect OP 0 only.
- **Why deferred:** Single-OP is the common live-streaming pattern.
  Multi-OP (used for scalable encodes) is rare in real-world captures
  and absent from the local corpus.
- **Trigger to revisit:** A real-world capture with multi-OP AV1
  streams, or a consumer shipping scalable AV1.

## AV1-in-MPEG-2-TS binding §3.2 / §3.4 carriage conformance

- **Status:** Shipped (validate-1 C8). Default carriage is now
  `Av1CarriageMode::Mpeg2TsBinding`: PES `stream_id = 0xBD`
  (private_stream_1, §3.4) and `ts_open_bitstream_unit()` framing
  on each OBU (3-byte `obu_start_code` = `uimsbf(24)` = `0x000001`,
  i.e. byte sequence `0x00 0x00 0x01`, + emulation prevention
  bytes, §3.2). Set `MuxerConfig::av1_carriage =
  Av1CarriageMode::InteropRawObu` (escape hatch) for ffmpeg /
  libaom / hls.js / mediamtx interop carriage.
  Demuxer-side: matching `DemuxerConfig::av1_carriage`; binding
  mode surfaces `NonConformantIssue::Av1WrongStreamId` /
  `NonConformantIssue::Av1MissingTsObuFraming` on non-conforming
  input and falls back to raw-OBU parsing in lenient mode.

## `AV1_video_descriptor` (typed PMT descriptor)

- **Status:** Deferred. The muxer auto-emits the AV01 `registration_descriptor`
  (AV1-in-MPEG-2-TS binding §2.1) but not the optional typed
  `AV1_video_descriptor` from binding §2.2.
- **Why deferred:** The registration descriptor alone is sufficient for
  receiver classification — the demuxer routes off `format_identifier =
  "AV01"` today. The typed descriptor adds metadata (profile, level,
  tier, bit depth) that consumers can recover by parsing the Sequence
  Header OBU directly via `codec::av1::parse_sequence_header`.
- **Trigger to revisit:** A consumer that strictly requires the typed
  descriptor for transport-level metadata extraction without parsing
  the elementary stream.

## H.266 APS (Adaptation Parameter Set) parsing

- **Status:** Deferred. APS NALs (type 17 PREFIX_APS_NUT, type 18
  SUFFIX_APS_NUT) pass through as untyped `NalUnit::H266 { nal_type, .. }`
  with raw RBSP payload. VPS / SPS / PPS are typed via `codec::h266`.
- **Why deferred:** APS carries ALF (Adaptive Loop Filter), LMCS
  (Luma Mapping with Chroma Scaling), and scaling-list data — all of
  it useful only for full decode, not stream-level metadata extraction.
- **Trigger to revisit:** A consumer needing typed APS access for
  decoder pipeline introspection.

## H.266 Picture Header (PH_NUT, type 19) parsing

- **Status:** Deferred. Picture Header NALs pass through as untyped
  `NalUnit::H266 { nal_type: 19, .. }` with raw RBSP payload.
- **Why deferred:** Picture Header carries per-picture flags relevant
  to the decoder pipeline (picture_output_flag, GDR fields, partitioning
  overrides) — not load-bearing for stream-level metadata.
- **Trigger to revisit:** A consumer needing per-picture state
  extraction.

## H.266 multi-layer streams (`nuh_layer_id != 0`)

- **Status:** Deferred. The `nuh_layer_id` field is parsed off every
  NAL header but parameter sets aren't tracked per-layer — `parse_parameter_sets`
  fills a single (vps_id → vps, sps_id → sps, pps_id → pps) map across
  all layers.
- **Why deferred:** Multi-layer H.266 (the VVC scalability extension)
  isn't shipped by common encoders today and isn't in the local corpus.
- **Trigger to revisit:** A consumer using H.266 with scalability
  layers (spatial, quality, or multi-view).

## H.266 `stream_type 0x32` (VVC temporal video subsets)

- **Status:** Deferred. Only `stream_type 0x33` (VVC main video stream)
  is recognized as `VideoCodec::H266` by the demuxer.
- **Why deferred:** `stream_type 0x32` is for temporal subsetting in
  scalable VVC video — a rare use case absent from the corpus.
- **Trigger to revisit:** A consumer using temporal subsetting, or a
  capture surfacing 0x32 in the corpus. Workaround today:
  `DemuxerConfig::treat_as` lets callers manually classify the PID.

## AV1 on `0x80` user-private `stream_type`

- **Status:** Deferred. Only the binding-conformant AV1 carriage —
  `stream_type = 0x06` plus AV01 `registration_descriptor` — is
  auto-classified as `VideoCodec::Av1`. Non-conformant captures using
  `stream_type = 0x80` (user-private) require manual classification
  via `DemuxerConfig::treat_as`.
- **Why deferred:** 0x80 is reserved by H.222.0 for user-private use;
  some early AV1 captures used it before the binding settled.
  `DemuxerConfig::treat_as` covers the corner case without baking a
  non-conformant default into the auto-classifier.
- **Trigger to revisit:** A real-world capture stream with `stream_type
  0x80` plus AV01 registration that needs auto-classification (rather
  than a `treat_as` hint).

## SEI parsing for video codecs

- **Status:** Deferred. SEI NALs surface as `NalUnit::H264 { nal_type: 6, .. }`
  / `NalUnit::H265 { nal_type: 39 or 40, .. }` with raw RBSP payload — the same
  pass-through treatment as non-parameter-set NALs today.
- **Why deferred:** SEI parsing would expose HDR mastering display info (SEI 137),
  content light level (SEI 144), picture timing, and recovery-point info. Each
  SEI message type is its own sub-parser. No consumer has asked for any
  specific SEI type yet.
- **Trigger to revisit:** A consumer asks for a specific SEI type — most likely
  HDR mastering display (SEI 137) or content light level (SEI 144) for an HDR
  delivery pipeline.
- **Scope when added:** Case-by-case; each SEI type is a separate parser
  function in the same `codec::h264` / `codec::h265` module namespace.

## Audio frame parsers — AAC LATM and AC-3

- **Status:** Partially shipped. The MP2 (Layer I/II/III) and AAC ADTS
  frame iterators ship in 2026-05-07 (`codec::mpegaudio` + `codec::aac`).
  An **AC-3 (ATSC A/52) syncframe parser ships** as `codec::ac3`
  (`parse_syncframe` / `Ac3SyncInfo`) — used by the muxer to derive the
  `AC-3_audio_stream_descriptor` and by the demuxer to enforce single-
  syncframe alignment. A **LOAS/LATM sync validator ships** as
  `codec::aac::latm` (`validate_latm_sync`). Still deferred: full AAC
  LATM `audioMuxElement` decode (ISO/IEC 14496-3 §1.7) and a full AC-3
  block-level frame iterator.
- **Why deferred (remaining decode):** Neither codec appears in the
  local capture corpus (zero LATM events, zero AC-3 events across 250
  files / 33 GB at plan #21 ship). Synthetic-only fixtures would be the
  validation path; we defer the deeper decode until a consumer or
  capture surfaces them so the work is driven by real-world bytes.
- **Trigger to revisit:** A consumer ships a stream needing full LATM
  `audioMuxElement` decode or AC-3 block-level typed frame access, or a
  corpus capture surfaces either need.
- **Scope when added:** the deeper decode extends the existing
  `codec::aac::latm` / `codec::ac3` modules, following the same
  iterator-of-`Result<Frame, CodecParseError>` shape as the existing
  ADTS / MPEG-audio slices.

## Audio carriage at the `tst-c` C ABI

- **Status:** Deferred (no consumer ask). Audio carriage in `mpegts::mux`
  and `mpegts::demux` ships in Rust (codec scope: MP2 + AAC ADTS + AAC
  LATM + AC-3, plus `DemuxerConfig::treat_as` for non-conformant
  stream_type cases). The `tst-c` C ABI sender surface currently exposes
  `tst_mux_sender_send_video` and `tst_mux_sender_send_klv` but no
  `tst_*_send_audio` / `tst_*_send_audio_to` siblings, and the config
  builders do not expose `tst_mux_config_add_audio_stream` /
  `tst_audio_stream_handle_t`.
- **Why deferred:** Adding the entries is mechanical (parallel to the
  existing video / KLV send entries) but requires deciding the audio
  frame envelope shape at the C boundary — whether to take raw access
  units, ADTS frames, LATM blocks, etc., and how to surface the codec
  selection per stream. No consumer has asked.
- **Trigger to revisit:** A binding-author asks for audio send through
  the C ABI; a downstream consumer needs in-band audio for a use case
  not served by the Rust API.

## Non-ATSC AC-3 variants (E-AC-3, DVB-shaped AC-3)

- **Status:** Deferred. `mpegts::mux` emits and `mpegts::demux` recognizes
  ATSC-shaped AC-3 only — `stream_type 0x81`, with `format_identifier =
  "AC-3"` in a registration descriptor (the shape ffmpeg's mpegts muxer
  emits by default). E-AC-3 (`stream_type 0x87`) and DVB-shaped AC-3
  (`stream_type 0x06` + AC-3 registration descriptor) are not classified
  as `AudioCodec::Ac3` automatically.
- **Why deferred:** Neither variant appears in the local corpus. Adding
  them means either (a) parsing registration descriptors on every
  `stream_type 0x06` PID to disambiguate "AC-3" from "KLVA" / "HDMV" /
  etc. (a structural complication that isn't justified without a corpus
  signal), or (b) adding a new `AudioCodec::EAc3` variant plus the
  corresponding stream_type byte / muxer / demuxer plumbing.
- **Workaround:** `DemuxerConfig::treat_as` lets callers map an
  `Unknown(0x87)` PID or a `0x06` PID with the AC-3 registration
  descriptor to `AudioCodec::Ac3`. The library hands back raw PES
  bytes; the caller's decoder handles whatever framing is actually
  present.
- **Trigger to revisit:** A capture surfaces in the corpus or a consumer
  ships either variant.

## Typed audio descriptor helpers in `mpegts::descriptors`

- **Status:** Deferred. Per-stream PMT descriptors are caller-supplied via
  `MuxerConfigBuilder::stream_descriptors_for_audio` (parallel to `_for_video` /
  `_for_klv` from plan #17). Two auto-emit helpers ship: `add_audio_with_language(pid, codec, lang)` emits an `iso_639_language_descriptor`
  (tag 0x0A); `AudioCodec::Ac3` streams auto-emit a `registration_descriptor`
  with `format_identifier="AC-3"`. Codec-specific helpers (`ac3_audio()` —
  descriptor tag 0x6A in DVB / 0x81 in ATSC; `aac_audio()` — tag 0x7C;
  `mpeg2_audio()`) are not added.
- **Why deferred:** No consumer has asked for typed audio descriptors,
  and the corpus shape is bare PMT entries (no audio descriptors at all
  on AAC / MP2 streams across the local capture set). Callers who need a
  codec-specific audio descriptor today assemble one via
  `user_private_with_tag(tag, payload)` from the existing helper menu and
  attach via `stream_descriptors_for_audio`.
- **Trigger to revisit:** A consumer needs a specific typed audio
  descriptor, OR the audio frame parser plan lands and pulls the descriptor
  surface into scope alongside the parsed frame metadata.

## Heuristic payload-kind detection (`codec::detect`)

- **Status:** Deferred. The demuxer maps `Unknown { stream_type, raw }` for
  PIDs it can't classify from the PMT (unregistered stream_types, missing
  descriptors). No heuristic inspection is applied.
- **Why deferred:** Heuristics (looks-like-ADTS, looks-like-UL+BER,
  looks-like-Annex-B H.264, etc.) are useful for the local-capture
  exploration use case — feeding in an unfamiliar capture and learning what's
  in it — but they add complexity and false-positive risk. A dedicated
  inspection plan is the right home.
- **Trigger to revisit:** A consumer asks for content-type detection on
  `Unknown` PIDs, or a corpus analysis workflow needs stream-kind heuristics
  without PMT descriptors.

## `pipeline::ext::pairing` — opt-in convenience pairing utility

- **Status:** Shipped (Rust API). `tst_pipeline::ext::pairing::Pairer` with
  `with_config` (Realtime + Buffered) and `last_before_pts` strategies.
  Cookbook recipes 24–27 cover the canonical patterns; recipes 12–14
  remain as the inline-pattern reference. C ABI / JNI / UniFFI
  exposure deferred — see the next entry.

## `pipeline::ext::pairing` C ABI / JNI / UniFFI exposure

- **Status:** Rust API only. `tst-c`, `tst-jni`, `tst-uniffi` do not
  yet expose `Pairer`.
- **Why deferred:** Receiver-side cross-language surfaces are deferred
  to the future receiver-surface plan, so all receiver-side exposure
  (multi-program demux at C ABI, receiver-side stats at C ABI, typed
  codec parsers at C ABI, audio / subtitle / AV1 / H.266 carriage at
  C ABI, and now `Pairer`) lands coherently in one pass instead of
  piecemeal. The Rust API was designed with FFI in mind: flat
  projection structs (`VideoSample`, `KlvSample`), a tagged-enum
  output (`PairerOutput`) that maps to C discriminator + union, and
  no lifetimes.
- **Trigger to revisit:** When the receiver-surface C ABI plan is
  written, `Pairer` joins as one more handle type
  (`tst_pairer_t`, `tst_pairer_open_with_config`,
  `tst_pairer_last_before_pts_open`, `tst_pairer_feed`,
  `tst_pairer_flush`, `tst_pairer_stats`, `tst_pairer_close`).
- **Scope when added:** ~7 C entry points + 1 handle type + tagged
  output discriminator. Sketch parallel to the existing
  `tst_demux_receiver_t` shape.

## Multi-program demux at the C ABI

- **Status:** Shipped in Phase 3 (plan #62, 2026-05-16). The
  `tst_demux_receiver_t` typed-event surface includes `tst_event_t`
  with a `PROGRAM_MAP` arm carrying `program_number`; `tst_stream_info_t`
  carries `program_number`; multi-program streams are handled naturally
  by the `DemuxReceiver` backend.

## Rustdoc lift to docs.rs via `#![doc = include_str!(...)]`

- **Status:** Shipped in minimal form, v0.4.0. Each of the 8
  publishable pure-Rust crates (`tst-core`, `tst-pipeline`, `tst-srt`,
  `tst-rtp`, `tst-udp`, `tst-tcp`, `tst-hls`, `tst-rist`) now ships a
  `README.md` (rendered on crates.io/docs.rs via the `readme` package
  key); the six transport crates additionally lift it into the
  crate-level rustdoc via `#![doc = include_str!("../README.md")]`
  (`tst-core`/`tst-pipeline` keep their existing, more detailed crate
  docs instead of double-including — their READMEs are a terser
  summary that agrees with, but doesn't replace, those docs).
  docs.rs now builds real API documentation from the bundled source
  once each crate publishes.
- **Why deferred (partially, still):** Full migration of the
  markdown guides under `docs/` (the per-language onramps, cookbook,
  guides) into rustdoc is not planned — those are user-journey docs
  meant to be browsed as a set from the repo/docs site, not per-crate
  API reference pages. Only the per-crate README summary moved.
- **Trigger to revisit:** Satisfied by the v0.4.0 README lift above.
  No further trigger is tracked; revisit only if a future consumer
  specifically asks for the full guide tree to be mirrored into
  docs.rs.

## URL parameter coverage (Group 3 — recognized but unsupported)

- **Status:** Parser recognizes the libsrt URL key by name and rejects
  with `UrlError::UnsupportedKey` carrying its `SRTO_*` name. No
  silent failure; the operator gets a clear "this option exists but
  isn't yet exposed" message.
- **The list:** `bindtodevice` (`SRTO_BINDTODEVICE`),
  `cryptomode` (`SRTO_CRYPTOMODE`), `drifttracer`
  (`SRTO_DRIFTTRACER`), `enforcedencryption` (`SRTO_ENFORCEDENCRYPTION`),
  `groupconnect` (`SRTO_GROUPCONNECT`), `groupminstabletimeo`
  (`SRTO_GROUPMINSTABLETIMEO`), `iptos` (`SRTO_IPTOS`), `ipttl`
  (`SRTO_IPTTL`), `ipv6only` (`SRTO_IPV6ONLY`), `kmpreannounce`
  (`SRTO_KMPREANNOUNCE`), `kmrefreshrate` (`SRTO_KMREFRESHRATE`),
  `maxrexmitbw` (`SRTO_MAXREXMITBW`), `messageapi` (`SRTO_MESSAGEAPI`),
  `mininputbw` (`SRTO_MININPUTBW`), `minversion` (`SRTO_MINVERSION`),
  `nakreport` (`SRTO_NAKREPORT`), `peeridletimeo` (`SRTO_PEERIDLETIMEO`),
  `retransmitalgo` (`SRTO_RETRANSMITALGO`), `snddropdelay`
  (`SRTO_SNDDROPDELAY`), `transtype` (`SRTO_TRANSTYPE`), `tsbpdmode`
  (`SRTO_TSBPDMODE`).
- **Why deferred:** Each requires a new `SocketBuilder` setter on
  `tst-srt` plus its typed wrapper / validation. Single-developer
  scope discipline — none of these has a current consumer ask.
- **Trigger to revisit:** A consumer asks for any specific key. Adding
  one is mechanical: add the builder setter + URL parser arm + remove
  it from this list and the parser's `GROUP3_REJECTED` table.

## URL parameter coverage — `rcvbuf` / `sndbuf` (units mismatch)

- **Status:** Listed in the URL parser as Group 3 (rejected). Separate
  entry from the rest because the blocker is units, not "no setter
  yet."
- **Why deferred:** The URL parser currently rejects `rcvbuf`/`sndbuf`
  to avoid a foot-gun: libsrt's `SRTO_RCVBUF` / `SRTO_SNDBUF` are
  byte counts, and earlier builder setters were misleadingly named
  `recv_buf_packets` / `send_buf_packets`. The units confusion is now
  resolved: the builder setters were renamed to `recv_buf_bytes` /
  `send_buf_bytes` (DA-SRT-1). Wiring the URL keys is now
  mechanical — the only remaining work is adding the parser arms.
- **Note:** distinct from `udprcvbuf` / `udpsndbuf` (kernel UDP socket
  buffer sizes via `SRTO_UDP_RCVBUF` / `SRTO_UDP_SNDBUF`), which **are**
  exposed as URL keys (and as `recv_buffer_size` / `send_buffer_size`
  ffmpeg aliases).
- **Trigger to revisit:** A consumer asks for URL-driven buffer sizing.
  Add the parser arms in `url.rs` (move `rcvbuf`/`sndbuf` from
  `GROUP3_REJECTED` to the mapped set) and remove from this list.

## URL-vs-builder conflict warning channel

- **Status:** Today the URL parser silently overrides builder values
  on conflict (per the documented "URL wins" rule). There's no
  channel to surface "FYI, your builder said X but the URL changed
  it to Y."
- **Why deferred:** No warning channel exists in the C ABI today —
  `tst_get_last_error_str()` is for failures, not warnings. Adding a
  warning surface is its own design (separate buffer? log callback?
  per-thread storage like the error?). Out of scope for the URL
  parser ship.
- **Trigger to revisit:** A consumer reports a debugging session
  where they spent more than a few minutes wondering why their
  builder values didn't take effect; OR an unrelated request for a
  warning surface lands first.

## URL parser: additional test coverage

- **Status:** Three test categories not in the initial ship:
  1. Property-based roundtrip via `proptest` (random valid URLs
     roundtrip cleanly through parse and apply).
  2. Concurrent-open smoke (50–100 threads, no shared parser state).
  3. Atomicity-under-load 1000-iteration smoke (Q9-A invariant
     defended against future regression).
- **Why deferred:** The initial ship includes a fuzz target for
  panic-freedom and a one-shot atomicity test. Property testing needs
  a `proptest` dev-dependency; the parser's structural invariants (no
  shared mutable state, clone-then-mutate) make 2–3 redundant for
  initial coverage. They're additive regression-guards, not must-have
  for first ship.
- **Trigger to revisit:** First consumer-reported URL parser bug
  becomes a property test; concurrent-open returns when adding builder
  setters from Group 3 (more parser surface = more potential for
  shared state); atomicity-under-load gets re-considered if the Q9-A
  invariant gets touched (e.g. someone optimizes the clone away for
  performance).

## URL parser: strict percent-encoding validation

- **Status:** The parser inherits `url::Url::query_pairs()`'s lenient
  handling of malformed percent-encoding in queries. Sequences like
  `%2` or `%XY` pass through as literal substrings rather than
  rejecting with `UrlError::Syntax`. The fuzz target enforces
  panic-freedom; functional rejection of malformed sequences is not
  enforced.
- **Why deferred:** Strict rejection would mean either pre-validating
  the query string before handing to `url::Url::parse` or adding a
  manual percent-decode pass. Non-trivial work for a low-risk failure
  mode — the worst that happens is the typed validator on the per-key
  value rejects the literal `%2` substring (e.g. `StreamId::new("%2")`
  accepts ASCII, so even that escape hatch is partial).
- **Trigger to revisit:** A consumer reports a malformed URL silently
  parsing where they expected an error.

## `Listener::accept_timeout` — bounded blocking accept

- **Status:** Shipped in plan #30 (commit cf3233b).
  `Listener::accept_timeout(Duration)` uses a one-shot `srt_epoll_wait`
  to gate readiness, then calls `srt_accept` once a connection arrives or
  returns `AcceptError::TimedOut` on expiry. `Listener::set_recv_timeout`
  continues to apply only to *accepted* sockets, not to the accept call
  itself — see `guide-srt.md` §Blocking semantics for the distinction.

## Errno-based error classification (`SrtErrno` minor codes)

- **Status:** Several `From<RawError> for *Error` impls match libsrt
  error message strings (`raw.message.contains("refused")`,
  `contains("in use")`, `contains("permission")`, `contains("closed")`,
  etc.) instead of the libsrt errno (`SRT_ENOSERVER`, `SRT_ECONNREJ`,
  `SRT_ELARGEMSG`, `SRT_EMSGSIZE`, etc.). The current `SrtErrno` enum
  collapses to major categories only.
- **Why deferred:** String-matching works against libsrt 1.5.7 today;
  the audit recommended deferring this refactor until either a libsrt
  upgrade breaks a string match or a user reports a misclassified
  error. Either trigger is well-defined and should reach the
  maintainer.
- **Trigger to revisit:** libsrt minor-version upgrade (1.5.x → 1.6.x)
  with classification regressions, OR a user-reported wrong-variant.

## `KeyLength` → `Option<KeyLength>` ergonomics

- **Status:** `SocketConfig::key_length` is `KeyLength` (default
  `Aes128`) and is unconditionally written to `SRTO_PBKEYLEN` whenever
  a passphrase is set. ffmpeg only sets `SRTO_PBKEYLEN` when the user
  explicitly passes `?pbkeylen=`, letting libsrt auto-negotiate.
- **Why deferred:** Negligible interop impact. AES-128 is the de-facto
  default everywhere; the only failure mode is a peer hardcoded to
  AES-256 rejecting our handshake.
- **Trigger to revisit:** A user reports an interop failure with an
  AES-256-only peer.

## `srt_cleanup()` shutdown hatch

- **Status:** Never called. `ensure_initialized()` runs once and libsrt
  stays initialized for the process lifetime. `init.rs` documents the
  rationale (drop-order ambiguity vs. negligible OS-reclaimed leaks).
- **Why deferred:** For long-running services this is correct. For
  short-lived CLIs and tests, valgrind / LeakSanitizer / Miri may
  report leaks; for dynamically-loadable modules unloaded by host
  processes, there's no escape hatch.
- **Trigger to revisit:** A consumer reports problems with libsrt
  init persisting beyond their module's lifetime (e.g. host plugin
  framework with hot-reload), OR LeakSanitizer integration becomes
  load-bearing in CI.

## Rust-API-only sender pipeline defaults

- **Status:** Shipped 2026-05-07. `SocketConfig::sender_defaults()` /
  `::receiver_defaults()` constructors, `merge_sender_defaults()` /
  `merge_receiver_defaults()` in-place merge methods, and matching
  `SocketBuilder::sender_defaults()` / `::receiver_defaults()` chain
  methods all live in `tst-srt`. The `tst-c::connect_srt` helper now
  calls `SocketConfig::merge_sender_defaults` instead of inlining the
  merge logic. See the "Sender / receiver presets" section in
  `docs/guides/srt.md`.

## URL parameter coverage — bigger Group 3 keys (audit Issue 6 Cat B/C)

- **Status:** The audit identified roughly 14 Group 3 keys that ffmpeg
  honors. This plan accepted only Category A (5 cheap aliases mapping
  to existing setters). Categories B (`rcvbuf` / `sndbuf` /
  `messageapi` / `nakreport` / `minversion`) and C
  (`enforcedencryption` / `kmrefreshrate` / `kmpreannounce` / `iptos` /
  `ipttl` / `snddropdelay` / `transtype` / `tsbpdmode`) remain
  deferred. Each Category C key needs a new `SocketConfig` field plus
  typed wrapper plus URL parser arm.
- **Why deferred:** Per the audit's recommendation: "duplicates work
  the project will eventually do anyway as deferred features get
  unblocked one by one — don't try to land them all in this audit
  fix."
- **Trigger to revisit:** Each individual key's existing trigger in
  the general "Group 3 unsupported keys" entry above; nothing
  additional.

## Reconnect counters on `ManagedTransport` stats

- **Status:** Partially resolved 2026-08-19. Send-side reconnect/gap
  telemetry (`reconnect_attempts`, `reconnect_successes`, `gap_len`,
  `gap_messages_dropped`, `gap_bytes_dropped`, `reconnecting`) now ships
  via `ManagedTransport::stats_handle() -> ManagedStatsHandle` and
  `ManagedTransportStats` — a dedicated accessor, not a field grafted
  onto `SenderStats`, per the design this entry originally called for.
  Receive-side counters (`ManagedRecvTransport`) remain deferred: it
  exposes only `reconnects_count()` (a bare rebuild tally), with no
  gap-analog (the receive side has no gap buffer) and no per-cycle
  attempt/success breakdown.
- **Why deferred (recv side):** No consumer has asked for receive-side
  reconnect telemetry beyond the existing rebuild counter; the send-side
  pass landed first because it was the one an integrator field report
  and the background-reconnect work both needed.
- **Trigger to revisit:** A consumer running a managed-reconnect
  receive pipeline asks for visibility into how often the link is
  flapping (e.g. for alarm thresholds / backoff tuning) beyond the
  existing rebuild count.

## Background reconnect — bindings parity (C / Python / JVM) — RESOLVED 2026-08-21

- **Status:** RESOLVED. `ReconnectMode::Background`,
  `ManagedTransport::stats_handle()`, and `ManagedStatsHandle` /
  `ManagedTransportStats` (send-side reconnect/gap telemetry) are now
  reachable from all three bindings: the C ABI mirror
  (`TstReconnectMode` + `tst_reconnect_policy_set_mode` on
  `tst_reconnect_policy_t`, plus `tst_managed_transport_stats_t` +
  `tst_managed_{sender,mux_sender,raw_sender}_get_reconnect_stats`, ABI
  minor 20) shipped in PR #165; the Python mirror (`ReconnectMode`
  incl. `BACKGROUND`, `.reconnect_stats()`, `ManagedTransportStats`)
  shipped in PR #166; the JVM mirror (`ReconnectMode.BACKGROUND`,
  `.reconnectStats()`, `ManagedTransportStats`) shipped in PR #167.
  All three stay send-side only, matching the Rust-side scope — see
  the "Reconnect counters on `ManagedTransport` stats" entry above for
  the still-deferred receive-side residue (unaffected by this arc).

## Last-activity-wall-clock gauges per stream (C / Python / JVM) — RESOLVED 2026-08-21

- **Status:** RESOLVED. `StreamStats.last_seen: Option<SystemTime>`
  (stamped on every mux push / demux emit, `std` builds only) is now
  reachable from all three bindings. The C ABI mirror shipped
  2026-08-20 (ABI minor 20, PR #165) — NOT by growing
  `tst_stream_stats_t` (still byte-size-asserted, plain-integer
  counters only), but as a new getter,
  `tst_*_get_stream_last_seen_micros`, on all six demux-receiver
  handle families (plain + managed SRT, RIST, RTP, TCP, UDP), returning
  a `uint64_t` Unix epoch microsecond timestamp (`0` if the PID has
  never been observed). The Python mirror (`last_seen_micros(pid) ->
  Optional[int]` on `rtp.DemuxReceiver`, `srt.DemuxReceiver`, and
  `srt.ManagedDemuxReceiver`) shipped in PR #166. The JVM mirror
  (`lastSeenMicros(pid) -> Long`, same three receiver classes) shipped
  in PR #167 — narrower than the C ABI's six families because neither
  binding exposes standalone RIST/TCP/UDP receiver classes today.
  Python and JVM diverge from the C getter's `0`-if-unseen sentinel —
  both use their native nullable idiom (`None` / boxed `null`)
  instead, since neither has a bare-integer ABI constraint.

## Per-stream PMT descriptor surface at the C ABI

- **Status:** The Rust core ships per-stream PMT descriptors via the
  `mpegts::descriptors` module and `MuxerConfigBuilder::stream_descriptors_for_video` /
  `stream_descriptors_for_klv` / `stream_descriptors_for_stream` methods.
  The C ABI exposure is deferred.
- **Why deferred:** The descriptor-construction surface and the future
  receiver C ABI's per-stream descriptor surface should land together —
  exposing a send-only C ABI shape now would need reshaping when the
  receiver C ABI lands (which will also need `TstRawDescriptor` and
  read access to `StreamInfo::raw_descriptors`).
- **Trigger to revisit:** The receiver-surface design lands and pulls
  the descriptor surface into scope. At that point the C ABI gets
  descriptor builders mirroring `mpegts::descriptors` plus
  `tst_mux_config_set_video_stream_descriptors` /
  `_set_klv_stream_descriptors` with bounded array params + a
  `TstRawDescriptor` `repr(C)` shape for the receive side's
  `StreamInfo::raw_descriptors`.

## `Socket::close` Result-type cleanup

- **Status:** `tst_srt::Socket::close(self) -> Result<(), IoError>` always
  returns `Ok` after the 2026-05-03 cancellation refactor. The
  underlying `srt_close` return code is consumed inside the
  `SrtCancelHandle` closer (which has a `Fn` signature, no return path).
  Same applies to `tst_srt::Listener::close`.
- **Why deferred:** The signature is preserved for API stability —
  changing it now would be a breaking change for consumers who pattern
  on `if let Err(e) = sock.close()`. A future breaking-change cycle
  could either drop the `Result` entirely (close becomes infallible)
  or plumb `srt_close`'s rc back via a richer `CloseError` channel.
- **Trigger to revisit:** Next breaking-change cycle, OR a consumer
  reports needing the `srt_close` rc (e.g., to distinguish
  graceful-close from race-close errors).

## Pre-emptive close cancellation at the C ABI

- **Status:** Partially shipped. All six sender `_cancel` entry points
  ship in Phase 1 (plan #59). `tst_raw_receiver_cancel` ships in
  Phase 1; `tst_receiver_cancel` and its managed sibling ship in
  Phase 2 (plan #60). The remaining `tst_demux_receiver_cancel` rides
  with Phase 3.
- **Why deferred (originally):** The C ABI's `Handle<T>`
  (= `Mutex<Option<T>>`) has the same blocking issue at the C layer
  that the Rust shells had — `tst_*_close` waits on the handle's
  mutex, so it competes with a parked C-side data-path call. Fixing
  it cleanly requires a side-channel `Arc<dyn TransportCancel>` +
  `Arc<AtomicBool>` captured at `_open` time, outside the mutex.
  That design was implemented in Phase 1 and carried forward.
- **Status (updated 2026-05-16):** `tst_demux_receiver_cancel` shipped
  in Phase 3 (plan #62). Pre-emptive close cancellation is now complete
  across all six sender families and all three receiver handle types.

## Typed WebVTT cue substrate (`mpegts::webvtt::format_pes_payload` + `WebVttCue`)

- **Status:** Deferred. WebVTT-in-TS carriage ships in plan #22
  (2026-05-04); `Muxer::push_subtitle` accepts pure pass-through
  bytes (caller hand-builds the cue PES payload). A typed substrate
  with `WebVttCue { identifier, start, end, settings, payload }` and
  `format_pes_payload(&WebVttCue) -> Vec<u8>` (write side) +
  `parse_pes_payload(bytes) -> Vec<WebVttCue>` (read side) is not
  shipped.
- **Why deferred:** Mirrors how `klv::st0601` typed builder layered
  on top of the `klv` byte substrate — typed layer is a separate
  session's worth of work. Downstream consumers (e.g. HLS POI
  injection) can build cue bytes ad-hoc until the typed layer ships.
- **Trigger to revisit:** A consumer asks for typed cue
  parameters / serialization or the second WebVTT consumer
  reimplements the same byte-builder logic.

## Typed DVB-sub data segment / DVB-teletext data unit / CEA-708 cc_data parsers

- **Status:** Deferred. Plan #22 ships carriage layer only — payload
  bytes pass through verbatim. Typed parsers (`subtitle_data_segment`
  per ETSI EN 300 743; `teletext_data_unit` per ETSI EN 300 706;
  `cc_data_pkt` per CEA-708-D) do not exist. (Note: SMPTE ST 334-2
  §5.4 — now cached — supplies the `cc_data_pkt` *container* byte layout
  [`marker(5)='11111' | cc_valid(1) | cc_type(2) | cc_data_1(8) |
  cc_data_2(8)`] plus the per-frame-rate `cc_count` table, so the
  carriage/container layer is now spec-anchored; only the CEA-708
  caption-text coding model itself remains behind the paywalled
  CEA-708-D/-E.)
- **Why deferred:** No driving consumer for typed access today; the
  typed layer is a separate session's worth of work per codec.
- **Trigger to revisit:** A consumer asks for typed access to
  specific fields (page composition pixel-data, teletext line
  Hamming-decoded text, CEA-708 caption text channel). Resolving
  this entry will also wire `NonConformantIssue::SubtitleDescriptorMalformed`
  (currently a reserved variant — the classification cascade is
  tag-presence-based via `find_descriptor_tag`, so malformed
  descriptor bodies pass through today).

## WebVTT-in-TS interop

- **Status:** Deferred. WebVTT-in-MPEG-TS carriage ships in plan #22
  (registration_descriptor `"VTTC"` + single-cue PES + subtitle PID
  excluded from PCR fallback) and round-trips through the library's
  own mux + demux. Interop with external tools (ffmpeg, hls.js,
  mediamtx, etc.) has not been empirically verified.
- **Why deferred:** The `"VTTC"` format_identifier is not defined by
  any published normative spec (RFC 8216, draft-pantos-hls-rfc8216bis,
  Apple HLS authoring docs — none mention it). It appears in ffmpeg's
  `mpegtsenc.c` emitter and is widely observed in WebVTT-in-TS
  captures, but the cross-tool interop is empirical, not normative.
  Empirical interop testing requires fixture corpus from each tool +
  a test matrix — a separate session's worth of work.
- **Trigger to revisit:** Validate-1 Wave I (empirical interop matrix)
  schedules an interop test against ffmpeg / hls.js / mediamtx /
  GStreamer; results from that pass either confirm interop or
  surface concrete divergences requiring spec follow-up.

## CEA-708 interop

- **Status:** Deferred. CEA-708 caption data as a standalone
  elementary stream ships in plan #22 (registration_descriptor
  `"GA94"` + private-data PES). Library-internal round-trip works;
  interop with ATSC ecosystem tooling (decoders, MPEG-2 video user_data
  bridges) has not been empirically verified.
- **Why deferred:** ATSC A/53 Part 4 §6.2.3 defines `"GA94"` as the
  `user_data_identifier` for caption data **embedded in MPEG-2 video
  user_data**, not as a stream-level marker. Using it for standalone
  PES carriage is best-effort interop with ATSC ecosystem tooling,
  not normatively defined. Empirical interop testing requires the
  same fixture / matrix infrastructure as the WebVTT-in-TS entry
  above.
- **Standards-aligned form not implemented (SMPTE ST 334-2 CDP):**
  The 2026-06-20 SMPTE-spec audit confirmed that SMPTE ST 334-2
  (Caption Distribution Packet) + EG 43 §6.7 define the *standards-aligned*
  unit for standalone caption-on-a-PID carriage: a **CDP** (`0x9669`
  header + `ccdata_section` + footer/checksum), NOT the bare
  `cc_data_pkt` stream this library currently emits. The library carries
  raw cc_data verbatim (passthrough), so it neither emits nor parses a
  CDP. Adopting the CDP — wrap caller cc_data in a CDP on mux; strip /
  validate on demux — would be the conformant upgrade; ST 334-2 §5.2-5.6
  (cached at `reference/smpte/st0334-2-2015.pdf`) gives the full byte
  layout. Do NOT silently change the wire format: if implemented, the CDP
  form should be a distinct carriage variant, not a mutation of the
  existing raw-cc_data `Cea708Standalone`.
- **Trigger to revisit:** Validate-1 Wave I (empirical interop matrix)
  schedules CEA-708 interop testing against ATSC ecosystem tooling;
  results from that pass either confirm interop or surface the need
  for a different marker convention. A consumer requiring ATSC-ecosystem
  interop is the trigger to implement the ST 334-2 CDP variant.

## RP 225 registered private information in KLV (in-KLV-stream vendor metadata)

- **Status:** Deferred / future opportunity (not a bug). Surfaced by the
  2026-06-20 SMPTE-spec audit. The library carries vendor/application
  private data **only** as a separate MPEG-TS private-data elementary
  stream (`StreamSpec::Data` / `push_data`, a transport-layer concept).
  It does NOT implement SMPTE RP 225 — the **KLV-layer** mechanism for
  carrying registered private information *inside* a KLV stream via a
  Universal Label whose registry category designator = `0x05`
  ("registered private information") + a SMPTE-RA-registered
  `format_identifier` mapped into UL bytes 9-16.
- **Why deferred:** No driving consumer. The two "private" concepts are
  distinct layers (transport private-data PID vs. KLV registered-private
  UL); the shipped private-data arc correctly makes no RP 225 claim. The
  generic KLV substrate (`UniversalLabel::new`,
  `klv::length::{write_ber, write_ber_oid}`) is already sufficient to
  build RP 225 records.
- **Trigger to revisit:** A consumer asks to carry self-identifying
  vendor-private metadata records *inside* the existing KLV elementary
  stream (alongside ST 0601) rather than on a side-channel private-data
  PID. Then add a thin `klv::rp225` UL-builder per RP 225 §3/§4 Tables
  1-3 (structure designator 1 for ASCII-only format_identifiers, 2 =
  BER-OID per §4) over that substrate. Spec cached at
  `reference/smpte/rp0225-2005.pdf`.

## Subtitle carriage at the `tst-c` C ABI

- **Status:** Deferred (no consumer ask). Plan #22 ships sender-side and
  receiver-side Rust APIs covering DVB-sub, teletext, CEA-708, and
  WebVTT-in-TS. The `tst-c` C ABI sender surface currently exposes
  `tst_mux_sender_send_video` and `tst_mux_sender_send_klv` but no
  `tst_*_send_subtitle` / `tst_*_send_subtitle_to` siblings, and the
  config builders do not expose `tst_mux_config_add_subtitle_stream` /
  `tst_subtitle_stream_handle_t`.
- **Why deferred:** Adding the entries is mechanical (parallel to the
  existing video / KLV send entries) but requires deciding the subtitle
  envelope shape at the C boundary across the four supported codec
  families. No consumer has asked.
- **Trigger to revisit:** A binding-author asks for subtitle send
  through the C ABI; a downstream consumer needs in-band subtitles
  for a use case not served by the Rust API.

## ARIB STD-B24 / ARIB STD-B37 subtitling

- **Status:** Deferred. Japanese broadcast subtitling carriage is
  not classified by the cascade. ARIB-shaped PIDs surface as
  `Unknown` today.
- **Why deferred:** No consumer ships ARIB content. The PMT shape
  diverges enough from ETSI / Apple forms that adding it is its
  own design.
- **Trigger to revisit:** A Japanese broadcast consumer.

## WebVTT out-of-band for HLS (separate `*.vtt` segment files)

- **Status:** Not in scope. The HLS spec lets subtitles ride
  out-of-band as separate `*.vtt` segment files referenced from a
  `#EXT-X-MEDIA:TYPE=SUBTITLES` entry in the playlist — no MPEG-TS
  involvement.
- **Why deferred:** Not a `ts-transformer` concern. Out-of-band WebVTT
  delivery is an HLS-packager / orchestrator concern outside this
  library.
- **Trigger to revisit:** Never (different layer).

## Real-world public-broadcast subtitle fixture acquisition

- **Status:** Deferred. Plan #22 ships synthetic-only fixtures
  (~200 KB) generated by `gen-subtitle-fixtures` — no real
  broadcast captures.
- **Why deferred:** Synthetic + ffprobe cross-check is enough for
  ship; real broadcast captures add coverage if/when a real-world
  bug surfaces that the synthetic suite missed. Acquisition has
  legal / licensing overhead too.
- **Trigger to revisit:** A consumer reports a real-world bug the
  synthetic suite missed.


## Multi-cell fragmented metadata AU cells

- **Status:** SHIPPED 2026-05-24.
- **Plan:** `docs/plans/2026-05-24-multi-cell-au-reassembly.md` (outside the published repo).
- **Behavior:** the demuxer now reassembles fragmented AU cells per
  H.222.0 V9 §2.12.4.2 Table 2-157. Both flavors covered:
  - Multiple AU cells back-to-back within one PES — every cell emits
    its own event; previously only the first cell did.
  - Cells of one AU spread across multiple PES packets — `First` +
    `Middle`* + `Last` accumulate in a per-PID buffer until `Last`
    completes the AU; the demuxer then emits one event with
    `MetadataKind::KlvSyncAuCell::was_reassembled = true` and
    `cell_count = N`.
- **Failure modes:** `NonConformantIssue::MultiCellAu` now carries a
  typed `reason: MultiCellAuReason` (`Orphan` / `SequenceGap` /
  `ConcurrentFirst` / `Overflow`). Per-PID buffer cap configurable via
  `DemuxerConfig::au_cell_cap_per_pid` (default 1 MiB).
- **Out of scope:** caller override of `random_access_indicator` /
  `decoder_config_flag` on the mux side; mux-side emit of fragmented
  output. Both remain as separate deferred entries.

## Caller override of `random_access_indicator` / `decoder_config_flag` on mux

- **Status:** Deferred.
- **Why deferred:** `Muxer::push_klv_to` hard-codes
  `random_access_indicator=true` (every push is an entry point —
  correct for self-contained ST 0601 LS records) and
  `decoder_config_flag=false` (we do not carry decoder
  configuration). For stateful KLV sets — ST 1206 SAR with delta
  encoding, ST 0902 motion imagery with reference-frame-relative
  VMTI — "entry point" semantics differ; only some pushes would be
  RAI=1. The current ST 0601 typed surface is correctly served by
  the hard-coded defaults.
- **Trigger to revisit:** A typed surface for a stateful set
  lands, OR a consumer emits non-ST-0601 sync KLV that needs
  different semantics. Likely landing shape:
  `Muxer::push_klv_to_with_config(handle, klv, pts,
  SyncKlvConfig { random_access_indicator,
  decoder_config_flag })` — following the workspace
  `_with_config` constructor convention (see `docs/reference/conventions.md`).

## ST 1910.1 KLV-in-CMAF-emsg-box delivery

- **Status:** Deferred.
- **Why deferred:** ST 1910.1 (Adaptive Bitrate Content Encoding,
  2020) defines KLV-in-CMAF emsg-box delivery for HLS/DASH
  consumption — separate from MPEG-TS carriage. No CMAF/HLS
  consumer asks for this in the current pipeline. Note: this is
  unrelated to the MPEG-TS sync-metadata AU cell at
  `mpegts::au_cell` (per H.222.0 § 2.12.4.2) — different specs,
  different layers.
- **Trigger to revisit:** An HLS/DASH-delivery consumer needs to
  ingest sync KLV from a CMAF stream (e.g., a future HLS pipeline
  that elects the emsg-box path instead of the MPEG-TS path).

## DVB-shaped AC-3 (`stream_type 0x06` + AC3_descriptor `0x6A`)

- **Status:** Deferred. AC-3 carriage is ATSC-shaped only —
  `stream_type 0x81` with `format_identifier="AC-3"` registration
  descriptor (the shape ffmpeg's mpegts muxer emits by default).
  The DVB shape uses `stream_type 0x06` with `AC3_descriptor`
  (tag `0x6A`) per ETSI TS 101 154 §5.6.
- **Why deferred:** No consumer in the current target deployment
  uses DVB-shaped AC-3. Adding the path means either a new
  `AudioCodec::Ac3Dvb` enum variant (parallel to existing `Ac3`)
  or a `MuxerConfig::ac3_mode: Ac3Mode { Atsc, Dvb }` switch — both
  expand the public API without a use case. ATSC-only mode covers
  every known consumer.
- **Workaround:** A receiver consuming DVB-shaped AC-3 today
  classifies as `Unknown(0x06)` unless the caller passes
  `DemuxerConfig::treat_as` mapping the PID to `AudioCodec::Ac3`;
  the library hands back raw PES bytes regardless of the
  registration descriptor shape.
- **Trigger to revisit:** A DVB-only receiver appears in the
  target deployment, or a corpus capture shows DVB-shaped AC-3.

## Auto-prepend of access-unit delimiter (AUD) on H.264 / H.265 / H.266

- **Status:** Deferred. Caller is responsible for prepending the
  codec-specific AUD NAL when required. `Muxer::push_video_to`
  passes the caller's NAL stream through verbatim (post-Annex-B
  framing validation).
- **Why deferred:** ffmpeg's `mpegtsenc.c:1907-2069` auto-inserts
  AUD on H.264/H.265 if missing, but the AUD NAL type and content
  differ across codecs (H.264 type 9, H.265 type 35, H.266 type
  20) and the encoder side already emits AUD on most modern
  toolchains (x264 with `--aud`, x265 with `--aud`, libavcodec
  with `flags +aud`). Adding auto-insert means a bit-stream-aware
  filter on every video push and a codec-dispatch table — non-
  trivial without a consumer-driven need.
- **Trigger to revisit:** A consumer reports decoder breakage on
  AUD-required hardware decoder (some HW decoders, libde265 in
  certain configurations, broadcast-grade STBs) when streams
  arrive without AUD. Likely landing shape:
  `MuxerConfig::auto_aud: bool` gate on the muxer, defaulting off,
  with per-codec NAL emission.

## SRT URL `mode=listener` / `mode=rendezvous` dispatch

- **Status:** Partially resolved (entry refreshed 2026-09-06; the
  earlier text predated the receiver C ABI). The URL parser at
  `crates/tst-srt/src/url.rs` accepts `mode=caller` (the default) and
  `mode=listener`; `mode=rendezvous` is still rejected with
  `UrlError::UnsupportedMode`. Listener dispatch is wired on every
  RECEIVER entry point: the C `tst_*receiver_open` family routes a
  `?mode=listener` URL through the listener path and the
  `_open_listener` variants force it regardless of the URL. It is NOT
  wired on the sender side: every `tst_*sender_open` in the C ABI
  (plain, mux, raw, and the managed variants) dials out as an SRT
  caller, and a `?mode=listener` URL handed to one parses fine but then
  fails at connect time (address lookup on the empty host,
  `TST_E_TRANSPORT`) rather than at validation. The Rust API is
  unaffected — build a `Listener`, wrap the accepted `Socket` in
  `SrtTransport`, as `examples/sending/srt_serve_ts_file.rs` does.
- **Why deferred (sender side):** a listener-mode sender is the
  "player dials in" shape (VLC, ffplay and `srt-live-transmit` default
  to caller mode), which is real but has had no C or binding consumer
  ask for it; the receiver side was the P0 need. A sender
  `_open_listener` family is a small additive C-ABI change mirroring
  the receiver's once someone needs it. Rendezvous needs both ends'
  cooperation and has no consumer.
- **Trigger to revisit:** a C or binding consumer that must serve a
  stream to a caller-mode player — the shape
  `bindings/c/examples/sending/send_srt_ts_file.c` documents as
  unavailable and works around by dialling a listening receiver. For
  rendezvous: a deployment with symmetric NAT on both ends.

## Media over QUIC (MoQ) transport target

- **Status:** Deferred. The only transport implementation is
  `tst-srt::SrtTransport` over libsrt. The IETF MoQ Transport
  draft (`draft-ietf-moq-transport`) and its MSFTS payload-
  format extension (`draft-gregoire-moq-msfts`, which carries
  MPEG-TS packets over MoQ) are not implemented and have no
  scaffolding in the workspace.
- **Why deferred:** Project scope is SRT-only by design. MoQ
  Transport itself is still a working-group draft; MSFTS is
  `draft-00`, Informational, individual submission, May 2026.
  Both specs are too early to commit binding code to. No
  consumer has asked for browser-reachable delivery, which is
  the natural pull-through for a MoQ binding. The `Transport`
  trait in `tst-pipeline` already cleanly decouples the SRT
  crate from `tst-core`, so this remains an additive future
  move rather than a refactor.
- **Trigger to revisit:** Any of: (1) a consumer asks for
  browser-side delivery that MoQ would enable; (2) MoQ
  Transport reaches WGLC; (3) MSFTS publishes a `-01` revision
  with metadata-stream / sidecar-data signaling (e.g. a
  KLV-aware mapping) or picks up an ISR-aware co-author;
  (4) ffmpeg or gstreamer ship a stable MoQ output that
  becomes a de facto receiver target.
- **Scope when added:** A new `tst-moq` crate parallel to
  `tst-srt`, exposing `MoqTransport` / `MoqRecvTransport`
  implementing the existing traits. Because MSFTS preserves
  TS packets verbatim, the existing `tst-core` mux/demux
  passes through unchanged — KLV-in-TS rides over MoQ
  without codec or framing changes. A QUIC stack dependency
  (likely `quinn`) is the main new build axis. URL surface
  gets a `moq://` family alongside `srt://`.

## iOS (arm64 device + arm64 simulator + x86_64 simulator)

- **Status:** Partial — a C-ABI iOS build spike exists (2026-09-02, Apple/
  VideoToolbox PoC arc). `scripts/apple/build-ios.sh` cross-compiles the
  `tst-c` static lib + vendored libsrt + mbedTLS for `aarch64-apple-ios` and
  `aarch64-apple-ios-sim`, and `scripts/apple/make-xcframework.sh` assembles
  `TSTrans.xcframework` (macOS + iOS + iOS-sim), gated by the manual
  `.github/workflows/apple-ios.yml` (`macos-14`). The enabler is
  `crates/srt-sys/build.rs`'s `apply_apple_ios` (sets `CMAKE_SYSTEM_NAME=iOS`
  etc. for `*-apple-ios*` triples). This is the **C-ABI** path; the full
  UniFFI Swift binding is still deferred (below). `tst-c` also builds Linux
  x86_64/aarch64, macOS arm64, and Windows MSVC (see `compatibility.md`).
- **Why the rest is deferred:** the Swift-idiomatic surface (UniFFI-generated
  bindings, an SPM package wrapper, x86_64-simulator/older-arch coverage)
  belongs with `tst-uniffi` so the consumer-facing shape drives it. The
  C-ABI XCFramework above is enough for a C/Swift-shim PoC in the meantime.
- **Trigger to revisit:** The `tst-uniffi` implementation plan starts (for
  the Swift binding + SPM). The C-ABI XCFramework build is available now.
- **Scope when added:** Three matrix entries (arm64 device,
  arm64 simulator, x86_64 simulator) under a separate iOS-
  specific CI workflow (the existing GHA `macos-14` runner
  can host all three via `xcodebuild` cross-targeting). The
  Rust target triples are `aarch64-apple-ios`,
  `aarch64-apple-ios-sim`, `x86_64-apple-ios`.

## Android (arm64 + x86_64 emulator + armv7)

- **Status:** Deferred. macOS arm64 + Windows MSVC are Tier 1
  (see `compatibility.md`).
- **Why deferred:** Android requires the Android NDK toolchain
  + cross-compile toolchain files for both libsrt and mbedTLS
  (the NDK sysroot, libc shape, and ABI selection per target
  arch). The work is bundled with iOS as part of the future
  `tst-uniffi` plan — mobile-binding consumers expect both
  platforms together, and the JNI-style shared-library
  packaging is symmetric.
- **Trigger to revisit:** The `tst-uniffi` implementation plan
  starts. armv7 specifically is the most-likely-to-stay-
  deferred sub-target — only re-included if a consumer reports
  the device class matters (modern Android devices have been
  arm64 since ~2018).
- **Scope when added:** NDK sysroot + cmake toolchain files
  for libsrt + mbedTLS, plus Rust target triples
  `aarch64-linux-android`, `x86_64-linux-android`, and
  (conditionally) `armv7-linux-androideabi`.

## macOS x86_64 (Intel)

- **Status:** Deferred. macOS arm64 (Apple Silicon) is Tier 1.
  The Python wheel for macOS Intel was built best-effort for
  v0.1.0 but removed 2026-06-10 — Intel-Mac users
  `pip install tstrans` from the sdist (it builds from source).
- **Why deferred:** Intel Macs are a declining install base;
  Apple Silicon covers the contributor and laptop case for
  modern macOS. Maintaining Intel-mac support would double the
  macOS CI surface (one runner per arch) for diminishing
  return. The wheel leg was dropped because GitHub's `macos-13`
  Intel runners are scarce — the job routinely sat queued for
  hours and, since the PyPI `publish` job's `needs:` waits for
  every matrix leg (even best-effort ones), it held up the
  whole release train.
- **Trigger to revisit:** A consumer running an Intel Mac
  reports a build failure they want fixed, or asks for a
  prebuilt wheel.
- **Scope when added:** A `macos-13` matrix entry (last Intel-
  only macOS runner; `macos-14`+ are arm64) in
  `.github/workflows/ci.yml` with `continue-on-error: true`
  initially, mirroring the Tier 1 phase-in pattern. For the
  wheel, a standalone job in `python-wheels.yml` that the
  `publish` job does **not** `needs:`-depend on, so a queued
  Intel runner can never gate the publish again.

## Windows MinGW (gcc toolchain)

- **Status:** Deferred. Windows MSVC is Tier 1.
- **Why deferred:** MSVC covers the production Windows case
  (most distributed Windows binaries link MSVCRT). MinGW is
  dev-environment friendly but doubles the Windows CI surface
  (one runner per toolchain) and has its own set of vendored-
  library quirks distinct from MSVC.
- **Trigger to revisit:** A consumer asks for a non-MSVC
  Windows build (e.g., they're shipping a MinGW-based
  application and the toolchain mismatch creates linker
  friction).
- **Scope when added:** A matrix entry using the
  `x86_64-pc-windows-gnu` Rust target on `windows-latest`
  with `continue-on-error: true` initially, mirroring the
  Tier 1 phase-in pattern.

## Windows MSVC runtime tests — RESOLVED 2026-05-29 (one sub-deferral remains)

- **Status:** RESOLVED. windows-msvc now runs the runtime test suite
  and is green across all four platforms (the one carve-out is the
  IPv6-multicast sub-deferral below). Plan #65's "SRT
  loopback hangs on Windows" diagnosis turned out STALE — it was an
  artifact of the pre-MSVC-`cl` librist build; on the cl-built libsrt
  the blocking `srt_recv` wakes on peer-close immediately (proven by a
  bounded diagnostic), same as Linux. The actual blocker was a real
  `SRTO_LINGER` struct-ABI product bug: `LingerOpt` was two `int`
  (8 bytes), but Winsock `struct linger` is two `u_short` (4 bytes), so
  libsrt's `cast_optval<linger>` rejected the size (`MJ_NOTSUP/MN_INVAL`)
  → every sender connect failed → receiver-accept hangs. Fixed
  per-platform in `crates/tst-srt/src/socket.rs`. CI now runs all
  platforms under cargo-nextest, so per-test timeouts bound any future
  hang (one hang can no longer stall the job).
- **Sub-deferrals, now all but one closed:**
  1. **Promote windows-msvc to gating:** DONE 2026-05-30 — all four
     Tier-1 platforms gate CI (`continue` = false in the `ci.yml` and
     `jvm-jar.yml` matrices).
  2. **RIST runtime on Windows:** DONE 2026-07-26 —
     `tst-rist/tests/{loopback,pipeline_round_trip}.rs` are un-gated.
     The blocker was a vendored-librist bug (teardown hung ~14s+ in
     `rist_destroy` and the data plane delivered nothing, both
     profiles), fixed upstream in librist 0.2.18: a CI diagnostic on
     the bumped pin showed teardown in 10–31 ms with full delivery.
  3. **Multicast on Windows:** IPv4 DONE 2026-05-29 (the failure was
     our `set_multicast_if_v4` being unix-only, not a runner
     limitation; fixed via socket2 + receiver-side
     `IP_MULTICAST_LOOP`). **IPv6 multicast remains gated** —
     `IPV6_MULTICAST_IF` needs an interface index on Windows, which
     the URL `?iface=` plumbing doesn't carry yet. This is the one
     remaining sub-deferral.
- **Trigger to revisit:** a consumer needs IPv6 multicast reception on
  Windows (adds interface-index plumbing to the multicast join path).

## RTSP server/client deferred test surface (`#[ignore]`d)

- **Status:** One `tst-rtp` RTSP test remains `#[ignore]`d:
  `rtsp_client/interleaved_e2e.rs`
  (`tcp_interleaved_end_to_end_round_trips_ts_bytes`) — re-ignored
  post-merge (hangs in the post-PLAY drop sequence in the merged
  state); the interleaved wire-up is covered by
  `rtsp_server_loopback_interleaved` + `rtsp_server_notice_5402`.
  Formerly-listed items now DONE: the client-side custom root-store
  wiring (`RtspClientBuilder::tls_root_certs`) is implemented and
  asserted end-to-end by `rtsp_server/tls.rs` (rcgen-generated certs,
  no env-var gate) plus the Python `rtsps://` integration test; the
  `rtsp_server/lagging_peer.rs` harness runs un-ignored.
- **Why deferred:** the interleaved-drop shutdown fix is a
  self-contained follow-up carved out of the tst-rtp Phase 2/3 waves.
- **Trigger to revisit:** the next-session "fully-green test suite"
  pass folds these in alongside the Windows un-gating.

## Audio frame iterators for LATM + AC-3

- **Status:** Deferred. No consumer trigger. Existing iterators in
  `tst_core::codec::*` cover MP2 (`mpegaudio::frames`) and AAC-ADTS
  (`aac::frames`) only — shipped in plan #34.
- **Why deferred:** AAC-LATM (`audio_mux_element` +
  `payload_length_info` framing per ISO/IEC 14496-3) and AC-3
  (ETSI TS 102 366 §6 syncword + frame-size table) both have
  well-defined per-spec frame boundaries; an iterator implementation
  is roughly a day of work each. The "Audio frame parsers — AAC LATM
  and AC-3" entry above tracks the spec-side decode (AC-3 syncframe
  parsing and LATM sync validation already ship there; full per-frame
  decode is the deferred part); this entry is the codec-stats-side
  mirror — no per-frame *counter* iterator exists for LATM/AC-3 yet.
- **Trigger to revisit:** A consumer asks for per-frame counters or
  frame-aligned dispatch on LATM/AC-3 audio. Once added, the `Audio`
  variant of `StreamCodecStats` (shipped in plan #68) automatically
  populates `frames` for those PIDs; today LATM/AC-3 PIDs return
  `Some(StreamCodecStats::Unknown)` via the codec-stats fallback.
- **Scope when added:** Wire the new iterators into the demuxer's
  per-PID audio counter-bump path in the same shape as the existing
  MP2 + AAC-ADTS bumps; the `StreamCodecStats::Audio { frames }`
  variant absorbs the new counts without an ABI change.

## Subtitle codec-specific stats

- **Status:** Deferred. The codec-stats surface shipped in plan #68
  covers Video / KLV / Audio kinds; subtitle PIDs surface as
  `StreamCodecStats::Unknown`.
- **Why deferred:** Low signal value. The codecs covered by the
  subtitle carriage plan (DVB-Subtitling, DVB-Teletext, CEA-708,
  WebVTT-in-TS) don't have meaningful per-segment counts distinct
  from the existing unified `items` counter on `StreamStats`. No
  consumer has asked for them.
- **Trigger to revisit:** A consumer asks for e.g. CEA-708
  caption-frame counts, DVB-sub region-update counts, WebVTT
  cue-emit counts, or DVB-teletext page-update counts.
- **Scope when added:** Add a `Subtitle { segments: u64 }` (or
  per-codec field names if the consumer asks for a finer breakdown)
  variant to `StreamCodecStats`. The `#[non_exhaustive]` enum makes
  this additive without a major bump; wire the bump-site at
  `Demuxer::emit_subtitle` alongside the existing `stats_per_stream`
  bump.

## Deep typed-time migration (arithmetic API design + internal sweep + signed-PCR delta type)

- **Status:** All **public** Rust APIs ship with `Pts90khz` as of Wave 2.1
  (plan `2026-05-18-typed-time-and-packet-constants.md`): `MuxSender::send_*`,
  `Muxer::push_*`, `pts_to_duration`, `DemuxEvent::{Sample,Metadata}.pts`, and
  `pairing::{VideoSample,KlvSample}.pts` all take/return `Pts90khz`. Internal
  arithmetic (private PES writers in `tst-core::mpegts::mux::pes`, private
  `psi_due`/`pcr_due`/`maybe_emit_psi` helpers, demuxer's `last_pts_by_pid:
  HashMap<u16, i64>` and `last_pcr_27mhz: Option<u64>` state, pairing engines
  that do `.as_ticks()` once before arithmetic) remains raw `i64` / `u64`.
  `NonConformantIssue::PcrAnomaly.delta: i64` remains raw `i64` because it's
  a *signed* 27 MHz delta and the existing `Pcr27mhz(u64)` newtype cannot
  represent it.
- **Why deferred:** Sweeping the internal sites is mechanical (~8-12h after
  Wave 2.1 lands), but the arithmetic API on `Pts90khz` / `Pcr27mhz` is a real
  design question: what does `pts_a + duration` return? Does `pts_a - pts_b`
  give a typed `Duration90khz` or raw `i64`? What about 33-bit wrap-around
  (`pts + 1` near `2^33`)? Saturate, wrap, or check (and return
  `Option`/`Result`)? Same questions for a hypothetical `Pcr27mhzDelta(i64)`
  signed-delta type. Picking the wrong default poisons every consumer
  arithmetic site. The internal sweep becomes much cheaper if the arithmetic
  API ships first.
- **Trigger to revisit:** (a) Pre-stabilization API freeze (forces the
  decision), (b) a binding-author request for type-safe internal arithmetic
  in the FFI bridge layer, (c) a binding-author request for a typed signed
  PCR delta on `PcrAnomaly`, or (d) a confirmed-by-fuzzer arithmetic bug
  traced to a `pts_90khz: i64` / `pcr_27mhz: u64` site that typed arithmetic
  would have caught.
- **Scope when added:** (1) Design wrap-vs-saturate semantics on
  `Pts90khz::Add<i64>`, `Sub<Self> -> Duration90khz` (with type definition),
  `Add<Duration90khz>`. Same for `Pcr27mhz`. Decide whether `Duration90khz`
  is its own type or a re-purposed `core::time::Duration`. (2) Add
  `Pcr27mhzDelta(i64)` if signed-delta typing is wanted; migrate
  `PcrAnomaly.delta`. (3) Sweep internal sites listed above. (4) Drop the
  `.as_ticks()` calls in pairing engines (they become typed arithmetic
  directly). (5) Refresh `cargo public-api` baselines; expect intentional
  breaking deltas if `Pcr27mhzDelta` lands. (6) Decide whether the
  fixture-generator `tests/tools/gen_*.rs` migrate too (probably not —
  raw integers are ergonomic for test scaffolding).
- **Effort estimate:** ~4-6h API design + writeup, ~8-12h sweep.

## Python-side subtitle muxing

- **Status:** `tstrans.Muxer` does not expose `add_subtitle` because the
  Rust mux-side `SubtitleCodec` is a struct-variant enum (carrying
  per-codec configuration like language, page IDs, and ancillary
  descriptors) that the flat Python `SubtitleCodec` enum doesn't yet
  model. Demux-side subtitle decoding via `DemuxEvent.Subtitle` IS
  supported. The `Muxer.push_subtitle` / `push_subtitle_to` /
  `Muxer.subtitle_handles()` / `MuxerProgramConfigBuilder.stream_descriptors_for_subtitle`
  surfaces remain wired so they work as soon as the construction gap
  closes.
- **Why deferred:** A half-implemented mux-side API (the previous
  `add_subtitle` that always raised `NotImplementedError`) is worse than
  a missing one — users build against it, then break. Mirroring the
  Rust struct-variant `SubtitleCodec` in Python as a tagged-union /
  dataclass hierarchy is sizeable work that should land as its own
  focused plan, not as a placeholder.
- **Trigger to revisit:** A consumer with a concrete Python subtitle
  muxing use case provides the structured config schema (which
  codec(s), which fields per codec, descriptor emission expectations).
- **Scope when added:** (1) Model the Rust mux-side `SubtitleCodec`
  variants as Python dataclasses (e.g. `DvbSubtitling(language: bytes,
  composition_page_id: int, ancillary_page_id: int)`, `DvbTeletext(...)`,
  `Cea708Standalone`, `WebVttInTs`). (2) Add `add_subtitle(pid, codec)`
  back to `MuxerProgramConfigBuilder` accepting the structured form.
  (3) Round-trip test against demux output. (4) Update CHANGELOG +
  binding-authors doc.

## macOS Intel (x86_64) JVM native library

- **Status:** The JVM fat JAR (`tstrans-jvm`) bundles native libraries
  for four platforms — linux-x86_64, linux-aarch64, macos-aarch64
  (Apple Silicon), and windows-x86_64. macOS Intel (`macos-x86_64`) is
  not built or bundled.
- **Why deferred:** GitHub's `macos-13` (Intel) runners are scarce and
  winding down — a build job for it sat queued 40+ minutes waiting for a
  runner, blocking the `assemble` job (a queued best-effort matrix entry
  never reaches `continue-on-error`, so it stalls the train rather than
  being skipped). The gating `ci.yml` workflow does not build macos-x86_64
  either. Apple is phasing out Intel Macs, and JVM consumers are now
  overwhelmingly on Apple Silicon.
- **Trigger to revisit:** A consumer needs the binding on an Intel Mac
  JVM, or GitHub macOS-Intel runner availability stops being a problem.
- **Scope when added:** Re-add a `macos-x86_64` / `macos-13` entry to the
  `jvm-jar.yml` build matrix as a **separate job** outside the gating set
  (so a scarce runner can't block `assemble`), include its lib in the
  staging download, and add `macos-x86_64` to the `NativeLoader` /
  `build.gradle.kts` triple set. Until then, Intel-Mac users build the
  cdylib from source (`cargo build --release -p tst-jni`).

## `private_stream_2` (0xBF) data carriage

- **Status:** Not implemented. `StreamSpec::Data` emits every data
  payload as PES `private_stream_1` (stream_id `0xBD`), which carries a
  full PES header (and optionally a PTS). There is no way to emit
  `private_stream_2` (stream_id `0xBF`) PES packets.
- **Why deferred:** `private_stream_2` packets carry no PES header
  fields at all — per ISO/IEC 13818-1 §2.4.3.7 the packet goes straight
  from `PES_packet_length` to payload bytes — so no PTS is possible,
  ever. We have not seen `0xBF` carriage in any capture; the `0xBD`
  form already covers both timed and untimed (`carries_pts = false`)
  payloads.
- **Trigger to revisit:** A consumer with real `0xBF` streams — either
  needing to mux them or to round-trip them through the demuxer.
- **Scope when added:** A stream_id selector on `StreamSpec::Data`
  (default `0xBD`), validation forcing `carries_pts = false` for
  `0xBF`, and the slimmer header-less PES emission path.

## RTCP statistics reporting (RFC 3550 §6.4 Sender/Receiver Reports)

- **Status:** Deferred. The RTCP encoders are correct — SR (Sender
  Report) and RR (Receiver Report) packets are transmitted on the RTCP
  schedule — but all statistics fields are placeholder zeros: NTP
  timestamp, RTP timestamp, sender packet and octet counts in SR, and all
  RR report-block fields (fraction lost, cumulative lost,
  extended-highest-sequence-number, interarrival jitter, LSR, DLSR).
  The receive-path round-trip-time estimate is part of this deferral:
  `RtcpStats::rtt_us` always reports 0. The earlier computation mixed
  clock domains (it seeded its anchor from the peer's SR NTP midpoint
  instead of echoing our own SR per RFC 3550 §6.4.1) and produced garbage
  on the rare path where it fired, so it was removed rather than shipped
  wrong; a real RTT needs the LSR/DLSR feedback loop described below.
- **Why deferred:** Populating the SR sender fields is a contained
  counter-sharing refactor, but full RR report blocks require a
  substantially larger new subsystem: RFC 3550 §A.1 sequence and cycle
  tracking, §A.8 interarrival-jitter estimation, and loss accounting must
  be built from scratch on the receive side, along with UDP-side RTCP
  reception (not yet implemented) to supply the LSR/DLSR feedback loop.
  Additionally, the production RTSP server and client paths emit no RTCP
  today; adding statistics reporting on those paths is greenfield wiring.
  The combined scope is sizable enough to be its own release-cycle
  deliverable.
- **Trigger to revisit:** A consumer needs RFC 3550–conformant RTCP
  statistics or RR feedback — for example, for congestion control, QoS
  monitoring, or interoperability with an endpoint that acts on received
  SR/RR values.

## `RtspSession.stats()` / `RtspStats` binding accessor (Python / JVM)

- **Status:** Deferred. Both bindings expose an `RtspSession.stats()` (JVM)
  / `RtspSession.stats()` (Python) accessor returning an `RtspStats`
  record/dataclass, but every field always reports zero — there is no
  Rust-core `RtspSession::stats()` counterpart these call, and no other
  code path ever constructs a non-default `RtspStats`. The accessor is
  kept (removing it would be a binding API break for no gain), but its
  docs on both bindings are explicit that it is reserved, not partially
  wired.
- **Why deferred:** Wiring real values here needs the underlying RTCP
  receive-side subsystem described in the entry above (RR report-block
  tracking, jitter/loss accounting, UDP-side RTCP reception) — this
  accessor is a thin projection of that subsystem, not a separate gap.
- **Trigger to revisit:** Ride the RTCP statistics reporting entry above;
  once `tst_rtp` has a real per-session stats source, project it through
  both bindings' `stats()` accessors.

## ST 0605 Nano Precision Time Stamp Pack

- **Status:** Not implemented. The `tst_core::klv::st0605` module
  decodes the standard 9-byte Precision Time Stamp Pack (the only
  mandatory pack in ST 0605.7). The Nano Precision Time Stamp Pack
  (10-byte body, nanosecond resolution, distinct UL) defined in the
  same standard is not yet handled — decoding it returns an unknown-key
  error.
- **Why deferred:** No capture in the test corpus uses nanosecond-
  resolution timestamps; the standard Precision Time Stamp Pack
  (microsecond resolution) covers all observed real-world payloads.
- **Trigger to revisit:** A consumer produces or ingests ST 0605 Nano
  Precision Time Stamp packs and needs sub-microsecond timestamp fidelity.

## Cross-thread receive cancellation for UDP / RIST

- **Status:** Not implemented for the `tst-udp` and `tst-rist` receive
  paths. SRT (`SrtCancelHandle`), RTP (`RtpCancelHandle`), and TCP
  (`TcpCancelHandle`) all expose cloneable cross-thread cancel handles;
  `tst-udp` and `tst-rist` have no equivalent. Both `recv_bytes` and
  `close` take `&mut self`, so calling `close()` while a `recv` is in
  flight is not possible in safe Rust — there is no race-free way to
  interrupt a live receive from another thread. Cooperative shutdown
  requires a finite per-call timeout plus a caller-side stop flag checked
  between calls. The Python bindings document this explicitly: "there is
  no race-free way to interrupt a live recv(); close() is only safe to
  call after recv() returns."
- **Why deferred:** Cooperative timeout-based shutdown covers the
  operational need for graceful teardown. A cancel handle is permanent
  public API on two crates plus up to three binding mirrors; no consumer
  has asked for it on these transports. This is precisely the gap that
  `SrtCancelHandle` / `RtpCancelHandle` / `TcpCancelHandle` close on
  the other transports — a future UDP/RIST cancel handle would follow
  the same shape.
- **Trigger to revisit:** A consumer needs to interrupt a parked
  UDP or RIST receive from a thread that does not own the transport
  (for example, a signal handler that cannot reach the transport
  object), or the Python bindings need to lift the documented single-
  thread recv/close contract for these transports.

## RTP H.264 depayloader (RFC 6184)

- **Status:** Shipped — v0.2.x (PRs #94 / #95 / #96). The
  depacketizer (`H264Depacketizer`), receiver shell (`H264Receiver`),
  and RTSP path (`setup_h264_auto` / `into_h264_receiver`) are all
  implemented in Rust (`tst-rtp`) and mirrored in the Python and JVM
  bindings. Covered: single-NAL unit, STAP-A aggregation, FU-A
  fragmentation (packetization modes 0 and 1). Design document:
  `docs/specs/2026-07-10-tst-rtp-rfc6184-depayloader.md`.
- **What remains open:** See the four entries below
  (C-ABI receiver, payloader send side, interleaved mode 2, and
  RTP jitter/RTCP).

## C-ABI H.264 receiver family

- **Status:** Deferred. No C-ABI `H264Receiver` / `H264DepayConfig` /
  `H264Au` family exists; the H.264 ingest path is only available from
  Rust, Python, and JVM.
- **Why deferred:** The C ABI grows additively (currently minor 20);
  adding an H.264-specific receive family would bump it further and
  requires cbindgen-friendly struct definitions (no opaque Rust types
  passed by value). The existing Python + JVM mirrors cover all current
  consumers.
- **Trigger to revisit:** A C or embedded consumer asks for first-party
  H.264 ingest (e.g. a bare-metal pipeline receiving from a STANAG 4609
  RTSP camera). Would be ABI minor 20 → 21.

## H.264-over-RTP payloader (RFC 6184 send side)

- **Status:** Deferred. The library can send MPEG-TS-over-RTP (RFC 2250,
  PT=33) via `MuxSender<RtpTransport>` / the RTSP `MountHandle` push
  family, but there is no way to send a bare H.264 elementary stream in
  RFC 6184 fragmented form.
- **Why deferred:** No consumer has asked for it. The STANAG 4609 pipeline
  shape — mux video + KLV + audio into MPEG-TS, then send over RTP —
  is sufficient for all current deployments; bare H.264 RTP output is a
  camera-side concern.
- **Trigger to revisit:** A consumer needs to push H.264 to an ONVIF
  recorder or WebRTC gateway that does not accept MPEG-TS.

## RTP interleaved mode 2 (STAP-B / MTAP / FU-B / DON)

- **Status:** Deferred. RFC 6184 §5.7.4–§5.7.5 interleaved mode
  (STAP-B, MTAP16, MTAP24, FU-B, all carrying a Decoding Order Number)
  is rejected at SETUP time with `UnsupportedPacketizationMode(2)`.
  Modes 0 (single-NAL) and 1 (non-interleaved FU-A / STAP-A) work with
  all cameras tested.
- **Why deferred:** Mode 2 requires a full DON-reorder buffer and out-of-
  order packet assembly. No production camera tested in the field uses it;
  all use mode 1.
- **Trigger to revisit:** A specific camera known to advertise mode 2
  appears in a deployment; the DON machinery is designed and fuzz-tested
  before shipping.

## RTP jitter/reorder buffer + H.264-path RTCP

- **Status:** Deferred. The `H264Receiver` processes packets in arrival
  order without a jitter buffer or sequence-number reorder window. RTCP
  RR/SR emission on the H.264 path is also not implemented (v1 decision).
- **Why deferred:** A jitter buffer requires a configurable depth and a
  flush strategy that interacts with the access-unit boundary rules (a
  reordered FU-A fragment may arrive after the next AU's start). The
  design is non-trivial. RTCP SR/RR support exists on the MPEG-TS-over-
  RTP path (interleaved pump) but was not wired to the H.264 path.
- **Large-payload handling (resolved 2026-07-11).** Receive-side
  `max_payload()` now reports each transport's *deliverable ceiling*
  (RTP: 65523; SRT: at least the 1456 live-mode wire max; RIST/UDP:
  65535) instead of the send-side packet-size budget, so the
  `Receiver` / `DemuxReceiver` shells size their buffers to accept any
  legal message from a conformant foreign sender. Full-MTU 7×188 RTP
  bundles, 1456-byte SRT messages, and oversize RIST blocks are all
  delivered intact instead of surfacing `Broken`, being silently
  truncated, or being silently dropped.
  `H264Receiver` was never affected.
- **Trigger to revisit:** A deployment sees significant packet
  reordering (e.g. multi-path satellite link) and requires in-order AU
  reconstruction, or RTCP RR feedback to the sender is needed for
  adaptive bitrate control.

## RTP receive-deadline bindings parity (C / Python / JVM) — RESOLVED 2026-08-21

- **Status:** RESOLVED. The `?recv_timeout=<ms>` URL key (already
  reaching all four Rust constructors) plus a typed per-call deadline
  are now honored across all three bindings. C ABI covered 2026-08-20
  (PR #165, no new symbols needed): `tst_rtp_recv_open` /
  `tst_rtp_demux_receiver_open` now also honor `?recv_timeout=`
  (previously only the RTSP-converted path applied it); expiry
  surfaces as the existing `TST_E_BUFFER_FULL` (-4), retryable,
  documented on `tst_rtp_receiver_recv_ts` /
  `tst_rtp_demux_receiver_next_event`. Python (PR #166) gained
  `timeout_ms: Optional[int]` keyword args on `recv()`/`recv_au()`
  (layered on top of, not replacing, the URL-configured persistent
  deadline) and a typed `RtpError(TIMEOUT)`. JVM (PR #167) mirrors it
  with `recv(Integer timeoutMs)` / `recvAu(Integer timeoutMs)`
  overloads plus a new `RtpException.Kind.TIMEOUT`, and additionally
  ships a checked `DemuxReceiver.recvEvent()` so a demux-side timeout
  is also a typed `TIMEOUT` rather than an ambiguous EOS-shaped `null`
  from the plain iterator-style `next()`.

## `MuxSender::finish` bindings parity

- **Status:** Deferred. `MuxSender::finish()` (fallible graceful
  shutdown: drain `pending_bytes` to the live transport, report the
  drain outcome, then close) is Rust-only; the C ABI, Python, and JVM
  surfaces expose only the prompt `close()` (cancel-first, pending
  abandoned) and `Drop`-equivalent teardown.
- **Why deferred:** shipped Rust-first at the 0.5.0 release gate as the
  resolution of the interop-arc close-ordering finding — the prompt
  `close()` contract had to stay unchanged for every binding, and no
  binding consumer has yet asked for an error-reporting lossless
  shutdown (the drop-don't-close workaround remains available and
  equivalent-minus-error-reporting there).
- **Trigger to revisit:** the first binding consumer that needs to know
  whether the buffered tail was delivered on shutdown — the same
  capture/gateway consumers the `FileTransport::finish` surface serves
  in Rust.

## `ManagedRecvTransport::max_payload` during reconnect

- **Status:** Resolved 2026-07-11 (recv API pass). `ManagedRecvTransport::max_payload`
  now caches the deliverable ceiling from the most recent live inner transport
  (set at construction, refreshed after each successful rebuild) and reports
  that cached value during the reconnect window instead of the old fixed 1316-byte
  fallback. Direct-caller edge only — pipeline shells (`Receiver` / `RawReceiver`
  / `DemuxReceiver`) size their receive buffers at construction and were
  unaffected. See the `### Fixed` entry in the `[Unreleased]` CHANGELOG section.

## Recv-side `pkt_size` knob (inert since the recv-ceiling change)

- **Status:** Resolved 2026-07-11 (recv API pass). The inert receive-side
  `pkt_size` knobs were removed as a breaking pre-1.0 change: Rust
  `RtpRecvSocketBuilder::pkt_size` / `UdpRecvTransportBuilder::pkt_size` /
  `RistRecvTransportBuilder::pkt_size`, Python `tstrans.rtp.Receiver(pkt_size=)`
  kwarg and the udp/rist receive builders' `pkt_size` methods, and JVM
  `org.tstrans.rtp.Receiver.fromUrl(url, pktSize)`. Receive-side URLs now
  actively reject `?pkt_size=` with a teaching error rather than silently
  ignoring it. Send-side `pkt_size` everywhere and TCP's receive-side
  read-granularity knob are unchanged. See the `### Removed` and
  `### Changed` entries in the `[Unreleased]` CHANGELOG section.

## ST 0604 Commercial Time Stamp (UTC wall-clock SEI, `payloadType=21`)

- **Status:** Deferred. The library's ST 0604 support covers the MISP
  Precision Time Stamp (Class 0 microsecond and Class 1 nanosecond
  precision) via `MispTimestamp::micros` / `::nanos` and
  `push_video_misp_to`. The "Commercial Time Stamp" sub-family
  (`payloadType=21`, UTC wall-clock values tied to the video bitstream
  via H.264 SPS `vui_parameters` / HRD timing / `pic_timing` SEI /
  `time_code` SEI) is not implemented.
- **Why deferred:** Commercial Time Stamp requires tightly coupling the
  SEI payload to SPS HRD / VUI timing parameters (CPB removal delay,
  coded picture buffer, output delay). The library does not own or
  inspect the encoder; reproducing a conformant `pic_timing` SEI from
  outside the codec pipeline is error-prone and would require at
  minimum parsing and echoing the SPS HRD fields from the caller's
  video stream. No current consumer has requested it.
- **Trigger to revisit:** An integrator requests timecode interop
  with a receiver toolchain that consumes `payloadType=21` UTC
  wall-clock SEI, specifically requiring the SPS/HRD coupling.

## H.262 / MPEG-2 video user_data timestamps (ST 0604 §10)

- **Status:** Deferred. ST 0604.6 §10 defines timestamp embedding for
  H.262 (MPEG-2 Video) via MPEG-2 user_data bytes in picture headers.
  The library has no H.262 video carriage path — `VideoCodec::H262` is
  not a variant; H.262 PIDs surface as `Unknown` on the demux side.
- **Why deferred:** H.262 write-side carriage (a new `VideoCodec`
  variant + PES `stream_id` selection + sequence/GOP/picture header
  emitter) is a full video-carriage scope item tracked under the MPEG-2
  Video roadmap item (P5). ST 0604 timestamp embedding rides with that
  item, not independently.
- **Trigger to revisit:** The P5 MPEG-2 Video carriage roadmap item
  is prioritized, OR a consumer specifically requests legacy-capture
  read-side H.262 user_data timestamp extraction.

## AV1 / H.266 MISP timestamp carriage (ST 0604 future extension)

- **Status:** Deferred. `push_video_misp_to` / `send_video_misp_to`
  splice the MISP SEI for H.264 and H.265 access units only. AV1 and
  H.266 are not supported.
- **Why deferred:** MISB has not defined a standardized MISP timestamp
  carriage shape for AV1 or H.266. AV1 has no SEI NAL unit mechanism
  (metadata OBUs are defined but no SMPTE-registered MISP payload has
  been specified); H.266 has SEI but the MISB spec does not currently
  reference it. Implementing an unstandardized shape risks interop
  failures with receivers that await the official definition.
- **Trigger to revisit:** MISB publishes a carriage spec for AV1 or
  H.266 MISP timestamps (either via MISB ST 0604 revision or a new
  supplemental spec), or a consumer reports a concrete interop
  agreement with a known receiver on a proposed shape.

## EG 0104 legacy "Predator" metadata decode

- **Status:** Deferred. The EG 0104 metadata format is a pre-ST 0601
  legacy format used in early STANAG 4609-era captures ("Predator
  metadata"). It is not an ST 0601 variant — it has a completely
  different tag numbering, encoding rules, and payload shape. No decode
  path exists; an EG 0104-bearing PES surfaces as `Unknown` metadata.
- **Why deferred:** EG 0104 is formally withdrawn in favor of ST 0601;
  no current consumer workflow ingests it. Adding a decoder means
  acquiring the spec (it is a publicly available MISB document) and
  writing a full per-tag decode table that would carry permanent
  maintenance weight for a format with no new producers.
- **Trigger to revisit:** A consumer requests legacy-capture interop
  for EG 0104-bearing `.ts` files, with a concrete corpus of files to
  validate against.

## MISMMS 30-second reporting-cadence tracker (stream-level)

- **Status:** Deferred. `klv::st0601::validate_mismms` is a
  per-record snapshot check — it tells the caller which of the 10
  required ST 0902.8 Table 1 fields are absent or out of range in a
  single decoded `UasDatalinkLs`. It does not track cadence across
  records: ST 0902.3 §4 and ST 1204.1-34 both require stream-level
  compliance (at least one conformant KLV record per 30 seconds for
  MISMMS, or at least one Core ID record per second for ST 1204), and
  the per-record validator cannot check this without state across calls.
- **Why deferred:** Stream-level cadence tracking is consumer-owned
  state (a rolling timestamp window + counter), not a record-level
  decode concern. Adding a stateful `MismmsTracker` struct that the
  caller ticks on each record would expand the API surface meaningfully
  for an audience that today can implement the 30-second window in ten
  lines. No conformance-audit tooling consumer has asked for it.
- **Trigger to revisit:** A conformance-audit pipeline or a recording-
  system consumer asks for a ready-made cadence tracker rather than
  implementing the window themselves.

## Binding-side `MuxSender` MISP timestamp mirrors (Python / JVM)

- **Status:** Deferred. `Muxer::push_video_misp_to` and the C ABI
  `tst_muxer_push_video_misp_to` are the only shipping call sites for
  ST 0604 MISP timestamp splice. The `MuxSender` pipeline-shell
  wrappers in Python (`tstrans.srt.MuxSender` / `ManagedMuxSender`,
  `tstrans.rtp.MuxSender`, etc.) and in the JVM
  (`org.tstrans.srt.MuxSender`, etc.) do not expose a
  `send_video_misp_to` / `sendVideoMispTo` method.
- **Why deferred:** Follows the binding DTS precedent: the
  `push_video_misp_to` / `send_video_misp_to` method family was
  added to `Muxer` and `MuxSender` in Rust, then mirrored into the C
  ABI at `bindings/c` in the same arc (PR #53 / BIND-01). The Python
  and JVM binding-shell mirrors follow separately once a binding-user
  request drives them. The Rust `MuxSender::send_video_misp_to` already
  ships; it is the Python/JVM shell layer that is missing.
- **Trigger to revisit:** A Python or JVM consumer asks for MISP
  timestamp push through the pipeline-shell layer rather than using
  `Muxer` directly.

## RTSP mount push-surface parity: DTS in bindings, MISP variants

- **Status:** Rust-only / partially absent. `MountHandle` gained
  `push_video_to_with_dts` (mirror of `MuxSender::send_video_to_with_dts`)
  from an integrator field-report request — Rust only. The Python, JVM,
  and C mount push surfaces still expose PTS-only video push, so streams
  served from a binding-managed mount are always DTS == PTS. Separately,
  the mount surface has no MISP variant at all
  (`MuxSender::send_video_misp_to` has no `push_video_misp_to` mount
  mirror in any language).
- **Why deferred:** binding mirrors follow the same precedent as the
  `MuxSender` MISP entry above — the Rust surface ships first, binding
  shells follow when a binding consumer asks. The requesting integrator
  consumes the Rust crates directly. MISP-over-mount has no requester in
  any language yet.
- **Trigger to revisit:** a Python/JVM/C consumer replaying captures
  through an RTSP mount (DTS mirrors), or any consumer wanting ST 0604
  MISP timestamps on served streams (MISP mount variant — mirror the
  `Muxer::push_video_misp_to` signature when it comes up).

## Input-consumption detail on binding send errors

- **Status:** Rust-only. `MuxSenderError`/`SenderError` carry
  `input_consumed`; Unreleased, first release will be v0.4.0. The C ABI,
  Python, and JVM send errors expose only the error kind.
- **Why deferred:** the C surface needs an ABI minor bump for a new error
  field; Python/JVM callers are steered to the `Managed*` wrappers, where
  the question does not arise. No binding consumer has asked.
- **Trigger to revisit:** a binding consumer hand-rolling retry on the
  bare shell, or the next planned C ABI bump (piggy-back the field).

## HLS: keyframe-driven-intent signal (segment-0 mid-GOP window)

- **Status:** Deferred to a post-v0.3.0 release (maintainer decision
  2026-07-13; HLS is still maturing as a supported feature). The segmenter
  enters keyframe-driven mode at the FIRST explicit cut — which
  `MuxPublisher` issues at the *second* keyframe — so segment 0 rides the
  wall-clock (raw `push_ts`) mode. If the first GOP is longer than
  `segment_duration`, segment 0 alone can be cut mid-GOP, breaking the
  PAT → PMT → IDR opening for that one segment. The initial raw-mode
  wall-clock cut is deliberately NOT counted in `forced_cuts` (pinned by
  test), so the window is currently invisible in telemetry. The HLS guide
  and CHANGELOG qualify the boundary guarantee accordingly. Surfaced
  independently by the 2026-07-13 v0.3.0 release-gate static audit (B2).
- **Why deferred:** Only reachable when the encoder's keyframe interval
  exceeds `segment_duration` (the inverse of normal HLS configuration),
  bounded to segment 0, and self-healing at the second keyframe. The clean
  fix is a default-no-op `Publisher` trait method (keyframe-driven intent)
  that `MuxPublisher` calls when the stream-head keyframe opens a segment
  and `HlsPublisher` overrides to pre-arm `note_explicit_cut()` — a trait
  surface change not worth rushing into a release.
- **Trigger to revisit:** the first field report of a mid-GOP segment 0
  (player-join artifact at stream start), or the next tst-hls feature arc
  (LL-HLS / fMP4), whichever comes first. Ship with a regression test
  proving no segment starts mid-GOP for an initial GOP > `segment_duration`,
  and decide the telemetry story (count it in `forced_cuts` or a dedicated
  counter).

## Python/JVM `tracing` diagnostics bridge + structured stream-end reason — RESOLVED 2026-08-21

- **Status:** RESOLVED. The Rust core logs meaningful diagnostics via
  `tracing` (the interleaved pump WARNs on read errors, malformed
  frames, queue floods; the keepalive WARNs on 454); previously the
  Python and JVM bindings installed no subscriber, so all of it
  vanished, and a dying RTSP session was largely indistinguishable
  from a clean end of stream (`recv_au()` returning `None` for both).
  Both pieces now ship in both bindings. The structured "why did the
  stream end" surface shipped Rust-side 2026-08-20 —
  `tst_rtp::StreamEndReason` / `StreamEndReasonHandle`,
  `RtpRecvTransport::{end_reason, end_reason_handle}`,
  `H264Receiver::end_reason` — see
  [troubleshooting.md](/docs/troubleshooting.md#why-did-my-rtsp-stream-end)
  — with a same-day C ABI mirror (ABI minor 20, PR #165):
  `TstStreamEndReason` + `tst_rtp_{receiver,demux_receiver}_end_reason`.
  The opt-in `TSTRANS_LOG` stderr bridge (an `EnvFilter`-driven
  `tracing-subscriber` install, `try_init` so a host's own subscriber
  is never displaced) and the structured `end_reason()`/`end_detail()`
  mirror both shipped in Python (PR #166) and JVM (PR #167) — JVM's
  bridge installs from `JNI_OnLoad`, the one guaranteed one-time
  native-library entry point (Python's installs from the `_native`
  module-init hook).

## ASan under the JVM test suite (tst-jni)

- **Status:** Not run. The nightly sanitizer CI covers the pure-Rust
  crates (`asan`/`tsan` jobs) and the native-linking crates with
  instrumented libsrt/librist/mbedTLS (`asan-native` job), but the JVM
  test suite runs no memory sanitizer. The Gradle test JVM does run
  HotSpot's built-in JNI checker (`-Xcheck:jni`, gating in ci.yml),
  which targets the JNI-*semantic* bug class actually observed here
  (local-ref exhaustion, handle misuse) — a class ASan would never
  catch.
- **Why deferred:** ASan under a JVM means `LD_PRELOAD`-ing the
  sanitizer runtime into the Gradle test JVM with
  `ASAN_OPTIONS=handle_segv=0` (HotSpot uses SEGV internally for
  safepoints and implicit null checks), and the result is high-noise:
  JVM-internal allocations report as leaks and the interceptors
  interact poorly with HotSpot's own memory management. TSan under a
  JVM is effectively impractical. The unsafe native code under
  `libtstjni.so` exercises the same tst-c-core/tst-srt/tst-rist paths
  the `asan-native` job now instruments directly, so the marginal
  coverage is the thin JNI glue itself.
- **Trigger to revisit:** the first memory-unsafety-shaped JNI bug
  (crash in native frames, corruption traced to the JNI layer), or
  tst-jni gaining substantial new `unsafe` surface.

## ASan under pytest (tst-py)

- **Status:** Not run. Same shape as the JVM entry above: the Python
  test suite runs no memory sanitizer; the native paths under the
  `tstrans` extension module are the same core/transport code the
  nightly `asan`/`asan-native` jobs instrument directly.
- **Why deferred:** ASan under CPython means `LD_PRELOAD`-ing the
  runtime into the interpreter running pytest; CPython's allocator and
  extension-module loading generate leak-report noise that needs its
  own suppression curation, and the PyO3 layer is a thin translation
  over already-sanitized Rust surfaces — low marginal coverage for the
  maintenance cost.
- **Trigger to revisit:** the first memory-unsafety-shaped bug reported
  through the Python surface, or tst-py gaining substantial new
  `unsafe` surface.

## RTSP keepalive response handling — RESOLVED

- **Status:** RESOLVED (was: "no 401 reaction / session-dead detection
  on rejected pings" + "keepalive-auth integration coverage"). The
  trigger fired — a field report of an interleaved receive session dying
  mid-flight — and the investigation found the fire-and-forget design
  was also the root cause of a hard session-death defect: keepalive
  responses were queued into the interleaved pump's bounded control
  queue, which nothing drains between main-thread requests, so every
  TCP-interleaved receive session died after `CTRL_QUEUE_BOUND + 1`
  pings (16.5 minutes at the default 30 s cadence), surfacing as a
  clean EOS. Keepalive responses (CSeq ≥ `KEEPALIVE_CSEQ_BASE`) are now
  consumed at whichever site owns reads — the interleaved pump, or the
  main thread's non-pump read path (which previously could misattribute
  a buffered keepalive 200 as the next request's response): a 401
  refreshes the shared challenge cache so the next ping signs against
  the rotated nonce, a 454 flips `session_dead` (surfaced via
  `RtspClient::is_session_alive`), and anything else is dropped. The
  keepalive cadence also now retunes to the server-advertised
  `Session: <id>;timeout=N` at SETUP and binds pings to the session
  (both were frozen at connect-time defaults before). The
  challenged-OPTIONS loopback coverage shipped with it
  (`keepalive_retune.rs::keepalive_pings_authenticate_against_challenged_options`
  drives per-request-authenticated pings against the fixture's
  `challenge_options` mode).

## Python `transmux`: payload substitution beyond KLV (`write_video` / `write_audio` / …)

- **Status:** `Transmuxer.write(ev)` copies any event to the output
  verbatim and `write_klv(ev, new_bytes)` substitutes a KLV payload;
  no substitution variant exists for the other channel kinds (video /
  audio / subtitle / data pass through unmodified or get dropped via
  `drop=`).
- **Why deferred:** KLV patching (`klv.patch_uas_datalink`-style
  anonymization) is the driving use case; no consumer has asked to
  swap a non-KLV payload mid-transmux. The canonical shape is settled,
  though: mirror `write_klv` — `write_<kind>(ev, new_bytes)` validates
  the event subclass, reuses the event's timing and flags, and routes
  through the same PID→handle dispatch as `write()`. Per-channel
  nuances to decide at build time: video substitution takes wire bytes
  (the `push_video_wire_to` path — byte-faithful, and the caller owns
  carriage-correct framing for AV1 binding-mode) with `dts=None`
  preserved as PTS-only and `ev.key_frame` reused unless overridden
  (a swapped payload can change keyframe-ness — probably a kwarg);
  audio / subtitle / data are plain PTS-preserving swaps through their
  existing `push_*_to` routes.
- **Trigger to revisit:** the first consumer request to modify a
  non-KLV payload in flight.

## Python `transmux` v1 limits: single-program scope, `metadata_service_id` passthrough, audio DTS

- **Status:** Three documented-but-previously-unledgered limits
  (docstrings + `docs/languages/python.md` carry them; this entry adds
  the ledger record). (1) **Single-program sources only** — a second
  program raises (guards in `io.py`). (2) **`metadata_service_id` is
  not threaded through**: the Rust demux event DOES recover it
  (`MetadataKind::KlvSyncAuCell { metadata_service_id, .. }`), but the
  Python `DemuxEvent.Metadata` flattens the kind to a tag enum and
  drops the AU-cell header fields, so `_push_klv` re-muxes with the
  muxer default (`0`) even though `push_klv_to` already accepts a
  `metadata_service_id=` kwarg. (3) **No audio DTS on the push API**
  — `push_audio` / `push_audio_to` take `(frames, pts)` core-wide, so
  dts≠pts audio cannot round-trip; benign for MP2/AAC/AC-3 where DTS
  always equals PTS.
- **Why deferred:** (1) multi-program needs a remap-policy design
  (program/PID allocation on the way out) with no consumer to shape
  it. (2) is a small, known fix — expose `metadata_service_id` on the
  Python `Metadata` event and pass it in `_push_klv` — but it is a
  binding-surface addition (`.pyi` + stubtest + sibling-binding parity
  question) for a field that is `0x00` in practice (ST 1402.2 App. B).
  (3) would touch the mux push API across Rust + all bindings (the
  video-DTS precedent: targeted-only `*_with_dts` variants) for a
  case no supported audio codec produces.
- **Trigger to revisit:** (1) the first multi-program transmux
  request; (2) the first stream observed with a non-zero
  `metadata_service_id`, or the next binding-surface wave that touches
  `DemuxEvent.Metadata` anyway; (3) an audio codec with dts≠pts
  joining the mux surface.

## CoT→KLV reverse conversion

- **Status:** Not implemented. `klv::st0805` (and its Python/JVM
  mirrors) convert a decoded ST 0601 UAS Datalink LS record to
  Cursor-on-Target XML in one direction only. There is no function that
  parses a CoT event and produces (or patches) ST 0601 KLV fields.
- **Why deferred:** ST 0805.1 itself defines only the KLV→CoT mapping —
  a reverse mapping isn't a spec gap we're filling, it would be a new
  design with no spec to anchor field choices (CoT's `detail` schema is
  intentionally open-ended per-producer, so a generic CoT→KLV mapping
  has no single canonical source). No current consumer ingests CoT and
  needs it turned back into KLV.
- **Trigger to revisit:** A consumer needs to ingest CoT (e.g. from a
  TAK server or another CoT-emitting sensor) and re-encode it as ST
  0601 KLV for downstream MPEG-TS muxing.

## CoT UDP egress

- **Status:** Not implemented. `klv::st0805` produces CoT XML strings
  only; nothing in the crate opens a socket or sends them anywhere. CoT
  is conventionally distributed over UDP multicast/broadcast (the
  ATAK/TAK ecosystem's usual transport), but that's a transport
  concern, not a KLV-conversion concern.
- **Why deferred:** Layering — `klv::st0805` stays a pure,
  `no_std`-clean conversion function with no I/O, matching every other
  `klv::` submodule (`st0601`, `st0806`, `st1010`, ...). Wiring a sender
  belongs in a transport-facing crate or binding (mirroring how
  `tst-udp`/`tst-tcp` already own egress for TS bytes), not in
  `tst-core::klv`, and no current consumer has asked for one.
- **Trigger to revisit:** A consumer asks for a wired CoT sender (e.g.
  "give me a `CotSender` that takes a `UasDatalinkLs` and multicasts
  the Platform Position event every N seconds").

## SDCC-FLP wire-adjacency preservation on encode

- **Status:** Not implemented. `UasDatalinkLs::sdcc_flps` (ST 0601 Tag
  102, MULTI-INSTANCE) captures each wire occurrence as raw pack bytes
  plus its `preceding_tags` — the Local Set item tags the occurrence
  refines, per the "Refined Source List" binding (ST 0601.19 §8.102).
  On encode, `write_typed_fields` re-emits every `sdcc_flps` entry
  verbatim but groups all occurrences together at Tag 102's
  ascending-tag-order position in the body — it does not reproduce the
  original interleaving with each occurrence's `preceding_tags` (see
  the `SdccFlpField` "Ascending-order emission caveat" rustdoc in
  `klv::st0601::model`). `preceding_tags` still records what the
  original wire order was; the pack bytes themselves stay byte-exact.
  The lower-level `klv::st1010::encode_sdcc_flp_mode2` pack encoder has
  no adjacency concept at all — it emits one self-contained pack from
  caller-supplied std-dev/correlation slices with no notion of a
  surrounding Local Set.
- **Why deferred:** Conformant SDCC emission with arbitrary member
  sets — constructing a fresh `UasDatalinkLs` (rather than replaying a
  captured one) with SDCC-FLP occurrences correctly interleaved at the
  wire positions of the items they refine — would need new encoder
  plumbing to place each Tag 102 TLV mid-body instead of at the fixed
  post-typed-fields position `write_typed_fields` uses today. No
  consumer has asked for constructed (as opposed to captured/replayed)
  SDCC-FLP emission.
- **Trigger to revisit:** A consumer needs to construct — not just
  decode-then-replay — an ST 0601 record with SDCC-FLP occurrences
  correctly interleaved with the Local Set items they refine.

## ST 0903 VTrack standalone LS

- **Status:** Not implemented, and not planned against the spec
  version this crate targets. MISB ST 0903.4/.5 defined a standalone
  "VTrack LS" + "VTrackItem" pack (track-level reporting, distinct
  from the nested `VTracker` sub-pack on each `VTargetPack` — see the
  "Typed nested VMTI Local Sets" entry above, which is a different
  construct and not affected by this entry). ST 0903.6 — the version
  `klv::st0903` implements — formally **removed VTrack LS** (its own
  Revision History: "Removed VTrack LS"). ST 0903.4-97 and
  ST 0903.5-109 through -115 are marked `(Deprecated)` inline;
  ST 0903.4-95/-96 are tagged `[VTrack related]` instead and fall
  under the revision history's VTrack removal.
- **Why deferred:** This isn't a coverage gap in the ST 0903.6
  decoder — VTrack LS is withdrawn from the spec version this crate
  targets, so `klv::st0903` correctly does not model it. The only
  reason to add it would be to read legacy captures still carrying an
  ST 0903.4/.5-era VTrack LS, which is a distinct (older-spec-version)
  concern from the ST 0903.6 typed layer this crate ships.
- **Trigger to revisit:** A consumer needs to ingest legacy ST
  0903.4/.5 captures that still carry the (now-removed) VTrack LS /
  VTrackItem pack.

## TLS interop cells (`tcps` / `rtsps` / `hlss` vs. third-party clients)

- **Status:** Not implemented. The published interop matrix
  ([`docs/project/validation-evidence.md`](/docs/project/validation-evidence.md),
  driven by `scripts/interop/run-matrix.sh`) exercises `udp`, `tcp`,
  `srt`, `rist`, `hls`, and `rtsp` against real third-party tools, but
  every one of those cells runs in plaintext. The TLS-wrapped transports
  this codebase supports — `tcps://` (raw TCP over TLS), `rtsps://`
  (RTSP over TLS), and HLS served with `tls` enabled — have no
  corresponding matrix cell exchanging encrypted traffic with FFmpeg,
  TSDuck, VLC, mpv, or GStreamer.
- **Why deferred:** Each TLS transport needs its own cert/trust-anchor
  setup per cell (a self-signed cert the harness generates and each peer
  tool has to be told to trust or explicitly told to ignore, with
  different flags per tool), which is real additional harness work
  beyond the plaintext matrix's existing peer-command shapes. SRT's own
  encryption (mbedTLS-backed passphrase encryption, not TLS) already has
  matrix coverage via the `srt/*-encrypted` cells — this gap is
  specifically about the three transports that wrap a *TLS* handshake
  rather than SRT's native encryption.
- **Trigger to revisit:** A consumer asks for interop evidence on an
  encrypted `tcps`/`rtsps`/`hlss` deployment specifically, or the
  harness gains a shared TLS-cert-provisioning helper cheap enough to
  extend to these three transports without a bespoke setup per cell.

## `release-validation.sh` consolidation into the interop matrix

- **Status:** Not done. The maintainer's local pre-release validation
  battery (Tier B: FFmpeg differential muxing, a player decode
  compatibility matrix, PTS-rollover/PCR-jitter checks, a soak run, long-
  budget fuzzing, and a real-corpus structural cross-check) predates
  this arc's published interop matrix and soak runner
  ([`docs/project/validation-evidence.md`](/docs/project/validation-evidence.md)) and
  overlaps it in places — both now separately exercise "does this decode
  in a real player," "does FFmpeg round-trip this correctly," and
  "does this survive a multi-hour run." The two batteries have not been
  consolidated; both still run as separate steps at release time.
- **Why deferred:** The pre-release battery also covers the sensitive
  real-world capture corpus and long-budget fuzzing, which are out of
  scope for the interop matrix's remit (real tools over real synthetic
  traffic, on a stock CI runner). Consolidating the overlapping steps —
  deciding which battery owns the player-decode-matrix and FFmpeg-
  differential checks going forward — is a deliberate design decision,
  not a mechanical merge, and hasn't been made yet.
- **Trigger to revisit:** Before or during a future release-validation
  pass, deliberately decide which steps move into the interop matrix
  (and get retired from the pre-release battery) versus which stay
  pre-release-only (corpus- or fuzzing-dependent steps that don't fit
  the interop matrix's synthetic-traffic, real-tool shape).
