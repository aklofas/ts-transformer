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

The inventory reads each file's PMT from the first program map the demuxer
emits in its first 4 MiB, so a PMT that spans packets or damaged bytes before
it never stops a file from being read. A file whose probe or digest panics is
counted as a parse failure under its SHA-256, and the sweep goes on. On a
file's PCR PID, a reset or a forward jump of more than one second adds no
time: the duration, the 600-second cut and the replay's pacing all leave it
out, so a capture that pauses never makes the replay sleep.

A directory with no `.ts` file exits 2. A `--baseline` directory that does
not exist exits 2, and a baseline that holds the digest of none of the
captures fails the sweep. A first run uses no `--baseline` at all.

## Invariants

A capture fails the sweep when any of these breaks. None needs a baseline.

- **feed-error:** the demuxer reported no error while being fed the file. A
  rejection is counted, never a parse failure, so a capture with damage such
  as a zero-filled recorder dropout is still digested. The annotation
  `known_feed_errors = true` records reviewed feed errors as excused.
- **klv-unknown:** every KLV payload on a KLV stream decodes, and no sample on
  it comes out as an unknown kind. An annotation that lists a PID under
  `non_misb_private_pids` excuses a private stream that is not MISB KLV.
- **accounting:** the file contains packets, and the demuxer's discontinuity
  count equals a continuity-counter count from an independent walk of the
  wire. The walk follows the demuxer's own rules: which PIDs count, PSI
  adoption, tolerated duplicates, and sync acquisition (after a loss, a
  candidate needs 5 of the next 7 strides). A PES that outgrows the
  demuxer's reassembly cap is a discontinuity the wire cannot show, so it is
  counted apart and left out. The annotation `known_cc_mismatch = true`
  records a reviewed difference as excused, with its reason. This rule
  compares continuity counters only: the design's other half, that every
  packet in is accounted for by the demuxer, needs per-packet counters the
  demuxer does not keep, and is not checked.
- **pts-order:** presentation timestamps on a PID never jump backwards. A PES
  that carries no PTS takes no part: the demuxer reports it as 0, so a
  timestamp of exactly 0 is counted as absent. The annotation
  `known_discontinuity = true` excuses a capture whose timeline is known to
  restart.

Annotations live outside the corpus, one `<sha256>.toml` file per capture with
flat `key = value` lines. Every key that excuses a rule needs a
`reason = "..."` line, or the annotation is refused. Only a person adds them,
after reading the finding.

## How the replay subset is chosen

Replaying everything would take days, so the tool picks a subset from the
inventory: the files that carry a rare codec or a rare KLV set (fewer than
three files), the longest capture of each PMT shape, and the largest and the
longest file. A capture longer than ten minutes is replayed as its first 600
seconds. The subset, other than the rare-coverage picks, is trimmed to fit a
budget of two hours of wall-clock time. A pick whose replayed bytes would
exceed 4 GiB is skipped, and so is a file with no usable duration. The plan
lists what was trimmed and skipped. A pick whose cut turns out longer than
4 GiB when it is read fails as skipped. A capture that is a symlink resolving
outside the corpus directory refuses the whole replay.

Replays run over SRT, RIST, UDP, TCP and the RTSP publisher path on loopback,
one long-lived receiver process per path. Each receiver's memory is gated per
path by the soak's rule: after the soak's warm-up, a slope above 200 KiB per
hour fails the replay. A run too short for the rule's sample count records
the path as not gated, with the reason.

The replay's time rules:

- A receiver that sees no byte for 10 seconds while the sender is still
  sending is stalled.
- UDP, RIST and the RTSP publisher path have no end of stream, so a file ends
  there after 2 seconds of quiet once the sender has finished. TCP and SRT
  end when the sender closes.
- A send may take the pick's own PCR seconds plus twice the stall time; one
  that takes longer is cancelled and the path's receiver is stopped.

The UDP receiver's socket buffer holds two bursts whenever the kernel default
cannot: a burst is the larger of the capture's largest access unit (the
stress harness's rule) and the largest interval the pacer writes at once. The
replay refuses to start when `net.core.rmem_max` would clamp the buffer the
access units need, and a pick whose interval needs more fails its UDP
verdict. When the sender lost and recovered the packet framing, a failed
verdict says how often, so a short delivery from a damaged capture is not
read as transport loss. A damaged capture can still arrive short with no
resync counted, because the sender's flush forwards a partial bundle
without checking its framing.

Picks replay largest first, by the bytes each one sends. Each receiver's
largest allocation then happens inside the warm-up the RSS gate leaves out,
so a one-time step in its memory is not read as a slope.

The variables named `TST_CORPUS_TEST_*` change these rules for the tool's own
tests. When one is set, the tool names it on stderr.

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
