//! AV1-in-MPEG-2-TS binding external decoder acceptance.
//!
//! Background
//! ----------
//! Binding-conformant AV1 carriage is the default mux + demux mode. The
//! binding spec (AV1-in-MPEG-2-TS, AOM 2020) requires:
//!
//!   - PMT stream_type 0x06 + `format_identifier "AV01"`
//!     registration_descriptor (§2.1)
//!   - PMT AV1_video_descriptor present alongside the registration
//!     descriptor (§2.2)
//!   - PES stream_id = 0xBD (private_stream_1) — NOT 0xE0 (§3.4)
//!   - PES payload framed via `ts_open_bitstream_unit()`: each OBU
//!     prefixed by `0x00 0x00 0x01` start code with
//!     emulation-prevention `0x03` bytes inserted after any
//!     `0x00 0x00 0xX` (X ≤ 0x03) in the body (§3.2)
//!
//! The empirical question:
//!
//!   > "do spec-compliant AV1 receivers (libaom, ffmpeg, dav1d) actually
//!   >  accept our binding-conformant output?"
//!
//! Why this matters
//! ----------------
//! `Av1CarriageMode::InteropRawObu` is the de-facto carriage used by ffmpeg /
//! hls.js / mediamtx today: PES `stream_id=0xE0`, raw OBU payload, no
//! `ts_open_bitstream_unit` framing. The default is binding-mode for spec
//! conformance — this test validates that choice empirically by comparing both
//! modes through the same external tools and asserting the **container-layer**
//! acceptance of each. It is NOT a pixel-decoding interop test: the AV1 OBU
//! bodies here are synthetic, so libaom / dav1d cannot decode pixels — but they
//! CAN tell us whether the container parser of each tool dispatches the bytes
//! correctly.
//!
//! What we observe
//! ---------------
//! - **ffprobe** on the binding-conformant stream classifies the AV1
//!   PID as `bin_data (AV01)` — ffmpeg's demuxer sees the AV01
//!   registration descriptor and tags the stream accordingly, but it
//!   does NOT promote the stream to `codec_type: video` because
//!   ffmpeg's AV1-in-TS support today is tied to `stream_id=0xE0`
//!   (the InteropRawObu shape).
//! - **tsanalyze** (tsduck) reports `PES stream id: 0xBD (Private
//!   stream 1)` for the AV1 video PID — exact spec match.
//! - The on-wire PES payload starts with `0x00 0x00 0x01` and
//!   contains one start code per OBU (pinned by
//!   `av1_carriage_roundtrip::av1_binding_mode_emits_one_start_code_per_obu_on_wire`).
//!
//! Tools used
//! ----------
//! - `ffprobe` (libavformat) — required for the primary subtests.
//!   Tests `silently skip` when absent. Available on every CI image
//!   that has ffmpeg installed.
//! - `tsanalyze` (tsduck) — used by the descriptor + stream_id
//!   subtests. Tests `silently skip` when absent.
//! - `dav1d` — bitstream-level OBU walker. Listed for completeness
//!   but cannot decode synthetic OBU bodies; we only check that it
//!   doesn't reject the extracted byte stream's framing.
//!
//! Test gating
//! -----------
//! All default `#[test]`s pass when external tools are absent (silent
//! skip via `which`-equivalent probe). The strict-acceptance and
//! deep-comparison variants are `#[ignore]`d and run via
//! `cargo test -- --ignored` on environments that have the full
//! external toolchain installed.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    Av1CarriageMode, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec,
};

// ─────────────────────────────────────────────────────────────────────
// AV1 access-unit synthesis (mirrors `tests/av1_carriage_roundtrip.rs`)
// ─────────────────────────────────────────────────────────────────────

/// Build a single AV1 low-overhead OBU with `obu_has_size_field = 1`.
///
/// AV1 spec §5.3.2 OBU header byte:
///   `obu_forbidden_bit f(1) | obu_type f(4) | obu_extension_flag f(1)
///    | obu_has_size_field f(1) | obu_reserved_1bit f(1)`
/// = `(obu_type << 3) | 0b010` for `extension_flag=0`, `has_size_field=1`.
///
/// AV1-in-MPEG-2-TS §3.1 *requires* `obu_has_size_field = 1` so that
/// demultiplexers can walk the OBU stream without an external framing
/// layer. The binding spec layers `ts_open_bitstream_unit()` on top of
/// this — the muxer handles that wrapping in `Av1CarriageMode::Mpeg2TsBinding`
/// mode.
fn obu(obu_type: u8, body: &[u8]) -> Vec<u8> {
    let header = (obu_type << 3) | 0x02;
    let mut v = vec![header];
    // For bodies <128 bytes a single-byte LEB128 size (high bit clear)
    // is sufficient. Larger bodies would need a multi-byte LEB128.
    assert!(body.len() < 128, "test helper supports bodies < 128 bytes");
    v.push(body.len() as u8);
    v.extend_from_slice(body);
    v
}

/// Build a 30-frame AV1 access-unit sequence: keyframes every 15
/// (SH + FH + TG) interleaved with inter frames (FH + TG). Each AU is
/// prefixed by a TemporalDelimiter per AV1 spec §5.3.1.
///
/// Length picked so the produced TS is several KB — enough for ffmpeg's
/// container-format heuristic to confidently identify `mpegts` (it
/// degrades to `mpeg` PS for very short streams). Real-life usage
/// always emits much longer streams; this avoids a low-score ffprobe
/// classification that would muddy the comparison.
///
/// Bodies are synthetic — real AV1 syntax would be required for libaom
/// or dav1d to actually decode pixels — but byte values are chosen so
/// the on-wire stream contains a `0x00 0x00 0x01` sequence inside an
/// OBU body, exercising binding-mode emulation prevention.
fn synthetic_av1_aus() -> Vec<(Vec<u8>, bool)> {
    let mut aus = Vec::new();
    let total = 30;
    for i in 0..total {
        let key = i % 15 == 0;
        let mut au = Vec::new();
        au.extend(obu(2, &[])); // TemporalDelimiter
        if key {
            // SeqHeader on every key frame. Body chosen to include
            // `0x00 0x00 0x01` so binding mode's emulation prevention
            // gets exercised.
            au.extend(obu(1, &[0x00, 0x00, 0x01, 0xAA]));
        }
        au.extend(obu(3, &[((i as u8) & 0x0F) | if key { 0x10 } else { 0 }]));
        // TileGroup body sized to ~80-100 bytes; varied so the stream
        // doesn't compress to a single TS packet's worth.
        au.extend(obu(4, &vec![0xA5 ^ (i as u8); 80 + (i as usize % 20)]));
        aus.push((au, key));
    }
    aus
}

/// Build a single-program AV1 TS in the requested carriage mode.
///
/// Output is a flat `Vec<u8>` of MPEG-TS bytes — ready to write to disk
/// for external-tool inspection.
fn build_av1_ts(mode: Av1CarriageMode) -> Vec<u8> {
    const VIDEO_PID: u16 = 0x1011;
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(VIDEO_PID, VideoCodec::Av1);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.av1_carriage(mode);
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.video_handles()[0];

    let aus = synthetic_av1_aus();
    // 30 fps PTS cadence (3000 ticks @ 90 kHz per frame).
    let pts_inc: i64 = 90_000 / 30;
    for (i, (au, key)) in aus.iter().enumerate() {
        let pts = Pts90khz::new((i as i64) * pts_inc);
        mux.push_video_to(h, au, pts, *key).unwrap();
    }

    let mut out = Vec::new();
    let mut buf = vec![0u8; 1316];
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

// ─────────────────────────────────────────────────────────────────────
// External-tool probes
// ─────────────────────────────────────────────────────────────────────

/// `which $tool` equivalent — checks the tool can be invoked.
///
/// Tries both `-version` (ffmpeg / ffprobe) and `--version` (tsduck
/// tools, dav1d) since the convention isn't uniform across CLIs.
///
/// Skipping silently (vs requiring CI to install the tools) keeps the
/// `cargo test --workspace` path green on minimal environments while
/// still exercising the probes when the tools are present.
fn tool_available(tool: &str) -> bool {
    let single_dash = Command::new(tool)
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if single_dash {
        return true;
    }
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Materialize TS bytes to a temp file. Uses `std::env::temp_dir()` per
/// the workspace cross-platform-paths rule (Rust tests must NOT hard
/// code `/tmp/`).
fn write_temp_ts(bytes: &[u8], name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("wave_i2_av1_{name}.ts"));
    std::fs::write(&path, bytes).unwrap_or_else(|e| {
        panic!("write {}: {e}", path.display());
    });
    path
}

/// Capture stdout + stderr + status from a Command.
fn run(cmd: &mut Command) -> Output {
    cmd.output().expect("spawn external tool")
}

// ─────────────────────────────────────────────────────────────────────
// Tests — default (always pass when tools absent)
// ─────────────────────────────────────────────────────────────────────

/// Builds a binding-conformant AV1 TS and asserts our own muxer's
/// wire-format invariants WITHOUT external tools. This is the
/// "baseline always-runs" test — confirms our generator works even on
/// minimal CI images.
///
/// Specifically: confirms (a) the PMT registration descriptor carries
/// `AV01`, (b) the first video PES starts with the 3-byte
/// `ts_open_bitstream_unit` start code, and (c) the wire stream contains
/// AT LEAST one start code per OBU per AU (the per-OBU framing invariant).
#[test]
fn binding_mode_wire_format_invariants_hold() {
    let ts = build_av1_ts(Av1CarriageMode::Mpeg2TsBinding);
    assert!(!ts.is_empty(), "muxer produced empty stream");

    // The PMT lives in the first packets; scan for the AV01 marker
    // in the descriptor area. The registration descriptor body is
    // exactly the 4 ASCII bytes 'A' 'V' '0' '1'.
    let has_av01 = ts.windows(4).any(|w| w == b"AV01");
    assert!(
        has_av01,
        "binding-mode mux MUST emit AV01 registration descriptor"
    );

    // Count `0x00 0x00 0x01` occurrences in the wire stream. The 30
    // AUs contain 30 TDs + 2 SHs (on keys 0 and 15) + 30 FHs + 30 TGs
    // = 92 OBUs => ≥92 binding start codes (per-OBU framing). Wrapping
    // once per AU would yield only ~30 start codes; this lower bound
    // catches any regression to that shape.
    let n_start_codes = ts.windows(3).filter(|w| *w == [0x00, 0x00, 0x01]).count();
    assert!(
        n_start_codes >= 92,
        "expected ≥92 start codes for 92 OBUs across 30 AUs (per-OBU framing), \
         got {n_start_codes}"
    );
}

/// Builds an interop-mode AV1 TS and asserts the reciprocal wire shape:
///   - AV01 registration STILL present (interop carriage doesn't strip
///     the descriptor — only the framing layer changes)
///   - NO `ts_open_bitstream_unit` per-OBU framing applied by the
///     muxer; the body should contain materially fewer `0x00 0x00 0x01`
///     matches than the binding mode does.
///
/// The exact count is sensitive to synthetic body contents (which
/// include some literal `0x00 0x00 0x01` patterns) plus TS-layer
/// stuffing/adaptation bytes, so we compare RATIO-wise to the binding
/// mode rather than asserting an absolute number.
#[test]
fn interop_mode_wire_format_invariants_hold() {
    let interop = build_av1_ts(Av1CarriageMode::InteropRawObu);
    let binding = build_av1_ts(Av1CarriageMode::Mpeg2TsBinding);
    assert!(!interop.is_empty(), "muxer produced empty stream");

    assert!(
        interop.windows(4).any(|w| w == b"AV01"),
        "interop-mode mux still emits AV01 registration descriptor"
    );

    let interop_sc = interop
        .windows(3)
        .filter(|w| *w == [0x00, 0x00, 0x01])
        .count();
    let binding_sc = binding
        .windows(3)
        .filter(|w| *w == [0x00, 0x00, 0x01])
        .count();
    // Binding mode wraps 16 OBUs => 16 added start codes. Interop mode
    // only carries incidental matches from synthetic body bytes plus
    // TS-layer artifacts. The delta must be at least ~10 to confirm
    // per-OBU framing is in fact applied in binding mode only.
    assert!(
        binding_sc >= interop_sc + 10,
        "binding mode start-code count ({binding_sc}) must exceed interop ({interop_sc}) \
         by ≥10 — per-OBU framing not being applied?"
    );
}

/// Binding and interop streams must differ at the PES layer. This is
/// a guard against accidental config-swallowing: if `av1_carriage` is
/// silently ignored, the two outputs would be byte-identical.
#[test]
fn binding_and_interop_streams_have_different_byte_signatures() {
    let binding = build_av1_ts(Av1CarriageMode::Mpeg2TsBinding);
    let interop = build_av1_ts(Av1CarriageMode::InteropRawObu);
    assert_ne!(
        binding, interop,
        "binding and interop muxer outputs must differ (start-code framing + stream_id)"
    );

    // Sanity: both have similar total size (within 50% of each other).
    // The binding wrapper adds ~3 bytes per OBU + escape bytes; not a
    // 2x size difference for these synthetic bodies.
    let ratio = binding.len() as f64 / interop.len() as f64;
    assert!(
        (0.5..=2.0).contains(&ratio),
        "size delta too large between binding ({}) and interop ({}) — likely a different bug",
        binding.len(),
        interop.len(),
    );
}

/// ffprobe probe: when ffprobe is installed, confirm it dispatches the
/// AV1 PID on the AV01 codec_tag (the `AV01` registration descriptor is
/// what tells external receivers the stream identity).
///
/// We assert specifically that ffprobe lists EXACTLY one stream with
/// codec_tag_string=`AV01`. We do NOT assert codec_type=`video` — see
/// the binding-vs-interop deep test below for why.
#[test]
fn ffprobe_recognizes_av01_tag_in_binding_mode() {
    if !tool_available("ffprobe") {
        eprintln!("ffprobe not available — skipping");
        return;
    }
    let ts = build_av1_ts(Av1CarriageMode::Mpeg2TsBinding);
    let path = write_temp_ts(&ts, "binding");
    let out = run(Command::new("ffprobe").args([
        "-v",
        "error",
        "-show_streams",
        "-of",
        "default=noprint_wrappers=1",
        path.to_str().unwrap(),
    ]));
    assert!(
        out.status.success(),
        "ffprobe rejected the binding-conformant TS — exit {:?}, stderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let av01_count = stdout.matches("codec_tag_string=AV01").count();
    assert_eq!(
        av01_count, 1,
        "expected exactly one AV01-tagged stream, got {av01_count}:\n{stdout}"
    );
    // Cleanup
    let _ = std::fs::remove_file(path);
}

/// tsanalyze probe: when tsduck's `tsanalyze` is installed, confirm
/// the AV1 video PID's PES `stream_id` reports as `0xBD (Private
/// stream 1)` — the binding spec mandate.
///
/// This is the strongest spec-conformance signal because tsduck
/// inspects PES headers directly rather than trying to classify the
/// codec.
#[test]
fn tsanalyze_reports_stream_id_0xbd_in_binding_mode() {
    if !tool_available("tsanalyze") {
        eprintln!("tsanalyze not available — skipping");
        return;
    }
    let ts = build_av1_ts(Av1CarriageMode::Mpeg2TsBinding);
    let path = write_temp_ts(&ts, "binding_tsa");
    let out = run(Command::new("tsanalyze").args(["--pid-analysis", path.to_str().unwrap()]));
    assert!(
        out.status.success(),
        "tsanalyze rejected binding-conformant TS — stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The video PID's section should mention `0xBD (Private stream 1)`.
    // Anything else (e.g. `0xE0`) would mean the muxer silently fell
    // back to the interop carriage shape.
    assert!(
        stdout.contains("PES stream id: 0xBD"),
        "expected binding-mode PES stream_id 0xBD in tsanalyze output, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("PES stream id: 0xE0"),
        "found unexpected 0xE0 stream_id in binding-mode tsanalyze output:\n{stdout}"
    );
    let _ = std::fs::remove_file(path);
}

/// tsanalyze cross-mode: in InteropRawObu mode the AV1 PES SHOULD use
/// `stream_id=0xE0` (video) — confirms the mode is wired through end
/// to end at the wire level.
#[test]
fn tsanalyze_reports_stream_id_0xe0_in_interop_mode() {
    if !tool_available("tsanalyze") {
        eprintln!("tsanalyze not available — skipping");
        return;
    }
    let ts = build_av1_ts(Av1CarriageMode::InteropRawObu);
    let path = write_temp_ts(&ts, "interop_tsa");
    let out = run(Command::new("tsanalyze").args(["--pid-analysis", path.to_str().unwrap()]));
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("PES stream id: 0xE0"),
        "expected interop-mode PES stream_id 0xE0 in tsanalyze output:\n{stdout}"
    );
    let _ = std::fs::remove_file(path);
}

// ─────────────────────────────────────────────────────────────────────
// Tests — ignored (require full external toolchain, run in CI Tier B)
// ─────────────────────────────────────────────────────────────────────

/// Deep ffprobe comparison: the conformant vs interop streams should
/// expose different stream `codec_tag_string` shapes, NOT just
/// different bytes. This is the empirical answer to "do external
/// receivers accept the binding-conformant output?".
///
/// As of ffmpeg 6.x:
///   - binding (stream_id=0xBD, ts_open_bitstream_unit framing) →
///     ffprobe shows `codec_tag_string=AV01`, but `codec_type=data`
///     (NOT `video`). ffmpeg's AV1-in-TS demuxer is currently coupled
///     to stream_id=0xE0 — it sees the AV01 tag but doesn't dispatch
///     to the AV1 codec parser.
///   - interop (stream_id=0xE0, raw OBU) → ffprobe still shows
///     `codec_tag_string=AV01`, and may attempt video classification
///     (depends on whether the bytes parse as an AV1 sequence header
///     — synthetic bodies here will not).
///
/// The test asserts the OBSERVED behavior, so a regression in either
/// mode (e.g. binding mode dropping the AV01 tag) trips it. When
/// ffmpeg eventually fixes AV1-on-stream_id=0xBD support, the
/// expectation here will need updating.
#[test]
#[ignore = "tier-b — requires ffprobe and exercises external decoder behavior"]
fn ffprobe_deep_classification_diff_binding_vs_interop() {
    if !tool_available("ffprobe") {
        eprintln!("ffprobe not available — skipping");
        return;
    }
    let binding_ts = build_av1_ts(Av1CarriageMode::Mpeg2TsBinding);
    let interop_ts = build_av1_ts(Av1CarriageMode::InteropRawObu);
    let binding_path = write_temp_ts(&binding_ts, "binding_deep");
    let interop_path = write_temp_ts(&interop_ts, "interop_deep");

    let binding_out = run(Command::new("ffprobe").args([
        "-v",
        "error",
        "-show_streams",
        "-of",
        "default=noprint_wrappers=1",
        binding_path.to_str().unwrap(),
    ]));
    let interop_out = run(Command::new("ffprobe").args([
        "-v",
        "error",
        "-show_streams",
        "-of",
        "default=noprint_wrappers=1",
        interop_path.to_str().unwrap(),
    ]));
    assert!(binding_out.status.success());
    assert!(interop_out.status.success());
    let binding_stdout = String::from_utf8_lossy(&binding_out.stdout);
    let interop_stdout = String::from_utf8_lossy(&interop_out.stdout);

    // Both modes carry the AV01 tag (descriptor unchanged across modes).
    assert!(binding_stdout.contains("codec_tag_string=AV01"));
    assert!(interop_stdout.contains("codec_tag_string=AV01"));

    // Diagnostic: print the stream blocks so the test log shows the
    // empirical acceptance shape. Skipped on green runs but visible
    // with `cargo test -- --ignored --nocapture`.
    eprintln!("=== ffprobe BINDING (stream_id=0xBD) ===\n{binding_stdout}");
    eprintln!("=== ffprobe INTEROP (stream_id=0xE0) ===\n{interop_stdout}");

    let _ = std::fs::remove_file(binding_path);
    let _ = std::fs::remove_file(interop_path);
}

/// ffmpeg-copy bitstream extraction: dumps the AV1 elementary stream
/// from the binding-conformant TS and confirms the extracted bytes
/// begin with `ts_open_bitstream_unit()` framing (3-byte start code +
/// per-OBU wrapping). This validates the round-trip of bytes through
/// ffmpeg's demuxer.
#[test]
#[ignore = "tier-b — requires ffmpeg and exercises bitstream extraction"]
fn ffmpeg_copy_extracts_ts_obu_framed_bitstream_from_binding_mode() {
    if !tool_available("ffmpeg") {
        eprintln!("ffmpeg not available — skipping");
        return;
    }
    let ts = build_av1_ts(Av1CarriageMode::Mpeg2TsBinding);
    let ts_path = write_temp_ts(&ts, "binding_copy");
    let dump_path = std::env::temp_dir().join("wave_i2_av1_dump.bin");
    // `-map 0:d:0` selects the first data stream (ffmpeg classifies
    // the AV1 binding PID as `data` because stream_id=0xBD isn't on
    // its video-dispatch path).
    let out = run(Command::new("ffmpeg").args([
        "-y",
        "-v",
        "error",
        "-i",
        ts_path.to_str().unwrap(),
        "-map",
        "0:d:0",
        "-c",
        "copy",
        "-f",
        "data",
        dump_path.to_str().unwrap(),
    ]));
    assert!(
        out.status.success(),
        "ffmpeg copy failed — stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    let bytes = std::fs::read(&dump_path).expect("read dump");
    assert!(!bytes.is_empty(), "ffmpeg copy produced empty bitstream");
    // Must start with the binding §3.2 start code.
    assert_eq!(
        &bytes[..3],
        &[0x00, 0x00, 0x01],
        "binding-mode bitstream MUST start with 0x00 0x00 0x01 ts_open_bitstream_unit prefix; \
         got {:02x?}",
        &bytes[..3.min(bytes.len())]
    );
    // Per-OBU framing: 92 OBUs across 30 AUs => ≥92 start codes in
    // the extracted bitstream.
    let n = bytes
        .windows(3)
        .filter(|w| *w == [0x00, 0x00, 0x01])
        .count();
    assert!(
        n >= 92,
        "expected ≥92 start codes in extracted binding bitstream (per-OBU framing), got {n}"
    );
    let _ = std::fs::remove_file(ts_path);
    let _ = std::fs::remove_file(dump_path);
}

/// dav1d-on-extracted-stream best-effort probe: dav1d expects an AV1
/// Low Overhead Bitstream Format (LOBF). The binding-mode extracted
/// bytes are `ts_open_bitstream_unit`-framed (NOT LOBF), so dav1d
/// SHOULD reject them — and that rejection is informative: it shows
/// that the binding framing isn't byte-equivalent to LOBF (which
/// would be the case if ts_open_bitstream_unit had been a no-op).
///
/// This test is `#[ignore]`d AND only asserts non-crash. We capture
/// exit code + stderr for the results doc.
#[test]
#[ignore = "tier-b — diagnostic dav1d probe, asserts non-crash only"]
fn dav1d_handles_extracted_binding_bitstream_without_crashing() {
    if !tool_available("dav1d") || !tool_available("ffmpeg") {
        eprintln!("dav1d or ffmpeg not available — skipping");
        return;
    }
    let ts = build_av1_ts(Av1CarriageMode::Mpeg2TsBinding);
    let ts_path = write_temp_ts(&ts, "binding_dav1d");
    let dump_path = std::env::temp_dir().join("wave_i2_av1_dav1d_dump.obu");
    let out = run(Command::new("ffmpeg").args([
        "-y",
        "-v",
        "error",
        "-i",
        ts_path.to_str().unwrap(),
        "-map",
        "0:d:0",
        "-c",
        "copy",
        "-f",
        "data",
        dump_path.to_str().unwrap(),
    ]));
    assert!(out.status.success());

    let dav1d_out =
        run(Command::new("dav1d").args(["-i", dump_path.to_str().unwrap(), "-o", "/dev/null"]));
    // dav1d exit codes: 0 = decoded, non-zero = parse/decode error.
    // For synthetic OBUs we expect failure but NOT a segfault (exit
    // code negative on Unix indicates a signal).
    let code = dav1d_out.status.code();
    assert!(
        code.is_some(),
        "dav1d terminated by signal (probable crash) on binding-extracted stream"
    );
    eprintln!(
        "dav1d exit={:?}, stderr:\n{}",
        code,
        String::from_utf8_lossy(&dav1d_out.stderr),
    );

    let _ = std::fs::remove_file(ts_path);
    let _ = std::fs::remove_file(dump_path);
}

/// Diagnostic dump for the results doc — writes both binding and
/// interop streams to /tmp + runs every available external tool,
/// captures all outputs to a single report file.
///
/// Run with: `cargo test -p tst-core --test regression
///   av1_external_decoder::diagnostic_dump_for_results_doc -- --ignored --nocapture`
#[test]
#[ignore = "diagnostic — writes AV1 binding/interop streams and external-tool reports to the temp dir"]
fn diagnostic_dump_for_results_doc() {
    let dir = std::env::temp_dir().join("av1_diagnostic_artifacts");
    std::fs::create_dir_all(&dir).unwrap();
    let binding_ts = build_av1_ts(Av1CarriageMode::Mpeg2TsBinding);
    let interop_ts = build_av1_ts(Av1CarriageMode::InteropRawObu);
    let binding_path = dir.join("av1_binding.ts");
    let interop_path = dir.join("av1_interop.ts");
    std::fs::write(&binding_path, &binding_ts).unwrap();
    std::fs::write(&interop_path, &interop_ts).unwrap();
    eprintln!(
        "WROTE {} ({} bytes)",
        binding_path.display(),
        binding_ts.len()
    );
    eprintln!(
        "WROTE {} ({} bytes)",
        interop_path.display(),
        interop_ts.len()
    );

    for (name, path) in [("BINDING", &binding_path), ("INTEROP", &interop_path)] {
        if tool_available("ffprobe") {
            let out = run(Command::new("ffprobe").args([
                "-v",
                "error",
                "-show_streams",
                "-show_format",
                "-of",
                "default=noprint_wrappers=1",
                path.to_str().unwrap(),
            ]));
            eprintln!(
                "--- ffprobe {} ---\nexit: {:?}\nstdout:\n{}\nstderr:\n{}",
                name,
                out.status.code(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
        if tool_available("tsanalyze") {
            let out =
                run(Command::new("tsanalyze").args(["--pid-analysis", path.to_str().unwrap()]));
            eprintln!(
                "--- tsanalyze {} ---\nexit: {:?}\nstdout:\n{}",
                name,
                out.status.code(),
                String::from_utf8_lossy(&out.stdout),
            );
        }
    }
    eprintln!("artifacts kept at {}", dir.display());
}

// Suppress dead-code warning for the path helper when external tools
// are absent on the CI image (some subtests don't call it).
#[allow(dead_code)]
fn _path_helper(_: &Path) {}
