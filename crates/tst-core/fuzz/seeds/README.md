# Fuzz seeds

Tracked seed inputs that steer libFuzzer onto interesting code paths from
cold start. Each subdirectory matches a fuzz target name under
`fuzz_targets/`. Seeds are small, hand-built or extracted-from-fixture
byte sequences — they are NOT runtime corpus.

The runtime corpus lives under `crates/*/fuzz/corpus/<target>/` and is
gitignored. To populate `corpus/` from `seeds/` before a fuzz run:

```bash
bash scripts/dev/seed-fuzz-corpora.sh
```

The script is idempotent — it only copies seeds that aren't already in
the target corpus directory, so libFuzzer's accumulated runtime corpus
is preserved.

To add a new seed: drop the file under `seeds/<target>/<name>` and run
the script. Convention is small files (≤ a few KB) with descriptive
names hinting at what code path they exercise (e.g. `pat_real`,
`audio_pusi_rai`, `boundary_188`).

## Target directories

| Directory | Contents |
|---|---|
| `demux_feed/` | Regression-fixture-derived; aac_adts, aac_latm, ac3, mp2, subtitle_with_klv |
| `demux_latm_pes/` | AAC-LATM payloads: a sound LOAS stream joined mid-record, one whose record bodies are full of false syncwords, and two PIDs that are not LOAS (past-the-end candidates; chains of empty records ending in junk) |
| `demux_pes_reassembly/` | PES boundary / reassembly edge cases |
| `demux_psi/` | PAT/PMT real and malformed section headers |
| `klv_st0601_decode/` | Synthetic ST 0601 packets (minimal, full, funky UL, field errors) |
| `mpegts_au_cell_read/` | ITU-T H.222.0 §2.12.4.2 cell header variants (first, last, middle, single, empty) |
| `parse_av1_sequence_header/` | AV1 OBU sequence header structures |
| `audio_frame_iter/` | ADTS and MPEG-1/2 Layer II frame iterator seeds |
| `mux_pull/` | Minimal muxer pull-packet seed (16B) |
| `mux_push_klv/` | Minimal KLV push-to-muxer seed (16B) |
| `mux_push_video/` | Minimal video push-to-muxer seed (16B) |
| `parse_parameter_sets/` | H.264 and H.265 SPS minimal seeds |
