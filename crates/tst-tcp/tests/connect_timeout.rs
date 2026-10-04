//! Review 9 (int R9-06): a `tcps://` connect that ran out `connect_timeout`
//! was `TcpError::Io` (TCP_IO, -30) while `tcp://` reported
//! `ConnectTimeout` (TCP_CONNECT_TIMEOUT, -32). Linux-only: the
//! deterministic timeout needs a listener whose accept queue is full
//! (`listen(0)` + unaccepted clients) so the next SYN is dropped
//! (`net.ipv4.tcp_abort_on_overflow = 0`, the default) and the caller's
//! connect runs out its timeout instead of being refused.
#![cfg(all(feature = "tls", target_os = "linux"))]

use std::net::TcpStream;
use std::time::Duration;
use tst_tcp::TcpTransport;
use tst_tcp::error::TcpError;

/// A listening socket that never accepts, with its backlog already full.
/// The fillers are kept alive so the queue stays full.
fn full_backlog_port() -> (socket2::Socket, Vec<TcpStream>, u16) {
    use socket2::{Domain, Socket, Type};
    let l = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    l.bind(&addr.into()).unwrap();
    l.listen(0).unwrap();
    let port = l.local_addr().unwrap().as_socket().unwrap().port();
    let fillers = (0..4)
        .filter_map(|_| {
            TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(300))
                .ok()
        })
        .collect();
    (l, fillers, port)
}

/// Any valid PEM: the connect fails before a handshake, but `connect_tls`
/// loads the trust root before dialling, so the file must parse. The
/// returned `NamedTempFile` (under `std::env::temp_dir()`) removes the file
/// on drop, so a failing assertion does not leak it.
fn ca_pem_file() -> tempfile::NamedTempFile {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let file = tempfile::Builder::new()
        .prefix("tst-tcp-r9-06-ca-")
        .suffix(".pem")
        .tempfile()
        .unwrap();
    std::fs::write(file.path(), cert.cert.pem()).unwrap();
    file
}

#[test]
fn a_timed_out_connect_is_connect_timeout_on_both_schemes() {
    let (_listener, _fillers, port) = full_backlog_port();
    let ca = ca_pem_file();
    for scheme in ["tcp", "tcps"] {
        let url = format!(
            "{scheme}://127.0.0.1:{port}?connect_timeout=1&ca={}",
            ca.path().display()
        );
        // `TcpTransport` has no `Debug`, so the panic text reports the
        // error side only.
        match TcpTransport::connect(&url) {
            Err(TcpError::ConnectTimeout { seconds: 1 }) => {}
            Err(other) => {
                panic!("{scheme}: expected ConnectTimeout {{ seconds: 1 }}, got {other:?}")
            }
            Ok(_) => panic!("{scheme}: expected ConnectTimeout {{ seconds: 1 }}, got Ok"),
        }
    }
}
