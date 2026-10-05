# Tier-B golden reference outputs

This directory holds committed reference outputs against which
the maintainer's release-validation script (kept outside this repository)
diffs the output of external tools (`tsanalyze`, `tspsi`, `ffprobe`) run
against a freshly muxed baseline `.ts` file.

## Files

| Golden file | Source command | Tool |
|---|---|---|
| `baseline-tsanalyze.txt` | `tsanalyze --normalized baseline.ts` | tsduck |
| `baseline-tspsi.txt` | `tspsi --pat --pmt baseline.ts` | tsduck |
| `baseline-ffprobe.json` | `ffprobe -v error -show_streams -show_packets -of json baseline.ts` | ffmpeg |

The baseline is produced by `cargo run -p tst-examples --example mux_to_file -- baseline.ts 5`
using `MuxerConfig::default()` — see `examples/muxing/mux_to_file.rs`.

## Regenerating

Goldens MUST be regenerated whenever any of the following change:

- `MuxerConfig::default()` (PIDs, cadences, defaults)
- The `mux_to_file` example body
- TS-packet payload synthesis logic in `tst_core::mpegts::mux`

`release-validation.sh` is maintainer tooling that is not shipped in this
repository; it re-runs steps 3, 4, 5 (invoked with `--update-goldens`) with
stdout redirected into `ts-transformer/tests/golden/`, overwriting the
existing files. Review the diff (`git diff tests/golden/`) before
committing — every byte change must be intentional and explained in the commit
message.

## Why in-repo

The published repo deliberately doesn't depend on tsduck or ffmpeg at build time.
Goldens are committed as reference artifacts; verification needs the tools
installed locally. Anyone with the tools can run `./scripts/release-validation.sh`
to verify their build matches the committed reference.
