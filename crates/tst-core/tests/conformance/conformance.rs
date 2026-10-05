//! Conformance-vector tests. Each fixture pair (`<name>.bin` + `<name>.json`)
//! under `tests/fixtures/conformance/<codec>/` is parsed by the relevant
//! `tst_core::codec` entry point and validated against the sidecar's
//! declared expectations.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

use tst_core::codec::{ChromaFormat, CodecParseError};

/// Fixtures with known parser bugs. The test runner SKIPS these fixtures
/// instead of asserting against them. Each entry must have a TODO comment
/// pointing at the issue tracker.
///
/// When the parser is fixed, REMOVE the entry from this list and let the
/// test exercise the fixture normally. If the sidecar's expected.fields
/// are still correct (they should be — they describe what the parser
/// SHOULD produce), the test will pass.
const KNOWN_PARSER_BUGS: &[(&str, &str, &str)] = &[
    // (codec, fixture_name, bug_summary)
    // Empty — DBLK_A_MAIN10_VIXS_4 entry removed after fixing the
    // SPS-context delta_idx_minus1 over-read in
    // crates/tst-core/src/codec/h265/short_term_rps.rs.
];

fn is_known_bug(codec: &str, name: &str) -> Option<&'static str> {
    KNOWN_PARSER_BUGS
        .iter()
        .find(|(c, n, _)| *c == codec && *n == name)
        .map(|(_, _, summary)| *summary)
}

#[derive(Debug, Deserialize)]
struct Sidecar {
    // Retained for documentation; not asserted by the test runner.
    #[allow(dead_code)]
    source: String,
    kind: String,
    expected: Expected,
}

#[derive(Debug, Deserialize)]
struct Expected {
    outcome: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    fields: Option<BTreeMap<String, serde_json::Value>>,
}

fn conformance_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/conformance")
}

fn load_pairs(codec: &str) -> Vec<(String, Vec<u8>, Sidecar)> {
    let dir = conformance_root().join(codec);
    if !dir.exists() {
        return vec![];
    }
    let mut out = vec![];
    for entry in std::fs::read_dir(&dir).expect("read_dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("bin") {
            continue;
        }
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let bin = std::fs::read(&path).expect("read bin");
        let json_path = path.with_extension("json");
        let json_text = std::fs::read_to_string(&json_path).expect("read json");
        let sidecar: Sidecar = serde_json::from_str(&json_text).expect("parse sidecar");
        out.push((stem, bin, sidecar));
    }
    out
}

fn error_variant_name(e: &CodecParseError) -> &'static str {
    match e {
        CodecParseError::TruncatedRbsp { .. } => "TruncatedRbsp",
        CodecParseError::InvalidGolomb { .. } => "InvalidGolomb",
        CodecParseError::ReservedValue { .. } => "ReservedValue",
        CodecParseError::UnsupportedProfile { .. } => "UnsupportedProfile",
        CodecParseError::DanglingSpsReference { .. } => "DanglingSpsReference",
        CodecParseError::DanglingVpsReference { .. } => "DanglingVpsReference",
        CodecParseError::EngineError(_) => "EngineError",
        CodecParseError::InvalidLeb128 { .. } => "InvalidLeb128",
        CodecParseError::BadSyncWord { .. } => "BadSyncWord",
        CodecParseError::Truncated { .. } => "Truncated",
        CodecParseError::Forbidden { .. } => "Forbidden",
        // CodecParseError is #[non_exhaustive]; future variants map to their
        // debug name so that test failures are self-describing.
        _ => "Unknown",
    }
}

fn chroma_format_name(c: ChromaFormat) -> &'static str {
    match c {
        ChromaFormat::Monochrome => "Monochrome",
        ChromaFormat::Yuv420 => "Yuv420",
        ChromaFormat::Yuv422 => "Yuv422",
        ChromaFormat::Yuv444 => "Yuv444",
    }
}

#[test]
fn h264_conformance_vectors() {
    let pairs = load_pairs("h264");
    assert!(!pairs.is_empty(), "no H.264 fixtures found");
    for (name, bin, sidecar) in pairs {
        if let Some(summary) = is_known_bug("h264", &name) {
            eprintln!("SKIP h264/{name}: {summary}");
            continue;
        }
        eprintln!("checking h264/{name}");
        assert_eq!(sidecar.kind, "h264_sps", "{name}: kind must be h264_sps");
        let result = tst_core::codec::h264::parse_sps(&bin);
        validate_result_h264(&name, &result, &sidecar.expected);
    }
}

fn validate_result_h264(
    name: &str,
    result: &Result<tst_core::codec::h264::H264Sps, CodecParseError>,
    expected: &Expected,
) {
    match (expected.outcome.as_str(), result) {
        ("ok", Ok(sps)) => {
            if let Some(fields) = &expected.fields {
                for (k, v) in fields {
                    match k.as_str() {
                        "profile_idc" => assert_eq!(
                            v.as_u64().unwrap() as u8,
                            sps.profile_idc,
                            "{name}: profile_idc"
                        ),
                        "level_idc" => assert_eq!(
                            v.as_u64().unwrap() as u8,
                            sps.level_idc,
                            "{name}: level_idc"
                        ),
                        "width" => {
                            assert_eq!(v.as_u64().unwrap() as u32, sps.width, "{name}: width")
                        }
                        "height" => {
                            assert_eq!(v.as_u64().unwrap() as u32, sps.height, "{name}: height")
                        }
                        "bit_depth_luma" => assert_eq!(
                            v.as_u64().unwrap() as u8,
                            sps.bit_depth_luma,
                            "{name}: bit_depth_luma"
                        ),
                        "bit_depth_chroma" => assert_eq!(
                            v.as_u64().unwrap() as u8,
                            sps.bit_depth_chroma,
                            "{name}: bit_depth_chroma"
                        ),
                        "chroma_format" => assert_eq!(
                            v.as_str().unwrap(),
                            chroma_format_name(sps.chroma_format),
                            "{name}: chroma_format"
                        ),
                        other => panic!("{name}: unknown expected.fields key '{other}'"),
                    }
                }
            }
        }
        ("err", Err(e)) => {
            let want = expected
                .error
                .as_deref()
                .expect("err outcome requires .error");
            assert_eq!(want, error_variant_name(e), "{name}: error variant");
        }
        ("ok", Err(e)) => panic!("{name}: expected Ok, got Err({e:?})"),
        ("err", Ok(_)) => panic!("{name}: expected Err, got Ok"),
        (other, _) => panic!("{name}: unknown outcome '{other}'"),
    }
}

#[test]
fn h265_conformance_vectors() {
    let pairs = load_pairs("h265");
    assert!(!pairs.is_empty(), "no H.265 fixtures found");
    for (name, bin, sidecar) in pairs {
        if let Some(summary) = is_known_bug("h265", &name) {
            eprintln!("SKIP h265/{name}: {summary}");
            continue;
        }
        eprintln!("checking h265/{name}");
        // h265::sps/vps/pps are private submodules; types are re-exported at h265::*.
        let result_kind: Result<H265AnyResult, CodecParseError> = match sidecar.kind.as_str() {
            "h265_sps" => tst_core::codec::h265::parse_sps(&bin).map(H265AnyResult::Sps),
            "h265_vps" => tst_core::codec::h265::parse_vps(&bin).map(H265AnyResult::Vps),
            "h265_pps" => tst_core::codec::h265::parse_pps(&bin).map(H265AnyResult::Pps),
            other => panic!("{name}: unsupported kind '{other}'"),
        };
        validate_result_h265(&name, &result_kind, &sidecar.expected);
    }
}

// Vps and Pps variants are defined for future fixtures; no field checks today.
#[allow(dead_code)]
enum H265AnyResult {
    Sps(tst_core::codec::h265::H265Sps),
    Vps(tst_core::codec::h265::H265Vps),
    Pps(tst_core::codec::h265::H265Pps),
}

fn validate_result_h265(
    name: &str,
    result: &Result<H265AnyResult, CodecParseError>,
    expected: &Expected,
) {
    match (expected.outcome.as_str(), result) {
        ("ok", Ok(H265AnyResult::Sps(sps))) => {
            if let Some(fields) = &expected.fields {
                for (k, v) in fields {
                    match k.as_str() {
                        "general_profile_idc" => assert_eq!(
                            v.as_u64().unwrap() as u8,
                            sps.general_profile_idc,
                            "{name}: general_profile_idc"
                        ),
                        "general_level_idc" => assert_eq!(
                            v.as_u64().unwrap() as u8,
                            sps.general_level_idc,
                            "{name}: general_level_idc"
                        ),
                        "general_tier_flag" => assert_eq!(
                            v.as_bool().unwrap(),
                            sps.general_tier_flag,
                            "{name}: general_tier_flag"
                        ),
                        "width" => {
                            assert_eq!(v.as_u64().unwrap() as u32, sps.width, "{name}: width")
                        }
                        "height" => {
                            assert_eq!(v.as_u64().unwrap() as u32, sps.height, "{name}: height")
                        }
                        "bit_depth_luma" => assert_eq!(
                            v.as_u64().unwrap() as u8,
                            sps.bit_depth_luma,
                            "{name}: bit_depth_luma"
                        ),
                        "bit_depth_chroma" => assert_eq!(
                            v.as_u64().unwrap() as u8,
                            sps.bit_depth_chroma,
                            "{name}: bit_depth_chroma"
                        ),
                        "chroma_format" => assert_eq!(
                            v.as_str().unwrap(),
                            chroma_format_name(sps.chroma_format),
                            "{name}: chroma_format"
                        ),
                        other => panic!("{name}: unknown expected.fields key '{other}'"),
                    }
                }
            }
        }
        ("ok", Ok(_)) => { /* VPS/PPS: no field checks in starter set */ }
        ("err", Err(e)) => {
            let want = expected
                .error
                .as_deref()
                .expect("err outcome requires .error");
            assert_eq!(want, error_variant_name(e), "{name}: error variant");
        }
        ("ok", Err(e)) => panic!("{name}: expected Ok, got Err({e:?})"),
        ("err", Ok(_)) => panic!("{name}: expected Err, got Ok"),
        (other, _) => panic!("{name}: unknown outcome '{other}'"),
    }
}

#[test]
fn h266_conformance_vectors() {
    let pairs = load_pairs("h266");
    assert!(!pairs.is_empty(), "no H.266 fixtures found");
    for (name, bin, sidecar) in pairs {
        if let Some(summary) = is_known_bug("h266", &name) {
            eprintln!("SKIP h266/{name}: {summary}");
            continue;
        }
        eprintln!("checking h266/{name}");
        let result_kind: Result<H266AnyResult, CodecParseError> = match sidecar.kind.as_str() {
            "h266_sps" => tst_core::codec::h266::parse_sps(&bin).map(H266AnyResult::Sps),
            "h266_vps" => tst_core::codec::h266::parse_vps(&bin).map(H266AnyResult::Vps),
            "h266_pps" => tst_core::codec::h266::parse_pps(&bin).map(H266AnyResult::Pps),
            other => panic!("{name}: unsupported kind '{other}'"),
        };
        validate_result_h266(&name, &result_kind, &sidecar.expected);
    }
}

// Vps and Pps variants are defined for future fixtures; no field checks today.
#[allow(dead_code)]
enum H266AnyResult {
    Sps(tst_core::codec::h266::H266Sps),
    Vps(tst_core::codec::h266::H266Vps),
    Pps(tst_core::codec::h266::H266Pps),
}

fn validate_result_h266(
    name: &str,
    result: &Result<H266AnyResult, CodecParseError>,
    expected: &Expected,
) {
    match (expected.outcome.as_str(), result) {
        ("ok", Ok(H266AnyResult::Sps(sps))) => {
            if let Some(fields) = &expected.fields {
                for (k, v) in fields {
                    match k.as_str() {
                        // general_profile_idc/general_level_idc are nested inside
                        // sps.profile_tier_level per the H.266 SPS struct layout.
                        "general_profile_idc" => assert_eq!(
                            v.as_u64().unwrap() as u8,
                            sps.profile_tier_level.general_profile_idc,
                            "{name}: general_profile_idc"
                        ),
                        "general_level_idc" => assert_eq!(
                            v.as_u64().unwrap() as u8,
                            sps.profile_tier_level.general_level_idc,
                            "{name}: general_level_idc"
                        ),
                        "width" => {
                            assert_eq!(v.as_u64().unwrap() as u32, sps.width, "{name}: width")
                        }
                        "height" => {
                            assert_eq!(v.as_u64().unwrap() as u32, sps.height, "{name}: height")
                        }
                        "bit_depth_luma" => assert_eq!(
                            v.as_u64().unwrap() as u8,
                            sps.bit_depth_luma,
                            "{name}: bit_depth_luma"
                        ),
                        "bit_depth_chroma" => assert_eq!(
                            v.as_u64().unwrap() as u8,
                            sps.bit_depth_chroma,
                            "{name}: bit_depth_chroma"
                        ),
                        "chroma_format" => assert_eq!(
                            v.as_str().unwrap(),
                            chroma_format_name(sps.chroma_format),
                            "{name}: chroma_format"
                        ),
                        other => panic!("{name}: unknown expected.fields key '{other}'"),
                    }
                }
            }
        }
        ("ok", Ok(_)) => {}
        ("err", Err(e)) => {
            let want = expected
                .error
                .as_deref()
                .expect("err outcome requires .error");
            assert_eq!(want, error_variant_name(e), "{name}: error variant");
        }
        ("ok", Err(e)) => panic!("{name}: expected Ok, got Err({e:?})"),
        ("err", Ok(_)) => panic!("{name}: expected Err, got Ok"),
        (other, _) => panic!("{name}: unknown outcome '{other}'"),
    }
}

#[test]
fn av1_conformance_vectors() {
    // The .bin fixture is the SequenceHeader OBU *payload* with the OBU
    // header already stripped — matching what parse_sequence_header expects.
    // The strip tool skips the 32-byte IVF container header before scanning.
    let pairs = load_pairs("av1");
    assert!(!pairs.is_empty(), "no AV1 fixtures found");
    for (name, bin, sidecar) in pairs {
        if let Some(summary) = is_known_bug("av1", &name) {
            eprintln!("SKIP av1/{name}: {summary}");
            continue;
        }
        eprintln!("checking av1/{name}");
        assert_eq!(
            sidecar.kind, "av1_sequence_header",
            "{name}: kind must be av1_sequence_header"
        );
        let result = tst_core::codec::av1::parse_sequence_header(&bin);
        validate_result_av1(&name, &result, &sidecar.expected);
    }
}

fn validate_result_av1(
    name: &str,
    result: &Result<tst_core::codec::av1::Av1SequenceHeader, CodecParseError>,
    expected: &Expected,
) {
    match (expected.outcome.as_str(), result) {
        ("ok", Ok(sh)) => {
            if let Some(fields) = &expected.fields {
                for (k, v) in fields {
                    match k.as_str() {
                        "profile" => {
                            assert_eq!(v.as_u64().unwrap() as u8, sh.profile, "{name}: profile")
                        }
                        "level" => {
                            assert_eq!(v.as_u64().unwrap() as u8, sh.level, "{name}: level")
                        }
                        "tier" => {
                            assert_eq!(v.as_u64().unwrap() as u8, sh.tier, "{name}: tier")
                        }
                        "max_frame_width" => assert_eq!(
                            v.as_u64().unwrap() as u32,
                            sh.max_frame_width,
                            "{name}: max_frame_width"
                        ),
                        "max_frame_height" => assert_eq!(
                            v.as_u64().unwrap() as u32,
                            sh.max_frame_height,
                            "{name}: max_frame_height"
                        ),
                        "bit_depth" => {
                            assert_eq!(v.as_u64().unwrap() as u8, sh.bit_depth, "{name}: bit_depth")
                        }
                        "monochrome" => {
                            assert_eq!(v.as_bool().unwrap(), sh.monochrome, "{name}: monochrome")
                        }
                        "chroma_format" => assert_eq!(
                            v.as_str().unwrap(),
                            chroma_format_name(sh.chroma_format),
                            "{name}: chroma_format"
                        ),
                        other => panic!("{name}: unknown expected.fields key '{other}'"),
                    }
                }
            }
        }
        ("err", Err(e)) => {
            let want = expected
                .error
                .as_deref()
                .expect("err outcome requires .error");
            assert_eq!(want, error_variant_name(e), "{name}: error variant");
        }
        ("ok", Err(e)) => panic!("{name}: expected Ok, got Err({e:?})"),
        ("err", Ok(_)) => panic!("{name}: expected Err, got Ok"),
        (other, _) => panic!("{name}: unknown outcome '{other}'"),
    }
}
