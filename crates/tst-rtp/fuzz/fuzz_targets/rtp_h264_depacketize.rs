#![no_main]

use libfuzzer_sys::fuzz_target;
use tst_rtp::{H264Depacketizer, H264DepayConfig, RtpHeader};

// Interpret the input as a packet sequence: [1 flag byte][2 seq][4 ts][1 len][len payload]…
// The flag byte's low bit is the marker, bit 1 selects one of two SSRCs (exercises the
// reset path). Successful and failed feeds are both fine; the harness asserts no panics.
fuzz_target!(|data: &[u8]| {
    let mut d = H264Depacketizer::new(H264DepayConfig::default());
    let mut rest = data;
    while rest.len() >= 8 {
        let flags = rest[0];
        let seq = u16::from_be_bytes([rest[1], rest[2]]);
        let ts = u32::from_be_bytes([rest[3], rest[4], rest[5], rest[6]]);
        let len = (rest[7] as usize).min(rest.len().saturating_sub(8));
        let mut h = RtpHeader::new(seq, ts, if flags & 0x02 != 0 { 1 } else { 2 });
        h.marker = flags & 0x01 != 0;
        h.payload_type = 96;
        d.feed(&h, &rest[8..8 + len]);
        while d.next_au().is_some() {}
        rest = &rest[8 + len..];
    }
    let _ = d.flush();
});
