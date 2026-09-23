//! Cross-binding scenario definitions.
//!
//! Each scenario is a self-contained unit that:
//!  1. Generates a deterministic synthetic input artifact (`.ts` bytes or
//!     similar) into `out_dir/<id>/`.
//!     (`out_dir` is the crate-local `crates/tst-integration/tests/fixtures/scenarios/`
//!     directory — canonical; all adapters resolve here, not workspace-root.)
//!  2. Returns a `Golden` envelope that the Rust adapter test verifies against.
//!
//! # Synthetic data only
//!
//! These generators NEVER read from `testfiles/`, any `local/` directory, or
//! any real corpus.  All input data is synthesised programmatically so that
//! the scenarios are hermetic and reproducible on any machine.

pub mod golden;

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tst_core::codec::misp_time::{MispTimestamp, extract as misp_extract};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    AudioCodec, KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, SubtitleCodec,
    VideoCodec,
};

use golden::{CoreEvent, Golden};

/// Trait every scenario implements.
pub trait Scenario {
    fn id(&self) -> &'static str;
    fn kind(&self) -> &'static str; // "demux" | "roundtrip" | "binding_contract"
    fn features(&self) -> Vec<&'static str>; // empty for Tier A
    fn tier(&self) -> &'static str; // "A"

    /// Generate the input artifact under `out_dir/<id>/` and return
    /// `(relative_path_of_input, Golden)`.
    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden);
}

/// All registered scenarios.
pub fn all_scenarios() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(H264St0601Mp),
        Box::new(VideoRoundtrip),
        Box::new(StrictRejection),
        Box::new(H265KlvMp),
        Box::new(H264SyncKlvAuCell),
        Box::new(Av1RegistrationDesc),
        Box::new(AacAudioOnly),
        Box::new(AudioVideoMp),
        Box::new(DvbSubtitleMp),
        Box::new(WebVttInTsScenario),
        Box::new(MalformedPsiStrict),
        Box::new(MalformedPesLenient),
        Box::new(UnknownStreamType),
        Box::new(AudioKlvRoundtrip),
        Box::new(VideoDtsRoundtrip),
        Box::new(VideoMispRoundtrip),
        Box::new(DropIdempotence),
        Box::new(ForgedHandle),
        Box::new(ExceptionKindStability),
        Box::new(CancelledRecvKind),
    ]
}

// ─────────────────────────────────────────────────────────────────────────────
// Shared mux helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Drain all buffered TS packets from `mux` into a `Vec<u8>`.
pub(crate) fn drain_mux(mux: &mut Muxer) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1316]; // 7 × 188
    loop {
        let n = mux.pull(&mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

/// Hex-encode a SHA-256 digest.
fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    hex::encode(digest)
}

// Minimal hex encoding — avoids pulling in the `hex` crate.
mod hex {
    pub(super) fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().fold(String::new(), |mut s, b| {
            use std::fmt::Write;
            write!(s, "{b:02x}").unwrap();
            s
        })
    }
}

/// Write `data` to `path`, creating parent directories as needed.
fn write_file(path: &Path, data: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create scenario dir");
    }
    std::fs::write(path, data).expect("write scenario input artifact");
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 1 — h264-st0601-mp (demux)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "demux"`: synthetic H.264 video + async ST 0601 KLV muxed to a TS,
/// golden is the normalised demux event sequence.
///
/// KLV stream is configured `PrivateData` (async, stream_type 0x06) so there
/// is no AU cell wrapping and the demux path is straightforward.  The muxer
/// auto-wraps sync KLV (SynchronousMetadata) in AU cell headers; async does
/// not require that.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct H264St0601Mp;

impl Scenario for H264St0601Mp {
    fn id(&self) -> &'static str {
        "h264-st0601-mp"
    }
    fn kind(&self) -> &'static str {
        "demux"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        // Build a simple single-program muxer: H.264 video + async KLV.
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x1011, VideoCodec::H264);
            prog.add_klv(
                0x1031,
                KlvStreamType::PrivateData,
                /*carries_pts=*/ false,
            );
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().expect("valid muxer config")
        };
        let mut mux = Muxer::new(cfg).expect("muxer init");

        // Synthetic H.264 IDR NAL — Annex-B start code + IDR nal_unit_type (5).
        // This generator NEVER reads from testfiles/ or local/ directories.
        let video_au = synthetic_h264_idr();
        let pts = Pts90khz::new(90_000); // t=1s
        mux.push_video(&video_au, pts, /*key_frame=*/ true)
            .expect("push_video");

        // Minimal valid KLV LS: MISB ST 0601 UL (16 bytes) + BER length 0.
        // This generator NEVER reads from testfiles/ or local/ directories.
        let klv_payload = minimal_st0601_ls();
        mux.push_klv(&klv_payload, pts, /*metadata_service_id=*/ 0x00)
            .expect("push_klv");

        let ts_bytes = drain_mux(&mut mux);

        // Demux the TS to derive the canonical event sequence.
        let core = demux_to_core_events(&ts_bytes);

        // Write input artifact.
        let artifact_rel = PathBuf::from(self.id()).join("input.ts");
        let artifact_abs = out_dir.join(&artifact_rel);
        write_file(&artifact_abs, &ts_bytes);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core,
            extensions: serde_json::Value::Null,
        };
        (artifact_rel, golden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 2 — video-roundtrip (roundtrip)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "roundtrip"`: mux synthetic H.264 video to TS bytes; golden is the
/// output bytes + sha256 (byte-identity).
///
/// A video-only program is used because:
/// - The muxer is deterministic → sha256 goldens are stable across runs.
/// - Audio payload (AAC ADTS) is more complex to synthesise correctly;
///   H.264 Annex-B is trivially correct with a 4-byte start code.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct VideoRoundtrip;

impl Scenario for VideoRoundtrip {
    fn id(&self) -> &'static str {
        "video-roundtrip"
    }
    fn kind(&self) -> &'static str {
        "roundtrip"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        // Single-program, single-video muxer (no KLV) for simplest possible
        // deterministic output. Produced via the shared single-source-of-truth
        // helper so the adapter test can re-run the identical recipe.
        // This generator NEVER reads from testfiles/ or local/ directories.
        let ts_bytes = video_roundtrip_ts_bytes();
        let digest = sha256_hex(&ts_bytes);

        // Write input artifact (the TS output IS the golden artifact for
        // roundtrip verification).
        let artifact_rel = PathBuf::from(self.id()).join("output.ts");
        let artifact_abs = out_dir.join(&artifact_rel);
        write_file(&artifact_abs, &ts_bytes);

        // A roundtrip scenario carries no media events — the whole-stream
        // byte-identity digest lives under `extensions.output_sha256` (the
        // demux-path `payload_sha256` field means "NAL payload hash" and must
        // not be overloaded). The adapter re-muxes and compares both the raw
        // bytes against `output.ts` and the digest against `output_sha256`.
        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core: vec![],
            extensions: serde_json::json!({ "output_sha256": digest }),
        };
        (artifact_rel, golden)
    }
}

/// Re-run the `video-roundtrip` mux and return the deterministic TS bytes.
///
/// Single source of truth shared by `VideoRoundtrip::generate` and the Rust
/// adapter test — no hand-retyped mux recipe.
pub fn video_roundtrip_ts_bytes() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().expect("valid muxer config")
    };
    let mut mux = Muxer::new(cfg).expect("muxer init");
    mux.push_video(
        &synthetic_h264_idr(),
        Pts90khz::new(0),
        /*key_frame=*/ true,
    )
    .expect("push_video");
    drain_mux(&mut mux)
}

/// Re-run the `audio-klv-roundtrip` mux and return the deterministic TS bytes.
///
/// Single source of truth shared by `AudioKlvRoundtrip::generate` and the Rust
/// adapter test — no hand-retyped mux recipe.
pub fn audio_klv_roundtrip_ts_bytes() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        prog.add_audio(0x1021, AudioCodec::Aac);
        // SynchronousMetadata requires carries_pts = true.
        prog.add_klv(
            0x1031,
            KlvStreamType::SynchronousMetadata,
            /*carries_pts=*/ true,
        );
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().expect("valid muxer config")
    };
    let mut mux = Muxer::new(cfg).expect("muxer init");

    // PTS 0 throughout: the committed output_sha256 golden is locked to this
    // exact mux output — do not change.
    let pts = Pts90khz::new(0);
    mux.push_video(&synthetic_h264_idr(), pts, /*key_frame=*/ true)
        .expect("push_video");
    mux.push_audio(&synthetic_adts_frame(), pts)
        .expect("push_audio");
    // Pass raw KLV LS bytes — muxer auto-wraps in the AU cell header.
    mux.push_klv(
        &minimal_st0601_ls(),
        pts,
        /*metadata_service_id=*/ 0x00,
    )
    .expect("push_klv");
    drain_mux(&mut mux)
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 3 — strict-rejection (binding_contract, non-media)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "binding_contract"`: deliberately malformed TS fed to a strict-mode
/// demuxer; golden asserts `STRICT_REJECTION` error identity AND that
/// close/drop after an error is idempotent (no panic).
///
/// The "input" artifact is a garbage byte sequence that cannot be a valid
/// MPEG-TS stream: all 0xFF, which contains no 0x47 sync byte within the
/// `SYNC_SEARCH_WINDOW` distance, triggering `DemuxError::Unrecoverable`.
/// With `StrictMode::Full`, *any* non-conformance also becomes a
/// `StrictRejection`; but for a completely garbled stream the simpler
/// `Unrecoverable` error is guaranteed first.
///
/// Stable public code: `"STRICT_REJECTION"` is the umbrella code used by the
/// C and Python adapters for any `DemuxError` that maps to a hard-reject; the
/// Rust test emits it directly for the unrecoverable-input case so the golden
/// code is uniform across languages.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct StrictRejection;

impl Scenario for StrictRejection {
    fn id(&self) -> &'static str {
        "strict-rejection"
    }
    fn kind(&self) -> &'static str {
        "binding_contract"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        // Garbage bytes: no 0x47 sync byte anywhere → Unrecoverable after
        // walking SYNC_SEARCH_WINDOW (188 * 32 = 6016) bytes.
        // Must be larger than SYNC_SEARCH_WINDOW to trigger the error.
        // This generator NEVER reads from testfiles/ or local/ directories.
        let garbage: Vec<u8> = vec![0xFF; 8192];

        let artifact_rel = PathBuf::from(self.id()).join("input.bin");
        let artifact_abs = out_dir.join(&artifact_rel);
        write_file(&artifact_abs, &garbage);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core: vec![CoreEvent::Error {
                code: "STRICT_REJECTION".to_string(),
            }],
            extensions: serde_json::Value::Null,
        };
        (artifact_rel, golden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Synthetic data helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Minimal valid H.264 IDR access unit in Annex-B framing.
///
/// 4-byte start code + 1-byte NAL header (IDR nal_unit_type = 5,
/// nal_ref_idc = 3) + 15 deterministic filler bytes.
pub(crate) fn synthetic_h264_idr() -> Vec<u8> {
    let mut buf = Vec::with_capacity(20);
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // Annex-B start code
    buf.push(0x65); // forbidden_zero=0, nal_ref_idc=11, nal_unit_type=5 (IDR)
    for i in 0u8..15 {
        buf.push(0xA5 ^ i);
    }
    buf
}

/// Minimal MISB ST 0601 UL (16 bytes) + BER short-form length 0.
///
/// This is the smallest self-consistent KLV LS: the UL identifies the ST 0601
/// set and BER length 0 means zero value bytes.  The muxer treats KLV payload
/// opaquely; the demuxer surfaces it verbatim on the Metadata event.
fn minimal_st0601_ls() -> Vec<u8> {
    vec![
        // MISB ST 0601 UAS Datalink LS UL (SMPTE 336M 16-byte form):
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01, 0x00, 0x00,
        0x00, // UL bytes 1-16
        0x00, // BER short-form length = 0 (no value bytes)
    ]
}

/// Minimal valid H.265 IDR access unit in Annex-B framing.
///
/// 4-byte start code + 2-byte NAL header (IDR_W_RADL nal_unit_type=19,
/// nuh_layer_id=0, nuh_temporal_id_plus1=1) + 14 deterministic filler bytes.
fn synthetic_h265_idr() -> Vec<u8> {
    let mut buf = Vec::with_capacity(20);
    buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // Annex-B start code
    // H.265 NAL header (2 bytes):
    //   forbidden_zero_bit(1) = 0
    //   nal_unit_type(6)      = 19 (IDR_W_RADL) → bits 1..=6
    //   nuh_layer_id(6)       = 0
    //   nuh_temporal_id_plus1(3) = 1
    // Byte 0: (19 << 1) & 0xFF = 0x26, Byte 1: 0x01
    buf.push(0x26);
    buf.push(0x01);
    for i in 0u8..14 {
        buf.push(0xB7 ^ i);
    }
    buf
}

/// Minimal AV1 access unit: Temporal Delimiter + Sequence Header + Frame
/// Header + Tile Group OBUs with `obu_has_size_field = 1`.
///
/// Bodies are placeholder bytes — the muxer and demuxer treat them as opaque
/// payload.  The demuxer recovers `VideoCodec::Av1` from the PMT
/// `format_identifier "AV01"` (registration descriptor emitted by the muxer),
/// not from the OBU payload.
fn synthetic_av1_au() -> Vec<u8> {
    fn obu(obu_type: u8, body: &[u8]) -> Vec<u8> {
        // AV1 spec §5.3.2 OBU header:
        //   forbidden(1)=0, obu_type(4), extension_flag(1)=0,
        //   has_size_field(1)=1, reserved(1)=0
        let header = (obu_type << 3) | 0x02;
        let mut v = vec![header];
        v.push(body.len() as u8); // single-byte LEB128 (body < 128 bytes)
        v.extend_from_slice(body);
        v
    }
    let mut au = Vec::new();
    au.extend(obu(2, &[])); // Temporal Delimiter (empty body)
    au.extend(obu(1, &[0x00, 0x00])); // Sequence Header placeholder
    au.extend(obu(3, &[0x00])); // Frame Header placeholder
    au.extend(obu(4, &[0x00, 0x01, 0x02])); // Tile Group placeholder
    au
}

/// Minimal ADTS frame (7-byte header + 8 payload bytes = 15 bytes total).
///
/// Fixed parameters: MPEG-2 ID, no CRC, AAC-LC profile, sample_rate_index=4
/// (44100 Hz), channel_config=2 (stereo).  The muxer treats audio bytes
/// opaquely; the 7-byte ADTS sync header makes the frame parsable by the
/// codec stats counter.
fn synthetic_adts_frame() -> Vec<u8> {
    let total_len: u32 = 15; // 7-byte header + 8 payload bytes
    let sample_rate_index: u8 = 4; // 44100 Hz
    let channel_config: u8 = 2; // stereo
    let mut h = vec![0u8; 7];
    h[0] = 0xFF;
    // ID=MPEG-2(1), layer=0b00, protection_absent=1 → 0b1111_0001
    h[1] = 0b1111_0001;
    // profile_objecttype(2)=1(AAC-LC), sampling_freq_index(4), private=0,
    // channel_config upper bit
    h[2] = (1 << 6) | ((sample_rate_index & 0xF) << 2) | ((channel_config >> 2) & 1);
    h[3] = ((channel_config & 0b11) << 6) | (((total_len >> 11) & 0b11) as u8);
    h[4] = ((total_len >> 3) & 0xFF) as u8;
    h[5] = (((total_len & 0b111) as u8) << 5) | 0b1_1111;
    h[6] = 0b11_1111 << 2;
    let mut out = h;
    // 8 deterministic payload bytes
    out.extend_from_slice(&[0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7]);
    out
}

/// Minimal DVB subtitle segment — a bare subtitle_segment body passed to
/// `push_subtitle` for a `DvbSubtitling`-codec stream.
///
/// The muxer's DVB-sub PES writer auto-prepends a 2-byte PES data field
/// header (`data_identifier=0x20`, `subtitle_stream_id=0x00`) and appends a
/// 1-byte end marker (`0xFF`). The caller therefore passes the raw
/// subtitle segment bytes only.
///
/// A DVB subtitle segment (ETSI EN 300 743 §7.2) has the structure:
///   sync_byte(1) = 0x0F
///   segment_type(1)
///   page_id(2)
///   segment_length(2)
///   … payload …
/// Segment type 0x80 = page_composition_segment (always present per §9.5.1).
/// Minimal body: no regions → segment_length = 0x0003 (the 3-byte fixed header
/// beyond the common 6-byte header: page_time_out(2) + state/version(1)).
/// In practice we emit the shortest possible page_composition_segment:
///   sync_byte=0x0F, type=0x80, page_id=0x0001, length=0x0003,
///   presentation_time=0x0000, page_state_version=0x00 (state=Normal+version 0).
fn synthetic_dvb_subtitle_segment() -> Vec<u8> {
    vec![
        0x0F, // sync_byte
        0x80, // segment_type = page_composition_segment
        0x00, 0x01, // page_id = 1
        0x00, 0x03, // segment_length = 3 (page_time_out(2) + version(1))
        0x00, 0x00, // page_time_out (2 byte) = 0
        0x00, // version_number(4) | page_state(2) | reserved(2) = 0x00
    ]
}

/// Minimal WebVTT cue carried in MPEG-TS PES (`WebVttInTs` codec).
///
/// The `WebVttInTs` PES shape passes caller bytes through unchanged
/// (passthrough mode — no auto-prepend). We use a deterministic ASCII cue
/// payload in the informal MPEG-TS WebVTT encoding.
fn synthetic_webvtt_cue() -> Vec<u8> {
    // A minimal well-formed WebVTT cue body — deterministic and ASCII-safe.
    b"00:00:00.000 --> 00:00:01.000\nHello\n".to_vec()
}

/// Minimal MPEG-2 audio frame — synthetic bytes that begin with the valid
/// MPEG audio sync word used by the mux audio path.
/// The muxer treats these bytes as opaque PES payload.
fn synthetic_mp2_frame() -> Vec<u8> {
    // MPEG-1 Layer II sync header: sync(12)=0xFFF, ID(1)=1(MPEG-1),
    // layer(2)=0b10(Layer II), protection(1)=1(no CRC) → 0xFF 0xFD
    let mut buf = vec![0u8; 20];
    buf[0] = 0xFF;
    buf[1] = 0xFD; // MPEG-1, Layer II, no CRC
    buf[2] = 0xC0; // bitrate=384kbps, 44100Hz, no padding
    buf[3] = 0x04; // stereo, original
    // 16 deterministic payload bytes
    for i in 4u8..20 {
        buf[i as usize] = 0xB0 ^ i;
    }
    buf
}

// ─────────────────────────────────────────────────────────────────────────────
// Demux-to-CoreEvent normaliser
// ─────────────────────────────────────────────────────────────────────────────

/// Run tst-core's Demuxer over `ts_bytes` and convert the event stream to a
/// `Vec<CoreEvent>` in canonical order.
///
/// Rules:
/// - `ProgramMap` → skipped (not part of media-event golden).
/// - `Sample { .. Video }` → `CoreEvent::Video`; `payload_sha256` covers the
///   full raw NAL bytes (all NAL units concatenated).
/// - `Sample { .. Audio }` → `CoreEvent::Audio`.
/// - `Metadata { .. }` → `CoreEvent::Klv`.
/// - `Discontinuity` / `NonConformant` / `ReconnectDiscontinuity` → skipped
///   (diagnostics, not part of the media golden).
/// - Unknown stream type → `CoreEvent::Unknown`.
///
/// Errors from `feed` are mapped to `CoreEvent::Error { code }` using the
/// stable public code.
pub fn demux_to_core_events(ts_bytes: &[u8]) -> Vec<CoreEvent> {
    use std::collections::HashMap;
    use tst_core::mpegts::demux::event::SamplePayload;
    use tst_core::mpegts::demux::{DemuxEvent, Demuxer, split_video};

    let mut demuxer = Demuxer::new();

    if let Err(e) = demuxer.feed(ts_bytes) {
        return vec![CoreEvent::Error {
            code: demux_error_code(&e),
        }];
    }

    demuxer.flush();

    // Drain all events first so we can build a pid → PMT stream_type byte map
    // from the ProgramMap events before emitting Sample / Metadata events.
    // The golden's `stream_type` is the raw PMT byte (e.g. "0x1b"), which is
    // binding-neutral — a C / Python adapter sees the wire byte, not a Rust
    // enum name.
    let mut raw_events = Vec::new();
    while let Some(ev) = demuxer.next_event() {
        raw_events.push(ev);
    }

    let mut stream_type_by_pid: HashMap<u16, u8> = HashMap::new();
    for ev in &raw_events {
        if let DemuxEvent::ProgramMap(pm) = ev {
            for si in &pm.streams {
                stream_type_by_pid.insert(si.pid, si.stream_type.as_byte());
            }
        }
    }

    // Hex-format a PMT stream_type byte for `pid`, e.g. "0x1b". Falls back to
    // mapping the codec/kind to its canonical PMT byte if the PID was never
    // seen in a ProgramMap (defensive — should not happen for well-formed TS).
    let stream_type_hex = |pid: u16, fallback: u8| -> String {
        let byte = stream_type_by_pid.get(&pid).copied().unwrap_or(fallback);
        format!("0x{byte:02x}")
    };

    let mut events = Vec::new();
    for ev in raw_events {
        match ev {
            DemuxEvent::ProgramMap(_) => { /* skip — topology, not media */ }
            DemuxEvent::Sample {
                stream,
                pts,
                payload,
                ..
            } => {
                match payload {
                    SamplePayload::Video {
                        codec,
                        raw: au,
                        random_access_indicator,
                        av1_carriage,
                        ..
                    } => {
                        // Raw-first: the demuxer emits the encoded access unit;
                        // recover the parsed NAL/OBU bodies via the opt-in
                        // `split_video` so the golden SHA matches the prior
                        // (parsed) demuxer output exactly.
                        let (vp, _issues) =
                            split_video(&au, codec, av1_carriage.unwrap_or_default());
                        let raw = video_payload_bytes(&vp);
                        events.push(CoreEvent::Video {
                            program: stream.program_number,
                            pid: stream.pid,
                            stream_type: stream_type_hex(stream.pid, video_codec_pmt_byte(codec)),
                            pts: pts.as_ticks(),
                            key: random_access_indicator,
                            payload_sha256: sha256_hex(&raw),
                        });
                    }
                    SamplePayload::Audio { codec, frames } => {
                        events.push(CoreEvent::Audio {
                            program: stream.program_number,
                            pid: stream.pid,
                            stream_type: stream_type_hex(stream.pid, audio_codec_pmt_byte(codec)),
                            pts: pts.as_ticks(),
                            payload_sha256: sha256_hex(&frames),
                        });
                    }
                    SamplePayload::Subtitle { codec, .. } => {
                        events.push(CoreEvent::Subtitle {
                            program: stream.program_number,
                            pid: stream.pid,
                            // All subtitle codecs emit PMT stream_type 0x06.
                            stream_type: stream_type_hex(stream.pid, 0x06),
                            codec: subtitle_codec_tag(codec),
                        });
                    }
                    SamplePayload::Unknown { .. } => {
                        events.push(CoreEvent::Unknown { pid: stream.pid });
                    }
                }
            }
            DemuxEvent::Metadata {
                stream,
                payload,
                kind,
                ..
            } => {
                // `set` is the MISB set identity derived from the KLV
                // Universal Label key (binding-neutral). Framing info
                // (sync_au_cell vs async) is NOT in the frozen core — it
                // lives under `extensions` if a scenario needs it.
                let _ = &kind;
                events.push(CoreEvent::Klv {
                    program: stream.program_number,
                    pid: stream.pid,
                    stream_type: stream_type_hex(stream.pid, klv_kind_pmt_byte(&stream.kind)),
                    set: klv_set_from_ul(&payload),
                });
            }
            // NonConformant → CoreEvent::Error with a stable public code.
            // The conformant Muxer emits zero NonConformant events, so clean
            // demux goldens are unaffected; malformed-input scenarios surface
            // the diagnostic in queue order alongside any recovered samples.
            DemuxEvent::NonConformant { issue, .. } => {
                events.push(CoreEvent::Error {
                    code: nonconformant_issue_code(&issue),
                });
            }
            DemuxEvent::Discontinuity { .. } | DemuxEvent::ReconnectDiscontinuity => {
                /* diagnostic — skip */
            }
        }
    }

    events
}

fn video_payload_bytes(vp: &tst_core::mpegts::demux::event::VideoPayload) -> Vec<u8> {
    use tst_core::mpegts::demux::event::{NalUnit, VideoPayload};
    match vp {
        VideoPayload::Nals(nals) => {
            let mut out = Vec::new();
            for n in nals {
                match n {
                    NalUnit::H264 { payload, .. }
                    | NalUnit::H265 { payload, .. }
                    | NalUnit::H266 { payload, .. } => out.extend_from_slice(payload),
                }
            }
            out
        }
        VideoPayload::Obus(obus) => {
            let mut out = Vec::new();
            for o in obus {
                out.extend_from_slice(&o.payload);
            }
            out
        }
    }
}

// Canonical PMT `stream_type` bytes per ISO/IEC 13818-1 Table 2-34 + the
// codec/metadata extensions tst-core supports.  Used only as a fallback when
// the PID was not seen in a ProgramMap; the primary source is the parsed PMT
// (`StreamInfo::stream_type.as_byte()`).
//   H.264 = 0x1B, H.265 = 0x24, H.266 = 0x33, AV1 = 0x06 (with AV01 reg-desc).
fn video_codec_pmt_byte(codec: tst_core::mpegts::demux::event::VideoCodec) -> u8 {
    use tst_core::mpegts::demux::event::VideoCodec;
    match codec {
        VideoCodec::H264 => 0x1B,
        VideoCodec::H265 => 0x24,
        VideoCodec::H266 => 0x33,
        VideoCodec::Av1 => 0x06,
    }
}

//   MP2 = 0x03/0x04, AAC ADTS = 0x0F, AAC LATM = 0x11, AC-3 = 0x81.
fn audio_codec_pmt_byte(codec: tst_core::mpegts::demux::event::AudioCodec) -> u8 {
    use tst_core::mpegts::demux::event::AudioCodec;
    match codec {
        AudioCodec::Mp2 => 0x03,
        AudioCodec::Aac => 0x0F,
        AudioCodec::AacLatm => 0x11,
        AudioCodec::Ac3 => 0x81,
    }
}

//   KLV sync metadata = 0x15, KLV async (private_data) = 0x06.
fn klv_kind_pmt_byte(kind: &tst_core::mpegts::demux::event::StreamKind) -> u8 {
    use tst_core::mpegts::demux::event::StreamKind;
    match kind {
        StreamKind::KlvSync { .. } => 0x15,
        StreamKind::KlvAsync => 0x06,
        _ => 0x06,
    }
}

/// Detect the MISB KLV set identity from the Universal Label key prefix of the
/// raw KLV LS bytes.  Binding-neutral: the same 16-byte UL is visible to a C /
/// Python adapter.  Returns `"st0601"` for the ST 0601 UAS Datalink LS UL,
/// else `"unknown"`.
fn klv_set_from_ul(payload: &[u8]) -> String {
    // MISB ST 0601 UAS Datalink LS Universal Label (SMPTE 336M 16-byte key).
    // The first 13 bytes are the canonical ST 0601 designator; bytes 14-16
    // carry the version, which varies, so we match on the stable prefix.
    const ST0601_UL_PREFIX: [u8; 13] = [
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x0B, 0x01, 0x01, 0x0E, 0x01, 0x03, 0x01, 0x01,
    ];
    if payload.len() >= ST0601_UL_PREFIX.len()
        && payload[..ST0601_UL_PREFIX.len()] == ST0601_UL_PREFIX
    {
        "st0601".to_string()
    } else {
        "unknown".to_string()
    }
}

/// Map a demux `SubtitleCodec` to the binding-neutral string tag used in
/// `CoreEvent::Subtitle::codec`.
///
/// Tags are stable across Rust/C/Python: "dvb_subtitle" | "dvb_teletext" |
/// "webvtt".  `Cea708Standalone` is not in the cross-binding golden surface
/// yet — it maps to "cea708_standalone" for forward-compat.
fn subtitle_codec_tag(codec: tst_core::mpegts::demux::event::SubtitleCodec) -> String {
    use tst_core::mpegts::demux::event::SubtitleCodec;
    match codec {
        SubtitleCodec::DvbSubtitling => "dvb_subtitle".to_string(),
        SubtitleCodec::DvbTeletext => "dvb_teletext".to_string(),
        SubtitleCodec::WebVttInTs => "webvtt".to_string(),
        SubtitleCodec::Cea708Standalone => "cea708_standalone".to_string(),
    }
}

/// Public alias for tests.
pub fn demux_error_code_pub(e: &tst_core::error::DemuxError) -> String {
    demux_error_code(e)
}

// Umbrella public code — every fatal demux error maps to the single
// binding-neutral reject code.
fn demux_error_code(_e: &tst_core::error::DemuxError) -> String {
    "STRICT_REJECTION".into()
}

/// Map a `NonConformantIssue` to the stable public string code used in the
/// cross-binding golden envelope.
///
/// String codes match the `TST_NONCONFORMANT_CODE_*` constant base names in
/// `bindings/c/include/tstrans.h` (e.g. `TST_NONCONFORMANT_CODE_PES_HEADER_MALFORMED`
/// → `"PES_HEADER_MALFORMED"`), minus the `TST_NONCONFORMANT_CODE_` prefix.
/// These are the stability surface: do NOT rename them without a schema_version bump.
pub fn nonconformant_issue_code(
    issue: &tst_core::mpegts::demux::event::NonConformantIssue,
) -> String {
    use tst_core::mpegts::demux::event::NonConformantIssue;
    match issue {
        NonConformantIssue::StreamTypeMismatchSyncOnAsyncPid => {
            "STREAM_TYPE_MISMATCH_SYNC_ON_ASYNC_PID"
        }
        NonConformantIssue::StreamTypeMismatchAsyncOnSyncPid => {
            "STREAM_TYPE_MISMATCH_ASYNC_ON_SYNC_PID"
        }
        NonConformantIssue::MissingMetadataDescriptor => "MISSING_METADATA_DESCRIPTOR",
        NonConformantIssue::PcrAnomaly { .. } => "PCR_ANOMALY",
        NonConformantIssue::PsiChecksumMismatch { .. } => "PSI_CHECKSUM_MISMATCH",
        NonConformantIssue::PusiMidPes => "PUSI_MID_PES",
        NonConformantIssue::MalformedPes { .. } => "MALFORMED_PES",
        NonConformantIssue::PidReusedAcrossPrograms { .. } => "PID_REUSED_ACROSS_PROGRAMS",
        NonConformantIssue::SubtitleMissingDescriptor { .. } => "SUBTITLE_MISSING_DESCRIPTOR",
        NonConformantIssue::SubtitleDescriptorAmbiguous { .. } => "SUBTITLE_DESCRIPTOR_AMBIGUOUS",
        NonConformantIssue::SubtitleDescriptorMalformed { .. } => "SUBTITLE_DESCRIPTOR_MALFORMED",
        NonConformantIssue::Av1RegistrationMalformed { .. } => "AV1_REGISTRATION_MALFORMED",
        NonConformantIssue::Av1ObuMissingSizeField { .. } => "AV1_OBU_MISSING_SIZE_FIELD",
        NonConformantIssue::Av1TileListNotAllowed { .. } => "AV1_TILE_LIST_NOT_ALLOWED",
        NonConformantIssue::PsiOverlongSection { .. } => "PSI_OVERLONG_SECTION",
        NonConformantIssue::TransportErrorPacket { .. } => "TRANSPORT_ERROR_PACKET",
        NonConformantIssue::PsiCcDiscontinuity { .. } => "PSI_CC_DISCONTINUITY",
        NonConformantIssue::MultiCellAu { .. } => "MULTI_CELL_AU",
        NonConformantIssue::CfiTolerated { .. } => "CFI_TOLERATED",
        NonConformantIssue::PsiMultiSectionUnsupported { .. } => "PSI_MULTI_SECTION_UNSUPPORTED",
        NonConformantIssue::DvbSubDataIdentifier { .. } => "DVB_SUB_DATA_IDENTIFIER",
        NonConformantIssue::PtsAnomaly { .. } => "PTS_ANOMALY",
        NonConformantIssue::MissingRequiredPts { .. } => "MISSING_REQUIRED_PTS",
        NonConformantIssue::PesHeaderMalformed { .. } => "PES_HEADER_MALFORMED",
        NonConformantIssue::SubtitleAlignmentMissing { .. } => "SUBTITLE_ALIGNMENT_MISSING",
        NonConformantIssue::PcrMalformed { .. } => "PCR_MALFORMED",
        NonConformantIssue::NalHeader { .. } => "NAL_HEADER",
        NonConformantIssue::Av1ObuHeader { .. } => "AV1_OBU_HEADER",
        NonConformantIssue::Ac3SyncMissing { .. } => "AC3_SYNC_MISSING",
        NonConformantIssue::LatmFraming { .. } => "LATM_FRAMING",
        NonConformantIssue::Av1WrongStreamId { .. } => "AV1_WRONG_STREAM_ID",
        NonConformantIssue::Av1MissingTsObuFraming { .. } => "AV1_MISSING_TS_OBU_FRAMING",
        NonConformantIssue::PmtProgramNumberMismatch { .. } => "PMT_PROGRAM_NUMBER_MISMATCH",
        NonConformantIssue::UnsupportedScrambling { .. } => "UNSUPPORTED_SCRAMBLING",
        NonConformantIssue::AdaptationFieldMalformed { .. } => "ADAPTATION_FIELD_MALFORMED",
        NonConformantIssue::ZeroLengthPesNonVideo { .. } => "ZERO_LENGTH_PES_NON_VIDEO",
        NonConformantIssue::PsiSyntax { .. } => "PSI_SYNTAX",
        NonConformantIssue::Other(_) => "OTHER",
    }
    .into()
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 4 — h265-klv-mp (demux)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "demux"`: H.265 video + asynchronous (PrivateData) KLV.
///
/// KLV stream is `PrivateData` (PMT stream_type 0x06, async) — the same
/// carriage as `h264-st0601-mp` but with an H.265 video codec.  This
/// exercises the H.265 `VideoCodec` path (PMT stream_type 0x24) end-to-end.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct H265KlvMp;

impl Scenario for H265KlvMp {
    fn id(&self) -> &'static str {
        "h265-klv-mp"
    }
    fn kind(&self) -> &'static str {
        "demux"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x1011, VideoCodec::H265);
            prog.add_klv(
                0x1031,
                KlvStreamType::PrivateData,
                /*carries_pts=*/ false,
            );
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().expect("valid muxer config")
        };
        let mut mux = Muxer::new(cfg).expect("muxer init");

        let pts = Pts90khz::new(90_000); // t=1s
        mux.push_video(&synthetic_h265_idr(), pts, /*key_frame=*/ true)
            .expect("push_video");
        mux.push_klv(
            &minimal_st0601_ls(),
            pts,
            /*metadata_service_id=*/ 0x00,
        )
        .expect("push_klv");

        let ts_bytes = drain_mux(&mut mux);
        let core = demux_to_core_events(&ts_bytes);

        let artifact_rel = PathBuf::from(self.id()).join("input.ts");
        write_file(&out_dir.join(&artifact_rel), &ts_bytes);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core,
            extensions: serde_json::Value::Null,
        };
        (artifact_rel, golden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 5 — h264-sync-klv-aucell (demux)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "demux"`: H.264 video + `SynchronousMetadata` KLV (PMT 0x15).
///
/// The muxer auto-prepends the 5-byte `Metadata_AU_cell` header per
/// ITU-T H.222.0 §2.12.4.2 — callers pass raw KLV LS bytes only
/// (`reference_klv_au_cell_caller_responsibility`).  The demuxer unwraps the
/// AU cell and surfaces the raw LS bytes in `DemuxEvent::Metadata::payload`.
///
/// `carries_pts = true` is required for `SynchronousMetadata` streams.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct H264SyncKlvAuCell;

impl Scenario for H264SyncKlvAuCell {
    fn id(&self) -> &'static str {
        "h264-sync-klv-aucell"
    }
    fn kind(&self) -> &'static str {
        "demux"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x1011, VideoCodec::H264);
            // SynchronousMetadata requires carries_pts = true.
            prog.add_klv(
                0x1031,
                KlvStreamType::SynchronousMetadata,
                /*carries_pts=*/ true,
            );
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().expect("valid muxer config")
        };
        let mut mux = Muxer::new(cfg).expect("muxer init");

        let pts = Pts90khz::new(90_000); // t=1s
        mux.push_video(&synthetic_h264_idr(), pts, /*key_frame=*/ true)
            .expect("push_video");
        // Pass raw KLV LS bytes — muxer auto-wraps in the AU cell header.
        mux.push_klv(
            &minimal_st0601_ls(),
            pts,
            /*metadata_service_id=*/ 0x00,
        )
        .expect("push_klv");

        let ts_bytes = drain_mux(&mut mux);
        let core = demux_to_core_events(&ts_bytes);

        let artifact_rel = PathBuf::from(self.id()).join("input.ts");
        write_file(&out_dir.join(&artifact_rel), &ts_bytes);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core,
            extensions: serde_json::Value::Null,
        };
        (artifact_rel, golden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 6 — av1-registration-desc (demux)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "demux"`: AV1 video stream (PMT stream_type 0x06 + AV01
/// registration descriptor).
///
/// AV1 OBU payload is pushed as raw OBU bytes (no Annex-B start code).
/// In `Mpeg2TsBinding` carriage mode (the default) the muxer wraps OBUs
/// in `ts_open_bitstream_unit()` framing.  The demuxer recovers
/// `VideoCodec::Av1` from the PMT `format_identifier "AV01"`.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct Av1RegistrationDesc;

impl Scenario for Av1RegistrationDesc {
    fn id(&self) -> &'static str {
        "av1-registration-desc"
    }
    fn kind(&self) -> &'static str {
        "demux"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x1011, VideoCodec::Av1);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().expect("valid muxer config")
        };
        let mut mux = Muxer::new(cfg).expect("muxer init");

        let pts = Pts90khz::new(90_000); // t=1s
        // AV1 OBU payload — no Annex-B start code.
        mux.push_video(&synthetic_av1_au(), pts, /*key_frame=*/ true)
            .expect("push_video");

        let ts_bytes = drain_mux(&mut mux);
        let core = demux_to_core_events(&ts_bytes);

        let artifact_rel = PathBuf::from(self.id()).join("input.ts");
        write_file(&out_dir.join(&artifact_rel), &ts_bytes);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core,
            extensions: serde_json::Value::Null,
        };
        (artifact_rel, golden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 7 — aac-audio-only (demux)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "demux"`: a program with a single AAC ADTS audio stream and no
/// video, exercising the audio-only program path.
///
/// Note: `MuxerConfig::validate` forbids subtitle-only programs but permits
/// audio-only programs — the PCR fallback chain includes audio.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct AacAudioOnly;

impl Scenario for AacAudioOnly {
    fn id(&self) -> &'static str {
        "aac-audio-only"
    }
    fn kind(&self) -> &'static str {
        "demux"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_audio(0x1021, AudioCodec::Aac);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().expect("valid muxer config")
        };
        let mut mux = Muxer::new(cfg).expect("muxer init");

        let pts = Pts90khz::new(90_000); // t=1s
        mux.push_audio(&synthetic_adts_frame(), pts)
            .expect("push_audio");

        let ts_bytes = drain_mux(&mut mux);
        let core = demux_to_core_events(&ts_bytes);

        let artifact_rel = PathBuf::from(self.id()).join("input.ts");
        write_file(&out_dir.join(&artifact_rel), &ts_bytes);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core,
            extensions: serde_json::Value::Null,
        };
        (artifact_rel, golden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 8 — audio-video-mp (demux)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "demux"`: one program with H.264 video + AAC ADTS audio + MPEG-2
/// audio.  Exercises multi-audio programs and the `push_audio_to` path with
/// explicit `AudioStreamHandle`s.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct AudioVideoMp;

impl Scenario for AudioVideoMp {
    fn id(&self) -> &'static str {
        "audio-video-mp"
    }
    fn kind(&self) -> &'static str {
        "demux"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x1011, VideoCodec::H264);
            prog.add_audio(0x1021, AudioCodec::Aac);
            prog.add_audio(0x1022, AudioCodec::Mp2);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().expect("valid muxer config")
        };
        let mut mux = Muxer::new(cfg).expect("muxer init");

        let pts = Pts90khz::new(90_000); // t=1s

        mux.push_video(&synthetic_h264_idr(), pts, /*key_frame=*/ true)
            .expect("push_video");

        // Two audio streams — use explicit handles to avoid AmbiguousTarget.
        // audio_handles() returns streams in PMT declaration order.
        let handles = mux.audio_handles();
        assert_eq!(handles.len(), 2, "expected two audio handles");
        let aac_handle = handles[0]; // AAC (declared first)
        let mp2_handle = handles[1]; // MP2 (declared second)

        mux.push_audio_to(aac_handle, pts, &synthetic_adts_frame())
            .expect("push_audio_to aac");
        mux.push_audio_to(mp2_handle, pts, &synthetic_mp2_frame())
            .expect("push_audio_to mp2");

        let ts_bytes = drain_mux(&mut mux);
        let core = demux_to_core_events(&ts_bytes);

        let artifact_rel = PathBuf::from(self.id()).join("input.ts");
        write_file(&out_dir.join(&artifact_rel), &ts_bytes);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core,
            extensions: serde_json::Value::Null,
        };
        (artifact_rel, golden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 9 — dvb-subtitle-mp (demux)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "demux"`: H.264 video + DVB Subtitling stream.
///
/// The subtitle stream uses `SubtitleCodec::DvbSubtitling` (PMT stream_type
/// 0x06, disambiguated by the auto-emitted `subtitling_descriptor` tag 0x59).
/// The muxer auto-wraps the caller's raw segment bytes in the EN 300 743 §6.2
/// PES data field envelope (`data_identifier=0x20`, `subtitle_stream_id=0x00`,
/// end marker `0xFF`); the demuxer strips the envelope and surfaces the inner
/// segment bytes in `SamplePayload::Subtitle::payload`.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct DvbSubtitleMp;

impl Scenario for DvbSubtitleMp {
    fn id(&self) -> &'static str {
        "dvb-subtitle-mp"
    }
    fn kind(&self) -> &'static str {
        "demux"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x1011, VideoCodec::H264);
            // DVB subtitling codec (PMT stream_type 0x06 + subtitling_descriptor).
            // Language "eng", subtitling_type 0x10 (DVB sub, no AR signalling),
            // composition and ancillary page IDs both 1.
            prog.add_subtitle(
                0x1041,
                SubtitleCodec::DvbSubtitling {
                    language: *b"eng",
                    subtitling_type: 0x10,
                    composition_page_id: 1,
                    ancillary_page_id: 1,
                },
            );
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().expect("valid muxer config")
        };
        let mut mux = Muxer::new(cfg).expect("muxer init");

        let pts = Pts90khz::new(90_000); // t=1s
        mux.push_video(&synthetic_h264_idr(), pts, /*key_frame=*/ true)
            .expect("push_video");
        // Push raw DVB subtitle segment bytes — muxer auto-wraps in
        // EN 300 743 §6.2 PES data field envelope.
        mux.push_subtitle(pts, &synthetic_dvb_subtitle_segment())
            .expect("push_subtitle");

        let ts_bytes = drain_mux(&mut mux);
        let core = demux_to_core_events(&ts_bytes);

        let artifact_rel = PathBuf::from(self.id()).join("input.ts");
        write_file(&out_dir.join(&artifact_rel), &ts_bytes);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core,
            extensions: serde_json::Value::Null,
        };
        (artifact_rel, golden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 10 — webvtt-in-ts (demux)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "demux"`: H.264 video + WebVTT-in-TS subtitle stream.
///
/// The subtitle stream uses `SubtitleCodec::WebVttInTs` (PMT stream_type 0x06,
/// disambiguated by the auto-emitted `registration_descriptor` with
/// `format_identifier = "VTTC"`). The `WebVttInTs` PES shape is passthrough —
/// no auto-prepend. The demuxer surfaces the raw cue bytes in
/// `SamplePayload::Subtitle::payload`.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct WebVttInTsScenario;

impl Scenario for WebVttInTsScenario {
    fn id(&self) -> &'static str {
        "webvtt-in-ts"
    }
    fn kind(&self) -> &'static str {
        "demux"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        let cfg = {
            let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
            prog.add_video(0x1011, VideoCodec::H264);
            // WebVTT-in-TS codec (PMT stream_type 0x06 + VTTC registration_descriptor).
            prog.add_subtitle(0x1042, SubtitleCodec::WebVttInTs);
            let mut b = MuxerConfig::builder();
            b.add_program(prog.build());
            b.build().expect("valid muxer config")
        };
        let mut mux = Muxer::new(cfg).expect("muxer init");

        let pts = Pts90khz::new(90_000); // t=1s
        mux.push_video(&synthetic_h264_idr(), pts, /*key_frame=*/ true)
            .expect("push_video");
        // Push raw WebVTT cue bytes — passthrough, no envelope added.
        mux.push_subtitle(pts, &synthetic_webvtt_cue())
            .expect("push_subtitle");

        let ts_bytes = drain_mux(&mut mux);
        let core = demux_to_core_events(&ts_bytes);

        let artifact_rel = PathBuf::from(self.id()).join("input.ts");
        write_file(&out_dir.join(&artifact_rel), &ts_bytes);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core,
            extensions: serde_json::Value::Null,
        };
        (artifact_rel, golden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 11 — malformed-psi-strict (binding_contract)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "binding_contract"`: hand-crafted TS with a valid PAT pointing to
/// a PMT PID, followed by a PMT whose CRC-32 is deliberately corrupted.
///
/// Under `StrictMode::Full` the demuxer converts
/// `NonConformantIssue::PsiChecksumMismatch` into
/// `DemuxError::StrictRejection`, which the adapter maps to `"STRICT_REJECTION"`.
///
/// **Distinction from `strict-rejection`:** The existing pilot feeds 8192 bytes
/// of `0xFF` — no 0x47 sync byte — triggering `DemuxError::Unrecoverable`
/// after exhausting the sync-search window. This scenario uses syntactically
/// valid TS packets with correct sync bytes and a valid PAT (correct CRC). The
/// demuxer acquires sync and parses the PAT successfully; the rejection fires
/// specifically from `PsiChecksumMismatch` on the PMT section's corrupted CRC,
/// exercising a different code path while producing the same stable golden code.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct MalformedPsiStrict;

impl Scenario for MalformedPsiStrict {
    fn id(&self) -> &'static str {
        "malformed-psi-strict"
    }
    fn kind(&self) -> &'static str {
        "binding_contract"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        // Two TS packets: PAT (valid CRC) + PMT (corrupted CRC last byte).
        // Under StrictMode::Full: PsiChecksumMismatch → StrictRejection →
        // "STRICT_REJECTION".
        let ts_bytes = synthetic_ts_bad_pmt_crc();

        let artifact_rel = PathBuf::from(self.id()).join("input.bin");
        write_file(&out_dir.join(&artifact_rel), &ts_bytes);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core: vec![CoreEvent::Error {
                code: "STRICT_REJECTION".to_string(),
            }],
            extensions: serde_json::Value::Null,
        };
        (artifact_rel, golden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 12 — malformed-pes-lenient (demux)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "demux"`: hand-crafted TS with a valid PAT + PMT declaring H.264
/// video at PID 0x0101, followed by a PES whose `flags1` byte (byte 6 of the
/// optional PES header) has its top two bits set to `0b00` instead of the
/// spec-required `0b10`. This violates H.222.0 V9 §2.4.3.6.
///
/// Under the DEFAULT (lenient) `DemuxerConfig` the demuxer:
///  1. Emits `DemuxEvent::NonConformant { issue: PesHeaderMalformed {
///     kind: InvalidMarkerBits } }` (stable code `"PES_HEADER_MALFORMED"`).
///  2. Recovers and emits `DemuxEvent::Sample` for the video PID.
///
/// The shared `demux_to_core_events` normaliser surfaces NonConformant events
/// as `CoreEvent::Error { code }` alongside the normal media events. The golden
/// includes both events in queue order (NonConformant fires before the Sample
/// event in the demuxer queue). The conformant Muxer emits zero NonConformant
/// events, so this added arm leaves the clean demux scenarios unchanged.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct MalformedPesLenient;

impl Scenario for MalformedPesLenient {
    fn id(&self) -> &'static str {
        "malformed-pes-lenient"
    }
    fn kind(&self) -> &'static str {
        "demux"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        let ts_bytes = synthetic_ts_malformed_pes_header();
        let core = demux_to_core_events(&ts_bytes);

        let artifact_rel = PathBuf::from(self.id()).join("input.ts");
        write_file(&out_dir.join(&artifact_rel), &ts_bytes);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core,
            extensions: serde_json::Value::Null,
        };
        (artifact_rel, golden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 13 — unknown-stream-type (demux)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "demux"`: hand-crafted TS with a valid PAT + PMT advertising
/// `stream_type = 0x02` (ISO/IEC 13818-2 MPEG-2 video — not parsed by tst-core)
/// at PID 0x0201, plus PES payload.
///
/// The demuxer classifies the PID as `StreamKind::Unknown(0x02)` and emits
/// `DemuxEvent::Sample { payload: SamplePayload::Unknown { .. } }`. The
/// normaliser's existing `SamplePayload::Unknown` arm converts this to
/// `CoreEvent::Unknown { pid: 0x0201 }`.
///
/// No new normaliser arm is required — the `Unknown` path already exists.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct UnknownStreamType;

impl Scenario for UnknownStreamType {
    fn id(&self) -> &'static str {
        "unknown-stream-type"
    }
    fn kind(&self) -> &'static str {
        "demux"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        // PID 0x0201 carries stream_type=0x02 (MPEG-2 video, not in tst-core's
        // known-type set). The normaliser maps SamplePayload::Unknown to
        // CoreEvent::Unknown { pid: 0x0201 }.
        let ts_bytes = synthetic_ts_unknown_stream_type();
        let core = demux_to_core_events(&ts_bytes);

        let artifact_rel = PathBuf::from(self.id()).join("input.ts");
        write_file(&out_dir.join(&artifact_rel), &ts_bytes);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core,
            extensions: serde_json::Value::Null,
        };
        (artifact_rel, golden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 14 — audio-klv-roundtrip (roundtrip)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "roundtrip"`: mux synthetic H.264 video + AAC audio + SYNCHRONOUS
/// KLV into one TS program; golden is the output bytes + sha256 (byte-identity).
///
/// Unlike `video-roundtrip` (video-only), this exercises a fully-populated
/// single program (video + audio + sync KLV) through the deterministic mux
/// path. SynchronousMetadata KLV requires `carries_pts = true`; callers pass
/// RAW ST 0601 LS bytes and the muxer auto-wraps the 5-byte AU cell header per
/// ITU-T H.222.0 §2.12.4.2.
///
/// The recipe lives in `audio_klv_roundtrip_ts_bytes()` (grouped with the other
/// roundtrip recipe fns near the top of this file).
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct AudioKlvRoundtrip;

impl Scenario for AudioKlvRoundtrip {
    fn id(&self) -> &'static str {
        "audio-klv-roundtrip"
    }
    fn kind(&self) -> &'static str {
        "roundtrip"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        // Produced via the shared single-source-of-truth helper so the adapter
        // test can re-run the identical recipe.
        // This generator NEVER reads from testfiles/ or local/ directories.
        let ts_bytes = audio_klv_roundtrip_ts_bytes();
        let digest = sha256_hex(&ts_bytes);

        // Write input artifact (the TS output IS the golden artifact for
        // roundtrip verification).
        let artifact_rel = PathBuf::from(self.id()).join("output.ts");
        let artifact_abs = out_dir.join(&artifact_rel);
        write_file(&artifact_abs, &ts_bytes);

        // Roundtrip carries no media events — the whole-stream byte-identity
        // digest lives under `extensions.output_sha256` (matching the
        // `video-roundtrip` convention).
        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core: vec![],
            extensions: serde_json::json!({ "output_sha256": digest }),
        };
        (artifact_rel, golden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 15 — video-dts-roundtrip (roundtrip)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "roundtrip"`: mux a synthetic H.264 IDR with **distinct PTS and DTS**
/// (`PTS = 9000`, `DTS = 6000` ticks at 90 kHz) using
/// `Muxer::push_video_to_with_dts`.  The golden records the whole-stream
/// SHA-256; all three bindings (Rust, C, JVM) re-mux the identical recipe and
/// assert the same digest, proving that BIND-01's cross-binding DTS parity
/// requirement holds.
///
/// Using `push_video_to_with_dts` (via the explicit-handle path) forces the
/// muxer to emit `PTS_DTS_flags = '11'` in the PES header — the DTS appears as
/// a separate 5-byte field — whereas `push_video` always emits
/// `PTS_DTS_flags = '10'` (PTS-only).  The difference is visible in the committed
/// `output.ts` and in every re-mux produced by the three adapters.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct VideoDtsRoundtrip;

impl Scenario for VideoDtsRoundtrip {
    fn id(&self) -> &'static str {
        "video-dts-roundtrip"
    }
    fn kind(&self) -> &'static str {
        "roundtrip"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        // Produced via the shared single-source-of-truth helper so the adapter
        // test can re-run the identical recipe.
        // This generator NEVER reads from testfiles/ or local/ directories.
        let ts_bytes = video_dts_roundtrip_ts_bytes();
        let digest = sha256_hex(&ts_bytes);

        // Write input artifact (the TS output IS the golden artifact for
        // roundtrip verification).
        let artifact_rel = PathBuf::from(self.id()).join("output.ts");
        let artifact_abs = out_dir.join(&artifact_rel);
        write_file(&artifact_abs, &ts_bytes);

        // Roundtrip carries no media events — the whole-stream byte-identity
        // digest lives under `extensions.output_sha256` (matching the
        // `video-roundtrip` convention).
        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core: vec![],
            extensions: serde_json::json!({ "output_sha256": digest }),
        };
        (artifact_rel, golden)
    }
}

/// Re-run the `video-dts-roundtrip` mux and return the deterministic TS bytes.
///
/// Single source of truth shared by `VideoDtsRoundtrip::generate` and the Rust
/// adapter test — no hand-retyped mux recipe.
///
/// Uses `push_video_to_with_dts` so the PES header carries separate PTS and DTS
/// fields (`PTS_DTS_flags = '11'`).  Fixed timestamps: `PTS = 9000` ticks,
/// `DTS = 6000` ticks (90 kHz), representing a reordered B-frame at roughly
/// 100 ms display time and 66 ms decode time.
pub fn video_dts_roundtrip_ts_bytes() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().expect("valid muxer config")
    };
    let mut mux = Muxer::new(cfg).expect("muxer init");
    // Fetch the lone video-stream handle so push_video_to_with_dts can target it.
    let handle = mux.video_stream_handle(0).expect("video handle 0");
    // PTS 9000 / DTS 6000 ticks (90 kHz) — fixed so the golden is stable.
    mux.push_video_to_with_dts(
        handle,
        &synthetic_h264_idr(),
        Pts90khz::new(9_000),
        Pts90khz::new(6_000),
        /*key_frame=*/ true,
    )
    .expect("push_video_to_with_dts");
    drain_mux(&mut mux)
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 15b — video-misp-roundtrip (roundtrip)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "roundtrip"`: muxes one H.264 AU with an embedded ST 0604 MISP
/// microsecond timestamp, then the adapters demux the TS, extract the MISP
/// timestamp, and assert round-trip equality.
///
/// Uses `push_video_misp_to` so the PES carries a MISP SEI NAL before the
/// first VCL NAL.  Fixed parameters: `MispTimestamp::micros(0x0005_F5E1_0000_0001, 0x1F)`,
/// `PTS = 9000` ticks (90 kHz), matching the DTS-roundtrip PTS so the two
/// scenarios share the same timestamp slot.
///
/// The golden `extensions` carries three fields:
///   - `output_sha256`  — byte-identity digest of the whole TS output.
///   - `misp_kind`      — 0 = Micro (u8).
///   - `misp_time_status` — 0x1F (u8).
///   - `misp_value`     — 0x0005F5E100000001 as a decimal u64 string.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct VideoMispRoundtrip;

impl Scenario for VideoMispRoundtrip {
    fn id(&self) -> &'static str {
        "video-misp-roundtrip"
    }
    fn kind(&self) -> &'static str {
        "roundtrip"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        // Produced via the shared single-source-of-truth helper.
        // This generator NEVER reads from testfiles/ or local/ directories.
        let ts_bytes = video_misp_roundtrip_ts_bytes();
        let digest = sha256_hex(&ts_bytes);

        // Write input artifact (the TS output IS the golden artifact for
        // roundtrip verification).
        let artifact_rel = PathBuf::from(self.id()).join("output.ts");
        let artifact_abs = out_dir.join(&artifact_rel);
        write_file(&artifact_abs, &ts_bytes);

        // Extract the MISP timestamp from the muxed AU to record in the golden.
        // We demux the TS to recover the AU bytes, then call misp_extract.
        let misp_golden = extract_misp_from_ts(&ts_bytes);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core: vec![],
            extensions: serde_json::json!({
                "output_sha256": digest,
                "misp_kind": misp_golden.0,
                "misp_time_status": misp_golden.1,
                "misp_value": misp_golden.2,
            }),
        };
        (artifact_rel, golden)
    }
}

/// Re-run the `video-misp-roundtrip` mux and return the deterministic TS bytes.
///
/// Single source of truth shared by `VideoMispRoundtrip::generate` and the Rust
/// adapter test — no hand-retyped mux recipe.
///
/// Uses `push_video_misp_to` so the PES header carries an embedded MISP SEI NAL.
/// Fixed timestamp: `MispTimestamp::micros(0x0005_F5E1_0000_0001, 0x1F)`.
/// Fixed PTS: 9000 ticks (90 kHz).
pub fn video_misp_roundtrip_ts_bytes() -> Vec<u8> {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
        prog.add_video(0x1011, VideoCodec::H264);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().expect("valid muxer config")
    };
    let mut mux = Muxer::new(cfg).expect("muxer init");
    // Fetch the lone video-stream handle so push_video_misp_to can target it.
    let handle = mux.video_stream_handle(0).expect("video handle 0");
    let misp = MispTimestamp::micros(0x0005_F5E1_0000_0001, 0x1F);
    // PTS 9000 ticks (90 kHz) — fixed so the golden is stable.
    mux.push_video_misp_to(
        handle,
        &synthetic_h264_idr(),
        Pts90khz::new(9_000),
        true,
        &misp,
    )
    .expect("push_video_misp_to");
    drain_mux(&mut mux)
}

/// Demux a TS produced by `video_misp_roundtrip_ts_bytes`, find the first Video
/// AU, extract the MISP timestamp, and return `(kind_u8, time_status, value)`.
///
/// Panics if the MISP timestamp is not found — used only during golden
/// generation to derive the committed golden values.
fn extract_misp_from_ts(ts_bytes: &[u8]) -> (u8, u8, u64) {
    use tst_core::mpegts::demux::event::SamplePayload;
    use tst_core::mpegts::demux::{DemuxEvent, Demuxer};
    let mut demuxer = Demuxer::new();
    demuxer.feed(ts_bytes).expect("demux feed");
    demuxer.flush();
    while let Some(event) = demuxer.next_event() {
        if let DemuxEvent::Sample {
            payload: SamplePayload::Video { raw, .. },
            ..
        } = event
        {
            let extracted = misp_extract(&raw, VideoCodec::H264)
                .expect("misp_extract error")
                .expect("no MISP timestamp found in video AU");
            use tst_core::codec::misp_time::MispTimeKind;
            let kind_u8 = match extracted.kind {
                MispTimeKind::Micro => 0u8,
                MispTimeKind::Nano => 1u8,
                _ => panic!("unknown MispTimeKind variant"),
            };
            return (kind_u8, extracted.time_status, extracted.value);
        }
    }
    panic!("no video event found in MISP-roundtrip TS — cannot derive golden values");
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 16 — drop-idempotence (binding_contract, lifecycle)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "binding_contract"`: asserts that closing/dropping a demuxer twice
/// is safe (no panic, no double-free, no use-after-free).
///
/// This is a LIFECYCLE contract, reusing the `CoreEvent::Error` envelope as a
/// contract-assertion carrier with the sentinel code `"DOUBLE_CLOSE_OK"` and an
/// `extensions.contract = "drop_idempotence"` tag naming the contract. The
/// sentinel is NOT a demux error code — it is a contract assertion defined by
/// this suite; the C adapter (Task 13) maps it to its real
/// `tst_demuxer_close`-called-twice guard, the Python adapter to its `close()`
/// idempotence.
///
/// The "input" is a minimal valid synthetic TS (a tiny H.264 video stream,
/// reusing the `video-roundtrip` recipe) so every binding has a real demuxer to
/// open + close twice. This generator NEVER reads an external fixture.
///
/// **Rust side:** the pure-Rust `Demuxer` has no explicit `close()` method — it
/// relies on `Drop`. "Double close" in Rust is therefore expressed as: feed the
/// input, `flush()` (the explicit end-of-stream finaliser) twice, then `drop`;
/// the nearest real guarantee is that a fresh `Demuxer` constructed afterwards
/// also works. The Rust adapter exercises this and emits the sentinel; the
/// binding-specific double-`close()` teeth live in the C/Python adapters.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct DropIdempotence;

impl Scenario for DropIdempotence {
    fn id(&self) -> &'static str {
        "drop-idempotence"
    }
    fn kind(&self) -> &'static str {
        "binding_contract"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        // Minimal valid TS so the binding adapters have a real demuxer to open
        // and close twice. Reuses the deterministic video-roundtrip mux recipe.
        // This generator NEVER reads from testfiles/ or local/ directories.
        let ts_bytes = video_roundtrip_ts_bytes();

        let artifact_rel = PathBuf::from(self.id()).join("input.ts");
        write_file(&out_dir.join(&artifact_rel), &ts_bytes);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core: vec![CoreEvent::Error {
                code: "DOUBLE_CLOSE_OK".to_string(),
            }],
            extensions: serde_json::json!({ "contract": "drop_idempotence" }),
        };
        (artifact_rel, golden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 16 — forged-handle (binding_contract, lifecycle)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "binding_contract"`: asserts that a forged / garbage handle passed
/// across a trust boundary is rejected, not dereferenced.
///
/// This is PRIMARILY a C-ABI contract — a forged `tst_*` handle must be
/// rejected, not deref'd into a freed/invalid pointer. The contract is carried
/// in the `CoreEvent::Error` envelope with sentinel code `"INVALID_HANDLE"` and
/// `extensions.contract = "forged_handle"`.
///
/// **Rust side (approach a — a real guard exists):** tst-core exposes
/// `VideoStreamHandle::try_from_raw`, the validating sibling of `from_raw` used
/// at every FFI / PyO3 trust boundary. The canonical handle mask is `0xFF`
/// (4-bit program + 4-bit within); a forged `u32` with any bit above `0xFF` set
/// (e.g. `0x100`, which sets bit 8, just past the mask) is rejected with
/// `MuxError::InvalidStreamHandle`. The Rust adapter asserts this rejection
/// directly, so the `INVALID_HANDLE` sentinel corresponds to a real pure-Rust
/// guard — not a fabricated assertion. The raw-pointer-deref teeth (a forged
/// opaque pointer must not be dereferenced) remain a C-adapter concern (Task 13).
///
/// The "input" artifact is the forged handle value as 4 little-endian bytes, so
/// the C/Python adapters can read the same forged value the Rust side rejects.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct ForgedHandle;

/// The forged handle value the adapters assert is rejected. The canonical mask
/// is `0xFF` (4-bit program + 4-bit within); `0x100` sets bit 8, just past the
/// mask, so `try_from_raw` returns `Err(InvalidStreamHandle)`. Shared single
/// source of truth between `ForgedHandle::generate` and the Rust adapter test.
pub const FORGED_HANDLE_RAW: u32 = 0x100;

impl Scenario for ForgedHandle {
    fn id(&self) -> &'static str {
        "forged-handle"
    }
    fn kind(&self) -> &'static str {
        "binding_contract"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        // The forged handle value as 4 little-endian bytes — the same value the
        // C/Python adapters read and pass to their handle-rewrap guard.
        // This generator NEVER reads from testfiles/ or local/ directories.
        let forged = FORGED_HANDLE_RAW.to_le_bytes().to_vec();

        let artifact_rel = PathBuf::from(self.id()).join("input.bin");
        write_file(&out_dir.join(&artifact_rel), &forged);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core: vec![CoreEvent::Error {
                code: "INVALID_HANDLE".to_string(),
            }],
            extensions: serde_json::json!({ "contract": "forged_handle" }),
        };
        (artifact_rel, golden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Scenario 17 — exception-kind-stability (binding_contract, lifecycle)
// ─────────────────────────────────────────────────────────────────────────────

/// `kind = "binding_contract"`: asserts the SAME stable public code surfaces
/// across all bindings for the same malformed input — exception/error-kind
/// stability across the language boundary.
///
/// Reuses the `malformed-psi-strict` malformation (a valid PAT + a PMT with a
/// deliberately corrupted CRC-32). Under `StrictMode::Full` the demuxer
/// converts `PsiChecksumMismatch` → `DemuxError::StrictRejection`, mapped to
/// the stable public code `"STRICT_REJECTION"`. `STRICT_REJECTION` is a REAL
/// stable code (already used by `strict-rejection` + `malformed-psi-strict`);
/// this scenario asserts that code is identical regardless of which binding
/// runs the demux. The `extensions.contract = "exception_kind_stability"` tag
/// names the contract.
///
/// This generator NEVER reads from `testfiles/`, `local/`, or any real corpus.
struct ExceptionKindStability;

impl Scenario for ExceptionKindStability {
    fn id(&self) -> &'static str {
        "exception-kind-stability"
    }
    fn kind(&self) -> &'static str {
        "binding_contract"
    }
    fn features(&self) -> Vec<&'static str> {
        vec![]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        // Same malformation as malformed-psi-strict: PAT (valid CRC) + PMT
        // (corrupted CRC). Under StrictMode::Full: PsiChecksumMismatch →
        // StrictRejection → "STRICT_REJECTION".
        // This shares both the input bytes AND the STRICT_REJECTION core with
        // malformed-psi-strict; the only thing distinguishing the two goldens is
        // the `extensions.contract` tag below (malformed-psi-strict carries
        // `extensions: Null`). That is deliberate — this scenario's assertion is
        // that the SAME stable code surfaces, named by the contract tag.
        // This generator NEVER reads from testfiles/ or local/ directories.
        let ts_bytes = synthetic_ts_bad_pmt_crc();

        let artifact_rel = PathBuf::from(self.id()).join("input.bin");
        write_file(&out_dir.join(&artifact_rel), &ts_bytes);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core: vec![CoreEvent::Error {
                code: "STRICT_REJECTION".to_string(),
            }],
            extensions: serde_json::json!({ "contract": "exception_kind_stability" }),
        };
        (artifact_rel, golden)
    }
}

// -----------------------------------------------------------------------------
// Scenario 20 - cancelled-recv-kind (binding_contract)
// -----------------------------------------------------------------------------

/// `kind = "binding_contract"`, `features = ["srt"]`: a plain SRT receive parked
/// on a silent peer and cancelled from another thread must surface the SAME
/// kind in every binding - Rust `BindingErrorKind::Closed` ("CLOSED"), C
/// `TST_E_CLOSED` (-7), Python `SrtErrorKind.CLOSED`, JVM
/// `SrtException.Kind.CLOSED` - with the detail `cancelled from another
/// thread` (Arc 2 spec 3.3 / 6). The generator is socket-free: it writes the
/// one TS null packet the adapters' idle peer may send as a rescue and returns
/// the expected envelope; the live cancel runs inside each adapter.
///
/// This generator NEVER reads from testfiles/ or local/ directories.
struct CancelledRecvKind;

impl Scenario for CancelledRecvKind {
    fn id(&self) -> &'static str {
        "cancelled-recv-kind"
    }
    fn kind(&self) -> &'static str {
        "binding_contract"
    }
    fn features(&self) -> Vec<&'static str> {
        vec!["srt"]
    }
    fn tier(&self) -> &'static str {
        "A"
    }

    fn generate(&self, out_dir: &Path) -> (PathBuf, Golden) {
        // One TS null packet (PID 0x1FFF): the rescue frame an adapter sends
        // from the still-connected peer when a cancel failed to end the parked
        // receive, so the reader thread is never left in a native read.
        let pkt = ts_packet(0x1FFF, false, &[]);

        let artifact_rel = PathBuf::from(self.id()).join("input.bin");
        write_file(&out_dir.join(&artifact_rel), &pkt);

        let golden = Golden {
            schema_version: 0,
            lossy: false,
            core: vec![CoreEvent::Error {
                code: "CLOSED".to_string(),
            }],
            extensions: serde_json::json!({
                "contract": "cancelled_recv_kind",
                "detail": "cancelled from another thread",
                "c_code": -7
            }),
        };
        (artifact_rel, golden)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Hand-crafted TS input helpers (Scenarios 11–13, 17)
// ─────────────────────────────────────────────────────────────────────────────
//
// Low-level TS/PSI/PES byte assembly used by the malformed-input and
// lifecycle-contract scenarios (`synthetic_ts_bad_pmt_crc` is shared by
// Scenarios 11 and 17); the codec-frame builders live in "Synthetic data
// helpers" above.

/// Build a 188-byte TS packet. `pid` must fit in 13 bits. `payload_unit_start`
/// sets PUSI. `payload` must be ≤ 184 bytes; the remainder is stuffed with 0xFF.
fn ts_packet(pid: u16, payload_unit_start: bool, payload: &[u8]) -> Vec<u8> {
    assert!(pid <= 0x1FFF, "PID out of range");
    assert!(payload.len() <= 184, "payload too large for one TS packet");
    let mut pkt = vec![0u8; 188];
    pkt[0] = 0x47; // sync byte
    let pusi_bit: u8 = if payload_unit_start { 0x40 } else { 0x00 };
    pkt[1] = pusi_bit | ((pid >> 8) as u8 & 0x1F);
    pkt[2] = pid as u8;
    // adaptation_field_control = 0b01 (payload only, no adaptation field), CC=0
    pkt[3] = 0x10;
    pkt[4..].fill(0xFF); // stuff unused portion with 0xFF
    pkt[4..4 + payload.len()].copy_from_slice(payload);
    pkt
}

/// Build a TS packet for a PSI section (PAT or PMT). PSI sections require a
/// `pointer_field` byte (0x00) immediately after the 4-byte TS header when
/// PUSI=1 (ISO/IEC 13818-1 §2.4.4.1).
fn ts_psi_packet(pid: u16, section: &[u8]) -> Vec<u8> {
    assert!(
        section.len() <= 183,
        "PSI section too large for one TS packet (need room for pointer_field)"
    );
    let mut payload = vec![0x00]; // pointer_field = 0
    payload.extend_from_slice(section);
    ts_packet(pid, /*pusi=*/ true, &payload)
}

/// Compute CRC-32/MPEG-2 and append the 4-byte big-endian result to `data`.
fn append_crc(data: &mut Vec<u8>) {
    let crc = tst_core::mpegts::common::crc32::crc32_mpeg2(data);
    data.extend_from_slice(&crc.to_be_bytes());
}

/// Build a minimal PAT section for one program: `program_number` → `pmt_pid`.
///
/// Returns a complete PSI section including 4-byte CRC trailer.
fn build_pat_section(transport_stream_id: u16, program_number: u16, pmt_pid: u16) -> Vec<u8> {
    // section_length = 5 (fixed post-length fields) + 4 (one entry) + 4 (CRC) = 13.
    let section_length: u16 = 13;
    let mut sec = Vec::new();
    sec.push(0x00); // table_id = PAT
    // section_syntax_indicator=1, '0'=0, reserved=0b11, section_length high nibble
    sec.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
    sec.push(section_length as u8);
    sec.extend_from_slice(&transport_stream_id.to_be_bytes());
    sec.push(0xC1); // reserved(2)=0b11, version_number(5)=0, current_next(1)=1
    sec.push(0x00); // section_number
    sec.push(0x00); // last_section_number
    // Program entry: program_number(16) + reserved(3)=0b111 + PMT_PID(13)
    sec.extend_from_slice(&program_number.to_be_bytes());
    let pmt_pid_field: u16 = 0xE000 | (pmt_pid & 0x1FFF);
    sec.extend_from_slice(&pmt_pid_field.to_be_bytes());
    append_crc(&mut sec);
    sec
}

/// Build a minimal PMT section for one elementary stream.
///
/// `corrupt_crc`: when `true`, the last byte of the appended CRC is flipped,
/// producing a checksum mismatch that the demuxer surfaces as
/// `NonConformantIssue::PsiChecksumMismatch`.
fn build_pmt_section(
    program_number: u16,
    pcr_pid: u16,
    es_pid: u16,
    stream_type: u8,
    corrupt_crc: bool,
) -> Vec<u8> {
    // PMT section content after the 3-byte header (table_id + section_length):
    //   program_number(2) + version/current_next(1) + section_number(1) +
    //   last_section_number(1)                                          = 5
    //   PCR_PID(2) + program_info_length(2)                             = 4
    //   stream_type(1) + ES_PID(2) + ES_info_length(2)                  = 5
    //   CRC_32(4)                                                        = 4
    //   total section_length = 5 + 4 + 5 + 4                           = 18
    let section_length: u16 = 18;
    let mut sec = Vec::new();
    sec.push(0x02); // table_id = PMT
    sec.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
    sec.push(section_length as u8);
    sec.extend_from_slice(&program_number.to_be_bytes());
    sec.push(0xC1); // reserved + version=0 + current_next=1
    sec.push(0x00); // section_number
    sec.push(0x00); // last_section_number
    let pcr_pid_field: u16 = 0xE000 | (pcr_pid & 0x1FFF);
    sec.extend_from_slice(&pcr_pid_field.to_be_bytes());
    sec.extend_from_slice(&[0xF0, 0x00]); // reserved(4) + program_info_length(12) = 0
    // Stream entry: stream_type(8) + reserved(3) + ES_PID(13) + reserved(4) + ES_info_length(12)=0
    sec.push(stream_type);
    let es_pid_field: u16 = 0xE000 | (es_pid & 0x1FFF);
    sec.extend_from_slice(&es_pid_field.to_be_bytes());
    sec.extend_from_slice(&[0xF0, 0x00]); // reserved + ES_info_length = 0
    append_crc(&mut sec);
    if corrupt_crc {
        let last = sec.len() - 1;
        sec[last] ^= 0xFF;
    }
    sec
}

/// Build a minimal H.264 video PES packet.
///
/// `malformed_flags1`: when `true`, sets the `flags1` byte (PES byte 6) to
/// `0x00` instead of the conformant `0x80`, triggering
/// `PesHeaderMalformed { kind: InvalidMarkerBits }` in the demuxer.
fn build_pes_h264(malformed_flags1: bool) -> Vec<u8> {
    let es_payload = synthetic_h264_idr();
    let flags1: u8 = if malformed_flags1 { 0x00 } else { 0x80 };
    let mut pes = Vec::new();
    pes.extend_from_slice(&[0x00, 0x00, 0x01]); // PES start code prefix
    pes.push(0xE0); // stream_id: video_stream_0
    pes.extend_from_slice(&[0x00, 0x00]); // PES_packet_length = 0 (unbounded)
    pes.push(flags1); // flags1
    pes.push(0x00); // flags2: PTS_DTS_flags=0, no timestamps
    pes.push(0x00); // header_data_length = 0
    pes.extend_from_slice(&es_payload);
    pes
}

/// Build a minimal PES packet for an unrecognized stream type.
fn build_pes_unknown() -> Vec<u8> {
    let mut pes = Vec::new();
    pes.extend_from_slice(&[0x00, 0x00, 0x01]);
    // stream_id = 0xBD (private_stream_1); arbitrary for this unknown-stream-type test.
    pes.push(0xBD);
    pes.extend_from_slice(&[0x00, 0x00]); // PES_packet_length = 0
    pes.push(0x80); // flags1 (conformant)
    pes.push(0x00); // flags2
    pes.push(0x00); // header_data_length = 0
    pes.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // synthetic payload
    pes
}

/// Synthetic TS for Scenario 11: PAT (valid CRC) + PMT (corrupted CRC) + PES.
///
/// Under `StrictMode::Full` the PMT's `PsiChecksumMismatch` → `StrictRejection`
/// → `"STRICT_REJECTION"`.
fn synthetic_ts_bad_pmt_crc() -> Vec<u8> {
    let pat = build_pat_section(0x0001, 0x0001, 0x0100);
    let pmt = build_pmt_section(0x0001, 0x0101, 0x0101, 0x1B, /*corrupt_crc=*/ true);

    let mut out = Vec::new();
    out.extend(ts_psi_packet(0x0000, &pat));
    out.extend(ts_psi_packet(0x0100, &pmt));
    // One PES packet for completeness (strict rejection fires before it's processed).
    let pes = build_pes_h264(/*malformed_flags1=*/ false);
    out.extend(ts_packet(0x0101, /*pusi=*/ true, &pes));
    out
}

/// Synthetic TS for Scenario 12: PAT (valid) + PMT (valid, H.264 at 0x0101)
/// + first PES with malformed `flags1` + second PES to trigger PUSI boundary.
///
/// Under lenient config the demuxer emits `NonConformant` then `Sample`. The
/// second PUSI on PID 0x0101 finalises the first (malformed) PES during
/// `feed()` so the events surface before `flush()`.
fn synthetic_ts_malformed_pes_header() -> Vec<u8> {
    let pat = build_pat_section(0x0001, 0x0001, 0x0100);
    let pmt = build_pmt_section(0x0001, 0x0101, 0x0101, 0x1B, /*corrupt_crc=*/ false);

    let mut out = Vec::new();
    out.extend(ts_psi_packet(0x0000, &pat));
    out.extend(ts_psi_packet(0x0100, &pmt));
    let pes1 = build_pes_h264(/*malformed_flags1=*/ true);
    out.extend(ts_packet(0x0101, /*pusi=*/ true, &pes1));
    // Second PUSI on PID 0x0101 finalises pes1 → emits NonConformant + Sample.
    let pes2 = build_pes_h264(/*malformed_flags1=*/ false);
    out.extend(ts_packet(0x0101, /*pusi=*/ true, &pes2));
    out
}

/// Synthetic TS for Scenario 13: PAT (valid) + PMT declaring `stream_type=0x02`
/// (MPEG-2 video — unknown to tst-core) at PID 0x0201, plus two PES packets.
///
/// `tst-core` classifies PID 0x0201 as `StreamKind::Unknown(0x02)`. The
/// demuxer emits `SamplePayload::Unknown`, which the normaliser maps to
/// `CoreEvent::Unknown { pid: 0x0201 }`.
fn synthetic_ts_unknown_stream_type() -> Vec<u8> {
    let pat = build_pat_section(0x0001, 0x0001, 0x0200);
    // stream_type = 0x02: ISO/IEC 13818-2 MPEG-2 video (not in tst-core's known set)
    let pmt = build_pmt_section(0x0001, 0x0201, 0x0201, 0x02, /*corrupt_crc=*/ false);

    let mut out = Vec::new();
    out.extend(ts_psi_packet(0x0000, &pat));
    out.extend(ts_psi_packet(0x0200, &pmt));
    let pes = build_pes_unknown();
    out.extend(ts_packet(0x0201, /*pusi=*/ true, &pes));
    // Second PUSI to trigger PES boundary and emit the event.
    let pes2 = build_pes_unknown();
    out.extend(ts_packet(0x0201, /*pusi=*/ true, &pes2));
    out
}
