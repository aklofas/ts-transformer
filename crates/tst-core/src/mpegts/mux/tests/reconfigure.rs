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
