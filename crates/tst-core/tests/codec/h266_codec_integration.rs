//! End-to-end: synthetic H.266 NALs -> mux -> demux -> codec::h266 parse.
//!
//! Composes three layers shipped earlier in this phase:
//!   1. `mpegts::mux` carries an Annex-B H.266 access unit at PMT
//!      stream_type 0x33.
//!   2. `mpegts::demux` recovers NALs and tags them
//!      `NalUnit::H266 { .. }` under `VideoCodec::H266`.
//!   3. `codec::h266::parse_parameter_sets` decodes the recovered
//!      parameter-set NALs into `H266ParameterSets`.
//!
//! The minimal VPS/SPS/PPS bodies match the per-set unit tests at
//! `codec/h266/{vps,sps,pps}.rs`. The SPS bit-builder is replicated
//! inline (see `BitWriter` below) so this integration test has zero
//! coupling to internal `pub(crate)` helpers.

use tst_core::codec::h266::{H266ParameterSets, parse_parameter_sets};
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::demux::Demuxer;
use tst_core::mpegts::demux::event::{
    DemuxEvent, NalUnit, SamplePayload, VideoCodec, VideoPayload,
};
use tst_core::mpegts::demux::split_video;
use tst_core::mpegts::mux::{
    Av1CarriageMode, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};

/// Wrap a NAL body in an Annex-B start code + 2-byte H.266 NAL header.
/// Per H.266 V4 §7.3.1.2:
///   byte 0: forbidden_zero_bit(1)=0 | nuh_reserved_zero_bit(1)=0 | nuh_layer_id(6)=0
///   byte 1: nal_unit_type(5) | nuh_temporal_id_plus1(3)=1
fn h266_nal(nal_type: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![0x00, 0x00, 0x00, 0x01, 0x00, (nal_type << 3) | 0x01];
    v.extend_from_slice(body);
    v
}

/// Minimal VPS RBSP: vps_id=0, max_layers=1, max_sub_layers=1.
/// Matches `codec::h266::vps::tests` minimal fixture.
fn minimal_vps_rbsp() -> Vec<u8> {
    vec![0x00, 0x02]
}

/// Minimal PPS RBSP: pps_id=0, sps_id=0.
/// Matches `codec::h266::pps::tests` minimal fixture.
fn minimal_pps_rbsp() -> Vec<u8> {
    vec![0x00, 0x20]
}

/// Inline bit-builder, copied verbatim from
/// `codec::h266::sps::tests::BitWriter`. Kept here so this test does
/// not depend on `pub(crate)` internals.
struct BitWriter {
    bytes: Vec<u8>,
    pos: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            pos: 0,
        }
    }
    fn write(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            let bit = ((value >> i) & 1) as u8;
            let byte_idx = (self.pos / 8) as usize;
            let bit_in_byte = 7 - (self.pos % 8);
            if byte_idx == self.bytes.len() {
                self.bytes.push(0);
            }
            self.bytes[byte_idx] |= bit << bit_in_byte;
            self.pos += 1;
        }
    }
    /// Exp-Golomb ue(v) per H.266 §9.3.2.2.
    fn write_ue(&mut self, value: u32) {
        let v = value + 1;
        let leading_zeros = 31 - v.leading_zeros();
        for _ in 0..leading_zeros {
            self.write(0, 1);
        }
        self.write(v, leading_zeros + 1);
    }
    /// rbsp_trailing_bits(): one '1' bit + zero-pad to byte align.
    fn end_rbsp(&mut self) {
        self.write(1, 1);
        while self.pos % 8 != 0 {
            self.write(0, 1);
        }
    }
}

/// Construct a minimal valid H.266 SPS bitstream:
/// sps_id=0, vps_id=0, 320x240, 8-bit 4:2:0, Main 10 profile @ Level 4.0.
/// Mirror of `codec::h266::sps::tests::minimal_sps_rbsp`.
///
/// Writes all body-walk fields through
/// sps_vui_parameters_present_flag=0, matching the updated
/// minimal_sps_rbsp_full() in sps.rs.
fn minimal_sps_rbsp() -> Vec<u8> {
    let mut bw = BitWriter::new();

    // §7.3.2.4 SPS header.
    bw.write(0, 4); // sps_seq_parameter_set_id
    bw.write(0, 4); // sps_video_parameter_set_id
    bw.write(0, 3); // sps_max_sublayers_minus1
    bw.write(1, 2); // sps_chroma_format_idc = 1 (4:2:0)
    bw.write(0, 2); // sps_log2_ctu_size_minus5
    bw.write(1, 1); // sps_ptl_dpb_hrd_params_present_flag = 1

    // §7.3.3.1 profile_tier_level(profileTierPresentFlag=1, MaxNumSubLayersMinus1=0).
    bw.write(1, 7); // general_profile_idc = 1 (Main 10)
    bw.write(0, 1); // general_tier_flag = 0 (Main tier)
    bw.write(63, 8); // general_level_idc = 63 (Level 4.0)
    bw.write(0, 1); // ptl_frame_only_constraint_flag
    bw.write(0, 1); // ptl_multilayer_enabled_flag
    bw.write(0, 1); // gci_present_flag = 0
    bw.write(0, 5); // byte-align PTL to 24 bits
    bw.write(0, 8); // ptl_num_sub_profiles = 0

    bw.write(0, 1); // sps_gdr_enabled_flag
    bw.write(0, 1); // sps_ref_pic_resampling_enabled_flag = 0

    bw.write_ue(320); // sps_pic_width_max_in_luma_samples
    bw.write_ue(240); // sps_pic_height_max_in_luma_samples

    bw.write(0, 1); // sps_conformance_window_flag = 0
    bw.write(0, 1); // sps_subpic_info_present_flag = 0
    bw.write_ue(0); // sps_bitdepth_minus8 = 0 -> bit_depth = 8

    // §7.3.2.4 body walk fields (all flags off, ue values = 0).
    bw.write(0, 1); // sps_entropy_coding_sync_enabled_flag
    bw.write(0, 1); // sps_entry_point_offsets_present_flag
    bw.write(0, 4); // sps_log2_max_pic_order_cnt_lsb_minus4 u(4)
    bw.write(0, 1); // sps_poc_msb_cycle_flag
    bw.write(0, 2); // sps_num_extra_ph_bytes u(2)
    bw.write(0, 2); // sps_num_extra_sh_bytes u(2)
    // ptl_dpb_hrd_present=1, max_sublayers=0 → no sublayer_dpb_params_flag
    bw.write_ue(1); // dpb_max_dec_pic_buffering_minus1[0]
    bw.write_ue(0); // dpb_max_num_reorder_pics[0]
    bw.write_ue(0); // dpb_max_latency_increase_plus1[0]
    bw.write_ue(0); // sps_log2_min_luma_coding_block_size_minus2
    bw.write(0, 1); // sps_partition_constraints_override_enabled_flag
    bw.write_ue(0); // sps_log2_diff_min_qt_min_cb_intra_slice_luma
    bw.write_ue(0); // sps_max_mtt_hierarchy_depth_intra_slice_luma
    bw.write(0, 1); // sps_qtbtt_dual_tree_intra_flag (chroma_format!=0)
    bw.write_ue(0); // sps_log2_diff_min_qt_min_cb_inter_slice
    bw.write_ue(0); // sps_max_mtt_hierarchy_depth_inter_slice
    // CTBsize=32 → no max_luma_transform_size_64_flag
    bw.write(0, 1); // sps_transform_skip_enabled_flag
    bw.write(0, 1); // sps_mts_enabled_flag
    bw.write(0, 1); // sps_lfnst_enabled_flag
    bw.write(0, 1); // sps_joint_cbcr_enabled_flag
    bw.write(1, 1); // sps_same_qp_table_for_chroma_flag = 1 → 1 QP table
    bw.write_ue(0); // qp_table_start_minus26[0] = se(0) → ue(0)
    bw.write_ue(0); // num_points_in_qp_table_minus1[0]
    bw.write_ue(0); // delta_qp_in_val_minus1[0][0]
    bw.write_ue(0); // delta_qp_diff_val[0][0]
    bw.write(0, 1); // sps_sao_enabled_flag
    bw.write(0, 1); // sps_alf_enabled_flag
    bw.write(0, 1); // sps_lmcs_enabled_flag
    bw.write(0, 1); // sps_weighted_pred_flag
    bw.write(0, 1); // sps_weighted_bipred_flag
    bw.write(0, 1); // sps_long_term_ref_pics_flag
    // vps_id=0 → no inter_layer_pred_enabled_flag
    bw.write(0, 1); // sps_idr_rpl_present_flag
    bw.write(0, 1); // sps_rpl1_same_as_rpl0_flag
    bw.write_ue(0); // sps_num_ref_pic_lists[0]
    bw.write_ue(0); // sps_num_ref_pic_lists[1]
    bw.write(0, 1); // sps_ref_wraparound_enabled_flag
    bw.write(0, 1); // sps_temporal_mvp_enabled_flag
    bw.write(0, 1); // sps_amvr_enabled_flag
    bw.write(0, 1); // sps_bdof_enabled_flag
    bw.write(0, 1); // sps_smvd_enabled_flag
    bw.write(0, 1); // sps_dmvr_enabled_flag
    bw.write(0, 1); // sps_mmvd_enabled_flag
    bw.write_ue(0); // sps_six_minus_max_num_merge_cand
    bw.write(0, 1); // sps_sbt_enabled_flag
    bw.write(0, 1); // sps_affine_enabled_flag
    bw.write(0, 1); // sps_bcw_enabled_flag
    bw.write(0, 1); // sps_ciip_enabled_flag
    bw.write(0, 1); // sps_gpm_enabled_flag (MaxNumMergeCand=6 >= 2)
    bw.write_ue(0); // sps_log2_parallel_merge_level_minus2
    bw.write(0, 1); // sps_isp_enabled_flag
    bw.write(0, 1); // sps_mrl_enabled_flag
    bw.write(0, 1); // sps_mip_enabled_flag
    bw.write(0, 1); // sps_cclm_enabled_flag (chroma_format!=0)
    bw.write(0, 1); // sps_chroma_horizontal_collocated_flag (chroma==1)
    bw.write(0, 1); // sps_chroma_vertical_collocated_flag
    bw.write(0, 1); // sps_palette_enabled_flag
    bw.write(0, 1); // sps_ibc_enabled_flag
    bw.write(0, 1); // sps_ladf_enabled_flag
    bw.write(0, 1); // sps_explicit_scaling_list_enabled_flag
    bw.write(0, 1); // sps_dep_quant_enabled_flag
    bw.write(0, 1); // sps_sign_data_hiding_enabled_flag
    bw.write(0, 1); // sps_virtual_boundaries_enabled_flag
    bw.write(0, 1); // sps_timing_hrd_params_present_flag (ptl_dpb_hrd=1)
    bw.write(0, 1); // sps_field_seq_flag
    bw.write(0, 1); // sps_vui_parameters_present_flag
    bw.write(0, 1); // sps_extension_flag

    bw.end_rbsp();
    bw.bytes
}

fn drain_mux(mux: &mut Muxer) -> Vec<u8> {
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

fn collect_events(d: &mut Demuxer) -> Vec<DemuxEvent> {
    let mut out = Vec::new();
    while let Some(e) = d.next_event() {
        out.push(e);
    }
    out
}

#[test]
fn h266_end_to_end_parses_minimal_vps_sps_pps() {
    let cfg = {
        let mut prog = MuxerProgramConfigBuilder::new(1, 0x100);
        prog.add_video(0x101, MuxVideoCodec::H266);
        let mut b = MuxerConfig::builder();
        b.add_program(prog.build());
        b.build().unwrap()
    };
    let mut mux = Muxer::new(cfg).unwrap();
    let h = mux.video_handles()[0];

    // Build an AU with valid minimal VPS/SPS/PPS from the per-task tests.
    let mut au = Vec::new();
    au.extend(h266_nal(20, &[0x10])); // AUD_NUT
    au.extend(h266_nal(14, &minimal_vps_rbsp())); // VPS_NUT
    au.extend(h266_nal(15, &minimal_sps_rbsp())); // SPS_NUT
    au.extend(h266_nal(16, &minimal_pps_rbsp())); // PPS_NUT

    mux.push_video_to(h, &au, Pts90khz::new(90_000), true)
        .unwrap();
    let ts_bytes = drain_mux(&mut mux);

    let mut demux = Demuxer::new();
    demux.feed(&ts_bytes).unwrap();
    // Unbounded video PES (PES_packet_length=0) buffers in-flight; flush
    // drains it. Live receive loops do this on TransportError::Closed.
    demux.flush();
    let events = collect_events(&mut demux);

    let nals: Vec<NalUnit> = events
        .iter()
        .filter_map(|e| match e {
            DemuxEvent::Sample {
                payload:
                    SamplePayload::Video {
                        codec: VideoCodec::H266,
                        raw,
                        ..
                    },
                ..
            } => match split_video(raw, VideoCodec::H266, Av1CarriageMode::Mpeg2TsBinding).0 {
                VideoPayload::Nals(nals) => Some(nals),
                _ => None,
            },
            _ => None,
        })
        .flatten()
        .collect();
    assert!(
        !nals.is_empty(),
        "expected H.266 NALs from demuxer, got events: {events:?}"
    );

    let sets: H266ParameterSets =
        parse_parameter_sets(&nals).expect("parse_parameter_sets should succeed");
    assert_eq!(sets.vpses.len(), 1);
    assert_eq!(sets.spses.len(), 1);
    assert_eq!(sets.ppses.len(), 1);

    // Spot-check parsed values match the inputs.
    assert_eq!(sets.vpses[0].vps_id, 0);
    assert_eq!(sets.spses[0].sps_id, 0);
    assert_eq!(sets.spses[0].vps_id, 0);
    assert_eq!(sets.spses[0].width, 320);
    assert_eq!(sets.spses[0].height, 240);
    assert_eq!(sets.ppses[0].pps_id, 0);
}
