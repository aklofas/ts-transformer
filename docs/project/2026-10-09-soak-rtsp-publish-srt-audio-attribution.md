# One-hour publisher soak (2026-10-08): the SRT leg's failing verdicts

The one-hour soak run on 2026-10-08 was the first with the `rtsp-publish` leg. Its
harness verdict is `overall_pass=false`. The run's own `soak-results.json` was written
by the `report soak` of the tree the run was built from. It has 57 verdicts: 44 gating
PASS, 3 gating FAIL and 10 provisional PASS. A later re-judge of the same artefacts with
the current `report soak` gives 58 verdicts with the same three FAILs. It adds
`delivery_complete_rtsp-publish`, which passes on the digest from the run's
`source.sha256` artefact, because this run's configuration predates the declared digest. All three FAILs belong to the SRT leg's receiver, and they
have two causes. The `rtsp-publish` and RIST legs passed every verdict they own.

| Verdict | Cause |
| --- | --- |
| `recv_invariants_srt` | The receiver's final verify failed `audio_codec_adts` (section 1). |
| `worker_exits` | `srt-recv` exited 1 at the end of the run because of that same verify failure. |
| `zero_process_exits` | The RSS sampler recorded one empty reading for `srt-recv`, which the report counts as an exit (section 2). |

**Verdict: both causes are harness defects, not library or transport behaviour.** The
`audio_codec_adts` failure is an attribution gap. The tap deliberately corrupted the
byte the oracle compares, and that oracle has no path to excuse injected damage.
Confidence is high, about 95 %. The `zero_process_exits` failure is a sampling artefact,
and the process's CPU counters show it never stopped. Neither is a regression of the
publisher-role change set the run was built to exercise.

The run was built from the tree `v0.7.0-135-gb6545e60` with seed 1. The leg profiles were
`audio` on SRT, `av1-klv-b` on RIST and `baseline` on `rtsp-publish`. Rich KLV, realistic
access-unit sizes and the corruption tap were on, at rate 5 per 10 000 packets with
`min_gap` 1000.

## 1. `audio_codec_adts`: one injected flip on the first ADTS byte

### The oracle's rule

`tst-interop`'s raw reader (`crates/tst-interop/src/rawts.rs`, `Reader::pes`) stores the
first four payload bytes of the first PES on each PID. Every later PES on that PID whose
first two payload bytes differ from them increments `PesShape::prefix_mismatches`. For an
audio PID, `oracles::audio` (`crates/tst-interop/src/oracles.rs`) first checks that the
stored prefix is an ADTS header. It then fails with this text for any non-zero count:

```text
audio_codec_adts: 1 later PES payload(s) on PID 4161 do not start like the first
```

The rule is not too strict. The two compared bytes hold the 12-bit ADTS syncword, the
MPEG ID, the layer and `protection_absent`. All of them are constant in a legal ADTS
stream, and the generator writes `FF F9` at the start of every audio PES. The bytes that
vary from frame to frame, such as the frame length, are not compared.

### The offending injection

The receiver's report records only the count, which is 1. It does not record where the
mismatching PES was. The sender's corruption log identifies the PES instead.

The SRT leg's tap made 1597 injections, and 55 of them were on the audio PID 4161:

| Class | Count |
| --- | --- |
| `body_flip` | 20 |
| `header` | 16 |
| `drop` | 8 |
| `garbage` | 6 |
| `dup` | 3 |
| `truncate` | 2 |

In this stream an audio packet that starts a PES carries a 62-byte adaptation field. The
PES header therefore starts at byte 67 and occupies 14 bytes: `00 00 01 C0 00 73 80 80 05`
plus a 5-byte PTS. The ADTS header starts at byte 81.

Exactly one injection touches byte 81 or 82 of an audio packet:

```text
ordinal 2043709  class body_flip  pid 4161  offsets [81, 127]
coord pcr_base 139524480 (about 1550 s into the stream), since_pcr 1
bytes 67..87 before: 00 00 01 C0 00 73 80 80 05 21 21 43 F3 01 | FF F9 4C 80 0D 7F FC
bytes 67..87 after:  00 00 01 C0 00 73 80 80 05 21 21 43 F3 01 | 9A F9 4C 80 0D 7F FC
```

The flip turned the syncword's first byte from `0xFF` into `0x9A`. The other 54
injections on PID 4161 cannot change the compared bytes:

- **Other `body_flip`s:** the remaining 19 flip bytes at offset 85 or later, past the two
  compared bytes.
- **`header`:** 4 rewrite the adaptation-field length to a value above 183, so the raw
  reader rejects the packet and never parses its PES. 3 change only the continuity
  counter. 9 move the packet off PID 4161 or break its sync byte.
- **The rest:** `garbage` and `truncate` force a resync or lose the PES. `drop` loses
  it. `dup` repeats an intact PES, so the repeat starts with the legal `FF F9`.

One injection can produce a mismatch, and the report counts one mismatch.

### Why no excusal path exists

The leg's own corruption verdicts are clean: 1597 injected, 1597 resolved, 0 unresolved,
0 unexplained, 0 undetected and 0 unrecovered. A `body_flip` inside a PES payload is not
a class the demuxer is expected to detect, because the demuxer does not validate ADTS
payload bytes. The injection therefore produced no demux event to attribute. The only
check that saw it was the codec-payload oracle.

That oracle takes no attribution input. `oracles::audio(inventory, wire, seconds)` reads
the raw reader's totals after the capture ends, and nothing subtracts the mismatches an
injection explains. The sibling `av1_carriage_wire` prefix check works the same way.

The harness already excuses this kind of damage elsewhere. A rich ST 0601 record damaged
by an injection is skipped through `corrupt::Attribution::explains_damage(at, pid)` and
counted in `damaged_by_injection` (`crates/tst-interop/src/verify.rs`). The prefix check
was simply never connected to it.

### Classification and confidence

This is a harness attribution gap, with high confidence (about 95 %). An injection sits
on the exact byte the oracle compares, the injection count matches the mismatch count,
and no other injection on the PID can produce the symptom.

What would make it certain is the receiver's packet ordinal or PTS for each prefix
mismatch. The harness does not record it today. With it, the mismatch could be matched to
the tap position of ordinal 2043709, whose PES carries the PTS field `21 21 43 F3 01`.

### Not a regression of this change set

The later-PES prefix check dates from 2026-09-14. The change set this run was built to
exercise touched `oracles.rs` only to skip `audio()` for re-muxed captures. The SRT leg
judges the generator layout, so its call is unchanged.

The gap was latent. This is the first soak to draw the `audio` profile on a leg with the
corruption tap. The 0.7.0 release soak and the run before it drew `pts-rollover` for SRT
and `klv-sync` for RIST. Here one of the 20 `body_flip`s on the audio PID landed on the
two compared bytes, so a longer run on the `audio` profile would hit this repeatedly. The
`av1-klv-a` and `av1-klv-b` profiles' prefix check has the same exposure.

### Fix shape

1. `rawts::Reader::pes` reports each prefix mismatch with its packet ordinal and PID,
   for example as a bounded list drained like the resync list, instead of only
   incrementing a total.
2. The live receive loop asks `Attribution::explains_damage(at, pid)` for each mismatch.
   Explained mismatches go to a new `prefix_mismatches_excused` counter, and the rest stay
   in `prefix_mismatches`.
3. `oracles::audio` and the `av1_carriage_wire` check fail only on the unexcused count.
   They print the excused count in their detail, as `damaged_by_injection` is printed, so
   the relaxation is declared, never silent. Without an attached attribution the
   behaviour is unchanged.
4. Two red-first tests use one hand-built capture with byte 0 of an ADTS header flipped.
   With a matching injection the capture passes with 1 excused mismatch. Without the
   injection it still fails.

## 2. `zero_process_exits`: one empty RSS sample

The verdict reports `srt-recv` (PID 251092) as having left `/proc` at elapsed 1350 s. The
process did not leave.

- **`rss.csv`:** the row for that tick has an empty `rss_kb`. The rows on either side
  read 10 604 KiB.
- **`proc.csv`:** the same tick for the same PID records 6 threads and 5 open
  descriptors. User CPU ticks go 870, 904 and 934 across that tick and its neighbours.
- **The event log:** the process ran to the end of the run and exited at
  02:14:08Z with status 1, the `audio_codec_adts` failure above.

The sampler reads `VmRSS` and `Threads` from `/proc/<pid>/status` in a single pass.
Whatever made that pass miss the `VmRSS` line, it read `Threads` on the same tick, so
the file was readable. The cause of the missed line was not determined. The report treats
any row with an empty `rss_kb` as a process exit. It does not check the same tick's CPU,
thread or descriptor counts in `proc.csv`, nor whether later rows for the PID carry an RSS
again.

**Classification:** a harness sampling artefact. The fix is a rule change: count a
`(leg, process, pid)` as exited only when its RSS stays empty through the end of the run
and its `proc.csv` rows stop advancing. A single empty reading becomes a recorded sample
gap, which `rss_sample_coverage_*` already judges. That verdict passed here: 97 of 98
samples, largest gap 60 s.

## 3. What the run does establish

- **`rtsp-publish` leg:**
  - All 6 declared publisher generations ended on the mount. The run's own report
    judged them against a floor of 5, and the current report against 6.
  - The received stream's SHA-256 equals the source's, `7b3647a4…c136`. Every byte of all
    six publishers arrived in order.
  - The receiver counted 106 920 video access units and 35 640 rich KLV records, with
    0 discontinuities and 0 non-conformant events. The mount counted 0 malformed packets.
- **RIST leg:** it passed every gating verdict, including the six corruption verdicts.
  1466 injections were all resolved, and 0 events were unexplained.
- **SRT leg, apart from the two failures above:** it passed every corruption verdict,
  with 0 unexplained events among 1386. It also passed the drop-rate, schedule, profile,
  KLV and reconnect-mode declarations. The receiver counted 107 956 of 108 000 video
  access units.
- **RSS:** every process passed sample coverage. Slopes are provisional below 72 hours.
  The `rtsp-publish` receiver measured 92.4 KiB/h and the publisher loop 0.0 KiB/h.
