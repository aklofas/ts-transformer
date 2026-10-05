//! Descriptor body length bound is 255, not 253.
//!
//! H.222.0 §2.6 defines `descriptor_length` as an 8-bit field, so any
//! body length in 0..=255 bytes is structurally valid. The old guard
//! `declared > 253` rejected 254- and 255-byte bodies with a misleading
//! `MalformedDescriptor` error. After the fix the bound is removed (it
//! would be dead code at 255 since `declared = tlv[1] as usize` is always
//! ≤ 255), and those descriptors correctly proceed to the separate
//! `PmtTooLarge` check.
//!
//! Git archaeology: the 253 bound was introduced in
//! "carve tst-core out of srt-core" (0d7daec9) with no rationale.
//! No structural PMT-section interplay forces the limit below 255 —
//! `PmtTooLarge` already rejects configs whose total PMT section exceeds
//! 183 bytes (188 − 4 TS header − 1 pointer field). For a minimal
//! single-H264-stream PMT the practical per-descriptor-body limit is
//! 160 bytes: 183 (total) − 16 (PMT overhead) − 5 (ES entry overhead)
//! − 2 (tag + length) = 160.

use tst_core::MuxError;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer};
use tst_core::mpegts::mux::{Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};

/// Build a tag-0xFF TLV with `body_len` zero bytes.
/// The tag and body content are arbitrary; the tests only care about
/// the well-formedness validation path.
fn raw_tlv_ff(body_len: u8) -> Vec<u8> {
    let mut tlv = vec![0u8; 2 + body_len as usize];
    tlv[0] = 0xFF; // user-private tag per H.222.0 Table 2-45
    tlv[1] = body_len;
    // bytes 2.. remain zero
    tlv
}

/// Build a `MuxerConfig` with one H264 video stream carrying a single
/// caller-supplied descriptor. Returns the config-validation result.
fn cfg_with_video_desc(desc: Vec<u8>) -> Result<MuxerConfig, MuxError> {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x100, VideoCodec::H264);
    prog.stream_descriptors_for_video(0, vec![desc]).unwrap();
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build()
}

// ── Step 1/2 (RED before fix, GREEN after) ──────────────────────────────────

/// A 255-byte descriptor body is valid per H.222.0 §2.6 (`descriptor_length`
/// is a u8 field). The old guard `declared > 253` rejected it as
/// `MalformedDescriptor`. After the fix the descriptor check passes and
/// config validation proceeds to the PMT-section-size check, which correctly
/// rejects it as `PmtTooLarge` (a 255-byte body produces a 278-byte PMT
/// section, well above the 183-byte single-packet limit).
#[test]
fn descriptor_body_255_bytes_not_malformed() {
    let result = cfg_with_video_desc(raw_tlv_ff(255));
    // OLD (wrong): Err(MalformedDescriptor { .. }) — rejected before PMT check.
    // NEW (correct): Err(PmtTooLarge { .. }) — descriptor is well-formed; PMT
    // section is too large for one TS packet (278 bytes > 183 bytes).
    assert!(
        matches!(result, Err(MuxError::PmtTooLarge { .. })),
        "255-byte descriptor body should fail with PmtTooLarge, not MalformedDescriptor; got {:?}",
        result
    );
}

/// Same as above for a 254-byte body (also rejected by the old guard).
#[test]
fn descriptor_body_254_bytes_not_malformed() {
    let result = cfg_with_video_desc(raw_tlv_ff(254));
    assert!(
        matches!(result, Err(MuxError::PmtTooLarge { .. })),
        "254-byte descriptor body should fail with PmtTooLarge, not MalformedDescriptor; got {:?}",
        result
    );
}

// ── Step 4: boundary / impossible-length tests ───────────────────────────────

/// "256-byte declared" is impossible by construction from a valid TLV byte:
/// `declared = tlv[1] as usize` can only be 0..=255. To express a 256-byte
/// body the caller would need `tlv[1] = 0xFF` (declares 255) but supply 256
/// body bytes — which the pre-existing length-mismatch check catches first.
///
/// This test pins that contract: a TLV where the length byte (0xFF = 255)
/// does not match the actual body (256 bytes) is rejected as
/// `MalformedDescriptor` via the mismatch arm, not the body-size guard.
#[test]
fn descriptor_length_byte_mismatch_is_malformed() {
    // TLV = tag(1) + length=0xFF(1) + body[256 zeros] = 258 bytes total.
    // Declared = 255, actual body = 256 → mismatch → MalformedDescriptor.
    let mut tlv = vec![0u8; 258];
    tlv[0] = 0xFF; // tag
    tlv[1] = 0xFF; // declared = 255; actual body below is 256 bytes
    // bytes 2..258 remain zero (256-byte body)
    let result = cfg_with_video_desc(tlv);
    assert!(
        matches!(result, Err(MuxError::MalformedDescriptor { .. })),
        "length-byte mismatch must be rejected with MalformedDescriptor; got {:?}",
        result
    );
}

// ── Step 5: full mux → demux round-trip ─────────────────────────────────────

/// A 160-byte descriptor body is the maximum that fits in a minimal
/// single-H264-stream PMT section:
///   183 (total TS payload) − 16 (PMT overhead) − 5 (ES entry) − 2 (TLV header)
///   = 160 bytes.
///
/// This test exercises the full Muxer → Demuxer pipeline: the descriptor
/// must survive byte-for-byte through the PMT section.
///
/// Note: this test passes under a 253-byte guard too (160 ≤ 253, so that
/// guard never triggers here). It exists to pin the pipeline contract —
/// a descriptor within the PMT budget must round-trip verbatim — not as TDD
/// RED evidence of the bug. The RED evidence is in the 254/255-byte tests above.
///
/// A 255-byte body is not achievable in a single-packet PMT (it would require
/// a 278-byte section). The body-size guard fix matters for the error
/// classification at the config-validation step; the PMT budget is the binding
/// operational constraint.
#[test]
fn large_descriptor_body_round_trips_through_demuxer() {
    let body_len: u8 = 160;
    let cfg = cfg_with_video_desc(raw_tlv_ff(body_len))
        .expect("160-byte body is exactly at the single-packet PMT budget limit");

    let mut mux = Muxer::new(cfg).unwrap();
    mux.push_video(&[0, 0, 0, 1, 0x09, 0x10], Pts90khz::new(9000), true)
        .expect("push_video must succeed for the round-trip payload path");
    let mut buf = vec![0u8; 188 * 32];
    let n = mux.pull(&mut buf);
    buf.truncate(n);

    let mut demuxer = Demuxer::new();
    demuxer.feed(&buf).unwrap();
    let mut pmt = None;
    while let Some(ev) = demuxer.next_event() {
        if let DemuxEvent::ProgramMap(pm) = ev {
            pmt = Some(pm);
        }
    }
    let pmt = pmt.expect("ProgramMap must be emitted from muxed bytes");
    let video = pmt.streams.iter().find(|s| s.pid == 0x100).unwrap();
    assert_eq!(
        video.raw_descriptors.len(),
        1,
        "video stream must carry exactly the one caller-supplied descriptor"
    );
    let d = &video.raw_descriptors[0];
    assert_eq!(d.tag, 0xFF, "descriptor tag must round-trip as 0xFF");
    assert_eq!(
        d.data.len(),
        body_len as usize,
        "descriptor body length must survive the mux→demux round-trip"
    );
    assert!(
        d.data.iter().all(|&b| b == 0),
        "descriptor body bytes must survive verbatim (all zero)"
    );
}
