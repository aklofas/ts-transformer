//! Reed-Solomon FEC config applies. Doesn't assert recovery quality.

use std::time::Duration;
use tst_srt::{PacketFilter, SrtListenerBuilder, SrtSocketBuilder};

#[test]
fn fec_config_applies() {
    require_loopback!();
    let pf_listener = PacketFilter::new("fec,cols:10,rows:5,arq:onreq").unwrap();
    let pf_caller = PacketFilter::new("fec,cols:10,rows:5,arq:onreq").unwrap();

    let mut builder = SrtListenerBuilder::new();
    builder
        .packet_filter(pf_listener)
        .recv_timeout(Duration::from_secs(5));
    let lb = crate::common::Loopback::bind_with(builder);
    let port = lb.port;

    let accept = lb.spawn_accept(|sock| {
        drop(sock);
    });
    accept.wait_ready();

    let _socket = SrtSocketBuilder::new()
        .packet_filter(pf_caller)
        .recv_timeout(Duration::from_secs(5))
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");

    accept.join();
}
