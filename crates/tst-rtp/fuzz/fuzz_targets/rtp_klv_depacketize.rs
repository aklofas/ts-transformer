#![no_main]

use libfuzzer_sys::fuzz_target;
use tst_rtp::RtpHeader;
use tst_rtp::rtsp::server::publish::KlvDepacketizer;

// Interpret the input as a packet sequence: [1 flag byte][2 seq][4 ts][2 len][payload]…
// The flag byte's low bit is the marker, bit 1 selects one of two SSRCs (exercises the
// reset path), and bit 2 repeats the payload bytes present up to the declared length, so
// a small input can still build a unit past the depacketizer's ~64 KiB oversize cap.
// Successful and failed feeds are both fine; the harness asserts no panics.
fuzz_target!(|data: &[u8]| {
    let mut d = KlvDepacketizer::new();
    let mut rest = data;
    while rest.len() >= 9 {
        let flags = rest[0];
        let seq = u16::from_be_bytes([rest[1], rest[2]]);
        let ts = u32::from_be_bytes([rest[3], rest[4], rest[5], rest[6]]);
        let declared = u16::from_be_bytes([rest[7], rest[8]]) as usize;
        let len = declared.min(rest.len() - 9);
        let present = &rest[9..9 + len];
        let mut h = RtpHeader::new(seq, ts, if flags & 0x02 != 0 { 1 } else { 2 });
        h.marker = flags & 0x01 != 0;
        h.payload_type = 97;
        if flags & 0x04 != 0 && !present.is_empty() {
            let repeated: Vec<u8> = present.iter().copied().cycle().take(declared).collect();
            d.feed(&h, &repeated);
        } else {
            d.feed(&h, present);
        }
        while d.next_unit().is_some() {}
        rest = &rest[9 + len..];
    }
    let _ = d.flush();
});
