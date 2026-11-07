//! PSI version state and in-place reconfiguration.

use super::psi::psi_version_byte;
use super::*;
use crate::mpegts::common::Pts90khz;

fn two_program_config() -> MuxerConfig {
    let mut prog1 = MuxerProgramConfig::new(1, 0x1000);
    prog1.streams = vec![
        StreamSpec::Video {
            pid: 0x1011,
            codec: VideoCodec::H264,
        },
        StreamSpec::Klv {
            pid: 0x1031,
            stream_type: KlvStreamType::PrivateData,
            carries_pts: false,
        },
    ];
    prog1.stream_descriptors = vec![Vec::new(), Vec::new()];
    let mut prog2 = MuxerProgramConfig::new(2, 0x1100);
    prog2.streams = vec![
        StreamSpec::Video {
            pid: 0x1111,
            codec: VideoCodec::H265,
        },
        StreamSpec::Klv {
            pid: 0x1131,
            stream_type: KlvStreamType::PrivateData,
            carries_pts: false,
        },
    ];
    prog2.stream_descriptors = vec![Vec::new(), Vec::new()];
    MuxerConfig {
        programs: vec![prog1, prog2],
        ..MuxerConfig::default()
    }
}

fn drain(m: &mut Muxer) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 188 * 64];
    loop {
        let n = m.pull(&mut buf);
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n]);
    }
}

#[test]
fn psi_version_byte_encodes_0_and_31() {
    assert_eq!(psi_version_byte(0), 0xC1);
    assert_eq!(psi_version_byte(31), 0xFF);
}

#[test]
fn fresh_muxer_emits_pat_and_pmt_version_0() {
    let mut m = Muxer::new(two_program_config()).unwrap();
    let nal = [0u8, 0, 0, 1, 0x65, 0x88, 0x84, 0x00];
    let h = m.video_stream_handle(0).unwrap();
    m.push_video_to(h, &nal, Pts90khz::new(0), true).unwrap();
    let bytes = drain(&mut m);
    let mut seen = Vec::new();
    for pkt in bytes.chunks(188) {
        let pid = (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2]);
        if matches!(pid, 0 | 0x1000 | 0x1100) {
            assert_eq!((pkt[10] >> 1) & 0x1F, 0, "pid {pid:#x}");
            seen.push(pid);
        }
    }
    assert_eq!(seen, vec![0, 0x1000, 0x1100]);
}

// ── plan_reconfigure ──────────────────────────────────────────────────────

/// Program 1 (PMT 0x1000) = H.264 0x1011 + sync KLV 0x1031 (service id 0).
fn visible() -> MuxerProgramConfigBuilder {
    let mut p = MuxerProgramConfigBuilder::new(1, 0x1000);
    p.add_video(0x1011, VideoCodec::H264);
    p.add_klv(0x1031, KlvStreamType::SynchronousMetadata, true);
    p
}

/// Program 2 (PMT 0x1100) = H.264 0x1111 + sync KLV 0x1131 (service id 1).
fn thermal() -> MuxerProgramConfigBuilder {
    let mut p = MuxerProgramConfigBuilder::new(2, 0x1100);
    p.add_video(0x1111, VideoCodec::H264);
    p.add_klv(0x1131, KlvStreamType::SynchronousMetadata, true);
    p.stream_descriptors_for_klv(0, vec![crate::mpegts::descriptors::metadata_klva(1)])
        .unwrap();
    p
}

/// Assembled without `build()` so a test can hand the planner a config
/// that `validate()` refuses.
fn config_of(programs: Vec<MuxerProgramConfig>) -> MuxerConfig {
    MuxerConfig {
        programs,
        ..MuxerConfig::default()
    }
}

fn visible_thermal(with_thermal: bool) -> MuxerConfig {
    let mut programs = vec![visible().build()];
    if with_thermal {
        programs.push(thermal().build());
    }
    config_of(programs)
}

/// Tombstone slot `idx` the way a reconfigure that removed its program does.
fn tombstone(m: &mut Muxer, idx: usize) {
    let slot = &mut m.slots[idx];
    slot.live = None;
    slot.video.iter_mut().for_each(|s| *s = None);
    slot.klv.iter_mut().for_each(|s| *s = None);
    slot.audio.iter_mut().for_each(|s| *s = None);
    slot.subtitle.iter_mut().for_each(|s| *s = None);
    slot.data.iter_mut().for_each(|s| *s = None);
}

fn kinds_withins(p: &reconfigure::PlannedProgram) -> Vec<(MuxStreamKind, usize, usize, bool)> {
    p.streams
        .iter()
        .map(|s| (s.kind, s.within, s.spec_index, s.survivor))
        .collect()
}

fn refusal(m: &Muxer, new: &MuxerConfig) -> alloc::string::String {
    match m.plan_reconfigure(new) {
        Err(MuxError::ConfigInvalid { reason }) => {
            assert!(reason.starts_with("reconfigure:"), "{reason}");
            reason
        }
        Err(other) => panic!("expected ConfigInvalid, got {other:?}"),
        Ok(_) => panic!("expected ConfigInvalid, got a plan"),
    }
}

#[test]
fn plan_drop_program_tombstones_its_slot() {
    let m = Muxer::new(visible_thermal(true)).unwrap();
    let plan = m.plan_reconfigure(&visible_thermal(false)).unwrap();
    assert_eq!(plan.removed_slots, vec![1]);
    assert_eq!(plan.programs.len(), 1);
    let p = &plan.programs[0];
    assert_eq!((p.slot, p.config_index, p.pcr_pid), (0, 0, 0x1011));
    assert!(p.removed_pids.is_empty());
    assert_eq!(
        kinds_withins(p),
        vec![
            (MuxStreamKind::Video, 0, 0, true),
            (MuxStreamKind::Klv, 0, 1, true)
        ]
    );
}

#[test]
fn plan_add_program_takes_a_fresh_slot() {
    let m = Muxer::new(visible_thermal(false)).unwrap();
    let plan = m.plan_reconfigure(&visible_thermal(true)).unwrap();
    assert!(plan.removed_slots.is_empty());
    let p = &plan.programs[1];
    assert_eq!((p.slot, p.config_index, p.pcr_pid), (1, 1, 0x1111));
    assert!(p.streams.iter().all(|s| !s.survivor));
    assert_eq!(
        kinds_withins(p),
        vec![
            (MuxStreamKind::Video, 0, 0, false),
            (MuxStreamKind::Klv, 0, 1, false)
        ]
    );
}

#[test]
fn plan_readd_same_number_reuses_its_tombstone() {
    let mut m = Muxer::new(visible_thermal(true)).unwrap();
    tombstone(&mut m, 1);
    let plan = m.plan_reconfigure(&visible_thermal(true)).unwrap();
    assert!(plan.removed_slots.is_empty());
    let p = &plan.programs[1];
    assert_eq!(p.slot, 1);
    assert_eq!(
        kinds_withins(p),
        vec![
            (MuxStreamKind::Video, 0, 0, false),
            (MuxStreamKind::Klv, 0, 1, false)
        ]
    );
}

#[test]
fn plan_readd_prefers_its_own_tombstone_over_a_lower_one() {
    let numbered = |n: u16| {
        let mut p = MuxerProgramConfigBuilder::new(n, 0x0100 + n * 0x10);
        p.add_video(0x0101 + n * 0x10, VideoCodec::H264);
        p.build()
    };
    let mut m = Muxer::new(config_of(vec![numbered(1), numbered(2), numbered(3)])).unwrap();
    tombstone(&mut m, 1);
    tombstone(&mut m, 2);
    let plan = m
        .plan_reconfigure(&config_of(vec![numbered(1), numbered(3)]))
        .unwrap();
    assert_eq!(plan.programs[1].slot, 2);
}

#[test]
fn plan_new_number_reuses_any_tombstone_when_full() {
    let numbered = |n: u16| {
        let mut p = MuxerProgramConfigBuilder::new(n, 0x0100 + n * 0x10);
        p.add_video(0x0101 + n * 0x10, VideoCodec::H264);
        p.build()
    };
    let m = Muxer::new(config_of((1..=16).map(numbered).collect())).unwrap();
    let mut programs: Vec<_> = (1..=15).map(numbered).collect();
    programs.push(numbered(17));
    let plan = m.plan_reconfigure(&config_of(programs)).unwrap();
    assert_eq!(plan.removed_slots, vec![15]);
    let p = &plan.programs[15];
    assert_eq!((p.slot, p.config_index), (15, 15));
    assert_eq!(kinds_withins(p), vec![(MuxStreamKind::Video, 0, 0, false)]);
}

#[test]
fn plan_stream_added_to_surviving_program() {
    let m = Muxer::new(visible_thermal(true)).unwrap();
    let mut p1 = visible();
    p1.add_audio(0x1041, crate::codec::AudioCodec::Aac);
    let plan = m
        .plan_reconfigure(&config_of(vec![p1.build(), thermal().build()]))
        .unwrap();
    assert_eq!(
        kinds_withins(&plan.programs[0]),
        vec![
            (MuxStreamKind::Video, 0, 0, true),
            (MuxStreamKind::Klv, 0, 1, true),
            (MuxStreamKind::Audio, 0, 2, false)
        ]
    );
    assert!(plan.programs[1].streams.iter().all(|s| s.survivor));
}

#[test]
fn plan_stream_removed_from_surviving_program() {
    let m = Muxer::new(visible_thermal(true)).unwrap();
    let mut p1 = MuxerProgramConfigBuilder::new(1, 0x1000);
    p1.add_video(0x1011, VideoCodec::H264);
    let plan = m
        .plan_reconfigure(&config_of(vec![p1.build(), thermal().build()]))
        .unwrap();
    assert_eq!(plan.programs[0].removed_pids, vec![0x1031]);
}

#[test]
fn refuses_pid_moved_to_another_program() {
    let m = Muxer::new(visible_thermal(true)).unwrap();
    let mut p1 = MuxerProgramConfigBuilder::new(1, 0x1000);
    p1.add_video(0x1011, VideoCodec::H264);
    let mut p2 = thermal();
    p2.add_klv(0x1031, KlvStreamType::PrivateData, false);
    let reason = refusal(&m, &config_of(vec![p1.build(), p2.build()]));
    assert!(reason.contains("0x1031"), "{reason}");
    assert!(
        reason.contains("program 1") && reason.contains("program 2"),
        "{reason}"
    );
}

#[test]
fn refuses_pid_redeclared_under_a_new_program_number() {
    let m = Muxer::new(visible_thermal(true)).unwrap();
    let mut p3 = MuxerProgramConfigBuilder::new(3, 0x1100);
    p3.add_video(0x1111, VideoCodec::H264);
    let reason = refusal(&m, &config_of(vec![visible().build(), p3.build()]));
    assert!(reason.contains("0x1111"), "{reason}");
    assert!(
        reason.contains("program 2") && reason.contains("program 3"),
        "{reason}"
    );
}

#[test]
fn refuses_stream_type_change_on_surviving_pid() {
    let m = Muxer::new(visible_thermal(true)).unwrap();
    let mut p1 = MuxerProgramConfigBuilder::new(1, 0x1000);
    p1.add_video(0x1011, VideoCodec::H264);
    p1.add_klv(0x1031, KlvStreamType::PrivateData, false);
    let reason = refusal(&m, &config_of(vec![p1.build(), thermal().build()]));
    assert!(
        reason.contains("0x1031") && reason.contains("program 1"),
        "{reason}"
    );
}

#[test]
fn refuses_codec_change_on_surviving_pid() {
    let m = Muxer::new(visible_thermal(true)).unwrap();
    let mut p1 = MuxerProgramConfigBuilder::new(1, 0x1000);
    p1.add_video(0x1011, VideoCodec::H265);
    p1.add_klv(0x1031, KlvStreamType::SynchronousMetadata, true);
    let reason = refusal(&m, &config_of(vec![p1.build(), thermal().build()]));
    assert!(
        reason.contains("0x1011") && reason.contains("program 1"),
        "{reason}"
    );
}

#[test]
fn refuses_pmt_pid_change() {
    let m = Muxer::new(visible_thermal(true)).unwrap();
    let mut p1 = MuxerProgramConfigBuilder::new(1, 0x1001);
    p1.add_video(0x1011, VideoCodec::H264);
    p1.add_klv(0x1031, KlvStreamType::SynchronousMetadata, true);
    let reason = refusal(&m, &config_of(vec![p1.build(), thermal().build()]));
    assert!(
        reason.contains("0x1001") && reason.contains("program 1"),
        "{reason}"
    );
}

#[test]
fn refuses_pcr_pid_change_unless_old_pcr_removed() {
    // KLV cannot carry PCR (validate() refuses it), so the new PCR stream is
    // an added audio PID.
    let m = Muxer::new(visible_thermal(true)).unwrap();
    let mut p1 = visible();
    p1.add_audio(0x1041, crate::codec::AudioCodec::Aac);
    p1.pcr_pid(0x1041);
    let reason = refusal(&m, &config_of(vec![p1.build(), thermal().build()]));
    assert!(
        reason.contains("0x1011") && reason.contains("0x1041") && reason.contains("program 1"),
        "{reason}"
    );
    assert!(!reason.contains("default rule"), "{reason}");

    // With no pcr_pid set, a video declared ahead of 0x1011 takes the PCR by
    // the default rule; the reason names the fix.
    let mut p1 = MuxerProgramConfigBuilder::new(1, 0x1000);
    p1.add_video(0x1012, VideoCodec::H264);
    p1.add_video(0x1011, VideoCodec::H264);
    p1.add_klv(0x1031, KlvStreamType::SynchronousMetadata, true);
    let reason = refusal(&m, &config_of(vec![p1.build(), thermal().build()]));
    assert!(
        reason.starts_with("reconfigure:")
            && reason.contains("default rule")
            && reason.contains("pcr_pid to 0x1011"),
        "{reason}"
    );

    let mut p1 = MuxerProgramConfigBuilder::new(1, 0x1000);
    p1.add_klv(0x1031, KlvStreamType::SynchronousMetadata, true);
    p1.add_audio(0x1041, crate::codec::AudioCodec::Aac);
    p1.pcr_pid(0x1041);
    let plan = m
        .plan_reconfigure(&config_of(vec![p1.build(), thermal().build()]))
        .unwrap();
    assert_eq!(plan.programs[0].pcr_pid, 0x1041);
    assert_eq!(plan.programs[0].removed_pids, vec![0x1011]);
}

#[test]
fn refuses_non_program_field_change() {
    let m = Muxer::new(visible_thermal(true)).unwrap();
    let mut new = visible_thermal(true);
    new.psi_interval_ms += 1;
    let reason = refusal(&m, &new);
    assert!(reason.contains("psi_interval_ms"), "{reason}");
}

#[test]
fn refuses_duplicate_metadata_service_id() {
    let m = Muxer::new(visible_thermal(true)).unwrap();
    let mut p1 = visible();
    p1.stream_descriptors_for_klv(0, vec![crate::mpegts::descriptors::metadata_klva(1)])
        .unwrap();
    let new = config_of(vec![p1.build(), thermal().build()]);
    let expected = new.validate().unwrap_err();
    let got = m.plan_reconfigure(&new).err().expect("refused");
    assert_eq!(format!("{got:?}"), format!("{expected:?}"));
}

/// Program 1 (PMT 0x1000) = H.264 0x1011 + the given private-data KLV PIDs.
fn video_and_klvs(klv_pids: &[u16]) -> MuxerProgramConfig {
    let mut p = MuxerProgramConfigBuilder::new(1, 0x1000);
    p.add_video(0x1011, VideoCodec::H264);
    for &pid in klv_pids {
        p.add_klv(pid, KlvStreamType::PrivateData, false);
    }
    p.build()
}

/// Tombstone one stream of a live slot the way a reconfigure that dropped
/// that PID does: its stream entry becomes `None` and it leaves the program.
fn tombstone_stream(m: &mut Muxer, idx: usize, pid: u16) {
    let slot = &mut m.slots[idx];
    let (kind, within) = slot.placed_pids[&pid];
    match kind {
        MuxStreamKind::Video => slot.video[within] = None,
        MuxStreamKind::Klv => slot.klv[within] = None,
        MuxStreamKind::Audio => slot.audio[within] = None,
        MuxStreamKind::Subtitle => slot.subtitle[within] = None,
        MuxStreamKind::Data => slot.data[within] = None,
    }
    let live = slot.live.as_mut().unwrap();
    let pos = live.streams.iter().position(|s| s.pid() == pid).unwrap();
    live.streams.remove(pos);
    live.stream_descriptors.remove(pos);
}

#[test]
fn plan_returning_stream_reuses_its_own_tombstone() {
    // A@0 and B@1 tombstoned; B alone returns and takes 1, not the lower 0.
    let mut m = Muxer::new(config_of(vec![video_and_klvs(&[0x1031, 0x1032])])).unwrap();
    tombstone_stream(&mut m, 0, 0x1031);
    tombstone_stream(&mut m, 0, 0x1032);
    let plan = m
        .plan_reconfigure(&config_of(vec![video_and_klvs(&[0x1032])]))
        .unwrap();
    assert_eq!(
        kinds_withins(&plan.programs[0]),
        vec![
            (MuxStreamKind::Video, 0, 0, true),
            (MuxStreamKind::Klv, 1, 1, false)
        ]
    );
}

#[test]
fn plan_new_stream_reuses_any_tombstone_of_its_kind() {
    // A@0 tombstoned, B@1 survives; new C takes A's index instead of appending.
    let mut m = Muxer::new(config_of(vec![video_and_klvs(&[0x1031, 0x1032])])).unwrap();
    tombstone_stream(&mut m, 0, 0x1031);
    let plan = m
        .plan_reconfigure(&config_of(vec![video_and_klvs(&[0x1032, 0x1033])]))
        .unwrap();
    assert_eq!(
        kinds_withins(&plan.programs[0]),
        vec![
            (MuxStreamKind::Video, 0, 0, true),
            (MuxStreamKind::Klv, 1, 1, true),
            (MuxStreamKind::Klv, 0, 2, false)
        ]
    );
}

#[test]
fn plan_new_stream_declared_first_leaves_a_returning_stream_its_tombstone() {
    // A@0 and B@1 tombstoned; new C is declared before returning A, yet A
    // gets 0 back and C takes the remaining tombstone 1.
    let mut m = Muxer::new(config_of(vec![video_and_klvs(&[0x1031, 0x1032])])).unwrap();
    tombstone_stream(&mut m, 0, 0x1031);
    tombstone_stream(&mut m, 0, 0x1032);
    let plan = m
        .plan_reconfigure(&config_of(vec![video_and_klvs(&[0x1033, 0x1031])]))
        .unwrap();
    assert_eq!(
        kinds_withins(&plan.programs[0]),
        vec![
            (MuxStreamKind::Video, 0, 0, true),
            (MuxStreamKind::Klv, 1, 1, false),
            (MuxStreamKind::Klv, 0, 2, false)
        ]
    );
}
