//! MPEG-TS built with the muxer from the H.264 GOP fixture, each access
//! unit preceded by a KLV record at its PTS. Socket-free, so both test
//! binaries include it; each uses a subset.
#![allow(dead_code)]

use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{
    KlvStreamType, Muxer, MuxerConfig, MuxerProgramConfigBuilder, VideoCodec as MuxVideoCodec,
};

/// The 30-frame GOP: IDR first, then 29 P frames, each opened by an AUD.
const GOP: &[u8] = include_bytes!("h264/gop.h264");

/// The PTS of the first access unit.
pub const FIRST_PTS: i64 = 90_000;

/// The PTS step between access units (30 frames per second).
pub const TICKS_PER_FRAME: i64 = 3000;

/// How far the second program's PTS runs ahead of the first's, so its
/// frames are told apart by their timestamps.
pub const SECOND_PROGRAM_OFFSET: i64 = 1500;

/// The first `n` access units of the GOP, split at each access unit
/// delimiter (NAL type 9).
pub fn access_units(n: usize) -> Vec<Vec<u8>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 4 < GOP.len() {
        let sc = if GOP[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if GOP[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        if GOP[i + sc] & 0x1F == 9 {
            starts.push(i);
        }
        i += sc;
    }
    starts.push(GOP.len());
    starts
        .windows(2)
        .take(n)
        .map(|w| GOP[w[0]..w[1]].to_vec())
        .collect()
}

/// The KLV record bound to access unit `i` of the first program.
pub fn record(i: usize) -> Vec<u8> {
    format!("klv-record-{i:03}").into_bytes()
}

/// The KLV record bound to access unit `i` of the second program.
pub fn second_program_record(i: usize) -> Vec<u8> {
    format!("other-record-{i:03}").into_bytes()
}

/// A single-program TS (video PID 0x1011, KLV PID 0x1031) of `n` access
/// units.
pub fn build_ts(n: usize) -> Vec<u8> {
    mux_programs(n, 1)
}

/// A TS of two programs, each one H.264 stream and one KLV stream of `n`
/// access units: program 1 as [`build_ts`], program 2 on PIDs 0x1111 and
/// 0x1131 with [`second_program_record`]s, its PTS
/// [`SECOND_PROGRAM_OFFSET`] ahead.
pub fn build_two_program_ts(n: usize) -> Vec<u8> {
    mux_programs(n, 2)
}

fn mux_programs(n: usize, programs: u16) -> Vec<u8> {
    let mut cfg = MuxerConfig::builder();
    for k in 0..programs {
        let base = 0x100 * k;
        let mut prog = MuxerProgramConfigBuilder::new(k + 1, 0x1000 + base);
        prog.add_video(0x1011 + base, MuxVideoCodec::H264);
        prog.add_klv(0x1031 + base, KlvStreamType::SynchronousMetadata, true);
        cfg.add_program(prog.build());
    }
    let mut mux = Muxer::new(cfg.build().unwrap()).unwrap();
    let (video, klv) = (mux.video_handles(), mux.klv_handles());
    let mut out = Vec::new();
    let mut buf = vec![0u8; 188 * 64];
    for (i, au) in access_units(n).iter().enumerate() {
        for k in 0..usize::from(programs) {
            let (offset, rec) = if k == 0 {
                (0, record(i))
            } else {
                (SECOND_PROGRAM_OFFSET, second_program_record(i))
            };
            let pts = Pts90khz::new(FIRST_PTS + offset + i as i64 * TICKS_PER_FRAME);
            mux.push_klv_to(klv[k], &rec, pts, 0).unwrap();
            mux.push_video_to(video[k], au, pts, i == 0).unwrap();
        }
        loop {
            let got = mux.pull(&mut buf);
            if got == 0 {
                break;
            }
            out.extend_from_slice(&buf[..got]);
        }
    }
    out
}
