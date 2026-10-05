//! Extract the first H.266 SPS NAL's payload from an Annex-B VVC/H.266 file.
//! Used to regenerate the H.266 SPS test fixtures from real-encoder output.
//!
//! Usage:
//!   `cargo run -p tst-examples --example extract_h266_sps_to_rbsp -- input.266 output.bin`
//!
//! # Why a dedicated extractor
//!
//! ffmpeg cannot natively dump SPS-only NALs in a clean single-unit form.
//! This tool walks the Annex-B byte stream, finds the first SPS NAL unit,
//! strips the 4- or 3-byte start code and the 2-byte NAL header, then writes
//! the remaining EBSP bytes verbatim to a `.bin` file for use as a unit-test
//! fixture. This mirrors `extract_h265_sps_to_rbsp.rs`.
//!
//! # EBSP vs RBSP — which does `parse_sps` expect?
//!
//! `tst_core::codec::h266::parse_sps` takes **EBSP** (Encapsulated Byte
//! Sequence Payload) — the raw NAL payload bytes with emulation-prevention
//! bytes (`00 00 03`) still in place. The underlying `BitReader` (shared with
//! the H.265 parser) transparently skips the `03` octet whenever the two
//! preceding bytes are `00 00`. Stripping EP bytes before calling `parse_sps`
//! would corrupt the bit stream.
//!
//! Therefore this helper writes the bytes **after the 2-byte NAL header**
//! verbatim, without any EP-stripping step.
//!
//! # H.266 NAL header layout (H.266 V4 §7.3.1.2)
//!
//! The H.266 NAL header is 2 bytes, but the bit layout is **different from
//! H.265** — the two specs put the fields in opposite bytes:
//!
//! ```text
//!                    H.265 NAL header (for contrast):
//!   byte 0: forbidden_zero_bit(1) | nal_unit_type(6) | nuh_layer_id (high bits)
//!   byte 1: nuh_layer_id (low bits) | nuh_temporal_id_plus1(3)
//!
//!                    H.266 NAL header:
//!   byte 0: forbidden_zero_bit(1) | nuh_reserved_zero_bit(1) | nuh_layer_id(6)
//!   byte 1: nal_unit_type(5) | nuh_temporal_id_plus1(3)
//! ```
//!
//! To extract the NAL unit type from an H.266 unit:
//!   `nut = (byte1 >> 3) & 0x1F`
//!
//! SPS_NUT = 15 (Table 7 in H.266 V4).
//!
//! # Regenerating the fixture
//!
//! ```bash
//! # 1. Generate raw YUV input (10-bit 4:2:0, 15 frames @ 30fps = 0.5s)
//! ffmpeg -f lavfi -i testsrc=size=320x240:rate=30:duration=0.5 \
//!        -c:v rawvideo -pix_fmt yuv420p10le /tmp/test_input.yuv
//!
//! # 2. Encode to H.266 elementary stream with VVenC
//! vvencapp -i /tmp/test_input.yuv -s 320x240 -r 30 --preset faster \
//!          -o /tmp/test_h266.266 -f 16
//!
//! # 3. Extract the SPS EBSP payload
//! cargo run -p tst-examples --example extract_h266_sps_to_rbsp -- \
//!       /tmp/test_h266.266 \
//!       crates/tst-core/tests/fixtures/codec/h266/h266_320x240_main10_real_sps.bin
//! ```

use std::env;
use std::fs;
use std::io::{self, Write};

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: extract_h266_sps_to_rbsp <input.266> <output.bin>");
        std::process::exit(1);
    }
    let input_path = &args[1];
    let output_path = &args[2];

    let bytes = fs::read(input_path)?;

    // Walk the Annex-B byte stream and collect NAL unit payloads (the bytes
    // between consecutive start codes). Start codes are either 4-byte
    // (00 00 00 01) or 3-byte (00 00 01); both delimit NAL boundaries.
    let mut nals: Vec<&[u8]> = Vec::new();
    let mut i = 0usize;
    let mut last_start: Option<usize> = None;
    while i < bytes.len() {
        let is_4byte = bytes[i..].starts_with(&[0x00, 0x00, 0x00, 0x01]);
        // Only test 3-byte after ruling out 4-byte to avoid double-counting.
        let is_3byte = !is_4byte && bytes[i..].starts_with(&[0x00, 0x00, 0x01]);
        if is_4byte || is_3byte {
            if let Some(s) = last_start {
                // Push the bytes from end of previous start code to here.
                // This slice includes the 2-byte NAL header as its first bytes.
                nals.push(&bytes[s..i]);
            }
            last_start = Some(i + if is_4byte { 4 } else { 3 });
            i += if is_4byte { 4 } else { 3 };
        } else {
            i += 1;
        }
    }
    // Don't forget the last NAL (no trailing start code).
    if let Some(s) = last_start {
        nals.push(&bytes[s..]);
    }

    // Find the first SPS NAL unit.
    //
    // H.266 NAL unit type is in byte 1 of the 2-byte header:
    //   nut = (byte1 >> 3) & 0x1F
    // SPS_NUT = 15 per H.266 V4 Table 7.
    //
    // This is the key difference from H.265, which encodes the NAL unit type
    // in byte 0 bits 6..1: `nut = (byte0 >> 1) & 0x3F`.
    let sps_ebsp = nals
        .iter()
        .find(|nal| nal.len() >= 2 && ((nal[1] >> 3) & 0x1F) == 15)
        .unwrap_or_else(|| {
            eprintln!("error: no SPS NAL (NUT=15) found in {input_path}");
            std::process::exit(1);
        });

    // Strip the 2-byte NAL header and write the EBSP payload as-is.
    // The H.266 parser's BitReader handles emulation-prevention byte
    // (00 00 03) skipping internally — do NOT strip EP bytes here.
    let payload = &sps_ebsp[2..];

    eprintln!(
        "Found SPS NAL: total={} bytes, EBSP payload={} bytes",
        sps_ebsp.len(),
        payload.len()
    );

    let mut out = fs::File::create(output_path)?;
    out.write_all(payload)?;

    eprintln!("Written to {output_path}");
    Ok(())
}
