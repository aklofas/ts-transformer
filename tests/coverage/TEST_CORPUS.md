# Test corpus shape catalog

Real-world MPEG-TS captures from gimbaled platforms exhibit more variation
than the synthetic golden fixtures cover. This document catalogs the
structural shapes and content variants we've observed in the wild, so
fixtures dropped into the gitignored `crates/tst-core/tests/fixtures/local/` slot can be
named and asserted against shape rather than against any specific recording.

This file ships with the public repo. It is intentionally anonymized: no
aircraft identifiers, operator names, incident codes, sensor product names,
or geographic locations appear here. Each shape is keyed by its on-the-wire
structural signature, not by who recorded it.

## How fixtures are loaded

Local corpus files belong under `crates/tst-core/tests/fixtures/local/` (gitignored
via the repo-level `**/tests/fixtures/local/` rule). They are consumed crate-relative,
not workspace-root-relative:

- `*.klv` files are decoded by `crates/tst-core/tests/klv/local_fixtures.rs`
  through `tst_core::klv::st0601::decode` (with `decode_unchecked` as a
  checksum-relaxed fallback).
- `*.ts` files exercise the streaming demux/mux paths via
  `crates/tst-core/tests/mpegts/demux_local.rs` and
  `crates/tst-core/tests/mpegts/mux_local.rs`.

The directory is gitignored. Tests pass silently with zero fixtures — the
corpus is for opt-in real-world coverage, not a CI gate.

Recommended fixture naming:

| Pattern | What it exercises |
|---|---|
| `shape-a-*.{ts,klv}` | simple PMT or single-record PES (KLV + h264 ± audio) |
| `shape-b-*.ts` | Shotover-ARS-style PMT (KLV + h264 + private/sync metadata) |
| `shape-c-*.ts` | HEVC-pipeline PMT (alternate live-stream path) |
| `multi-record-pes-*.klv` | wrapper UL + ST 0601 LS in a single PES payload |
| `framed-pes-prefix-skip-*.klv` | encoder-specific framing prefix (non-UL bytes) before the ST 0601 LS |
| `decode-unchecked-only-*.klv` | ST 0601 record with broken/missing checksum |
| `field-error-*.klv` | tags with truncated lengths or out-of-range values |
| `corrupt-pcr-*.ts` | structurally valid TS with damaged PCR/PTS timing |

The PMT shapes above are keyed for tools in `corpus-shapes.toml`; a new shape is added there and here together.

Each fixture's filename should be self-descriptive and free of any
operationally identifying tokens (no callsigns, no incidents).

---

## Shape A — minimal HDMV+KLVA

Most common shape. Universal across multiple unrelated platforms.

```
PMT:
  PID 0x0?  stream_type 0x06  desc: tag=0x05 fmt="KLVA"
  PID 0x0?  stream_type 0x1B  desc: tag=0x05 fmt="HDMV"
 [PID 0x0?  stream_type 0x03  (audio MPEG-1, optional)]
 [PID 0x0?  stream_type 0x0F  (audio AAC, optional)]
```

Properties:

- Single KLV record per PES payload (the simple case).
- Video: H.264, profiles High and Main, both 1280×720 and 1920×1080 seen.
- Audio: zero, one, or two streams (AAC LC, MPEG-1, MP3).

Test goals against shape A:

- `decode()` succeeds on every PES payload extracted from the KLV PID.
- KLV PID is identified via the registration descriptor with format
  identifier `KLVA`; the demuxer must not assume a specific PID number.
- Decoder must handle PES payloads with PES-header optional fields
  (PTS-only, PTS+DTS, ESCR) — the `pes_header_data_length` byte is the
  authoritative skip count.

## Shape B — Shotover ARS encoder family

Distinctive multi-PID layout used by gimbal encoders that emit private
auxiliary data alongside the standard KLV stream. **Not unique to one
platform** — multiple platforms share this encoder family.

```
PMT:
  PID 0x0?  stream_type 0x06  desc: tag=0x05 fmt="KLVA", tag=0x26, tag=0x27, tag=0xFF
  PID 0x0?  stream_type 0x0F  desc: tag=0xFF
  PID 0x0?  stream_type 0x15  desc: tag=0x26, tag=0x27, tag=0xFF        ← private synchronous metadata
  PID 0x0?  stream_type 0x1B  desc: tag=0xFF
  PID 0x0?  stream_type 0xF0  desc: tag=0xFF                            ← user-private (1× to 4×)
  [PID 0x0?  stream_type 0xF1  desc: tag=0xFF]                          ← user-private (sometimes absent)
```

Properties:

- KLV PID's elementary-stream descriptor list contains four entries (KLVA
  + three private tags). The descriptor walker must iterate the full list,
  not stop at the first match.
- Some captures from this encoder family carry a KLV PID **without** an
  ST 0601 record — the elementary stream is present but its PES payloads
  contain only the private wrapper records and no ST 0601 LS to decode.
- The `0x15` PID's tag-set `{0x26, 0x27, 0xFF}` reliably identifies this
  encoder family even when the KLV PID is absent or missing the KLVA
  registration.

Test goals against shape B:

- Multi-descriptor parsing in PMT.
- KLV decode iteration (see "Multi-record PES" below) — captures from this
  encoder family are the primary source of wrapper-prefixed PES payloads.
- Demuxer should accept the unusual `stream_type 0xF0`/`0xF1` PIDs as
  opaque user-private (don't crash, don't claim them as anything specific).

## Shape C — HEVC live-stream pipeline

Alternate output path used by some platforms in parallel with their
ARS-recorded shape-B output. Distinct PMT, distinct codec.

```
PMT:
  PID 0x0?  stream_type 0x24                                            ← HEVC video
 [PID 0x0?  stream_type 0x15  (KLV-tagged private metadata, optional)]
 [PID 0x0?  stream_type 0x0F  (AAC audio, optional)]
```

Properties:

- HEVC video, Main profile, yuv420p, 1920×1080.
- KLV PID, when present, may **not** carry ST 0601 records (the PID
  exists but elementary-stream content is non-ST-0601).
- Some captures carry no audio.

Test goals against shape C:

- Demuxer recognizes stream_type 0x24 as HEVC video and does not require
  any specific descriptors.
- KLV demux is best-effort: presence of a KLV-tagged PID does not imply
  ST 0601 content.

## Multi-record PES: Precision Time Stamp Pack + ST 0601 LS

Some captures emit PES payloads carrying **two adjacent KLV records**:
a Precision Time Stamp Pack first, then the ST 0601 LS:

```
[ Time Stamp UL (16B) ][ BER 0x09 ][ status(1) + µs(8) ][ ST 0601 UL (16B) ][ BER length ][ ST 0601 body ]
└── record 0: Precision Time Stamp Pack ─────────────────┘└── record 1: ST 0601 LS ──────────────────────┘
```

The first record's Universal Label
`06 0E 2B 34 02 05 01 01 0E 01 01 03 11 00 00 00` (CRC 23259) is
registered in MISB ST 0807.27 row 1061 as the **Microsecond Timestamp
Pack**, defined formally in **MISB ST 0605 §7 as the Precision Time
Stamp Pack**. Body layout:

- byte 0: **Time Status** (1 byte) — bit field per MISB ST 0603 §7.4:
  - bit 7: `0` = clock locked to absolute time reference, `1` = lock unknown
  - bit 6: `0` = time incrementing linearly, `1` = discontinuity
  - bit 5: `0` = forward, `1` = reverse (only meaningful when bit 6=1)
  - bits 4-0: reserved, must be `0b11111`
- bytes 1-8: **Precision Time Stamp** (8 bytes, big-endian uint64
  microseconds since 1970-01-01 UTC) per MISB ST 0603 §7.1.

This pairing is the canonical MISP-compliant pattern documented in
**MISB TRM 0909.4 §7** (KLV pipeline). The Precision Time Stamp Pack
gives PES-emit-time; the ST 0601 LS Tag 2 gives metadata-collection-time.
They typically differ by 0–5 seconds depending on the encoder pipeline.

A decoder calling `st0601::decode(&pes_payload)` from offset 0 reads
the Time Stamp Pack UL (registry byte `0x05`, not the ST 0601 LS
`0x0B`), attempts to parse the 9-byte body as ST 0601 fields, and
fails with `Truncated`.

Required handling:

- Iterate KLV records over the PES payload (read 16-byte UL, BER
  length, body; advance; repeat until exhausted).
- For each record, gate on `UniversalLabel::is_st0601_family()` before
  attempting `decode`. Records whose UL identifies a different KLV
  set (e.g., the Precision Time Stamp Pack) should be either skipped
  or routed to a typed handler for that set.
- The ST 0601 record may appear at any offset within the PES, not just
  byte 0.

Test goals:

- A `multi-record-pes-*.klv` fixture (PES payload extracted to a `.klv`
  file) should:
  - Cause `decode(&fixture)` to **fail** (it tries to decode the wrapper).
  - Parse cleanly when iterated record-by-record and decoded only on
    ST 0601-family records.
- A future principled handler should expose the Precision Time Stamp
  Pack's `(time_status, microseconds)` tuple alongside the ST 0601 LS,
  rather than discarding it.

## Synchronous-method 5-byte AU cell header before the ST 0601 LS

Some captures emit PES payloads whose first bytes are **not** a SMPTE
Universal Label, but rather the standard ISO/IEC 13818-1 **Metadata
Access Unit (AU) cell header** specified in MISB ST 1402.2 Appendix B
Table 2 for the *Synchronous Metadata Multiplex Method*:

```
[ 5-byte AU cell header ][ ST 0601 UL (16B) ][ BER length ][ ST 0601 body ]
```

The 5-byte AU cell header encodes (big-endian):

```
byte 0:      metadata_service_id                       (8 bits)
byte 1:      sequence_number                           (8 bits)  — increments per cell
byte 2 bits 7-6: cell_fragmentation_indication         (2 bits)
byte 2 bit  5:  decoder_config_flag                    (1 bit)
byte 2 bit  4:  random_access_indicator                (1 bit)
byte 2 bits 3-0: reserved (must be 1111)               (4 bits)
bytes 3-4:   AU_cell_data_length                       (16 bits) — body length, excludes header
```

Per the spec, this carriage form is intended for **stream_type 0x15**
(metadata in PES packets) flagged by `metadata_descriptor` (tag 0x26)
and `metadata_std_descriptor` (tag 0x27) in the PMT. In our corpus
this header has been observed appearing on a `stream_type 0x06` PID
that is also flagged with the asynchronous KLVA registration
descriptor — most likely a remux artifact where the stream_type label
was rewritten without stripping the AU cell wrappers. Real-world
encoders/muxers do this; consumers should be tolerant of the
mismatch.

A consumer that walks records using only UL+BER iteration cannot
align past the AU cell header — byte 0 (`0x00`) and byte 2 (`0x0F`,
which encodes `cell_fragmentation=00, flags=00, reserved=1111`) are
not valid SMPTE UL leading bytes.

**One file in our corpus carries two KLV PIDs in the same PMT**: one
PID emits wrapper-UL multi-record PES (the multi-record-pes shape
above), the other emits this AU-cell-prefixed PES. Their PESs
interleave on demux, so a consumer that demuxes both KLV PIDs in this
file will encounter both shapes within the same elementary stream
collection.

Two valid handlings:

1. **Spec-compliant**: parse the 5-byte AU cell header, use
   `AU_cell_data_length` to bound the KLV record exactly, and
   pass body bytes to `st0601::decode`. Optionally surface
   `sequence_number` and `random_access_indicator` to the caller for
   loss detection / resync. (Future enhancement; not yet implemented.)
2. **Fallback recovery**: scan the payload for the first occurrence
   of the SMPTE UL prefix `06 0E 2B 34` and decode from that offset.
   This works regardless of whether the bytes preceding the UL are
   an AU cell header, an unknown vendor envelope, or padding.

Test goals:

- A `framed-pes-prefix-skip-*.klv` fixture should:
  - Cause `decode(&fixture)` to fail (the AU cell header bytes are
    parsed as a SMPTE UL with bogus body length).
  - Decode cleanly via fallback recovery (scan for `06 0E 2B 34` and
    decode from that offset).
- A future `mpegts::demux` pipeline that detects sync metadata via
  the `metadata_descriptor` (tag 0x26) PMT entry should preferentially
  use the spec-compliant AU cell parser, reserving the fallback for
  unidentified envelopes.

## ST 0601 field-presence reality

Across the corpus, every KLV-bearing capture decoded cleanly via
`decode()` or `decode_unchecked()`. However:

- **Tag 4 (`platform_tail_number`) and Tag 3 (`mission_id`) were absent
  in 100% of the corpus.** Decoder must treat both as fully optional;
  any unwrap or `expect` on these fields is a bug.
- **Tag 11 (`image_source_sensor`) values are freeform** — short tokens
  (`MWIR`, `EO`, `RGB`, `IR`), product strings with spaces and hyphens,
  and abbreviations. The field encoding is plain UTF-8; the decoder must
  preserve whitespace and casing verbatim.
- **Tag 2 (`timestamp_us`) is reliably present** when KLV is present.
  Use it as the authoritative clock when filename or mtime conflict.

Test goals:

- A no-tail / no-mission fixture round-trips through `decode → encode →
  decode` without spurious field generation.
- Sensor field with embedded spaces and hyphens preserves byte-exact.

## Decode-unchecked path

Some captures have records whose checksum (Tag 1) does not match the
ST 0601 running-sum-16 computation. `decode()` rejects them with
`ChecksumMismatch`; `decode_unchecked()` succeeds. The vast majority of
records in the corpus pass `decode()` cleanly — the unchecked path is a
minority but real fallback.

Test goals:

- A `decode-unchecked-only-*.klv` fixture exercises the fallback branch.
- `decode_strict()` (which also requires the ST 0601 family UL) is
  separately covered against synthetic fixtures.

## Container-damage edge case

At least one capture in the corpus has a structurally valid TS header
but severely truncated PCR/PTS timing — `ffprobe` reports a duration
under one second on a multi-hundred-megabyte file. The PMT and KLV PID
parse normally; the demuxer should not infinite-loop or panic on such
input.

Test goals (planned, alongside `mpegts::demux`):

- Bounded-byte demux: a `--max-bytes` ceiling on the streaming demuxer
  ensures pathological files don't read forever.
- Resync-on-lost-sync-byte: scan forward for the next 0x47 with
  packet-size alignment rather than aborting.

## Multi-PES decode at scale

KLV records are emitted at roughly 25–30 Hz, so a 30-second file carries
~750 PESs on the KLV PID, and a 30-minute file carries ~50 000. The
streaming probe binary (an outside-repo, workspace-internal scratch
tool, not part of this repo) exercises this end-to-end and stops after
a configurable record count.

Test goals (for `mpegts::demux`):

- Demux of 100 000+ KLV records from a single fixture without quadratic
  memory growth or double-buffering bugs.
- Per-PES reassembly handles `payload_unit_start_indicator` semantics
  and adaptation-field skip correctly under typical CC drops.

## Spec compliance summary

Cross-checked against MISB ST 0601 (UAS Datalink LS), MISB ST 0102.12
(Security Metadata), MISB ST 1402.2 (KLV-in-MPEG-TS), and SMPTE ST 336
(KLV encoding):

**Compliant:**

- `UniversalLabel::ST_0601_LS` UL bytes match ST 0601 §6.4 (with version
  byte 0x13 reflecting ST 0601.19); `is_st0601_family()` correctly
  accepts any version byte at position 13 per the spec's evolution rule.
- BER short-form, BER long-form, and BER-OID encodings match ST 0601
  §6.5.2 and SMPTE ST 336.
- 16-bit running-sum checksum across UL through length-of-checksum
  (`checksum_running_sum_16`) matches the example algorithm in
  ST 0601 §6.8.
- Big-endian byte and bit ordering throughout (ST 0601 §6.5.1).
- Linear-range int↔float mapping (`decode_fixed_range`) matches ST 0601
  §7.5. Tags that define INT_MIN as a special value (e.g., Tag 6/7 use
  `0x8000` for "Out of Range") produce `Ok(None)` and the tag number is
  recorded in `UasDatalinkLs::sentinel_tags` rather than `field_errors`.
- KLV-in-MPEG-TS detection via stream_type 0x06 + `registration_descriptor`
  (tag 0x05) with `format_identifier = "KLVA"` (`0x4B4C5641`) matches
  ST 1402.2-03/-19/-25 (Asynchronous Metadata Multiplex Method).
- **Precision Time Stamp Pack** typed handler (`klv::st0605::decode`)
  decodes the `[time_status:1][microseconds:8 BE]` body per MISB
  ST 0605 §7. `TimeStatus(u8)` newtype exposes `is_locked()`,
  `has_discontinuity()`, `is_reverse_jump()`, and
  `reserved_bits_valid()` per MISB ST 0603 §7.4 Table 3.
- **ST 0601 strict-compliance decode** (`klv::st0601::decode_strict_compliance`)
  enforces ST 0601.8-09 (Tag 2 first), ST 0601.8-11 (Tag 1 last),
  and ST 0601.8-12 (Tag 65 present). `decode` remains permissive for
  real-world captures.
- **ST 0601.19 Items 75, 78, 90, 91** typed in `UasDatalinkLs` —
  Sensor Ellipsoid Height, Frame Center Height Above Ellipsoid,
  Platform Pitch Angle (Full), Platform Roll Angle (Full). Brings
  typed coverage from 41 to 45 of the 143 ST 0601.19 items.

**Permissive (deliberate, accepts non-strict captures):**

- ST 0601.8-09 says Tag 2 (timestamp) **must be the first element** and
  ST 0601.8-11 says Tag 1 (checksum) **must be the last**. Our `decode`
  accepts any field order; `decode_strict_compliance` enforces.
- ST 0601.8-12 says Tag 65 (UAS LS Version) **shall be present**. Our
  `decode` treats it as `Option<u8>`. Across the test corpus, all
  KLV-bearing files include Tag 65. `decode_strict_compliance` enforces
  the spec rule.
- Tags 3, 4, 10, 11, 12 are spec-required to be ISO 646 (7-bit ASCII).
  Our impl accepts any UTF-8 string. All observed captures use
  plain ASCII, so this hasn't surfaced as an issue.

- **Universal Label registry alignment**: every UL we observed in the
  corpus appears in MISB ST 0807.27 (the KLV Metadata Registry). The
  ST 0601 LS UL family check (`is_st0601_family`) correctly accepts
  any version byte at position 13, matching the registry's
  effectivity-versioned entries.
- **ST 0107.5 future-proof skip rule** (§ST 0107.3-04): "decoders
  shall skip unknown Local Set values so as to not impact the
  decoding of known items." Our `unknown: Vec<OwnedRawField>` field
  on `UasDatalinkLs` matches this requirement — unknown tags are
  preserved verbatim rather than dropped or causing failure.
- **ST 1607.2 (Constructs to Amend/Segment KLV)**: applies to KLV
  records segmented across multiple PESs. Not exercised by our
  corpus — typical records are 100–300 bytes, well under PES size.
  Our streaming demux logic (`mpegts::demux` future work) would need
  reassembly logic if multi-PES KLV segments appear.

**Compliance gaps surfaced by real-world data (not yet handled):**

- **Synchronous Metadata Multiplex Method (ST 1402.2 §9.4.1)**: the
  5-byte AU cell header (per Appendix B Table 2) is **not parsed** by
  our code — see "Synchronous-method 5-byte AU cell header" section
  above. Fallback recovery (UL prefix scan) handles it correctly; a
  spec-compliant AU cell parser is a future enhancement.
- **Sync-metadata PMT detection (ST 1402.2-15/-16)**: probe-ts and
  the `extract_klv` example identify KLV PIDs only via the async
  `registration_descriptor`. Streams flagged by the sync-method
  `metadata_descriptor` (tag 0x26) + `metadata_std_descriptor`
  (tag 0x27) are catalogued but not demuxed. Captures in the corpus
  with sync metadata also have a redundant async PID, so no real
  records are missed today; a hypothetical sync-only capture would
  go undetected.
**Out of scope (not exercised by the corpus, deferred):**

- MISB ST 0102 Security Metadata Universal Set / Local Set (UL
  `06.0E.2B.34.02.01.01.01.02.08.02.00.00.00.00.00`) — none observed.
- MISB ST 0902 Sensor Minimum Metadata Set — none observed.
- Asynchronous metadata at high record rates (>1 KHz) — corpus
  captures emit at ~25-30 Hz.

## Specs cross-referenced

The compliance summary above was checked against:

- **MISB ST 0102.12** — Security Metadata Universal and Local Sets
- **MISB ST 0107.5** — KLV Metadata in Motion Imagery (baseline KLV rules)
- **MISB ST 0601.19** — UAS Datalink Local Set (current revision; 143 items)
- **MISB ST 0603.5** — MISP Time System and Timestamps (Time Status byte)
- **MISB ST 0604.6** — Timestamps for Class 1/Class 2 Motion Imagery
- **MISB ST 0605.10** — Class 0 Motion Imagery Metadata and Audio over SDI
  (defines Precision Time Stamp Pack)
- **MISB ST 0607.5** — MISB KLV Metadata Registry and Processes
- **MISB ST 0807.27** — MISB KLV Metadata Registry (1168-row UL registry)
- **MISB ST 0902.8** — Sensor Minimum Metadata Set
- **MISB ST 0903.6** — Video Moving Target Indicator Metadata
- **MISB ST 1201.5** — Floating Point to Integer Mapping (IMAPB algorithm)
- **MISB ST 1303.2** — Multi-Dimensional Array Pack
- **MISB ST 1402.2** — MPEG-2 Transport Stream for Class 1/Class 2 MI
- **MISB ST 1607.2** — Constructs to Amend/Segment KLV Metadata
- **MISB ST 1910.1** — Adaptive Bitrate (ABR) Content Encoding
- **MISB TRM 0909.4** — Constructing a MISP Compliant File/Stream
- **MISB RP 0802.2** — H.264/AVC Motion Imagery Coding
- **MISB RP 1011.1** — LVSD Motion Imagery Streaming
- **MISP-2023.2** (cited as mandated) and **MISP-2025.1**

These are the MISB documents from `nsgreg.nga.mil/misb.jsp` directly
relevant to the KLV-in-MPEG-TS pipeline this crate targets. Specs
not yet exercised by the corpus (e.g., MIMD ST 1901-1908, SAR
ST 1206, Range MI ST 1002) were skipped.

## Coverage gaps (synthetic fixtures recommended)

Variants we have not seen in the real-world corpus — synthetic golden
fixtures should fill these:

- ST 0601 records with **all** typed tags populated (the existing
  synthetic golden in `tests/fixtures/st0601/` already covers this).
- Tag 4 + Tag 3 set to non-empty UTF-8 strings.
- KLV PES with adaptation-field bytes spanning the PSI section.
- KLV records >1 MiB requiring BER long-form length encoding (4-byte
  length).
- Asynchronous KLV streaming per ST 1909 §7 (multi-segment records
  spanning PESes).

These are listed for future test authors; not blocking today.
