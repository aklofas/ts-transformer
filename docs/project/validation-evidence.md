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

# Full transport + format matrix (159 cells; ~8s/cell locally is what
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
pass or fail. Both scripts are validated on linux-x86_64; linux-aarch64 is
expected to work (no arch-specific code) but hasn't been validated yet —
`run-matrix.sh` additionally depends on per-arch apt/deb availability of
the peer tools above. `soak.sh`'s SRT and RIST legs need no third-party
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

**Current census: 159 cells — 94 PASS, 0 FAIL, 65 EXPECTED-UNSUPPORTED, 0
SKIPPED — measured at realistic access-unit sizes**
([run 37875160134](https://github.com/aklofas/ts-transformer/actions/runs/37875160134),
`workflow_dispatch`, 2026-10-09 at `41ed12e4`, `--seconds 10`, shape
`full-159`, empty allowed-skip list, no stale expectation). The two cells
added on 2026-10-08 both pass, as they did on their first CI run
([run 37858440566](https://github.com/aklofas/ts-transformer/actions/runs/37858440566), 2026-10-08 at `945911d4`, the same census). Each has a GStreamer `rtspclientsink`
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
(`crates/tst-rtp/src/rtsp/`), or the workflow file itself. The verified
run cited above is
[run 37875160134](https://github.com/aklofas/ts-transformer/actions/runs/37875160134)
(the 2026-10-09 `workflow_dispatch` at `41ed12e4`), which completed
`success` with the census-completeness assert, the 159 / 94 / 0 / 65 / 0
census, and 159 per-cell result records with zero `FAIL` and no
expectation drift: all 65 documented-gap rows reproduced. The first run
with the RTSP publisher cells,
[run 37858440566](https://github.com/aklofas/ts-transformer/actions/runs/37858440566)
(2026-10-08 at `945911d4`), reached the same census. The first public run at realistic
access-unit sizes,
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
  two rounds of disproven hypotheses recorded honestly.** A `tsp -P regulate`
  or `gst` paced sender lands well inside this project's nominal-count
  tolerance but the received `stream_sha256` doesn't exactly match — the
  same pacing over RIST and UDP is byte-perfect in the same run, so this is
  SRT-specific. Round 1 suspected the receive-side deadline; round 2
  deliberately disproved that by giving the receiver 2.5x the sender's real
  content duration and reproducing the identical byte shortfall — the
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
  against the spec text in this session); and FFmpeg can't determine the
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
raw-TS parser (`crates/tst-interop/src/rawts.rs`) in addition to the
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
under "Publisher role" below, with the rest of that role's evidence.

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
159 interop-matrix cells inject corruption — the census above is a pristine
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
correctly reads 0; this is recorded, not gating (see `report.rs`'s own
module doc for why that check stays provisional even once a real count is
available).

RSS sampling with `--no-klv-digest` active on every process (the fix from
an earlier round) shows the SRT leg's `send`/`recv` both flat at 0.0
KiB/hour, matching the proxy baseline — the digest-accumulation fix
accounts for its entire prior growth. The RIST leg's `send`/`recv` still
show measurable growth (1485.1 / 349.3 KiB/hour respectively) — an order
of magnitude down from the pre-fix ~5.8 / ~4.0 MiB/hour, confirming digest
accumulation was the dominant cause there too, but not zero: an
unexplained RIST-specific residual remained an open watch item at the
time (plausibly librist's own recovery-buffer growth, per the earlier
round's own unresolved note) — since resolved by the 72-hour run below,
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
  long run also covers a codec/carriage/cadence shape the 159-cell matrix
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
judged as one on a declared PID. The
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
provisional PASS.

- **SRT leg:** passed every gating verdict. That covers the twelve
  scheduled outage windows survived with 12 reconnects, and 107,849
  corruption injections with zero unexplained, undetected or unrecovered
  events.
- **RIST leg:** failed corruption attribution. All three gating FAILs
  (`worker_exits`, `recv_invariants_rist`, `corruption_attributed_rist`)
  are that one finding, described under "The RIST attribution failure"
  below.
- **Memory:** no process grew past the RSS gate.

What ran. The tree was `e86dd9ea`, clean at launch (`provenance.json`:
`dirty=false`, 10 submodules pinned). It was built `--release` with the
vendored libsrt / librist / mbedTLS on a dedicated cloud VM with 2 vCPUs
and 3.9 GB, as `provenance.json` records it. The command was
`SRT_RECONNECT_MODE=background bash scripts/interop/soak.sh --hours 72
--seed 11`, run from 2026-10-01T18:32Z to 2026-10-04T18:37Z.

Seed 11 drew `pts-rollover` for the SRT leg and `klv-sync` for the RIST
leg. Both legs ran the current shape described above:

- twelve seeded impairment phases of six hours each, with 0.5–4 % loss;
- the corruption tap, at 5 per 10,000 packets with seven damage classes;
- rich ST 0601 KLV.

The SRT leg's proxy also applied a 90-second full-drop outage every 6
hours, which makes twelve windows over the run. Every one of those choices
was declared in `soak-config.json` before launch, and every `*_declared_<leg>`
verdict passed.

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

The 177 MB reading for the RIST sender was an ad-hoc `ps` sample. The
gated 30-second series shows that sender flat at 181.3–181.4 MB from about
4.5 hours to the end, so 181.4 MB is the plateau and there was no growth.

**The RIST attribution failure.** The RIST receiver reported 8 events that
the harness could not tie to an injection. It exited 1 on that verdict, so
the one finding fails three gating verdicts: `worker_exits`,
`recv_invariants_rist` and `corruption_attributed_rist`. The events are
spread from 13.8 h to 71.6 h into the run; none falls at teardown.

- **Six of the eight are a gap in the harness.** Each is a PSI checksum
  error on the PMT PID, 5 packets after the tap truncated that PMT packet.
  The `klv-sync` profile's PMT section runs to byte 59, so a truncation
  that keeps fewer bytes cuts inside the section. The demuxer then
  correctly reports the section's CRC failure. The harness's
  injection-to-signal expectation table does not admit a PSI checksum as
  a consequence of a truncation, so it could not attribute these events.
  This was reproduced offline on the soaked tree: cutting a `klv-sync` PMT
  to 45, 52 or 58 bytes reports the checksum error each time. The
  `pts-rollover` profile's PMT section ends at byte 37, which no
  truncation reaches; the SRT leg had 83 truncations on its PMT PID and no
  such event.
- **The other two are a second harness attribution defect.** One is a PAT
  checksum error at about 58 h; the other is a resync with no PID at about
  71.6 h. Each had its true cause 1–6 packets before it: a `psi_flip` on
  the PAT, and a `garbage` injection on the video PID. The harness had
  stopped tracking that cause, because its raw reader takes a PCR anchor
  from any packet with no plausibility check. After a framing injection, a
  mis-framed packet occasionally has its payload bytes read as a header.
  If those bytes carry adaptation-field and PCR flags, the reader hands the
  attribution engine a random anchor. Every pending injection that anchor
  appears to be more than 0.4 s ahead of is then marked unresolved and
  never judged. The report therefore named an injection about 1,100 packets
  earlier as the "nearest". This was reproduced with the real `send` and
  `recv` binaries over a byte-transparent relay:
  - with no extra packets, 149 of 149 injections resolved and the run
    passed;
  - with 12 inserted packets carrying a PCR 100 s ahead, 84 injections
    were stranded, 37 events were charged unexplained, and the run
    failed, with failure text of the same shape as the soak's.

  The reproduction forces the anchor; the soak's own anchors are inferred
  from its counters, since its PCR stream was not archived.

Six of the eight unexplained RIST events match the reproduced
expectation-table defect directly (each is a PSI-checksum error five
packets after a PMT truncation past byte 44). The other two are attributed
to the reproduced anchor-stranding defect as a high-confidence inference:
the mechanism is reproduced, each event has a plausible cause 1–6 packets
earlier, and the receiver logged no decode or transport error — but the
run's own PCR anchors were not archived, so the attribution is inferred
from the counters rather than observed. No evidence from this run
contradicts the receiver, the demuxer or the RIST transport; none of the
eight events is explained by a library defect. The `overall_pass=false`
above is the harness's verdict on this run, and this page does not
re-label it as a pass.

Both fixes are in the harness, not the library, and are tracked for the
harness follow-up:

- accept PCR anchors only from declared PCR PIDs, or from confirmed-lock
  packets, with a bound on how far an anchor may jump;
- require two confirming anchors before an injection is stranded;
- report any unresolved injection as a limitation;
- admit the PSI-checksum pairing for framing injections on a PSI packet
  in the expectation table (`tst-interop` `expects()`).

Everything else on the RIST leg passed: drop rate, coverage (100.0 %
ingested, 100.0 % resolved), detection (0 undetected), recovery (0
unrecovered) and the rich-KLV oracles.

**What Background mode means for these numbers.** During each outage the
SRT leg's managed sender went on producing into a bounded gap buffer (256
messages, `drop_oldest`). When the link came back it sent what the buffer
still held, and it evicted what it could not hold. So this run shows that
both ends reconnect after every scheduled outage and that the received
stream recovers. It does not show that every source frame was delivered,
and it says nothing about Blocking mode's stalled-producer replay, which
the 2026-08-05 run below exercised. The SRT leg received 33,090 fewer video
AUs than it sent, by design.

The sender's counters are recorded, not gated:

- 144 reconnect attempts and 12 successes;
- 201,132 messages (236.9 MB) evicted from the gap buffer;
- an empty buffer at exit (`gap_len_at_exit` 0).

The eviction counters are not the whole cost of an outage. They miss what
the transport had already accepted before it noticed the break, and the
sent-minus-received difference is the fuller figure. Nothing on this page
claims that a frame accepted into the gap buffer was later delivered.

**How the run was judged.** Each leg is judged by its own receiver's
end-of-run report against its own corruption log.

- **Coverage:** `corruption_coverage_srt` passed at 100.0 % ingested and
  99.6 % resolved, `corruption_coverage_rist` at 100.0 % and 100.0 % (the
  floors are 99 % and 90 %).
- **Duration:** `duration_coverage` passed at 259,137 s against 259,200 s.
- **Samples:** all six `rss_sample_coverage_<leg>_<process>` verdicts
  passed, at 8,557 of 8,578 expected samples each with a largest gap of
  31 s.
- **Exits:** `zero_process_exits` passed. `worker_exits` failed only on
  the RIST receiver's exit 1 described above.

The two provisional verdicts are `reconnect_count_matches_outage_count_srt`,
which passed at 12 rebuilds against 12 windows, and its RIST counterpart,
which is not applicable without an outage. They stay provisional because
the counter counts factory rebuilds, not outage windows. All 39 other
verdicts gate.

Every RSS slope passed the 200 KiB/hour gate after its 30-minute warm-up:

| Process | Slope (KiB/hour) |
| --- | --- |
| RIST sender (worst) | 22.8 |
| SRT sender | 9.2 |
| SRT proxy | 5.2 |
| SRT receiver | 5.0 |
| RIST receiver | 1.8 |
| RIST proxy | 0.4 |

The verdict document also records five standing limitations:

- the managed sender's reconnect counters are recorded, not gated;
- the drop-rate check is aggregate, not localised per event to the
  outage windows;
- outage-window drops are excluded from both sides of that check;
- a worker killed before it writes its report makes `report soak` exit 2
  rather than name the failed role;
- the gap-buffer eviction counters are not an outage's whole cost.

The CPU, file-descriptor and thread telemetry (`proc.csv`, `host.csv`)
appears in this run for the first time. It is recorded, not gated, and the
figures below are observations, not verdicts.

- **RSS at the end of the run:** SRT sender 172.1 MB (a plateau of about
  173 MB from 6 h), RIST sender 181.4 MB, SRT receiver 11.4 MB, RIST
  receiver 10.0 MB, proxies 7.0 and 6.5 MB.
- **CPU over the whole run** (one core = 100 %): SRT sender 2.14 %, SRT
  receiver 2.07 %, RIST receiver 1.71 %, RIST sender 1.63 %, proxies 0.76 %
  and 0.54 %. One-minute host load (2 vCPUs) had a median of 0.06 and a
  mean of 0.08; 26 % of the 30-second samples were above 0.11 and the
  maximum was 0.73 (at 131 073 s, outside any outage window).
- **Threads and descriptors:** the two RIST processes and both proxies held
  the same counts in every sample after launch. The SRT pair returned to its
  baseline (sender 4 threads / 5 descriptors, receiver 6 / 5) after each of
  the 12 outage windows, with bounded transients of up to 90 s inside them: the
  receiver 5 threads / 6 descriptors, the sender 3 or 5 threads (once 7, at
  237 482 s) and 4 to 6 descriptors. There was no drift across the run.

**Which binary this is evidence for.** The soak exercised the `e86dd9ea`
binary, not the tree tagged 0.7.0. The library changes that landed after it
are listed in the CHANGELOG's 0.7.0 entry:

- KLV classification and malformed-PES handling in the demuxer;
- the ST 0601 Tag 102 and SPS-crop parser bounds;
- RTSP server session teardown;
- zero-length UDP / RIST datagrams;
- binding error kinds.

Before those, the `tcps://` close drain had also landed after `e86dd9ea`.
The tagged tree's own evidence for these changes comes from shorter runs,
all on `effc7f9c`, the last library change before the tag:

- the 157-cell interop census at 157 / 92 / 0 / 65 / 0
  ([run 37181827346](https://github.com/aklofas/ts-transformer/actions/runs/37181827346),
  `workflow_dispatch`, 2026-10-04);
- all four sanitizer jobs, ASan and TSan over the pure-Rust and the
  native-linking crates
  ([run 37181828490](https://github.com/aklofas/ts-transformer/actions/runs/37181828490),
  2026-10-04);
- a comparison over the maintainer's local corpus of 260 captures (one of
  them a reconstruction, derived rather than captured raw; 41.5 GB),
  demuxed by the tree before those fixes (`d2fa73dc`) and by
  `effc7f9c`. The two outputs are byte-identical. No raw capture's KLV PID
  has an `Unknown` sample. The reconstruction has 32, and they are the
  same under both trees. This proves only that the fixes did not change
  how real streams demux. No corpus KLV record reaches 13 319 bytes (the
  largest is 302 B), and no PID reported a malformed PES, so the corpus
  does not exercise the large-async-KLV or malformed-PES-neighbour fixes.
  The synthetic reproductions in the CHANGELOG entries are what prove
  those;
- a one-hour soak smoke at the shape above (seed 11, Background mode),
  2026-10-04T06:53Z–07:56Z, with `overall_pass=true` (41 verdicts, 8 of
  them provisional):
  - **SRT:** 107,954 of 108,000 video AUs received, a 1.82 % drop rate
    against 1.83 % expected, and 1,509 injections of which 1,508 resolved,
    with 0 unexplained events.
  - **RIST:** 107,990 of 108,000 received, 1.88 % against 1.88 %, and
    1,534 injections, all resolved, with 0 unexplained events (1
    unexplained discontinuity within the budget of 24) and 1 excused as
    transport loss.

  No outage window falls inside one hour, so this smoke does not exercise
  the reconnect path. The provisional verdicts were the 6 RSS slopes,
  which are not gated below 72 h, and the 2 reconnect counts.

None of those runs is endurance evidence, and the 72-hour figures above
belong to `e86dd9ea`. The raw archive (`soak-results.json`,
`provenance.json`, the 30-second RSS samples, per-process logs and
corruption logs) is retained offline by the maintainer. Its 32 files were
md5-checked against the VM before teardown: 32 matched and none failed.

### The previous run (2026-08-05 → 2026-08-08, seed 1; fixed impairment, Blocking mode)

This run is kept as historical evidence. It ran on a tree between the 0.4.0
(2026-07-31) and 0.5.0 (2026-08-09) releases and was published with 0.5.0
(CHANGELOG `## [0.5.0]`); its exact source commit was not recorded. It predates the current soak
shape (it held one fixed impairment level, both legs ran the `baseline`
profile, and nothing was corrupted). It also predates Background mode: its
SRT leg ran the Blocking replay. Its delivery and drop-rate figures
describe that configuration, not the 0.7.0 release soak above.

**Overall PASS (fixed impairment, Blocking mode) — zero process exits,
all twelve scheduled outage windows survived with exactly twelve
reconnects and zero unscheduled ones, no memory growth on any of the six
processes.** The run ran to its full
72-hour deadline — the verdict document's measured sampling window
spans 259,152 of the nominal 259,200 seconds, the 48-second difference
being ordinary launch/shutdown process staggering, and the senders
pushed the complete 72 hours of media (7,776,000 AUs at 30 fps) — on a
dedicated cloud VM (AWS EC2
t3.medium, 2 vCPU / 4 GiB, x86_64), both legs concurrently, sustaining
~1.9 Mb/s of GOP-structured video + 10 Hz KLV per leg — about 60.8 GB
of received wire traffic per leg.

| Leg | Sent (video AUs) | Received | KLV records (sent → received) | Drop rate (observed vs. expected) | Reconnects | Verdict |
| --- | --- | --- | --- | --- | --- | --- |
| SRT (managed send + managed recv, 90 s outage every 6 h) | 7,776,000 | 7,773,537 (99.968%) | 2,592,000 → 2,591,182 | 2.0152% vs. 2.00% | 12 (of 12 scheduled windows) | PASS |
| RIST (continuous impairment, no outage) | 7,776,000 | 7,775,971 (99.9996%) | 2,592,000 → 2,591,991 | 2.0009% vs. 2.00% | n/a | PASS |

The reconnect story is the headline: the proxy injected a 90-second
full-drop outage every 6 hours, twelve in all, and
`ManagedRecvTransport`'s rebuild counter finished at exactly 12 — one
successful receive-side transport rebuild per scheduled window, with no
spurious rebuilds anywhere in between. (That check still reports itself
as provisional: the rebuild counter counts factory rebuilds, not outage
windows, and a single window *can* drive more than one rebuild — see
`report.rs`'s module doc — but this run's observed ratio at the
production 90-second outage duration was exactly 1:1.)

The two legs' drop rates validate each other. The RIST leg, which ran
continuous impairment with no outage windows, is the clean statistical
control: its observed 2.0009% sits +0.0009 percentage points above the
configured 2% — about half a standard deviation of a pure binomial at
its 59.9-million-packet volume. The SRT leg's +0.0152pp excess is the
documented outage-window model artifact (reconnect handshake packets
sent while an outage is still active are counted as proxy drops — see
the limitations note in `report.rs`), landing just under that
artifact's predicted 0.02–0.2pp range at 72-hour scale; both legs sit
well inside the 0.10pp verdict tolerance. The SRT leg's 2,463
undelivered AUs (0.032%) are similarly outage-attributable — roughly
seven seconds of video lost per 90-second outage, the in-flight data
from the moment of each cut, with the continuous 2% packet loss fully
absorbed by retransmission in between.

Memory closed out the smoke run's open question (for that run's
fixed-impairment, Blocking configuration). The worst
post-warmup RSS slope across all six processes was 68.8 KiB/hour (the
SRT sender) — and even that is warm-up convergence, not growth: both
senders climb to a ~176 MiB working-set plateau over the first ~7
hours and hold byte-flat after, so every process's slope over the
run's **final 24 hours** was at or below 18 KiB/hour, with the RIST
sender (the smoke run's watch item) at 0.1 KiB/hour. On the strength
of these numbers the RSS gate is no longer provisional:
[`scripts/interop/soak.sh`](/scripts/interop/soak.sh) now defaults
`--rss-slope-threshold-kb-per-hour` to 200 for full-length runs (its
header comment carries the derivation), a threshold this run passes
retroactively with ~3× margin and the pre-fix digest-accumulation
leaks would have tripped.

The supervisor fail-fast machinery (added after an earlier attempt
lost both senders to a fixture bug 14.5 hours in — a synthetic-KLV
latitude walk crossing ST 0601's encode range, not a library bug —
and idled undetected) was armed throughout and never fired: the run's
event log holds exactly seven launch lines. The reproduction recipe is
unchanged — `SRT_RECONNECT_MODE=blocking scripts/interop/soak.sh --seed 1`
with the fixed seed making the impairment engine's decision sequence
deterministic (the mode is now a required setting; this run predates
Background mode and so exercised the blocking path) — and
the full artifact set (30-second-cadence RSS samples, per-leg
send/recv/proxy reports, per-process logs, `soak-results.json`) is
retained offline by the maintainer.

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
delivers. They are part of the 159-cell census above
([run 37875160134](https://github.com/aklofas/ts-transformer/actions/runs/37875160134):
94 PASS, 0 FAIL, 65 EXPECTED-UNSUPPORTED, 0 SKIPPED), and both passed on
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
  The cell holds KLV **content**, not KLV **timing**: no interop oracle
  relates a KLV unit's PTS to the video PTS it was placed against, so
  where the server puts a unit on the video timeline is pinned by the
  aligner's unit tests (`align.rs`) only. A timing oracle for `klv-sync`
  sources under `--remuxed` is after-tag work. The soak's `rtsp-publish`
  leg is MP2T passthrough and never reaches the re-muxer.

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
`tests/coverage/fuzz-targets.toml` lists all 36 targets in the workspace.

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
  cut off there would also fail `worker_exits`.
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

[The analysis of both](/docs/project/2026-10-09-soak-rtsp-publish-srt-audio-attribution.md)
covers the injection, the sampler rows and the fix shape for each. The
RIST leg passed every gating verdict.

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

## Third-party field validation

A third-party integrator has independently validated this codebase against
their own real-world flight capture data on their own embedded-Linux
target. An anonymized summary of that validation will be added to this page
once the consenting party has reviewed the exact text — no numbers,
platform names, or other identifying detail appear here until that review
completes.
