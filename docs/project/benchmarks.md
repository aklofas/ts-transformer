# Benchmarks + ceilings

This page publishes the sweep and the completed 24-hour hold of the
stress run of 2026-11-07 (tree `d0a8e34ef`, the 0.9.1 library code), run on
two hosts: an AWS `c7i.2xlarge` (x86_64) and a `c7g.2xlarge` (aarch64).
Below it are the sweep of the run of 2026-10-23 (tree `12c1ff1d`, one
`c7i.2xlarge`; its 24-hour hold did not run — a harness defect ended it at
launch), the sweep and the completed 24-hour hold of the run of 2026-10-06
(tree `9bb728e1`), the sweep of the earlier 2026-10-03 run (tree
`3a3f964f`, whose hold ended early), and the steady-state figures of the
72-hour 0.7.0 release-candidate soak (tree `e86dd9ea`). Each figure is from
one host; use measured results from your target system when choosing a
deployment size.

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
| `proxy_stats_final` | Every stream with an impairment proxy (every transport but TCP) carries the stats file the proxy wrote as it exited. A missing file is a proxy that died before its first write. A periodic snapshot stops short of the step's tail, so its totals are neither recorded nor judged. | present for every proxied stream |
| `recv_invariants` | Every receiver's own internal invariants hold throughout. | no violation |
| `delivery_complete` | Received video access units against sent. | ≥ 0.7×, and > 1.2× fails as an accounting mismatch rather than a pass |
| `cpu_headroom` | Sum of worker CPU-seconds over wall-seconds over vCPU count. | ≤ 0.80 |
| `rss_slope_<leg>_<process>` | Memory growth rate per process, judged after warm-up and scaled for a window shorter than an hour. A process declared `rss_slope_ungated` (default: the RIST sender, whose librist buffers settle over about an hour) is recorded, not gated, in sweep steps — the hold gates it — and every such case is listed under the run's limitations. | ≤ a required KB/hour threshold |
| `fd_count_flat_*` | File-descriptor count range per process after a 60-second warm-up. | max − min ≤ 2 |
| `thread_count_flat_*` | Thread count range per process after the same warm-up. | max − min ≤ 1 |
| `sample_coverage` | Fraction of expected sampler ticks actually recorded. | ≥ 90% |
| `proc_sample_coverage` | The same for the CPU, file-descriptor and thread samples; every declared role of every stream is expected, and a sample counts only with all four of its fields. | ≥ 90% |
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

The arm64 reference machine is an AWS `c7g.2xlarge` (8 vCPU Graviton, 16 GiB)
running the same recipe; its first run is the 2026-11-07 one. Given its
archive, the same command adds a ceilings-by-architecture table at the top of
the block:

```bash
scripts/gen/benchmarks-page.sh <archive-dir> --arm <arm-archive-dir>
```

The archive directory holds `stress-results.json` (the sweep steps and
their verdicts), `provenance.json` (host, toolchain, and source commit),
and one subdirectory per sweep step with that step's raw logs and samples.

## Measured results

The block below is generated from the 2026-11-07 run: the ceilings of
both hosts, then the x86_64 host's sweep steps and hold. What the run
found, which of its ceilings are caveated, and the aarch64 host's hold, are
under "Measured: stress run of 2026-11-07" below the block. Four of the
block's streams ceilings (UDP on x86_64; SRT, RIST and UDP on aarch64) read
128 because the 256 rung came out unjudgeable on a harness launch race; a
re-run of those rungs after the harness fix passed, and that section
publishes 256 for them. The older runs keep their own sections below that.

<!-- bench:begin -->
### Ceilings by architecture

| Transport | Axis | x86_64 ceiling | aarch64 ceiling |
|---|---|---|---|
| srt | streams | 256 (memory_budget) | 128 (step_unjudgeable) |
| srt | bitrate | 32 (transport_loss_excused, rss_slope_srt-0_send) | 8 (rss_slope_srt-0_send) |
| rist | streams | 256 (memory_budget) | 128 (step_unjudgeable) |
| rist | bitrate | 32 (transport_loss_excused) | 64 (top of ladder) |
| udp | streams | 128 (step_unjudgeable) | 128 (step_unjudgeable) |
| udp | bitrate | 32 (rss_slope_udp-0_send) | 32 (rss_slope_udp-0_send) |
| tcp | streams | 256 (memory_budget) | 512 (memory_budget) |
| tcp | bitrate | 16 (rss_slope_tcp-0_send) | 16 (rss_slope_tcp-0_send) |

### Reference machine

- Architecture: x86_64
- Kernel: Linux 7.0.0-1014-aws x86_64
- vCPUs: 8
- Memory: 15.3 GiB
- Toolchain: rustc 1.93.1 (01f6ddf75 2026-02-11)
- Source: `d0a8e34ef942` (v0.9.0-946-gd0a8e34ef)
- Recorded: 2026-11-07T17:51:59Z

### Reference machine (aarch64)

- Architecture: aarch64
- Kernel: Linux 7.0.0-1014-aws aarch64
- vCPUs: 8
- Memory: 15.3 GiB
- Toolchain: rustc 1.93.1 (01f6ddf75 2026-02-11)
- Source: `d0a8e34ef942` (v0.9.0-946-gd0a8e34ef)
- Recorded: 2026-11-07T17:52:00Z

### Stream scaling

#### SRT

| N | CPU/stream (cores) | RSS/stream p99 (MiB) | threads max | fds max | wire Mb/s | pass |
|---|---|---|---|---|---|---|
| 1 | 0.032 | 29.4 | 6 | 5 | 1.9 | pass |
| 2 | 0.031 | 29.8 | 6 | 5 | 3.8 | pass |
| 4 | 0.029 | 29.6 | 6 | 5 | 7.5 | pass |
| 8 | 0.020 | 29.6 | 6 | 5 | 15.1 | pass |
| 16 | 0.015 | 29.7 | 6 | 5 | 30.1 | pass |
| 32 | 0.015 | 29.7 | 6 | 5 | 60.2 | pass |
| 64 | 0.016 | 29.7 | 6 | 5 | 120.4 | pass |
| 128 | 0.017 | 29.7 | 6 | 5 | 240.8 | pass |
| 256 | 0.018 | 29.7 | 6 | 5 | 481.6 | pass |
| 512 | 0.000 | 0.0 | 0 | 0 | 0.0 | fail |

Per-stream figures are averages over the step's streams.

Ceiling: 256 streams — ended by memory_budget

#### RIST

| N | CPU/stream (cores) | RSS/stream p99 (MiB) | threads max | fds max | wire Mb/s | pass |
|---|---|---|---|---|---|---|
| 1 | 0.024 | 28.8 | 3 | 5 | 1.9 | pass |
| 2 | 0.021 | 29.0 | 3 | 5 | 3.8 | pass |
| 4 | 0.018 | 28.9 | 3 | 5 | 7.5 | pass |
| 8 | 0.017 | 28.8 | 3 | 5 | 15.1 | pass |
| 16 | 0.014 | 28.9 | 3 | 5 | 30.2 | pass |
| 32 | 0.011 | 28.9 | 3 | 5 | 60.3 | pass |
| 64 | 0.012 | 29.0 | 3 | 5 | 120.6 | pass |
| 128 | 0.013 | 28.9 | 3 | 5 | 241.2 | pass |
| 256 | 0.015 | 28.9 | 3 | 5 | 482.4 | pass |
| 512 | 0.000 | 0.0 | 0 | 0 | 0.0 | fail |

Per-stream figures are averages over the step's streams.

Ceiling: 256 streams — ended by memory_budget

#### UDP

| N | CPU/stream (cores) | RSS/stream p99 (MiB) | threads max | fds max | wire Mb/s | pass |
|---|---|---|---|---|---|---|
| 1 | 0.007 | 24.4 | 1 | 5 | 1.9 | pass |
| 2 | 0.006 | 24.2 | 1 | 5 | 3.8 | pass |
| 4 | 0.006 | 24.2 | 1 | 5 | 7.5 | pass |
| 8 | 0.006 | 24.3 | 1 | 5 | 15.1 | pass |
| 16 | 0.006 | 24.5 | 1 | 5 | 30.2 | pass |
| 32 | 0.007 | 24.5 | 1 | 5 | 60.3 | pass |
| 64 | 0.007 | 24.5 | 1 | 5 | 120.6 | pass |
| 128 | 0.006 | 24.4 | 1 | 5 | 241.2 | pass |
| 256 | 0.000 | 0.0 | 0 | 0 | 0.0 | fail |

Per-stream figures are averages over the step's streams.

Ceiling: 128 streams — ended by step_unjudgeable

#### TCP

| N | CPU/stream (cores) | RSS/stream p99 (MiB) | threads max | fds max | wire Mb/s | pass |
|---|---|---|---|---|---|---|
| 1 | 0.009 | 18.1 | 2 | 5 | 1.9 | pass |
| 2 | 0.007 | 17.9 | 2 | 5 | 3.8 | pass |
| 4 | 0.007 | 18.1 | 2 | 5 | 7.5 | pass |
| 8 | 0.006 | 18.0 | 2 | 5 | 15.1 | pass |
| 16 | 0.006 | 18.0 | 2 | 5 | 30.2 | pass |
| 32 | 0.005 | 18.1 | 2 | 5 | 60.3 | pass |
| 64 | 0.005 | 18.1 | 2 | 5 | 120.6 | pass |
| 128 | 0.005 | 18.1 | 2 | 5 | 241.2 | pass |
| 256 | 0.004 | 18.1 | 2 | 5 | 482.4 | pass |
| 512 | 0.000 | 0.0 | 0 | 0 | 0.0 | fail |

Per-stream figures are averages over the step's streams.

Ceiling: 256 streams — ended by memory_budget

### Single-stream throughput

#### SRT

| scale | declared Mb/s | observed Mb/s | CPU (cores) | pass |
|---|---|---|---|---|
| 1 | 1.7 | 1.9 | 0.032 | pass |
| 2 | 3.4 | 3.7 | 0.035 | pass |
| 4 | 6.8 | 7.3 | 0.041 | pass |
| 8 | 13.6 | 14.4 | 0.053 | pass |
| 16 | 27.2 | 28.8 | 0.078 | pass |
| 32 | 54.4 | 57.5 | 0.129 | pass |
| 64 | 108.8 | 115.0 | 0.236 | fail |

Ceiling: 32 scale — ended by transport_loss_excused, rss_slope_srt-0_send

#### RIST

| scale | declared Mb/s | observed Mb/s | CPU (cores) | pass |
|---|---|---|---|---|
| 1 | 1.7 | 1.9 | 0.023 | pass |
| 2 | 3.4 | 3.7 | 0.026 | pass |
| 4 | 6.8 | 7.3 | 0.032 | pass |
| 8 | 13.6 | 14.5 | 0.043 | pass |
| 16 | 27.2 | 28.8 | 0.067 | pass |
| 32 | 54.4 | 57.6 | 0.123 | pass |
| 64 | 108.8 | 113.8 | 0.251 | fail |

Ceiling: 32 scale — ended by transport_loss_excused

#### UDP

| scale | declared Mb/s | observed Mb/s | CPU (cores) | pass |
|---|---|---|---|---|
| 1 | 1.7 | 1.9 | 0.011 | pass |
| 2 | 3.4 | 3.7 | 0.013 | pass |
| 4 | 6.8 | 7.3 | 0.018 | pass |
| 8 | 13.6 | 14.5 | 0.026 | pass |
| 16 | 27.2 | 28.8 | 0.043 | pass |
| 32 | 54.4 | 57.6 | 0.076 | pass |
| 64 | 108.8 | 115.1 | 0.146 | fail |

Ceiling: 32 scale — ended by rss_slope_udp-0_send

#### TCP

| scale | declared Mb/s | observed Mb/s | CPU (cores) | pass |
|---|---|---|---|---|
| 1 | 1.7 | 1.9 | 0.009 | pass |
| 2 | 3.4 | 3.7 | 0.010 | pass |
| 4 | 6.8 | 7.3 | 0.013 | pass |
| 8 | 13.6 | 14.5 | 0.019 | pass |
| 16 | 27.2 | 28.8 | 0.029 | pass |
| 32 | 54.4 | 57.6 | 0.050 | fail |

Ceiling: 16 scale — ended by rss_slope_tcp-0_send

### The hold

| transport | N | aggregate Mb/s |
|---|---|---|
| srt | 49 | 89.0 |
| rist | 49 | 92.1 |
| udp | 24 | 45.1 |
| tcp | 49 | 92.1 |

| name | observed | threshold | pass |
|---|---|---|---|
| worker_exits | 43 | 0 | fail |
| proxy_stats_final | 0 | 0 | pass |
| recv_invariants | 43 | 0 | fail |
| corruption_coverage | 0.964 | 0.9 | pass |
| corruption_declared | 0 | 0 | pass |
| transport_loss_excused | 6 | 392 | pass |
| delivery_complete | 0.964 | 0.7 | pass |
| cpu_headroom | 0.259 | 0.8 | pass |
| sample_coverage | 0.995 | 0.9 | pass |
| proc_sample_coverage | 0.995 | 0.9 | pass |
| reconnect_count | 96 | 95 | pass |
| peer_restart_recovery | 12 | 12 | pass |
| queue_depth_p99 | 0 | 230.4 | pass |
| gap_drains_after_outage | 4655 | 4655 | pass |
| hold_sizing_declared | 0 | 0 | pass |

| class | transport role | n | pass | fail | max observed | threshold |
|---|---|---|---|---|---|---|
| rss_slope | srt send | 49 | 49 | 0 | 18.53 | 1024 |
| rss_slope | srt recv | 49 | 49 | 0 | 213.949 | 1024 |
| rss_slope | srt proxy | 49 | 49 | 0 | 9.002 | 1024 |
| rss_slope | rist send | 49 | 49 | 0 | 219.944 | 1024 |
| rss_slope | rist recv | 49 | 49 | 0 | 12.897 | 1024 |
| rss_slope | rist proxy | 49 | 49 | 0 | 3.449 | 1024 |
| rss_slope | udp send | 24 | 24 | 0 | 2.8 | 1024 |
| rss_slope | udp recv | 24 | 24 | 0 | 5.758 | 1024 |
| rss_slope | udp proxy | 24 | 24 | 0 | 0.687 | 1024 |
| rss_slope | tcp send | 49 | 49 | 0 | 0 | 1024 |
| rss_slope | tcp recv | 49 | 49 | 0 | 3.678 | 1024 |
| fd_count_flat | srt send | 49 | 49 | 0 | 0 | 2 |
| fd_count_flat | srt recv | 49 | 49 | 0 | 0 | 2 |
| fd_count_flat | srt proxy | 49 | 49 | 0 | 0 | 2 |
| fd_count_flat | rist send | 49 | 49 | 0 | 0 | 2 |
| fd_count_flat | rist recv | 49 | 49 | 0 | 0 | 2 |
| fd_count_flat | rist proxy | 49 | 49 | 0 | 0 | 2 |
| fd_count_flat | udp send | 24 | 24 | 0 | 0 | 2 |
| fd_count_flat | udp recv | 24 | 24 | 0 | 0 | 2 |
| fd_count_flat | udp proxy | 24 | 24 | 0 | 0 | 2 |
| fd_count_flat | tcp send | 49 | 49 | 0 | 0 | 2 |
| fd_count_flat | tcp recv | 49 | 49 | 0 | 0 | 2 |
| thread_count_flat | srt send | 49 | 49 | 0 | 0 | 1 |
| thread_count_flat | srt recv | 49 | 49 | 0 | 0 | 1 |
| thread_count_flat | srt proxy | 49 | 49 | 0 | 0 | 1 |
| thread_count_flat | rist send | 49 | 49 | 0 | 0 | 1 |
| thread_count_flat | rist recv | 49 | 49 | 0 | 0 | 1 |
| thread_count_flat | rist proxy | 49 | 49 | 0 | 0 | 1 |
| thread_count_flat | udp send | 24 | 24 | 0 | 0 | 1 |
| thread_count_flat | udp recv | 24 | 24 | 0 | 0 | 1 |
| thread_count_flat | udp proxy | 24 | 24 | 0 | 0 | 1 |
| thread_count_flat | tcp send | 49 | 49 | 0 | 0 | 1 |
| thread_count_flat | tcp recv | 49 | 49 | 0 | 0 | 1 |

Per-process rows are rolled up; the full list is `hold/step-results.json` in the archive.

### Limitations

- rist/bitrate: rss_slope_rist-0_send over its allowance at step 1, step 2, step 4, step 8, step 16, step 32, step 64 — recorded, not gated (declared rss_slope_ungated; the hold gates it)
- rist/streams: ended by the memory budget at 512 — the box's memory, not a transport verdict; 256 is the last load that fit
- rist/streams: rss_slope over its allowance for 256 processes matching `rist-*_send` (per-process list in stress-results.json) — recorded, not gated (declared rss_slope_ungated; the hold gates it)
- srt/streams: ended by the memory budget at 512 — the box's memory, not a transport verdict; 256 is the last load that fit
- tcp/streams: ended by the memory budget at 512 — the box's memory, not a transport verdict; 256 is the last load that fit
- udp/streams: ended by step_unjudgeable at 256 — a harness condition, not a transport verdict; the ceiling is the last judgeable load, not a measured limit
- hold scaled to 35% by memory: predicted 31286 MB vs budget 10990 MB (n_hold rist=49 srt=49 tcp=49 udp=24)
- hold throughput for srt-0 is its last segment only: 6810 s of the 86460 s run (the receiver was restarted; earlier segments wrote no report)
<!-- bench:end -->

## Measured: stress run of 2026-11-07

The run used tree `d0a8e34ef` (`v0.9.0-946-gd0a8e34ef`) on two hosts at
once: the x86_64 reference machine (`c7i.2xlarge`) and the aarch64 one
(`c7g.2xlarge`), both 8 vCPU and 15.3 GiB, with seed 11, all four
transports, an RSS-slope threshold of 1024 KB/hour, and the RIST sender
recorded, not gated, in sweep steps. The library code in this tree is the
0.9.1 release's: the commits between it and the release change only the
test harness and documentation. The sweep ran from 2026-11-07T17:53Z to
2026-11-08T05:58Z on x86_64 (66 steps) and to 05:47Z on aarch64 (63
steps); the traffic is seeded, so both hosts carried the same bytes,
injections and discontinuities at every rung.

**Stream count.** On x86_64, SRT, RIST and TCP stopped at a memory ceiling
of 256 streams (the 512 rung skipped by the step memory guard); on
aarch64, TCP reached 512 before its 1024 rung was skipped. RSS per stream
was flat across every ladder: on x86_64 about 29.7 MiB for SRT, 28.9 for
RIST, 24.4 for UDP and 18.1 for TCP; on aarch64 about 26.7, 26.5, 22.6
and 16.3 — 2 to 3 MiB less per stream, which is what let aarch64's TCP
fit one rung higher.

Four 256 rungs ended `step_unjudgeable` in this sweep: UDP on x86_64, and
SRT, RIST and UDP on aarch64. The cause is in the harness's launch order,
not in a transport. A step starts every receiver, waits two seconds, then
starts every sender, and each receiver waits 15 seconds for its sender's
traffic. With 768 processes to start, these steps took 19–22 seconds to
launch, so the earliest receivers gave up before their senders existed:
the failures were a contiguous run of streams from stream 0, and every
later receiver in the same steps passed. The harness now reserves all of a
step's ports before its launch loop, which brings a 256-stream launch back
to 7–9 seconds. A re-run of the six 256 rungs (SRT, RIST and UDP on both
hosts) with that change passed all six, with no receiver timing out:

| Host | Transport | Launch span | CPU/stream (cores) | RSS/stream p99 (MiB) | Wire Mb/s | Pass |
|---|---|---|---|---|---|---|
| x86_64 | SRT | 7 s | 0.018 | 29.8 | 481.6 | pass |
| x86_64 | RIST | 7 s | 0.015 | 29.3 | 482.4 | pass |
| x86_64 | UDP | 8 s | 0.005 | 24.8 | 482.4 | pass |
| aarch64 | SRT | 9 s | 0.024 | 26.7 | 481.6 | pass |
| aarch64 | RIST | 8 s | 0.020 | 26.6 | 482.4 | pass |
| aarch64 | UDP | 9 s | 0.006 | 22.6 | 482.4 | pass |

So the streams ceilings on this tree are 256 for SRT, RIST and UDP on both
hosts, TCP 256 on x86_64 and 512 on aarch64. The re-run did not attempt
512 for the four re-run rungs, so on those 256 is the highest load
tested, not a measured limit.

**Per-stream bitrate.** The last passing `--au-scale` rung on one stream:

| Transport | x86_64 ceiling | aarch64 ceiling | Ended by |
|---|---|---|---|
| SRT | 32 (54.4 Mb/s) | 8 (13.6 Mb/s) | x86_64 at 64: `rss_slope_srt-0_send`, 10 150 KB/h against 6 827 allowed, and one transport-loss event on the clean link; aarch64 at 16: `rss_slope_srt-0_send`, 9 364 KB/h against 6 827 |
| RIST | 32 (54.4 Mb/s) | top of the ladder (64) | x86_64 at 64: 6 771 transport-loss events, with librist logging lost packets; aarch64 passed every rung |
| UDP | 32 (54.4 Mb/s) | 32 | 64 on both: `rss_slope_udp-0_send`, 9 432 KB/h against 6 827 |
| TCP | 16 (27.2 Mb/s) | 16 | 32 on both: `rss_slope_tcp-0_send`, 9 165 KB/h against 6 827 |

UDP and TCP ended at the same rungs, on the same verdict, by the same
figures on both hosts: the warm-up plateau a 10-minute step cannot tell
from growth, described under the 2026-10-06 run. RIST's x86_64 failure at
108.8 Mb/s is real packet loss, the configuration ceiling described under
the 2026-10-06 run (librist's default `recovery_maxbitrate` of 100 Mb/s);
the aarch64 host carried the same rung without loss, so 64 is the top of
its ladder, not a measured limit. The aarch64 SRT ceiling of 8 is the
slope gate on the sender's early allocation: at the 16 rung its RSS rose
while buffers filled and then settled near 14.8 MB, adding 16 KiB over the
last 120 seconds, so the step's linear fit is dominated by the fill, not
by growth. A longer constant-load window at that rung would bound it;
until one does, 8 is the gated figure. The single transport-loss event on
x86_64's SRT 64 rung was not examined further; the slope verdict alone
ends that rung, so 32 stands either way.

**The hold.** Both hosts sized their hold from the memory budget. x86_64
ran 49 SRT + 49 RIST + 24 UDP + 49 TCP = 171 streams (464 processes) at a
CPU scale of 0.79 and a memory scale of 0.35; aarch64 ran 31 SRT + 31 RIST
+ 31 UDP + 124 TCP = 217 streams (527 processes) at 0.94 and 0.37. Both
ran their full 86 400 seconds under the impairment schedule described
above, and 12 of 12 scheduled receiver restarts ran on each.

Every resource and liveness verdict passed on both hosts. The largest RSS
slope of any process was 220 KB/h on x86_64 and 232 KB/h on aarch64,
against the 1024 KB/hour allowance; descriptor and thread counts stayed
flat on every process; every SRT sender reconnected after each of the 96
outages; every outage's gap drained (4 655 on x86_64, 2 945 on aarch64);
and the queue-depth and restart-recovery verdicts passed. The lowest
delivery ratio was 0.964 on both hosts, on the restarted receiver `srt-0`,
whose report covers only its last segment (the final 6 810 seconds), so
the ratio is judged against that segment. Aggregate wire throughput was
318 Mb/s at 0.26 of the host's 8 vCPUs on x86_64 and 406 Mb/s at 0.29 on
aarch64 (SRT 56.3, RIST 58.3, UDP 58.3, TCP 233.0 Mb/s). The x86_64 hold's
tables are in the generated block above; the aarch64 hold's are in its
archive's `stress-results.json`.

The hold's `recv_invariants` and `worker_exits` verdicts FAILED on both
hosts, the same way the 2026-10-06 hold's did. On x86_64, 43 of the 171
receivers (38 SRT, 3 RIST, 1 UDP, 1 TCP) ended with corruption-attribution
gaps; on aarch64, 29 of 217 (22 SRT, 2 RIST, 5 TCP). Each of those
receivers exited 1 on its own verdict; none crashed. Across both hosts the
attribution engine could not charge 120 events to a logged injection
(65 on x86_64, 55 on aarch64), and 28 injections were expected to produce
an event that it did not match; no injection went unrecovered.

Every one of the 120 events fits a harness mechanism already described
under the 2026-10-06 run. 96 sit at SRT outage seams, about 833–870
seconds into the 900-second outage period, where the engine holds the
post-reconnect PCR jump as an unconfirmed anchor. 23 come from the raw
reader's sync-byte hunt locking inside a PCR packet; they group into nine
incidents, several at exactly the packet positions they had in the
2026-10-06 run and identical across the two hosts, as seeded traffic
predicts. The last one falls one packet outside the attribution window's
lower edge at a seam. Of the 28 unmatched injections, 17 have a recorded
resync beside them, and 4 are RIST injections on packets sent before the
receiver had stored its first packet. The remaining 7, all on SRT streams
at outage seams, have no retained signal to pair with. The archive keeps
no receive capture, so those 7 cannot be shown to be harness-only; they
have the seam shape the 2026-10-06 analysis described. No event needs a
library mechanism to explain it.

As with the 2026-10-06 hold, the `recv_invariants` FAIL is cited here as
that harness verdict defect, not as an overall 24-hour PASS and not as a
product finding; a passing attribution verdict needs a re-run after the
harness fix. The resource and liveness measurements above stand on their
own.

## Measured: stress run of 2026-10-23

The run used tree `12c1ff1d` (`v0.8.0-531-g12c1ff1d`) on
the reference machine above (8 vCPU, 15.3 GiB), with seed 11, all four
transports, an RSS-slope threshold of 1024 KB/hour, and the RIST sender
recorded, not gated, in sweep steps. The sweep ran 64 steps from
2026-10-23T21:27Z to 2026-10-24T09:08Z (11 h 41 m); the traffic is seeded,
so every step carried the same bytes, injections and discontinuities as
the 2026-10-06 run's step at the same rung.

**Stream count.** All four transports stopped at a memory ceiling rather
than a transport verdict: 256 streams each, with the 512 rung skipped by
the step memory guard (predicted 19.2 GB for SRT, 18.8 GB for RIST,
15.9 GB for UDP and 11.4 GB for TCP against the 11.25 GB budget). TCP's
figure is 1.2 % over the budget, so it is one rung below the 2026-10-06
run's 512: its RSS per stream grew from 16.1 to 17.8 MiB between the two
trees, as did every transport's (SRT and RIST by about 3 MiB per stream,
UDP by about 2), and the memory guard reads that directly. RSS per stream
was flat across every ladder — about 29.4 MiB for SRT, 28.8 for RIST, 24.3
for UDP and 17.4 for TCP — and at 256 streams each transport carried about
482 Mb/s on the wire at 0.019, 0.016, 0.006 and 0.004 cores per stream.

**Per-stream bitrate.** The last passing `--au-scale` rung on one stream:

| Transport | Ceiling (scale) | Declared rate at the ceiling | Ended at | Ended by |
|---|---|---|---|---|
| SRT | 16 | 27.2 Mb/s | 32 | `rss_slope_srt-0_send`, 9 160 KB/h against 6 827 allowed |
| RIST | 16 | 27.2 Mb/s | 32 | `rss_slope_rist-0_recv`, 10 163 KB/h against 6 827 allowed — **untriaged** |
| UDP | 32 | 54.4 Mb/s | 64 | `rss_slope_udp-0_send`, 10 232 KB/h against 6 827 allowed |
| TCP | 16 | 27.2 Mb/s | 32 | `rss_slope_tcp-0_send`, 9 167 KB/h against 6 827 allowed |

Each axis ended on its memory-slope verdict alone; delivery, CPU,
descriptor and thread verdicts passed at the failing rung, and the RIST
rung that failed delivered 0.998 of its bytes with no librist loss lines.
UDP and TCP ended where the two earlier runs did, by the same figures to
within a few KB: the warm-up plateau a 10-minute step cannot tell from
growth, as described under the 2026-10-06 run. SRT's sender shows the
same shape one rung lower than on the 2026-10-06 tree: its RSS rose from
16.6 to 21.6 MB inside the first 90 seconds of the 32 rung and then held
within 70 KB for the remaining nine minutes (about 0.6 MB/hour), so the
verdict is that early rise landing inside the judged window, not growth;
the 2026-10-06 run's sender at the same rung made the same rise inside
its 60-second warm-up and passed.

RIST's row is different and is published as **untriaged**: the receiver,
not the sender, failed the slope, and it kept rising through the whole
judged window — 29.8 MB at 90 s, then 30.1, 30.5, 30.9 and 31.1 MB at
180, 360, 450 and 540 s, in steps of a few hundred KB that were getting
smaller. The 2026-10-03 run's RIST axis ended at the same rung on the same
receiver verdict (11 334 KB/h), while the 2026-10-06 run's receiver was
flat at that rung (29.10 to 29.16 MB) on byte-identical traffic, so the
behaviour predates this tree and is not deterministic across hosts. It is
specific to RIST: the SRT, UDP and TCP receivers at their rungs were flat
in both runs with the same discontinuity counts, and the RIST receive path
in the library did not gain any buffering between the two trees, which
puts the growth inside librist's receive and recovery buffers. The
72-hour 0.9.0 release soak's RIST receiver, at a far lower rate under the
same impairment, is flat from its second hour on. Whether the curve
plateaus at 54 Mb/s past the 10-minute window is the open question; until
a longer window answers it, 16 is the RIST bitrate ceiling on this tree
and the 2026-10-06 run's 32 (a configuration ceiling, see below) is not
carried forward.

**The hold.** The hold was sized from the memory budget at 44 streams per
transport (176 streams, 484 processes, a CPU scale of 0.70 and a memory
scale of 0.35) and died 36 seconds after launch, before any sample was
judged. The cause is in the harness, not in a transport: each stream's
receiver port is reserved by a probe-and-close bind, but the impairment
proxy in front of it asks the kernel for a port of its own at launch, and
in a 484-process launch one proxy was handed the very port reserved for
its own receiver, which had not bound yet. That receiver's listener bind
failed and exited, and the supervisor ended the hold, as it is meant to
when any hold worker dies. The harness now reserves the proxies' ports
the same way; this run predates that change. The run therefore carries no 24-hour hold
verdict, and no figure on this page from this run is a 24-hour
measurement.

## Measured: stress run of 2026-10-06

The run used tree `9bb728e1` (`v0.7.0-24-g9bb728e1`) on the reference
machine above (8 vCPU, 16 077 016 kB), with seed 11, all four transports,
an RSS-slope threshold of 1024 KB/hour, and the RIST sender recorded, not
gated, in sweep steps.
The sweep ran from 2026-10-06T16:25Z to 2026-10-07T04:42Z (12 h 17 m),
climbing the stream ladder to 1024 with a step memory guard that skips any
rung predicted to exceed 0.70 of the box's memory.

**Stream count.** All four transports stopped at a memory ceiling rather
than a transport verdict. SRT and RIST both stopped at 256 streams (the 512
rung was skipped, predicted at 17.07 GB and 17.12 GB against the 11.25 GB
budget), UDP likewise stopped at 256 (predicted at approximately 17 GB),
and TCP reached 512 before its 1024 rung was skipped at a predicted
20.6 GB. In every case the number is the box's memory, not a transport
verdict. Average RSS per stream (sender, proxy and receiver together)
stayed flat across the whole ladder: about 26.1 MiB for SRT and RIST,
21.9 MiB for UDP, and 15.7 MiB for TCP. At 256 streams each transport
carried about 482 Mb/s on the wire; TCP's extra rung at 512 streams carried
about 965 Mb/s. Measured as cores per stream at the ceiling, SRT used
0.017, RIST 0.014, UDP 0.005, and TCP 0.003.

**Per-stream bitrate.** The last passing `--au-scale` rung on one stream:

| Transport | Ceiling (scale) | Declared rate at the ceiling | Ended at | Ended by |
|---|---|---|---|---|
| SRT | 64 | 108.8 Mb/s | — | never failed — the top of the ladder, not a measured limit |
| RIST | 32 | 54.4 Mb/s | 64 | `transport_loss_excused`, 4781 unexplained transport-loss events against a budget of 0 |
| UDP | 32 | 54.4 Mb/s | 64 | `rss_slope_udp-0_send`, 10 227 KB/h against 6 827 allowed |
| TCP | 16 | 27.2 Mb/s | 32 | `rss_slope_tcp-0_send`, 8 855 KB/h against 6 827 allowed |

RIST's failing rung was real packet loss — librist logged "Lost N packets"
once a second against a 16 MiB receiver buffer, not a harness artefact.
This is a configuration ceiling: librist's default recovery bitrate cap
(`recovery_maxbitrate`, 100 Mb/s, exposed as `?recovery_maxbitrate=` /
`?bandwidth=`) is below the 108.8 Mb/s rung; the transport was not
re-measured with the cap raised.

UDP and TCP failed the same way the 2026-10-03 run did, by the same figures to the KB:
each axis ended on its memory-slope verdict alone, with delivery, CPU,
descriptor and thread verdicts passing at the failing rung. The allowance
is the 1024 KB/hour threshold scaled to the step's judged window, so a
one-time rise while buffers grow to a higher rate fails it. These ceilings
mark where a 10-minute step stops being able to tell warm-up from growth,
not where a transport stops delivering — and both figures reproducing run
3's exactly is that same deterministic warm-up plateau, not a regression.

**The hold.** The sweep found a streams ceiling on every transport, so the
hold was sized from the memory budget rather than the CPU budget: 41 SRT +
41 RIST + 41 UDP + 83 TCP = 206 streams (535 processes), at a CPU scale of
0.74 and a memory scale of 0.32. It ran its full 86 400 seconds under the
soak's impairment schedule — a 30-second outage on every SRT proxy every 15
minutes, and the SRT receiver `srt-0` restarted every 2 hours (12 of 12
scheduled restarts ran; every old process exited on SIGTERM on schedule,
including the ninth restart, where the 2026-10-03 run's hold died).

Every resource and liveness verdict passed. All 535 processes' RSS slopes
stayed under the 1024 KB/hour allowance — the largest anywhere was
292 KB/h, on the restarted receiver `srt-0`, judged per restarted pid —
file descriptor and thread counts stayed flat on every process, and the
reconnect, queue-depth, restart-recovery and gap-drain verdicts all passed.
The SRT sender whose receiver was restarted twelve times slopes at
23.0 KB/h over 86 340 seconds; the 41 SRT senders span 2.4–23.0 KB/h.
Aggregate wire throughput over the hold was 384.6 Mb/s at 0.23 of the
host's 8 vCPUs.

The hold's `recv_invariants` and `worker_exits` verdicts nevertheless
FAILED: 36 of the 206 receivers (33 SRT, 2 TCP, 1 UDP; no RIST) ended
their 24 hours with corruption-attribution gaps — 85 events the harness's
attribution engine could not charge to a logged injection, plus 10
injections it expected to see an event for and did not, against
7.40 million injections run over the hold. That is about 11.5 per million
overall (53 on SRT, 1.7 on TCP, 1.4 on UDP, 0 on RIST).

A same-day analysis traced every one of those events to the test harness's
attribution engine, not to the library, a transport, or a proxy. 75 sit at
SRT outage seams — phase 834–869 s of each 900-second outage window —
where the engine holds the post-reconnect PCR clock jump as an
unconfirmed candidate anchor until a second PCR agrees, leaving anything
injected in that window unresolved when its own event fires; this is why
only SRT receivers failed this way. The other 10 — the only non-SRT
failures, at a uniform 0.5 per million injections — come from the raw
reader's sync-byte hunt false-locking just inside a PCR packet whose very
next packet had a flipped sync byte, discarding a good anchor and
resolving the injection one PCR interval late. The one unrecovered
injection comes from a reconnect-gap excusal keyed to media on any PID
rather than the injection's own. The `recv_invariants` FAIL is cited here
as that harness verdict defect, not as an overall 24-hour
PASS and not as a product finding; a passing attribution verdict needs a
re-run after the harness fix. The resource measurements above stand on
their own.

The hold's tables, as the generator rendered them from that run's
`stress-results.json` (per-process rows rolled up; the full list is
`hold/step-results.json` in the archive):

| transport | N | aggregate Mb/s |
|---|---|---|
| srt | 41 | 74.5 |
| rist | 41 | 77.0 |
| udp | 41 | 77.1 |
| tcp | 83 | 156.0 |

| name | observed | threshold | pass |
|---|---|---|---|
| worker_exits | 36 | 0 | fail |
| recv_invariants | 36 | 0 | fail |
| corruption_coverage | 0.965 | 0.9 | pass |
| corruption_declared | 0 | 0 | pass |
| transport_loss_excused | 6 | 392 | pass |
| delivery_complete | 0.964 | 0.7 | pass |
| cpu_headroom | 0.23 | 0.8 | pass |
| sample_coverage | 0.997 | 0.9 | pass |
| reconnect_count | 96 | 95 | pass |
| peer_restart_recovery | 12 | 12 | pass |
| queue_depth_p99 | 0 | 230.4 | pass |
| gap_drains_after_outage | 3895 | 3895 | pass |
| hold_sizing_declared | 0 | 0 | pass |

| class | transport role | n | pass | fail | max observed | threshold |
|---|---|---|---|---|---|---|
| rss_slope | srt send | 41 | 41 | 0 | 23.049 | 1024 |
| rss_slope | srt recv | 41 | 41 | 0 | 292.067 | 1024 |
| rss_slope | srt proxy | 41 | 41 | 0 | 11.562 | 1024 |
| rss_slope | rist send | 41 | 41 | 0 | 224.166 | 1024 |
| rss_slope | rist recv | 41 | 41 | 0 | 15.214 | 1024 |
| rss_slope | rist proxy | 41 | 41 | 0 | 3.163 | 1024 |
| rss_slope | udp send | 41 | 41 | 0 | 3.328 | 1024 |
| rss_slope | udp recv | 41 | 41 | 0 | 4.704 | 1024 |
| rss_slope | udp proxy | 41 | 41 | 0 | 0.752 | 1024 |
| rss_slope | tcp send | 83 | 83 | 0 | 2.43 | 1024 |
| rss_slope | tcp recv | 83 | 83 | 0 | 8.148 | 1024 |
| fd_count_flat | srt send | 41 | 41 | 0 | 0 | 2 |
| fd_count_flat | srt recv | 41 | 41 | 0 | 0 | 2 |
| fd_count_flat | srt proxy | 41 | 41 | 0 | 0 | 2 |
| fd_count_flat | rist send | 41 | 41 | 0 | 0 | 2 |
| fd_count_flat | rist recv | 41 | 41 | 0 | 0 | 2 |
| fd_count_flat | rist proxy | 41 | 41 | 0 | 0 | 2 |
| fd_count_flat | udp send | 41 | 41 | 0 | 0 | 2 |
| fd_count_flat | udp recv | 41 | 41 | 0 | 0 | 2 |
| fd_count_flat | udp proxy | 41 | 41 | 0 | 0 | 2 |
| fd_count_flat | tcp send | 83 | 83 | 0 | 0 | 2 |
| fd_count_flat | tcp recv | 83 | 83 | 0 | 0 | 2 |
| thread_count_flat | srt send | 41 | 41 | 0 | 0 | 1 |
| thread_count_flat | srt recv | 41 | 41 | 0 | 0 | 1 |
| thread_count_flat | srt proxy | 41 | 41 | 0 | 0 | 1 |
| thread_count_flat | rist send | 41 | 41 | 0 | 0 | 1 |
| thread_count_flat | rist recv | 41 | 41 | 0 | 0 | 1 |
| thread_count_flat | rist proxy | 41 | 41 | 0 | 0 | 1 |
| thread_count_flat | udp send | 41 | 41 | 0 | 0 | 1 |
| thread_count_flat | udp recv | 41 | 41 | 0 | 0 | 1 |
| thread_count_flat | udp proxy | 41 | 41 | 0 | 0 | 1 |
| thread_count_flat | tcp send | 83 | 83 | 0 | 0 | 1 |
| thread_count_flat | tcp recv | 83 | 83 | 0 | 0 | 1 |

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
