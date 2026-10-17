//! The old names this crate still accepts compile and name the new types.

#![allow(deprecated)]

#[test]
fn socket_config_is_tcp_config() {
    let _: tst_tcp::SocketConfig = tst_tcp::TcpConfig::default();
}
