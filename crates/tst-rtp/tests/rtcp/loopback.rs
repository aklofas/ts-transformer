//! Validate the experimental RTCP SR/RR reporter's default + opt-in
//! behavior.
//!
//! The outgoing SR/RR reporter is **off by default** (it emits
//! placeholder zero statistics and is not RFC 3550-conformant). These
//! tests assert that default and that the opt-in path still spawns the
//! companion socket. RTCP *reception* (ingest) is covered by the unit
//! tests in `transport.rs` and is unaffected.

use tst_core::transport::Transport;

#[test]
fn no_rr_sent_by_default() {
    // Receiver opens on a known port. With the reporter off by default,
    // no companion RTCP socket is bound and no RR is ever emitted — so
    // port+1 stays free and the sent-counter stays 0. Deterministic: no
    // sleep needed (the old default-on test had to sleep > 7.5 s for the
    // randomized RTCP interval).
    let base: u16 = 5100;
    let recv = tst_rtp::RtpRecvTransport::listen(&format!("rtp://127.0.0.1:{base}")).unwrap();
    let probe = std::net::UdpSocket::bind(("127.0.0.1", base + 1));
    assert!(
        probe.is_ok(),
        "RTCP companion port {} must stay free when the reporter is off by default",
        base + 1
    );
    let stats = recv.rtcp_stats();
    assert_eq!(stats.rr_packets_sent, 0);
    assert_eq!(stats.rr_packets_received, 0);
    drop(recv);
}

#[test]
fn reporter_opt_in_binds_companion_socket() {
    // Explicit opt-in still works: the companion RTCP socket is bound on
    // port+1 (so binding it ourselves fails with AddrInUse). We don't wait
    // for an actual RR emission here (the randomized RTCP interval is
    // 2.5-7.5 s); the socket bind is the deterministic proof the reporter
    // path is wired.
    let base: u16 = 5110;
    let recv = tst_rtp::RtpRecvSocketBuilder::from_url(&format!("rtp://127.0.0.1:{base}"))
        .unwrap()
        .rtcp(true)
        .build()
        .unwrap();
    match std::net::UdpSocket::bind(("127.0.0.1", base + 1)) {
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {}
        other => panic!(
            "RTCP port {} should be held by the opted-in receiver (AddrInUse); got {other:?}",
            base + 1
        ),
    }
    drop(recv);
}

#[test]
fn rtt_zero_before_any_rtcp_exchange() {
    // The send-side `socket_stats().rtt_us` is 0 until RTCP reception
    // populates it (reception is unaffected by the reporter default).
    let sender = tst_rtp::RtpTransport::connect("rtp://127.0.0.1:5300").unwrap();
    let stats = sender.socket_stats().expect("alive transport");
    assert_eq!(stats.rtt_us, 0);
    drop(sender);
}
