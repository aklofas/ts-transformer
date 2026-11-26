# Codec parameter-set test fixtures

Each `.bin` file is one parameter-set NAL's RBSP body — the bytes after
the codec's NAL header (1 byte for H.264, 2 bytes for H.265) and after
any Annex B start code. Emulation prevention bytes (`00 00 03`) are
preserved, matching what `NalUnit::H264 { payload }` and
`NalUnit::H265 { payload }` ship from the demuxer.

Each fixture's expected parsed values come from the encoder parameters:

| Fixture | Resolution | Profile | Level | Bit depth | Chroma | Color |
|---|---|---|---|---|---|---|
| `h264/h264_1080p_high40_bt709_*` | 1920×1080 | High (100) | 4.0 (40) | 8 | 4:2:0 | BT.709 |
| `h264/h264_720p_main31_*` | 1280×720 | Main (77) | 3.1 (31) | 8 | 4:2:0 | (default) |
| `h265/h265_1080p_main40_*` | 1920×1080 | Main (1) | 4.0 (120) | 8 | 4:2:0 | (default) |
| `h265/h265_1080p_main10_50_pq_*` | 1920×1080 | Main 10 (2) | 5.0 (150) | 10 | 4:2:0 | BT.2020 + PQ |

Note H.265 `general_level_idc` is in units of 30 × level (so level 4.0
is `level_idc=120`, level 5.0 is `level_idc=150`).

`h266/h266_320x240_vvenc_vui_sps.bin` is the RBSP of an H.266 SPS that
carries a VUI (2-byte NAL header stripped, emulation-prevention bytes
kept). VVenC 1.14.0 wrote it from three frames of 8-bit 4:2:0 input (their
content does not change the SPS) with `vvencFFapp --SourceWidth=320
--SourceHeight=240 --InputBitDepth 8 --FrameRate=30 --FramesToBeEncoded=3
-vui 1 --AspectRatioInfoPresent 1 --AspectRatioIdc 2
--ColourDescriptionPresent 1 --ColourPrimaries bt709
--TransferCharacteristics bt709 --MatrixCoefficients bt709 --VideoFullRange
1 --ChromaLocInfoPresent 1`. Its values (320×240, 30/1, BT.709, full range,
chroma location 0, SAR 12:11) are what `real_vvenc_sps_with_vui_parses`
expects. `_regen.sh` does not produce it.

To regenerate any of these fixtures, run `./_regen.sh` in this
directory. The script extracts SPS/PPS (and VPS for H.265) RBSP
bytes from FFmpeg-produced encoded streams.
