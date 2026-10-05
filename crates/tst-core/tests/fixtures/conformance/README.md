# Codec conformance parameter-set fixtures

Each `.bin` file is one parameter-set body extracted from an official
codec conformance bitstream:

- H.264 / AVC: ITU-T JVT-AVC draft_conformance (AVCv1 and FRExt sub-trees)
- H.265 / HEVC: ITU-T JCT-VC HEVC_v1 and RExt draft_conformance sub-trees
- H.266 / VVC: ITU-T JVET draft_conformance/draft6 sub-tree
- AV1: AOMedia test vectors (storage.googleapis.com/aom-test-data)

The `.bin` is the RBSP body (NAL header stripped, emulation prevention
bytes preserved) for h264/h265/h266, or the OBU payload (OBU header
stripped) for AV1 — matching what the parsers in `tst_core::codec`
accept directly.

The accompanying `.json` sidecar declares the source vector's URL and
sha256 (of the upstream archive, not the stripped fixture) and the
expected parser outcome. See `crates/tst-core/tests/conformance.rs`
for how the test runner consumes them.

## Starter fixture set

The starter set holds these 11 parameter-set fixtures:

| Codec | Fixture | Source vector | Profile | Level | Bit depth | Chroma | Notes |
|---|---|---|---|---|---|---|---|
| H.264 | `h264/MR1_BT_A.bin` | MR1_BT_A.h264 (JVT-AVC AVCv1) | Baseline (66) | 1.1 | 8 | 4:2:0 | |
| H.264 | `h264/MR9_BT_B.bin` | MR9_BT_B.h264 (JVT-AVC AVCv1) | Main (77) | 2.1 | 8 | 4:2:0 | |
| H.264 | `h264/HCAFR1_HHI.bin` | HCAFR1_HHI.264 (JVT-AVC FRExt) | High (100) | 3.0 | 8 | 4:2:0 | |
| H.265 | `h265/AMVP_A_MTK_4.bin` | AMVP_A_MTK_4.bit (JCT-VC HEVC_v1) | Main (1) | 4.0 (level_idc=120) | 8 | 4:2:0 | |
| H.265 | `h265/DBLK_A_MAIN10_VIXS_4.bin` | DBLK_A_MAIN10_VIXS_4.bit (JCT-VC HEVC_v1) | Main 10 (2) | 4.0 (level_idc=120) | 10 | 4:2:0 | **Currently skipped** — known parser bug in `short_term_rps` (see `KNOWN_PARSER_BUGS` in `tests/conformance.rs`) |
| H.265 | `h265/QMATRIX_A_RExt_Sony_1.bin` | QMATRIX_A_RExt_Sony_1.bit (JCT-VC RExt) | Main 4:4:4 (4) | n/a | n/a | n/a | Deliberate `EngineError` bail — `sps_scaling_list_data_present_flag=1` triggers unimplemented scaling-list walk (H.265 §7.3.4) |
| H.266 | `h266/8b420_A_Bytedance_2.bin` | 8b420_A_Bytedance_2.bit (JVET draft6) | Main 10 (1) | 3.1 (level_idc=51) | 8 | 4:2:0 | |
| H.266 | `h266/10b400_A_Bytedance_2.bin` | 10b400_A_Bytedance_2.bit (JVET draft6) | Main 10 (1) | 3.1 (level_idc=51) | 10 | Monochrome | |
| H.266 | `h266/8b444_A_Kwai_2.bin` | 8b444_A_Kwai_2.bit (JVET draft6) | Main 4:4:4 10 (33) | 6 (level_idc=102) | 8 | 4:4:4 | Profile name "10" caps max bit depth — vector itself is 8-bit |
| AV1 | `av1/av1-1-b8-00-quantizer-00.bin` | av1-1-b8-00-quantizer-00.ivf (AOMedia GCS) | 0 (Main) | n/a | 8 | 4:2:0 | |
| AV1 | `av1/av1-1-b10-00-quantizer-00.bin` | av1-1-b10-00-quantizer-00.ivf (AOMedia GCS) | 0 (Main) | n/a | 10 | 4:2:0 | |

H.265 `general_level_idc` is in units of 30 × level (level 4.0 = 120). H.266
`general_level_idc` is per H.266 Table A.3 (level 3.1 = 51, level 6 = 102).
AV1 test vectors from the AOM GCS bucket do not embed level in the sequence
header for these quantizer-sweep clips; see the `.json` sidecar for the full
expected field set.

## Layout

```
conformance/
  manifest.toml          # hand-curated list of upstream vectors + expected fields
  README.md              # this file
  .gitignore             # excludes _cache/
  _cache/                # gitignored; the strip tool saves downloads here
    .gitkeep
  h264/                  # committed .bin + .json pairs for H.264
  h265/                  # committed .bin + .json pairs for H.265
  h266/                  # committed .bin + .json pairs for H.266
  av1/                   # committed .bin + .json pairs for AV1
```

## Regenerating

The committed `.bin` files are derived from `manifest.toml`. To regenerate:

From the workspace root, run:

```
cargo run -p tst-core --bin strip-conformance-parameter-sets
```

The tool downloads each upstream archive into `_cache/` (gitignored),
verifies sha256, extracts the named bitstream, scans for parameter-set
NAL/OBU units, and writes `.bin` + `.json` pairs. Reruns are idempotent
when `_cache/` is warm.

## Adding vectors

1. Pick a conformance bitstream from the upstream sources listed above.
2. Download the archive, run `sha256sum`, capture the hash.
3. Add an entry to `manifest.toml` with the URL, sha256, inner filename,
   `kind`, and `expected` fields.
4. Run `cargo run -p tst-core --bin strip-conformance-parameter-sets` to
   produce the `.bin` and `.json` sidecar.
5. Commit the new `.bin` + `.json` files.

## Licensing

The committed fixtures are tiny derived works (parameter-set bytes only,
~50-500 bytes each) from publicly-distributed conformance test sets
maintained by ITU-T / ISO/IEC SCs and AOMedia. The full conformance
vectors are NOT redistributed by this repo — only the parameter-set
bytes are committed, and only for the purpose of testing this codebase's
parsers. See `manifest.toml` for upstream URLs.
