//! Pipeline-shell integration over UDP: `MuxSender<UdpTransport>` → UDP →
//! `DemuxReceiver<UdpRecvTransport>` recovers the `ProgramMap`. The shared
//! body, and why it pushes several PSI-carrying frames, is in
//! `tst-pipeline/tests/support/round_trip.rs`.

#[path = "support/round_trip.rs"]
mod round_trip;

use tst_udp::{UdpRecvTransport, UdpTransport};

#[test]
fn mux_via_udp_demux_round_trip_recovers_program_map() {
    // Bind first so the kernel picks the port the sender dials.
    let recv = UdpRecvTransport::listen("udp://@127.0.0.1:0").expect("bind UdpRecvTransport");
    let url = format!("udp://127.0.0.1:{}", recv.local_addr().port());
    round_trip::assert_program_map_round_trip(
        move || recv,
        move || UdpTransport::connect(&url).expect("connect UdpTransport"),
        round_trip::LOOPBACK,
    );
}
