//! A caller that speaks the SRT caller-listener handshake over a plain UDP
//! socket, so admission tests can send what libsrt's own caller never would
//! (a handshake-v4 conclusion, repeated stream-ID blocks) to our listener.
//!
//! Layout follows libsrt `srtcore/`: the 16-byte control header (`packet.h`),
//! the 48-byte handshake body (`CHandShake::store_to`, `handshake.cpp`) and the
//! extension blocks a v5 caller appends (`CUDT::createSrtHandshake`,
//! `core.cpp`). Every 32-bit word travels big-endian.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HS_VERSION_UDT4: u32 = 4;
const HS_VERSION_SRT1: u32 = 5;
const UDT_DGRAM: u32 = 2;
const URQ_INDUCTION: i32 = 1;
const URQ_CONCLUSION: i32 = -1;
const URQ_FAILURE_TYPES: i32 = 1000;
const UMSG_HANDSHAKE: u32 = 0;
const UMSG_SHUTDOWN: u32 = 5;
const HS_EXT_HSREQ: u32 = 1 << 0;
const HS_EXT_CONFIG: u32 = 1 << 2;
const SRT_CMD_HSREQ: u32 = 1;
const SRT_CMD_SID: u32 = 5;
/// The SRT version the HSREQ block advertises (libsrt 1.5.7).
const SRT_VERSION: u32 = 0x0001_0507;
/// TSBPDSND | TSBPDRCV | HAICRYPT | TLPKTDROP | NAKREPORT | REXMITFLG |
/// FILTERCAP: what a libsrt 1.5 live-mode caller sets (`handshake.h` SRT_OPT_*).
const SRT_FLAGS: u32 = 0xBF;
const LATENCY_MS: u32 = 120;
const HEADER_LEN: usize = 16;
const HS_LEN: usize = 48;
/// libsrt's caller repeats an unanswered handshake every 250 ms.
const RESEND: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HsVersion {
    /// A legacy (libsrt < 1.3.0) caller: version 4, no extension blocks.
    V4,
    V5,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HsOutcome {
    /// The listener answered the conclusion with a conclusion.
    Accepted,
    /// The listener answered with a failure request type; this is the reject
    /// reason, i.e. the request type minus `URQ_FAILURE_TYPES`.
    Rejected(i32),
    /// The listener answered with a shutdown: how libsrt refuses a v4
    /// caller, which cannot read a failure request type
    /// (`processConnectRequest`, core.cpp).
    Shutdown,
    NoAnswer,
}

pub struct RawCaller {
    sock: UdpSocket,
    socket_id: u32,
    start: Instant,
}

enum Reply {
    Handshake { req_type: i32, cookie: u32 },
    Shutdown,
}

impl RawCaller {
    /// Induction then conclusion against `127.0.0.1:port`. A v5 conclusion
    /// carries an HSREQ block and then one SID block per entry of
    /// `sid_blocks`, in order; it never carries a KMREQ.
    pub fn handshake(
        port: u16,
        version: HsVersion,
        sid_blocks: &[&[u8]],
        timeout: Duration,
    ) -> HsOutcome {
        assert!(
            version == HsVersion::V5 || sid_blocks.is_empty(),
            "a v4 handshake has no extension blocks"
        );
        let sock = UdpSocket::bind("127.0.0.1:0").expect("bind raw caller");
        sock.connect(("127.0.0.1", port))
            .expect("connect raw caller");
        let caller = RawCaller {
            sock,
            socket_id: fresh_socket_id(),
            start: Instant::now(),
        };
        let deadline = Instant::now() + timeout;

        // libsrt's v5 caller also sends version 4 in the induction
        // (`startConnect`, core.cpp); the listener answers version 5.
        let induction = caller.packet(HS_VERSION_UDT4, UDT_DGRAM, URQ_INDUCTION, 0, &[]);
        let cookie = match caller.exchange(&induction, deadline, |r| match r {
            Reply::Handshake {
                req_type: URQ_INDUCTION,
                cookie,
            } => Some(Ok(*cookie)),
            Reply::Handshake { req_type, .. } if *req_type >= URQ_FAILURE_TYPES => {
                Some(Err(HsOutcome::Rejected(req_type - URQ_FAILURE_TYPES)))
            }
            Reply::Shutdown => Some(Err(HsOutcome::Shutdown)),
            Reply::Handshake { .. } => None,
        }) {
            Some(Ok(cookie)) => cookie,
            Some(Err(outcome)) => return outcome,
            None => return HsOutcome::NoAnswer,
        };

        let conclusion = match version {
            HsVersion::V4 => caller.packet(HS_VERSION_UDT4, UDT_DGRAM, URQ_CONCLUSION, cookie, &[]),
            HsVersion::V5 => {
                let mut ext = vec![
                    cmdspec(SRT_CMD_HSREQ, 3),
                    SRT_VERSION,
                    SRT_FLAGS,
                    (LATENCY_MS << 16) | LATENCY_MS,
                ];
                for sid in sid_blocks {
                    ext.extend(sid_block(sid));
                }
                let mut flags = HS_EXT_HSREQ;
                if !sid_blocks.is_empty() {
                    flags |= HS_EXT_CONFIG;
                }
                caller.packet(HS_VERSION_SRT1, flags, URQ_CONCLUSION, cookie, &ext)
            }
        };
        caller
            .exchange(&conclusion, deadline, |r| match r {
                Reply::Handshake {
                    req_type: URQ_CONCLUSION,
                    ..
                } => Some(HsOutcome::Accepted),
                Reply::Handshake { req_type, .. } if *req_type >= URQ_FAILURE_TYPES => {
                    Some(HsOutcome::Rejected(req_type - URQ_FAILURE_TYPES))
                }
                Reply::Shutdown => Some(HsOutcome::Shutdown),
                // A late answer to a repeated induction.
                Reply::Handshake { .. } => None,
            })
            .unwrap_or(HsOutcome::NoAnswer)
    }

    /// Send `pkt` (again every `RESEND`) until `pick` accepts a reply or the
    /// deadline passes.
    fn exchange<T>(
        &self,
        pkt: &[u8],
        deadline: Instant,
        mut pick: impl FnMut(&Reply) -> Option<T>,
    ) -> Option<T> {
        let mut buf = [0u8; 1500];
        loop {
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            self.sock.send(pkt).expect("send handshake");
            let resend_at = (now + RESEND).min(deadline);
            loop {
                let wait = resend_at.saturating_duration_since(Instant::now());
                if wait.is_zero() {
                    break;
                }
                self.sock
                    .set_read_timeout(Some(wait))
                    .expect("read timeout");
                let n = match self.sock.recv(&mut buf) {
                    Ok(n) => n,
                    Err(_) => break,
                };
                if let Some(reply) = parse_reply(&buf[..n])
                    && let Some(out) = pick(&reply)
                {
                    return Some(out);
                }
            }
        }
    }

    fn packet(
        &self,
        version: u32,
        hs_type: u32,
        req_type: i32,
        cookie: u32,
        ext: &[u32],
    ) -> Vec<u8> {
        let ts = self.start.elapsed().as_micros() as u32;
        let mut words = vec![
            // Header: control bit + type, type-specific info, timestamp,
            // destination socket id (0: the listener).
            0x8000_0000 | (UMSG_HANDSHAKE << 16),
            0,
            ts,
            0,
            // Handshake body.
            version,
            hs_type,
            self.socket_id & 0x7FFF_FFFF, // initial sequence number
            1500,                         // MSS
            8192,                         // flow window
            req_type as u32,
            self.socket_id,
            cookie,
            // Peer IP as libsrt stores an IPv4 address (`CIPAddress::ntop`):
            // the in-memory `s_addr`, then swapped like every other word.
            u32::from_ne_bytes([127, 0, 0, 1]),
            0,
            0,
            0,
        ];
        words.extend_from_slice(ext);
        words.iter().flat_map(|w| w.to_be_bytes()).collect()
    }
}

fn parse_reply(pkt: &[u8]) -> Option<Reply> {
    let word = |i: usize| u32::from_be_bytes(pkt[i * 4..i * 4 + 4].try_into().unwrap());
    if pkt.len() < HEADER_LEN || word(0) & 0x8000_0000 == 0 {
        return None;
    }
    match (word(0) >> 16) & 0x7FFF {
        UMSG_HANDSHAKE if pkt.len() >= HEADER_LEN + HS_LEN => Some(Reply::Handshake {
            req_type: word(4 + 5) as i32,
            cookie: word(4 + 7),
        }),
        UMSG_SHUTDOWN => Some(Reply::Shutdown),
        _ => None,
    }
}

fn cmdspec(cmd: u32, words: usize) -> u32 {
    (cmd << 16) | words as u32
}

/// A SID block: NUL-padded to whole words, each word stored little-endian
/// before the big-endian wire swap (`fillHsExtConfigString`, core.cpp), so
/// on the wire every 4-byte group of the string appears reversed.
fn sid_block(sid: &[u8]) -> Vec<u32> {
    let mut padded = sid.to_vec();
    padded.resize(sid.len().div_ceil(4) * 4, 0);
    let mut block = vec![cmdspec(SRT_CMD_SID, padded.len() / 4)];
    block.extend(
        padded
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap())),
    );
    block
}

/// A socket id no other raw caller in this process uses; libsrt keys a
/// pending connection by peer id and address.
fn fresh_socket_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let id = (seed ^ NEXT.fetch_add(0x9E37_79B9, Ordering::Relaxed)) & 0x3FFF_FFFF;
    id.max(1)
}
