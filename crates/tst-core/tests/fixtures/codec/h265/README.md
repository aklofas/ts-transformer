# H.265 codec fixtures

Generated from real x265 output using `extract_h265_sps_to_rbsp`. The
SPS/VPS/PPS bytes are EBSP (NAL header stripped, emulation-prevention bytes
preserved) — the same format the H.265 `BitReader` accepts.

Note: x265 encodes short-term reference pictures in slice headers, not in
the SPS `short_term_ref_pic_sets` list. The RPS walker is exercised by the
synthetic `build_synthetic_sps_with_*` helpers in `mod.rs::sps_tests`, not by
these fixtures.

## To regenerate

```bash
ffmpeg -y -f lavfi -i "color=c=black:s=1920x1080:r=25:d=2.0" \
    -c:v libx265 -profile:v main -preset ultrafast \
    -x265-params "aud=0:level-idc=120:no-info=1:repeat-headers=1:keyint=25" \
    -frames:v 3 -f hevc /tmp/hevc_main40.h265
cargo run --quiet --example extract_h265_sps_to_rbsp -p tst-core -- /tmp/hevc_main40.h265 \
    > tests/fixtures/codec/h265/h265_1080p_main40_sps.bin

ffmpeg -y -f lavfi -i "color=c=black:s=1920x1080:r=50:d=2.0,format=yuv420p10le" \
    -c:v libx265 -profile:v main10 -pix_fmt yuv420p10le -preset ultrafast \
    -x265-params "aud=0:level-idc=50:no-info=1:repeat-headers=1:keyint=50:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc" \
    -frames:v 3 -f hevc /tmp/hevc_main10_50_pq.h265
cargo run --quiet --example extract_h265_sps_to_rbsp -p tst-core -- /tmp/hevc_main10_50_pq.h265 \
    > tests/fixtures/codec/h265/h265_1080p_main10_50_pq_sps.bin
```

After regenerating, update assertions in `mod.rs::sps_tests` that reference
`general_profile_compatibility_flags` or `general_profile_idc` — these are
encoder-version-sensitive.
