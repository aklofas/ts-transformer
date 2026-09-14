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

// ============================================================================
// Rich ST 0601 records (spec §5.3)
// ============================================================================

/// Salt mixed into the run seed before drawing the rich presence
/// schedule, so the KLV schedule is statistically independent of every
/// other seeded component of the same run (impairment, corruption, AU
/// sizes) even though all of them share one run seed. The digits spell
/// the two standards this generator speaks plus the tag that nests one
/// inside the other: 0601, 0102, 48.
pub const KLV_SALT: u64 = 0x5EC0_0601_0102_0048;

/// Per-record `timestamp_us` step in rich mode — 10 Hz, i.e.
/// `seq * 100_000` µs. Rich mode times records at the real soak KLV
/// cadence (the compact record's 1 s step is a fixture artifact), so a
/// receiver can invert a decoded timestamp back to the sender's `seq`
/// with [`rich_seq_of_timestamp`].
pub const RICH_TS_STEP_US: u64 = 100_000;

/// Epoch for rich-mode timestamps — the same base value
/// [`klv_record`] uses (`gen_synthetic_fixtures.rs::minimal()`).
pub const RICH_TS_BASE_US: u64 = 1_700_000_000_000_000;

/// How many records one core-only record falls in, on average: the
/// schedule reserves one `seq` residue in 50 for a record that carries
/// nothing but [`RICH_CORE_TAGS`], so a receiver-side oracle has to cope
/// with a legitimately sparse record rather than assuming every record
/// carries the same tag set.
const CORE_ONLY_PERIOD: u32 = 50;

/// Which ST 0601 tag set the KLV generator emits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KlvSet {
    /// The original 4-tag, 50-byte record — byte-identical to what
    /// [`klv_record`] has always produced. The 157-cell interop matrix's
    /// expectations were validated against these exact bytes, so this
    /// stays the default everywhere.
    Compact,
    /// A realistic ~32-tag record: the same walking core, plus six
    /// optional tag groups that come and go on a seeded schedule, plus a
    /// nested ST 0102 security set. Exercises the demuxer/decoder against
    /// records whose tag set varies record to record, which is what a
    /// real ST 0601 producer emits.
    Rich,
}

impl KlvSet {
    /// Inverse of the wire names used on the command line (`compact` /
    /// `rich`). Case-sensitive, like [`crate::corrupt::Class::parse`].
    #[must_use]
    pub fn parse(s: &str) -> Option<KlvSet> {
        match s {
            "compact" => Some(KlvSet::Compact),
            "rich" => Some(KlvSet::Rich),
            _ => None,
        }
    }
}

/// The optional ST 0601 tag groups a rich record carries on a seeded
/// schedule (spec §5.3). Each group is all-or-nothing: a record either
/// carries every tag of the group or none of them, which is how a real
/// producer behaves (a platform either has a pose solution this frame or
/// it does not).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum RichGroup {
    Pose,
    Optics,
    Frame,
    Target,
    Identity,
    Security,
}

impl RichGroup {
    /// Every group, in declaration order. This order is load-bearing:
    /// [`rich_presence`] draws one phase per group in exactly this
    /// sequence, so reordering the enum would silently reshuffle every
    /// seed's schedule.
    pub const ALL: [RichGroup; 6] = [
        RichGroup::Pose,
        RichGroup::Optics,
        RichGroup::Frame,
        RichGroup::Target,
        RichGroup::Identity,
        RichGroup::Security,
    ];
}

/// Tags every rich record carries, no matter the schedule: Checksum (1),
/// Precision Time Stamp (2), Platform Heading (5), Sensor Lat/Lon/Alt
/// (13/14/15) and UAS LS Version (65).
///
/// Tag 1 is on the list because `st0601::encode_to_vec` appends a
/// checksum to every record it writes and `st0601::decode` validates it,
/// so it is always on the wire even though no model field holds it.
pub const RICH_CORE_TAGS: [u8; 7] = [1, 2, 5, 13, 14, 15, 65];

/// The tags belonging to `g`. Kept adjacent to [`observed_tags`]'s
/// field table so the two stay diffable by eye — they are the two halves
/// of the census oracle's identity (declared presence == observed tags).
#[must_use]
pub fn rich_group_tags(g: RichGroup) -> &'static [u8] {
    match g {
        // Platform pitch/roll + sensor relative az/el/roll.
        RichGroup::Pose => &[6, 7, 18, 19, 20],
        // Sensor H/V field of view, slant range, target width.
        RichGroup::Optics => &[16, 17, 21, 22],
        // Frame centre lat/lon/elev + the eight corner offsets.
        RichGroup::Frame => &[23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33],
        // Target location lat/lon/elev + platform ground speed.
        RichGroup::Target => &[40, 41, 42, 56],
        // Mission id, tail number, platform designation, image source.
        RichGroup::Identity => &[3, 4, 10, 11],
        // Nested ST 0102 security local set.
        RichGroup::Security => &[48],
    }
}

/// How often `g` recurs, in records. Fixed per group (never drawn), so
/// the *rate* each group appears at is a property of the generator and
/// only the *offset* varies with the seed: pose and frame geometry every
/// record, optics every other, target every third, security every fifth,
/// identity every tenth — the shape a real producer emits, where the
/// slow-changing descriptive tags are sent far less often than the
/// per-frame geometry.
fn rich_group_period(g: RichGroup) -> u32 {
    match g {
        RichGroup::Pose => 1,
        RichGroup::Optics => 2,
        RichGroup::Frame => 1,
        RichGroup::Target => 3,
        RichGroup::Identity => 10,
        RichGroup::Security => 5,
    }
}

/// Which tags the rich record at `seq` carries, as a pure function of
/// `(seed, seq)` — no state, no clock, so a sender and a receiver-side
/// oracle can compute the same answer independently and a replay of the
/// same run reproduces it exactly.
///
/// **Draw order (part of the determinism contract — changing it changes
/// every seed's schedule):** from `XorShift64::new(seed ^ KLV_SALT)`,
/// draw one phase per group in [`RichGroup::ALL`] order, then the
/// core-only phase. Every group's phase is drawn unconditionally,
/// including the period-1 groups whose phase can only be 0, so the
/// period table and the draw order stay independent of each other.
///
/// A group is present iff `seq % period == phase`; a record whose `seq`
/// hits the core-only residue carries [`RICH_CORE_TAGS`] alone.
#[must_use]
pub fn rich_presence(seed: u64, seq: u32) -> std::collections::BTreeSet<u8> {
    let mut rng = XorShift64::new(seed ^ KLV_SALT);
    let mut phases = [0u32; RichGroup::ALL.len()];
    for (slot, g) in phases.iter_mut().zip(RichGroup::ALL) {
        *slot = (rng.next_u64() % u64::from(rich_group_period(g))) as u32;
    }
    let core_only_phase = (rng.next_u64() % u64::from(CORE_ONLY_PERIOD)) as u32;

    let mut tags: std::collections::BTreeSet<u8> = RICH_CORE_TAGS.into_iter().collect();
    if seq % CORE_ONLY_PERIOD == core_only_phase {
        return tags;
    }
    for (phase, g) in phases.into_iter().zip(RichGroup::ALL) {
        if seq % rich_group_period(g) == phase {
            tags.extend(rich_group_tags(g).iter().copied());
        }
    }
    tags
}

/// The tag ids a decoded record actually carries — the observed half of
/// the census oracle whose declared half is [`rich_presence`]. Covers
/// exactly the tags the rich generator can set (core + all six groups);
/// tags outside that set are ignored, so this is not a general-purpose
/// "what's in this record" helper.
#[must_use]
pub fn observed_tags(rec: &UasDatalinkLs) -> std::collections::BTreeSet<u8> {
    // Mirrors RICH_CORE_TAGS and rich_group_tags above, in the same
    // order — keep the two adjacent and diff them whenever either moves.
    let present = [
        // Core, minus Tag 1: the checksum has no model field (encode
        // appends it, decode validates and consumes it), so a record
        // that decoded at all carries it.
        (2, rec.timestamp_us.is_some()),
        (5, rec.platform_heading_deg.is_some()),
        (13, rec.sensor_lat_deg.is_some()),
        (14, rec.sensor_lon_deg.is_some()),
        (15, rec.sensor_alt_m.is_some()),
        (65, rec.uas_ls_version.is_some()),
        // Pose
        (6, rec.platform_pitch_deg.is_some()),
        (7, rec.platform_roll_deg.is_some()),
        (18, rec.sensor_rel_az_deg.is_some()),
        (19, rec.sensor_rel_el_deg.is_some()),
        (20, rec.sensor_rel_roll_deg.is_some()),
        // Optics
        (16, rec.sensor_hfov_deg.is_some()),
        (17, rec.sensor_vfov_deg.is_some()),
        (21, rec.slant_range_m.is_some()),
        (22, rec.target_width_m.is_some()),
        // Frame
        (23, rec.frame_center_lat_deg.is_some()),
        (24, rec.frame_center_lon_deg.is_some()),
        (25, rec.frame_center_elev_m.is_some()),
        (26, rec.corner_lat_offset_p1_deg.is_some()),
        (27, rec.corner_lon_offset_p1_deg.is_some()),
        (28, rec.corner_lat_offset_p2_deg.is_some()),
        (29, rec.corner_lon_offset_p2_deg.is_some()),
        (30, rec.corner_lat_offset_p3_deg.is_some()),
        (31, rec.corner_lon_offset_p3_deg.is_some()),
        (32, rec.corner_lat_offset_p4_deg.is_some()),
        (33, rec.corner_lon_offset_p4_deg.is_some()),
        // Target
        (40, rec.target_location_lat_deg.is_some()),
        (41, rec.target_location_lon_deg.is_some()),
        (42, rec.target_location_elev_m.is_some()),
        (56, rec.platform_ground_speed.is_some()),
        // Identity
        (3, rec.mission_id.is_some()),
        (4, rec.platform_tail_number.is_some()),
        (10, rec.platform_designation.is_some()),
        (11, rec.image_source_sensor.is_some()),
        // Security
        (48, rec.security_local_set.is_some()),
    ];
    let mut out: std::collections::BTreeSet<u8> = std::iter::once(1u8).collect();
    out.extend(present.iter().filter(|(_, p)| *p).map(|(t, _)| *t));
    out
}

/// Map the `0..=1` walk parameter into `[min, max]`, stopping 5 % of the
/// span short of both ends.
///
/// This is the run-1 rule generalised: soak run 1 died 14.5 h in because
/// an unbounded latitude walk crossed Tag 13's +90 encode max. Every
/// numeric tag the rich generator sets goes through here, so no tag can
/// ever reach its own encode limit however long the run goes — and the
/// 5 % margin means a later rounding or unit tweak has room to be wrong
/// without becoming a mid-soak panic.
fn walk(w: f64, min: f64, max: f64) -> f64 {
    let margin = (max - min) * 0.05;
    min + margin + w * (max - min - 2.0 * margin)
}

/// Build the rich ST 0601 record for `(seed, seq)` — see [`KlvSet::Rich`]
/// and [`rich_presence`]. Returns raw KLV LS bytes on the same
/// "caller passes unwrapped LS bytes" contract as [`klv_record`].
///
/// # Errors
/// Returns the encoder's message (which names the offending tag) rather
/// than panicking. Every numeric field is walked through `walk` and so
/// should be structurally incapable of going out of range; surfacing a
/// failure as `Err` instead of an `expect` is what keeps a generator bug
/// from killing a 72 h soak the way run 1's did.
pub fn klv_record_rich(seed: u64, seq: u32) -> Result<Vec<u8>, String> {
    let tags = rich_presence(seed, seq);
    let has = |t: u8| tags.contains(&t);
    // The same bounded triangle the compact record walks, normalised to
    // 0..=1 so one parameter drives every tag: all values sweep their
    // ranges together and turn around together, which keeps the record
    // self-consistent (a frame centre that tracks the sensor position)
    // instead of looking like independent noise per tag.
    let w = triangle_wave(seq) / 10_000.0;

    let mut rec = UasDatalinkLs {
        timestamp_us: Some(RICH_TS_BASE_US + u64::from(seq) * RICH_TS_STEP_US),
        platform_heading_deg: Some(walk(w, 0.0, 360.0)),
        sensor_lat_deg: Some(38.0 + w),
        sensor_lon_deg: Some(-121.5 - w),
        sensor_alt_m: Some(walk(w, -900.0, 19_000.0)),
        uas_ls_version: Some(19),
        ..Default::default()
    };

    if has(6) {
        rec.platform_pitch_deg = Some(walk(w, -20.0, 20.0));
        rec.platform_roll_deg = Some(walk(w, -50.0, 50.0));
        rec.sensor_rel_az_deg = Some(walk(w, 0.0, 360.0));
        rec.sensor_rel_el_deg = Some(walk(w, -180.0, 180.0));
        rec.sensor_rel_roll_deg = Some(walk(w, 0.0, 360.0));
    }
    if has(16) {
        rec.sensor_hfov_deg = Some(walk(w, 0.0, 180.0));
        rec.sensor_vfov_deg = Some(walk(w, 0.0, 180.0));
        rec.slant_range_m = Some(walk(w, 0.0, 5_000_000.0));
        rec.target_width_m = Some(walk(w, 0.0, 10_000.0));
    }
    if has(23) {
        rec.frame_center_lat_deg = Some(38.0 + w);
        rec.frame_center_lon_deg = Some(-121.5 - w);
        rec.frame_center_elev_m = Some(walk(w, -900.0, 19_000.0));
        // The four corner offsets bracket the frame centre the way a
        // real footprint's do — (+,+) (+,-) (-,-) (-,+) walking the quad
        // — rather than all sharing one sign, which would collapse the
        // "footprint" onto a single diagonal.
        let d = walk(w, -0.075, 0.075);
        rec.corner_lat_offset_p1_deg = Some(d);
        rec.corner_lon_offset_p1_deg = Some(d);
        rec.corner_lat_offset_p2_deg = Some(d);
        rec.corner_lon_offset_p2_deg = Some(-d);
        rec.corner_lat_offset_p3_deg = Some(-d);
        rec.corner_lon_offset_p3_deg = Some(-d);
        rec.corner_lat_offset_p4_deg = Some(-d);
        rec.corner_lon_offset_p4_deg = Some(d);
    }
    if has(40) {
        rec.target_location_lat_deg = Some(38.0 + w);
        rec.target_location_lon_deg = Some(-121.5 - w);
        rec.target_location_elev_m = Some(walk(w, -900.0, 19_000.0));
        rec.platform_ground_speed = Some(walk(w, 0.0, 255.0));
    }
    if has(3) {
        // Every string is far inside ST 0601's 127-byte UTF-8 cap, and
        // the mission id cycles rather than growing with `seq`.
        rec.mission_id = Some(format!("MISSION-{:04}", seq % 10_000));
        rec.platform_tail_number = Some("N0TST".into());
        rec.platform_designation = Some("SYNTH-UAS".into());
        rec.image_source_sensor = Some("EO".into());
    }
    if has(48) {
        rec.security_local_set = Some(security_set(seq)?);
    }

    encode_to_vec(&rec).map_err(|e| format!("rich record seq {seq}: {e}"))
}

/// The nested ST 0102 security local set carried in ST 0601 Tag 48.
/// Field shapes (the `//US` classifying-country spelling, the plain
/// two-letter object country code that Tag 13 re-encodes as UTF-16, the
/// version number) are exactly those of
/// `klv::st0102::tests::round_trip_with_unknown_tag_preserved`, so the
/// set decodes with no `field_errors` and passes ST 0102 strict
/// compliance (tags 1, 2, 3, 12, 13 and 22 are all present).
fn security_set(seq: u32) -> Result<Vec<u8>, String> {
    use tst_core::klv::st0102::{
        ClassifyingCountryCodingMethod, ObjectCountryCodingMethod, SecurityClassification,
        SecurityLs,
    };
    let sec = SecurityLs {
        security_classification: Some(SecurityClassification::Unclassified),
        classifying_country_coding_method: Some(ClassifyingCountryCodingMethod::Iso3166TwoLetter),
        classifying_country: Some("//US".into()),
        caveats: Some("SYNTHETIC".into()),
        releasing_instructions: Some("NONE".into()),
        object_country_coding_method: Some(ObjectCountryCodingMethod::Iso3166TwoLetter),
        object_country_codes: Some("US".into()),
        version: Some(12),
        ..Default::default()
    };
    tst_core::klv::st0102::encode_to_vec(&sec)
        .map_err(|e| format!("rich record seq {seq}: nested ST 0102 set: {e}"))
}

/// Dispatch on the configured [`KlvSet`]: `Compact` reproduces
/// [`klv_record`] byte for byte (ignoring `seed` — the compact record has
/// no seeded component), `Rich` builds [`klv_record_rich`].
///
/// # Errors
/// Propagates [`klv_record_rich`]'s encode failure. `Compact` is
/// infallible but shares the signature so callers have one call site.
pub fn klv_record_for(set: KlvSet, seed: u64, seq: u32) -> Result<Vec<u8>, String> {
    match set {
        KlvSet::Compact => Ok(klv_record(seq)),
        KlvSet::Rich => klv_record_rich(seed, seq),
    }
}

/// Recover the sender's `seq` from a rich record's decoded
/// `timestamp_us`. `None` if the stamp predates [`RICH_TS_BASE_US`], is
/// off the [`RICH_TS_STEP_US`] grid, or is further out than a `u32` of
/// records — any of which means the record did not come from
/// [`klv_record_rich`].
#[must_use]
pub fn rich_seq_of_timestamp(ts_us: u64) -> Option<u32> {
    let offset = ts_us.checked_sub(RICH_TS_BASE_US)?;
    if offset % RICH_TS_STEP_US != 0 {
        return None;
    }
    u32::try_from(offset / RICH_TS_STEP_US).ok()
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
    fn rich_presence_is_deterministic_and_core_is_always_present() {
        for seq in [0u32, 1, 49, 50, 999, 2_591_999] {
            let a = rich_presence(9, seq);
            assert_eq!(a, rich_presence(9, seq));
            for t in RICH_CORE_TAGS {
                assert!(a.contains(&t), "seq {seq} missing core tag {t}");
            }
        }
        assert_ne!(
            rich_presence(9, 7),
            rich_presence(10, 7),
            "seed changes the schedule"
        );
    }

    #[test]
    fn rich_presence_has_core_only_records_and_every_group_appears() {
        let mut core_only = 0;
        let mut seen: std::collections::BTreeSet<RichGroup> = Default::default();
        for seq in 0..5_000u32 {
            let p = rich_presence(1, seq);
            if p.iter().all(|t| RICH_CORE_TAGS.contains(t)) {
                core_only += 1;
            }
            for g in RichGroup::ALL {
                if rich_group_tags(g).iter().all(|t| p.contains(t)) {
                    seen.insert(g);
                }
            }
        }
        assert!(
            (50..=150).contains(&core_only),
            "~1 in 50 core-only, got {core_only}/5000"
        );
        assert_eq!(seen.len(), 6);
    }

    #[test]
    fn rich_record_decodes_with_the_declared_tag_set_and_a_nested_security_set() {
        use tst_core::klv::{st0102, st0601};
        for seq in [0u32, 3, 50, 777] {
            let bytes = klv_record_rich(1, seq).unwrap();
            let rec = st0601::decode(&bytes).unwrap();
            assert!(rec.field_errors.is_empty(), "{:?}", rec.field_errors);
            let expect = rich_presence(1, seq);
            // A rich record carries a materially larger tag set than the
            // compact one: measured 128..=212 bytes across seeds against
            // compact's 50 (a core-only record is 54, hence the gate).
            // Stated against the compact record rather than a magic floor
            // so the claim survives either generator's field set moving;
            // the ceiling catches runaway growth, e.g. a string field
            // that starts scaling with `seq`.
            assert!(bytes.len() <= 400, "seq {seq}: {} bytes", bytes.len());
            if expect.len() > RICH_CORE_TAGS.len() {
                assert!(
                    bytes.len() > 2 * klv_record(seq).len(),
                    "seq {seq}: rich {} bytes vs compact {}",
                    bytes.len(),
                    klv_record(seq).len()
                );
            }
            // Presence check on a representative field per group + core.
            assert_eq!(rec.platform_pitch_deg.is_some(), expect.contains(&6));
            assert_eq!(rec.sensor_hfov_deg.is_some(), expect.contains(&16));
            assert_eq!(rec.corner_lat_offset_p1_deg.is_some(), expect.contains(&26));
            assert_eq!(rec.target_location_lat_deg.is_some(), expect.contains(&40));
            assert_eq!(rec.mission_id.is_some(), expect.contains(&3));
            assert_eq!(rec.security_local_set.is_some(), expect.contains(&48));
            assert_eq!(rich_seq_of_timestamp(rec.timestamp_us.unwrap()), Some(seq));
            if let Some(sec) = &rec.security_local_set {
                let s = st0102::decode(sec).unwrap();
                assert!(s.field_errors.is_empty());
                assert!(s.security_classification.is_some());
            }
        }
    }

    /// The census oracle's exact identity (Task 10 consumes both halves):
    /// the tags a rich record actually carries on the wire are precisely
    /// the tags its seeded presence schedule declared, with no drift
    /// between the two tag lists.
    #[test]
    fn observed_tags_of_a_rich_record_equal_its_declared_presence() {
        for seed in [1u64, 77] {
            for seq in [0u32, 1, 3, 50, 777, 4_999, 2_591_999] {
                let rec = tst_core::klv::st0601::decode(&klv_record_rich(seed, seq).unwrap())
                    .expect("rich record must decode");
                assert_eq!(
                    observed_tags(&rec),
                    rich_presence(seed, seq),
                    "seed {seed} seq {seq}"
                );
            }
        }
    }

    /// The run-1 rule applied to every rich tag: a 72h soak at 10 Hz needs
    /// 2_592_000 records; sweep every triangle extreme plus a prime stride
    /// plus 10_000 seeded random seqs, for two seeds. Any encode error is
    /// a generator bug (out-of-range walk), caught here, never in a soak.
    #[test]
    fn rich_record_encodes_across_a_full_72h_soak_seq_range() {
        let mut rng = XorShift64::new(4242);
        for seed in [1u64, 77] {
            for seq in (0..3_000_000u32).step_by(997) {
                klv_record_rich(seed, seq).unwrap_or_else(|e| panic!("seed {seed} seq {seq}: {e}"));
            }
            for seq in [0u32, 10_000, 20_000, 520_001, 2_591_999] {
                klv_record_rich(seed, seq).unwrap();
            }
            for _ in 0..10_000 {
                let seq = (rng.next_u64() % 3_000_000) as u32;
                klv_record_rich(seed, seq).unwrap();
            }
        }
    }

    /// The run-1 rule has to bite at the walk's *turnaround* points —
    /// `w == 0` (seq 0) and `w == 1` (seq 10_000) — where a range bug
    /// hides. Group presence is seeded, so sweeping a single seed at
    /// those seqs silently leaves every absent group unchecked: the
    /// sweep above can pass while a group that never happened to be
    /// scheduled at an extreme carries an out-of-range walk. Sweep
    /// seeds instead, and assert every group was genuinely exercised at
    /// both extremes before believing the encodes proved anything.
    #[test]
    fn every_rich_group_encodes_at_both_triangle_extremes() {
        for seq in [0u32, 10_000] {
            let mut covered: std::collections::BTreeSet<RichGroup> = Default::default();
            for seed in 0..200u64 {
                klv_record_rich(seed, seq).unwrap_or_else(|e| panic!("seed {seed} seq {seq}: {e}"));
                let p = rich_presence(seed, seq);
                for g in RichGroup::ALL {
                    if rich_group_tags(g).iter().all(|t| p.contains(t)) {
                        covered.insert(g);
                    }
                }
            }
            assert_eq!(
                covered.len(),
                RichGroup::ALL.len(),
                "seq {seq}: only {covered:?} were exercised at this extreme"
            );
        }
    }

    #[test]
    fn compact_dispatch_is_byte_identical_to_klv_record() {
        for seq in [0u32, 1, 520_001] {
            assert_eq!(
                klv_record_for(KlvSet::Compact, 99, seq).unwrap(),
                klv_record(seq)
            );
        }
    }

    #[test]
    fn klv_set_parses_its_wire_names_and_rejects_anything_else() {
        assert_eq!(KlvSet::parse("compact"), Some(KlvSet::Compact));
        assert_eq!(KlvSet::parse("rich"), Some(KlvSet::Rich));
        assert_eq!(KlvSet::parse("Rich"), None);
        assert_eq!(KlvSet::parse(""), None);
    }

    #[test]
    fn rich_seq_of_timestamp_rejects_off_grid_and_pre_base_stamps() {
        assert_eq!(rich_seq_of_timestamp(RICH_TS_BASE_US), Some(0));
        assert_eq!(
            rich_seq_of_timestamp(RICH_TS_BASE_US + 7 * RICH_TS_STEP_US),
            Some(7)
        );
        assert_eq!(rich_seq_of_timestamp(RICH_TS_BASE_US - 1), None);
        assert_eq!(rich_seq_of_timestamp(RICH_TS_BASE_US + 1), None);
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
