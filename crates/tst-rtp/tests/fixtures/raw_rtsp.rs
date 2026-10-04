//! Raw-socket RTSP client helpers for server tests that must do what
//! `RtspClient` will not: leave without TEARDOWN, skip keepalives, hold a
//! connection idle. Blocking std sockets; every read is bounded by the
//! caller's `set_read_timeout`.

use std::io::{Read, Write};
use std::net::TcpStream;
use tst_core::mpegts::mux::{MuxerConfig, MuxerProgramConfigBuilder, VideoCodec};

/// One H.264 program on PID 0x1011 — the mount config every server test uses.
pub fn make_muxer_cfg() -> MuxerConfig {
    let mut prog = MuxerProgramConfigBuilder::new(1, 0x1000);
    prog.add_video(0x1011, VideoCodec::H264);
    let mut b = MuxerConfig::builder();
    b.add_program(prog.build());
    b.build().unwrap()
}

/// Write one request, read until the end of the response head (CRLFCRLF).
/// Panics with the server's behaviour in the message if it closes first.
pub fn request(tcp: &mut TcpStream, req: &str) -> String {
    tcp.write_all(req.as_bytes()).expect("request written");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = tcp
            .read(&mut chunk)
            .expect("server answered within the read timeout");
        assert!(
            n > 0,
            "server closed the connection before answering {req:?}"
        );
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// The value of header `name` (case-insensitive), trimmed.
pub fn header<'a>(response: &'a str, name: &str) -> &'a str {
    response
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
        })
        .unwrap_or_else(|| panic!("no {name} header in {response:?}"))
}

/// `Session:` value without its `;timeout=` parameter.
pub fn session_id(response: &str) -> String {
    header(response, "Session")
        .split(';')
        .next()
        .unwrap()
        .trim()
        .to_owned()
}
