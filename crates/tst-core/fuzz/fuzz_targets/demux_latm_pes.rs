#![no_main]

//! Fuzz target — the AAC-LATM framing check (the LOAS resync) through the
//! demuxer. `demux_feed` reaches it only when the fuzzer invents a PAT and a
//! PMT with good CRCs that declare stream type 0x11; here a fixed PAT and PMT
//! declare PID 0x100 as AAC-LATM and the input is the payload of its PES.
//!
//! # Input layout
//!
//! ```text
//! [0]     selector byte
//!           bits [1:0] → StrictMode: 0=Off 1=TimingOnly 2=DescriptorsOnly 3=Full
//!           bit  [2]   → every PES sets data_alignment_indicator
//!           bit  [3]   → PES carry PES_packet_length (else unbounded)
//!           bit  [4]   → drop the second TS packet of each PES whose size
//!                        byte is odd (a continuity gap: the PES is damaged)
//!           bit  [5]   → `reset_sync` half way, then the PAT and PMT again
//! [1..9]  eight PES size bytes, used in turn: a byte below 128 is a PES of
//!         byte + 1 bytes, any other a PES of (byte - 127) * 128 bytes
//! [9..]   the PES payloads, back to back
//! ```
//!
//! Errors from `feed_aligned` (strict rejections) are normal outcomes and
//! are discarded. Only panics count as failures: the resync's bounds (no
//! more than its candidate limit held, no PES walking more record
//! boundaries than it has bytes) are debug assertions in the library.

use libfuzzer_sys::fuzz_target;
use tst_core::mpegts::demux::{Demuxer, DemuxerConfig, StrictMode};

const LATM_PID: u16 = 0x100;
const PMT_PID: u16 = 0x1000;

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// One TS packet: the header, then `payload` behind adaptation-field
/// stuffing when it is shorter than 184 bytes.
fn ts_packet(pid: u16, pusi: bool, cc: u8, payload: &[u8]) -> [u8; 188] {
    let mut p = [0xFFu8; 188];
    p[0] = 0x47;
    p[1] = (u8::from(pusi) << 6) | ((pid >> 8) as u8 & 0x1F);
    p[2] = pid as u8;
    let stuffing = 184 - payload.len();
    p[3] = if stuffing > 0 { 0x30 } else { 0x10 } | (cc & 0x0F);
    if stuffing > 0 {
        p[4] = (stuffing - 1) as u8;
        if stuffing > 1 {
            p[5] = 0x00;
        }
    }
    p[188 - payload.len()..].copy_from_slice(payload);
    p
}

/// A one-section PSI packet: pointer field, `section` and its CRC_32.
fn psi_packet(pid: u16, mut section: Vec<u8>) -> [u8; 188] {
    let len = section.len() - 3 + 4;
    section[1] = 0xB0 | ((len >> 8) as u8 & 0x0F);
    section[2] = len as u8;
    let crc = crc32(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    let mut payload = vec![0x00];
    payload.extend_from_slice(&section);
    payload.resize(184, 0xFF);
    ts_packet(pid, true, 0, &payload)
}

fn feed_psi(d: &mut Demuxer) {
    // PAT: program 1 on PMT_PID. PMT: PCR_PID 0x1FFE, one stream of type 0x11.
    let pat = vec![
        0x00,
        0,
        0,
        0x00,
        0x01,
        0xC1,
        0,
        0,
        0x00,
        0x01,
        0xE0 | (PMT_PID >> 8) as u8,
        PMT_PID as u8,
    ];
    let pmt = vec![
        0x02,
        0,
        0,
        0x00,
        0x01,
        0xC1,
        0,
        0,
        0xFF,
        0xFE,
        0xF0,
        0x00,
        0x11,
        0xE0 | (LATM_PID >> 8) as u8,
        LATM_PID as u8,
        0xF0,
        0x00,
    ];
    let _ = d.feed_aligned(&psi_packet(0x0000, pat));
    let _ = d.feed_aligned(&psi_packet(PMT_PID, pmt));
    while d.next_event().is_some() {}
}

/// The PES carrying `payload`, with a PTS.
fn pes(payload: &[u8], aligned: bool, bounded: bool, pts: u64) -> Vec<u8> {
    let len = 3 + 5 + payload.len();
    let len = if bounded && len <= 0xFFFF { len } else { 0 };
    let mut v = vec![0x00, 0x00, 0x01, 0xC0, (len >> 8) as u8, len as u8];
    v.push(0x80 | (u8::from(aligned) << 2));
    v.extend_from_slice(&[0x80, 5]);
    v.extend_from_slice(&[
        0x21 | ((pts >> 29) as u8 & 0x0E),
        (pts >> 22) as u8,
        ((pts >> 14) as u8 & 0xFE) | 1,
        (pts >> 7) as u8,
        ((pts << 1) as u8 & 0xFE) | 1,
    ]);
    v.extend_from_slice(payload);
    v
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 9 {
        return;
    }
    let selector = data[0];
    let sizes = &data[1..9];
    let mut payload = &data[9..];

    let strict = match selector & 0b11 {
        0 => StrictMode::Off,
        1 => StrictMode::TimingOnly,
        2 => StrictMode::DescriptorsOnly,
        _ => StrictMode::Full,
    };
    let aligned = selector & 0b100 != 0;
    let bounded = selector & 0b1000 != 0;
    let lose = selector & 0b1_0000 != 0;
    let reset_midway = selector & 0b10_0000 != 0;

    let mut builder = DemuxerConfig::builder();
    builder.strict(strict);
    let mut d = Demuxer::with_config(builder.build());
    feed_psi(&mut d);

    let half = payload.len() / 2;
    let mut fed = 0;
    let mut reset_done = false;
    let mut cc = 0u8;
    let mut i = 0;
    while !payload.is_empty() {
        let size_byte = sizes[i % sizes.len()];
        let size = if size_byte < 128 {
            usize::from(size_byte) + 1
        } else {
            usize::from(size_byte - 127) * 128
        };
        let (this, rest) = payload.split_at(size.min(payload.len()));
        payload = rest;
        let bytes = pes(this, aligned, bounded, i as u64 * 1_920);
        for (n, chunk) in bytes.chunks(184).enumerate() {
            let pkt = ts_packet(LATM_PID, n == 0, cc, chunk);
            cc = (cc + 1) & 0x0F;
            if lose && n == 1 && size_byte % 2 == 1 {
                continue;
            }
            let _ = d.feed_aligned(&pkt);
            while d.next_event().is_some() {}
        }
        fed += this.len();
        if reset_midway && !reset_done && fed >= half {
            reset_done = true;
            d.reset_sync();
            feed_psi(&mut d);
        }
        i += 1;
    }

    d.flush();
    while d.next_event().is_some() {}
});
