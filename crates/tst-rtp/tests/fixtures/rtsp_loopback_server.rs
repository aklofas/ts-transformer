//! Tokio-driven loopback RTSP server fixture.
//!
//! Binds 127.0.0.1:0 (kernel picks port); speaks RTSP/1.0 by default
//! (RTSP/2.0 if client sends 2.0). Supports both UDP and
//! TCP-interleaved transports. Configurable to demand `Basic`, `MD5
//! Digest`, `SHA-256 Digest`, or no auth. Configurable to return
//! `461 Unsupported Transport` on first UDP SETUP (forces TCP
//! fallback).
//!
//! Returns a `FixtureHandle` that exposes:
//! - `port()` — the TCP port to put in `rtsp://127.0.0.1:<port>/test`
//! - `set_auth_mode(...)` — configure auth requirement
//! - `force_461_on_udp(true)` — drop the first UDP SETUP with 461
//! - `shutdown()` — kill the server thread

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    None,
    Basic,
    DigestMd5,
    DigestSha256,
}

pub struct FixtureConfig {
    pub auth: AuthMode,
    pub force_461_on_udp: bool,
    pub username: String,
    pub password: String,
    pub sdp_body: Vec<u8>,
    /// Optional raw bytes to write to the client immediately after the PLAY
    /// 200 OK response. Used by H.264 tests to push TCP-interleaved `$`-frames.
    pub play_data: Vec<u8>,
    /// If true, the server records all SETUP requests received.
    /// The count is exposed through the shared `setup_count` in `FixtureHandle`.
    pub track_setup: bool,
    /// `Session:` header timeout advertised in the SETUP response —
    /// `Some(t)` renders `Session: <id>;timeout=<t>`, `None` omits the
    /// parameter (RFC 7826 §18.49 default of 60 s applies client-side).
    pub setup_timeout_secs: Option<u64>,
    /// When true, EVERY OPTIONS request is authenticated individually
    /// (per-request, ignoring the connection's `auth_passed` latch) —
    /// unauthorized ones get 401 + challenge and are NOT counted in the
    /// OPTIONS counters. Models servers that challenge keepalive pings;
    /// the default (false) mirrors our own `RtspServer`, which never
    /// auth-gates OPTIONS (it is the connectivity probe, and OPTIONS never
    /// resets the 3-strike auth-failure lockout).
    pub challenge_options: bool,
}

impl Default for FixtureConfig {
    fn default() -> Self {
        Self {
            auth: AuthMode::None,
            force_461_on_udp: false,
            username: "admin".into(),
            password: "secret".into(),
            // RFC 2250 §2: MP2T uses m=video; RFC 7826 App. D: session-level
            // a=control:* lets clients resolve the aggregate control URL.
            sdp_body: br#"v=0
o=- 0 0 IN IP4 127.0.0.1
s=tst-rtp test
t=0 0
a=control:*
m=video 0 RTP/AVP 33
a=control:trackID=0
"#
            .to_vec(),
            play_data: Vec::new(),
            track_setup: false,
            setup_timeout_secs: Some(60),
            challenge_options: false,
        }
    }
}

pub struct FixtureHandle {
    pub port: u16,
    /// Number of SETUP requests the server has received (only tracked when
    /// `FixtureConfig::track_setup` is true).
    pub setup_count: Arc<std::sync::atomic::AtomicU32>,
    /// Total OPTIONS requests received (always tracked — keepalive tests
    /// observe the client's ping cadence through this).
    pub options_total: Arc<std::sync::atomic::AtomicU32>,
    /// OPTIONS requests that carried a `Session:` header (i.e. keepalives
    /// bound to the SETUP-issued session, which is what actually refreshes
    /// the server's session timer per RFC 7826 §10.5).
    pub options_with_session: Arc<std::sync::atomic::AtomicU32>,
    shutdown: Arc<AtomicBool>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl FixtureHandle {
    pub fn spawn(cfg: FixtureConfig) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let setup_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let setup_count_clone = setup_count.clone();
        let options_total = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let options_total_clone = options_total.clone();
        let options_with_session = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let options_with_session_clone = options_with_session.clone();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let cfg_arc = Arc::new(Mutex::new(cfg));
        let (port_tx, port_rx) = std::sync::mpsc::sync_channel(1);
        runtime.spawn(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            port_tx.send(listener.local_addr().unwrap().port()).unwrap();
            loop {
                if shutdown_clone.load(Ordering::Relaxed) {
                    break;
                }
                tokio::select! {
                    accept_res = listener.accept() => {
                        match accept_res {
                            Ok((sock, peer)) => {
                                let cfg = cfg_arc.lock().unwrap().clone();
                                tokio::spawn(handle_client(
                                    sock,
                                    peer,
                                    cfg,
                                    shutdown_clone.clone(),
                                    setup_count_clone.clone(),
                                    options_total_clone.clone(),
                                    options_with_session_clone.clone(),
                                ));
                            }
                            Err(_) => break,
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                        // Wake to check shutdown flag
                    }
                }
            }
        });
        let port = port_rx.recv().unwrap();
        Self {
            port,
            setup_count,
            options_total,
            options_with_session,
            shutdown,
            runtime: Some(runtime),
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl Drop for FixtureHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(std::time::Duration::from_secs(2));
        }
    }
}

impl Clone for FixtureConfig {
    fn clone(&self) -> Self {
        Self {
            auth: self.auth,
            force_461_on_udp: self.force_461_on_udp,
            username: self.username.clone(),
            password: self.password.clone(),
            sdp_body: self.sdp_body.clone(),
            play_data: self.play_data.clone(),
            track_setup: self.track_setup,
            setup_timeout_secs: self.setup_timeout_secs,
            challenge_options: self.challenge_options,
        }
    }
}

async fn handle_client(
    mut sock: TcpStream,
    _peer: SocketAddr,
    cfg: FixtureConfig,
    shutdown: Arc<AtomicBool>,
    setup_count: Arc<std::sync::atomic::AtomicU32>,
    options_total: Arc<std::sync::atomic::AtomicU32>,
    options_with_session: Arc<std::sync::atomic::AtomicU32>,
) {
    let mut buf = vec![0u8; 8192];
    let mut accumulator = Vec::new();
    let mut auth_passed = matches!(cfg.auth, AuthMode::None);
    let mut session_id = String::new();
    let mut udp_setup_attempts = 0;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let n = match sock.read(&mut buf).await {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };
        accumulator.extend_from_slice(&buf[..n]);

        // Parse one RTSP message from accumulator
        let end = match find_message_end(&accumulator) {
            Some(end) => end,
            None => continue,
        };
        let request = String::from_utf8_lossy(&accumulator[..end]).into_owned();
        accumulator.drain(..end);

        let method = request.split(' ').next().unwrap_or("").to_string();
        let cseq = extract_header(&request, "CSeq").unwrap_or_else(|| "0".to_string());
        // Auth check
        if !auth_passed && method != "OPTIONS" {
            if let Some(auth_header) = extract_header(&request, "Authorization") {
                auth_passed = validate_auth(&cfg, &method, &auth_header);
            }
            if !auth_passed {
                let _ = sock
                    .write_all(
                        format!(
                            "RTSP/1.0 401 Unauthorized\r\nCSeq: {}\r\nWWW-Authenticate: {}\r\n\r\n",
                            cseq,
                            challenge_for(cfg.auth),
                        )
                        .as_bytes(),
                    )
                    .await;
                continue;
            }
        }

        // Route by method
        match method.as_str() {
            "OPTIONS" => {
                // Per-request auth (deliberately NOT the per-connection
                // `auth_passed` latch): each ping must carry its own valid
                // Authorization, the way pre-emptive keepalive signing is
                // meant to work against a challenging server.
                if cfg.challenge_options {
                    let authorized = extract_header(&request, "Authorization")
                        .map(|h| validate_auth(&cfg, &method, &h))
                        .unwrap_or(false);
                    if !authorized {
                        let _ = sock
                            .write_all(
                                format!(
                                    "RTSP/1.0 401 Unauthorized\r\nCSeq: {}\r\nWWW-Authenticate: {}\r\n\r\n",
                                    cseq,
                                    challenge_for(cfg.auth),
                                )
                                .as_bytes(),
                            )
                            .await;
                        continue;
                    }
                }
                options_total.fetch_add(1, Ordering::Relaxed);
                if extract_header(&request, "Session").is_some() {
                    options_with_session.fetch_add(1, Ordering::Relaxed);
                }
                let _ = sock.write_all(format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {}\r\nPublic: OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN\r\n\r\n",
                    cseq,
                ).as_bytes()).await;
            }
            "DESCRIBE" => {
                let _ = sock.write_all(format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n",
                    cseq, cfg.sdp_body.len(),
                ).as_bytes()).await;
                let _ = sock.write_all(&cfg.sdp_body).await;
            }
            "SETUP" => {
                if cfg.track_setup {
                    setup_count.fetch_add(1, Ordering::Relaxed);
                }
                let transport = extract_header(&request, "Transport").unwrap_or_default();
                let is_udp = transport.contains("RTP/AVP;") && !transport.contains("/TCP");
                if is_udp && cfg.force_461_on_udp && udp_setup_attempts == 0 {
                    udp_setup_attempts += 1;
                    let _ = sock
                        .write_all(
                            format!(
                                "RTSP/1.0 461 Unsupported Transport\r\nCSeq: {}\r\n\r\n",
                                cseq,
                            )
                            .as_bytes(),
                        )
                        .await;
                    continue;
                }
                session_id = format!("{:08X}", rand_session_id());
                let resp_transport = if is_udp {
                    let client_port = extract_client_port(&transport).unwrap_or(5004);
                    format!(
                        "RTP/AVP;unicast;client_port={}-{};server_port=6970-6971",
                        client_port,
                        client_port + 1
                    )
                } else {
                    "RTP/AVP/TCP;unicast;interleaved=0-1".to_string()
                };
                let session_hdr = match cfg.setup_timeout_secs {
                    Some(t) => format!("{session_id};timeout={t}"),
                    None => session_id.clone(),
                };
                let _ = sock
                    .write_all(
                        format!(
                            "RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: {}\r\nTransport: {}\r\n\r\n",
                            cseq, session_hdr, resp_transport,
                        )
                        .as_bytes(),
                    )
                    .await;
            }
            "PLAY" => {
                let _ = sock.write_all(format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: {}\r\nRTP-Info: url=rtsp://127.0.0.1/test/streamid=0;seq=1234;rtptime=5000000\r\n\r\n",
                    cseq, session_id,
                ).as_bytes()).await;
                // Push any canned interleaved data after the PLAY response.
                if !cfg.play_data.is_empty() {
                    let _ = sock.write_all(&cfg.play_data).await;
                }
            }
            "PAUSE" => {
                let _ = sock
                    .write_all(
                        format!(
                            "RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: {}\r\n\r\n",
                            cseq, session_id,
                        )
                        .as_bytes(),
                    )
                    .await;
            }
            "TEARDOWN" => {
                let _ = sock
                    .write_all(
                        format!(
                            "RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: {}\r\n\r\n",
                            cseq, session_id,
                        )
                        .as_bytes(),
                    )
                    .await;
                return;
            }
            _ => {
                let _ = sock
                    .write_all(
                        format!("RTSP/1.0 501 Not Implemented\r\nCSeq: {}\r\n\r\n", cseq,)
                            .as_bytes(),
                    )
                    .await;
            }
        }
    }
}

fn find_message_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// The `WWW-Authenticate` challenge value for the configured auth mode.
/// Panics on `AuthMode::None` (callers only challenge when auth is on).
fn challenge_for(auth: AuthMode) -> String {
    match auth {
        AuthMode::Basic => r#"Basic realm="test""#.to_string(),
        AuthMode::DigestMd5 => r#"Digest realm="test", nonce="abc123", algorithm=MD5"#.to_string(),
        AuthMode::DigestSha256 => {
            r#"Digest realm="test", nonce="abc123", algorithm=SHA-256, qop="auth""#.to_string()
        }
        AuthMode::None => unreachable!("challenge requested with auth disabled"),
    }
}

fn extract_header(req: &str, name: &str) -> Option<String> {
    let lname = name.to_ascii_lowercase();
    for line in req.split("\r\n") {
        if let Some(colon) = line.find(':') {
            if line[..colon].trim().to_ascii_lowercase() == lname {
                return Some(line[colon + 1..].trim().to_string());
            }
        }
    }
    None
}

fn extract_client_port(transport: &str) -> Option<u16> {
    transport
        .split(';')
        .find_map(|p| p.trim().strip_prefix("client_port="))
        .and_then(|v| v.split('-').next())
        .and_then(|s| s.parse().ok())
}

fn validate_auth(cfg: &FixtureConfig, _method: &str, header: &str) -> bool {
    match cfg.auth {
        AuthMode::Basic => {
            // Compare base64-encoded user:pass
            use base64::Engine;
            let expected = base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", cfg.username, cfg.password));
            header.contains(&expected)
        }
        AuthMode::DigestMd5 | AuthMode::DigestSha256 => {
            // Lax check — just look for username= in the header
            header.contains(&format!("username=\"{}\"", cfg.username))
        }
        AuthMode::None => true,
    }
}

fn rand_session_id() -> u32 {
    let mut bytes = [0u8; 4];
    getrandom::getrandom(&mut bytes).unwrap();
    u32::from_le_bytes(bytes)
}
