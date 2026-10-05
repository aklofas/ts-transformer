# Benchmarks + ceilings

This page publishes the measured sweep of the stress run of 2026-10-03
(tree `3a3f964f`, one AWS `c7i.2xlarge`) and the steady-state figures of
the 72-hour 0.7.0 release-candidate soak (tree `e86dd9ea`). The stress
run's 24-hour hold did not finish, so no 24-hour hold verdict is published
yet. The figures are from one host each; use measured results from your
target system when choosing a deployment size.

This page answers a deployment-planning question the interop matrix and the
soak evidence don't: how big a system can I build on one machine before
something gives out? The stress harness pushes each transport past the
point where it stays healthy and records where that point was, so the
numbers below are sizing data, not a correctness check.

## How to read a ceiling

Each axis is a ladder of steps, run in ascending order, and every step is
judged against the same verdict set a soak run uses. The **ceiling** is the
last step that passed before the first step that failed. If every step on a
ladder passes, the page says so explicitly as "top of the ladder" — that is
the highest point this run happened to test, not a measured limit, and it
is never reported as a ceiling.

## What is measured

Two sweeps and a hold, all four transports (SRT, RIST, UDP, TCP):

- **Stream count**, at a fixed 1× bitrate per stream: 1, 2, 4, 8, … up to
  1024 concurrent streams on one transport, looking for the point where CPU,
  memory, or file/thread accounting stops holding steady. A step whose
  predicted memory (streams × the previous passing step's per-stream RSS ×
  1.25) would exceed 70% of the host's memory is not launched: the axis
  ends there with a `memory_budget` verdict, so the host's memory limit is
  reported as a ceiling instead of the kernel killing the run.
- **Per-stream bitrate**, at a fixed single stream: `--au-scale` 1, 2, 4,
  … up to 64, which scales a realistic access-unit size up to roughly
  1.7–110 Mb/s. The top of this ladder is bounded by a 4 MiB per-PID PES
  cap on keyframes, not by anything this harness goes looking for.

Both sweeps run on a clean loopback link — the SRT and RIST legs go through
their usual proxies with every impairment knob at zero, and the UDP/TCP
legs connect directly — so a ceiling here is a measure of the host, not of
link tolerance.

The **hold** runs all four transports at once, each sized to ⌊0.7 × its own
ceiling⌋ streams — scaled down further, and declared, when the sweep's
per-stream CPU or memory cost predicts more than 70% of the host — for 24
hours. The SRT and RIST legs run the soak's seeded impairment schedule
(UDP keeps the clean link, TCP stays direct), and the SRT legs take two
extra disruptions: a 30-second full-drop outage on every SRT proxy every
15 minutes, and a receiver restart on one SRT stream every 2 hours. A
transport with no streams ceiling is excluded from the hold and listed
under the run's limitations. It answers a different question than the
ceiling does — not "how far can this go" but "does a system sized at 70%
of its ceiling actually survive a full day of real-world disruption."

## Verdicts

| Verdict | Rule | Threshold |
|---|---|---|
| `worker_exits` | No worker process exits before the run's own end-of-run teardown. | zero unscheduled exits |
| `recv_invariants` | Every receiver's own internal invariants hold throughout. | no violation |
| `delivery_complete` | Received video access units against sent. | ≥ 0.7×, and > 1.2× fails as an accounting mismatch rather than a pass |
| `cpu_headroom` | Sum of worker CPU-seconds over wall-seconds over vCPU count. | ≤ 0.80 |
| `rss_slope_<leg>_<process>` | Memory growth rate per process, judged after warm-up and scaled for a window shorter than an hour. A process declared `rss_slope_ungated` (default: the RIST sender, whose librist buffers settle over about an hour) is recorded, not gated, in sweep steps — the hold gates it — and every such case is listed under the run's limitations. | ≤ a required KB/hour threshold |
| `fd_count_flat_*` | File-descriptor count range per process after a 60-second warm-up. | max − min ≤ 2 |
| `thread_count_flat_*` | Thread count range per process after the same warm-up. | max − min ≤ 1 |
| `sample_coverage` | Fraction of expected sampler ticks actually recorded. | ≥ 90% |
| `reconnect_count` *(hold only)* | Each SRT receiver rebuilds its transport at least once per outage window it lived through. | ≥ (outage windows − 1); one less than the window count because the final window can coincide with teardown |
| `peer_restart_recovery` *(hold only)* | After each scheduled receiver restart, the sender reconnects. | within 120 seconds; a restart in the run's final 120 seconds is unjudgeable |
| `queue_depth_p99` *(hold only)* | The managed sender's gap-length distribution. | p99 ≤ 0.9× its configured capacity |
| `hold_sizing_declared` *(hold only)* | The hold's configured stream counts actually match the sizing rule, and every swept transport is either held or excluded because the sweep found it no streams ceiling (an exclusion is listed under the run's limitations). | each held transport ≤ max(1, ⌊0.7 × ceiling⌋); a CPU or memory scale-down is declared in the hold config |

## Reference machine and reproduction

The reference machine is an AWS `c7i.2xlarge` (8 vCPU, 16 GiB) on
Ubuntu noble. Launch recipe:

```bash
SRT_RECONNECT_MODE=background RSS_SLOPE_THRESHOLD_KB_PER_HOUR=<value> \
  nohup bash scripts/interop/stress.sh --outdir <dir> --seed <N> & disown
# RSS_SLOPE_UNGATED=rist/send is the default; set it empty to gate every process.
```

and once it finishes, render this page's measured block from the resulting
archive:

```bash
scripts/gen/benchmarks-page.sh <archive-dir>
```

The archive directory holds `stress-results.json` (the sweep steps and
their verdicts), `provenance.json` (host, toolchain, and source commit),
and one subdirectory per sweep step with that step's raw logs and samples.

## Measured results

The block below is generated from the 2026-10-03 run's sweep steps. Its
hold section reads "No hold in this run" because the hold ended early and
wrote no results file; what the hold did measure is under "Measured: stress
run of 2026-10-03" below.

<!-- bench:begin -->
### Reference machine

- Kernel: Linux 7.0.0-1013-aws x86_64
- vCPUs: 8
- Memory: 15.3 GiB
- Toolchain: rustc 1.85.1 (4eb161250 2025-03-15)
- Source: `3a3f964f5c6b` (v0.6.0-669-g3a3f964f)
- Recorded: 2026-10-03T07:14:04Z

### Stream scaling

#### SRT

| N | CPU/stream (cores) | RSS/stream p99 (MiB) | threads max | fds max | wire Mb/s | pass |
|---|---|---|---|---|---|---|
| 1 | 0.033 | 26.1 | 6 | 5 | 1.9 | pass |
| 2 | 0.031 | 26.5 | 6 | 5 | 3.8 | pass |
| 4 | 0.028 | 26.4 | 6 | 5 | 7.5 | pass |
| 8 | 0.020 | 26.5 | 6 | 5 | 15.1 | pass |
| 16 | 0.016 | 26.4 | 6 | 5 | 30.1 | pass |
| 32 | 0.016 | 26.4 | 6 | 5 | 60.2 | pass |
| 64 | 0.016 | 26.4 | 6 | 5 | 120.4 | pass |
| 128 | 0.017 | 26.5 | 6 | 5 | 240.8 | pass |

Per-stream figures are averages over the step's streams.

Ceiling: top of ladder (128), not a measured limit

#### RIST

| N | CPU/stream (cores) | RSS/stream p99 (MiB) | threads max | fds max | wire Mb/s | pass |
|---|---|---|---|---|---|---|
| 1 | 0.024 | 26.5 | 3 | 5 | 1.9 | pass |
| 2 | 0.022 | 26.4 | 3 | 5 | 3.8 | pass |
| 4 | 0.020 | 26.4 | 3 | 5 | 7.5 | pass |
| 8 | 0.018 | 26.5 | 3 | 5 | 15.1 | pass |
| 16 | 0.014 | 26.4 | 3 | 5 | 30.2 | pass |
| 32 | 0.012 | 26.4 | 3 | 5 | 60.3 | pass |
| 64 | 0.013 | 26.3 | 3 | 5 | 120.6 | pass |
| 128 | 0.014 | 26.3 | 3 | 5 | 241.2 | pass |

Per-stream figures are averages over the step's streams.

Ceiling: top of ladder (128), not a measured limit

#### UDP

| N | CPU/stream (cores) | RSS/stream p99 (MiB) | threads max | fds max | wire Mb/s | pass |
|---|---|---|---|---|---|---|
| 1 | 0.008 | 22.5 | 1 | 5 | 1.9 | pass |
| 2 | 0.007 | 22.1 | 1 | 5 | 3.8 | pass |
| 4 | 0.007 | 22.2 | 1 | 5 | 7.5 | pass |
| 8 | 0.007 | 21.9 | 1 | 5 | 15.1 | pass |
| 16 | 0.007 | 22.0 | 1 | 5 | 30.2 | pass |
| 32 | 0.006 | 22.1 | 1 | 5 | 60.3 | pass |
| 64 | 0.006 | 22.1 | 1 | 5 | 120.6 | pass |
| 128 | 0.005 | 22.1 | 1 | 5 | 241.2 | pass |

Per-stream figures are averages over the step's streams.

Ceiling: top of ladder (128), not a measured limit

#### TCP

| N | CPU/stream (cores) | RSS/stream p99 (MiB) | threads max | fds max | wire Mb/s | pass |
|---|---|---|---|---|---|---|
| 1 | 0.005 | 16.1 | 1 | 5 | 1.9 | pass |
| 2 | 0.005 | 16.1 | 1 | 5 | 3.8 | pass |
| 4 | 0.005 | 16.0 | 1 | 5 | 7.5 | pass |
| 8 | 0.005 | 16.1 | 1 | 5 | 15.1 | pass |
| 16 | 0.005 | 16.1 | 1 | 5 | 30.2 | pass |
| 32 | 0.004 | 16.0 | 1 | 5 | 60.3 | pass |
| 64 | 0.004 | 16.1 | 1 | 5 | 120.6 | pass |
| 128 | 0.004 | 16.1 | 1 | 5 | 241.2 | pass |

Per-stream figures are averages over the step's streams.

Ceiling: top of ladder (128), not a measured limit

### Single-stream throughput

#### SRT

| scale | declared Mb/s | observed Mb/s | CPU (cores) | pass |
|---|---|---|---|---|
| 1 | 1.7 | 1.9 | 0.033 | pass |
| 2 | 3.4 | 3.7 | 0.036 | pass |
| 4 | 6.8 | 7.3 | 0.042 | pass |
| 8 | 13.6 | 14.4 | 0.054 | pass |
| 16 | 27.2 | 28.8 | 0.078 | pass |
| 32 | 54.4 | 57.5 | 0.128 | pass |
| 64 | 108.8 | 115.0 | 0.231 | fail |

Ceiling: 32 scale — ended by rss_slope_srt-0_send

#### RIST

| scale | declared Mb/s | observed Mb/s | CPU (cores) | pass |
|---|---|---|---|---|
| 1 | 1.7 | 1.9 | 0.024 | pass |
| 2 | 3.4 | 3.7 | 0.027 | pass |
| 4 | 6.8 | 7.3 | 0.032 | pass |
| 8 | 13.6 | 14.5 | 0.044 | pass |
| 16 | 27.2 | 28.8 | 0.068 | pass |
| 32 | 54.4 | 57.6 | 0.124 | fail |

Ceiling: 16 scale — ended by rss_slope_rist-0_recv

#### UDP

| scale | declared Mb/s | observed Mb/s | CPU (cores) | pass |
|---|---|---|---|---|
| 1 | 1.7 | 1.9 | 0.009 | pass |
| 2 | 3.4 | 3.7 | 0.009 | pass |
| 4 | 6.8 | 7.3 | 0.014 | pass |
| 8 | 13.6 | 14.5 | 0.023 | pass |
| 16 | 27.2 | 28.8 | 0.036 | pass |
| 32 | 54.4 | 57.6 | 0.066 | pass |
| 64 | 108.8 | 115.1 | 0.127 | fail |

Ceiling: 32 scale — ended by rss_slope_udp-0_send

#### TCP

| scale | declared Mb/s | observed Mb/s | CPU (cores) | pass |
|---|---|---|---|---|
| 1 | 1.7 | 1.9 | 0.006 | pass |
| 2 | 3.4 | 3.7 | 0.007 | pass |
| 4 | 6.8 | 7.3 | 0.009 | pass |
| 8 | 13.6 | 14.5 | 0.015 | pass |
| 16 | 27.2 | 28.8 | 0.025 | pass |
| 32 | 54.4 | 57.6 | 0.044 | fail |

Ceiling: 16 scale — ended by rss_slope_tcp-0_send

### The hold

No hold in this run.

### Limitations

- rist/bitrate: rss_slope_rist-0_send over its allowance at step 1, step 2, step 4, step 8, step 16, step 32 — recorded, not gated (declared rss_slope_ungated; the hold gates it)
- rist/streams: never failed — the ceiling is the top of the ladder, not a measured limit
- rist/streams: rss_slope over its allowance for 128 processes matching `rist-*_send` (per-process list in stress-results.json) — recorded, not gated (declared rss_slope_ungated; the hold gates it)
- srt/streams: never failed — the ceiling is the top of the ladder, not a measured limit
- tcp/streams: never failed — the ceiling is the top of the ladder, not a measured limit
- udp/streams: never failed — the ceiling is the top of the ladder, not a measured limit
<!-- bench:end -->

## Measured: stress run of 2026-10-03

The run used tree `3a3f964f` on the reference machine above (8 vCPU,
16 077 024 kB), with seed 11, all four transports, an RSS-slope threshold
of 1024 KB/hour, and the RIST sender recorded, not gated, in sweep steps.
The sweep ran 58 steps from 07:14Z to 18:21Z: 54 passed, and the 4 that
failed are the rungs that end the four bitrate axes.

**Stream count.** All four transports passed every rung up to 128
concurrent streams, the top of this run's ladder. That is the highest load
tested, not a measured limit. At 128 streams each transport carried about
241 Mb/s on the wire. Measured as a fraction of the host's 8 vCPUs, SRT
used 0.27, RIST 0.23, UDP 0.09 and TCP 0.06. Average RSS per stream (sender,
proxy and receiver together) was flat across the ladder: about 26 MiB for
SRT and RIST, 22 MiB for UDP, 16 MiB for TCP.

**Per-stream bitrate.** The last passing `--au-scale` rung on one stream:

| Transport | Ceiling (scale) | Declared rate at the ceiling | Ended at | Ended by |
|---|---|---|---|---|
| SRT | 32 | 54.4 Mb/s | 64 | `rss_slope_srt-0_send`, 10 150 KB/h against 6 827 allowed |
| RIST | 16 | 27.2 Mb/s | 32 | `rss_slope_rist-0_recv`, 11 334 KB/h against 6 827 allowed |
| UDP | 32 | 54.4 Mb/s | 64 | `rss_slope_udp-0_send`, 10 227 KB/h against 6 827 allowed |
| TCP | 16 | 27.2 Mb/s | 32 | `rss_slope_tcp-0_send`, 8 855 KB/h against 6 827 allowed |

Each axis ended on its memory-slope verdict alone; delivery, CPU,
descriptor and thread verdicts passed at the failing rung. The allowance is
the 1024 KB/hour threshold scaled to the step's 540-second judged window,
so a one-time rise while buffers grow to a higher rate fails it. These
ceilings therefore mark where a 10-minute step stops being able to tell
warm-up from growth, not where a transport stops delivering.

**The hold.** The sweep found no streams ceiling, so the hold was sized
from the top rung: ⌊0.7 × 128⌋ = 89 per transport by CPU, then scaled by
0.54 to fit the memory budget, giving 48 streams on each transport (192
streams). It ran 58 081 of its 86 400 seconds (16 h 08 m) under the soak's
impairment schedule, a 30-second outage on every SRT proxy every 15
minutes and the 2-hourly SRT receiver restart. It ended when the receiver started at the ninth restart
exited before reading data. The cause is in the harness: a restarted
receiver re-reads the whole corruption log before its first read, and by
the ninth restart that took longer than its 15-second no-data deadline. The
SRT sender reconnected after that restart as it had after the eight
before. The run therefore carries no 24-hour hold verdict.

What the 16 hours do show: the SRT sender whose receiver was restarted
stayed flat across restarts 3 to 9, with RSS 66 580 kB from the fourth
hour to the end and a slope of 1.65 KB/hour from the first hour on. The
SRT sender with no restarts plateaued at 65 568 kB, with a slope of 0.92
KB/hour.

## Measured: 72-hour 0.7.0 release-candidate soak

The soak ran tree `e86dd9ea` in `ReconnectMode::Background`, seed 11, for
259 137 of 259 200 seconds, on a 2-vCPU, 3.9 GB host (smaller than the
stress reference machine). Its harness verdict is `overall_pass=false`
(41 verdicts: 36 gating PASS, 3 gating FAIL, 2 provisional PASS). The SRT
leg passed every gating verdict. The RIST leg failed corruption attribution
with 8 unexplained events: six match a reproduced harness expectation-table
defect, and the other two are attributed, as a high-confidence inference,
to a reproduced harness anchor-stranding defect. All three gating FAILs
(`worker_exits`, `recv_invariants_rist`, `corruption_attributed_rist`)
stem from those 8 RIST events.
[Validation evidence](/docs/project/validation-evidence.md) has the full
account.

Steady state, from the 30-second RSS series:

| Process | Start | Steady state | Slope after the 30-minute warm-up |
|---|---|---|---|
| SRT sender | 171.3 MB | 172.1–173.2 MB from 6 h to the end | 9.2 KiB/h |
| RIST sender | 76.6 MB | 181.3–181.4 MB from 8.3 h to the end (above 179 MB from 4.4 h) | 22.8 KiB/h |
| SRT receiver | 10.1 MB | 11.4 MB at the end | 5.0 KiB/h |
| RIST receiver | 7.5 MB | 10.0 MB at the end | 1.8 KiB/h |
| SRT proxy | 6.4 MB | 7.0 MB at the end | 5.2 KiB/h |
| RIST proxy | 5.9 MB | 6.5 MB at the end | 0.4 KiB/h |

Every slope is under the 200 KiB/hour gate; the largest is 22.8. The SRT
receiver rebuilt its transport 12 times for the 12 scheduled outage
windows. The SRT receiver judged 2 580 965 rich ST 0601 KLV records and the
RIST receiver 2 591 588, with 0 decode errors on either leg.

## Not measured yet

These are out of scope for the current harness:

- FFI overhead for the Python, JVM, and C bindings — the sweeps above
  exercise the Rust core directly.
- ARM64 — the reference machine and every run so far are x86_64.
- Latency percentiles — the harness judges throughput, memory, and
  resource accounting, not end-to-end latency.
- In-process multi-stream topology — every stream in a sweep step runs as
  its own process; a single process carrying many streams is untested.
- Single-stream rates above the roughly 110 Mb/s top of the bitrate
  ladder, which is bounded by the 4 MiB per-PID PES cap on keyframes
  rather than by anything this harness measures.
- Hold variants beyond the one described above: aggressive impairment,
  connection churn, and a deliberately slow receiver.

## Evidence rule

A stress run is not a soak PASS, and a soak is not a ceiling. The two
measure different things — a soak proves endurance at a size someone
already chose; a stress run finds the size. Neither result stands in for
the other on this page or anywhere else in this repo's evidence.
