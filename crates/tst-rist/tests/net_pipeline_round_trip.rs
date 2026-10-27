//! Pipeline-shell integration over RIST: `MuxSender<RistTransport>` → RIST
//! loopback → `DemuxReceiver<RistRecvTransport>` recovers the `ProgramMap`
//! (shared body in `tst-pipeline/tests/support/round_trip.rs`; RIST uses
//! its `RIST` pacing for the librist handshake).
//!
//! Runs on Windows too: the librist ≤ 0.2.16 Windows teardown hang and
//! zero-delivery bug are fixed upstream in librist 0.2.18 (see `loopback.rs`).

#[path = "support/round_trip.rs"]
mod round_trip;

use std::net::UdpSocket;
use std::sync::Mutex;

use tst_rist::{RistRecvTransportBuilder, RistTransportBuilder};

/// Serialize RIST pipeline tests within the process — same rationale as
/// `tests/net_loopback.rs`. Ephemeral-port + librist-rebind race is brittle
/// when multiple RIST tests run in parallel.
static SERIAL: Mutex<()> = Mutex::new(());

fn find_free_udp_port() -> u16 {
    let s = UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = s.local_addr().expect("local_addr").port();
    drop(s);
    port
}

#[test]
fn mux_via_rist_demux_round_trip_recovers_program_map() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let port = find_free_udp_port();
    let connect_url = format!("rist://127.0.0.1:{port}");
    // Open the receiver first so the sender's first packet has a listener.
    let recv = RistRecvTransportBuilder::new(&format!("rist://@127.0.0.1:{port}"))
        .expect("RistRecvTransportBuilder::new")
        .listen()
        .expect("listen");
    round_trip::assert_program_map_round_trip(
        move || recv,
        move || {
            RistTransportBuilder::new(&connect_url)
                .expect("RistTransportBuilder::new")
                .connect()
                .expect("connect")
        },
        round_trip::RIST,
    );
}
