//! Strip parameter-set NAL/OBU bytes from official codec conformance
//! bitstreams. Driven by `manifest.toml`.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default)]
    h264: Vec<Entry>,
    #[serde(default)]
    h265: Vec<Entry>,
    #[serde(default)]
    h266: Vec<Entry>,
    #[serde(default)]
    av1: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
struct Entry {
    name: String,
    archive_url: String,
    /// Path within the archive. Empty string => archive_url IS the raw bitstream.
    #[serde(default)]
    extract: String,
    sha256: String,
    kind: String,
    #[serde(default)]
    nal_index: u32,
    #[serde(default)]
    obu_index: u32,
    expected: Expected,
}

#[derive(Debug, Deserialize, Serialize)]
struct Expected {
    outcome: String, // "ok" or "err"
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fields: Option<BTreeMap<String, toml::Value>>,
}

fn load_manifest(path: &std::path::Path) -> Result<Manifest, String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    toml::from_str(&s).map_err(|e| format!("parse {}: {}", path.display(), e))
}

/// Iterate Annex B NAL units. Yields each NAL unit as a byte slice
/// starting at the NAL header byte (start codes stripped, header
/// included). Trailing bytes after the last NAL until EOF form the
/// last yielded slice.
fn iter_annex_b(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    AnnexBIter { stream, cursor: 0 }
}

struct AnnexBIter<'a> {
    stream: &'a [u8],
    cursor: usize,
}

impl<'a> Iterator for AnnexBIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<Self::Item> {
        // Find next start code from cursor.
        let nal_start = find_start_code(&self.stream[self.cursor..])?;
        let nal_data_start = self.cursor + nal_start.0 + nal_start.1; // skip past start code
        // Find next start code after this one (or EOF).
        let next = find_start_code(&self.stream[nal_data_start..])
            .map(|(off, _len)| nal_data_start + off)
            .unwrap_or(self.stream.len());
        self.cursor = next;
        Some(&self.stream[nal_data_start..next])
    }
}

/// Returns (offset_to_start_code, start_code_length) in haystack, or None.
fn find_start_code(haystack: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 3 <= haystack.len() {
        // 4-byte start code 00 00 00 01.
        // MUST check 4-byte before 3-byte: a `00 00 00 01` sequence would
        // otherwise alias as a 3-byte code starting at offset 1.
        if i + 4 <= haystack.len() && haystack[i..i + 4] == [0, 0, 0, 1] {
            return Some((i, 4));
        }
        // 3-byte start code 00 00 01
        if haystack[i..i + 3] == [0, 0, 1] {
            return Some((i, 3));
        }
        i += 1;
    }
    None
}

/// Extract the RBSP body of the `n`th NAL of the given `nal_unit_type`
/// from an Annex B H.264 bytestream. Returns None if not found.
fn extract_h264_parameter_set(stream: &[u8], nal_type: u8, n: u32) -> Option<Vec<u8>> {
    let mut matched = 0u32;
    for nal in iter_annex_b(stream) {
        if nal.is_empty() {
            continue;
        }
        // H.264 NAL header is 1 byte. Bits 1-5 = nal_unit_type.
        let header = nal[0];
        let this_type = header & 0x1F;
        if this_type == nal_type {
            if matched == n {
                return Some(nal[1..].to_vec());
            }
            matched += 1;
        }
    }
    None
}

/// Extract the RBSP body of the `n`th NAL of the given `nal_unit_type`
/// from an Annex B H.265 bytestream. H.265 NAL header is 2 bytes; the
/// first byte's bits 1-6 carry nal_unit_type.
fn extract_h265_parameter_set(stream: &[u8], nal_type: u8, n: u32) -> Option<Vec<u8>> {
    let mut matched = 0u32;
    for nal in iter_annex_b(stream) {
        if nal.len() < 2 {
            continue;
        }
        // H.265: first byte bit 0 = forbidden_zero_bit, bits 1-6 = nal_unit_type.
        let this_type = (nal[0] >> 1) & 0x3F;
        if this_type == nal_type {
            if matched == n {
                return Some(nal[2..].to_vec());
            }
            matched += 1;
        }
    }
    None
}

/// Extract the RBSP body of the `n`th NAL of the given `nal_unit_type`
/// from an Annex B H.266 / VVC bytestream. H.266 NAL header is 2 bytes;
/// nal_unit_type lives in byte 1, bits 3-7 (i.e. `(byte_1 >> 3) & 0x1F`)
/// per H.266 V4 §7.3.1.2.
fn extract_h266_parameter_set(stream: &[u8], nal_type: u8, n: u32) -> Option<Vec<u8>> {
    let mut matched = 0u32;
    for nal in iter_annex_b(stream) {
        if nal.len() < 2 {
            continue;
        }
        let this_type = (nal[1] >> 3) & 0x1F;
        if this_type == nal_type {
            if matched == n {
                return Some(nal[2..].to_vec());
            }
            matched += 1;
        }
    }
    None
}

/// Extract the `n`th SequenceHeader OBU payload from an AV1 bytestream.
/// Handles both raw OBU streams and IVF-wrapped streams (autodetected
/// via DKIF signature at offset 0). Returns None if not found.
///
/// For IVF inputs, only the first frame's OBU stream is scanned. AOM
/// conformance vectors always place the SequenceHeader in frame 0, so
/// this is sufficient for our purposes.
fn extract_av1_sequence_header(stream: &[u8], n: u32) -> Option<Vec<u8>> {
    // IVF autodetect: DKIF + 32-byte header.
    let obu_stream: &[u8] = if stream.len() >= 32 && &stream[..4] == b"DKIF" {
        // Skip IVF file header.
        let mut frames = &stream[32..];
        // For sequence-header extraction we only need the first frame's payload.
        // IVF per-frame header is 12 bytes: 4-byte size LE + 8-byte pts.
        if frames.len() < 12 {
            return None;
        }
        let size = u32::from_le_bytes([frames[0], frames[1], frames[2], frames[3]]) as usize;
        frames = &frames[12..];
        if frames.len() < size {
            return None;
        }
        &frames[..size]
    } else {
        stream
    };

    let mut matched = 0u32;
    let mut cursor = 0usize;
    while cursor < obu_stream.len() {
        let header = obu_stream[cursor];
        // AV1 §5.3.2 OBU header bit layout:
        //   bit 7: obu_forbidden_bit
        //   bits 6-3: obu_type (4 bits)
        //   bit 2: obu_extension_flag
        //   bit 1: obu_has_size_field
        //   bit 0: obu_reserved_1bit
        let obu_type = (header >> 3) & 0x0F;
        let extension = (header >> 2) & 0x01 != 0;
        let has_size = (header >> 1) & 0x01 != 0;
        cursor += 1;
        if extension {
            cursor += 1; // skip 1-byte extension header
        }
        let payload_size = if has_size {
            let (sz, consumed) = read_leb128(&obu_stream[cursor..])?;
            cursor += consumed;
            sz
        } else {
            // For raw streams without size, payload runs to EOF. Conformance
            // vectors always set has_size; if not, treat remaining bytes as
            // this OBU's payload (scanning stops after this OBU since the
            // cursor advance exhausts the buffer).
            obu_stream.len() - cursor
        };
        if obu_type == 1 {
            // OBU type 1 = SequenceHeader per AV1 §6.4.
            if matched == n {
                return Some(obu_stream[cursor..cursor + payload_size].to_vec());
            }
            matched += 1;
        }
        cursor += payload_size;
    }
    None
}

/// Read an AV1 LEB128. Returns (value, bytes_consumed) or None on malformed input.
/// Spec caps at 8 bytes (AV1 §4.10.5).
fn read_leb128(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut value: u64 = 0;
    for i in 0..8 {
        if i >= bytes.len() {
            return None;
        }
        let b = bytes[i];
        value |= ((b & 0x7F) as u64) << (i * 7);
        if b & 0x80 == 0 {
            return Some((value as usize, i + 1));
        }
    }
    None
}

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/conformance")
}

fn cache_dir() -> PathBuf {
    fixtures_root().join("_cache")
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Fetch `url` into `dest` via curl. No-op if dest exists with correct sha.
fn fetch_with_sha(url: &str, dest: &Path, expected_sha: &str) -> Result<(), String> {
    if dest.exists() {
        let observed = sha256_hex(dest)?;
        if observed == expected_sha {
            eprintln!("  cache hit: {}", dest.display());
            return Ok(());
        }
        eprintln!("  cache miss (sha mismatch), redownloading");
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let out = std::process::Command::new("curl")
        .args(["-fsSL", "-o", dest.to_str().ok_or("non-utf8 path")?, url])
        .output()
        .map_err(|e| format!("spawn curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "curl failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let observed = sha256_hex(dest)?;
    if observed != expected_sha {
        return Err(format!(
            "sha mismatch for {}: expected {expected_sha}, got {observed}",
            dest.display()
        ));
    }
    Ok(())
}

/// Read the named member out of a zip archive. Uses `unzip -p` to avoid a
/// zip crate dep. For non-zip archive_urls (raw bitstream), reads the file
/// directly.
fn read_archive_member(archive: &Path, member: &str) -> Result<Vec<u8>, String> {
    if member.is_empty() {
        return std::fs::read(archive).map_err(|e| format!("{}: {e}", archive.display()));
    }
    let out = std::process::Command::new("unzip")
        .args(["-p", archive.to_str().ok_or("non-utf8")?, member])
        .output()
        .map_err(|e| format!("spawn unzip: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "unzip failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(out.stdout)
}

fn process_entry(codec: &str, entry: &Entry) -> Result<(), String> {
    let archive_path = cache_dir().join(format!("{}.archive", entry.name));
    fetch_with_sha(&entry.archive_url, &archive_path, &entry.sha256)?;
    let bytestream = read_archive_member(&archive_path, &entry.extract)?;

    let parameter_set = match entry.kind.as_str() {
        "h264_sps" => extract_h264_parameter_set(&bytestream, 7, entry.nal_index),
        "h264_pps" => extract_h264_parameter_set(&bytestream, 8, entry.nal_index),
        "h265_vps" => extract_h265_parameter_set(&bytestream, 32, entry.nal_index),
        "h265_sps" => extract_h265_parameter_set(&bytestream, 33, entry.nal_index),
        "h265_pps" => extract_h265_parameter_set(&bytestream, 34, entry.nal_index),
        "h266_vps" => extract_h266_parameter_set(&bytestream, 14, entry.nal_index),
        "h266_sps" => extract_h266_parameter_set(&bytestream, 15, entry.nal_index),
        "h266_pps" => extract_h266_parameter_set(&bytestream, 16, entry.nal_index),
        "av1_sequence_header" => extract_av1_sequence_header(&bytestream, entry.obu_index),
        other => return Err(format!("unknown kind: {other}")),
    }
    .ok_or_else(|| format!("parameter set not found in {}", entry.name))?;

    let out_dir = fixtures_root().join(codec);
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("mkdir: {e}"))?;
    let bin_path = out_dir.join(format!("{}.bin", entry.name));
    let json_path = out_dir.join(format!("{}.json", entry.name));

    std::fs::write(&bin_path, &parameter_set).map_err(|e| format!("write bin: {e}"))?;

    let sidecar = serde_json::json!({
        "source": entry.extract,
        "url": entry.archive_url,
        "sha256_source": entry.sha256,
        "kind": entry.kind,
        "expected": entry.expected,
    });
    std::fs::write(
        &json_path,
        serde_json::to_string_pretty(&sidecar).map_err(|e| format!("json: {e}"))?,
    )
    .map_err(|e| format!("write json: {e}"))?;

    eprintln!(
        "  wrote {} ({} bytes)",
        bin_path.strip_prefix(fixtures_root()).unwrap().display(),
        parameter_set.len()
    );
    Ok(())
}

fn main() {
    let manifest_path = fixtures_root().join("manifest.toml");
    let manifest = match load_manifest(&manifest_path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let mut errors: Vec<String> = vec![];
    for (codec, entries) in [
        ("h264", &manifest.h264),
        ("h265", &manifest.h265),
        ("h266", &manifest.h266),
        ("av1", &manifest.av1),
    ] {
        eprintln!("=== {codec} ({} entries) ===", entries.len());
        for entry in entries {
            eprintln!("- {}", entry.name);
            if let Err(e) = process_entry(codec, entry) {
                errors.push(format!("{codec}/{}: {e}", entry.name));
            }
        }
    }

    if !errors.is_empty() {
        eprintln!("\n{} entries failed:", errors.len());
        for e in &errors {
            eprintln!("  {e}");
        }
        std::process::exit(1);
    }
    eprintln!("\nOK");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_loads_starter_set() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/conformance/manifest.toml");
        let m = load_manifest(&path).expect("starter manifest must parse");
        assert!(m.h264.len() >= 3, "h264 entries: {}", m.h264.len());
        assert!(m.h265.len() >= 3, "h265 entries: {}", m.h265.len());
        assert!(m.h266.len() >= 3, "h266 entries: {}", m.h266.len());
        assert!(m.av1.len() >= 2, "av1 entries: {}", m.av1.len());
    }

    #[test]
    fn manifest_entry_round_trips_required_fields() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/conformance/manifest.toml");
        let m = load_manifest(&path).expect("parse");
        for e in &m.h264 {
            assert!(!e.name.is_empty());
            assert!(!e.archive_url.is_empty());
            assert_eq!(e.sha256.len(), 64, "sha256 hex must be 64 chars");
            assert_eq!(e.kind, "h264_sps");
        }
    }

    /// Hand-crafted minimal Annex B stream: AUD + SPS + PPS. The SPS body
    /// here is 4 bytes of arbitrary placeholder — the scanner must extract
    /// those 4 bytes back out (NAL header stripped, NOT the start code).
    const FAKE_H264_STREAM: &[u8] = &[
        // 4-byte start code + AUD NAL (type 9)
        0x00, 0x00, 0x00, 0x01, 0x09, 0x10,
        // 3-byte start code + SPS NAL (type 7) + 4 bytes RBSP placeholder
        0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB, 0xCC, 0xDD,
        // 3-byte start code + PPS NAL (type 8) + 2 bytes RBSP
        0x00, 0x00, 0x01, 0x68, 0xEE, 0xFF,
    ];

    #[test]
    fn scan_h264_sps_extracts_rbsp_body() {
        let extracted =
            extract_h264_parameter_set(FAKE_H264_STREAM, 7, 0).expect("SPS should be found");
        assert_eq!(extracted, vec![0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn scan_h264_sps_respects_nal_index() {
        // Only one SPS in fixture; index 1 should miss.
        assert!(extract_h264_parameter_set(FAKE_H264_STREAM, 7, 1).is_none());
    }

    #[test]
    fn scan_h264_pps_extracts_rbsp_body() {
        let extracted =
            extract_h264_parameter_set(FAKE_H264_STREAM, 8, 0).expect("PPS should be found");
        assert_eq!(extracted, vec![0xEE, 0xFF]);
    }

    const FAKE_H265_STREAM: &[u8] = &[
        // 4-byte start code + VPS NAL (type 32) + 3 bytes RBSP
        // 2-byte NAL header: 0100 0000 1<temporal_id+1>
        // type=32 -> first byte (0 << 7) | (32 << 1) | 0 = 0x40
        // second byte: layer_id=0, temporal_id_plus_1=1 -> 0x01
        0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x11, 0x22, 0x33,
        // 3-byte start + SPS (type 33) + 4 bytes RBSP
        // first byte: (33 << 1) = 0x42
        0x00, 0x00, 0x01, 0x42, 0x01, 0x44, 0x55, 0x66, 0x77,
        // 3-byte start + PPS (type 34) + 2 bytes RBSP
        // first byte: (34 << 1) = 0x44
        0x00, 0x00, 0x01, 0x44, 0x01, 0x88, 0x99,
    ];

    #[test]
    fn scan_h265_sps_extracts_rbsp_body() {
        let extracted =
            extract_h265_parameter_set(FAKE_H265_STREAM, 33, 0).expect("SPS should be found");
        assert_eq!(extracted, vec![0x44, 0x55, 0x66, 0x77]);
    }

    #[test]
    fn scan_h265_vps_extracts_rbsp_body() {
        let extracted =
            extract_h265_parameter_set(FAKE_H265_STREAM, 32, 0).expect("VPS should be found");
        assert_eq!(extracted, vec![0x11, 0x22, 0x33]);
    }

    #[test]
    fn scan_h265_pps_extracts_rbsp_body() {
        let extracted =
            extract_h265_parameter_set(FAKE_H265_STREAM, 34, 0).expect("PPS should be found");
        assert_eq!(extracted, vec![0x88, 0x99]);
    }

    const FAKE_H266_STREAM: &[u8] = &[
        // 4-byte start + H.266 VPS (type 14, in byte 1 high bits)
        // byte 0: 0 (forbidden) | 0 (reserved) | layer_id_hi<<2 = 0x00
        // byte 1: type(14)<<3 | temporal_id_plus_1(1) = (14<<3)|1 = 0x71
        0x00, 0x00, 0x00, 0x01, 0x00, 0x71, 0xAA, 0xBB,
        // SPS (type 15)
        // byte 1: (15<<3)|1 = 0x79
        0x00, 0x00, 0x01, 0x00, 0x79, 0xCC, 0xDD, 0xEE, 0xFF,
        // PPS (type 16)
        // byte 1: (16<<3)|1 = 0x81
        0x00, 0x00, 0x01, 0x00, 0x81, 0x11, 0x22,
    ];

    #[test]
    fn scan_h266_sps_extracts_rbsp_body() {
        let extracted =
            extract_h266_parameter_set(FAKE_H266_STREAM, 15, 0).expect("SPS should be found");
        assert_eq!(extracted, vec![0xCC, 0xDD, 0xEE, 0xFF]);
    }

    #[test]
    fn scan_h266_vps_extracts_rbsp_body() {
        let extracted =
            extract_h266_parameter_set(FAKE_H266_STREAM, 14, 0).expect("VPS should be found");
        assert_eq!(extracted, vec![0xAA, 0xBB]);
    }

    #[test]
    fn scan_h266_pps_extracts_rbsp_body() {
        let extracted =
            extract_h266_parameter_set(FAKE_H266_STREAM, 16, 0).expect("PPS should be found");
        assert_eq!(extracted, vec![0x11, 0x22]);
    }

    /// Raw OBU stream (no IVF): TemporalDelimiter (type 2) + SequenceHeader (type 1).
    /// Both have obu_has_size_field=1 (bit 6 of byte 0) and a 1-byte LEB128 size.
    const FAKE_AV1_RAW_OBUS: &[u8] = &[
        // TD: type=2, has_size=1 -> byte = (2<<3)|(1<<1) = 0x12
        0x12, 0x00, // SeqHeader: type=1, has_size=1 -> byte = (1<<3)|(1<<1) = 0x0A
        0x0A, 0x04, 0xAA, 0xBB, 0xCC, 0xDD,
    ];

    #[test]
    fn scan_av1_sequence_header_raw_obus() {
        let extracted = extract_av1_sequence_header(FAKE_AV1_RAW_OBUS, 0)
            .expect("SequenceHeader should be found");
        assert_eq!(extracted, vec![0xAA, 0xBB, 0xCC, 0xDD]);
    }

    /// IVF-wrapped: signature DKIF + 28 bytes header padding + one frame
    /// containing a SequenceHeader OBU. The IVF per-frame header is 12 bytes:
    /// 4-byte size LE + 8-byte pts LE.
    const FAKE_AV1_IVF: &[u8] = &[
        // IVF file header (32 bytes)
        b'D', b'K', b'I', b'F', // signature
        0x00, 0x00, // version
        0x20, 0x00, // header length (32 LE)
        b'A', b'V', b'0', b'1', // fourcc
        0x40, 0x01, // width 320
        0xF0, 0x00, // height 240
        0x1E, 0x00, 0x00, 0x00, // framerate num
        0x01, 0x00, 0x00, 0x00, // framerate den
        0x01, 0x00, 0x00, 0x00, // frame count
        0x00, 0x00, 0x00, 0x00, // unused
        // Frame 0: size=6 + pts=0
        0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        // Frame 0 payload: SeqHeader OBU (type=1, has_size=1) + size=4 + 4 bytes
        0x0A, 0x04, 0xEE, 0xFF, 0x12, 0x34,
    ];

    #[test]
    fn scan_av1_sequence_header_ivf() {
        let extracted = extract_av1_sequence_header(FAKE_AV1_IVF, 0)
            .expect("SequenceHeader inside IVF should be found");
        assert_eq!(extracted, vec![0xEE, 0xFF, 0x12, 0x34]);
    }
}
