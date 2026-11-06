# Muxing

File-only muxer examples — no SRT, no transport, just `Muxer` →
`pull` → bytes on disk. Eight examples; the first four are a cumulative
codec progression, the last four are independent variations.

## Cumulative codec progression (read in order)

### 1. `mux_to_file.rs` — minimal H.264 + async-KLV → `.ts`

```sh
cargo run -p tst-examples --example mux_to_file -- /tmp/out.ts 5
```

The smallest "make a real `.ts` file" example. One H.264 video + one
async-KLV stream on one program. Plays under ffprobe / VLC / mpv. Use
this as the baseline before you add anything.

Cookbook: [Mux to a file](../../docs/cookbook/muxing/mux-to-file.md).

### 2. `mux_h265_with_klv.rs` — switch H.264 → H.265, plus sync KLV

```sh
cargo run -p tst-examples --example mux_h265_with_klv -- /tmp/h265.ts
```

Diff from §1: `VideoCodec::H265` instead of `H264`, plus
`KlvStreamType::SynchronousMetadata` (stream_type 0x15, KLV with PTS
in the PES header — auto-wrapped in a 5-byte AU cell header per
ITU-T H.222.0 V9 §2.12.4.2).

Cookbook: [Mux H.265 + sync KLV](../../docs/cookbook/muxing/mux-h265-with-klv.md).

### 3. `mux_h266_with_klv.rs` — H.265 → H.266 / VVC

```sh
cargo run -p tst-examples --example mux_h266_with_klv -- /tmp/h266.ts
```

Diff from §2: `VideoCodec::H266`, stream_type 0x33. KLV side identical.

Cookbook: [Mux H.266 / VVC video with synchronous KLV](../../docs/cookbook/muxing/mux-h266-with-klv.md).

### 4. `mux_av1_with_klv.rs` — H.266 → AV1

```sh
cargo run -p tst-examples --example mux_av1_with_klv -- /tmp/av1.ts
```

Diff from §3: `VideoCodec::Av1` (stream_type 0x06 with auto-emitted
`AV01` registration descriptor — the muxer takes care of it). The
elementary stream is OBU-framed instead of NAL-framed; same
`Muxer::push_video` API.

Cookbook: [Mux AV1 video with KLV](../../docs/cookbook/muxing/mux-av1-with-klv.md).

## Independent variations

### 5. `mux_audio_video_klv.rs` — adding audio

```sh
cargo run -p tst-examples --example mux_audio_video_klv -- /tmp/avk.ts
```

Add an audio stream alongside video + KLV. MP2 carriage in this example;
the API is the same for AAC ADTS / AAC LATM / AC-3 (swap `AudioCodec`).

Cookbook: [Mux audio + video + KLV in a single program](../../docs/cookbook/muxing/mux-audio-video-klv.md).

### 6. `mux_with_webvtt_subtitles.rs` — adding subtitles

```sh
cargo run -p tst-examples --example mux_with_webvtt_subtitles -- /tmp/subs.ts
```

Adds a WebVTT-in-TS subtitle stream. Subtitles share `stream_type` 0x06
with several other carriage forms (DVB-sub / DVB-teletext / CEA-708);
the right descriptor distinguishes them. The muxer auto-emits the
right descriptor based on `SubtitleCodec`.

### 7. `mux_dual_camera.rs` — multi-stream within one program

```sh
cargo run -p tst-examples --example mux_dual_camera
```

EO + IR camera pair sharing one PCR + KLV stream within a single
program. Demonstrates `push_video_to(handle, ...)` and the
`video_handles_for_program()` accessor.

Cookbook: [Label EO + IR + KLV streams in a multi-stream program](../../docs/cookbook/muxing/mux-eo-ir-klv.md).

### 8. `repack_two_programs.rs` — demux → remux round-trip

```sh
cargo run -p tst-examples --example repack_two_programs -- input1.ts input2.ts /tmp/combined.ts
```

Take two single-program input `.ts` files, demux both, mux a single
output with both programs side-by-side. Exercises the receive →
transmit round-trip without an SRT link.

Cookbook: [Repack two single-program inputs into one multi-program TS](../../docs/cookbook/muxing/repack-multi-program.md).
