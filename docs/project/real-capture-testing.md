# Real-capture testing

Synthetic fixtures prove that the library handles the streams its authors
thought of. Real captures carry the rest: encoders that interleave tables in
unexpected places, metadata streams with private framing, timelines that
restart mid-file, and sheer length. `tst-interop corpus` runs the demuxer and
the network transports over a directory of such captures and turns the result
into a verdict that can be compared against the previous release.

## What the maintainer's corpus covers

The corpus is not published, because the captures are not ours to share. It
holds captures from airborne electro-optical and infrared sensor pods, a
ground bench rig and vendor test fixtures; H.264 and H.265 video; ST 0601 KLV
in both carriage methods and private metadata streams; 260 files, 47 GB,
durations from under a minute to several hours; PMT shapes a, b and c of the
test-corpus catalog.

The tool is generic. Point it at any directory of `.ts` files to get the same
checks on your own material.

## Two modes

- **Sweep** reads every file once, offline, and records a digest of what the
  demuxer saw. It proves the demuxer is stable: the same bytes give the same
  answer as the previous release, and a change is either explained or a
  finding. It also checks the invariants below, with no baseline needed.
- **Replay** sends a chosen subset of the files through a real transport and
  compares what a receiver on the far side reconstructs with the digest of
  the bytes that were sent. It proves the transports carry real streams
  without loss, reordering or corruption, at the capture's own pace.

## Sweep configurations

Every file is demuxed three times: with the default configuration, with
timestamp unwrapping, and with full strict mode. The digests of all three are
stored, so a change in any of them shows up against the baseline.

## Invariants

A capture fails the sweep when any of these breaks. None needs a baseline.

- **feed-error:** the demuxer reported no error while being fed the file.
- **klv-unknown:** every KLV payload on a KLV stream decodes, and no sample on
  it comes out as an unknown kind. An annotation that lists a PID under
  `non_misb_private_pids` excuses a private stream that is not MISB KLV.
- **accounting:** the file contains packets, and the demuxer's discontinuity
  count equals the count from an independent walk of the wire.
- **pts-order:** presentation timestamps on a PID never jump backwards. The
  annotation `known_discontinuity = true` excuses a capture whose timeline is
  known to restart.

Annotations live outside the corpus, one `<sha256>.toml` file per capture with
flat `key = value` lines and an optional `reason = "..."`. Only a person adds
them, after reading the finding.

## How the replay subset is chosen

Replaying everything would take days, so the tool picks a subset from the
inventory: the files that carry a rare codec or a rare KLV set (fewer than
three files), one file of each PMT shape, and the largest and the longest
file. A capture longer than ten minutes is replayed as its first 600 seconds.
The subset is trimmed to fit a budget of two hours of wall-clock time. A pick
whose replayed bytes would exceed 4 GiB is skipped, and so is a file with no
usable duration. The plan lists what was trimmed and skipped.

Replays run over SRT, RIST, UDP, TCP and the RTSP publisher path on loopback,
one long-lived receiver process per path, so a leak or a slow drift shows in
that receiver's memory over the whole run.

## Running it on your own directory

`RUN` is an output directory that the tool creates; `DIR` is the directory of
captures. Every output names a file by its SHA-256, and paths appear only in
`RUN/inventory.json`.

```bash
# 1. Catalogue the files: codecs, KLV sets, PMT shape, duration.
tst-interop corpus inventory DIR --out RUN

# 2. Sweep. Add --baseline PREVIOUS_RUN_DIGESTS to compare with an earlier run,
#    --annotations ANNOTATIONS_DIR for curated notes, and
#    --accept SHA --because TEXT to accept one reviewed change (repeatable).
tst-interop corpus sweep DIR --out RUN

# 3. Replay the subset. --paths narrows the transports (default: all five) and
#    --budget-seconds changes the two-hour budget.
tst-interop corpus replay DIR --out RUN --paths srt,tcp --budget-seconds 1800

# 4. Report: a Markdown summary and the one-line archive row.
tst-interop corpus report RUN --markdown RUN/report.md --archive-row RUN/archive-row.json
```

Exit status is 0 on a pass, 1 when the sweep or replay fails, and 2 on a usage
or I/O error. `report` exits 0 even for a failed run, because its job is to
describe it.

## How results are cited

The archive row holds eight counts: files, identical, changed and explained,
invariant failures, parse failures, replayed files, transport paths and failed
replays. A run without a replay is not a pass. The row is added to the archive
list that the release qualification reads.

A release qualification run requires a passing corpus archive for the exact
tree that was soaked, and its record carries a release-only line with those
eight counts. See [validation evidence](/docs/project/validation-evidence.md)
for how the record is published.
