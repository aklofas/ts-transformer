### Reference machine

- Kernel: Linux 7.0.0-34-generic x86_64
- vCPUs: 32
- Memory: 62.6 GiB
- Toolchain: rustc 1.85.1 (4eb161250 2025-03-15)
- Source: `e3d7567d4fb7` (v0.6.0-645-ge3d7567d)
- Recorded: 2026-10-02T01:59:06Z

### Stream scaling

#### SRT

| N | CPU/stream (cores) | RSS/stream p99 (MiB) | threads max | fds max | wire Mb/s | pass |
|---|---|---|---|---|---|---|
| 1 | 0.035 | 25.5 | 6 | 5 | 2.3 | pass |
| 2 | 0.035 | 25.4 | 6 | 5 | 4.6 | pass |

Per-stream figures are averages over the step's streams.

Ceiling: top of ladder (2), not a measured limit

### Single-stream throughput

#### TCP

| scale | declared Mb/s | observed Mb/s | CPU (cores) | pass |
|---|---|---|---|---|
| 1 | 1.7 | 2.4 | 0.008 | pass |
| 2 | 3.4 | 4.6 | 0.009 | fail |

Ceiling: 1 scale — ended by cpu_headroom

### The hold

| transport | N | aggregate Mb/s |
|---|---|---|
| srt | 1 | 1.2 |
| tcp | 1 | 2.2 |

| name | observed | threshold | pass |
|---|---|---|---|
| worker_exits | 1 | 0 | fail |
| recv_invariants | 1 | 0 | fail |
| delivery_complete | 0.936 | 0.7 | pass |
| cpu_headroom | 0.001 | 0.8 | pass |
| sample_coverage | 0.986 | 0.9 | pass |
| reconnect_count | 1 | 0 | pass |
| peer_restart_recovery | 2 | 2 | pass |
| queue_depth_p99 | 0 | 230.4 | pass |
| hold_sizing_declared | 0 | 0 | pass |

| class | transport role | n | pass | fail | max observed | threshold |
|---|---|---|---|---|---|---|
| rss_slope | srt send | 1 | 0 | 1 | 19789.863 | 10355.056 |
| rss_slope | srt recv | 1 | 1 | 0 | 4636.614 | 20144.262 |
| rss_slope | srt proxy | 1 | 1 | 0 | 757.191 | 10355.056 |
| rss_slope | tcp send | 1 | 1 | 0 | 0 | 10355.056 |
| rss_slope | tcp recv | 1 | 1 | 0 | 176.589 | 10355.056 |
| fd_count_flat | srt send | 1 | 1 | 0 | 1 | 2 |
| fd_count_flat | srt recv | 1 | 1 | 0 | 1 | 2 |
| fd_count_flat | srt proxy | 1 | 1 | 0 | 0 | 2 |
| fd_count_flat | tcp send | 1 | 1 | 0 | 0 | 2 |
| fd_count_flat | tcp recv | 1 | 1 | 0 | 0 | 2 |
| thread_count_flat | srt send | 1 | 0 | 1 | 3 | 1 |
| thread_count_flat | srt recv | 1 | 1 | 0 | 1 | 1 |
| thread_count_flat | srt proxy | 1 | 1 | 0 | 0 | 1 |
| thread_count_flat | tcp send | 1 | 1 | 0 | 0 | 1 |
| thread_count_flat | tcp recv | 1 | 1 | 0 | 0 | 1 |

Per-process rows are rolled up; the full list is `hold/step-results.json` in the archive.

### Limitations

- rist/bitrate: never failed — the ceiling is the top of the ladder, not a measured limit
- rist/streams: never failed — the ceiling is the top of the ladder, not a measured limit
- srt/bitrate: never failed — the ceiling is the top of the ladder, not a measured limit
- srt/streams: never failed — the ceiling is the top of the ladder, not a measured limit
- tcp/streams: never failed — the ceiling is the top of the ladder, not a measured limit
- udp/bitrate: never failed — the ceiling is the top of the ladder, not a measured limit
- udp/streams: never failed — the ceiling is the top of the ladder, not a measured limit
