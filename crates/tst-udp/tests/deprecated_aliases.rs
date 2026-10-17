//! The old names this crate still accepts compile and name the new types.

#![allow(deprecated)]

#[test]
fn socket_config_is_udp_config() {
    let _: tst_udp::SocketConfig = tst_udp::UdpConfig::default();
}
