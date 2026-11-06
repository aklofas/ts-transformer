# Codec parsing

Pull video / audio / subtitle parameter sets out of demuxed streams.
Six examples; the first three are progressively richer parsers, the
last three are extraction recipes that feed downstream tooling.

## 1. `parse_video_parameters.rs` — multi-codec sweep

```sh
cargo run -p tst-examples --example parse_video_parameters -- <file.ts>
```

Walks every video stream in a `.ts` file, dispatches on `VideoCodec`,
prints resolution + profile + level for H.264 and H.265 in one pass.
The "what's in this capture?" diagnostic.

Cookbook: [Extract video resolution and profile from a demuxed stream](../../docs/cookbook/codecs/extract-resolution-profile.md).

## 2. `parse_audio_frames.rs` — MP2 / AAC frame iteration

```sh
cargo run -p tst-examples --example parse_audio_frames -- path/to/some.ts
```

Diff from §1: audio side. Lazy stateless iterator over MPEG audio
(MP1 / MP2 / MP3 / MPEG-2 LSF / MPEG-2.5) and AAC ADTS frames; pulls
sample rate, channel count, frame size out of the headers.

Cookbook: [Pull sample rate and channel count out of an audio stream](../../docs/cookbook/codecs/extract-audio-format.md).

## 3. `extract_video_au.rs` — extract video AUs to disk

```sh
cargo run -p tst-examples --example extract_video_au -- <input.ts> [out_dir]
```

Diff from §1: instead of just printing parameters, write each access
unit to its own file. Useful when feeding a downstream decoder /
analyzer that wants Annex B or OBU bytes. Multi-codec: H.264 / H.265 /
H.266 / AV1 all handled.

## 4. `extract_h265_sps_to_rbsp.rs` — extract an HEVC SPS payload

```sh
cargo run -p tst-examples --example extract_h265_sps_to_rbsp -- input.265 > output.bin
```

Extract the first SPS, strip its Annex-B start code and two-byte NAL
header, and write its EBSP payload to stdout. Emulation-prevention bytes
are preserved; the H.265 `BitReader` skips them during parsing.

Cookbook: [Reconstitute Annex B parameter sets for decoder replay](../../docs/cookbook/codecs/reconstitute-annex-b.md).

## 5. `extract_h266_sps_to_rbsp.rs` — same recipe, H.266 / VVC

```sh
cargo run -p tst-examples --example extract_h266_sps_to_rbsp -- input.266 output.bin
```

Diff from §4: VVC-specific NAL header layout (two bytes, as in HEVC, with
the type in the second byte), an output-file argument instead of stdout,
and the same EP bytes preserved.

## 6. `demux_subtitle_file.rs` — subtitle codec discrimination

```sh
cargo run -p tst-examples --example demux_subtitle_file -- input.ts
```

DVB-sub / DVB-teletext / CEA-708 / WebVTT-in-TS all share
`stream_type` 0x06; the right descriptor distinguishes them. This
example walks every subtitle stream in a capture and reports which
codec each one carries.

Cookbook: [Extract subtitle PES bytes from a captured `.ts` file](../../docs/cookbook/receiving/extract-subtitle-pes.md).
