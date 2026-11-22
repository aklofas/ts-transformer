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
use tstrans::error::{TstError, tst_get_last_error};
use tstrans::handle::TstRtspServerBuilder;
use tstrans::{
    TstRtspMountHandle, TstRtspServer, tst_rtsp_client_builder_connect,
    tst_rtsp_client_builder_new, tst_rtsp_mount_handle_free, tst_rtsp_server_add_unicast_mount,
    tst_rtsp_server_builder_accept_digest_without_qop, tst_rtsp_server_builder_allow_udp_media,
    tst_rtsp_server_builder_auth_digest_md5, tst_rtsp_server_builder_max_readers,
    tst_rtsp_server_builder_new, tst_rtsp_server_builder_start, tst_rtsp_server_free,
    tst_rtsp_server_local_addr, tst_rtsp_session_play, tst_rtsp_session_teardown_and_free,
};

use crate::common::{last_error_msg, scaled};
use crate::common_net::{request, status};

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

/// A reader SETUP of `/live` over TCP-interleaved on a fresh connection:
/// the status, and whether the server then closed the connection (a read
/// that ends, is reset or aborted, rather than one that times out on an
/// open connection). The connection is returned so a reader that got its
/// slot keeps it.
fn interleaved_reader(port: u16) -> (u16, bool, TcpStream) {
    let mut tcp = connect(port);
    let r = request(
        &mut tcp,
        &format!(
            "SETUP rtsp://127.0.0.1:{port}/live/trackID=0 RTSP/1.0\r\nCSeq: 1\r\nTransport: {INTERLEAVED}\r\n\r\n"
        ),
    );
    // Long enough for the close to arrive on a loaded machine; a reader
    // under the cap waits it out once.
    tcp.set_read_timeout(Some(scaled(Duration::from_secs(1))))
        .unwrap();
    let mut byte = [0u8; 1];
    let closed = match tcp.read(&mut byte) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
        ),
    };
    (status(&r), closed, tcp)
}

#[test]
fn max_readers_answers_453_to_the_reader_over_the_cap_and_closes_its_connection() {
    let b = builder();
    unsafe { tst_rtsp_server_builder_max_readers(b, 1) };
    let (server, mount, port) = start(b);
    let (first, first_closed, _held) = interleaved_reader(port);
    assert_eq!(
        (first, first_closed),
        (200, false),
        "the reader under the cap"
    );
    let (second, second_closed, _) = interleaved_reader(port);
    assert_eq!(
        (second, second_closed),
        (453, true),
        "the reader over the cap"
    );
    stop(server, mount);
}

#[test]
fn max_readers_zero_refuses_every_reader() {
    let b = builder();
    unsafe { tst_rtsp_server_builder_max_readers(b, 0) };
    let (server, mount, port) = start(b);
    let (status, closed, _) = interleaved_reader(port);
    assert_eq!((status, closed), (453, true));
    stop(server, mount);
}

/// Through the C client: the reader under the cap connects and plays, the
/// one over it is refused at connect (its SETUP reads 453).
#[test]
fn a_c_client_over_the_reader_cap_is_refused_and_one_under_it_plays() {
    let b = builder();
    unsafe { tst_rtsp_server_builder_max_readers(b, 1) };
    let (server, mount, port) = start(b);
    let url = CString::new(format!("rtsp://127.0.0.1:{port}/live")).unwrap();
    let pull = || {
        let cb = unsafe { tst_rtsp_client_builder_new(url.as_ptr()) };
        assert!(!cb.is_null(), "{}", last_error_msg());
        unsafe { tst_rtsp_client_builder_connect(cb) }
    };
    let first = pull();
    assert!(!first.is_null(), "{}", last_error_msg());
    assert_eq!(
        unsafe { tst_rtsp_session_play(first) },
        0,
        "{}",
        last_error_msg()
    );
    let second = pull();
    assert!(second.is_null(), "a second reader over a cap of 1");
    let msg = last_error_msg();
    assert_eq!(
        unsafe { tst_get_last_error() },
        TstError::RtspProtocol as i32,
        "{msg}"
    );
    assert!(msg.contains("453"), "{msg}");
    unsafe { tst_rtsp_session_teardown_and_free(first) };
    stop(server, mount);
}

/// The same option on an `rtsps://` server, where leaving it uncalled means
/// NO: the media then never leaves the TLS connection.
#[cfg(feature = "tls")]
mod rtsps {
    use std::ffi::CString;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use tstrans::event::{TstEvent, TstEventKind};
    use tstrans::handle::TstRtspClientBuilder;
    use tstrans::rtp::{
        tst_rtp_demux_receiver_cancel, tst_rtp_demux_receiver_close,
        tst_rtp_demux_receiver_next_event,
    };
    use tstrans::{
        TstRtspMountHandle, TstRtspServer, tst_rtsp_client_builder_connect,
        tst_rtsp_client_builder_new, tst_rtsp_client_builder_tls_root_cert_pem,
        tst_rtsp_client_builder_transport_pref, tst_rtsp_mount_push_video,
        tst_rtsp_server_builder_allow_udp_media, tst_rtsp_server_builder_new,
        tst_rtsp_server_builder_tls_cert_pem, tst_rtsp_session_into_demux_receiver,
        tst_rtsp_session_play, tst_rtsp_session_teardown_and_free,
    };

    use super::{start, stop};
    use crate::common::{last_error_msg, scaled};

    /// A started `rtsps://` server with a self-signed certificate for
    /// 127.0.0.1 and the mount `/live`; `allow` is the UDP-media setter's
    /// argument, `None` when it is never called. Returns the server, the
    /// mount, the port and the certificate a client trusts.
    pub(super) fn tls_server(
        allow: Option<bool>,
    ) -> (*mut TstRtspServer, *mut TstRtspMountHandle, u16, String) {
        let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
            .expect("a self-signed certificate");
        let (cert_pem, key_pem) = (cert.cert.pem(), cert.signing_key.serialize_pem());
        let url = CString::new("rtsps://127.0.0.1:0").unwrap();
        let b = unsafe { tst_rtsp_server_builder_new(url.as_ptr()) };
        assert!(!b.is_null(), "{}", last_error_msg());
        unsafe {
            tst_rtsp_server_builder_tls_cert_pem(
                b,
                cert_pem.as_ptr(),
                cert_pem.len(),
                key_pem.as_ptr(),
                key_pem.len(),
            )
        };
        if let Some(yes) = allow {
            unsafe { tst_rtsp_server_builder_allow_udp_media(b, yes) };
        }
        let (server, mount, port) = start(b);
        (server, mount, port, cert_pem)
    }

    /// A client builder for `rtsps://127.0.0.1:<port>/live` that trusts `cert_pem`.
    pub(super) fn tls_client(port: u16, cert_pem: &str) -> *mut TstRtspClientBuilder {
        let url = CString::new(format!("rtsps://127.0.0.1:{port}/live")).unwrap();
        let b = unsafe { tst_rtsp_client_builder_new(url.as_ptr()) };
        assert!(!b.is_null(), "{}", last_error_msg());
        unsafe { tst_rtsp_client_builder_tls_root_cert_pem(b, cert_pem.as_ptr(), cert_pem.len()) };
        b
    }

    /// A client forced to UDP (transport preference 1, no fallback) against
    /// `server`'s port: the session, or NULL.
    fn udp_pull(port: u16, cert_pem: &str) -> *mut tstrans::TstRtspSession {
        let b = tls_client(port, cert_pem);
        unsafe { tst_rtsp_client_builder_transport_pref(b, 1) };
        unsafe { tst_rtsp_client_builder_connect(b) }
    }

    #[test]
    fn an_rtsps_server_left_at_its_default_refuses_udp_media() {
        let (server, mount, port, cert) = tls_server(None);
        let session = udp_pull(port, &cert);
        assert!(session.is_null(), "a UDP SETUP was served on rtsps://");
        let msg = last_error_msg();
        assert_eq!(
            unsafe { tstrans::error::tst_get_last_error() },
            tstrans::error::TstError::RtspProtocol as i32,
            "{msg}"
        );
        assert!(msg.contains("461"), "{msg}");
        stop(server, mount);
    }

    #[test]
    fn an_rtsps_server_that_allows_udp_media_serves_it() {
        let (server, mount, port, cert) = tls_server(Some(true));
        let session = udp_pull(port, &cert);
        assert!(!session.is_null(), "{}", last_error_msg());
        unsafe { tst_rtsp_session_teardown_and_free(session) };
        stop(server, mount);
    }

    /// One SPS + PPS + IDR access unit.
    const H264_AU: &[u8] = &[
        0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1f, 0x96, 0x54, 0x05, 0x01, // SPS
        0x00, 0x00, 0x00, 0x01, 0x68, 0xce, 0x3c, 0x80, // PPS
        0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x80, 0x40, // IDR
    ];

    /// A raw C handle moved to a helper thread.
    struct SendPtr<T>(*mut T);
    // SAFETY: the pushes and the receiver's cancel are documented as safe
    // from another thread.
    unsafe impl<T> Send for SendPtr<T> {}
    impl<T> SendPtr<T> {
        fn get(&self) -> *mut T {
            self.0
        }
    }

    /// A TCP forwarder in front of `server_port` for one connection. It
    /// cannot read the TLS records, but it counts the bytes the server sends
    /// on the control connection (before passing them on), which is where
    /// TCP-interleaved media travels and UDP media does not.
    fn counting_forwarder(server_port: u16) -> (u16, Arc<AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let count = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&count);
        std::thread::spawn(move || {
            let Ok((client, _)) = listener.accept() else {
                return;
            };
            let server = TcpStream::connect(("127.0.0.1", server_port)).unwrap();
            let (mut from_client, mut to_server) =
                (client.try_clone().unwrap(), server.try_clone().unwrap());
            std::thread::spawn(move || {
                let _ = std::io::copy(&mut from_client, &mut to_server);
                let _ = to_server.shutdown(Shutdown::Write);
            });
            let (mut from_server, mut to_client) = (server, client);
            let mut buf = [0u8; 16 * 1024];
            while let Ok(n) = from_server.read(&mut buf) {
                if n == 0 {
                    break;
                }
                counted.fetch_add(n as u64, Ordering::SeqCst);
                if to_client.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            let _ = to_client.shutdown(Shutdown::Both);
        });
        (port, count)
    }

    /// Pulls `/live` from an `rtsps://` server that allows UDP media, through
    /// a counting forwarder, with the client's transport preference `pref`
    /// (`None`: the setter is never called). Returns how many bytes the
    /// server sent on the control connection between PLAY's reply and the
    /// fifth video sample the receiver handed out.
    fn control_bytes_while_media_flows(pref: Option<u32>) -> u64 {
        const SAMPLES: usize = 5;
        let (server, mount, port, cert) = tls_server(Some(true));
        let (fwd, count) = counting_forwarder(port);
        let b = tls_client(fwd, &cert);
        if let Some(p) = pref {
            unsafe { tst_rtsp_client_builder_transport_pref(b, p) };
        }
        let session = unsafe { tst_rtsp_client_builder_connect(b) };
        assert!(!session.is_null(), "{}", last_error_msg());
        assert_eq!(
            unsafe { tst_rtsp_session_play(session) },
            0,
            "{}",
            last_error_msg()
        );
        let rx = unsafe { tst_rtsp_session_into_demux_receiver(session, std::ptr::null()) };
        assert!(!rx.is_null(), "{}", last_error_msg());
        // The PLAY reply was read, so it is counted already.
        let before = count.load(Ordering::SeqCst);
        let stop_pushing = Arc::new(AtomicBool::new(false));
        let pusher = {
            let (mount, stop) = (SendPtr(mount), Arc::clone(&stop_pushing));
            std::thread::spawn(move || {
                let mut pts: i64 = 0;
                while !stop.load(Ordering::SeqCst) {
                    let rc = unsafe {
                        tst_rtsp_mount_push_video(
                            mount.get(),
                            H264_AU.as_ptr(),
                            H264_AU.len(),
                            pts,
                            true,
                        )
                    };
                    assert_eq!(rc, 0, "push_video: {}", last_error_msg());
                    pts += 3600;
                    // Pacing, not synchronisation: about 25 access units a second.
                    std::thread::sleep(Duration::from_millis(40));
                }
            })
        };
        // A receiver that never delivers is cancelled, so the test fails
        // instead of hanging.
        let watchdog = {
            let (rx, stop) = (SendPtr(rx), Arc::clone(&stop_pushing));
            let deadline = Instant::now() + scaled(Duration::from_secs(10));
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    if Instant::now() >= deadline {
                        unsafe { tst_rtp_demux_receiver_cancel(rx.get()) };
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            })
        };
        let mut samples = 0;
        let mut ev = TstEvent::default();
        while samples < SAMPLES {
            let rc = unsafe { tst_rtp_demux_receiver_next_event(rx, &mut ev) };
            assert_eq!(rc, 0, "only {samples} video samples: {}", last_error_msg());
            if ev.kind == TstEventKind::Sample as i32 && unsafe { ev.u.sample.pid } == 0x1011 {
                samples += 1;
            }
        }
        let during = count.load(Ordering::SeqCst) - before;
        stop_pushing.store(true, Ordering::SeqCst);
        pusher.join().unwrap();
        watchdog.join().unwrap();
        unsafe { tst_rtp_demux_receiver_close(rx) };
        stop(server, mount);
        during
    }

    /// A pull left at its default on `rtsps://` asks for TCP-interleaved,
    /// although this server would grant UDP: the media comes inside the TLS
    /// connection (each sample is at least one 188-byte TS packet there).
    #[test]
    fn an_rtsps_pull_left_at_its_default_ends_on_tcp_interleaved() {
        let during = control_bytes_while_media_flows(None);
        assert!(
            during >= 5 * 188,
            "{during} bytes on the control connection"
        );
    }

    /// Calling the setter with 0 overrides the URL's default: UDP first, and
    /// this server grants it, so no media crosses the control connection.
    #[test]
    fn an_rtsps_pull_with_transport_pref_0_asks_for_udp_first() {
        let during = control_bytes_while_media_flows(Some(0));
        assert!(during < 188, "{during} bytes on the control connection");
    }
}
