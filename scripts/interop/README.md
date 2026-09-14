# Interop evidence: transport + format matrix

`run-matrix.sh` exchanges the `tst-interop` driver's synthetic MPEG-TS/KLV
traffic with real third-party tools (ffmpeg, TSDuck's `tsp`, GStreamer, VLC,
mpv) over live network sessions AND local per-profile analyzer/decode
probes on this box, across every transport this crate supports (SRT, RIST,
UDP, TCP, HLS, RTSP) and every one of the 12 canonical stream profiles
(`crates/tst-interop/src/profiles.rs`), and writes one evidence JSON file
per "cell" (one peer, one direction, one transport-or-local-probe, one
optional variant like encryption or profile).

This is the arc's core deliverable: real tools talking to this codebase over
a real wire (or a real local decode/analysis pass), not another closed-loop
self-test.

**Validated on linux-x86_64.** linux-aarch64 is expected to work for the
orchestration itself (no arch-specific code) but hasn't been validated
yet, and separately depends on whichever peer tools (ffmpeg, TSDuck,
GStreamer, VLC) are available as aarch64 apt/deb packages on a given box.
Peer tools are discovered at runtime (`have()` in `lib.sh`) — a missing
tool produces a `SKIPPED_TOOL_MISSING` cell, never a fake pass or fail.
Requires `jq` and `python3` on `PATH` in addition to whichever peer tools
you want exercised.

## Running it

```bash
# Everything (all 12 profiles, transport axis pinned to baseline —
# see --profiles's own --help text), default 10s per cell:
bash scripts/interop/run-matrix.sh --outdir /tmp/interop-run

# Just the SRT transport block, while iterating on it:
bash scripts/interop/run-matrix.sh --outdir /tmp/interop-run --cells 'srt/*'

# Just the format axis, a couple of profiles, for a fast local loop:
bash scripts/interop/run-matrix.sh --outdir /tmp/interop-run \
  --cells 'analyze/*' --profiles baseline,h266-klv

# Shorter cells for a fast local loop, longer for more margin on a loaded box:
bash scripts/interop/run-matrix.sh --outdir /tmp/interop-run --seconds 5

# Compact (tens-of-bytes) access units, to reproduce a pre-realism run:
bash scripts/interop/run-matrix.sh --outdir /tmp/interop-run --au-sizes compact
```

Every cell runs at `--au-sizes realistic` by default: GOP-structured
access units (keyframes tens of KB, inter frames single-digit KB,
~1.7 Mb/s at 30 fps) rather than the tens-of-bytes fixtures, so the
traffic real peer tools see has a real encoder's size regime. The
regime a run used is recorded in its `meta.json`.

Output layout under `--outdir`:

```
DIR/
  meta.json        # host, date, seconds-per-cell, AU size mode, tool versions
  inventory.json    # declared {id, profile} multiset + shape (see "Inventory and shape" below)
  cells/*.json      # one RawCell JSON per cell (see report.rs's doc comment)
  logs/*.log        # combined (our side + peer side) log per cell
  work/*             # generated source .ts files, per-cell captures, intermediate metrics
  results.json      # `report merge`'s output (expectations-applied, inventory-checked)
  results.md        # `report render`'s markdown table, same shape as the published evidence page
```

Exit code is `report merge`'s: 0 iff every produced cell exactly matches
`inventory.json`'s declared multiset (see "Inventory and shape" below) AND
every `FAIL` matched a row in `expectations.toml`. That file now carries a
real row for every genuine gap this matrix has surfaced (65 as of task 12,
see "Known, already-evidenced gaps" below) — running the full matrix exits
0. Any *new* `FAIL` an expectations row doesn't already cover still exits
nonzero: see `report.rs`'s module doc for why an unmatched `FAIL` must
never be silently absorbed. Two more ways `report merge` now fails
nonzero, both load-bearing since this arc: an `expected_unsupported` row
whose (cell, profile) actually `PASS`ed this run — a *stale* row, reported
as `stale_expectations` and fatal everywhere (no `--strict` flag, no
warn-only mode: CI, a branch dispatch, and a local run all reject it the
same way) — and `expectations.toml` itself failing to parse because two
`[[expect]]` blocks can both match the same (cell, profile) without
distinct `failure_contains` strings (see the file's own header comment
for the exact rule). `failure_contains` is mandatory on
`expected_unsupported` rows (a row absorbs only the failure it names,
so an unrelated regression on the same cell can never be silently
swallowed); `known_flaky` rows may omit it, since a flake has no single
mechanism string by definition.

## Cell id / tier / direction conventions

- **Cell id**: transport-axis cells are `<transport>/<direction>-<peer>[-encrypted]`,
  e.g. `srt/us-to-ffmpeg`, `srt/tsp-to-us-encrypted`, `rtsp-serve/vlc-probe`.
  Format-axis cells (task 12) add a third segment, the profile name:
  `<axis>/<peer>/<profile>`, e.g. `decode/mpv/h266-klv`,
  `srt-live/tsp-to-us/baseline`. `report merge`'s expectations grammar
  treats the id's segment before the *first* `/` as the axis (`report
  render`'s per-axis table grouping uses the same split) and supports a
  trailing `*` glob, e.g. `srt/*` matches every SRT transport-axis cell,
  `decode/*` matches every decode format-axis cell across every profile,
  `decode/mpv/*` matches every mpv decode cell across every profile.
- **`direction`** in the emitted cell JSON always names *our* role:
  `send` = `tst-interop` pushes or serves; `recv` = `tst-interop` listens
  and receives; `n/a` for cells with no us-side transport role at all —
  the one transport-axis cell where `tst-interop` only brackets a
  peer-to-peer exchange (see `rtsp-consume` below), and every format-axis
  `analyze/*`/`decode/*` cell (local probes against an already-generated
  file, no transport leg on any side).
- **`peer`**: the third-party tool's binary name (`ffmpeg`, `tsp`,
  `gst-launch-1.0`, `cvlc`), or `vlc+ffmpeg` for the one two-peer cell.
- **`tier`**: what the cell asserts, beyond `tst-interop verify`/`recv`
  reporting `pass`:
  - `transparent` — the capture must be **byte-for-byte identical** to
    what was sent (`stream_sha256` equality), plus `tst-interop verify
    --strict`: any `NonConformant` or `Discontinuity` event on the
    capture fails the cell. Used for `tsp` (a pure relay/dump tool with
    no re-encoding path) and `gst` (`srtsrc`/`srtsink` are raw byte
    pass-throughs in this pipeline shape).
  - `remux` — the capture only has to satisfy `tst-interop verify`'s
    profile invariants (video AU/KLV-record counts within the documented
    70% slack, correct codec/carriage, monotonic PTS, etc.) **plus the
    demuxer-independent wire oracles** (`crates/tst-interop/src/oracles.rs`,
    read straight off the bytes by the naive raw-TS parser in
    `rawts.rs`: PMT stream types/descriptors, PCR cadence, AV1
    carriage-mode discrimination, per-program media accounting, an
    actually-observed PTS wrap for `pts-rollover`, and the audio
    codec/cadence checks) — the mode depends on which side captured the
    bytes: an offline `tst-interop verify --file` cell (`send` direction,
    e.g. `us-to-ffmpeg`) always runs `VerifyMode::Strict` — a peer-written
    file is lossless by construction, so a `Discontinuity` found in it is
    a real finding, not tolerable noise — while a live `tst-interop recv`
    cell without `--strict` (`recv` direction, e.g. `ffmpeg-to-us`) runs
    `VerifyMode::Lossy`, so a `Discontinuity` there is counted rather than
    fatal. Either way a `NonConformant` event fails the cell. Used for
    `ffmpeg`/`gst`-decode/HLS/RTSP cells, where the peer
    actively re-packetizes (HLS segmenting, RTSP interleaving) or is known
    to touch PES framing (see the KLV-PTS finding below). See
    `docs/project/validation-evidence.md`'s "What each profile's oracle
    proves" table for the full oracle-to-profile mapping, and
    `crates/tst-interop/tests/mutations.rs` for the proof each oracle
    actually bites (one mutation per oracle, offline, no network).
  - `n/a` — decode-only probes (`rtsp-serve/vlc-probe`, every format-axis
    `decode/*` cell) with no capture file to compare against anything;
    PASS means "no error/fatal marker in the peer's own log" (plus mpv's
    own explicit no-stream-selected check — see lib.sh), mirroring this
    project's own pre-release decoder-compatibility check.
    Also used for every format-axis `analyze/*` cell (a structural/
    counter assertion, not a byte- or profile-invariant comparison).

### Inventory and shape

Before running any cell, `run-matrix.sh` makes a declare pass — every cell
shape is invoked once per `--profiles` entry with `DECLARE_ONLY=1`, which
records the id it would run and returns immediately without touching the
network — and writes the resulting exact `{id, profile}` multiset to
`inventory.json` in `--outdir`. That file's `shape` field is `full-157`
iff `--cells` is the default `*` and `--profiles` is the default full
12-profile list; any narrowing of either flag makes it `subset`.
`inventory.json` also carries `cells_glob`, `profiles`, `allowed_skips`,
and the peer tool versions probed at declare time. `tst-interop report
merge --inventory inventory.json` then compares the cells actually
produced against this declared multiset **exactly** — missing,
duplicate, or extra cells, and any `SKIPPED_TOOL_MISSING` cell whose id
isn't in `allowed_skips`, are all hard merge errors (exit 2, no
`results.json` written), not silent absorption into a smaller census.
The merged `results.json` records the inventory it was checked against
under `.inventory` (`shape`, `declared_cells`, `allowed_skips`), and
`results.md` gets a matching `Inventory:` line. `--allowed-skips LIST`
is a local escape hatch for a box missing a peer tool (a comma-separated
list of exact cell ids or `prefix/*` globs); `interop.yml` never passes
it, so a CI run must produce every declared cell for real. Because
`shape`, `declared_cells`, and `allowed_skips` are all part of the same
fail-closed check as everything else, **the published evidence page may
only cite a `full-157` run with an empty `allowed_skips`** — a `subset`
run (any `--cells`/`--profiles` narrowing) proves less than the
advertised census and isn't evidence of it, and a `full-157` run that
used `--allowed-skips` to tolerate a missing peer tool didn't actually
produce the full census either (`interop.yml` enforces both: it asserts
`allowed_skips | length == 0` alongside `shape == "full-157"`).

## Known, already-evidenced gaps (read before re-chasing these)

**Transport axis** (the 25 `srt`/`udp`/`rist`/`tcp`/`hls`/`rtsp-*` cells,
run against the `baseline` profile only): a full local run (`--seconds 8`,
every peer tool installed) is stable at **8 PASS / 17 FAIL / 0 SKIPPED**,
reproduced identically across independent runs (including a re-run in Task
12, after the `tst-pipeline` "flush pending PES on terminal receive
errors" library fix landed upstream — zero change to any transport-axis
verdict, confirming that fix's scope genuinely didn't overlap these
findings). Every `FAIL` is a genuine, understood finding from developing
and running this script — not a bug in the orchestrator — falling into
five root causes below. This is exactly what `expectations.toml`'s rows
exist to document with a reason + reference, not something to paper over.

1. **ffmpeg `-c copy` corrupts our KLV PID's elementary-stream payload
   (10 of 17 transport-axis FAILs: 9 cells whose only cited mechanism is
   this one, plus `rtsp-serve/ffmpeg-pull` where it compounds with a
   second gap — item 5 below; plus 14 `srt-live/*` format-axis rows: 12
   from 6 profiles × 2 directions using this mechanism directly, plus
   `klv-sync`'s own 2-row variant below — see the "Affects" list at the
   end of this item for the exact profile names and the 5 profiles that
   fail *earlier*, for unrelated reasons, instead).**
   ★Task 12 refined this finding: it is **not** primarily a PTS-loss bug
   as originally described — it is a **5-byte payload truncation** ffmpeg
   applies to every KLV PES packet it demuxes, independent of whether a
   PTS was ever present. Verified byte-for-byte with `tsp -P pes
   --save-es` (extracts the raw elementary stream) on a pure file-to-file
   remux with no network transport involved at all:

   ```
   tst-interop gen --profile baseline --seconds 5 --out in.ts
   ffmpeg -y -i in.ts -map 0 -c copy -copy_unknown -f mpegts out.ts
   tst-interop verify --file out.ts --expect baseline --seconds 5
   # -map 0 is required — ffmpeg's default stream selection excludes
   # data/unknown-codec streams, so without it the KLV PID is silently
   # ABSENT from the output rather than present-but-corrupted, the
   # opposite failure mode, so don't drop it when reproducing this.
   ```

   `tst-interop verify` reports `KLV records: got 0, want >= 35` — the
   identical symptom every live-transport ffmpeg-remux cell in this
   matrix shows. **The PMT itself survives the remux intact**
   (`tspsi in.ts` vs `tspsi out.ts`: both show `Elementary stream: type
   0x06 (MPEG-2 PES private data)` + the `Registration (0x05)` descriptor
   with format identifier `"KLVA"`, only the PID number changes) — so
   this is not a PMT/stream-classification loss, and OUR demuxer's
   classification requirement (stream_type 0x06 + `KLVA` registration
   descriptor) is unaffected. `tsp -P pes --pid <klv> --header` on both
   files shows a spec-compliant, PTS-less 9-byte optional header in
   *both* `in.ts` and `out.ts` (`baseline`'s async/`PrivateData` KLV
   carriage is `carries_pts: false` by design —
   `crates/tst-interop/src/mux_setup.rs`'s `build_config` — so the
   original never had PTS to lose in the first place; confirmed
   independently via `tsanalyze --normalized`'s `pts=0` count on the KLV
   PID in both files, and via `pes=50`/`pes=50` — same PES packet count,
   ruling out packet drops). The actual damage is in the **payload**:
   `tsp -P pes --pid <klv> --save-es` on `in.ts` shows every KLV record
   starting with the 16-byte SMPTE Universal Label
   `06 0E 2B 34 02 0B 01 01 0E 01 03 01 01 00 00 00...` (the ST 0601
   local-set key); the same extraction on `out.ts` shows
   `0B 01 01 0E 01 03 01 01 00 00 00...` — **the leading 5 bytes
   (`06 0E 2B 34 02`) of every single KLV record are gone**, corrupting
   the UL key our demuxer requires to recognize a record at all, hence
   "KLV records: got 0". The missing-byte-count (5) exactly matches what
   a PTS-only PES optional header would occupy, which — combined with the
   companion `klv-sync` finding directly below — strongly suggests
   ffmpeg's mpegts demuxer applies an unconditional 5-byte consumption to
   any elementary stream it classifies as `klv` via the `KLVA`
   registration descriptor, regardless of what the packet's own
   `PES_header_data_length` says.

   **The `klv-sync` profile (`SynchronousMetadata`, stream_type `0x15`,
   which *does* carry a real H.222.0 §2.12.4.2 5-byte `Metadata_AU_cell`
   header + a real PES PTS) shows a *different*, and more revealing,
   symptom** — not "KLV records: got 0" but
   `verify: FAIL (klv-sync): KLV carriage: expected Sync, observed
   {Async}`. Byte-level inspection explains it: `tspsi out.ts`'s PMT now
   reports the KLV PID as **stream_type `0x06` (PrivateData)** — ffmpeg's
   *own output muxer* downgrades `0x15` to `0x06` on remux, even though
   it correctly read `0x15` + the `KLVA` descriptor on input (its own
   stderr identifies the stream as `Data: klv (KLVA / 0x41564C4B)`). And
   `tsp -P pes --save-es` on the remuxed KLV PID shows the SAME 5-byte
   strip — but this time those 5 bytes *were* the genuine
   `Metadata_AU_cell` header (`in.ts`'s payload starts
   `00 00 df 00 32 06 0e 2b 34...` — AU-cell header, then the UL key;
   `out.ts`'s starts directly at `06 0e 2b 34...`). Put together: ffmpeg's
   demux side evidently assumes every `klv`-classified elementary stream
   is AU-cell-wrapped (true for `SynchronousMetadata`, which is the only
   carriage its own muxer/demuxer round-trips a KLV codec_id through) and
   unconditionally strips what it treats as that 5-byte header — correct
   for `klv-sync` (which really is AU-cell-wrapped, hence the stream_type
   downgrade to `0x06`+no-wrap on write-back, since ffmpeg apparently has
   no output path that re-wraps+re-classifies as `0x15`) but destructive
   for `PrivateData`/async KLV (this codebase's convention: raw,
   *unwrapped* KLV bytes on `0x06` — see
   `reference_klv_au_cell_caller_responsibility` — a valid MISB/ST 1402
   carriage ffmpeg's `klv` codec apparently doesn't account for).

   This is an ffmpeg-side mpegts demux/mux limitation with the `klv`
   codec_id, not a bug in this codebase's own PMT signaling or PES
   framing (both independently confirmed spec-compliant, byte-for-byte,
   on both KLV carriage modes) — **not a "our demuxer could legitimately
   accept this" case**: the bytes ffmpeg delivers for async/`PrivateData`
   KLV are genuinely truncated/corrupted (a valid UL key becomes an
   invalid one), so there is nothing for a more-lenient demuxer to
   legitimately accept. `-copy_unknown` and `-fflags +genpts` were both
   tried and neither restores a KLV record count. Affects every
   ffmpeg-remux cell that touches this codebase's KLV PID with `-c copy`,
   full stop — transport-axis: `hls/ffmpeg-pull`, `rist/ffmpeg-to-us`,
   `srt/{us,ffmpeg}-to-{ffmpeg,us}[-encrypted]` (4 cells),
   `tcp/us-to-ffmpeg`, `tcp/ffmpeg-to-us`, `udp/ffmpeg-to-us` (all
   `baseline`); format-axis (`srt-live/us-to-ffmpeg` and
   `srt-live/ffmpeg-to-us`, one profile each): exactly the 6 profiles
   whose KLV PID actually reaches this same UL-key-truncation code path
   with no *other* gap intervening first — `baseline`, `h265-klv`,
   `misp`, `pcr-sparse`, `pcr-tight`, `pts-rollover` — plus `klv-sync`
   specifically (the carriage-downgrade symptom above, same root cause,
   different observable text). The remaining 5 profiles' `srt-live`
   cells on this same axis (`av1-klv-a`, `av1-klv-b`, `h266-klv`,
   `audio`, `two-program`) fail *earlier*, for their own distinct,
   separately-documented reasons (AV1 stream-selection/live-demux,
   H.266 VPS rejection, AAC live-probe, dropped second program — see
   "Format axis: findings beyond `baseline`" below, items 6-9) — this
   UL-key-truncation mechanism never gets a chance to manifest on those.
   `rtsp-serve/ffmpeg-pull`
   (`baseline`, transport-axis) is the one cell where this finding
   *compounds* with a second, unrelated gap — see item 5 below.

2. **SRT (only) loses a small tail even with a paced/lingered `tsp`/`gst`
   sender (3 of 17 transport-axis FAILs; reproduces on every profile's
   `srt-live/tsp-to-us` cell too).** `tsp -P regulate -O srt --caller ...
   --linger 5` (needed — without `-P regulate`, `tsp` bursts an entire
   file's worth of packets almost instantly and the live transport drops
   nearly all of it; see the `-P regulate` note below) and a
   similarly-paced `gst filesrc ! tsparse set-timestamps=true ! srtsink`
   both land comfortably inside `tst-interop`'s 70% nominal-count slack
   (`recv: PASS`), but the received `stream_sha256` does not exactly
   match the source file's — a handful of trailing video AUs/KLV records
   (order of magnitude 3% for `tsp`, ~18% for `gst`, in local testing)
   never arrive. **This is SRT-specific, not a generic "any live recv
   loses a tail" effect**: the identical `tsp -P regulate` pattern over
   RIST (`rist/tsp-to-us`) and UDP (`udp/tsp-to-us`) is **byte-perfect**
   (PASS) in the same run — only the SRT direction shows the mismatch.

   ★Task 12 investigated this twice (bounded investigation both rounds,
   live SRT sessions with wall-clock instrumentation on both sides).
   **Round 1** ruled out a sender-side drain/linger problem:
   `tst-interop recv`'s `Teeing` tap (`crates/tst-interop/src/transport.rs`)
   tallies bytes at the transport boundary — *below* the demuxer, i.e.
   exactly what libsrt delivered to this codebase's own code — and it
   already shows the shortfall (87044/90240 bytes on an 8s baseline
   run), which rules out "we received it but discarded it during our own
   processing" and confirms libsrt itself never handed those bytes to
   the recv side. Tripling `tsp`'s `--linger` (5s → 15s) reproduced the
   **exact same byte count**. Given `recv`'s own process exit landed
   within 3ms of `tsp`'s in that same test, round 1 concluded the cause
   was `tst-interop recv`'s own receive-side deadline
   (`crates/tst-interop/src/recv.rs`'s `seconds + POST_START_GRACE`
   window) closing the connection while `tsp` was still transmitting.

   **Round 2 (fix-round re-review) tested that conclusion directly with
   a script-only mitigation, and it disproves round 1's root cause.**
   Giving `tst-interop recv` a *much* longer window than the peer's
   actual content duration — `--seconds 10` (2s of extra margin) and
   `--seconds 20` (12s of extra margin, 2.5x the source's real 8s) —
   while the peer (`tsp` and, separately, `gst-launch-1.0`) still only
   sends 8s worth of paced data, produced the **identical byte count in
   every case** (87044/90240 for `tsp`, 74072/90240 for `gst`,
   byte-for-byte the same as the original `--seconds 8` run). At
   `--seconds 20`, `recv`'s own deadline would not fire until roughly
   22s after the connection's first byte — vastly more slack than the
   sender needs to finish its 8s of regulated pacing — yet `recv`'s
   process still exited (via a natural end-of-stream signal, not a
   deadline timeout) within ~1ms of `tsp`'s own process exit, with the
   identical bytes missing. **This conclusively rules out
   `tst-interop recv`'s deadline as the cause**, in either direction: no
   amount of additional receive-side waiting recovers the missing bytes,
   because the connection closes (from the *sender's* side) once
   `tsp`/`gst` decide they're done, and whatever wasn't queued for
   `srt_sendmsg()` by then is gone — there is no "still draining" window
   for a longer receive-side deadline to exploit.

   This narrows the true mechanism to something in `tsp`'s (and,
   separately, `gst-launch-1.0`'s) own `-O srt`/`srtsink` output path —
   specifically SRT, since the identical `-P regulate` pacing over RIST
   and UDP is byte-perfect with the same source file and the same
   `tst-interop recv` code on the receiving end. Plausible shape (not
   independently confirmed against `tsp`/GStreamer's own source or
   `--statistics-interval` instrumentation in this round either): the
   regulate/pacing stage's own internal dispatch still has a handful of
   trailing packets queued when it decides "duration reached, stop
   feeding the output plugin," and the SRT output plugin's closing
   sequence (unlike its RIST/UDP counterparts, which apparently flush or
   don't need to) discards whatever was never explicitly handed to
   `srt_sendmsg()` — a `--linger`-*insensitive* loss, consistent with
   round 1's linger test, since `--linger` only governs bytes already
   inside libsrt's own send buffer, not bytes tsp's own dispatcher never
   got around to submitting.

   **No script-only fix exists for this** (per the round-2 evaluation
   above) — the residual work is either upstream (`tsp`/GStreamer's own
   SRT output-plugin flush behavior) or would need this codebase's own
   send-side instrumentation/reproduction outside this driver entirely,
   not a `tst-interop recv` change. Noted here, not filed as a formal
   backlog entry (out of scope for this scripts-only task). The
   `transparent`-tier byte-hash comparison this script does for
   `tsp`/`gst` peer-to-us SRT cells is specifically designed to surface
   this class of gap, not something to loosen away. Affects:
   `srt/tsp-to-us[-encrypted]`, `srt/gst-to-us`, and `srt-live/tsp-to-us`
   for every profile.

3. **`ffmpeg` hangs against a live RIST or UDP listener with nothing ever
   received (2 of 17 FAILs).** `rist/us-to-ffmpeg`: ffmpeg's librist
   listener registers a peer/flow ("Listening peer 2 timed out after
   ~300ms", "Flow ... is dead") but the output file stays empty; ffmpeg has
   to be force-killed (`timeout --kill-after`, exit 137). `udp/us-to-ffmpeg`:
   ffmpeg successfully *probes* the stream (both streams correctly
   identified, output mapping set up, "Press [q] to stop" printed) but then
   never writes a single byte to the output file before being force-killed
   — reproduces identically with `-fflags nobuffer` and with longer
   pre-send settle times, and reproduces with `-loglevel info`/`warning`
   equally, so it isn't a startup-race artifact of this script's own
   timing. Both are peer-side (ffmpeg/librist-input-path) issues, not
   `tst-interop` send-side ones — the identical `send` calls work
   correctly against `tsp` listeners on the same two transports in the
   same run (`rist/us-to-tsp`, `udp/us-to-tsp` both PASS byte-perfect).

4. **`rtsp-consume/vlc-serve-ffmpeg-pull` (1 of 17 FAILs, best-effort by
   design).** VLC's `--sout '#rtp{sdp=rtsp://:PORT/s}'` RTSP serving
   returned a "5XX Server Error" to ffmpeg's DESCRIBE in local testing.
   Wired as `known_flaky`-bound from day one per the plan — see the
   "RTSP-consume has no `tst-interop` transport leg at all" note below for
   why this cell can't exercise this crate's own code either way.

5. **`rtsp-serve/vlc-probe`: "main decoder error: buffer deadlock
   prevented" (1 of 17 FAILs).** Plausibly this crate's own synthetic
   fixture, not the wire protocol: `crates/tst-interop/src/fixtures.rs`'s
   H.264 generator only builds a real, decodable SPS/PPS/IDR on keyframes
   (every 30th frame) — every inter-frame AU is `0xA5`-filler bytes wrapped
   in a bare NAL header, which a real decoder (VLC here; ffmpeg logs the
   same "decode_slice_header error"/"no frame!" pattern on every ffmpeg
   cell too, just without failing the *cell* since `-c copy` doesn't need a
   successful decode) cannot decode. `rtsp-serve/ffmpeg-pull`'s own
   `verify` FAIL additionally shows a real video-AU shortfall (149/240,
   just under the 70% floor) beyond the universal KLV issue above — RTSP's
   TCP-interleaved SETUP/PLAY handshake plausibly costs a bit more startup
   time than a direct SRT/TCP connect. Left as an expectation row rather
   than special-cased with a longer `--seconds` for RTSP specifically —
   that would diverge this one cell's timing from every other cell's
   shared `--seconds` budget for a margin call this close to the 70%
   floor, not a clear win.

### Format axis: findings beyond `baseline` (task 12)

Task 11 only ever exercised the `baseline` profile. Running the format
axis's `srt-live/*` cells across all 12 profiles for the first time
surfaced four more distinct, run-log-verified gaps — none of them the
async-KLV truncation above, even though the symptom text sometimes looks
similar at a glance:

6. **`av1-klv-a`/`av1-klv-b`, two different symptoms depending on cell
   direction.** `srt-live/ffmpeg-to-us` (ffmpeg is the sender; this
   direction's peer command has no `-map 0`, inherited verbatim from the
   transport-axis `srt/ffmpeg-to-us` cell): this codebase's AV1 carriage
   uses PMT stream_type `0x06`, the same generic classification as this
   codebase's own KLV `PrivateData` convention — ffmpeg's stderr
   confirms ("Stream 0, codec bin_data, is muxed as a private data
   stream and may not be recognized upon reading"), and with *neither*
   PID auto-selected by ffmpeg's default mapping, it refuses to even
   open its output ("Output file does not contain any stream", exit
   234) — its SRT connection never establishes, so `tst-interop recv`
   times out on accept. `srt-live/us-to-ffmpeg` (ffmpeg listens; **does**
   have `-map 0`): a different failure — "Error during demuxing:
   Input/output error" partway through, after this codebase's own send
   side confirms it pushed the full 240 video AUs / 80 KLV records
   correctly — plausibly ffmpeg's live (non-seekable) mpegts demux being
   less robust with two ambiguously-classified private-data PIDs than a
   seekable file (not reproduced on any single-PID-per-type profile's
   live-listen direction).
7. **`h266-klv`, total failure on both directions.** ffmpeg's stderr
   explicitly rejects this codebase's H.266/VVC VPS on every parsed
   unit, including keyframes: "vps_video_parameter_set_id out of range:
   0, but must be in [1,15]". The VPS RBSP bytes are `tst-core`'s own
   tested `vps_main10` fixture (`crates/tst-interop/src/fixtures.rs`'s
   `h266_au`, byte 0's top 4 bits = `vps_video_parameter_set_id` = 0).
   H.266/VVC (unlike HEVC) reserves `vps_id=0` to mean a single-layer
   bitstream with no VPS referenced — plausibly ffmpeg's VVC parser
   (recently added, still maturing) doesn't implement that special case,
   though **this was not independently verified against the H.266 V4
   spec text** in this session. Either way ffmpeg never establishes
   valid codec parameters for the PID and writes/receives nothing at all
   ("Output file is empty, nothing was encoded" / zero video AUs on the
   recv side). Potential library follow-up, not filed as a formal
   backlog entry and not changed in this scripts-only task: confirm
   `vps_id=0`'s H.266 spec meaning and consider whether the fixture
   should use a nonzero id for broader third-party-decoder
   compatibility.
8. **`audio`, total failure on both directions.** ffmpeg cannot
   determine this codebase's AAC-ADTS stream's sample rate quickly
   enough over a live (non-seekable, default `analyzeduration=0`/
   `probesize`) SRT source to open its mpegts output at all: "Could not
   find codec parameters for stream 2 (Audio: aac ... unspecified
   sample rate" / "sample rate not set" / "Could not write header
   (incorrect codec parameters?)". ffmpeg's SRT connection never fully
   opens on either direction, so the failure surfaces as a total
   capture/receive failure (video+audio+KLV all zero, or `send`
   reporting `TransportBroken` once ffmpeg's listener never accepts)
   rather than the KLV-specific finding above — an ffmpeg live-source
   auto-probe limitation with this codebase's particular AAC-ADTS
   framing, not specific to KLV at all.
9. **`two-program`, one dropped program each direction, two different
   mechanisms.** `srt-live/ffmpeg-to-us` (no `-map 0`, the same root
   cause as one of the AV1 symptoms above): ffmpeg's default
   single-best-stream auto-selection picks only its highest-ranked video
   stream, silently dropping the second program entirely (never
   surfaced on task 11's `baseline`-only, single-program testing) — on
   top of the usual KLV-payload-truncation finding on whichever
   program's data does get through. `srt-live/us-to-ffmpeg` (**does**
   have `-map 0`): the same "Error during demuxing: Input/output error"
   live-demux robustness gap as the AV1 finding above, this time with a
   2-full-program PSI topology (4 PIDs) instead of two ambiguous
   private-data PIDs — this codebase's own send side confirms both
   programs' full 480 video AUs / 160 KLV records were pushed correctly
   first.

## Multi-day soak (`soak.sh`)

`soak.sh` is the other half of this arc's published evidence: a long-running
two-leg (SRT + RIST) endurance run through an impaired proxy, judged by
`tst-interop report soak` rather than by the cell-based matrix above. See
`soak.sh`'s own header comment for the full topology, prerequisites, and the
detached-launch recipe (`setsid`/`nohup` + `disown` — never a supervising
tool/session wrapper, which can enforce its own lifetime cap short of the
run's actual duration). `--hours` accepts a decimal (e.g. `--hours 0.05` for
a ~3-minute drill), not just whole hours.

`soak.sh` writes two additional declaration/observation files under
`--outdir` that `report soak` now requires:

- **`soak-config.json`** — the run's declared parameters, written before any
  worker is launched: `expected_duration_s`, `rss_cadence_s`,
  `warmup_fraction`, `sampler_end_slack_s`, `expected_worker_exits`.
  `report soak --config <file>` is mandatory; a missing file is a hard
  error, not a fallback to inference.
- **`exits.json`** — one reaped exit status per worker role (`srt-send`,
  `srt-proxy`, `srt-recv`, `rist-send`, `rist-proxy`, `rist-recv` — exactly
  those six; the RSS sampler is killed by `soak.sh` itself on schedule and
  is never recorded here), written at teardown, including any worker that
  died inside the supervisor's end-of-run grace window. `report soak
  --exits <file>` is likewise mandatory.

Those two files feed three new verdicts in `soak-results.json`:

- **`duration_coverage`** — the RSS series must span at least
  `expected_duration_s` minus the sampler's end slack and two cadences,
  so a real run that ended early (a truncated series) fails here.
- **`rss_sample_coverage_<leg>_<process>`** — each process needs at least
  90% of the cadence-implied post-warmup sample count with the gap between
  consecutive samples strictly under three cadences (`< 3 × cadence`); a
  series with fewer than two distinct timestamps fails outright, and no
  `rss_slope_<leg>_<process>` verdict is emitted for that process at all —
  the coverage verdict above is the one that fails.
- **`worker_exits`** — every status in `exits.json` must be `0` unless the
  role is listed in `expected_worker_exits`; a missing `exits.json` fails
  the run.

### Realism knobs

A soak run is not one fixed impairment level against one fixed stream shape.
Everything below is derived from the single `--seed`, so a run reproduces
from its seed alone, and every derived choice is also DECLARED in
`soak-config.json` before launch so `report soak` can check the run did what
it said it would.

| flag | default | what it does |
|---|---|---|
| `--profile auto\|NAME` | `auto` | `auto` draws two DISTINCT profiles from the seed, one per leg. `NAME` pins both legs to one profile. |
| `--schedule-phases K` | `12` | Phases in the seeded impairment schedule both proxies walk. |
| `--schedule-phase-s S` | `TOTAL_SECONDS / K` (floor 1) | Seconds each phase stays in force. |
| `--fixed-impairment` | off | Reverts both proxies to the old single fixed level (`--loss`/`--jitter`/`--delay`/`--reorder`). |
| `--no-corrupt` | tap on | Turns off the per-leg sender corruption tap. |

Corruption and rich ST 0601 KLV are **on by default** on both legs. The tap
runs at `rate=5,min_gap=1000` with per-leg seed offsets (`SEED+1` on srt,
`SEED+2` on rist) writing one `corruption.jsonl` per leg, read back by that
leg's own receiver. Rich KLV runs `--klv-set rich --klv-seed $SEED` on send
AND recv of both legs.

`--profile NAME` is the REPRODUCTION form (bisecting a failure a drawn
profile exposed). `--no-corrupt` and `--fixed-impairment` are the two BISECT
forms — "is this the tap's doing?" and "is this the schedule's doing?".

#### The impairment schedule

Each proxy takes `--schedule seed=$SEED,phases=K,phase_s=Ss` instead of the
four fixed knobs, and walks a phase table that is a pure function of
`(seed, phases)` — echoed into that proxy's stats JSON, with one set of
forwarded/dropped counters PER PHASE. The generator is
`XorShift64::new(seed ^ SCHEDULE_SALT)`; `SCHEDULE_SALT` exists because one
`--seed` feeds the impairment engine, this schedule, the corruption tap and
the profile draw, and unsalted they would walk correlated trajectories.

Each phase consumes exactly six draws, in this fixed order:

| # | field | range |
|---|---|---|
| 1 | `loss` | 0.5–4.0 % effective loss |
| 2 | `burst` | `r < 0.3`, so ~30 % of phases are bursty |
| 3 | `jitter` | 5–40 ms |
| 4 | `reorder` | 0.0–2.0 % |
| 5 | `reorder_hold` | 100–300 ms |
| 6 | `base_delay` | 10–60 ms |

A bursty phase drops in consecutive runs of 3–8 packets, and divides its
`loss_pct` by the mean run length (5.5) to get the per-packet draw
threshold — so a burst phase and a non-burst phase at the same `loss_pct`
lose the same FRACTION of packets, they just lose them in different shapes.

Changing the draw order, the draw count or any range re-generates every
schedule ever produced. Archived evidence quotes its seed, not its phase
table, so that mapping must stay stable. A pinned unit test replays the
seed-1 fixed-mode decision sequence against a SHA-256 for exactly this
reason.

Seed 3 over 4 phases, as an illustration (this is the table the WP's own
verification soaks ran):

| phase | loss % | burst | jitter ms | reorder % | hold ms | delay ms |
|---|---|---|---|---|---|---|
| 0 | 2.72 | yes | 6 | 1.49 | 233 | 47 |
| 1 | 2.15 | no | 26 | 0.15 | 222 | 53 |
| 2 | 1.73 | yes | 25 | 0.22 | 267 | 48 |
| 3 | 3.32 | no | 19 | 0.70 | 173 | 37 |

**The srt leg sets `?latency=1200` on both ends**, sized from that table's
documented worst case: a 300 ms reorder hold plus 40 ms of jitter plus 60 ms
of base delay is 400 ms, plus retransmission headroom over an RTT the base
delay itself inflates. libsrt's default 120 ms TSBPD budget is SMALLER than
the link this schedule emulates, and a packet delivered past its play time is
dropped by TSBPD as loss no injection can explain. That mis-sizing was
invisible until the schedule and the corruption tap first ran together.

#### The per-leg profile draw

`--profile auto` runs `tst-interop pick-profiles --seed N --legs 2`
(`profiles::pick`), which draws distinct profiles from
`XorShift64::new(seed ^ PROFILE_SALT)`, re-rolling duplicates under a bounded
draw budget and erroring if the registry holds fewer profiles than legs. A
long run then also covers a codec/carriage/cadence shape the short interop
matrix only sees for five seconds at a time. Seed 3 draws `klv-sync` (srt)
and `audio` (rist); seed 1 draws `audio` and `av1-klv-b`. `PROFILE_SALT` is
likewise load-bearing and likewise frozen.

#### The declarations, and the phase-integrated drop verdict

`soak-config.json` gains a `corruption` flag and a `legs` map — one
`LegDeclaration { profile, schedule: { seed, phases, phase_s } }` per leg,
written before any worker launches. `parse_soak_config` rejects an unknown
profile name AND an unknown leg key, and `soak.sh` runs `report soak
--validate-only` before launch so a typo surfaces then rather than 72 hours
later. Two new non-provisional verdicts check the run against them:

- **`profile_declared_<leg>`** — the recv report's own `profile` stamp must
  equal the declaration. No declaration passes ("pre-realism config");
  declared-but-recv-carries-none fails.
- **`schedule_declared_<leg>`** — the declaration must match the proxy's own
  schedule echo on `(seed, phases, phase_s)`. Both absent passes (fixed-
  impairment mode). Either side alone fails. Only the three INPUTS are
  compared, because the phase table is a pure function of them.

`drop_rate_consistent_with_impairment_<leg>` now integrates over the echoed
phase table instead of one flat rate: expected drops are
`Σ (forwarded+dropped)ᵢ × rateᵢ` over the per-phase counters, divided by the
total. The verdict detail names the phase count, so a one-phase reading is
`--fixed-impairment` and a twelve-phase reading is the schedule. A stats
file whose phase-counter count disagrees with its own schedule echo fails
LOUD as a malformed artifact — it never falls back to a partial computation.

#### Lossy-mode transport loss

A soak leg crosses a real impaired link, so the receiver verifies in
`VerifyMode::Lossy` and packets go missing for reasons the corruption log
never recorded: the proxy's own configured loss, an SRT/RIST buffer overrun,
the gap a reconnect leaves behind an outage window. The attribution engine
cannot tell such a gap from one the tap made, so in Lossy mode — and ONLY
there; offline `verify` is Strict and byte-for-byte unchanged — it excuses
two narrow things:

- An unexplained **discontinuity-family** signal (`ContinuityJump`,
  `OtherDiscontinuity`) moves out of `unexplained_events` into
  `unexplained_transport_loss`. It stays counted in the verifier's own
  `discontinuities`, which is what the Lossy contract already said to do
  with it. An unexplained `Resync`, `PsiChecksum`, `MalformedPes` or
  `OtherNonConformant` still FAILS: a lost packet does not forge a bad CRC
  or a malformed PES header.
- An undetected or unrecovered injection with a **foreign** continuity jump
  in its window moves to `undetected_lost` / `unrecovered_lost`. Foreign is
  load-bearing: an injection's OWN jump never excuses it, or a `drop` —
  whose only observable IS a continuity jump — would arrive pre-excused and
  its recovery obligation would evaporate.

What that costs is one mutation, honestly: a WITHHELD `drop` log line is not
catchable on a lossy leg by construction, because nothing distinguishes a gap
the tap made and hid from a gap the network made. The mutation test is now an
honest pair, strict-catches / lossy-excuses. The other six classes surface as
resyncs or non-conformances that no packet loss can forge, and their withheld-
line mutations still bite in Lossy.

Separately, the rich-KLV oracles skip a record an injection DAMAGED, via
`Attribution::explains_damage(at, pid)` — true when the receiver ordinal lies
in the window of a resolved injection that either targeted that PID or was a
`truncate`/`garbage` on any PID (those misalign the whole multiplex). Such a
record still counts in `records` and in the new
`KlvRichMetrics.damaged_by_injection`, and all three rich failure strings say
how many were skipped, so a reader can tell "3 of 6000 records are wrong"
from "3 are wrong and 200 more were never examined". An unexplained rich
decode error still fails `klv_rich_decode_clean`. Without this, the tap's
1-2 KLV-PID hits per ten minutes would scale to several hundred guaranteed
rich-KLV failures over 72 hours, and the arc's headline soak could not pass
with its own default settings.

## Corruption tap (`send --corrupt`)

`send --corrupt` wraps the sender's `Transport` in a seeded tap that damages
the muxer's own output on its way to the wire and writes one JSONL line per
injection to `--corruption-log`. A `recv --corruption-log <same file>` peer
reads that log back and judges what the receiver made of the damage: did
every error event have a cause, did the receiver notice everything it was
required to, did the stream produce media again afterwards.

The two halves are joined by the FILE, never by process memory. A receiver
can therefore only ever "know" about an injection through the same evidence
a human would read, and the same judgement can be made live (`recv`) or
offline from an archived capture plus its log
(`verify::verify_bytes_with_corruption`).

**Corruption is a soak and offline-test feature: no matrix cell injects.**
`run-matrix.sh` never passes `--corrupt`, so all 157 cells run a pristine
stream and the census is unaffected by anything in this section.

### Grammar

```bash
tst-interop send --profile P --url URL --seconds N \
  --corrupt rate=PER_10K[,min_gap=PKTS][,classes=a+b+c] \
  --corruption-log PATH [--seed N]

tst-interop recv --url URL --expect P --seconds N --corruption-log PATH
```

- **`rate=PER_10K`** (required) — per-ten-thousand chance that an eligible
  packet is corrupted, so `rate=5` is 0.05 %. `rate=10000` means "every
  eligible packet", which combined with `min_gap` is how the offline tests
  force exactly one injection at a known place.
- **`min_gap=PKTS`** (default `1000`) — floor on the packet distance between
  two injections. It must be `>= 2 x ATTRIBUTION_WINDOW` (1000) and
  `> RECOVERY_BOUND` (600), validated at parse: closer than that, an error
  event could legitimately belong to either of two injections and the report
  would be guessing, so the tap refuses to run rather than produce an
  ambiguous verdict.
- **`classes=a+b+c`** (default: all seven) — `+`-separated, not comma-
  separated, because the spec string itself is split on commas. Weights are
  re-normalised over whatever subset is named.
- **`--seed N`** (default `0`) — the run seed, salted with `CORRUPT_SALT` so
  the tap's PRNG stream is independent of every other seeded component
  sharing the same number (the impairment proxy, the AU-size factory). Same
  seed + same config + same input bytes produce a byte-identical wire and a
  byte-identical log; nothing in the tap reads the clock.
- **`--corruption-log PATH`** is REQUIRED with `--corrupt`, and pointless
  without it (both are rejected). Corruption nobody recorded is
  indistinguishable from a library bug, and defaulting a path would let a
  run destroy its own evidence by overwriting the previous one.

Every parse error is fatal (exit 2): an unknown key, an unknown class, a
missing `rate=`, an out-of-range value. A typo must not silently degrade
into "no corruption", which would make a whole soak run's evidence vacuous
while still reporting PASS.

Both flags are rejected for the `hls://` and `rtsp://` serve schemes, which
have no connect-mode `Transport` to wrap.

### Classes and detectability

| Class | What goes on the wire | `detectable` |
|---|---|---|
| `body_flip` | 1-4 payload bytes XOR-flipped (adaptation-field bytes on a payload-less packet) | only when a flipped offset lands inside a PAT/PMT section's body-or-CRC span; otherwise never |
| `header` | one of four sub-kinds: sync byte destroyed, PID rewritten to 0x1FFE, continuity counter advanced by +2..+7, `adaptation_field_length` overrun. On a PAT/PMT packet only the sync-byte sub-kind is used | always |
| `truncate` | only the first 40..=187 bytes of the packet | always |
| `garbage` | the packet, then 1..=300 inserted bytes, never 0x47 | always |
| `drop` | nothing | only on a media PID, and only on a packet that carries payload |
| `dup` | the packet twice, back to back | never |
| `psi_flip` | one byte flipped inside a PAT/PMT section body, CRC left intact | always |

`detectable` means "a conformant receiver is REQUIRED to notice this", and
it is the only thing the `corruption_detected` verdict judges. It is
deliberately under-claimed rather than over-claimed: an event that lands
inside an injection's attribution window is attributed regardless of the
flag, so claiming less costs nothing, while claiming more would fail a
conformant receiver for damage it was never obliged to report. The
per-class reasoning, all of it measured rather than assumed:

- **`body_flip` is only detectable under a CRC.** The only range of a
  transport stream a receiver is guaranteed to checksum is a PSI section's
  body and CRC-32. Everything else on a PAT/PMT packet is 0xFF stuffing
  (a PAT here is 17 bytes of section and 167 of padding), and everything on
  a media packet is picture bytes or loosely-validated PES header fields.
  A flip at the PAT's `table_id` or `section_length` produces zero demux
  events — the section is discarded before the CRC is ever reached — while a
  flip one byte later, the first body byte, produces `PsiChecksumMismatch`.
- **`body_flip` never touches a PES header.** On a packet that starts a PES,
  the draw range begins after the optional-header bytes. tst-core's PES
  parser accepts `stream_id`, `PES_packet_length`, `header_data_length` and
  33 of the 40 PTS bits as-is, so a flip there is undetectable by contract —
  yet it silently rewrites a PTS or a stream id, and the WIRE oracles read
  those same bytes structurally (`pts_wrap_unexpected`, `av1_carriage_wire`).
  The run would then fail for damage the tap itself declared nobody has to
  notice. Corrupting the picture is what "body" means; corrupting the timing
  is a different experiment, and not one this harness can judge.
- **`header` on a PAT/PMT packet uses the sync-byte sub-kind only.** A CC
  jump on PID 0 produces no demux event at all (tst-core reports continuity
  jumps only for a resolved elementary stream); a rewritten PID just makes
  the section not arrive, and the next repetition of the table covers for
  it; an `adaptation_field_length` overrun needs an adaptation field, which
  this harness's PSI packets do not have. The sub-kind is still DRAWN on
  that path, so the PRNG stream does not depend on which PID the draw landed
  on — only the arm taken changes.
- **`drop` is detectable only on a payload-carrying media packet.** A
  dropped packet is noticed as a continuity jump and nothing else. H.222.0
  section 2.4.3.3 advances the counter only on packets that carry payload,
  so dropping one of this muxer's PCR-only catch-up packets leaves the
  counter sequence intact, and a lost PAT/PMT repetition is covered by the
  next one.
- **`dup` is never detectable.** A repeated packet with the same continuity
  counter is legal (section 2.4.3.3 allows one duplicate); a receiver that
  says nothing about it is conformant.
- **`psi_flip` leaves the CRC intact on purpose**, so the section genuinely
  fails its checksum. It is only schedulable on a PAT/PMT packet that
  carries one complete section; a draw that fires elsewhere keeps the class
  pending until the next PSI packet rather than being spent on a packet that
  cannot carry it.

### Log format

One JSONL file: a header line, then one line per injection, appended as the
run proceeds and flushed per line so a reader tailing it is never more than
one injection behind.

```jsonc
{"header":{"tap_version":1,"seed":7,"rate_per_10k":5,"min_gap":1000,
           "classes":["body_flip", ...],"attribution_window":500,
           "recovery_bound":600}}
{"injection":{"ordinal":41234,"coord":{"pcr_base":8123456789,"since_pcr":37},
              "class":"psi_flip","pid":0,"offsets":[9],
              "before":[71,64,0,...],"after":[71,64,0,...],
              "detectable":true,"psi":true,"pes_start":false}}
```

The load-bearing field is `coord`, not `ordinal`. Sender byte offsets and
packet ordinals are useless at the receiver — the transport loses,
duplicates and reorders, and the corruption itself changes the wire length —
so every injection is logged at a **PCR coordinate**: the most recent PCR
base seen strictly BEFORE the packet, plus the number of packets since the
packet that carried it. PCR bases travel in the stream, so both ends can
name the same instant with no shared clock, and anchoring to the previous
base rather than the packet's own means the anchor survives an injection
that destroys the very packet it lands on. `ordinal` is informational.

Reading fails closed: a line that is neither a header nor an injection, a
missing header, a second header, or a `tap_version` this build does not
understand is an error naming the line.

### Verdicts

`recv --corruption-log` (and the offline equivalent) adds
`metrics.corruption_attribution` to the report plus three verdicts. All three
are enforced in `Strict` and `Lossy` alike; `Lossy` differs only in the two
narrow transport-loss excusals described at the end of this section:

- **`corruption_attributed`** — every error event the receiver surfaced
  (non-conformance, discontinuity, raw-reader resync) lies inside some
  resolved injection's attribution window. An unexplained event fails this
  verdict, so a demuxer that invents errors still fails a corrupted run.
- **`corruption_detected`** — every injection flagged `detectable` produced
  at least one event of the kind its class implies inside that window
  (`header`/`truncate`/`garbage` -> resync, continuity jump or a plain
  non-conformance; `drop` -> continuity jump; `psi_flip` and a PSI
  `body_flip` -> PSI checksum or a non-conformance).
- **`corruption_recovered`** — media (`Sample` or `Metadata`) arrived again
  within `RECOVERY_BOUND` packets. On the affected PID for the classes that
  damage one stream; on ANY PID for `truncate` and `garbage`, which break
  packet sync for the whole multiplex, and for any injection on a PSI PID,
  which carries no media of its own and would otherwise wait forever.

Constants: `ATTRIBUTION_WINDOW = 500` packets, `RECOVERY_BOUND = 600`
packets, `DEFAULT_MIN_GAP = 1000` packets, and `MAX_APPROX_TICKS = 4 × 9000`
= 36 000 ticks of the 90 kHz clock, four 100 ms PCR intervals — the bound on
approximate resolution described below. The realized `min_gap`, attribution
window and recovery bound are all echoed into the log header, and the latter
two into the report as well, so an archived run is self-describing;
`MAX_APPROX_TICKS` is compile-time only.

Two things the verdicts deliberately do NOT do:

- An injection that never resolved is never judged, and one whose recovery
  window runs past the end of the capture is not judged for recovery. Absent
  evidence is not evidence of a failure.
- The whole-capture count floors are scaled by `(1 - injected_fraction)`, and
  events an injection explains are subtracted before the existing
  `nonconformant_event` / `discontinuity_event` fatality rules apply. A run
  that deliberately destroyed part of its own stream cannot be held to a
  clean run's arithmetic — but everything the log does not explain keeps
  failing exactly as it did before.

An injection whose PCR anchor the receiver never saw (the packet carrying it
was destroyed) resolves against the NEXT base instead, with a wider window
to cover the error, and only while that base is within four PCR intervals of
its own. Beyond that bound it stays unresolved rather than being pinned to
the wrong place — which is what keeps a reconnect outage, where the sender
logs on through a gap the receiver never saw, from reporting a whole
outage's worth of injections as undetected.

### Receiver side

`recv --corruption-log PATH` changes three things beyond adding the verdicts:

- The file **need not exist when `recv` starts** and is **tailed** for the
  whole capture, so injections the sender records while the receive loop is
  already running are judged too.
- The independent raw reader (`rawts`) runs in **resync mode**: on a bad sync
  byte it hunts forward for the next 0x47 confirmed at a 188 stride, records
  a resync with its coordinate, and carries on. Without this, deliberately
  destroyed packets would latch a `rawts_sync_loss` failure that says nothing
  beyond "the tap did its job".
- That reader also **checks each PAT/PMT section's CRC-32** and discards a
  section that fails it, counting it in `psi_crc_rejected`. Measured before
  the check existed: a CRC-failed PAT registered a bogus PMT PID and then
  poisoned 600 later sections.

**Start `recv` before `send`.** A receiver that joins a stream already in
progress may misjudge injections the sender logged before the stream's first
PCR: those coordinates carry no anchor and mean "this many packets from the
start of the stream", a position a late joiner never saw and cannot compute.
Injections appended after this receiver has seen its own first PCR are
stranded rather than guessed at — counted `unresolved`, never judged — but
ones already in the log when it opened are taken at face value.

### In a soak run

`report soak` mirrors each leg's attribution as
`corruption_{attributed,detected,recovered}_<leg>`, gated by the
`corruption` flag in `soak-config.json`:

- **declared off** — three passing "corruption tap disabled" verdicts, so a
  corruption-free run's `soak-results.json` is not cluttered with failures
  for a check that was never meant to run. This is what an archived
  pre-tap run deserialises to, and what a soak run produces until `soak.sh`
  turns the tap on.
- **declared on, attribution present** — the three real verdicts, each
  quoting its counts and first offender.
- **declared on, no attribution** — three FAILING verdicts. The tap was
  declared but `recv` never got a `--corruption-log`, which is a harness
  mismatch that must fail loud rather than read as "no corruption observed".

`send`'s own stats JSON carries the sent-side counterpart in
`metrics.corruption`: packets seen, injections, how many were detectable,
per-class counts, and bytes in/out. Its `passthrough_unclassified` counter is
always 0 for this harness's own muxer output — a non-zero value means
something upstream emitted a malformed packet and the run's evidence is not
trustworthy.

### On a lossy leg

A live soak capture verifies in `VerifyMode::Lossy`, where packets go missing
for reasons the log never recorded. `Attribution::finish_with` then excuses
exactly two things, and records each in its own counter so nothing vanishes
silently: an unexplained DISCONTINUITY-family signal moves to
`unexplained_transport_loss` (non-conformances and resyncs still fail), and
an undetected or unrecovered injection with a FOREIGN continuity jump in its
window moves to `undetected_lost` / `unrecovered_lost` (an injection's own
jump never excuses itself). `corruption_attributed`'s count subtracts the
excused events and names them, so the number it quotes always matches the
list it shows. Offline `verify` is always `Strict` and sees none of this.

The rich-KLV oracles separately skip a record an injection damaged
(`explains_damage`), counting it in `KlvRichMetrics.damaged_by_injection`;
all three rich failure strings then say how many records were skipped. Both
mechanisms are described in full under "Lossy-mode transport loss" in the
soak section above.

## Rich ST 0601 mode (`--klv-set rich`)

`--klv-set rich` swaps the 4-tag fixture record for a realistic ST 0601 record
whose tag set varies from record to record on a seeded schedule, carrying a
nested ST 0102 security set. It exists because a producer that emits the same
four tags forever exercises one decode path forever: the rich record is what
makes a capture prove the decoder handles a tag set that MOVES.

The flag is accepted by `gen`, `send` (the `hls://` / `rtsp://` serve modes
included), `recv` and `verify`, paired with `--klv-seed N` (default `0`).
**Sender and receiver must be told the same pair.** A receiver cannot infer
either from the wire, and a mismatched seed is a real failure rather than a
configuration nuisance: it means the records on the wire are not the ones the
sender was supposed to emit.

**The matrix stays compact: no cell sends rich records.** `--klv-set` defaults
to `compact` everywhere and `run-matrix.sh` never passes it, so all 157 cells
carry the byte-identical 4-tag, 50-byte record their expectations were
validated against. That matters most for the `KLV records: got 0` expectation
rows above: they document third-party peers dropping or mangling KLV carriage
FOR THAT EXACT RECORD, with byte-level evidence pinned to its bytes, so moving
those cells to rich records would invalidate every one of them. Rich mode is a
soak and offline-test feature.

### Grammar

```bash
tst-interop gen    --profile P --seconds N --out out.ts  --klv-set rich --klv-seed 7
tst-interop send   --profile P --url URL --seconds N     --klv-set rich --klv-seed 7
tst-interop recv   --url URL --expect P --seconds N      --klv-set rich --klv-seed 7
tst-interop verify --file out.ts --expect P --seconds N  --klv-set rich --klv-seed 7
```

### What a rich record carries

Seven core tags on every record, whatever the schedule says: **1** Checksum,
**2** Precision Time Stamp, **5** Platform Heading, **13/14/15** Sensor
Lat/Lon/Alt, **65** UAS LS Version. Tag 1 is on that list because
`st0601::encode_to_vec` appends a checksum to every record it writes and
`st0601::decode` validates it, so it is always on the wire even though no model
field holds it.

On top of the core, six groups come and go. A group is **all-or-nothing** — a
record carries every tag of the group or none of them, the way a real platform
either has a pose solution this frame or does not:

| Group | Period | Tags |
|---|---|---|
| Pose | every record | 6, 7 platform pitch/roll; 18, 19, 20 sensor relative az/el/roll |
| Frame | every record | 23, 24, 25 frame centre lat/lon/elev; 26-33 the four corner offsets |
| Optics | every 2nd | 16, 17 sensor H/V field of view; 21 slant range; 22 target width |
| Target | every 3rd | 40, 41, 42 target location lat/lon/elev; 56 platform ground speed |
| Security | every 5th | 48 — a nested ST 0102 security local set |
| Identity | every 10th | 3 mission id; 4 tail number; 10 platform designation; 11 image source |

The PERIODS are fixed properties of the generator, never drawn from the seed:
the slow-changing descriptive tags are sent far less often than the per-frame
geometry, which is the shape a real producer emits. Only each group's PHASE —
its offset within the period — comes from the seed.

That is up to 36 tags on one record. Measured sizes: 128-212 bytes for a record
carrying groups, 54 for a core-only one, against the compact record's 50.

### The seeded schedule

`fixtures::rich_presence(seed, seq)` is a pure function — no state, no clock —
so the sender and a receiver-side oracle compute the same answer independently,
and a replay of the same run reproduces it exactly:

1. Seed the PRNG with `seed ^ KLV_SALT`, so the KLV schedule is statistically
   independent of every other seeded component sharing the same run seed (the
   impairment proxy, the corruption tap, the AU-size factory).
2. Draw one phase per group in `RichGroup::ALL` order, every group
   unconditionally — including the period-1 groups whose phase can only be `0`,
   so the period table and the draw order stay independent of each other.
3. Draw the core-only phase, modulo 50.
4. One `seq` residue in 50 is reserved for a record that carries the seven core
   tags and **nothing else**, so a receiver-side oracle has to cope with a
   legitimately sparse record instead of assuming every record carries the same
   tag set. Otherwise a group is present iff `seq % period == phase`.

The draw order is part of the determinism contract: reordering `RichGroup::ALL`
or inserting a draw silently reshuffles every seed's schedule.

### The timestamp grid

Rich records are stamped at the real soak KLV cadence, 10 Hz (the compact
record's 1 s step is a fixture artifact):

```
timestamp_us = 1_700_000_000_000_000 + seq * 100_000
```

The grid is the point. `fixtures::rich_seq_of_timestamp` inverts a decoded
stamp back to the sender's `seq`, so a receiver knows which schedule entry the
record in its hand was supposed to satisfy — with no side channel, and with no
assumption that records arrived in order or arrived at all. A stamp that is
missing, pre-epoch, or off the 100 000 µs grid names no `seq` and is itself a
census mismatch, not a reason to skip the check.

Every numeric field is walked through a helper that stops 5 % short of the
tag's encode range at both ends, so no tag can reach its own limit however long
a run goes. That is the run-1 rule generalised to every tag the rich generator
sets: an unbounded latitude walk crossed Tag 13's +90 encode max 14.5 h into
the first 72 h soak and panicked both senders.

### The three verdicts

A capture judged with `--klv-set rich` gains `metrics.klv_rich` and three
verdicts. All three are decode-based — every record goes through the real
`tst_core::klv::st0601` decoder, the same code a consumer would use, not
through a private parser written to agree with the generator.

- **`klv_rich_decode_clean`** — every record decoded, and none carried a
  `field_errors` entry. Proves the bytes that survived the transport are still
  a well-formed ST 0601 set: a truncated record, a damaged BER length, a tag
  whose bytes no longer make sense all land here.
- **`klv_rich_census`** — every record's observed tag set equals
  `rich_presence(seed, seq)` for the `seq` its own timestamp names. This is
  what makes the mode more than a decoder smoke test: a producer that dropped a
  whole tag group, or shipped one record's tags under another record's
  timestamp, still decodes cleanly. Checking against a schedule the receiver
  computes INDEPENDENTLY from `(seed, seq)` is what catches it, and the failure
  text names the missing and the unexpected tags.
- **`klv_rich_security_nested`** — wherever the schedule demanded Tag 48, the
  nested ST 0102 set decoded with no field errors and carried a security
  classification. Proves the nesting survived intact: a Tag 48 carrying bytes
  no ST 0102 decoder accepts is worse than a missing one, not better, so both
  fail this same verdict.

`metrics.klv_rich` carries the cumulative counters (`records`,
`decode_errors`, `field_error_records`, `census_mismatches`,
`security_expected`, `security_ok`) plus `first_problem`, describing the first
record to trip any of the three — so a report holds one concrete example
alongside the totals. A compact capture carries no `klv_rich` block at all.

## Peer command-line notes (deviations from the plan's starting sketches)

- **`tsp -I file ... -O <srt|rist|ip> ...` needs `-P regulate` inserted**
  between the file input and the live-network output plugin. Without it,
  `tsp` reads the whole file and pushes it essentially as fast as
  `srt_send()`/librist will accept, finishing in tens of milliseconds
  regardless of the file's nominal duration — the live transport's
  congestion control/flow window can't absorb a burst that size, and
  almost everything gets dropped (confirmed: without `-P regulate`, a
  56 KB / 5 s file arrived as ~5 KB / 4 SRT packets before `tsp` exited
  "successfully"). `-P regulate` paces the packet flow to the file's
  PCR-derived bitrate, matching how a real live sender behaves. The SRT
  side additionally needs `--linger 5` on `tsp -O srt` (SRT's own
  "Default: no linger" — an unlingered close discards whatever's still
  queued in libsrt's send buffer at close time, the same drain-before-close
  concern `crates/tst-interop/src/transport.rs`'s own module doc describes
  for this crate's SRT sender).
- **`ffmpeg -copy_unknown`** is required alongside `-c copy -map 0` for
  ffmpeg to carry our KLV private-data stream through a remux at all (its
  default stream-copy mapping otherwise silently drops it) — it does not,
  however, fix the payload-truncation finding above.
- **`ffmpeg -passphrase` position matters.** It's an SRT-protocol AVOption:
  it must sit immediately before whichever `-i`/output URL is the SRT side.
  For `srt://.../ffmpeg-to-us`, ffmpeg reads a plain local file and writes
  to SRT, so `-passphrase` goes *after* `-i $GEN_FILE`, right before
  `-f mpegts srt://...` — putting it before `-i` (matching the sibling
  `us-to-ffmpeg` cell, where ffmpeg's *input* is the SRT side) fails with
  `Option passphrase not found`.
- **`gst-launch-1.0 filesrc ! srtsink` needs `tsparse set-timestamps=true`**
  in between to get any real-time pacing at all — a bare `filesrc` has no
  timestamps on its raw-byte-stream buffers, so nothing downstream can
  sync to wall-clock time without it. `tsparse` derives per-buffer
  timestamps from the stream's own PCR and smooths them
  (`smoothing-latency=100000`, microseconds).
- **RIST profile defaults line up across every tool for free**: this
  crate's `RistConfig::default()`, `tsp`'s `--profile` ("main profile by
  default"), and ffmpeg's `-rist_profile` (`default main`) all default to
  RIST Main profile — no `?profile=` override needed anywhere in this
  matrix.
- **`rist://@host:port` = bind/listen, `rist://host:port` = connect/send**
  is the shared convention across `tst-rist`, `tsp -I/-O rist`, and
  ffmpeg's `rist://` protocol — the same `@`-prefix idiom `tst-udp`/
  `tst-tcp`/`tst-rist`'s own URL parsers use for "this is a receive-side
  bind" (see e.g. `crates/tst-rist/src/url.rs`'s module doc).
- **RTSP-consume has no `tst-interop` transport leg at all.**
  `crates/tst-interop/src/transport.rs`'s `make_recv` only dispatches
  `udp`/`tcp`/`tcps`/`rist`/`srt` — there is no `rtsp://` connect-side
  support (RTSP only appears as a *serve* scheme, driven by `send`; see
  `serve.rs`'s module doc for why HLS/RTSP work that way). So
  `rtsp-consume/vlc-serve-ffmpeg-pull` is peer-to-peer only: `tst-interop`
  contributes the source file (`gen`) and the final verification
  (`verify`), while VLC serves it over RTSP (`--sout
  '#rtp{sdp=rtsp://:PORT/s}'`) and ffmpeg pulls it. Wired from day one as
  a likely `known_flaky` candidate for Task 12 — VLC's `--sout` RTSP
  serving is fiddly and this cell doesn't exercise this crate's own RTSP
  code at all either way.

## Adding a cell

Transport-axis cells are built from one of four shared shapes in
`run-matrix.sh` (`run_send_peer_recv` / `run_peer_send_recv` /
`run_serve_peer_pull` / `run_serve_peer_probe`) — read their doc comments
first; a new transport×peer×direction combination is almost always a
one-line call to an existing shape, not new plumbing. Format-axis cells
(local, no-transport analyzer/decode probes, and the `srt-live/*`
per-profile SRT block) have their own three shapes just below the
transport-axis ones (`run_analyze_ffprobe` / `run_analyze_tsanalyze` /
`run_analyze_tsp` / `run_decode_probe`, plus `srt_live_cells_for_profile`
reusing `run_send_peer_recv`/`run_peer_send_recv` directly) — a new
profile added to `crates/tst-interop/src/profiles.rs`'s registry needs no
new plumbing here either, just `lib.sh`'s `ALL_PROFILE_NAMES` updated (and
`expected_stream_count` if the new profile's program/audio shape isn't
already covered by that function's formula). `lib.sh` holds the
shape-independent primitives (`have`, `free_port`, `cell_timeout`,
`emit_pass`/`emit_fail`/`emit_skipped`, `metrics_only`, plus the
format-axis's `expected_stream_count`/`tsanalyze_ts_line_counters_zero`/
`tsp_analyze_counters_zero`/`DECODE_PAYLOAD_NOISE` family — see their own
doc comments).

**`metrics_only` is load-bearing** — `tst-interop recv`/`verify --json`
both write a `VerifyReport` (`{pass, failures, metrics: {...}}`), one level
of nesting deeper than the bare `CellMetrics` object `send --json` writes
and than what a `RawCell.metrics` field expects (see `report.rs`). Every
call site that has a `recv`/`verify` JSON file must route it through
`metrics_only` before handing it to `emit_pass`/`emit_fail` — passing the
`VerifyReport` file straight through embeds the wrong shape and `report
merge` fails to parse the cell entirely (missing `video_aus` etc.).
