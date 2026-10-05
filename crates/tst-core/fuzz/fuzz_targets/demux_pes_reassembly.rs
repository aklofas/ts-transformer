#![no_main]

use libfuzzer_sys::fuzz_target;
use tst_core::mpegts::demux::low_level::Reassembler;

// Direct fuzz of the PES reassembler. The first three bytes of each
// fuzz input are repurposed as steering: bytes 0-1 form a 13-bit PID
// (matching the on-the-wire PID width in a TS header), bit 0 of byte 2
// sets the payload_unit_start indicator, bit 1 sets the adaptation-
// field random_access_indicator, and bit 2 sets the is_video flag (the
// zero-PES_packet_length gate). The remainder is pushed as
// one packet's worth of payload bytes. Caps are sized generously
// (1 MiB per PID, 4 MiB total) so the reassembler exercises completion
// paths rather than just bouncing off cap-exceeded errors.
fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    let pid = u16::from_be_bytes([data[0] & 0x1F, data[1]]);
    let pusi = data[2] & 1 != 0;
    let rai = data[2] & 2 != 0;
    let is_video = data[2] & 4 != 0;
    let payload = &data[3..];
    let mut r = Reassembler::new(1 << 20, 4 << 20);
    let _ = r.push(pid, payload, pusi, rai, is_video);
});
