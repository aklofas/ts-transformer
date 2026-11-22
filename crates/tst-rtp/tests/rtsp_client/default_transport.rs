//! The transport a client asks for in its first SETUP when nothing says
//! otherwise: UDP first on `rtsp://`, TCP-interleaved on `rtsps://`, where a
//! UDP media path would leave the TLS tunnel.

#![cfg(feature = "tls")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::fixtures::raw_rtsp_server::SDP_BODY;
use tst_rtp::{PushDescription, RtspClientBuilder, RtspError, RtspPush};

/// A peer that answers every SETUP with 461 and hands back each SETUP's
/// `Transport` header, in order, when the connection ends. TLS when `tls`
/// carries a server config.
fn scripted_peer(tls: Option<Arc<rustls::ServerConfig>>) -> (u16, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let peer = std::thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        match tls {
            Some(config) => {
                let conn = rustls::ServerConnection::new(config).unwrap();
                serve(rustls::StreamOwned::new(conn, sock))
            }
            None => serve(sock),
        }
    });
    (port, peer)
}

fn serve<S: Read + Write>(mut stream: S) -> Vec<String> {
    let mut transports = Vec::new();
    let mut buf = Vec::new();
    loop {
        let end = loop {
            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break p + 4;
            }
            let mut chunk = [0u8; 2048];
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return transports,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let req = String::from_utf8_lossy(&buf[..end]).into_owned();
        buf.drain(..end);
        let header = |name: &str| {
            req.lines()
                .find_map(|l| {
                    let (key, value) = l.split_once(':')?;
                    key.eq_ignore_ascii_case(name)
                        .then(|| value.trim().to_string())
                })
                .unwrap_or_default()
        };
        // A push's ANNOUNCE carries its SDP as a body: skip it, so the next
        // request is read from its first line.
        let body = header("Content-Length").parse::<usize>().unwrap_or(0);
        while buf.len() < body {
            let mut chunk = [0u8; 2048];
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return transports,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        buf.drain(..body);
        let cseq = header("CSeq");
        let reply = match req.split(' ').next().unwrap_or("") {
            "DESCRIBE" => format!(
                "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{}",
                SDP_BODY.len(),
                String::from_utf8_lossy(SDP_BODY)
            ),
            "SETUP" => {
                transports.push(header("Transport"));
                format!("RTSP/1.0 461 Unsupported Transport\r\nCSeq: {cseq}\r\n\r\n")
            }
            _ => format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nPublic: DESCRIBE, SETUP\r\n\r\n"),
        };
        if stream.write_all(reply.as_bytes()).is_err() {
            return transports;
        }
    }
}

/// Pull the first SETUP's reply for `url_path_and_query` from a peer that
/// refuses it, and what the peer saw.
fn first_setups(
    scheme: &str,
    query: &str,
    builder: impl FnOnce(RtspClientBuilder) -> RtspClientBuilder,
) -> (Result<(), RtspError>, Vec<String>) {
    with_peer(scheme, query, |b| {
        let mut client = builder(b).connect().unwrap();
        let sdp = client.describe().unwrap();
        let result = client.setup_mp2t_auto(&sdp).map(|_| ());
        drop(client);
        result
    })
}

/// Push MPEG-TS to `url_path_and_query` on a peer that refuses every SETUP:
/// the push's outcome, and the `Transport` of each SETUP the peer saw.
fn push_setups(scheme: &str, query: &str) -> (Result<(), RtspError>, Vec<String>) {
    with_peer(scheme, query, |b| {
        RtspPush::connect(b, &PushDescription::Mp2t).map(|_| ())
    })
}

/// Run `client` against a fresh scripted peer at
/// `<scheme>://127.0.0.1:<port>/x<query>`; its result, and what the peer saw.
fn with_peer(
    scheme: &str,
    query: &str,
    client: impl FnOnce(RtspClientBuilder) -> Result<(), RtspError>,
) -> (Result<(), RtspError>, Vec<String>) {
    let certs = crate::fixtures::tls_certs::SelfSignedCert::generate();
    let tls = (scheme == "rtsps").then(|| {
        let key_pem = std::fs::read(&certs.key_path).unwrap();
        Arc::new(
            tst_core::net::tls::server_config_from_pem(certs.root_pem.as_bytes(), &key_pem)
                .unwrap(),
        )
    });
    let (port, peer) = scripted_peer(tls);
    let roots =
        tst_core::net::tls::load_root_store(certs.root_pem.as_bytes()).expect("trust bundle");
    let b = RtspClientBuilder::new(&format!("{scheme}://127.0.0.1:{port}/x{query}"))
        .unwrap()
        .no_auto_keepalive(true)
        .tls_root_certs(roots);
    let result = client(b);
    (result, peer.join().unwrap())
}

fn is_interleaved(transport: &str) -> bool {
    transport.starts_with("RTP/AVP/TCP") && transport.contains("interleaved=")
}

#[test]
fn an_rtsps_url_asks_for_interleaved_by_default() {
    let (_, setups) = first_setups("rtsps", "", |b| b);
    assert!(
        setups.first().is_some_and(|t| is_interleaved(t)),
        "{setups:?}"
    );
}

#[test]
fn an_rtsps_pull_does_not_fall_back_to_udp() {
    let (result, setups) = first_setups("rtsps", "", |b| b);
    assert!(
        matches!(result, Err(RtspError::Protocol { code: 461, .. })),
        "{result:?}"
    );
    assert_eq!(setups.len(), 1, "{setups:?}");
}

#[test]
fn an_rtsps_url_with_transport_udp_asks_for_udp() {
    let (_, setups) = first_setups("rtsps", "?transport=udp", |b| b);
    assert!(
        setups
            .first()
            .is_some_and(|t| t.starts_with("RTP/AVP;") && t.contains("client_port=")),
        "{setups:?}"
    );
}

#[test]
fn an_rtsps_client_told_to_prefer_udp_asks_for_udp_and_falls_back() {
    let (_, setups) = first_setups("rtsps", "", |b| {
        b.transport_preference(tst_rtp::RtspTransportPref::PreferUdp)
    });
    assert_eq!(setups.len(), 2, "{setups:?}");
    assert!(!is_interleaved(&setups[0]), "{setups:?}");
    assert!(is_interleaved(&setups[1]), "{setups:?}");
}

#[test]
fn an_rtsp_url_still_prefers_udp() {
    let (_, setups) = first_setups("rtsp", "", |b| b);
    assert_eq!(setups.len(), 2, "{setups:?}");
    assert!(!is_interleaved(&setups[0]), "{setups:?}");
    assert!(is_interleaved(&setups[1]), "{setups:?}");
}

#[test]
fn an_rtsps_url_with_transport_tcp_asks_for_interleaved_only() {
    let (result, setups) = first_setups("rtsps", "?transport=tcp", |b| b);
    assert!(
        matches!(result, Err(RtspError::Protocol { code: 461, .. })),
        "{result:?}"
    );
    assert_eq!(setups.len(), 1, "{setups:?}");
    assert!(is_interleaved(&setups[0]), "{setups:?}");
}

/// A push records over TCP-interleaved on either scheme; on `rtsps://` it
/// asks for nothing else and does not try UDP after a refusal.
#[test]
fn an_rtsps_push_asks_for_interleaved_record() {
    let (result, setups) = push_setups("rtsps", "");
    assert!(
        matches!(result, Err(RtspError::Protocol { code: 461, .. })),
        "{result:?}"
    );
    assert_eq!(setups.len(), 1, "{setups:?}");
    assert!(
        is_interleaved(&setups[0]) && setups[0].contains("mode=record"),
        "{setups:?}"
    );
}

/// Told to use UDP, a push is refused by the client itself: recording over
/// UDP is not supported, so no SETUP reaches the server.
#[test]
fn an_rtsps_push_told_to_use_udp_sends_no_setup() {
    let (result, setups) = push_setups("rtsps", "?transport=udp");
    assert!(
        matches!(result, Err(RtspError::UnsupportedTransport)),
        "{result:?}"
    );
    assert!(setups.is_empty(), "{setups:?}");
}
