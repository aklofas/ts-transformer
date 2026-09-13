//! Synthetic AU + KLV fixture factory for the interop test driver.
//!
//! Every byte recipe here is lifted from an existing tst-core generator,
//! example, or test rather than invented fresh, so the shapes are the same
//! ones tst-core's own muxer/demuxer and codec parsers already accept. See
//! each function's doc comment for its source.

use crate::impair::XorShift64;
use crate::profiles::VideoCodec;
use tst_core::klv::st0601::{UasDatalinkLs, encode_to_vec};

/// Every 30th frame (0, 30, 60, ...) is a keyframe.
const KEYFRAME_INTERVAL: u32 = 30;

/// Which size regime the synthetic AU factory targets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuSizeMode {
    /// The original tiny fixtures (tens of bytes per AU) — byte-
    /// identical to what [`video_au`] has always produced, and what the
    /// 157-cell interop matrix's expectations were validated against.
    /// Stays the default everywhere except where a caller explicitly
    /// opts in to `Realistic`.
    Compact,
    /// GOP-structured sizes matching a real encoder's output shape:
    /// keyframes tens of KB, inter frames single-digit KB, both varying
    /// per frame (seeded from `frame_idx`, so the same frame is always
    /// byte-identical across processes and replays). At the schedule's
    /// 30 fps this lands the video elementary stream near ~1.7 Mb/s —
    /// the soak's "true bandwidth" regime, exercising real PES/TS
    /// packetization bursts (a keyframe spans hundreds of TS packets)
    /// instead of the compact fixtures' one-or-two.
    Realistic,
}

/// Realistic-mode slice payload size bounds (bytes), drawn uniformly
/// per frame. Keyframe ~28-52 KiB, inter ~2-10 KiB: at 30 fps / 30-frame
/// GOPs that averages ~217 KB/s ≈ 1.7 Mb/s of elementary stream —
/// representative of a modest HD gimbal feed, and comfortably under the
/// demuxer's 4 MiB per-PID PES reassembly cap.
const REALISTIC_KEY_PAYLOAD: (usize, usize) = (28_672, 53_248);
const REALISTIC_INTER_PAYLOAD: (usize, usize) = (2_048, 10_240);

/// Deterministic per-frame slice payload for [`AuSizeMode::Realistic`]:
/// length drawn from the bounds above, bytes drawn from a PRNG seeded
/// by `frame_idx` alone (same frame → identical bytes, forever). Every
/// byte is remapped to non-zero so the payload can never contain an
/// Annex-B `00 00 01` sequence — it rides inside a single NAL, where a
/// bogus start code would break the demuxer's AU splitting (real
/// encoders solve this with emulation-prevention bytes; a fixture can
/// simply never emit 0x00).
fn realistic_slice_payload(frame_idx: u32, keyframe: bool) -> Vec<u8> {
    let mut rng = XorShift64::new(
        (frame_idx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5EED_AB1E_F1E1_D000,
    );
    let (lo, hi) = if keyframe {
        REALISTIC_KEY_PAYLOAD
    } else {
        REALISTIC_INTER_PAYLOAD
    };
    let len = lo + (rng.next_u64() as usize) % (hi - lo + 1);
    let mut buf = vec![0u8; len];
    for chunk in buf.chunks_mut(8) {
        let mut v = rng.next_u64();
        for b in chunk.iter_mut() {
            *b = ((v & 0xFF) as u8).max(1);
            v >>= 8;
        }
    }
    buf
}

/// Annex-B start code shared by the H.264/H.265/H.266 AUs below.
const ANNEX_B_START: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// Build one video access unit for `codec` at `frame_idx`. Returns
/// `(bytes, is_keyframe)` — a keyframe (parameter sets + IDR slice/frame)
/// every 30th frame, an inter slice/frame otherwise. Equivalent to
/// [`video_au_sized`] with [`AuSizeMode::Compact`].
pub fn video_au(codec: VideoCodec, frame_idx: u32) -> (Vec<u8>, bool) {
    video_au_sized(codec, frame_idx, AuSizeMode::Compact)
}

/// [`video_au`] with an explicit [`AuSizeMode`]. `Compact` is byte-
/// identical to what `video_au` has always produced; `Realistic`
/// appends a deterministic per-frame payload (see this module's
/// private `realistic_slice_payload`) to the slice/tile-group NAL/OBU,
/// giving the AU stream a real encoder's GOP size structure.
pub fn video_au_sized(codec: VideoCodec, frame_idx: u32, mode: AuSizeMode) -> (Vec<u8>, bool) {
    let keyframe = frame_idx % KEYFRAME_INTERVAL == 0;
    let extra = match mode {
        AuSizeMode::Compact => Vec::new(),
        AuSizeMode::Realistic => realistic_slice_payload(frame_idx, keyframe),
    };
    let bytes = match codec {
        VideoCodec::H264 => h264_au(frame_idx, keyframe, &extra),
        VideoCodec::H265 => h265_au(frame_idx, keyframe, &extra),
        VideoCodec::H266 => h266_au(frame_idx, keyframe, &extra),
        VideoCodec::Av1 => av1_au(frame_idx, keyframe, &extra),
    };
    (bytes, keyframe)
}

/// Filler bytes for a non-keyframe slice/frame, sized off `frame_idx` the
/// way `examples/muxing/mux_h265_with_klv.rs:133` varies its inter-AU
/// sizes (`au.resize(1000 + (i as usize % 200), 0xA5)`), scaled down for a
/// lightweight fixture.
fn filler(frame_idx: u32) -> Vec<u8> {
    let len = 32 + (frame_idx as usize % 32);
    vec![0xA5; len]
}

/// H.264 Annex-B AU. Keyframe = SPS (NAL type 7) + PPS (type 8) + IDR
/// slice (type 5), byte-for-byte from `build_h264_keyframe_au` in
/// `examples/muxing/mux_audio_video_klv.rs:163-185`. Non-keyframe = a
/// single non-IDR slice NAL (type 1, `nal_ref_idc=2` → header byte
/// `0x41`), the same NAL-header formula that recipe uses for its slice.
///
/// `extra` (empty in Compact mode) extends the slice NAL's payload —
/// appended after the IDR slice bytes / inter filler, inside the same
/// NAL, so the AU's NAL structure is identical in both size modes.
fn h264_au(frame_idx: u32, keyframe: bool, extra: &[u8]) -> Vec<u8> {
    let mut au = Vec::new();
    if keyframe {
        au.extend_from_slice(&ANNEX_B_START);
        au.extend_from_slice(&[0x67, 0x42, 0x00, 0x1F, 0xE9, 0x02, 0x80, 0x14, 0x07, 0x80]);
        au.extend_from_slice(&ANNEX_B_START);
        au.extend_from_slice(&[0x68, 0xCE, 0x06, 0xE2]);
        au.extend_from_slice(&ANNEX_B_START);
        au.extend_from_slice(&[
            0x65, 0x88, 0x80, 0x40, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00,
        ]);
        au.extend_from_slice(extra);
    } else {
        au.extend_from_slice(&ANNEX_B_START);
        au.push(0x41); // non-IDR slice: nal_ref_idc=2, nal_unit_type=1
        au.extend(filler(frame_idx));
        au.extend_from_slice(extra);
    }
    au
}

/// H.265 Annex-B AU, the exact shape built inline in
/// `examples/muxing/mux_h265_with_klv.rs:115-133`: 2-byte NAL header
/// (`nal_type << 1`, then `0x01` for `nuh_layer_id=0` +
/// `nuh_temporal_id_plus1=1`), IDR_W_RADL (19) for the keyframe, TRAIL_N
/// (1) otherwise.
fn h265_au(frame_idx: u32, keyframe: bool, extra: &[u8]) -> Vec<u8> {
    let nal_type: u8 = if keyframe { 19 } else { 1 };
    let mut au = Vec::new();
    au.extend_from_slice(&ANNEX_B_START);
    au.push(nal_type << 1);
    au.push(0x01);
    au.extend(filler(frame_idx));
    au.extend_from_slice(extra);
    au
}

/// H.266 Annex-B AU. Parameter-set RBSP bytes are lifted verbatim from
/// `crates/tst-core/tests/tools/gen_h266_fixtures.rs` (`vps_main10`,
/// `sps_main10`, `pps_main10` — the same bytes as the
/// `codec::h266::{vps,sps,pps}::tests::minimal_*_rbsp()` unit fixtures),
/// wrapped in the 2-byte VVC NAL header per H.266 V4 Table 5
/// (`nal_unit_type << 3 | nuh_temporal_id_plus1`; VPS_NUT=14, SPS_NUT=15,
/// PPS_NUT=16, IDR_W_RADL=7, TRAIL_NUT=0 — confirmed against
/// `codec::h266::mod::parse_parameter_sets` and
/// `codec::h266::slice_header_light::is_idr_nal`).
fn h266_au(frame_idx: u32, keyframe: bool, extra: &[u8]) -> Vec<u8> {
    fn nal(nal_type: u8, rbsp: &[u8]) -> Vec<u8> {
        let mut v = ANNEX_B_START.to_vec();
        v.push(0x00); // forbidden_zero_bit | nuh_reserved_zero_bit | nuh_layer_id[5:0]=0
        v.push((nal_type << 3) | 0x01); // nal_unit_type(5) | nuh_temporal_id_plus1=1
        v.extend_from_slice(rbsp);
        v
    }
    let slice_body: Vec<u8> = filler(frame_idx)
        .into_iter()
        .chain(extra.iter().copied())
        .collect();
    let mut au = Vec::new();
    if keyframe {
        au.extend(nal(14, &[0x00, 0x02])); // VPS
        au.extend(nal(
            15,
            &[
                0x00, 0x09, 0x02, 0x3f, 0x00, 0x00, 0x00, 0x28, 0x20, 0x3c, 0x48, 0x00, 0x5d, 0xb0,
                0xf8, 0x06, 0x02, 0x08, 0x00, 0x02,
            ],
        )); // SPS
        au.extend(nal(16, &[0x00, 0x20])); // PPS
        au.extend(nal(7, &slice_body)); // IDR_W_RADL slice
    } else {
        au.extend(nal(0, &slice_body)); // TRAIL_NUT slice
    }
    au
}

/// AV1 low-overhead OBU sequence — AV1 has no Annex-B framing, so this is
/// the codec's own "start-code" equivalent. Structure and the
/// `(obu_type << 3) | 0x02` header formula (`obu_has_size_field=1`,
/// single-byte LEB128 size) are lifted verbatim from
/// `synthetic_av1_au`/`obu` in
/// `crates/tst-core/tests/codec/av1_carriage_roundtrip.rs:25-47`: a
/// Temporal Delimiter + Sequence Header + Frame Header + Tile Group. The
/// Sequence Header body and the keyframe Frame Header body are the exact
/// bytes from `crates/tst-core/tests/tools/gen_av1_fixtures.rs`
/// (`seq_header_main_320x240`, `frame_header_keyframe`); the non-keyframe
/// Frame Header body flips `frame_type` from KEY_FRAME(0) to
/// INTER_FRAME(1) per that file's documented bit layout
/// (`show_existing_frame(1) | frame_type(2) | show_frame(1)` in the high
/// nibble).
fn av1_au(frame_idx: u32, keyframe: bool, extra: &[u8]) -> Vec<u8> {
    // OBU header + TRUE LEB128 size (AV1 §4.10.5) — one byte per 7 size
    // bits, high bit = continuation. Byte-identical to the original
    // single-byte form for every body under 128 bytes (i.e. every
    // Compact-mode OBU), and correct for Realistic mode's multi-KB tile
    // groups, which the original `body.len() as u8` cast would silently
    // truncate into a corrupt stream.
    fn obu(obu_type: u8, body: &[u8]) -> Vec<u8> {
        let header = (obu_type << 3) | 0x02;
        let mut v = vec![header];
        let mut n = body.len();
        loop {
            let byte = (n & 0x7F) as u8;
            n >>= 7;
            if n == 0 {
                v.push(byte);
                break;
            }
            v.push(byte | 0x80);
        }
        v.extend_from_slice(body);
        v
    }
    const SEQ_HEADER: [u8; 10] = [0, 0, 0, 4, 60, 255, 188, 0, 0, 0];
    let frame_header: [u8; 1] = if keyframe { [0x10] } else { [0x30] };

    let mut au = Vec::new();
    au.extend(obu(2, &[])); // Temporal Delimiter (always empty body)
    if keyframe {
        au.extend(obu(1, &SEQ_HEADER)); // Sequence Header
    }
    au.extend(obu(3, &frame_header)); // Frame Header
    let tile_len = 3 + (frame_idx as usize % 8);
    let tile_body: Vec<u8> = core::iter::repeat_n(0xA5, tile_len)
        .chain(extra.iter().copied())
        .collect();
    au.extend(obu(4, &tile_body)); // Tile Group
    au
}

/// Build an ST 0601 UAS Datalink Local Set with a handful of numeric tags
/// derived from `seq`, so consecutive records differ on the wire. Returns
/// raw KLV LS bytes (Universal Label + BER length + TLVs + checksum) —
/// **not** AU-cell-wrapped. Per this workspace's KLV convention,
/// `Muxer::push_klv` / `MuxSender::send_klv` prepend the 5-byte
/// `Metadata_AU_cell` header themselves for `SynchronousMetadata`
/// streams, so callers always pass raw LS bytes here.
pub fn klv_record(seq: u32) -> Vec<u8> {
    let rec = UasDatalinkLs {
        // Tag 2: Precision Time Stamp (µs since Unix epoch) — same base
        // value as `gen_synthetic_fixtures.rs::minimal()`, offset by
        // `seq` seconds.
        timestamp_us: Some(1_700_000_000_000_000 + (seq as u64) * 1_000_000),
        // Tag 5: Platform Heading Angle — encode range [0, 360] deg.
        platform_heading_deg: Some((seq % 360) as f64),
        // Tag 13/14: Sensor Latitude/Longitude — oscillate a bounded,
        // 1°-wide triangle wave off a fixed point (lat climbs
        // 38.0→39.0 and back; lon mirrors it, -121.5→-122.5 and back)
        // so records differ without EVER leaving the encode ranges.
        // The original unbounded `38.0 + seq * 0.0001` walk crossed
        // Tag 13's +90 max at seq 520_001 (14h27m into a 10 Hz soak)
        // and panicked both senders of the first 72h soak run — see
        // `klv_record_encodes_across_a_full_72h_soak_seq_range`.
        sensor_lat_deg: Some(38.0 + triangle_wave(seq) * 0.0001),
        sensor_lon_deg: Some(-121.5 - triangle_wave(seq) * 0.0001),
        ..Default::default()
    };
    encode_to_vec(&rec).expect("well-formed UasDatalinkLs always encodes")
}

/// Triangle wave over `seq` with period 20_000 and amplitude
/// `0..=10_000`: ramps 0→10_000 then back down to 0, forever. Scaled by
/// 0.0001° per step in [`klv_record`], this bounds each coordinate's
/// walk to a 1°-wide window on one side of its base point (the offset
/// is `0..=+1.0°`, added to the latitude base and subtracted from the
/// longitude base — not symmetric about either) while consecutive
/// `seq` values still map to different coordinates (the direction
/// reverses at the peaks, it never repeats a value two steps in a
/// row).
fn triangle_wave(seq: u32) -> f64 {
    const HALF_PERIOD: u32 = 10_000;
    let phase = seq % (2 * HALF_PERIOD);
    let tri = if phase <= HALF_PERIOD {
        phase
    } else {
        2 * HALF_PERIOD - phase
    };
    tri as f64
}

/// Build a single 7-byte-header ADTS AAC frame (no CRC). Header layout
/// lifted verbatim from `make_adts_buf` in
/// `crates/tst-core/benches/codec_parsers.rs:105-140` (MPEG-2 ID, AAC-LC
/// profile, 48 kHz, stereo, 1 raw data block) — the same bit layout
/// `codec::aac::frames` parses, and the rate `schedule::AUDIO_SAMPLE_RATE_HZ`
/// paces frames at (1024 samples/frame ⇒ `oracles::audio`'s cadence
/// check). Body bytes vary with `seq` so consecutive frames differ on the
/// wire.
pub fn aac_frame(seq: u32) -> Vec<u8> {
    const BODY_LEN: usize = 100;
    const FRAME_LEN: u32 = 7 + BODY_LEN as u32;
    const SAMPLE_RATE_INDEX: u8 = 3; // 48000 Hz
    const CHANNEL_CONFIG: u8 = 2; // stereo

    let mut h = [0u8; 7];
    h[0] = 0xFF;
    h[1] = 0b1111_0000 | (1 << 3) | 1; // ID=MPEG-2, layer=0, no CRC
    h[2] = (1 << 6) | ((SAMPLE_RATE_INDEX & 0xF) << 2) | ((CHANNEL_CONFIG >> 2) & 1); // profile=LC
    h[3] = ((CHANNEL_CONFIG & 0b11) << 6) | (((FRAME_LEN >> 11) & 0b11) as u8);
    h[4] = ((FRAME_LEN >> 3) & 0xFF) as u8;
    h[5] = (((FRAME_LEN & 0b111) as u8) << 5) | 0b1_1111;
    h[6] = 0b11_1111 << 2; // buffer_fullness low bits | num_raw_data_blocks=0

    let mut frame = h.to_vec();
    frame.extend((0..BODY_LEN).map(|i| (seq as u8).wrapping_add(i as u8)));
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use tst_core::klv::st0601::decode;

    #[test]
    fn klv_record_varies_with_seq_and_decodes_back() {
        let a = klv_record(1);
        let b = klv_record(2);
        assert_ne!(a, b, "records for different seq must differ on the wire");
        for bytes in [&a, &b] {
            let _ = decode(bytes).expect("well-formed ST 0601 record must decode");
        }
    }

    /// Soak run 1 (2026-08-04) died 14.5h in: the old unbounded
    /// `38.0 + seq * 0.0001` latitude walk crossed Tag 13's +90 encode
    /// max at seq 520_001 (10 Hz KLV cadence → 14h27m) and the
    /// `encode_to_vec(...).expect(...)` panicked both soak senders. A
    /// 72h run at 10 Hz needs 2_592_000 records; sweep past that with a
    /// prime stride (fast, hits ~3000 points across every oscillation
    /// period) plus the exact seq that killed the run.
    #[test]
    fn klv_record_encodes_across_a_full_72h_soak_seq_range() {
        for seq in (0..3_000_000u32).step_by(997) {
            let _ = klv_record(seq);
        }
        let _ = klv_record(520_001);
    }

    /// The bounded walk must stay inside Tag 13/14's encode ranges at
    /// its turnaround points (where a bounds bug would sit) and still
    /// produce in-range values a decoder hands back.
    #[test]
    fn klv_record_lat_lon_stay_in_range_at_oscillation_peaks() {
        for seq in [
            0u32, 9_999, 10_000, 10_001, 19_999, 20_000, 520_001, 2_591_999,
        ] {
            let rec = decode(&klv_record(seq)).expect("record must decode");
            let lat = rec.sensor_lat_deg.expect("lat present");
            let lon = rec.sensor_lon_deg.expect("lon present");
            assert!(
                (-90.0..=90.0).contains(&lat),
                "seq {seq}: lat {lat} out of range"
            );
            assert!(
                (-180.0..=180.0).contains(&lon),
                "seq {seq}: lon {lon} out of range"
            );
        }
    }

    #[test]
    fn video_au_frame_zero_is_keyframe_frame_one_is_not() {
        for codec in [
            VideoCodec::H264,
            VideoCodec::H265,
            VideoCodec::H266,
            VideoCodec::Av1,
        ] {
            let (frame0, key0) = video_au(codec, 0);
            let (frame1, key1) = video_au(codec, 1);
            assert!(key0, "{codec:?} frame 0 must be a keyframe");
            assert!(!key1, "{codec:?} frame 1 must not be a keyframe");
            assert!(!frame0.is_empty(), "{codec:?} frame 0 must be non-empty");
            assert!(!frame1.is_empty(), "{codec:?} frame 1 must be non-empty");
            assert_prefix(codec, &frame0);
            assert_prefix(codec, &frame1);
        }
    }

    fn assert_prefix(codec: VideoCodec, bytes: &[u8]) {
        match codec {
            VideoCodec::H264 | VideoCodec::H265 | VideoCodec::H266 => {
                assert!(
                    bytes.starts_with(&ANNEX_B_START),
                    "{codec:?} AU must start with the Annex-B start code"
                );
            }
            VideoCodec::Av1 => {
                // Temporal Delimiter OBU header: (obu_type=2 << 3) | has_size_field(0x02).
                assert_eq!(
                    bytes[0], 0x12,
                    "AV1 AU must start with a Temporal Delimiter OBU header"
                );
            }
        }
    }

    /// Realistic mode must produce GOP-structured sizes — keyframes
    /// tens of KB, inter frames single-digit KB, both varying per frame
    /// — deterministically (same `frame_idx` → identical bytes, so
    /// send-side ground truth and any replay agree byte-for-byte).
    #[test]
    fn realistic_au_sizes_are_gop_structured_and_deterministic() {
        for codec in [
            VideoCodec::H264,
            VideoCodec::H265,
            VideoCodec::H266,
            VideoCodec::Av1,
        ] {
            let (key, is_key) = video_au_sized(codec, 0, AuSizeMode::Realistic);
            assert!(is_key, "{codec:?} frame 0 must be a keyframe");
            assert!(
                key.len() >= 20_000,
                "{codec:?} realistic keyframe must be tens of KB, got {}",
                key.len()
            );
            let mut inter_lens = std::collections::HashSet::new();
            for idx in 1..30u32 {
                let (inter, k) = video_au_sized(codec, idx, AuSizeMode::Realistic);
                assert!(!k);
                assert!(
                    inter.len() >= 1_000 && inter.len() < key.len(),
                    "{codec:?} frame {idx}: inter AU {} bytes not in (1KB, keyframe)",
                    inter.len()
                );
                inter_lens.insert(inter.len());
            }
            assert!(
                inter_lens.len() > 10,
                "{codec:?}: inter AU sizes must vary across a GOP, got {} distinct",
                inter_lens.len()
            );
            let (key2, _) = video_au_sized(codec, 0, AuSizeMode::Realistic);
            assert_eq!(
                key, key2,
                "{codec:?}: same frame_idx must be byte-identical"
            );
        }
    }

    /// Compact mode must be byte-for-byte what `video_au` has always
    /// produced — the 157-cell interop matrix's expectations were
    /// validated against those exact fixtures and must not shift.
    #[test]
    fn compact_mode_is_byte_identical_to_video_au() {
        for codec in [
            VideoCodec::H264,
            VideoCodec::H265,
            VideoCodec::H266,
            VideoCodec::Av1,
        ] {
            for idx in [0u32, 1, 7, 30, 31] {
                assert_eq!(
                    video_au(codec, idx),
                    video_au_sized(codec, idx, AuSizeMode::Compact),
                    "{codec:?} frame {idx}"
                );
            }
        }
    }

    /// The realistic filler rides INSIDE a single NAL, so it must never
    /// contain a 0x00 byte — three-byte `00 00 01` inside a NAL payload
    /// would read as a bogus Annex-B start code to the demuxer's AU
    /// splitter (real encoders escape those with emulation-prevention
    /// bytes; the fixture sidesteps the problem entirely by never
    /// emitting 0x00 in filler). Start-code count therefore equals NAL
    /// count exactly.
    #[test]
    fn realistic_annex_b_aus_contain_no_emulated_start_codes() {
        let start_code_count =
            |au: &[u8]| au.windows(3).filter(|w| w == &[0x00, 0x00, 0x01]).count();
        // H.264 keyframe = SPS + PPS + IDR = 3 NALs; inter = 1 NAL.
        let (key, _) = video_au_sized(VideoCodec::H264, 0, AuSizeMode::Realistic);
        assert_eq!(start_code_count(&key), 3);
        let (inter, _) = video_au_sized(VideoCodec::H264, 1, AuSizeMode::Realistic);
        assert_eq!(start_code_count(&inter), 1);
        // H.265/H.266 keyframes carry their parameter sets + slice.
        let (key265, _) = video_au_sized(VideoCodec::H265, 0, AuSizeMode::Realistic);
        assert_eq!(start_code_count(&key265), 1); // single-NAL keyframe shape
        let (key266, _) = video_au_sized(VideoCodec::H266, 0, AuSizeMode::Realistic);
        assert_eq!(start_code_count(&key266), 4); // VPS + SPS + PPS + IDR
    }

    /// Realistic AV1 tile groups exceed 127 bytes, so the OBU size field
    /// must be real multi-byte LEB128 — walk the AU by parsing each OBU
    /// header + LEB128 size and confirm the walk consumes it exactly.
    #[test]
    fn realistic_av1_au_walks_cleanly_by_leb128_obu_sizes() {
        for idx in [0u32, 1, 15] {
            let (au, _) = video_au_sized(VideoCodec::Av1, idx, AuSizeMode::Realistic);
            let mut pos = 0usize;
            let mut obus = 0usize;
            while pos < au.len() {
                pos += 1; // OBU header byte (has_size_field always set)
                let mut size = 0u64;
                let mut shift = 0u32;
                loop {
                    let b = au[pos];
                    pos += 1;
                    size |= u64::from(b & 0x7F) << shift;
                    if b & 0x80 == 0 {
                        break;
                    }
                    shift += 7;
                }
                pos += size as usize;
                obus += 1;
            }
            assert_eq!(
                pos,
                au.len(),
                "frame {idx}: OBU walk must land exactly at end"
            );
            assert!(obus >= 2, "frame {idx}: at least TD + tile group");
        }
    }

    #[test]
    fn aac_frame_parses_as_one_lc_stereo_48000_frame() {
        let a = aac_frame(1);
        let b = aac_frame(2);
        assert_ne!(a, b, "frames for different seq must differ on the wire");

        let mut frames = tst_core::codec::aac::frames(&a);
        let frame = frames
            .next()
            .expect("frame should parse")
            .expect("frame should parse");
        assert_eq!(frame.profile, tst_core::codec::aac::AacProfile::Lc);
        assert_eq!(frame.sample_rate_hz, 48_000);
        assert_eq!(frame.channel_configuration, 2);
        assert!(frames.next().is_none(), "buffer holds exactly one frame");
    }
}
