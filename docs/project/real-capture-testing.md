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
in both carriage methods and private metadata streams; 251 files, 39.76 GB,
durations from under a minute to several hours; PMT shapes a, b and c of the
test-corpus catalog.
The release gate requires the corpus run to have swept exactly the number of
files named by `expected_files` in `tests/coverage/evidence-archives.toml`.

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

On a KLV stream each payload is split into its top-level MISB items: ST 0601
local sets and ST 0605 Precision Time Stamp Packs, standalone or before the
set. A set is decoded and re-encoded in the encoder's tag order, so a
producer's tag order never changes the hash; a pack has one shape and is
hashed as it is. A payload with an item that does not split, decode or
re-encode is hashed as it is and counted as undecoded, with the first failing
item's reason. Because packs decode, a capture that carries them changes
digest once against a baseline written before they did. The item split is
also stricter than the earlier rule, which decoded a payload when any label
in it did, so it can mark as changed a capture the earlier sweep accepted.
A field added to the digest is taken from the current run when the baseline
predates it, so a new field alone never marks a capture changed.

The ST 0601 decoder keeps a record whose fields it could not all read and
counts the field errors. The sweep reports their total and the captures that
carry them; they are recorded, not gated.

The inventory reads each file's PMT from the first program map the demuxer
emits in its first 4 MiB, so a PMT that spans packets or damaged bytes before
it never stops a file from being read. A file whose probe or digest panics is
counted as a parse failure under its SHA-256, and the sweep goes on. On a
file's PCR PID, a reset or a forward jump of more than one second adds no
time: the duration, the 600-second cut and the replay's pacing all leave it
out, so a capture that pauses never makes the replay sleep.

The inventory never enters a directory named `quarantine`, at any depth,
matched on the whole name and case-sensitively: a corpus keeps its set-aside
captures there. The release VM's bucket sync leaves out only the top-level
`quarantine/`, so the tool's rule is the stricter one.

## Comparing with a baseline

A baseline is the `digests/` directory of an earlier sweep. Beside the
digests the sweep writes `manifest.txt`: the SHA-256 of every whole-capture
digest, one per line, sorted. A comparison reads the manifest before any file
is digested and refuses a baseline without one, with a line out of order or
repeated, or with a listed digest that is missing, does not parse or names
another capture. A baseline that lost digests therefore never stands in for a
whole one.

A directory with no `.ts` file exits 2. A `--baseline` directory that does
not exist exits 2, and a baseline that holds the digest of none of the
captures fails the sweep. A first run uses no `--baseline` at all.

Every capture that changed against the baseline, every capture the baseline
does not hold and every capture the baseline holds that this corpus lacks
fails the sweep until it is accepted by name with
`--accept <sha256> --because <text>`. The reason is evidence: the sweep
summary keeps it for each addition and removal, and the report prints it. An
`--accept` that matches no such capture fails the sweep too.

A digests directory written before manifests existed needs one, once:

```bash
tst-interop corpus manifest BASELINE --expect-files 251
```

The command checks that every whole-capture digest parses and names its own
capture, refuses to write unless it found exactly the expected count, and
overwrites any `manifest.txt` already there. The first comparison against
such a baseline also meets the ST 0605 change above: the captures whose
baseline digest has undecoded KLV records are the ones whose packs now
decode. This lists them, for review and an `--accept` each:

```bash
jq -r 'select(.prefix_seconds == null and .configs.default.klv_unknown > 0) | .sha256' BASELINE/*.json
```

A listed capture whose digest did not change is reported as `unused-accept`;
drop its `--accept` and run the sweep again.

## Invariants

A capture fails the sweep when any of these breaks. None needs a baseline.

- **feed-error:** the demuxer reported no error while being fed the file. A
  rejection is counted, never a parse failure, so a capture with damage such
  as a zero-filled recorder dropout is still digested.
- **klv-unknown:** every KLV payload on a KLV stream decodes, and no sample on
  it comes out as an unknown kind. Undecoded records and label-less samples
  are judged apart: a PID with both, and the records not excused, writes two
  rows, and excusing the records never excuses the samples. A record whose
  declared length runs past its payload is a truncation, the cut packet loss
  leaves. A payload shorter than its first label, a few bytes shorter than a
  label after a complete item, or a missing checksum is not a truncation:
  it is an undecoded record, reviewed with its reason.
- **klv-missing:** a PID the PMT declares as KLV, whose packets open PES on
  the wire, shows KLV in the digest. A demuxer that drops that KLV, or
  classifies the PID as something else, fails here. A PID that carries only
  label-less private payloads fails both this rule and `klv-unknown`.
- **accounting:** the file contains packets, and the demuxer's discontinuity
  count equals a continuity-counter count from an independent walk of the
  wire. The walk follows the demuxer's own rules: which PIDs count, PSI
  adoption, tolerated duplicates, and sync acquisition (after a loss, a
  candidate needs 5 of the next 7 strides). A PES that outgrows the
  demuxer's reassembly cap is a discontinuity the wire cannot show, so it is
  counted apart and left out. This rule
  compares continuity counters only: the design's other half, that every
  packet in is accounted for by the demuxer, needs per-packet counters the
  demuxer does not keep, and is not checked.
- **pts-order:** presentation timestamps on a PID never jump backwards. A PES
  that carries no PTS takes no part: the demuxer reports it as 0, so a
  timestamp of exactly 0 is counted as absent.

Annotations live outside the corpus, one `<sha256>.toml` file per capture with
flat `key = value` lines. Only a person adds them, after reading the finding.
Each key that excuses a rule carries the count the person reviewed, and the
PIDs where the rule is per PID. It waives that much of that damage and no
more, so a capture that gets worse, or is damaged somewhere else, fails
again; a waived finding is still recorded, as an excused row.

| Key | Bound | Excuses |
| --- | --- | --- |
| `known_feed_errors = true` | `max_feed_errors` | `feed-error`: reviewed feed errors |
| `non_misb_private_pids = [...]` | `max_private_records` | `klv-unknown` and `klv-missing` on each listed PID: a private stream that is not MISB KLV. The bound covers the PID's undecoded records and unknown samples, and its count of KLV PES on the wire |
| `known_klv_truncation = true` | `max_klv_truncated`, `klv_truncation_pids` | `klv-unknown` on each listed PID whose every undecoded record is a truncation |
| `known_cc_mismatch = true` | `max_cc_mismatch` | `accounting`: the largest reviewed difference between the wire walk and the demuxer |
| `known_discontinuity = true` | `max_pts_backward_breaks`, `discontinuity_pids` | `pts-order`: a timeline known to restart, in total over the listed PIDs |

A key that excuses a rule needs its bound, its PID list where it has one,
and a `reason = "..."` line, or the annotation is refused; so is a bound
whose key excuses nothing.

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
one long-lived receiver process per path. Each receiver's memory is sampled
and recorded per path, not gated: the report notes whether the slope after
the soak's warm-up is within or above the soak's 200 KiB per hour threshold,
or that the run was too short for the rule. The 72-hour soak is the memory
instrument. A replay of a few unlike captures spends its first picks filling
each receiver's working set, and a slope rule over a run that short reads
that as growth.

The replay's time rules:

- A receiver that sees no byte for 10 seconds while the sender is still
  sending is stalled.
- UDP, RIST and the RTSP publisher path have no end of stream, so a file ends
  there after 2 seconds of quiet once the sender has finished. TCP and SRT
  end when the sender closes.
- A send may take the pick's own PCR seconds plus twice the stall time; one
  that takes longer is cancelled and the path's receiver is stopped.
- No pacing wait is longer than one second: a PCR reset, or a forward step
  of more than one second, is sent as no time. The replay prints per pick
  how many such gaps it did not pace, so a capture that went out faster than
  its clock is named. A replay that runs far past its budget is not waiting on
  the pacer; the 154-second overrun of the 0.9.0 release run was the RTSP
  publisher path blocked on a write.

A path whose receiver died, or was stopped for a stall, gets a fresh receiver
before the next pick, so a dead path costs one verdict, not the rest of the
run. A path is restarted at most once per pick. The restarted receiver
appends to its path's log, and the replay summary and the report list, per
path, the SHA of each pick a restart preceded. A path's memory slope after a
restart mixes two processes; it is recorded with the rest and, like them, not
gated.

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

Picks replay largest first, by the bytes each one sends, so the receiver's
largest allocation comes early in the run.

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

The release VM runs exactly this sequence, with the bucket sync before it and
the digest push after, through `scripts/interop/release-vm-corpus.sh`.

## How results are cited

The archive row holds eight counts: files, identical, changed and explained,
invariant failures, parse failures, replayed files, transport paths and failed
replays. A run without a replay is not a pass, and neither is a run whose
replay belongs to an earlier sweep: a new sweep removes the old replay's
results, and the report accepts a replay only when it recorded the sweep's
tree and the digests it ran against. The row is added to the archive
list that the release qualification reads.

A release qualification run requires a passing corpus archive for the exact
tree that was soaked, and its record carries a release-only line with those
eight counts. See [validation evidence](/docs/project/validation-evidence.md)
for how the record is published.
