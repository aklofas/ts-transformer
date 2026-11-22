//! The RTSP server options of the C builder reach the server: whether media
//! may travel over UDP (`tst_rtsp_server_builder_allow_udp_media`) and
//! whether a Digest answer without `qop` is taken
//! (`tst_rtsp_server_builder_accept_digest_without_qop`).
//!
//! The clients are raw std `TcpStream`s; every read is bounded by a socket
//! timeout, so a regression fails instead of hanging the suite.
#![cfg(feature = "rtp")]

use std::ffi::CString;
use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::time::Duration;

use md5::{Digest as _, Md5};

use tstrans::config::{
    TstVideoCodec, tst_mux_config_add_program, tst_mux_config_add_video_stream,
    tst_mux_config_free, tst_mux_config_new,
};
use tstrans::handle::TstRtspServerBuilder;
use tstrans::{
    TstRtspMountHandle, TstRtspServer, tst_rtsp_mount_handle_free,
    tst_rtsp_server_add_unicast_mount, tst_rtsp_server_builder_accept_digest_without_qop,
    tst_rtsp_server_builder_allow_udp_media, tst_rtsp_server_builder_auth_digest_md5,
    tst_rtsp_server_builder_new, tst_rtsp_server_builder_start, tst_rtsp_server_free,
    tst_rtsp_server_local_addr,
};

use crate::common::{last_error_msg, scaled};

/// A builder for a plain server on a kernel-picked loopback port.
fn builder() -> *mut TstRtspServerBuilder {
    let url = CString::new("rtsp://127.0.0.1:0").unwrap();
    let b = unsafe { tst_rtsp_server_builder_new(url.as_ptr()) };
    assert!(!b.is_null(), "{}", last_error_msg());
    b
}

/// Starts `b` with one H.264 unicast mount `/live`; returns the server, the
/// mount and the port.
fn start(b: *mut TstRtspServerBuilder) -> (*mut TstRtspServer, *mut TstRtspMountHandle, u16) {
    let server = unsafe { tst_rtsp_server_builder_start(b) };
    assert!(!server.is_null(), "{}", last_error_msg());
    let mut buf = [0 as std::os::raw::c_char; 64];
    let n = unsafe { tst_rtsp_server_local_addr(server, buf.as_mut_ptr(), buf.len()) };
    assert!(n > 0, "local_addr: {}", last_error_msg());
    let port = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_str()
        .unwrap()
        .parse::<std::net::SocketAddr>()
        .unwrap()
        .port();
    let cfg = unsafe { tst_mux_config_new() };
    let prog = unsafe { tst_mux_config_add_program(cfg, 1, 0x1000) };
    unsafe { tst_mux_config_add_video_stream(cfg, prog, 0x1011, TstVideoCodec::H264 as i32) };
    let path = CString::new("/live").unwrap();
    let mount = unsafe { tst_rtsp_server_add_unicast_mount(server, path.as_ptr(), cfg) };
    unsafe { tst_mux_config_free(cfg) };
    assert!(!mount.is_null(), "{}", last_error_msg());
    (server, mount, port)
}

fn stop(server: *mut TstRtspServer, mount: *mut TstRtspMountHandle) {
    unsafe { tst_rtsp_mount_handle_free(mount) };
    unsafe { tst_rtsp_server_free(server) };
}

fn connect(port: u16) -> TcpStream {
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect to the server");
    tcp.set_read_timeout(Some(scaled(Duration::from_secs(5))))
        .unwrap();
    tcp
}

/// Write one request and read the response head (up to CRLFCRLF).
fn request(tcp: &mut TcpStream, req: &str) -> String {
    tcp.write_all(req.as_bytes()).expect("request written");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tcp.read(&mut chunk).expect("server answered in time");
        assert!(n > 0, "server closed before answering {req:?}");
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn status(resp: &str) -> u16 {
    resp.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {resp:?}"))
}

fn header<'a>(resp: &'a str, name: &str) -> Option<&'a str> {
    resp.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// A reader SETUP of `/live` asking for `transport`, on a fresh connection:
/// (status, whether the reply names a `server_port`).
fn reader_setup(port: u16, transport: &str) -> (u16, bool) {
    let mut tcp = connect(port);
    let r = request(
        &mut tcp,
        &format!(
            "SETUP rtsp://127.0.0.1:{port}/live/trackID=0 RTSP/1.0\r\nCSeq: 1\r\nTransport: {transport}\r\n\r\n"
        ),
    );
    let server_port = header(&r, "Transport").is_some_and(|t| t.contains("server_port"));
    (status(&r), server_port)
}

/// A `client_port` pair nobody sends to: only its numbers go into the request.
fn udp_transport() -> String {
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    let cp = client.local_addr().unwrap().port();
    format!("RTP/AVP;unicast;client_port={cp}-{}", cp.saturating_add(1))
}

const INTERLEAVED: &str = "RTP/AVP/TCP;unicast;interleaved=0-1";

#[test]
fn udp_media_off_answers_461_to_a_udp_setup_and_200_to_an_interleaved_one() {
    let b = builder();
    unsafe { tst_rtsp_server_builder_allow_udp_media(b, false) };
    let (server, mount, port) = start(b);
    assert_eq!(reader_setup(port, &udp_transport()), (461, false));
    assert_eq!(reader_setup(port, INTERLEAVED).0, 200);
    stop(server, mount);
}

#[test]
fn a_plain_server_grants_udp_media_when_the_option_is_unset() {
    let (server, mount, port) = start(builder());
    assert_eq!(reader_setup(port, &udp_transport()), (200, true));
    stop(server, mount);
}

/// The last call wins, as for the other setters.
#[test]
fn allow_udp_media_after_a_refusal_allows_it_again() {
    let b = builder();
    unsafe { tst_rtsp_server_builder_allow_udp_media(b, false) };
    unsafe { tst_rtsp_server_builder_allow_udp_media(b, true) };
    let (server, mount, port) = start(b);
    assert_eq!(reader_setup(port, &udp_transport()), (200, true));
    stop(server, mount);
}

fn md5_hex(s: &str) -> String {
    Md5::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A DESCRIBE of `/live` on a server that asks for Digest MD5, answered the
/// way GStreamer 1.24 answers: the RFC 2069 form (no `qop`, `nc` or
/// `cnonce`). Returns the challenge and the status of the answered request.
fn rfc2069_describe(accept_without_qop: bool) -> (String, u16) {
    let b = builder();
    let (user, pass) = (
        CString::new("admin").unwrap(),
        CString::new("secret").unwrap(),
    );
    unsafe { tst_rtsp_server_builder_auth_digest_md5(b, user.as_ptr(), pass.as_ptr()) };
    unsafe { tst_rtsp_server_builder_accept_digest_without_qop(b, accept_without_qop) };
    let (server, mount, port) = start(b);
    let uri = format!("rtsp://127.0.0.1:{port}/live");
    let mut tcp = connect(port);
    let r = request(
        &mut tcp,
        &format!("DESCRIBE {uri} RTSP/1.0\r\nCSeq: 1\r\n\r\n"),
    );
    assert_eq!(status(&r), 401, "{r}");
    let challenge = header(&r, "WWW-Authenticate")
        .expect("a challenge")
        .to_owned();
    let field = |name: &str| {
        challenge
            .split(&format!("{name}=\""))
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_else(|| panic!("no {name} in {challenge}"))
            .to_owned()
    };
    let (realm, nonce) = (field("realm"), field("nonce"));
    let ha1 = md5_hex(&format!("admin:{realm}:secret"));
    let ha2 = md5_hex(&format!("DESCRIBE:{uri}"));
    let response = md5_hex(&format!("{ha1}:{nonce}:{ha2}"));
    let answered = request(
        &mut tcp,
        &format!(
            "DESCRIBE {uri} RTSP/1.0\r\nCSeq: 2\r\nAuthorization: Digest username=\"admin\", \
             realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\"\r\n\r\n"
        ),
    );
    stop(server, mount);
    (challenge, status(&answered))
}

#[test]
fn a_digest_answer_without_qop_is_accepted_with_the_option() {
    let (challenge, answered) = rfc2069_describe(true);
    assert!(challenge.contains(r#"qop="auth""#), "{challenge}");
    assert_eq!(answered, 200);
}

#[test]
fn a_digest_answer_without_qop_is_refused_without_the_option() {
    let (challenge, answered) = rfc2069_describe(false);
    assert!(challenge.contains(r#"qop="auth""#), "{challenge}");
    assert_eq!(answered, 401);
}
