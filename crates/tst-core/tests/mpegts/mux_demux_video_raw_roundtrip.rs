//! Byte-faithful video AU round-trip: demux→mux→demux fixpoint.
//!
//! Property under test: for each codec (H.264, H.265, H.266, AV1), muxing a
//! synthetic access unit and demuxing produces `raw1`; muxing `raw1` again and
//! demuxing produces `raw2`. The fixpoint assertion `raw1 == raw2` proves the
//! demux→mux→demux cycle is byte-stable.
//!
//! AV1 uses `Av1CarriageMode::InteropRawObu` throughout so the mux passes raw
//! OBU bytes without binding-mode wrapping — the byte-faithful path.

use crate::common::mux_demux::drain_mux;
use crate::common::synthetic::synthetic_h264_au;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::{
    Demuxer, DemuxerConfig,
    event::{DemuxEvent, SamplePayload},
};
use tst_core::mpegts::mux::{
    Av1CarriageMode, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};
use tst_core::shared::SharedBytes;

// ─────────────────────────────────────────────────────────────────────────────
// Synthetic access unit builders (one per codec)
// ─────────────────────────────────────────────────────────────────────────────

/// Minimal Annex-B H.265 access unit: VPS + SPS + PPS + IDR slice.
fn synthetic_h265_au() -> Vec<u8> {
    fn nal(nal_unit_type: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0x00, 0x00, 0x00, 0x01];
        // H.265 NAL header: forbidden_zero(1)=0 | nal_unit_type(6) | layer_id(6)=0 | tid(3)=1
        v.push(nal_unit_type << 1);
        v.push(0x01); // tid=1
        v.extend_from_slice(body);
        v
    }
    let mut au = Vec::new();
    au.extend(nal(32, &[0x01, 0x60])); // VPS_NUT
    au.extend(nal(33, &[0x01, 0x02])); // SPS_NUT
    au.extend(nal(34, &[0xC8])); // PPS_NUT
    au.extend(nal(19, &[0xA0, 0x10, 0x20])); // IDR_W_RADL
    au
}

/// Minimal Annex-B H.266 access unit: AUD + VPS + SPS + PPS + IDR slice.
fn synthetic_h266_au() -> Vec<u8> {
    fn nal(nal_type: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0x00, 0x00, 0x00, 0x01];
        // H.266 V4 §7.3.1.2:
        //   byte 0: forbidden_zero_bit(1)=0 | nuh_reserved_zero_bit(1)=0 | nuh_layer_id(6)=0
        //   byte 1: nal_unit_type(5) | nuh_temporal_id_plus1(3)=1
        v.push(0x00);
        v.push((nal_type << 3) | 0x01);
        v.extend_from_slice(body);
        v
    }
    let mut au = Vec::new();
    au.extend(nal(20, &[0x10])); // AUD_NUT
    au.extend(nal(14, &[0xA0])); // VPS_NUT
    au.extend(nal(15, &[0xB0])); // SPS_NUT
    au.extend(nal(16, &[0xC0])); // PPS_NUT
    au.extend(nal(7, &[0xD0])); // IDR_W_RADL
    au
}

/// Minimal AV1 access unit: Temporal Delimiter + Sequence Header + Frame
/// Header + Tile Group OBUs, each with `obu_has_size_field=1`.
fn synthetic_av1_au() -> Vec<u8> {
    fn obu(obu_type: u8, body: &[u8]) -> Vec<u8> {
        // AV1 spec §5.3.2: (obu_type << 3) | obu_has_size_field(1<<1)
        let header = (obu_type << 3) | 0x02;
        let mut v = vec![header];
        v.push(body.len() as u8); // single-byte LEB128 (bodies < 128 bytes)
        v.extend_from_slice(body);
        v
    }
    let mut au = Vec::new();
    au.extend(obu(2, &[])); // Temporal Delimiter (empty body)
    au.extend(obu(1, &[0x00, 0x00])); // Sequence Header
    au.extend(obu(3, &[0x00])); // Frame Header
    au.extend(obu(4, &[0x00, 0x01, 0x02])); // Tile Group
    au
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Mux `au` as a video AU (key frame, PTS=90000) into a fresh `Muxer` built
/// with `build_cfg`, drain all TS packets, then demux and collect each Video
/// `raw` payload. Uses an interop-mode demuxer for AV1 when `av1_interop` is
/// true; binding-mode demuxer otherwise.
fn mux_then_collect_video_raws(
    au: &[u8],
    build_cfg: impl Fn() -> MuxerConfig,
    av1_interop: bool,
) -> Vec<SharedBytes> {
    let mut mux = Muxer::new(build_cfg()).unwrap();
    let handle = mux.video_handles()[0];
    mux.push_video_to(handle, au, Pts90khz::new(90_000), true)
        .expect("push_video_to");
    let ts_bytes = drain_mux(&mut mux);

    let mut demux = if av1_interop {
        Demuxer::with_config(
            DemuxerConfig::builder()
                .av1_carriage(Av1CarriageMode::InteropRawObu)
                .build(),
        )
    } else {
        Demuxer::with_config(DemuxerConfig::builder().build())
    };
    demux.feed(&ts_bytes).unwrap();
    // flush drains unbounded video PES (PES_packet_length=0) buffered in-flight.
    demux.flush();

    let mut raws = Vec::new();
    while let Some(event) = demux.next_event() {
        if let DemuxEvent::Sample {
            payload: SamplePayload::Video { raw, .. },
            ..
        } = event
        {
            raws.push(raw);
        }
    }
    raws
}

/// Drive the full demux→mux→demux fixpoint for one codec: mux `original` →
/// `raw1`, assert the muxer passed it through byte-for-byte (`original == raw1`),
/// then re-mux `raw1` → `raw2` and assert the cycle is byte-stable
/// (`raw1 == raw2`). `av1_interop` selects interop-mode mux+demux (the
/// byte-transparent AV1 carriage path); `label` names the codec in panics.
fn roundtrip_one_codec(
    original: &[u8],
    build_cfg: impl Fn() -> MuxerConfig,
    av1_interop: bool,
    label: &str,
) {
    let raw1_vec = mux_then_collect_video_raws(original, &build_cfg, av1_interop);
    assert_eq!(
        raw1_vec.len(),
        1,
        "{label}: expected exactly 1 video AU from first demux"
    );
    let raw1 = &raw1_vec[0];

    // original → raw1: the muxer passes the encoded AU through without reframing.
    assert_eq!(
        &**raw1, original,
        "{label}: original AU must survive mux→demux byte-for-byte"
    );

    let raw2_vec = mux_then_collect_video_raws(raw1, &build_cfg, av1_interop);
    assert_eq!(
        raw2_vec.len(),
        1,
        "{label}: expected exactly 1 video AU from second demux"
    );
    let raw2 = &raw2_vec[0];

    // Fixpoint: raw1 == raw2.
    assert_eq!(
        raw1.as_ref(),
        raw2.as_ref(),
        "{label}: raw AU must be byte-stable across demux→mux→demux (fixpoint)"
    );
}

/// Build a single-program, single-video-stream config for `codec`. AV1 uses
/// `Av1CarriageMode::InteropRawObu` so raw OBU bytes pass through without the
/// MPEG-2-TS binding wrapping (start-code insertion + emulation-prevention),
/// keeping the carriage byte-transparent.
fn build_video_cfg(codec: MuxVideoCodec) -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
    prog.add_video(0x101, codec);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    if codec == MuxVideoCodec::Av1 {
        b.av1_carriage(Av1CarriageMode::InteropRawObu);
    }
    b.build().unwrap()
}

// ─────────────────────────────────────────────────────────────────────────────
// Test functions — one per codec (thin callers of `roundtrip_one_codec`)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn h264_raw_roundtrip_fixpoint() {
    roundtrip_one_codec(
        &synthetic_h264_au(),
        || build_video_cfg(MuxVideoCodec::H264),
        false,
        "H.264",
    );
}

#[test]
fn h265_raw_roundtrip_fixpoint() {
    roundtrip_one_codec(
        &synthetic_h265_au(),
        || build_video_cfg(MuxVideoCodec::H265),
        false,
        "H.265",
    );
}

#[test]
fn h266_raw_roundtrip_fixpoint() {
    roundtrip_one_codec(
        &synthetic_h266_au(),
        || build_video_cfg(MuxVideoCodec::H266),
        false,
        "H.266",
    );
}

/// AV1 uses interop-mode carriage (mux + demux), the byte-faithful path.
///
/// Contrast: binding mode (`Av1CarriageMode::Mpeg2TsBinding`) reframes each
/// OBU with a `ts_open_bitstream_unit()` start code and emulation-prevention
/// bytes, so `original != raw1` in that mode (the wire payload diverges from
/// the bare OBU bytes). That mode has its own fixpoint tests in
/// `codec/av1_carriage_roundtrip.rs`. Here we exercise the interop path
/// because it is the byte-transparent channel.
#[test]
fn av1_interop_raw_roundtrip_fixpoint() {
    roundtrip_one_codec(
        &synthetic_av1_au(),
        || build_video_cfg(MuxVideoCodec::Av1),
        true,
        "AV1 interop-mode",
    );
}
