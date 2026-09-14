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
estimated. Re-run either script yourself to reproduce it:

```bash
# Build the interop driver (native deps: vendored libsrt + librist + mbedTLS).
SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1 cargo build --release -p tst-interop

# Full transport + format matrix (157 cells; ~8s/cell locally is what
# produced the census below — see scripts/interop/README.md for the
# full cell/tier/profile vocabulary and per-axis `--cells` filtering).
# Every cell runs realistic access-unit sizes by default; pass
# `--au-sizes compact` to reproduce a pre-2026-09-14 run instead:
bash scripts/interop/run-matrix.sh --outdir /tmp/interop-run --seconds 8

# One-hour soak smoke (the same shape as the 72-hour run below, at
# 1/72nd the duration and the same fixed seed):
bash scripts/interop/soak.sh --outdir /tmp/interop-soak-smoke --hours 1 --seed 1
```

`run-matrix.sh` requires `jq` and `python3` on `PATH`, plus whichever peer
tools (`ffmpeg`, TSDuck's `tsp`, `gst-launch-1.0`, `vlc`, `mpv`) you want
exercised — a missing tool degrades its cells to `SKIPPED`, never a fake
pass or fail. Both scripts are validated on linux-x86_64; linux-aarch64 is
expected to work (no arch-specific code) but hasn't been validated yet —
`run-matrix.sh` additionally depends on per-arch apt/deb availability of
the peer tools above, while `soak.sh` needs no third-party media tools at
all, both legs being `tst-interop` talking to itself through its own
impairment proxy.

## The transport + format interop matrix

`run-matrix.sh` exchanges synthetic MPEG-TS/KLV traffic with real
third-party tools over live network sessions (SRT, RIST, UDP, TCP, HLS,
RTSP), plus runs local decode/analyze probes against the same synthetic
files, across every one of the 12 canonical stream profiles the crate
models (baseline H.264+KLV, H.265, H.266/VVC, AV1 in two PID-classification
shapes, MISP timestamps, synchronous AU-cell KLV, sparse/tight PCR, PTS
rollover, AAC audio, and a two-program stream). Each (transport-or-probe,
peer, direction, profile) combination is one "cell," and every cell gets
one of four verdicts:

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
| **PASS** | The cell's tier requirement held: a byte-for-byte match against the source (`transparent` tier, used for pure-relay tools/paths), or `tst-interop verify`'s profile invariants **and demuxer-independent wire oracles** (`remux` tier, used where the peer legitimately re-packetizes: video/KLV/audio event counts within a documented slack, correct video codec and KLV carriage kind, program count, rollover-aware monotonic PTS, plus per-profile wire-level properties — PCR cadence, AV1 carriage-mode discrimination, per-program media accounting, and an actually-observed PTS wrap, read directly off the bytes by a naive raw-TS parser independent of the demuxer under test — see "What each profile's oracle proves" below), or no error in the peer's own log (`n/a` tier, used for decode-only probes). |
| **EXPECTED-UNSUPPORTED** | A `FAIL` that matches a row in `expectations.toml` — a known, already-investigated gap (see below). |
| **KNOWN-FLAKY** | A `FAIL` (or PASS) matching a row marked flaky rather than reliably-reproducing. |
| **SKIPPED** | The peer tool wasn't installed on the runner. Never a silent pass. |

**Current census: 157 cells — 92 PASS, 0 FAIL, 65 EXPECTED-UNSUPPORTED, 0
SKIPPED — measured at realistic access-unit sizes, and identical to the
census the same matrix produced at compact sizes.** Every one of the 92
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
(`workflow_dispatch`, 2026-09-14, `--seconds 10`, `au_sizes: realistic`,
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
on any PR touching `crates/tst-interop/`, `scripts/interop/`, or the
workflow file itself. The verified run cited above is
[run 34830892357](https://github.com/aklofas/ts-transformer/actions/runs/34830892357)
(the 2026-09-14 `workflow_dispatch` — the first public run at realistic
access-unit sizes — completed `success` with the census-completeness
assert, the 157 / 92 / 0 / 65 / 0 census, and 157 per-cell result records
with zero `FAIL` and no expectation drift: all 65 documented-gap rows
reproduced. The last compact-size run,
[33359181955](https://github.com/aklofas/ts-transformer/actions/runs/33359181955)
(the 2026-08-31 weekly `schedule` run at `73ae1ced`, same census), the
gst-play-enablement run
[32400751057](https://github.com/aklofas/ts-transformer/actions/runs/32400751057)
(`pull_request`, 2026-08-20 — the first 92-PASS census run), the
0.5.1 release-gate run
[32106462041](https://github.com/aklofas/ts-transformer/actions/runs/32106462041)
at `47ceee90` — the last of the 80 / 0 / 65 / 12 predecessor-census
runs — the ancestor run
[32103732958](https://github.com/aklofas/ts-transformer/actions/runs/32103732958)
at `239e2d80`, and the 0.5.0-candidate run
[31335394509](https://github.com/aklofas/ts-transformer/actions/runs/31335394509)
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

Since 2026-09-13 (PR #213) `tst-interop verify`/`recv` check every profile
against demuxer-independent wire facts read by a deliberately naive
raw-TS parser (`crates/tst-interop/src/rawts.rs`) in addition to the
demuxed tallies, and every oracle has a mutation test that removes the
property and asserts the named failure (`crates/tst-interop/tests/mutations.rs`).

| Profile | Independently verified on the wire | Oracle names |
| --- | --- | --- |
| all | PMT stream_type per PID, KLVA/AV01 registration, PCR median interval ≥ configured, every interval ≤ configured + one frame period (the muxer's PCR-only catch-up packets are a legitimate minority), no unexpected 33-bit PTS wrap, `NonConformant` fatal, `Discontinuity` fatal offline / counted live | `pmt_stream_type_*`, `pmt_descriptor_*`, `pcr_interval`, `pts_wrap_unexpected`, `nonconformant_event`, `discontinuity_event` |
| two-program | video + KLV counts per program_number, packets on both programs' media PIDs | `program_{1,2}_{video,klv}_floor`, `program_{1,2}_wire_media` |
| audio | ADTS syncword + 48 kHz index in the raw PES payload, `sample_rate/1024` frames/s ±10 %, 1920-tick PTS step ±5 % | `audio_codec_adts`, `audio_cadence`, `audio_pts_step` |
| av1-klv-a / av1-klv-b | PES stream_id 0xE0 + raw OBU header vs 0xBD + `00 00 01` `ts_open_bitstream_unit` framing (the PMT is identical in both modes) | `av1_carriage_wire` |
| pcr-tight / pcr-sparse | PCR median interval ≥ 1 ms / ≥ 100 ms; every interval ≤ configured + one frame period in Strict cells (the muxer's PCR-only catch-up packets are a legitimate minority) | `pcr_interval` |
| pts-rollover | at least one raw PES PTS wrap observed | `pts_wrap_observed` |
| klv-sync | stream_type 0x15 + metadata (0x26) and metadata_STD (0x27) descriptors | `pmt_stream_type_*`, `pmt_descriptor_*` |

## Soak evidence

`soak.sh` runs two concurrent, hours-long legs of `tst-interop` pushing
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
RSS-growth slopes per process.

The impairment itself changed shape on 2026-09-14 (see "The current soak
shape" below). Every published run on this page so far used the previous
**fixed-impairment** shape — one level held for the whole run: 2 % loss,
20 ms jitter over a 30 ms base link delay, 1 % reorder held 200 ms, seeded
deterministically, both legs on the `baseline` profile with no corruption
injected. Those numbers are what the 2026-08-05 run's tables below mean.

`tst-interop` also carries a sender-side corruption tap (`send --corrupt`)
that deliberately damages the muxer's own output on its way to the wire —
flipped bytes, destroyed headers, truncated and duplicated and dropped
packets, damaged PAT/PMT sections — and records every injection to a JSONL
log the receiving side reads back. A receiver judged against that log
answers three questions that a clean run cannot ask: did every error event
it reported have a cause (`corruption_attributed`), did it notice every
injection a conformant receiver is required to notice
(`corruption_detected`), and did the stream produce media again afterwards
(`corruption_recovered`). The tap is now **on by default on both soak legs**;
the next 72-hour run will carry these verdicts end to end and its numbers
will be published here alongside the loss and RSS figures below. None of the
157 interop-matrix cells inject corruption — the census above is a pristine
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

The soak rail was tightened on 2026-09-13 (PR #211): `report soak` now
judges a run against a configured duration and RSS cadence
(`soak-config.json`), requires ≥90 % of the cadence-implied post-warmup
samples per process with the gap between consecutive samples strictly
under three cadences, and fails on any
nonzero worker exit (`exits.json`) — the published 72-hour run above
predates that rail; the next long run will be judged under it. A 1-hour
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

Everything in this subsection describes how the NEXT long run will be
configured and judged. No 72-hour run has yet been executed under it; the
numbers published further down all come from the fixed-impairment shape.

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
  long run also covers a codec/carriage/cadence shape the 157-cell matrix
  only sees for five seconds at a time.
- **Sender-side corruption on both legs**, each with its own seed offset and
  its own injection log, read back by that leg's own receiver.
- **Rich ST 0601 KLV on both legs** — a record of up to 36 tags (mean about
  27) carrying a nested ST 0102 security set on a seeded presence schedule,
  rather than the matrix's 4-tag minimal record.

The verdict document gains four families on top of the existing ones. Two
are declaration checks — `profile_declared_<leg>` and
`schedule_declared_<leg>` — which fail a run whose proxy or receiver did not
actually run what the config declared, so a drift between the recipe and the
run cannot pass unnoticed. The third is the drop-rate verdict, now
integrated over the echoed phase table rather than compared against one flat
rate, and failing loud on a stats file whose phase counters disagree with its
own schedule echo. The fourth is the corruption family described above.

Two attribution rules keep those verdicts honest on a link that really does
lose packets. A live capture is judged in the lossy tier, where an
unexplained continuity gap is excused as transport loss rather than charged
to the corruption tap (non-conformances and resyncs are never excused, and an
injection's own gap never excuses that injection); and the rich-KLV oracles
skip a record an injection demonstrably damaged, counting it separately so
every failure string reports how many records went unexamined. Offline
verification remains in the strict tier and sees neither excusal.

**Smoke evidence (2026-09-14, local, seed 3).** Two ten-minute runs over a
four-phase schedule each passed all 31 verdicts with `overall_pass: true`,
drawing `klv-sync` on the SRT leg and `audio` on the RIST leg. Attribution
was complete on both legs and in both runs — 267 injections, all resolved,
against 211 events, all attributed, on SRT; 287 injections, all resolved,
against 249 attributed events on RIST — with zero undetected and zero
unrecovered injections throughout. One of the two runs additionally excused
2 RIST events as transport loss, which is the lossy-tier rule firing on live
timing rather than on anything seeded, so it is expected to vary run to run.
Of about 6,100 rich KLV records per leg, 236 (SRT) and 223 (RIST) were
skipped as injection-damaged in both runs, identically, and the rest decoded
clean with no census mismatches and every expected nested security set valid.
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

### The 72-hour run (2026-08-05 → 2026-08-08, seed 1)

**Overall PASS — zero process exits, all twelve scheduled outage windows
survived with exactly twelve reconnects and zero unscheduled ones, no
memory growth on any of the six processes.** The run ran to its full
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

Memory closed out the smoke run's open question. The worst
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
unchanged — `scripts/interop/soak.sh --seed 1` with the fixed seed
making the impairment engine's decision sequence deterministic — and
the full artifact set (30-second-cadence RSS samples, per-leg
send/recv/proxy reports, per-process logs, `soak-results.json`) is
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
