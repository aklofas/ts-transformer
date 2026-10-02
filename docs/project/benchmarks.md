# Benchmarks + ceilings

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
  128 concurrent streams on one transport, looking for the point where CPU,
  memory, or file/thread accounting stops holding steady.
- **Per-stream bitrate**, at a fixed single stream: `--au-scale` 1, 2, 4,
  … up to 64, which scales a realistic access-unit size up to roughly
  1.7–110 Mb/s. The top of this ladder is bounded by a 4 MiB per-PID PES
  cap on keyframes, not by anything this harness goes looking for.

Both sweeps run on a clean loopback link — the SRT and RIST legs go through
their usual proxies with every impairment knob at zero, and the UDP/TCP
legs connect directly — so a ceiling here is a measure of the host, not of
link tolerance.

The **hold** runs all four transports at once, each sized to ⌊0.7 × its own
ceiling⌋ streams, for 24 hours, under the soak's seeded impairment
schedule plus two extra disruptions on the SRT legs: a 30-second full-drop
outage on every SRT proxy every 15 minutes, and a receiver restart on one
SRT stream every 2 hours. It answers a different question than the
ceiling does — not "how far can this go" but "does a system sized at 70%
of its ceiling actually survive a full day of real-world disruption."

## Verdicts

| Verdict | Rule | Threshold |
|---|---|---|
| `worker_exits` | No worker process exits before the run's own end-of-run teardown. | zero unscheduled exits |
| `recv_invariants` | Every receiver's own internal invariants hold throughout. | no violation |
| `delivery_complete` | Received video access units against sent. | ≥ 0.7×, and > 1.2× fails as an accounting mismatch rather than a pass |
| `cpu_headroom` | Sum of worker CPU-seconds over wall-seconds over vCPU count. | ≤ 0.80 |
| `rss_slope_<leg>_<process>` | Memory growth rate per process, judged after warm-up and scaled for a window shorter than an hour. | ≤ a required KB/hour threshold |
| `fd_count_flat_*` | File-descriptor count range per process after a 60-second warm-up. | max − min ≤ 2 |
| `thread_count_flat_*` | Thread count range per process after the same warm-up. | max − min ≤ 1 |
| `sample_coverage` | Fraction of expected sampler ticks actually recorded. | ≥ 90% |
| `reconnect_count` *(hold only)* | Each SRT receiver rebuilds its transport at least once per outage window it lived through. | ≥ (outage windows − 1); one less than the window count because the final window can coincide with teardown |
| `peer_restart_recovery` *(hold only)* | After each scheduled receiver restart, the sender reconnects. | within 120 seconds; a restart in the run's final 120 seconds is unjudgeable |
| `queue_depth_p99` *(hold only)* | The managed sender's gap-length distribution. | p99 ≤ 0.9× its configured capacity |
| `hold_sizing_declared` *(hold only)* | The hold's configured stream counts actually match the sizing rule. | ⌊0.7 × ceiling⌋ per transport, at least 1 |

## Reference machine and reproduction

The first measured run targets an AWS `c7i.2xlarge` (8 vCPU, 16 GiB) on
Ubuntu noble. Launch recipe:

```bash
SRT_RECONNECT_MODE=background RSS_SLOPE_THRESHOLD_KB_PER_HOUR=<value> \
  nohup bash scripts/interop/stress.sh --outdir <dir> --seed <N> & disown
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

<!-- bench:begin -->
_No measured run has been rendered into this page yet. The first render comes from the c7i.2xlarge stress run; until then this block is empty on purpose rather than carrying numbers from the dev-box smoke, which is a harness check, not a measurement._
<!-- bench:end -->

## Not measured yet

These are out of scope for the current harness, each tracked as part of
the roadmap's stress-harness item:

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
  connection churn, and a deliberately slow receiver are all left for the
  roadmap's benchmarks item to pick up next.

## Evidence rule

A stress run is not a soak PASS, and a soak is not a ceiling. The two
measure different things — a soak proves endurance at a size someone
already chose; a stress run finds the size. Neither result stands in for
the other on this page or anywhere else in this repo's evidence.
