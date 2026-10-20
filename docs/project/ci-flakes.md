# CI flake ledger

The classes of CI failure the project currently tolerates with a rerun, so
that a rerun on a release candidate can be checked against a published list
rather than taken on trust. A failure that matches no row is a finding,
never a rerun. A class leaves this table when its fix lands; the CHANGELOG
is the record of the fix and a recurrence after it is a regression.

**How a row is used.** Before rerunning a red job, confirm two things: the
failure matches the row's symptom, and the change under test cannot reach
the failing code (for a library-code change, `git show --stat <sha>` lists
no path the test exercises). Then `gh run rerun <id> --failed`. Every
sighting updates the row's last-seen run. A class seen twice with a
mechanism in our code is hardened, not rerun again; the "Rule" column says
which.

**Where the numbers come from.** The weekly `ci-health` workflow reports
rerun counts per job over the last seven days in its step summary and CSV
artifact, and prints an `ALERT` line for any job rerun twice or more. An
`ALERT` with no matching row here is a finding. The other two ledgers are
`known_flaky` rows in `scripts/interop/expectations.toml` (interop cells)
and `environmental` entries in `tests/coverage/skip-ledger.toml`
(timing-sensitive skipped tests).

## Active classes

| Class | Job | Symptom | Cause | First seen | Last seen | Rule |
|---|---|---|---|---|---|---|
| Runner setup fetch failure | any; the install, toolchain or checkout step | `apt-get` mirror unreachable or `403` from a runner-image repository; `rustup` channel download timeout; `setup-java` `504`; Gradle wrapper or Plugin Portal fetch failure; `actions/checkout` cannot reach `code.videolan.org` for the librist submodule | external host or runner image, before any project code runs | 2026-07-16 | 2026-10-20 (run 39459737215, macOS wheel leg: `Could not resolve host: code.videolan.org` at checkout; 2026-10-19 runs 39221903221, 39216292373 and 39284237561: the Chocolatey feed, which no workflow calls any more — see the closed table; 2026-10-14 run 38610389641 for the submodule host) | rerun; the apt steps already retry five times. A submodule-host outage on several runs in a row is the trigger to mirror librist |
| GitHub-wide degradation | several unrelated legs at once | many legs fail in under 60 s on downloads or API calls | GitHub incident | 2026-07-17 | — | check `https://www.githubstatus.com/api/v2/status.json` first; rerun once it reads `none`. A cancelled run during an incident is not a failure |
| Wedged runner | any | a job sits for an hour or more on a step that takes minutes elsewhere in the same run (an apt install, a warm `cargo build`, the end of a pytest suite) | runner starvation | 2026-07-18 | 2026-08-18 | `gh run cancel <id>`, wait until the run reads completed, then `gh run rerun <id> --failed`; a rerun is refused while the cancel is in progress |
| FreeRTOS SRT gate timeout | `no_std bare-metal (compile + QEMU runtime)` | the `freertos-srt` QEMU gate is killed by its timeout (`rc=124`), sometimes with an empty transcript | SRT handshake and ARQ under QEMU on a loaded runner | 2026-06-03 (run 26870670934) | 2026-09-14 | the gate retries once inside the job and prints `FLAKY PASS` with a warning annotation on a pass-on-retry, which is expected output. A red leg means both attempts failed on identical binaries: investigate unless the change touches neither `embedded/` nor `crates/`, then rerun on a fresh runner |
| Windows build-directory file lock | `build (windows-msvc)`, `cargo clippy` step | `error: failed to write ...\target\debug\deps\lib<crate>-<hash>.rmeta: The process cannot access the file because it is being used by another process` on a dependency crate, then `could not compile` | another process on the runner (indexer or scanner) holding a freshly written artefact; tree-independent (first seen on a docs-only commit) | 2026-10-19 (run 39216292373) | 2026-10-19 | rerun. A third sighting adds a retry of the clippy step on Windows |
| Windows loopback UDP buffer exhaustion | `build (windows-msvc)` | `tst-rtp::net_conformance rtp_send_contract` fails at `RtpTransport::connect` with `Os { code: 10055 }` (`WSAENOBUFS`) | Windows loopback UDP under a full test run | 2026-10-18 (run 39107986333) | 2026-10-18 | rerun. A second sighting serialises the test or sizes its socket buffers; see the `?rcvbuf=`/`?sndbuf=` URL keys |
| Windows loopback throughput floors | `build (windows-msvc)` and `build (windows-msvc-features)` | a tst-interop round-trip test delivers about 60 % of its nominal AU count inside its window (`serve::rtsp_serve_round_trip_via_own_client`, `srt_realistic_au_sizes_round_trip_and_match`) | RTP-over-UDP on the Windows loopback under runner load | 2026-09-08 (run 34294405247) | 2026-09-14 (run 34901386367) | rerun; harvest the job log first. A third sighting widens the window or lowers the floor for Windows only |
| Windows JVM loopback timeouts | `jvm windows-x86_64` | `RtpTransportTest` bind or recv raises `RtpException`; `HlsServingTest > httpsServingWithTls` hits its 10 s connect timeout to the test's own local server | the Windows JVM runner slow to bind or accept on loopback | 2026-09-11 (run 34543938354) | 2026-10-15 (run 38770420190) | rerun. A second sighting of the HLS case raises the connect timeout |
| SRT lossy-proxy retransmission recovery | `cargo test` on macos-arm64 and windows-msvc | `tst-interop::proxy srt_round_trip_through_lossy_proxy_recovers_via_retransmission` misses its recovery assertion | unknown; three sightings on changes that cannot reach it | 2026-09-10 (run 34529518649) | 2026-09-29 (run 36530543161) | **hardening due**, not a rerun case: the project's own rule closes a class at its third sighting. The next sighting is investigated with the job log |
| Python SRT bind-fault kind | `tst-py default tests` | `test_listener_open_bind_fault_is_broken_with_a_bind_prefix[Receiver]` sees `Notsup` from libsrt instead of the `bind:`-prefixed `BROKEN` for a port another listener holds | one sighting; mechanism unexplained | 2026-10-18 (run 39148686695) | 2026-10-18 | harvest the log and look at the bind path before rerunning; a second sighting is a finding, not a flake |

## Closed classes

A recurrence of any of these is a regression. Read the CHANGELOG entry of
the release that shipped the fix before touching the test.

| Class | Shipped in | What changed |
|---|---|---|
| Windows `tcps://` close ends the peer read `Broken` instead of `CleanEof` | 0.7.0 | `tst-tcp` drains unread TLS records before the `close_notify` |
| macOS transparent-proxy order test and `tst-rtp oom_guard` | 0.7.0 | the harness and the test's guard, root-caused |
| Interop `rtsp-serve` "bind address in use" | 0.7.0 | `free_port` probes the protocol the cell binds |
| macOS keepalive count, `accept_timeout` upper bound, tst-c reconnect `conntimeo` | 0.8.0 | the assertions measure the right quantity with a margin |
| Interop `es-klv` KLV digest mismatch | 0.8.0 | an aligner tick-rounding defect; a mismatch now names the loss site |
| Python exit code 134 after a passing suite | 0.8.0 | a worker re-taking the GIL after finalisation; the job dumps a core on recurrence |
| `raw_rtsp` fixture read timeout under a sanitizer | unreleased (0.9.0) | the fixture's timeouts scale with `TST_TEST_TIMEOUT_SCALE` |
| Windows `client_relearns_after_a_genuine_quiet_period` recv deadline | unreleased (0.9.0) | a bounded retry of the reconnect datagram |
| Python GIL-probe workload and throughput floors | unreleased (0.9.0) | every probe's workload is three times the minimum |
| Linux thread-count probe right after `join()` | unreleased (0.9.0) | the probe polls the task directory for up to 2 s |
| ASan leg hangs with a thread parked in libsrt's receive | unreleased (0.9.0) | the exit test's child runs with `detect_leaks=0`; LeakSanitizer's stop-the-world does not return past a parked receive |
| Windows prerequisites through the Chocolatey feed (`503`/`504` on ripgrep, sourceforge `522` through pkgconfiglite) | unreleased (0.9.0) | the Windows legs take ninja, meson and pkgconf from PyPI and ripgrep from its pinned GitHub release with a checksum; no workflow calls Chocolatey |

A sanitizer-job red whose tree has not changed is compared against the
nightly toolchain hash of the last green run before any code bisect; the
unpinned nightly is itself a variable in `sanitizers.yml`.
