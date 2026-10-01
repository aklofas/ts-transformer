# OSS-Fuzz local verification log

## Run 2026-10-01 — pre-submission re-verification (current bundle)

**Tree:** `e86dd9ea` (main, cloned by the Dockerfile) · **Bundle:** the
`oss-fuzz/` files as submitted upstream (libFuzzer + AddressSanitizer only,
targeted recursive submodule init, toolchain via `RUSTUP_TOOLCHAIN`).
**Images:** `base-builder-rust@sha256:2d04d044…cd4cae8` (pinned
`nightly-2025-09-05`), `base-runner@sha256:29ffac3c…6405df`.
**Method:** the bind-mount-free sequence in `README.md` — `docker build`
of this directory, `compile` inside the builder container, `docker cp` of
`/out`, then the runner image's `test_all.py` and `run_fuzzer` fed by copy.
Same entry points `helper.py build_fuzzers` / `check_build` / `run_fuzzer`
invoke; only the transport of `/out` differs.

### compile

```
INFO: shipped 33/33 fuzz drivers to $OUT
```

A first attempt with a non-recursive submodule init failed in mbedTLS's
CMake configure (`No module named 'mbedtls_framework'` — the nested
`framework` submodule); the Dockerfile now passes `--recursive`.

### check_build (`test_all.py`) — PASS

All 33 drivers passed the bad-build checks (`test_all.py` exit 0).

### Artifact inventory in `/out` — 54 files

- 33 fuzz drivers (+ `llvm-symbolizer`)
- 13 `*_seed_corpus.zip`: audio_frame_iter, demux_feed, demux_pes_reassembly,
  demux_psi, klv_st0601_decode, mpegts_au_cell_read, mux_pull, mux_push_klv,
  mux_push_video, parse_av1_sequence_header, parse_parameter_sets, ts_parser,
  url_parse
- 4 `*.options`: demux_feed, demux_pes_reassembly, demux_psi, ts_parser
- 3 `*.dict`: klv_st0102_decode, klv_st0601_decode, klv_st0903_decode

### run_fuzzer smoke — 33/33 clean at 1000 iterations

| target | rc | cov | ft | corp |
|---|---|---|---|---|
| `audio_frame_iter` | 0 | 77 | 83 | 14/76b |
| `audio_syncframe` | 0 | 67 | 93 | 17/84b |
| `demux_feed` | 0 | 277 | 488 | 30/12974b |
| `demux_pes_reassembly` | 0 | 133 | 143 | 9/1801b |
| `demux_psi` | 0 | 84 | 85 | 8/94b |
| `h264_slice_header_light` | 0 | 295 | 402 | 45/128b |
| `h265_slice_header_light` | 0 | 88 | 149 | 30/78b |
| `h266_slice_header_light` | 0 | 30 | 31 | 2/9b |
| `klv_imapb` | 0 | 19 | 20 | 1/1b |
| `klv_st0102_decode` | 0 | 404 | 593 | 50/162b |
| `klv_st0601_decode` | 0 | 1178 | 2584 | 133/27Kb |
| `klv_st0601_patch` | 0 | 75 | 76 | 1/1b |
| `klv_st0605_decode` | 0 | 21 | 22 | 1/1b |
| `klv_st0806_decode` | 0 | 134 | 165 | 27/83b |
| `klv_st0903_decode` | 0 | 438 | 538 | 43/138b |
| `klv_st1010_decode` | 0 | 161 | 190 | 32/97b |
| `klv_st1204_decode` | 0 | 41 | 42 | 9/30b |
| `misp_sei_extract` | 0 | 53 | 67 | 19/91b |
| `mpegts_au_cell_read` | 0 | 34 | 35 | 4/21b |
| `mux_pull` | 0 | 451 | 453 | 5/12b |
| `mux_push_klv` | 0 | 487 | 488 | 1/1b |
| `mux_push_video` | 0 | 506 | 663 | 44/558b |
| `nal_framing` | 0 | 180 | 272 | 32/155b |
| `parse_av1_sequence_header` | 0 | 258 | 348 | 74/583b |
| `parse_parameter_sets` | 0 | 1330 | 2201 | 224/6308b |
| `rtp_h264_depacketize` | 0 | 161 | 168 | 9/75b |
| `rtp_packet_decode` | 0 | 22 | 23 | 1/1b |
| `rtsp_client_pump_framing` | 0 | 82 | 96 | 13/60b |
| `rtsp_message_decode` | 0 | 71 | 105 | 19/122b |
| `rtsp_request_decode` | 0 | 51 | 65 | 9/56b |
| `split_video` | 0 | 152 | 183 | 30/98b |
| `ts_parser` | 0 | 15 | 16 | 1/1b |
| `url_parse` | 0 | 299 | 402 | 62/1961b |

Deep pass: `demux_feed -runs=10000` clean —
`#10000 DONE cov: 298 ft: 671 corp: 89/51Kb rss: 52Mb`.

---

## Run 2026-05-15 (historical — 16-target bundle)

**Run:** 2026-05-15
**Reviewer:** andrew.klofas@gmail.com

> **Subsequent change (2026-05-24):** the `klv_iter` fuzz target was retired
> when `klv::pack::Iter` was tightened to `pub(crate)` — its coverage is
> provided transitively by `klv_st0601_decode` / `klv_st0102_decode` /
> `klv_st0903_decode`. Counts and inventory below are unchanged from the
> 2026-05-15 run; re-verification before the OSS-Fuzz PR ships will produce
> updated figures (expect 15 targets / 13 seed corpora / 3 dicts).

> **Subsequent change (2026-08-18):** the workspace fuzz inventory has since
> grown to **31 targets** (26 in `tst-core`, 4 in `tst-rtp`, 1 in `tst-srt`
> — ground truth: `tests/coverage/fuzz-targets.toml` /
> `find crates -path '*fuzz/fuzz_targets/*.rs'`). `build.sh` was updated the
> same day to build + bundle all three crates and to hard-fail on any
> shipped-driver count mismatch against that inventory (the old script built
> only tst-core + tst-srt and its final count line counted every `$OUT` file,
> corpora included). **This log's 2026-05-15 run is therefore STALE for the
> current bundle: the full `helper.py` build_image / build_fuzzers /
> check_build / run_fuzzer sequence MUST be re-run and recorded here before
> the upstream `google/oss-fuzz` PR is opened.** No upstream enrollment
> exists yet; nothing runs continuously on the OSS-Fuzz fleet today.

## Build method

Built with local source mount (required because Tasks 1-10 commits are not yet pushed to
GitHub — the Dockerfile clones from GitHub, so a `--mount_path` or `source_path` argument
is needed for pre-push local verification):

```
python3 infra/helper.py build_fuzzers --sanitizer address --clean \
  ts-transformer /home/aklofas/Projects/ts-transformer/ts-transformer
```

Build reported: `INFO: shipped 39 fuzz drivers to $OUT`

## check_build

```
INFO: performing bad build checks for demux_psi
INFO: performing bad build checks for demux_pes_reassembly
INFO: performing bad build checks for audio_frame_iter
INFO: performing bad build checks for klv_st0903_decode
INFO: performing bad build checks for url_parse
INFO: performing bad build checks for klv_st0601_decode
INFO: performing bad build checks for mux_pull
INFO: performing bad build checks for mux_push_klv
INFO: performing bad build checks for mpegts_au_cell_read
INFO: performing bad build checks for klv_iter
INFO: performing bad build checks for mux_push_video
INFO: performing bad build checks for klv_st0102_decode
INFO: performing bad build checks for parse_parameter_sets
INFO: performing bad build checks for demux_feed
INFO: performing bad build checks for ts_parser
INFO: performing bad build checks for parse_av1_sequence_header
INFO:__main__:Check build passed.
```

All 16 targets check_build = PASS.

## run_fuzzer smoke (1k iters each)

14 of 16 targets ran 1000 libFuzzer iterations without crash. 2 targets surfaced issues:

- **demux_psi**: OOB slice at `psi.rs:90` — real library bug. See "Known issues" below.
- **klv_st0903_decode**: round-trip assertion failure — harness logic mismatch with plan #46 encode semantics, NOT a library bug. See "Known issues" below.

`demux_feed` ran 10,000 iterations without crash — satisfies spec acceptance criterion #4:

```
#2	  INITED cov: 141 ft: 142 corp: 1/1710b exec/s: 0 rss: 31Mb
#10000	DONE   cov: 279 ft: 596 corp: 65/66Kb lim: 1710 exec/s: 0 rss: 50Mb
```

## Artifact inventory in $OUT/

Built with local source. Counts confirmed by Docker container inspection:

- 16 fuzz driver binaries (+ llvm-symbolizer = 17 executables)
- 14 `*_seed_corpus.zip` files
  - fixture-derived: demux_feed, demux_pes_reassembly, demux_psi, klv_st0601_decode, ts_parser
  - synthetic: audio_frame_iter, klv_iter, mpegts_au_cell_read, mux_pull, mux_push_klv,
    mux_push_video, parse_av1_sequence_header, parse_parameter_sets, url_parse
  - intentionally absent: klv_st0102_decode, klv_st0903_decode (no seeds committed)
- 4 `*.options` files: demux_feed, demux_pes_reassembly, demux_psi, ts_parser
- 4 `*.dict` files: klv_iter, klv_st0102_decode, klv_st0601_decode, klv_st0903_decode
- Plus llvm-symbolizer (OSS-Fuzz infrastructure helper)

Total: 39 files in $OUT.

## Resolved issues — fixed in plan #54

Both bugs surfaced by plan #53's local 1k smoke pass have been fixed.
The next local OSS-Fuzz fleet rebuild produces no crashes across the
16 targets' 1k smoke runs.

### 1. `demux_psi`: `parse_pat` / `parse_pmt` OOB on `section_length < 4`

- **Was:** `&section[..total_len - 4]` underflowed `usize` subtraction when
  `section_length < 4` (because `total_len = 3 + section_length` was then 3,
  4, 5, or 6, with `total_len - 4` underflowing for `section_length = 0`).
- **Fix:** New `PsiParseError::SectionTooShort` variant + early guards in
  both `parse_pat` (min section_length = 9) and `parse_pmt` (min = 13).
- **Tests:** Four new unit tests in `crates/tst-core/src/mpegts/demux/psi.rs`
  pin the new behavior at the boundary (section_length = 0 and at min - 1).

### 2. `klv_st0903_decode` harness: round-trip vs. plan-#46 Tag-1 drop

- **Was:** Harness asserted `decoded_a == decoded_b` after `decode → encode → decode`,
  but `klv::st0903::encode` deliberately drops Tag 1 (checksum) per plan #46,
  so a Tag-1-containing input would always trip the assert.
- **Fix:** Normalize `.checksum = None` on both sides before the equality
  comparison.
- **Not a library bug.** Production decode/encode semantics are unchanged.
