# Validation evidence

This page publishes concrete, reproducible evidence that `ts-transformer`
interoperates with real third-party tools over a real wire — not just with
itself. [`docs/reference/compatibility.md`](/docs/reference/compatibility.md)
documents what the library implements against the specs; this page documents
what happened when that implementation was pointed at FFmpeg, TSDuck, VLC,
mpv, and GStreamer over live SRT / RIST / UDP / TCP / HLS / RTSP sessions,
plus what happened when it ran for hours under packet loss, jitter, and
reorder.

Every number below comes from the `tst-interop` crate and the two driver
scripts at `scripts/interop/` — nothing on this page is hand-measured or
estimated, except where a sentence says so (the ad-hoc checkpoint readings
and the inferred event positions below). Re-run either script yourself to reproduce it:

```bash
# Build the interop driver (native deps: vendored libsrt + librist + mbedTLS).
SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1 cargo build --release -p tst-interop

# Full transport + format matrix (161 cells; ~8s/cell locally is what
# produced the census below — see scripts/interop/README.md for the
# full cell/tier/profile vocabulary and per-axis `--cells` filtering).
# Every cell runs realistic access-unit sizes by default; pass
# `--au-sizes compact` to reproduce a pre-2026-09-14 run instead:
bash scripts/interop/run-matrix.sh --outdir /tmp/interop-run --seconds 8

# One-hour soak smoke: the 0.7.0 release soak's shape and seed (seeded
# phase schedule, corruption tap, rich KLV) at 1/72nd the duration — the
# 6-hourly outage never fires in one hour. SRT_RECONNECT_MODE is required —
# the script refuses to launch without it; `background` is the mode the
# 0.7.0 release soak ran, `blocking` the mode of the 2026-08-05 run:
SRT_RECONNECT_MODE=background bash scripts/interop/soak.sh --outdir /tmp/interop-soak-smoke --hours 1 --seed 11
```

`run-matrix.sh` requires `jq` and `python3` on `PATH`, plus whichever peer
tools (`ffmpeg`, TSDuck's `tsp`, `gst-launch-1.0`, `vlc`, `mpv`) you want
exercised — a missing tool degrades its cells to `SKIPPED`, never a fake
pass or fail. `run-matrix.sh` runs its full census on linux-x86_64. On
linux-aarch64 the interop workflow's second job runs the cells listed in
the `[arm64_subset]` table of `scripts/interop/expectations.toml`
(`--only-subset arm64`): SRT and RIST live with ffmpeg, TSDuck and
GStreamer, every profile over SRT, RTP over RTSP, the publisher role in
both its MPEG-TS and elementary shapes, and TSDuck's analyzers. The
browser, VLC, mpv and HLS cells are left out. That job's census is recorded
in the qualification record beside the x86_64 one, and the release verdict
reads the x86_64 census. A peer tool missing from the arm runner skips its
cells, and the job's step summary names them. `soak.sh` runs on
linux-x86_64. `soak.sh`'s SRT and RIST legs need no third-party
media tools, being `tst-interop` talking to itself through its own
impairment proxy; its `rtsp-publish` leg needs GStreamer's
`gst-launch-1.0` with `rtspclientsink`, and `--no-rtsp-publish` runs
without it.

## The transport + format interop matrix

`run-matrix.sh` exchanges synthetic MPEG-TS/KLV traffic with real
third-party tools over live network sessions (SRT, RIST, UDP, TCP, HLS,
RTSP, including GStreamer publishing into the RTSP server's publisher
role), plus runs local decode/analyze probes against the same synthetic
files, across every one of the 12 canonical stream profiles the crate
models (baseline H.264+KLV, H.265, H.266/VVC, AV1 in two PID-classification
shapes, MISP timestamps, synchronous AU-cell KLV, sparse/tight PCR, PTS
rollover, AAC audio, and a two-program stream). Each (transport-or-probe,
peer, direction, profile) combination is one "cell." The verdict describes
whether that combination passed, failed, matched a known limitation, or
could not run.

Since 2026-09-14 every cell carries **realistic access-unit sizes**: the
generator draws a GOP-structured stream — 28-52 KiB keyframes and 2-10 KiB
inter frames on 30-frame GOPs, roughly 1.7 Mb/s of elementary stream at
30 fps — so a single keyframe spans on the order of 160-290 transport
packets rather than the one or two the previous tens-of-bytes fixtures
produced. That is the regime in which a peer tool's PES reassembly,
`payload_unit_start_indicator` handling, continuity counters and buffer
model are actually exercised; a multi-packet access unit is the shape a
real encoder emits, and the census below is the first one measured under
it. The compact regime is still reachable with `--au-sizes compact`, and
it remains the regime the crate's own offline unit and round-trip tests
use, where a fixture's value is being small and byte-exact rather than
representative.

| Verdict | Meaning |
| --- | --- |
| **PASS** | The checks required for that cell's tier passed. The tiers and their checks are explained below. |
| **EXPECTED-UNSUPPORTED** | A `FAIL` that matches a row in `expectations.toml` — a known, already-investigated gap (see below). |
| **KNOWN-FLAKY** | A `FAIL` (or PASS) matching a row marked flaky rather than reliably-reproducing. |
| **SKIPPED** | The peer tool wasn't installed on the runner. Never a silent pass. |

**What a PASS establishes depends on the tier:**

- **`transparent`** (relay paths): the received bytes match the source byte for byte.
- **`remux`** (paths that may re-packetize): `tst-interop verify` checks the
  media content and timing. An independent parser also checks the raw TS
  bytes, so the result does not rely solely on the demuxer under test.
- **`n/a`** (decode-only probes): the peer's log contains no error. This is
  a narrower check than byte equality or the remux checks.

For `remux`, video, KLV, and audio event counts must reach at least 70% of
the nominal count (`NOMINAL_COUNT_SLACK`). This allows for captures that
start or end partway through a stream. **That floor alone is not enough to
pass.** For each media PID, `wire_vs_demux_<pid>` also compares the demuxer's
event count with PES starts counted independently in the raw bytes.

Any shortfall in that comparison must fit the permitted allowances. In
strict mode, explained loss comes from attributed injections on that PID.
In lossy mode, it instead comes from that PID's recorded discontinuities
and non-conformances, which already include attributed events.
Multiplex-wide resynchronizations and a fixed per-PID boundary allowance
also contribute. The boundary allowance covers units before the demuxer
acquires PAT/PMT and units left incomplete at the end of the capture.

The remaining checks cover the video codec, KLV carriage kind, program
count, and monotonic PTS with rollover handling. Depending on the profile,
the raw-byte parser also checks PCR cadence, AV1 carriage mode, media
counts per program, and an observed PTS wrap. See "What each profile's
oracle proves" below for the individual checks.

**Current census: 161 cells — 96 PASS, 0 FAIL, 65 EXPECTED-UNSUPPORTED, 0
SKIPPED — measured at realistic access-unit sizes**
([run 39687057760](https://github.com/aklofas/ts-transformer/actions/runs/39687057760),
`workflow_dispatch`, 2026-10-21 at `907adb30`, shape
`full-161`, empty allowed-skip list, no stale expectation). The count is `[census] full_cells` in `scripts/interop/expectations.toml`; it moved from 159 to 161 when the two `whep/*` browser cells (headless Chrome playing a WHEP mount with the reference player, see `scripts/interop/README.md`) were declared; the 159-cell figures below are the earlier runs. The two cells
added on 2026-10-08 both pass, as they did on their first CI run
([run 37858440566](https://github.com/aklofas/ts-transformer/actions/runs/37858440566), 2026-10-08 at `945911d4`, the 159-cell census of that date). Each has a GStreamer `rtspclientsink`
publishing into `tst-interop recv`'s RTSP publish mount:
`rtsp-publish/gst-push-mp2t` (MPEG-TS over RTP, byte-identical end to
end) and `rtsp-publish/gst-push-es-klv` (H.264 + KLV as two RTP tracks,
re-muxed by the server, judged with `recv --remuxed`: the server's own
PIDs, every content oracle, and a KLV set digest equal to the source's).
The other 157 cells kept their verdicts: no expectation row was added or
edited. There is no ffmpeg publisher cell. ffmpeg's RTSP muxer cannot
publish the harness's synthetic stream (`dimensions not set` from its
minimal SPS) and cannot publish KLV over RTSP at all; see
[`scripts/interop/README.md`](/scripts/interop/README.md) gap item 10.
"Publisher role (RTSP ANNOUNCE/RECORD ingest)" below collects these cells
with the role's fuzz and soak evidence.

**The 157-cell census before that addition (92 PASS, 0 FAIL, 65
EXPECTED-UNSUPPORTED, 0 SKIPPED) was identical at realistic and at compact
access-unit sizes.** Every one of the 92
passing cells still passes on roughly six times the payload per access
unit, and every one of the 65 documented gaps reproduced on the mechanism
its `expectations.toml` row already names: the 2026-09-14 re-validation
added no expectation row, edited none, and `report merge` reported no
stale row (a documented gap that has started passing). That the census
survived the size change is itself the evidence — the accepted
nonconformances are peer-tool properties, not artifacts of unusually
small access units, and this codebase's own PES framing and PSI signaling
hold when a keyframe has to be split across hundreds of transport packets
instead of fitting in one.

Provenance: the re-validation ran twice. On the dev box the full matrix
exits 0 at **80 PASS / 0 FAIL / 65 EXPECTED-UNSUPPORTED / 12 SKIPPED** —
that box has no `gst-play-1.0`, so its 12 `decode/gst-play` cells are
declared as allowed skips (`--allowed-skips 'decode/gst-play/*'`) rather
than silently missing. The CI runner installs that tool and reports the
full 92-PASS census:
[run 34830892357](https://github.com/aklofas/ts-transformer/actions/runs/34830892357)
(`workflow_dispatch`, 2026-09-14 at `dc912eea`, `--seconds 10`, `au_sizes: realistic`,
shape `full-157` with an empty allowed-skip list and an empty
`stale_expectations`) — a second host, a different seconds-per-cell
setting, and a different TSDuck point release (3.44-4676 against the dev
box's 3.43-4549) reaching the same verdict on every cell. Before the move to
realistic sizes, the 92-PASS census had reproduced identically on the dev
box and in CI since 2026-08-20, and its predecessor (80 / 0 / 65 / 12,
from when `gst-play-1.0` was deliberately withheld from the runner
pending a first evidenced local run) reproduced byte-identically across
four independent full local runs plus a public CI run — five runs, two
hosts, two `--seconds`-per-cell settings (8 locally, 10 in CI), two
TSDuck point releases (3.43-4549 locally, 3.44-4676 in CI) — and then on
every verified run through the 0.5.1 release gate. The gst-play
enablement rationale and its harvested filler-AU noise phrasings live in
`scripts/interop/lib.sh` and the workflow's peer-tools step.
`report merge`'s exit code is 0 iff every `FAIL` matched a documented
`expectations.toml` row; any new, undocumented `FAIL` still exits nonzero —
an unexpected failure is never silently absorbed into the census.

### Public, continuously-refreshed CI evidence

The same matrix runs on a stock GitHub Actions `ubuntu-latest` runner (no
local dev-box state, no vendored corpus) via
[`.github/workflows/interop.yml`](https://github.com/aklofas/ts-transformer/blob/main/.github/workflows/interop.yml):
weekly on a schedule (Mondays 05:00 UTC), on every `workflow_dispatch`, and
on any PR touching `crates/tst-interop/`, `scripts/interop/`,
`crates/tst-core/src/mpegts/`, the RTSP client and server
(`crates/tst-rtp/src/rtsp/`), or the workflow file itself. The current
census run cited above is
[run 39687057760](https://github.com/aklofas/ts-transformer/actions/runs/39687057760)
(the 2026-10-21 `workflow_dispatch` at `907adb30`), which completed
`success` with the census-completeness assert, the 161 / 96 / 0 / 65 / 0
census, and 161 per-cell result records with zero `FAIL` and no expectation
drift: all 65 documented-gap rows reproduced. The previous verified run, at
159 cells, was
[run 37875160134](https://github.com/aklofas/ts-transformer/actions/runs/37875160134)
(the 2026-10-09 `workflow_dispatch` at `41ed12e4`), with the 159 / 94 / 0 /
65 / 0 census and 159 per-cell result records with zero `FAIL` and no
expectation drift: all 65 documented-gap rows reproduced. The first run
with the RTSP publisher cells,
[run 37858440566](https://github.com/aklofas/ts-transformer/actions/runs/37858440566)
(2026-10-08 at `945911d4`), reached the 159-cell census. The first public
run at realistic access-unit sizes,
[run 34830892357](https://github.com/aklofas/ts-transformer/actions/runs/34830892357)
(2026-09-14 at `dc912eea`, the 157 / 92 / 0 / 65 / 0 census), the last
compact-size run,
[33359181955](https://github.com/aklofas/ts-transformer/actions/runs/33359181955)
(the 2026-08-31 weekly `schedule` run at `73ae1ced`, same census), the
gst-play-enablement run
[32400751057](https://github.com/aklofas/ts-transformer/actions/runs/32400751057)
(`pull_request`, 2026-08-20 at `eb2bc419` — the first 92-PASS census run), the
0.5.1 release-gate run
[32106462041](https://github.com/aklofas/ts-transformer/actions/runs/32106462041)
(2026-08-18, at `47ceee90`) — the last of the 80 / 0 / 65 / 12 predecessor-census
runs — the ancestor run
[32103732958](https://github.com/aklofas/ts-transformer/actions/runs/32103732958)
(2026-08-18, at `239e2d80`), and the 0.5.0-candidate run
[31335394509](https://github.com/aklofas/ts-transformer/actions/runs/31335394509)
(2026-08-09, at `049fd4e3`)
remain as historical evidence) — its `results.json`/`results.md`,
per-cell logs, and captures are attached as the `interop-evidence` artifact
(90-day retention) and the run's own step summary. Every future weekly run
re-publishes a fresh `interop-evidence` artifact and step summary on that
same workflow; check the [workflow's runs page](https://github.com/aklofas/ts-transformer/actions/workflows/interop.yml)
for the latest one.

Peer-tool versions drift over time on a rolling `ubuntu-latest` image
(FFmpeg 6.1.1-3ubuntu5, TSDuck 3.44-4676 pinned via a specific `.deb`
release, VLC 3.0.20, mpv 0.37.0, GStreamer 1.24.2 as recorded in the
cited run's `results.md` tool stamps) — a future
run surfacing a new `FAIL`, or flagging a documented gap as stale because it
started passing, is this system working as designed, not a bug in the
workflow. See the workflow file's own comments for the exact reasoning.

## Findings highlights

Every `FAIL` this matrix produces is investigated and recorded — in
[`scripts/interop/README.md`](/scripts/interop/README.md)'s "Known,
already-evidenced gaps" section and/or the corresponding
[`expectations.toml`](/scripts/interop/expectations.toml) reason field,
each with the mechanism and a peer-tool version stamp — not papered over
with a looser assertion. Five clusters account for 64 of the 65
`EXPECTED-UNSUPPORTED` + `KNOWN-FLAKY` cells:

- **FFmpeg strips the leading 5 bytes of every KLV record on `-c copy`
  remux (the largest cluster).** Verified byte-for-byte with `tsp -P pes
  --save-es` on a pure file-to-file remux: the async/`PrivateData` KLV
  carriage's 16-byte SMPTE UL key loses its first 5 bytes, corrupting the
  key this codebase's demuxer requires to recognize a record at all. The
  PMT itself (stream_type + `KLVA` registration descriptor) survives the
  remux intact — this is an FFmpeg mpegts-demux payload bug with the `klv`
  codec_id, not a PMT/classification loss. The synchronous, AU-cell-wrapped
  KLV carriage shows a related but distinct symptom: FFmpeg strips the same
  5 bytes (this time the genuine `Metadata_AU_cell` header) *and*
  downgrades the PMT's stream_type from `0x15` to `0x06` on write-back. Full
  byte-level evidence in README.md item 1.
- **SRT (only) loses a small trailing fraction of a paced live send, with
  the disproven hypotheses recorded.** A `tsp -P regulate`
  or `gst` paced sender lands well inside this project's nominal-count
  tolerance but the received `stream_sha256` doesn't exactly match — the
  same pacing over RIST and UDP is byte-perfect in the same run, so this is
  SRT-specific. Giving the receiver 2.5x the sender's real content
  duration reproduced the identical byte shortfall, which rules out the
  receive-side deadline — the
  connection closes from the sender's side the moment it decides it's done,
  regardless of how much slack the receiver is given. The residual
  mechanism narrows to the peer tool's own SRT output-plugin closing
  behavior, not confirmed against its source, and not something a
  `tst-interop`-side change can fix. Full investigation trail in
  README.md item 2.
- **FFmpeg's librist/UDP input path hangs against a live listener with
  nothing ever received**, while the identical send calls against a TSDuck
  listener on the same two transports pass byte-perfect in the same run —
  a peer-side issue, not a send-side one (README.md item 3).
- **Per-codec format-axis gaps beyond the KLV-truncation cluster**: FFmpeg
  refuses to open its output for this codebase's AV1 carriage or its
  two-program stream when its default single-stream auto-selection silently
  drops a PID it can't classify; FFmpeg's VVC parser rejects this codebase's
  H.266 fixture's `vps_video_parameter_set_id = 0` (a value H.266, unlike
  HEVC, reserves to mean "no VPS referenced" — not independently checked
  against the cited specification); and FFmpeg can't determine the
  AAC-ADTS audio stream's sample rate quickly enough over a live,
  non-seekable SRT source to open its output at all. Full detail in
  README.md items 6-9.
- **mpv has no working VVC decoder on the box this matrix ran on**, a
  distinct finding from FFmpeg's own VPS rejection above — mpv gets far
  enough to identify the H.266/VVC track but then reports "Failed to
  initialize a decoder for codec 'vvc'." mpv also can't classify this
  codebase's AV1 carriage as a video track at all (the same PID
  classification issue that affects FFmpeg's AV1 cells). VLC, separately,
  can't decode this project's synthetic H.264/H.265 fixture's inter-frame
  filler content ("buffer deadlock prevented") — a fixture limitation, not
  a wire-protocol one, corroborated by the identical message appearing on a
  bare local file with no transport involved at all.

The 65th cell, `rtsp-consume/vlc-serve-ffmpeg-pull`, is the one `KNOWN-FLAKY`
entry rather than an `EXPECTED-UNSUPPORTED` one: VLC's own `--sout` RTSP
serving intermittently returns a 5XX Server Error to ffmpeg's DESCRIBE. It
has no `tst-interop` transport leg at all (VLC serves, ffmpeg pulls
peer-to-peer), so it doesn't exercise this codebase's own RTSP code either
way — wired `known_flaky` from day one, not a finding about this project.

None of these are library bugs on this codebase's own PMT signaling, PES
framing, or wire conformance — each is independently confirmed by
byte-level or log-level evidence, recorded in README.md's gaps list for
the transport-axis and multi-mechanism findings, or in the relevant
`expectations.toml` row's `reason` field for the single-mechanism
format-axis decode findings (the 12 `decode/mpv/*` and `decode/vlc/*`
rows). Read either source for the exact reproduction command, tool
version, and root-cause argument behind each one.

### What each profile's oracle proves

Since 2026-09-13 `tst-interop verify`/`recv` check every profile
against demuxer-independent wire facts read by a deliberately naive
raw-TS parser (`crates/tst-test-helpers/src/rawts.rs`) in addition to the
demuxed tallies, and every oracle has a mutation test that removes the
property and asserts the named failure (`crates/tst-interop/tests/mutations.rs`).

| Profile | Independently verified on the wire | Oracle names |
| --- | --- | --- |
| all | PMT stream_type per PID, KLVA/AV01 registration, PCR median interval ≥ configured; in Strict cells (offline `verify`, `recv --strict`) additionally every interval ≤ configured + one frame period (the muxer's PCR-only catch-up packets are a legitimate minority) — Lossy live cells judge the median only; demuxer event count per media PID ≥ the raw reader's PES-start count (minus the events the capture explains on that PID plus multiplex-wide resyncs, and the per-PID boundary allowance); no unexpected 33-bit PTS wrap, `NonConformant` fatal, `Discontinuity` fatal offline / counted live | `pmt_stream_type_*`, `pmt_descriptor_*`, `pcr_interval`, `wire_vs_demux_*`, `pts_wrap_unexpected`, `nonconformant_event`, `discontinuity_event` |
| two-program | video + KLV counts per program_number, packets on both programs' media PIDs | `program_{1,2}_{video,klv}_floor`, `program_{1,2}_wire_media` |
| audio | ADTS syncword + 48 kHz index in the raw PES payload, `sample_rate/1024` frames/s ±10 %, 1920-tick PTS step ±5 % | `audio_codec_adts`, `audio_cadence`, `audio_pts_step` |
| av1-klv-a / av1-klv-b | PES stream_id 0xE0 + raw OBU header vs 0xBD + `00 00 01` `ts_open_bitstream_unit` framing (the PMT is identical in both modes) | `av1_carriage_wire` |
| pcr-tight / pcr-sparse | PCR median interval ≥ 1 ms / ≥ 100 ms; every interval ≤ configured + one frame period in Strict cells (the muxer's PCR-only catch-up packets are a legitimate minority) | `pcr_interval` |
| pts-rollover | at least one raw PES PTS wrap observed | `pts_wrap_observed` |
| klv-sync | stream_type 0x15 + metadata (0x26) and metadata_STD (0x27) descriptors | `pmt_stream_type_*`, `pmt_descriptor_*` |

## Soak evidence

`soak.sh` runs two concurrent, hours-long transport legs of `tst-interop` pushing
synthetic MPEG-TS/KLV traffic through an impaired proxy: an SRT leg
wrapped in `tst_pipeline::ManagedTransport` (so it must reconnect across a
90-second full-drop outage window injected every 6 hours) and a RIST leg
under the same impairment with no outage (RIST has no managed
reconnect wrapper in this codebase, so its job is purely "does sustained
loss/jitter/reorder behave the same over hours as it does over a
five-second matrix cell"). Both legs' senders run `--au-sizes realistic`
— the same GOP-structured regime the interop matrix now runs (28-52 KiB
keyframes, 2-10 KiB inter frames, ~1.7 Mb/s at 30 fps) — so the soak
measures endurance under a real encoder's traffic shape and burst
pattern, and the two evidence bodies on this page are measured on the
same stream. `tst-interop report soak` renders a pass/fail verdict plus
RSS-growth slopes per process. Since 2026-10-08 a third leg, `rtsp-publish`,
runs beside them: an external RTSP publisher pushing into the RTSP server's
publish mount, ending and re-ANNOUNCEing on a schedule. It is described
under "Publisher role" below, with the rest of that role's evidence. A
fourth leg, `rtsp-publish-es`, publishes elementary streams into a second
mount; it is described after the runs table below. Since 2026-10-21 a fifth
leg, `whep`, serves a WebRTC mount to a headless browser that reconnects on
a schedule; it is described under "WebRTC playback" below.

The impairment itself changed shape on 2026-09-14 (see "The current soak
shape" below), and the 0.7.0 release soak below runs it. The earlier
runs on this page, the 2026-08-05 72-hour run among them, used the
previous **fixed-impairment** shape. That shape held one level for the
whole run: 2 % loss, 20 ms jitter over a 30 ms base link delay, and 1 %
reorder held 200 ms, seeded deterministically, with both legs on the
`baseline` profile and no corruption injected. Those numbers are what
that run's tables below mean.

`tst-interop` also carries a sender-side corruption tap (`send --corrupt`)
that deliberately damages the muxer's own output on its way to the wire —
flipped bytes, destroyed headers, truncated and duplicated and dropped
packets, damaged PAT/PMT sections — and records every injection to a JSONL
log the receiving side reads back. A receiver judged against that log
answers three questions that a clean run cannot ask: did every error event
it reported have a cause (`corruption_attributed`), did every injection a
conformant receiver is required to notice produce a signal
(`corruption_detected`) — reported in two halves, injections the receiver's
own demuxer reported (`detected_by_demux`) and injections only the harness's
raw-TS reader caught as a sync loss (`detected_by_reader_only`: garbage runs
and destroyed sync bytes, which tst-core re-syncs past without an event by
design), so the receiver is credited only with what it reported itself —
and did the stream produce media again afterwards
(`corruption_recovered`). The tap is **on by default on both soak legs**,
and the 0.7.0 release soak below carried these verdicts end to end over
72 hours. None of the
161 interop-matrix cells inject corruption — the census above is a pristine
stream throughout.

**One-hour smoke run (2026-08-03, seed 1, `recv --managed` now on the SRT
leg) — both legs PASS, zero crashes.** (This smoke predates the
realistic-AU-size and base-delay knobs — it ran compact fixtures over a
0ms-base-delay proxy — so its byte volumes are not directly comparable
to the 72-hour run's; its drop-rate and RSS conclusions are
regime-independent.)

| Leg | Sent (video AUs) | Received | Drop rate (observed vs. expected) | Verdict |
| --- | --- | --- | --- | --- |
| SRT (managed send + managed recv) | 108,000 | 107,999 | 2.027% vs. 2.00% expected | PASS |
| RIST (no outage) | 108,000 | 107,987 | 2.029% vs. 2.00% expected | PASS |

Both legs' observed drop rate sits well inside a 6σ binomial tolerance band
around the proxy's configured 2% loss rate (±0.159 percentage points for
the SRT leg's packet volume, ±0.177pp for RIST's — the actual deviation is
roughly one sigma in both cases (~1.01σ for SRT, ~0.99σ for RIST),
comfortably inside the 6σ tolerance band — a 5.9-6.1x margin), and `recv`
reported no verification failures on either leg. This run is also the
first to exercise `recv --managed`'s receive-side reconnect wrapper (the
SRT leg's `recv`, not just its `send`, now survives a transport break via
`ManagedRecvTransport`/`ManagedDemuxReceiver`) — as expected for a run
this short, the 6-hourly outage schedule never actually fires against the
send/recv pair (only the proxy's own pre-warmup window elapses, which the
runner deliberately shields them from), so the recv-side reconnect counter
correctly reads 0; this is recorded, not gating (see `report/soak.rs`'s own
module doc for why that check stays provisional even once a real count is
available).

RSS sampling with `--no-klv-digest` active on every process shows the SRT leg's `send`/`recv` both flat at 0.0
KiB/hour, matching the proxy baseline — the digest-accumulation fix
accounts for its entire prior growth. The RIST leg's `send`/`recv` still
show measurable growth (1485.1 / 349.3 KiB/hour respectively) — an order
of magnitude down from the pre-fix ~5.8 / ~4.0 MiB/hour, confirming digest
accumulation was the dominant cause there too, but not zero: an
unexplained RIST-specific residual remained an open watch item at the
time (plausibly librist's own recovery-buffer growth) — since resolved by the 72-hour run below,
which measured that same sender at 0.1 KiB/hour over its final 24 hours:
the residual was warm-up convergence toward a steady-state plateau, not
growth.

The soak rail was tightened on 2026-09-13: `report soak` now
judges a run against a configured duration and RSS cadence
(`soak-config.json`), requires ≥90 % of the cadence-implied post-warmup
samples per process with the gap between consecutive samples strictly
under three cadences, and fails on any
nonzero worker exit (`exits.json`). The 2026-08-05 run below predates
that rail; the 0.7.0 release soak is judged under it. A 1-hour
smoke on 2026-09-13 (seed 1) passed every new verdict: `duration_coverage`
PASS (3542 s observed against a 3600 s expected duration), all six
`rss_sample_coverage_<leg>_<process>` verdicts PASS at 98/98 samples each
(largest gap 31 s), and `worker_exits` PASS with all six roles exiting 0.
Two kill drills on the same day confirmed the failure paths: a sender
killed inside the end-grace window leaves no report artifact, so `report
soak` refuses to write `soak-results.json` and the run exits 2; a sender
killed earlier trips the supervisor's fail-fast (`soak-FAILED`) with every
worker's status recorded in `exits.json`.

### Runs

Every soak run this page cites, oldest first. The tree is the commit the
run was built from. A release soak declares 259 200 seconds (72 hours);
the release qualification accepts no shorter run.

| Id | Dates (UTC) | Tree | Declared duration | Verdict | Note |
| --- | --- | --- | --- | --- | --- |
| `soak-run2-2026-08-05` | 2026-08-05 → 2026-08-08 | not recorded | none (it predates `soak-config.json`); ran 72 h | PASS | Fixed-impairment shape, Blocking mode; no source commit was recorded. Described under "The previous run" below. |
| `soak-rc070-2026-10-04` | 2026-10-01 → 2026-10-04 | `e86dd9ea6d1a` | 259 200 s | `overall_pass=false` | The 0.7.0 release soak. The SRT leg passed every gating verdict; the RIST leg failed corruption attribution on harness defects. |
| `soak-rtsp-publish-2026-10-08` | 2026-10-09 | `b6545e6004ae` | 3 600 s | `overall_pass=false` | The first one-hour run with the `rtsp-publish` leg, which passed every verdict it owns; the three failures were harness defects on the SRT leg's receiver. |
| `soak-whep-2026-10-21` | 2026-10-21 | `a5c0e9d7a6af` | 3 600 s | `overall_pass=true` | The first one-hour run with the `whep` leg (WebRTC playback to a reconnecting headless browser); every verdict on every leg passed. Described under "WebRTC playback" below. |
| `soak-0.9.0-2026-10-27` | 2026-10-24 → 2026-10-27 | `9b8acb3b6fae` | 259 200 s | `overall_pass=false`, waived | The 0.9.0 release soak, with all five legs (`srt`, `rist`, `rtsp-publish`, `rtsp-publish-es` and `whep`). 89 of 93 verdicts passed and the SRT receiver reconnected across all 12 outage windows. The harness reported `overall_pass=false` on four verdicts: `recv_invariants_rist` from the `rist` leg's AV1 carriage check (a harness oracle gap: it does not excuse corruption the impairment schedule attributes), `worker_exits` from one external-publisher teardown race in 432 generations on the `rtsp-publish` leg, and `recv_invariants_rtsp-publish-es` with `klv_placement_rtsp-publish-es` from a 2.3 ms KLV placement excursion over one sender-report interval on the ES leg. None is a library defect: harness FAIL on four verdicts, waived by the maintainer (harness defects, fixed in 0.9.1); the release was tagged on that waiver, which the release gate no longer admits. Run on the release VM after the corpus run on the same tree. |

The release qualification reads the same runs from
`tests/coverage/evidence-archives.toml`, except `soak-run2-2026-08-05`, which
predates the archive list and has no row there.

### The elementary publish leg (`rtsp-publish-es`)

GStreamer's `rtspclientsink` publishes H.264 and KLV as two separate RTP
tracks into a publish mount, and the mount re-muxes them into MPEG-TS for
`tst-interop recv`. The leg runs the `klv-sync` profile with compact KLV,
because each publisher generation replays the same one-generation source
and the rich KLV oracle's positional schedule cannot survive a replay. The
stream is re-muxed, so its digest is not judged. The weekly
`rtsp-publish/gst-push-es-klv` interop cell holds the received KLV set's
digest to its source's, so an altered record fails there. Two
verdicts judge the leg. `delivery_complete_rtsp-publish-es` requires the
received KLV record count to equal the declared count per generation times
the declared generations, with no KLV unit dropped by the mount and at
least one access unit emitted. `klv_placement_rtsp-publish-es` requires
every record to sit within 91 ticks of the video access unit it was
generated with, no unexpected timeline reset, and one generation seen per
declared publisher generation. The placement oracle admits one
sender-report-sized excursion per generation and reports it: a run of
consecutive records off by more than 91 ticks, spanning at most one RTCP
sender-report interval (7.5 s, the randomised maximum of the nominal 5 s)
with no record off by more than 10 ms, is the mapping jitter of one
report. It passes as a `klv_placement_excursion_rtsp-publish-es` row
marked excused, with its record count, span, largest error and
generation. A second excursion in a generation, a longer one, or any
record off by more than 10 ms fails the verdict. Each new publisher
restarts the timeline, so the receiver excuses one backward timestamp step
per PID at each declared generation boundary, and the timing anomalies that
coincide with it; any other backward step fails. The verdict detail gives the excused
counts. No archived run has carried this leg yet.

### The current soak shape (2026-09-14)

This subsection describes how the 0.7.0 release soak below was
configured and judged. The 2026-08-05 run, kept further down as historical
evidence, used the fixed-impairment shape.

A soak run no longer holds one impairment level against one stream shape for
its whole duration. Every choice below is derived from the single `--seed`,
so a run still reproduces from its seed alone, and every derived choice is
also DECLARED in `soak-config.json` before any worker launches so the report
can check the run did what it said it would.

- **A seeded phase schedule.** Both proxies walk twelve phases by default,
  each in force for an equal slice of the run, varying loss (0.5–4 %, with
  roughly 30 % of phases dropping in bursts of 3–8 consecutive packets),
  jitter (5–40 ms), reorder (0–2 %, held 100–300 ms) and base link delay
  (10–60 ms). The phase table is a pure function of the seed and the phase
  count, echoed into each proxy's stats file with per-phase
  forwarded/dropped counters. A multi-day run now exercises a link whose
  conditions CHANGE rather than one synthetic average.
- **Distinct per-leg stream profiles.** The two legs draw two different
  profiles from the seed instead of both running `baseline` forever, so a
  long run also covers a codec/carriage/cadence shape the 161-cell matrix
  only sees for five seconds at a time. Profiles drawn and soak-exercised
  so far: `klv-sync` and `audio` at seed 3, and `baseline` and `pcr-sparse`
  at seed 7, all in smokes; then `pts-rollover` (SRT) and `klv-sync` (RIST)
  at seed 11, the 0.7.0 release soak.
- **Sender-side corruption on both legs**, each with its own seed offset and
  its own injection log, read back by that leg's own receiver.
- **Rich ST 0601 KLV on both legs** — a record of up to 36 tags (mean about
  27) carrying a nested ST 0102 security set on a seeded presence schedule,
  rather than the matrix's 4-tag minimal record.

The SRT leg's managed sender runs in one of two reconnect modes, and the
run records which: declared in the config before launch, reported by the
sender afterwards, and compared by `reconnect_mode_declared_<leg>`. The two
modes support different claims. A Blocking leg stalls its producer for the
outage and replays the backlog, so it exercises the replay and a source that
waits. That is the mode of the 2026-08-05 run below, and the replay claim
belongs to that run alone. A Background leg, the mode of the 0.7.0 release
soak, keeps producing into a bounded buffer that drops what it cannot hold: it shows that both ends reconnect and that the received
stream recovers, but it does not show delivery of every source frame, and it
does not exercise the Blocking replay. The sender's reconnect and gap-buffer
counters are recorded beside each leg's sent and received totals and are not
gated.

The verdict document gains three groups on top of the existing ones. The
first is five declaration checks — `profile_declared_<leg>`,
`schedule_declared_<leg>`, `corruption_declared_<leg>`,
`klv_declared_<leg>` and `reconnect_mode_declared_<leg>` — which fail a run
whose proxy, sender or receiver did
not actually run what the config declared, so a drift between the recipe and
the run cannot pass unnoticed. Each is worth its own check because each
failure is silent: a leg declared rich whose receiver was launched in
compact mode, for instance, produces no rich-KLV block at all, and every
rich-KLV verdict is then skipped rather than failed. The second is the
drop-rate verdict, now integrated over the echoed phase table rather than
compared against one flat rate, and failing loud on a stats file whose phase
counters disagree with its own schedule echo. The third is the corruption
family described above, together with `corruption_coverage_<leg>`: the three
finding verdicts all pass on an empty finding list, so a leg whose tap
injected nothing would pass every one of them while proving nothing, and the
coverage verdict is what requires the injections to have happened, the
receiver to have ingested the whole log, and at least nine in ten of them to
have been placed on the receiver's own timeline.

Two attribution rules keep those verdicts honest on a link that really does
lose packets. A live capture is judged in the lossy tier, where an
unexplained continuity gap is excused as transport loss rather than charged
to the corruption tap, and an injection the link demonstrably lost is not
charged as undetected: one with a foreign continuity gap in its window, or
one placed inside a reconnect gap that the receiver was never in a position
to notice. Inside such a gap an injection keeps its detection obligation
only when two things are shown: that it arrived (it resolved exactly from a
PCR received after the reconnect), and that the demuxer, which the reconnect
reset, had already produced media again before it — on the injection's own
PID for damage that is noticed as a continuity jump (a dropped packet, a
rewritten PID or continuity counter), on any PID for everything else.
Otherwise it is counted as lost in the reconnect gap, inside the same
excusal budget. A reconnect gap ends at the next reconnect, so an injection
is judged against the reconnect it arrived after and an earlier one cannot
excuse it. An injection's own gap never
excuses that injection. Resyncs are never excused. Non-conformances are
never excused either, with one bounded exception: a PCR anomaly that jumps
forward, on a PID a PMT declares as the program's PCR PID, beside a
continuity gap on that same PID, is that gap's timestamp signature, and each
gap excuses at most one. A backward PCR jump, a PCR anomaly on any other
PID, and a second anomaly against the same gap all still fail the leg. The
declared PCR PIDs and the continuity gaps are those of the current
connection: a reconnect resets the demuxer, so after one no PID is declared
until a program map is reported again, and a gap seen before it excuses
nothing after it. Between reconnects a program map replaces the declaration
of its own program and of any program that held its PMT PID. The demuxer
reports no event when a PAT drops a program, so the harness cannot see that
removal, and a PCR anomaly on such a program's former PCR PID would still be
judged as one on a declared PID. The AV1 carriage oracle and the ADTS audio oracle excuse a later PES whose
opening bytes the tap damaged (its start lies inside an attributed
corruption window), and the attribution report counts each
(`av1_prefix_excused`, `adts_prefix_excused`). The
rich-KLV oracles
skip a record an injection demonstrably damaged, counting it separately so
every failure string reports how many records went unexamined. Offline
verification remains in the strict tier and sees neither excusal.

**Smoke evidence (2026-09-14, local, seed 3).** One ten-minute configuration
over a four-phase schedule, run twice at identical settings, passed all 31
verdicts the harness emitted at the time with `overall_pass: true` on both
occasions, drawing `klv-sync` on the SRT leg and `audio` on the RIST leg. A
separate three-minute run with the tap off, one fixed impairment and the
`baseline` profile on both legs covered the disabled path. Attribution was
complete on both legs and in both ten-minute runs — 267 injections, all
resolved, against 211 events, all attributed, on SRT; 287 injections, all
resolved, against 249 attributed events on RIST — with zero undetected and
zero unrecovered injections throughout. One of the two additionally excused
2 RIST events as transport loss, which is the lossy-tier rule firing on live
timing rather than on anything seeded, so it is expected to vary run to run.
Of about 6,100 rich KLV records per leg, 236 (SRT) and 223 (RIST) were
skipped as injection-damaged in both runs, identically — which is the tap
behaving deterministically for a fixed seed — and the rest decoded clean
with no census mismatches and every expected nested security set valid.
Observed drop rates tracked the phase-integrated expectation on both legs
(2.55 % against 2.59 % on SRT, 2.35 % against 2.48 % on RIST). Ten minutes is
a smoke, not endurance evidence: it proves the wiring, the declarations and
the oracles work together end to end, and nothing about multi-day behaviour.

The first run with the schedule and the corruption tap enabled together
exposed a real sizing bug in the harness, worth recording because it is
exactly what this shape exists to find: libsrt's default 120 ms TSBPD budget
is smaller than the link the schedule emulates (up to 300 ms of reorder hold
on top of 60 ms of base delay), so packets arriving past their play time were
dropped as loss no injection could explain — 38 unexplained continuity jumps
against 43 `RCV-DROPPED` warnings in the receiver's own log. The SRT leg now
sets a 1200 ms latency sized from the schedule's documented worst case, after
which the same run logged zero such warnings. No verdict was relaxed to
accommodate it.

### The 0.7.0 release soak (2026-10-01 → 2026-10-04, seed 11, Background mode)

**The run went to completion, and the harness verdict is
`overall_pass=false`.** It covered 259,137 of the configured 259,200
seconds and produced 41 verdicts: 36 gating PASS, 3 gating FAIL and 2
provisional PASS. The SRT leg passed every gating verdict, including
twelve outage windows survived with 12 reconnects and 107,849 corruption
injections with zero unexplained, undetected or unrecovered events. The
RIST leg failed corruption attribution: all three gating FAILs
(`worker_exits`, `recv_invariants_rist`, `corruption_attributed_rist`)
are that one finding, explained below. No process grew past the RSS gate.
This is the latest 72-hour SRT / RIST endurance run; the two harness
defects behind the RIST failure were fixed in 0.8.0. Its tree, dates and
declared duration are in the runs table above.

The tree was clean at launch (`provenance.json`:
`dirty=false`, 10 submodules pinned), built `--release` with the vendored
libsrt / librist / mbedTLS on a dedicated 2-vCPU / 3.9 GB cloud VM, and run
as `SRT_RECONNECT_MODE=background bash scripts/interop/soak.sh --hours 72
--seed 11` from 2026-10-01T18:32Z to 2026-10-04T18:37Z. Seed 11 drew
`pts-rollover` for the SRT leg and `klv-sync` for the RIST leg. Both legs
ran the current shape above (twelve 6-hour impairment phases at 0.5–4 %
loss, the corruption tap at 5 per 10,000 packets over seven damage
classes, rich ST 0601 KLV), and the SRT proxy cut the link for 90 s every
6 hours. Every choice was declared in `soak-config.json` before launch and
every `*_declared_<leg>` verdict passed.

| Leg | Sent → received (video AUs) | Drop rate (observed vs. phase-integrated expected) | Reconnects | Corruption | Rich KLV records judged | Verdict |
| --- | --- | --- | --- | --- | --- | --- |
| SRT (managed send in Background mode + managed recv, 90 s outage every 6 h, `pts-rollover`) | 7,776,000 → 7,742,910 (−0.43 %, the twelve outages) | 1.87 % vs. 1.87 % (±0.10 pp) | 12 (of 12 scheduled windows) | 107,849 injected, 107,414 resolved (435 unresolved); 90,784 events, all attributed; 0 unexplained, 0 undetected (3 excused inside reconnect gaps), 0 unrecovered | 2,580,965, 0 decode errors | PASS |
| RIST (continuous scheduled impairment, no outage, `klv-sync`) | 7,776,000 → 7,775,101 (−0.012 %) | 1.88 % vs. 1.88 % (±0.10 pp) | n/a | 108,171 injected, 108,168 resolved (3 unresolved); 91,814 events, 91,804 attributed, **8 unexplained** (2 more excused as transport loss); 0 undetected, 0 unrecovered | 2,591,588, 0 decode errors | **FAIL** (attribution) |

The maintainer's read-only checkpoints during the run (ssh only, no
intervention):

| Checkpoint | Processes | SRT receiver reconnects (windows elapsed) | Sender RSS (SRT / RIST) | Verdict |
| --- | --- | --- | --- | --- |
| 24 h (2026-10-02T18:53Z) | 7 / 7 alive, no fail-fast marker | 4 (4) | 173 MB / 177 MB | PASS |
| 48 h (2026-10-03T19:40Z) | 7 / 7 alive, no fail-fast marker | 8 (8) | 173 MB / 181 MB | PASS |
| 57.5 h (2026-10-04T04:07Z) | 7 / 7 alive, no fail-fast marker | 9 (9) | 173 MB / 181 MB | PASS |

The 177 MB RIST reading was an ad-hoc `ps` sample; the gated 30-second
series is flat at 181.3–181.4 MB from about 4.5 hours to the end.

**The RIST attribution failure.** The RIST receiver reported 8 events the
harness could not tie to an injection, spread from 13.8 h to 71.6 h. Both
causes were harness defects, fixed in `tst-interop` for 0.8.0 (see the
CHANGELOG's 0.8.0 entry); none of the eight is explained by a library
defect.

- **Six** are PSI checksum errors on the PMT PID 5 packets after the tap
  truncated that PMT packet inside its section (the `klv-sync` PMT section
  runs to byte 59). The demuxer correctly reports the CRC failure; the
  harness's injection-to-signal table did not admit a PSI checksum as a
  consequence of a truncation. Reproduced offline by cutting a `klv-sync`
  PMT to 45, 52 or 58 bytes. The SRT leg's `pts-rollover` PMT section ends
  at byte 37, which no truncation reaches (83 PMT truncations, no event).
- **Two** (a PAT checksum error at about 58 h, a PID-less resync at about
  71.6 h) had their true causes 1–6 packets earlier: a `psi_flip` on the
  PAT and a `garbage` injection on the video PID. The harness's raw reader
  took a PCR anchor from any packet, so a mis-framed packet after a framing
  injection could hand it a random anchor, which stranded every pending
  injection more than 0.4 s behind it. Reproduced with the real `send` and
  `recv` binaries over a byte-transparent relay: with no extra packets 149
  of 149 injections resolved; with 12 inserted packets carrying a PCR 100 s
  ahead, 84 were stranded and 37 events charged unexplained. The soak's own
  anchors were not archived, so this attribution is a high-confidence
  inference, not an observation.

The `overall_pass=false` above is the harness's verdict on this run, and
this page does not re-label it as a pass. Everything else on the RIST leg
passed: drop rate, coverage (100.0 % ingested and resolved), detection (0
undetected), recovery (0 unrecovered) and the rich-KLV oracles.

**What Background mode means for these numbers.** During each outage the
SRT sender kept producing into a bounded gap buffer (256 messages,
`drop_oldest`) and sent what it still held on reconnect. The run shows that
both ends reconnect after every outage and the stream recovers; it does not
show that every source frame was delivered (33,090 fewer video AUs received
than sent, by design), and it says nothing about Blocking mode's replay.
Sender counters, recorded and not gated: 144 reconnect attempts, 12
successes, 201,132 messages (236.9 MB) evicted, `gap_len_at_exit` 0. The
eviction counters miss what the transport had accepted before it noticed
the break; the sent-minus-received difference is the fuller figure.

**How the run was judged.** `corruption_coverage_srt` passed at 100.0 %
ingested and 99.6 % resolved, `corruption_coverage_rist` at 100.0 % and
100.0 % (floors 99 % and 90 %); `duration_coverage` at 259,137 s; all six
`rss_sample_coverage_<leg>_<process>` verdicts at 8,557 of 8,578 samples
with a largest gap of 31 s; `zero_process_exits` passed and `worker_exits`
failed only on the RIST receiver's exit 1. The two provisional verdicts are
the reconnect counts (SRT 12 rebuilds for 12 windows; RIST not
applicable), provisional because the counter counts factory rebuilds, not
windows. Every RSS slope passed the 200 KiB/hour gate after its 30-minute
warm-up:

| Process | Slope (KiB/hour) |
| --- | --- |
| RIST sender (worst) | 22.8 |
| SRT sender | 9.2 |
| SRT proxy | 5.2 |
| SRT receiver | 5.0 |
| RIST receiver | 1.8 |
| RIST proxy | 0.4 |

The verdict document records five standing limitations: the managed
sender's reconnect counters are not gated; the drop-rate check is
aggregate, not per outage window; outage-window drops are excluded from
both sides of it; a worker killed before writing its report makes `report
soak` exit 2 instead of naming the role; the gap-buffer eviction counters
are not an outage's whole cost.

Recorded, not gated (this run is the first with `proc.csv` and
`host.csv`): end-of-run RSS SRT sender 172.1 MB (a plateau of about 173 MB
from 6 h), RIST sender 181.4 MB, receivers 11.4 and 10.0 MB, proxies 7.0
and 6.5 MB; whole-run CPU between 0.54 % and 2.14 % of one core per
process; one-minute host load median 0.06, maximum 0.73. Thread and
descriptor counts returned to their baselines after each of the 12 outage
windows, with no drift across the run.

**Which binary this is evidence for.** `e86dd9ea`, not the tree tagged
0.7.0. The library changes after it are in the CHANGELOG's 0.7.0 entry:
the `tcps://` close drain, KLV classification and malformed-PES handling,
the ST 0601 Tag 102 and SPS-crop bounds, RTSP session teardown,
zero-length UDP / RIST datagrams and binding error kinds. Evidence on
`effc7f9c`, the last library change before the 0.7.0 tag:

- the 157-cell interop census at 157 / 92 / 0 / 65 / 0
  ([run 37181827346](https://github.com/aklofas/ts-transformer/actions/runs/37181827346),
  `workflow_dispatch`, 2026-10-04);
- all four sanitizer jobs, ASan and TSan over the pure-Rust and the
  native-linking crates
  ([run 37181828490](https://github.com/aklofas/ts-transformer/actions/runs/37181828490),
  2026-10-04);
- a byte-identical demux comparison of `d2fa73dc` and `effc7f9c` over the
  maintainer's local corpus of 260 captures (41.5 GB; one is a
  reconstruction, whose 32 `Unknown` KLV samples are the same under both
  trees). No raw capture's KLV PID has an `Unknown` sample. The corpus
  reaches neither the 13 319-byte async-KLV fix (its largest KLV record is
  302 B) nor the malformed-PES-neighbour fix; the synthetic reproductions
  in the CHANGELOG entries prove those;
- a one-hour soak smoke at seed 11, Background mode
  (2026-10-04T06:53Z–07:56Z), `overall_pass=true` with 41 verdicts (8
  provisional: the 6 RSS slopes and the 2 reconnect counts). SRT received
  107,954 of 108,000 video AUs (1.82 % drop against 1.83 % expected; 1,508
  of 1,509 injections resolved); RIST 107,990 of 108,000 (1.88 % against
  1.88 %; 1,534 of 1,534 resolved); 0 unexplained events on either leg. No
  outage falls inside one hour, so it does not exercise reconnects.

None of those runs is endurance evidence. The raw archive
(`soak-results.json`, `provenance.json`, the 30-second RSS samples,
per-process and corruption logs; 32 files, md5-checked against the VM
before teardown) is retained offline by the maintainer.

### The previous run (2026-08-05 → 2026-08-08, seed 1; fixed impairment, Blocking mode)

Historical evidence, published with 0.5.0 (CHANGELOG `## [0.5.0]`; its
exact source commit was not recorded). It predates the current soak shape
(one fixed impairment level, both legs on `baseline`, no corruption tap)
and Background mode (its SRT leg ran the Blocking replay), so its figures
describe that configuration only. **Overall PASS:** the full 72 hours
(259,152 of 259,200 s sampled; 7,776,000 AUs sent per leg) on an AWS EC2
t3.medium (2 vCPU / 4 GiB), zero process exits, twelve reconnects for
twelve scheduled outage windows and none unscheduled, no memory growth.

| Leg | Sent (video AUs) | Received | KLV records (sent → received) | Drop rate (observed vs. expected) | Reconnects | Verdict |
| --- | --- | --- | --- | --- | --- | --- |
| SRT (managed send + managed recv, 90 s outage every 6 h) | 7,776,000 | 7,773,537 (99.968%) | 2,592,000 → 2,591,182 | 2.0152% vs. 2.00% | 12 (of 12 scheduled windows) | PASS |
| RIST (continuous impairment, no outage) | 7,776,000 | 7,775,971 (99.9996%) | 2,592,000 → 2,591,991 | 2.0009% vs. 2.00% | n/a | PASS |

The SRT leg's 2,463 undelivered AUs (0.032%) are outage-attributable:
roughly seven seconds of video lost per 90-second outage, the in-flight
data from the moment of each cut, with the continuous 2% packet loss fully
absorbed by retransmission in between.

The worst post-warm-up RSS slope was 68.8 KiB/hour (SRT sender), warm-up
convergence to a ~176 MiB plateau reached by about 7 hours; every
process's slope over the final 24 hours was at or below 18 KiB/hour. Those
figures set `soak.sh`'s 200 KiB/hour default gate for full-length runs.
The supervisor fail-fast was armed throughout and never fired. Reproduce
with `SRT_RECONNECT_MODE=blocking scripts/interop/soak.sh --seed 1`; the
artifacts are retained offline by the maintainer.

## Real-capture corpus

The demuxer and the transports are also run over a private corpus of real
captures: an offline sweep of every file against the previous release, and a
paced replay of a chosen subset over SRT, RIST, UDP, TCP and the RTSP
publisher path. [Real-capture testing](/docs/project/real-capture-testing.md)
describes the corpus classes, the invariants and how to run the tool on your
own captures. The release gate requires a passing corpus archive for the
soaked tree.

## Publisher role (RTSP ANNOUNCE/RECORD ingest)

`RtspServer`'s publish mounts accept a publisher's ANNOUNCE, SETUP
`mode=record` and RECORD, and hand the received stream to the application
as an `RtpRecvTransport` (see the
[publisher-ingest cookbook recipe](/docs/cookbook/receiving/rtsp-publish-ingest.md)).
Three instruments cover the role: two interop matrix cells with a real
third-party publisher, three fuzz targets on the server's ingest parsers,
and a soak leg that repeatedly ends a publisher and re-ANNOUNCEs into the
same mount. Each is described below in the order it ran, with what it
found.

### Interop cells

Both cells run GStreamer 1.24.2's `rtspclientsink` against `tst-interop
recv --url rtsp-publish://127.0.0.1:<port>/mount`, a harness-only scheme
that binds an `RtspServer` with one publish mount and judges what the mount
delivers. They are part of the 161-cell census above
([run 39687057760](https://github.com/aklofas/ts-transformer/actions/runs/39687057760):
96 PASS, 0 FAIL, 65 EXPECTED-UNSUPPORTED, 0 SKIPPED), and both passed on
their first CI run ([run 37858440566](https://github.com/aklofas/ts-transformer/actions/runs/37858440566)) as well.

- **`rtsp-publish/gst-push-mp2t`** proves byte identity. The publisher
  sends MPEG-TS over RTP (RFC 2250, payload type 33). The cell's tier is
  `transparent`: `recv --strict` must pass, and the received stream's
  SHA-256 must equal the source file's.
- **`rtsp-publish/gst-push-es-klv`** proves the elementary path. The
  publisher sends H.264 and KLV as two RTP tracks, and the server re-muxes
  them into its own transport stream. The cell's tier is `remux`, judged
  against a `klv-sync` source with `recv --remuxed`. Locally it received
  240 of 240 video access units and 80 of 80 KLV records, with 0
  non-conformant events. Its KLV set digest equals the source's, so every
  KLV record survived the trip through RTP and the re-mux unchanged. The
  cell gates that: it computes the source file's digest with `verify` and
  fails on any difference. In the final-tree CI run the cell logged equal
  source and received digests.
  On three CI runs the cell lost exactly the KLV record at PTS 0; the cause (the sender-report offset rounding that unit a tick before the video origin) is fixed in this release and the cell record now carries the mount's own counters.
  The cell holds KLV **timing** as well as content: it runs `recv
  --klv-placement 1`, which holds every KLV record to the video access
  unit it was generated with, within 91 ticks (the aligner's 1 ms step
  tolerance plus one tick of sender-report offset rounding). The soak's
  `rtsp-publish` leg is MP2T passthrough and never reaches the re-muxer;
  its `rtsp-publish-es` leg publishes the elementary tracks and runs the
  same oracle across publisher generations.

Sanitizers (ASan + TSan, nightly jobs) ran green on the final tree `43834194` in [run 37883341432](https://github.com/aklofas/ts-transformer/actions/runs/37883341432) (dispatched 2026-10-09).

**What `--remuxed` skips, and why it is declared.** The server picks its
own PIDs (PMT 0x1000, video 0x100, KLV 0x101 as `PrivateData`), so the
oracles keyed on the generator's layout fail by construction. `--remuxed`
skips only those: per-PID wire media, PMT stream types and descriptors,
per-PID wire-versus-demux counts, the PTS-wrap check, audio and AV1
carriage, and the KLV carriage kind. The recv report lists them under
`skipped_oracles`, and the cell log reads `expect klv-sync, --remuxed`.
The cell record carries the list and the judged profile, and the results
table prints both on the row. Every content oracle still runs:
access-unit, keyframe and KLV counts, the codec, programs and PMTs seen,
PTS monotonicity, PCR cadence and non-conformant events, and the cell
holds the KLV set digest to the source's. A unit test shows that a re-muxed
capture missing half its content still fails the video floor under
`--remuxed`.

**Why the elementary cell uses a `klv-sync` source.** KLV over RTP needs a
timestamp per unit. The baseline profile's asynchronous KLV PES carries no
PTS, so GStreamer's `tsdemux` hands its KLV payloader untimed buffers, and
every KLV RTP packet goes out with one timestamp (observed: all 80 packets
of an 8 s clip). The server cannot place such units on the video timeline.
A `klv-sync` source carries a PTS per record, and its KLV RTP timestamps
advance by 9000 per record. The cell is still recorded under `baseline`,
the axis it belongs to.

**There is no ffmpeg publisher cell.** ffmpeg 6.1.1's RTSP muxer stops
with `dimensions not set` because it cannot read a picture size from the
generator's minimal H.264 SPS. A real 1280x720 SPS lets ffmpeg publish,
but it also makes 25 H.264 `decode/*` cells fail, because the decoders
then start decoding the synthetic slice payloads. ffmpeg's RTSP muxer also
has no KLV payloader. ffmpeg publisher evidence is therefore manual: see
[`scripts/interop/README.md`](/scripts/interop/README.md) gap item 10.

**A finding the elementary cell produced, fixed.** Before the
`klv-sync` source was chosen, two of five runs of the elementary cell
against the untimed baseline KLV showed a PCR anomaly of roughly 14 000
and 17 500 seconds. When a publisher ended, the server released every KLV
unit still waiting for the video at its placed PTS, however far ahead of
the video that was. Every PLAY reader and the application transport saw
the jump. The server now releases only units within 10 s of the last
muxed video PTS when a publisher ends. It drops the rest and counts them
in `PublishMountStats::klv_units_dropped`. Unit tests pin both sides of
the bound and the case where no video was muxed yet.

### Fuzz targets

Three `cargo-fuzz` targets cover the publisher's ingest parsers.
`tests/coverage/fuzz-targets.toml` lists all 40 targets in the workspace.

| Target | Drives |
| --- | --- |
| `rtp_klv_depacketize` | the RFC 6597 KLV depacketizer |
| `rtsp_server_publish_framing` | the server session's `$`-frame drain, then its RTSP request framing, as the session read loop runs them |
| `sdp_announce_classify` | SDP parsing, then the classification of an ANNOUNCE into a supported shape or a refusal |

A 20-second run of `rtsp_server_publish_framing` finished clean:

```text
Done 2348854 runs in 21 second(s)
```

**`sdp_announce_classify` found a panic, fixed.** After about 687 000
executions it crashed in the third-party `sdp-types` 0.1.8 parser, an
internal assertion failure on an 8-byte body:

```text
v=0\n\xff\n\0=
```

Any peer allowed to ANNOUNCE could send that body. Before the fix, the
server session thread panicked and the connection hung open with no
answer. A server's DESCRIBE answer could likewise unwind through an
`RtspClient` caller's thread. The fix has two layers:

- `sdp-types` moves to 0.2.0, whose parser returns an error for that
  input.
- `Sdp::parse` also catches a panic from the parser and returns
  `RtspError::BadSdp`, because the parser runs on unauthenticated peer
  bytes. The server answers the ANNOUNCE with 400 and keeps the
  connection.

Three regression tests use the 8-byte body: the parser alone, a server
ANNOUNCE followed by a clean publish on the same connection, and a client
DESCRIBE. After the fix, a 120-second run was clean, and the saved crash
inputs replay clean:

```text
Done 5741244 runs in 121 second(s)
```

### Soak leg (`rtsp-publish`)

`soak.sh` runs a third leg beside SRT and RIST. GStreamer's
`rtspclientsink` publishes MPEG-TS into a `tst-interop recv` publish
mount, and a new publisher session takes over at a fixed period. The leg
is designed as follows:

- **One stream, many publishers.** The script generates one stream for
  the whole run and cuts it at PAT packets into one segment per publisher
  generation. Each segment is pushed as its own publisher session.
  Concatenated, the segments are the source byte for byte, so `recv
  --strict` judges one unbroken stream across every change of publisher.
  The `delivery_complete_rtsp-publish` verdict requires the received
  stream's SHA-256 to equal the source's, declared in `soak-config.json`
  before any worker launches. A lost or truncated last generation leaves no
  discontinuity behind it and can still clear the count floors; the digest
  catches it.
- **Graceful drops.** Each generation ends at its segment's end of
  stream, and the session then closes. The next publisher ANNOUNCEs into
  the mount, which stays open between publishers. An abrupt publisher
  loss is not exercised on this leg. The server's integration tests cover
  a publisher that drops its connection with no TEARDOWN.
- **Generations from the mount, not from the script.** The recv report
  carries the mount's own `PublishMountStats`, read while the server still
  runs. The `publisher_generations_rtsp-publish` verdict requires
  `generation >= publisher_generations`, where `publisher_generations`
  is the declared number of segments. The stream stops `30 + N` seconds
  before the run, so the last publisher ends before recv's deadline; one
  cut off there would also fail `worker_exits`. A publisher's post-EOS
  send failure is a clean end: `gst-launch-1.0` exiting nonzero after it
  logged `Got EOS` failed to send its closing message on a stream whose
  media was delivered, and the publisher log names it as such. A nonzero
  exit before EOS, or an overrun the timeout kills, fails the generation.
- **Declared like the other legs.** `soak-config.json` declares the leg's
  profile, KLV mode, publisher command, segment length, segment count and
  source digest before any worker launches. A declared leg whose report is
  not supplied to `report soak` fails `leg_evidence_<leg>`. The leg has no impairment proxy and no
  corruption tap, so those verdicts read "not applicable" with the reason.
  Its RSS is sampled for `recv` and for the publisher loop.

The first one-hour run, on 2026-10-09 from 01:13:50Z to 02:14Z UTC, used the
default 600-second period. It cut a 3564-second stream into six segments
and ran the `baseline` profile with rich KLV. The SRT and RIST legs drew
`audio` and `av1-klv-b` from seed 1. The run was built from the tree
`v0.7.0-135-gb6545e60`, before the `delivery_complete` verdict and the
declared source digest existed.

**The run's harness verdict is `overall_pass=false`, and the publisher leg
passed every verdict it owns.** The figures below are from the run's own
`soak-results.json`, written at the end of the run by the `report soak` of
the tree it was built from. That report has no `delivery_complete` or
`leg_evidence` verdict, and it judged the generations against a floor of 5
(one less than the declared count). Of its 57 verdicts, 44 are gating
PASS, 3 are gating FAIL and 10 are provisional PASS. All three FAILs are on the SRT
leg's receiver and are harness defects:

- `recv_invariants_srt` and `worker_exits` share one cause. The receiver's
  final verify failed `audio_codec_adts` on one audio PES whose first ADTS
  byte the corruption tap had flipped. The audio prefix oracle has no path
  to excuse injected damage.
- `zero_process_exits` comes from one empty RSS reading for the SRT
  receiver at 1350 s. Its CPU counters kept advancing on that tick, and it
  ran to the end of the run.

The maintainer's analysis of both covers the injection, the sampler rows
and the fix shape for each. The RIST leg passed every gating verdict.

The `rtsp-publish` leg's figures:

| Measure | Result |
| --- | --- |
| Publisher generations | 6 ended on the mount, 6 declared, all six publishers exited 0 |
| Stream identity | received SHA-256 equals the source's (`7b3647a4…c136`) |
| Received content | 106 920 video access units (3564 s × 30 fps), 3564 keyframes, 35 640 rich KLV records |
| Errors | 0 discontinuities, 0 non-conformant events, 0 malformed RTP packets |
| RSS slope, `recv` | 92.4 KiB/h over 98 samples (provisional below 72 h) |
| RSS slope, publisher loop | 0.0 KiB/h over 98 samples (provisional below 72 h) |

A second report, `soak-results-rejudged.json`, re-judges the same
artefacts with the current `report soak`. It gives 58 verdicts with the
same three FAILs. Its `delivery_complete_rtsp-publish` passes, but the
source digest it compares against comes from the run's `source.sha256`
artefact, not from a declaration. This run's `soak-config.json` predates
the declared digest. `publisher_generations_rtsp-publish` passes at 6 of
6, under the current floor of every declared generation. No
`leg_evidence` verdict appears, because every declared leg's report was
supplied.

The run's full artifact set is retained offline by the maintainer.

## WebRTC playback

`tst-webrtc`'s WHEP server serves mounts fed with MPEG-TS or elementary
streams to browsers over WebRTC, with every KLV record carried both in the
H.264 SEI of the frame it binds to and on a data channel (see the
[WebRTC guide](/docs/guides/webrtc.md) and the
[browser playback recipe](/docs/cookbook/sending/whep-browser-playback.md)).
Three instruments cover it: two interop matrix cells that play a mount in
a real headless browser with the reference player, three fuzz targets on
the server's peer-facing parsers, and a soak leg in which the browser
reconnects on a schedule for the whole run. Each is described below, with
what it found.

### Interop cells

Both cells run Google Chrome headless (155.0.8059.39 on the dev box, the
`ubuntu-latest` runner's Chrome 155) on `scripts/interop/whep-page.html`,
which loads the reference player under `docs/webrtc/` unmodified by
relative path with its de-duplication off, so every record arrives from
both carriages and each carriage is proved on its own. The mount is served
by `tst-interop serve-whep`: the profile's KLV schedule, which is the
generator's exact record set, and a real 320x240 Constrained Baseline
H.264 GOP as the video, because the generator's synthetic slices decode in
no browser (Chrome logs "Failed to parse SPS NAL unit" and renders
nothing, so a "video plays" oracle could never pass on them). The cells
are declared in the census, which is 161 cells since they were added; the
first runner census with them read 161 declared, 0 FAIL, 0 skipped
([run 39547857032](https://github.com/aklofas/ts-transformer/actions/runs/39547857032)).

- **`whep/chromium-sei-dc-klv`** serves an MP2T mount (the `push_ts` path a
  muxed source takes) under `AudioPolicy::Drop`, so the browser's audio
  section is answered with port 0. The cell's tier is `remux` (WebRTC is
  not byte-transparent TS). Oracles: the set of KLV records seen in the SEI
  equals the generator's set digest (`verify --file`'s `klv_set_sha256`),
  the set seen on the data channel equals it too, each record's two copies
  name the same (PTS, RTP timestamp) so the data-channel copy is bound to
  the frame its SEI copy rides in, the decoded frame count (`FRAMES`,
  `totalVideoFrames`) is above 0 within the first five seconds and rises
  every second after that (the video is not black), no `AUDIO` line is
  printed, and the page reports no error. Locally at `--seconds 10`: 100
  of 100 records on each carriage, 239 of 240 frames decoded (the last
  access unit stays in the mount's demuxer, documented on `push_ts`).
- **`whep/chromium-opus-audio`** serves an elementary mount under
  `AudioPolicy::PassthroughOpus`, pushed directly with one synthesized
  20 ms Opus silence packet every 20 ms (the CELT silence frame libopus
  emits, three bytes). The same oracles, plus the inbound audio packet
  count (`AUDIO`, `packetsReceived`) rising (400 of 400 at 8 s, 0 audio
  frames dropped on the mount) and the KLV PTS multiset equal to the
  schedule's.

**The cells found a player defect, fixed.** On their first run Chrome
delivered every record on the data channel and none via SEI: an
`RTCRtpScriptTransform` assigned inside `ontrack` on a plain
`RTCPeerConnection` never receives a frame, although the worker starts.
Six probe pages isolated it; the transform must be attached to the video
receiver right after `addTransceiver`, before the offer is created, and
that is what the shipped player now does. Before the fix both cells failed
on exactly the SEI oracle (0 records); after it both pass. The cells also
pin a library behaviour this release keeps: an MP2T mount carries PTS 0 on
every async private-data KLV record, because the demuxer reports no PTS
for a KLV PES without one; the data-channel copy names the bound frame's
RTP timestamp, so playback is unaffected, and the deferred row "Async KLV
PTS on MP2T mounts = the bound frame's PTS" names the change that would
re-pin the cell.

### Fuzz targets

Three `cargo-fuzz` targets in `tst-webrtc` cover the server's peer-facing
parsers. `tests/coverage/fuzz-targets.toml` lists all 40 targets in the
workspace.

| Target | Drives |
| --- | --- |
| `whep_http_request` | the signalling request: the request line, headers and body-length framing within the head and body limits, and routing |
| `whep_sdp_select` | SDP offer parsing and payload selection, seeded with the browser offers under `crates/tst-webrtc/tests/fixtures/offers/` |
| `klv_sei_parse` | the KLV SEI payload (UUID, version, PTS, record) the reference player extracts from a frame |

**A browser offer that gives one payload type two formats crashed the
server, fixed.** The CI fuzz leg found that str0m asserts on an
offer whose `rtx` payload names two codecs through `apt`, or that maps one
payload number to different formats in two media sections, which ended the
server's media thread. Such an offer is now answered with 400 before str0m
sees it, with regression tests for both shapes.

### Soak leg (`whep`)

`soak.sh` runs a fifth leg beside SRT, RIST and the two publish legs.
`tst-interop serve-whep` binds a WHEP server with one MP2T mount and serves
the leg's profile for the whole run the way the `whep/chromium-sei-dc-klv`
cell is served: the profile's KLV schedule muxed in-process and fed through
`push_ts`, with a real H.264 GOP as the video. A viewer loop plays it with
headless Chrome on the matrix's page, the reference player under
`docs/webrtc/` loaded unmodified, one browser per `WHEP_RECONNECT_S` slot
(600 s by default), each ended by its own timeout 3 s before the slot ends.
The server is never told: a browser leaving is a session it reaps, a browser
arriving a session it starts at the next keyframe. The leg is designed as
follows:

- **Reconnects counted on both sides.** The run declares the number of
  browser connections before launch (`run / WHEP_RECONNECT_S`, at least
  two). The `whep_reconnects_observed_whep` verdict requires the viewer
  loop's report to list exactly that many connections on the declared slot,
  each browser's window to be that slot less the 3 s relaunch gap, AND the
  server's own `sessions_total` to equal the declared count.
- **Playback continuous, at the served rate.** The page prints the decoded
  frame count once a second. `whep_frames_continuous_whep` requires every
  connection to have been ended by its own timeout, to have reported no
  error, to decode its first frame within 5 samples, never to go more than
  2 samples without the count rising (no gap over 2 s except at a
  reconnect), and to end with at least 0.9 of the served frame rate (the
  server's `frames_pushed` over its window) times its samples after the
  first-frame window, so a decode that only ticks fails. The server must
  have dropped no packet for a full queue and demuxed nothing
  non-conformant.
- **Both KLV carriages agree, at the served rate, on the same frame.** With
  the player's de-duplication off, every record arrives from the SEI and
  from the data channel. The page prints one digest and the bound frame's
  (PTS, RTP timestamp) per record and carriage; `whep_klv_sei_equals_dc_whep`
  requires the two sets to be equal in every connection, each carriage to
  hold at least 0.9 of the served record rate (`klv_records_pushed` over
  the window) times the samples after the first-frame window, and every
  record's two copies to name the same (PTS, RTP timestamp): both come
  from one server-side binding, and only a run long enough to cross a PTS
  wrap or a clock rebase can see them differ. A record on one carriage
  only is allowed among the first 3 records of its carriage (the data
  channel opens a moment after the session is connected) or the last 2
  (the browser is ended while the server pushes), and such records are
  counted; a record missing anywhere else fails. The server must have
  dropped no record unbound or oversize.
- **Declared like the other legs.** `soak-config.json` declares the leg's
  profile, mount kind, viewer command, slot and connection count before
  any worker launches; `profile_declared_whep` holds the server's counters
  to the profile and mount kind. The leg has no proxy and no corruption
  tap, so those verdicts read "not applicable" with the reason. RSS is
  sampled for `serve-whep` and for the viewer loop's shell; the browser's
  own memory is not sampled, as a publish leg's tool is not.

The first one-hour run, on 2026-10-21 from 06:20:40Z to 07:21Z, used
the default 600-second slot: six connections, five reconnects. The SRT and
RIST legs drew `audio` and `av1-klv-b` from seed 1 and both publish legs
ran as in their own one-hour run. It was built from the tree
`v0.8.0-458-ga5c0e9d7` and judged at the end of the run by that tree's
`report soak`. The volume floors, the server drop-counter gates and the
copy-agreement rule described above were added afterwards, in the tree
`v0.8.0-460-g136ec408`; that tree's `report soak` re-judged the run from its archived
inputs with the same `overall_pass=true`, 93 verdicts and 0 failing, and
the floor figures below are that re-judgment's (its `soak-results.json`
sits beside the archive under `rejudged-136ec408/`, with its own md5).

**The run's harness verdict is `overall_pass=true`: every leg passed every
gating verdict.** Of its 93 verdicts, 79 are gating PASS, 0 are FAIL and
14 are provisional PASS (the RSS slopes and the two reconnect counts,
provisional by design below 72 h). The whep leg's figures, from the run's
`soak-results.json` and, for the floors, from the re-judgment's:

| Measure | Result |
| --- | --- |
| Browser connections | 6 on the viewer's side, 6 sessions on the server's, 6 declared; every browser ended by its own timeout |
| Frames decoded | 107 206 over 6 connections of 597 s; first frame on the first sample of every connection; longest stall 0 s; every connection's final count at least 17 857 against a floor of 15 957 (0.9 × 30 frames/s × 591 samples) |
| KLV records seen | 35 788 via SEI, 35 788 via the data channel, equal sets in every connection; at least 5961 per carriage and connection against a floor of 5319 (0.9 × 10 records/s × 591); 0 records on one carriage only; 0 copies disagreeing on (PTS, RTP timestamp) |
| Served | 107 999 frames (3600 keyframes), 36 000 records, 36 000 SEI insertions, 0 data-channel messages dropped, 0 packets dropped for a full queue, 0 PLIs, 0 NACKs |
| Errors | 0 page errors, 0 browser console errors, 0 unrouted datagrams |
| RSS slope, `serve-whep` | 417.7 KiB/h over 98 samples (provisional below 72 h; 18.3 MB at the end of the warm-up, 18.9 MB at the end) |
| RSS slope, viewer loop | 16.2 KiB/h over 98 samples (provisional below 72 h) |

The 212 records the viewer did not see fell in the five 3-second relaunch
gaps and each browser's connect time, when no browser was connected. The
run's full artifact set is retained offline by the maintainer.

<!-- BEGIN GENERATED by scripts/gen/qualification.sh (evidence); do not edit -->
## Evidence for the main at 4a3a0b96 tree

Qualified commit `4a3a0b963431` (`refs/heads/main`, a clean tree). Bundled natives: libsrt 1.5.7, librist 0.2.20, mbedTLS 3.6.7. Toolchain: rustc 1.93.1; public API pinned to nightly-2026-07-03; cbindgen 0.29.2. Runner: ubuntu-24.04 x86_64, kernel 6.17.0.

Verdict: **GO**. Every required leg succeeded on this commit.

Release gate: soak soak-0.9.0-2026-10-27 (tree 9b8acb3b6fae, 259200 s, legs rist, rtsp-publish, rtsp-publish-es, srt, whep, PASS), corpus corpus-0.9.0-2026-10-24 (PASS).

Release-only checks

real-capture corpus: The 0.9.0 release corpus run: 251 real captures (39.8 GB) swept with no parse or invariant failure, the first baseline on this tree; four captures replayed over all five paths with no failure; receiver RSS recorded, not gated. (files 251, identical 0, changed+explained 0, invariant failures 0, parse failures 0; replay 4 files × 5 paths, 0 failed)

Test legs on this commit:

- `interop`: success ([run](https://github.com/aklofas/ts-transformer/actions/runs/40626631867)).
- `sanitizers/asan`: success ([run](https://github.com/aklofas/ts-transformer/actions/runs/40626631867)).
- `sanitizers/asan-native`: success ([run](https://github.com/aklofas/ts-transformer/actions/runs/40626631867)).
- `sanitizers/tsan`: success ([run](https://github.com/aklofas/ts-transformer/actions/runs/40626631867)).
- `sanitizers/tsan-native`: success ([run](https://github.com/aklofas/ts-transformer/actions/runs/40626631867)).
- `sanitizers`: success ([run](https://github.com/aklofas/ts-transformer/actions/runs/40626631867)).
- `fuzz`: success ([run](https://github.com/aklofas/ts-transformer/actions/runs/40626631867)).
- `jvm-jar`: success ([run](https://github.com/aklofas/ts-transformer/actions/runs/40626631867)).
- `python-wheels`: success ([run](https://github.com/aklofas/ts-transformer/actions/runs/40626631867)).
- `c-abi`: success ([run](https://github.com/aklofas/ts-transformer/actions/runs/40626631867)).
- `ci (on this sha)`: success ([run](https://github.com/aklofas/ts-transformer/actions/runs/40626603941)).
- `semver-checks`: success ([run](https://github.com/aklofas/ts-transformer/actions/runs/40626631867)), informational.

The interop census (full-161) reads 161 / 96 / 0 / 65 / 0: total, pass, fail, expected-unsupported, skipped. Stale expectations: 0. The declared full census is 161 cells, at realistic access-unit sizes.

Peer tools: chromium Chromium 155.0.8059.0 , ffmpeg ffmpeg version 6.1.1-3ubuntu5 Copyright (c) 2000-2023 the FFmpeg developers, google-chrome Google Chrome 155.0.8059.39 , gst-launch-1.0 gst-launch-1.0 version 1.24.2, mpv mpv 0.37.0 Copyright © 2000-2023 mpv/MPlayer/mplayer2 projects, tsp tsp: TSDuck - The MPEG Transport Stream Toolkit - version 3.44-4676, vlc VLC version 3.0.20 Vetinari (3.0.20-0-g6f0d0ab126b).

Fuzz: 40 of 40 declared targets ran for 60 s each with 0 crash(es).

C ABI 0.25 with 505 exported symbols, compared with v0.8.0 (built): abidiff exit 0, compatible.

cargo-semver-checks, informational: tst-core 7 breaking; tst-hls 5 breaking; tst-pipeline 9 breaking; tst-rist 4 breaking; tst-rtp 10 breaking; tst-srt 4 breaking; tst-tcp 5 breaking; tst-udp 5 breaking; tst-webrtc not analysed; tstrans-mbedtls-src 0 breaking; tstrans-rist-sys 1 breaking.

Cited soak, stress and corpus archives, with the verdicts they recorded:

- `soak-rc070-2026-10-04` (soak, 2026-10-04, tree `e86dd9ea6d1a`): `overall_pass=false`, 259200 s declared. The SRT leg passed every gating verdict; the RIST leg failed corruption attribution, and all eight of its unexplained events were harness attribution defects, not library defects.
- `soak-rtsp-publish-2026-10-08` (soak, 2026-10-09, tree `b6545e6004ae`): `overall_pass=false`, 3600 s declared. The publisher leg passed every verdict it owns; the three failures were on the SRT leg's receiver and were harness defects.
- `soak-whep-2026-10-21` (soak, 2026-10-21, tree `a5c0e9d7a6af`): `overall_pass=true`, 3600 s declared. The first one-hour run with the whep leg: six browser connections, every verdict on every leg passed, and no KLV record was seen on one carriage only; the RSS slopes are provisional below 72 hours.
- `soak-0.9.0-2026-10-27` (soak, 2026-10-27, tree `9b8acb3b6fae`): `overall_pass=true`, 259200 s declared. The 0.9.0 release soak: 72 hours with all five legs, 89 of 93 verdicts passed and the SRT receiver reconnected across all 12 outage windows. The harness reported overall_pass=false on four verdicts: recv_invariants_rist from the rist leg's AV1 carriage check (a harness oracle gap: it does not excuse corruption the impairment schedule attributes), worker_exits from one external-publisher teardown race in 432 generations on the rtsp-publish leg, and recv_invariants_rtsp-publish-es with klv_placement_rtsp-publish-es from a 2.3 ms KLV placement excursion over one sender-report interval on the ES leg. None is a library defect; the maintainer re-judged the run as a pass on 2026-10-28, and the harness fixes are queued for 0.9.1. The run's own report, with the harness verdict, is retained offline.
- `stress-2026-10-02` (stress, 2026-10-02, tree `a64702459299`): `overall_pass=false`. The sweep ended early and the hold was refused for harness calibration findings; it is not a measurement and no ceiling is published from it.
- `stress-2026-10-02b` (stress, 2026-10-03, tree `f86f4406b86e`): `overall_pass=false`. The sweep completed; the hold was killed by the kernel for memory because it was sized from CPU alone, so the sweep is valid and the hold is not.
- `stress-2026-10-03` (stress, 2026-10-04, tree `3a3f964f5c6b`): `overall_pass=false`. The sweep passed; the hold ended at 58 081 of 86 400 s on a harness artefact in the receiver restart path, not a library defect.
- `stress-2026-10-06` (stress, 2026-10-08, tree `9bb728e1484b`): `overall_pass=false`. The sweep and the full 24 h hold ran; resource and liveness verdicts passed and the hold verdict is FAIL on corruption attribution gaps, a harness attribution artefact.
- `2026-10-04-wp1-corpus-evidence` (corpus, 2026-10-03, tree `effc7f9c4d4d`): `overall_pass=true`. The demuxer output was byte-identical before and after the KLV classification change on all 260 real captures (41.5 GB); no real producer trips the reserved-nibble rule.
- `corpus-0.9.0-2026-10-24` (corpus, 2026-10-24, tree `9b8acb3b6fae`): `overall_pass=true`. The 0.9.0 release corpus run: 251 real captures (39.8 GB) swept with no parse or invariant failure, the first baseline on this tree; four captures replayed over all five paths with no failure; receiver RSS recorded, not gated.

Checks that stay with the maintainer and are not part of this record:

- long fuzz runs (10 min per target)
- bench regression
- one-hour synthetic soak
<!-- END GENERATED by scripts/gen/qualification.sh (evidence) -->

The 0.9.0 record above predates the waiver table: it shows the soak row as
it stood when the record was collected, `overall_pass = true`. The archive
table now carries the harness's own `false` with the maintainer's waiver
beside it, as the runs table on this page shows.

## Evidence for the 0.8.0 tree

The publisher-role figures above were produced on `43834194`. The library
changes that landed after it and before the 0.8.0 tag are the pre-tag fix
set listed in the CHANGELOG's 0.8.0 entry: control bytes refused in mount
paths, the interleaved RTCP drain for TCP readers, the per-request idle
bound, the UDP ingest end gate, the mount-registration re-check under the
mounts lock, the sender-report plausibility window and the origin-tick KLV
placement, `MaxBandwidth` renamed and `Socket::close` made infallible, and
the Python exit park. The last library commit before the tag is `c9cd8163`;
the commits after it change a fuzz harness oracle, CI workflows, one test's
read margin and the version numbers. Evidence on `c9cd8163`:

- the 159-cell interop census at 159 / 94 / 0 / 65 / 0 at realistic
  access-unit sizes
  ([run 38206815679](https://github.com/aklofas/ts-transformer/actions/runs/38206815679),
  the weekly schedule, 2026-10-12);
- all four sanitizer jobs, ASan and TSan over the pure-Rust and the
  native-linking crates
  ([run 38505046089](https://github.com/aklofas/ts-transformer/actions/runs/38505046089),
  the nightly schedule, 2026-10-14). The nightly run of 2026-10-13 on the
  same tree failed one TSan test, `tst-rtp`'s
  `malformed_request_line_gets_400_and_the_next_request_is_served`, on the
  2 s socket read margin it used to tell an answer from silence; the margin
  is now 10 s and no library code changed;
- ClusterFuzzLite's first one-hour batch over the 36 fuzz targets, under
  AddressSanitizer, on `4bb93a17`
  ([run 38543594093](https://github.com/aklofas/ts-transformer/actions/runs/38543594093)):
  no crash. Its first 600 s pull-request run had found a harness oracle
  defect, `klv_st0601_patch` comparing records whose `field_errors` carried
  a NaN, fixed before the batch ran. See "Fuzz targets" above for the
  bundle.

## Evidence for the 0.9.0 tree

The frozen 0.9.0 tree is `9b8acb3b`. The results below were produced on
`907adb30`, after the merge of the WebRTC playback work and before the
0.9.0 version sweep. The release fixes that followed changed
library and binding code (among them the HLS mux publisher's segment cuts
and playlist lifetimes, and the WHEP server's DTLS source binding), so
these earlier results do not by themselves qualify `9b8acb3b`. Evidence on
`907adb30`:

- the interop census at 161 / 96 / 0 / 65 / 0 at realistic
  access-unit sizes, including the two `whep/*` browser cells
  ([run 39687057760](https://github.com/aklofas/ts-transformer/actions/runs/39687057760),
  `workflow_dispatch`, 2026-10-21): `full-161` declared and produced, 0
  skips, 0 stale rows;
- all four sanitizer jobs, ASan and TSan over the pure-Rust and the
  native-linking crates
  ([run 39687054921](https://github.com/aklofas/ts-transformer/actions/runs/39687054921),
  2026-10-21);
- ClusterFuzzLite's scheduled batch of 2026-10-21 under AddressSanitizer,
  with its corpus prune and coverage report
  ([run 39543668790](https://github.com/aklofas/ts-transformer/actions/runs/39543668790)):
  no crash. It ran on `c7d9cf52`, the merge of the WebRTC code
  that precedes the browser-evidence, interop and soak-leg work and the
  Python binding of WebRTC playback, so it does not cover the final WebRTC
  code;
- the one-hour WHEP soak, archive `soak-whep-2026-10-21`, listed in the
  runs table above.

The release qualification of the 0.9.0 tree is the generated block above,
produced in release mode on the date commit plus the archive row below it:
the frozen tree with only the CHANGELOG and the evidence archive list
changed. The gate reads the newest 72-hour soak archive for the tree,
`soak-0.9.0-2026-10-27`. The harness judged that run `overall_pass=false`
on four verdicts; the runs table above names them and the archive's own
report analyses them: a harness oracle gap (the AV1 carriage check does not
excuse corruption the impairment schedule attributes), one external
publisher's teardown race in 432 generations, and a 2.3 ms KLV placement
excursion over one sender-report interval on the elementary publish leg.
None is a defect in the library: harness FAIL on four verdicts, waived by
the maintainer (harness defects, fixed in 0.9.1); the release was tagged on
that waiver, which the release gate no longer admits. The archive row keeps
`overall_pass=false` and carries the waiver beside it. The harness fixes
are the AV1 oracle's excusal, the publisher's post-end-of-stream exit rule
and the placement oracle's excursion rule.

## Stress and ceilings

A stress run is a different instrument than a soak: it sweeps stream count
and per-stream bitrate on each transport to find the ceiling where
something stops holding steady, then holds a system sized at 70% of that
ceiling for 24 hours under the soak's own impairment schedule. **A stress
run is not a soak PASS, and a soak is not a ceiling** — the soak evidence
above proves endurance at a size already chosen; a stress run is what
chooses that size. See [`benchmarks.md`](/docs/project/benchmarks.md) for the verdict
definitions, how to read a ceiling, and the measured results of the
2026-10-03 stress run. Archives from each run — a results file, a provenance file,
and one subdirectory per sweep step holding that step's raw logs — are
retained offline by the maintainer.

## Reading `expectations.toml`

[`scripts/interop/expectations.toml`](/scripts/interop/expectations.toml)
is the accepted-nonconformances record: every `FAIL` this matrix has ever
produced either has a row here, backed by a run that actually reproduced
that exact failure, or it's an unresolved regression that fails the CI job.
The whole file was re-validated on 2026-09-14 against the realistic
access-unit regime described above, and came through unchanged: every row
still reproduced, and none went stale. Two verdict kinds:

- **`expected_unsupported`** — this (cell, profile) pair is known to fail
  and isn't expected to ever pass. If it starts passing, `report merge`
  reports it as a stale expectation and exits nonzero, so the row has to
  be removed — this is how a fixed gap surfaces for cleanup rather than
  silently lingering as a row nobody re-checks. Staleness is fatal
  everywhere, with no warn-only mode: a local run, a branch dispatch and
  the CI job all reject it the same way.
- **`known_flaky`** — this pair intermittently fails; a `FAIL` is reported
  non-fatally and a `PASS` is simply normal, never flagged stale.

An optional `failure_contains` key narrows a row to only match a `FAIL`
whose failure text contains a specific substring — load-bearing whenever a
(cell, profile) pair can genuinely fail for more than one distinct,
already-understood reason (several rows above hit this: an ffmpeg-remux
cell can fail on either the KLV-truncation mechanism or a completely
unrelated one, and a plain cell/profile match would blanket over whichever
one it wasn't written for). A `FAIL` whose text doesn't match falls through
to whatever else might match, or — if nothing does — surfaces as a genuine,
unexpected failure. This is the integrity property the whole file rests
on: **an undocumented `FAIL` always fails the run.** There is no way to add
a permissive expectation that quietly widens to catch failures it wasn't
written to describe.
The match is made per failure line: a row explains only the lines of a
cell's `FAIL` that contain its text, so a second, different failure on the
same cell keeps it a `FAIL`.

## Third-party field validation

A third-party integrator has independently validated this codebase against
their own real-world flight capture data on their own embedded-Linux
target. An anonymized summary of that validation will be added to this page
once the consenting party has reviewed the exact text — no numbers,
platform names, or other identifying detail appear here until that review
completes.
