//! freertos-srt host harness — two modes sharing one crate/golden:
//!
//! - default (no args): the `example` gate's tst-srt SRT LISTENER, receiving
//!   the firmware caller's GOLDEN×N stream over SLIRP and verifying it
//!   byte-exact. The authoritative verdict for the `example` gate.
//!   `FREERTOS_SRT_PASSPHRASE` env (set by the gate in Phase B) enables
//!   AES-128.
//! - `--send <host:port> <hold-secs>`: the `srt-recv` gate's tst-srt SRT CALLER (the
//!   reverse role) — connects to the firmware's SRT LISTENER on the guest NIC
//!   and streams the golden once, then holds the connection until the
//!   firmware closes it. The firmware is the authoritative verifier
//!   there (it demuxes on-device), so this mode only needs to prove the send
//!   succeeded and keep the connection up until the firmware has read it —
//!   see `run_send`'s doc comment.
use std::io::Write;
use std::process::exit;
use std::time::Duration;
use tst_srt::{KeyLength, Passphrase, RecvError, SrtListenerBuilder, SrtSocketBuilder};

include!(concat!(env!("OUT_DIR"), "/golden.rs")); // pub static GOLDEN + GOLDEN_LEN

const REPEAT: usize = 64;
const STREAM_LEN: usize = GOLDEN_LEN * REPEAT;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 && args[1] == "--send" {
        // `--send <host:port> <hold-secs>`: hold-secs bounds the wait for the
        // receiver's close (see run_send); the gate script owns the number.
        let hold = match args.get(3).map(|s| s.parse::<u64>()) {
            Some(Ok(secs)) if args.len() == 4 => Duration::from_secs(secs),
            _ => {
                eprintln!("usage: freertos-srt-host --send <host:port> <hold-secs>");
                exit(2);
            }
        };
        run_send(&args[2], hold);
        return;
    }
    run_recv();
}

// srt-recv gate: CALLER mode, the reverse of `run_recv` below. Connects to
// the firmware's SRT LISTENER (reached through QEMU's SLIRP hostfwd=udp port
// forward — the guest has no routable inbound address otherwise) and streams
// the golden ONCE as a single LIVE message. No REPEAT/pacing: the golden is
// 564 B, "burst is fine" for a stream this small, and the firmware-side gate
// is the authoritative verifier (on-device demux + byte-compare), so this
// driver only needs to prove the bytes actually left the host and keep the
// connection up until the firmware has read them.
fn run_send(addr: &str, hold: Duration) {
    // sender_defaults() (connect_timeout=15s, linger=5s, role=Sender) is the
    // house preset for a pure caller-sender socket — see its doc example in
    // crates/tst-srt/src/builder.rs and crates/tst-interop/src/transport.rs's
    // rationale for why a SHARED send/recv builder can't use it (this
    // function is send-only, so nothing here blocks using it directly).
    // Explicit over SrtSocketBuilder::new()'s raw (receiver-shaped) defaults —
    // the golden's LIVE handshake proved fine either way over lossless SLIRP
    // loopback, but a real radio-link deployment of this driver shape
    // shouldn't be timing-sensitive on the caller-role/connect-timeout knobs.
    let mut b = SrtSocketBuilder::new();
    b.sender_defaults();
    let mut sock = match b.connect(addr) {
        Ok(s) => s,
        Err(e) => { eprintln!("FAIL[srt_recv_host_send]: connect {addr}: {e}"); exit(1); }
    };
    if let Err(e) = sock.send(&GOLDEN) {
        eprintln!("FAIL[srt_recv_host_send]: send: {e}");
        exit(1);
    }
    // Hold the socket open until the firmware closes its end. The firmware
    // reads the message only at its TSBPD release time (send time plus the
    // negotiated latency, later on a starved emulated guest), and a SHUTDOWN
    // that reaches a receiver parked in srt_recv before that release makes
    // libsrt drop the message and report the connection lost. An ACK, or an
    // empty send buffer, only says the packet reached the peer's buffer, not
    // its reader, so neither is a safe moment to close. The firmware closes
    // right after its last read, so a broken connection here is the
    // hand-off; the gate's bound is the backstop.
    if let Err(e) = sock.set_recv_timeout(Some(hold)) {
        eprintln!("FAIL[srt_recv_host_send]: set_recv_timeout: {e}");
        exit(1);
    }
    // Message API: the buffer must hold a full payload even though the
    // firmware never sends anything back.
    let mut sink = [0u8; 2048];
    loop {
        match sock.recv(&mut sink) {
            Ok(n) if n > 0 => continue,
            Err(RecvError::TimedOut) => {
                eprintln!(
                    "WARN[srt_recv_host_send]: the receiver did not close within {}s; closing anyway",
                    hold.as_secs()
                );
                break;
            }
            // The receiver closed its end: the hand-off.
            Ok(_) | Err(RecvError::ConnectionBroken) => break,
            // Anything else ends the wait before the receiver is known to
            // have read the message, so say so; the firmware token decides.
            Err(e) => {
                eprintln!("WARN[srt_recv_host_send]: recv while holding: {e}; closing anyway");
                break;
            }
        }
    }
    sock.close();
    println!("PASS: srt_recv_host_send ({GOLDEN_LEN} bytes streamed to {addr})");
}

fn run_recv() {
    let aes = std::env::var("FREERTOS_SRT_PASSPHRASE").ok();
    let tag = if aes.is_some() { "s4_host_aes" } else { "s4_host_plain" };

    let mut lb = SrtListenerBuilder::new();
    // LIVE mode (tst-srt default) — matches the firmware caller's SRTT_LIVE.
    if let Some(pass) = aes.as_deref() {
        lb.passphrase(Passphrase::new(pass.to_string()).expect("valid passphrase"));
        lb.key_length(KeyLength::Aes128);
    }
    let mut listener = match lb.bind("0.0.0.0:9000") {
        Ok(l) => l,
        Err(e) => { eprintln!("FAIL[{tag}]: bind: {e}"); exit(1); }
    };

    // Signal the gate that the bind is up BEFORE the caller is launched.
    println!("host-ready");
    let _ = std::io::stdout().flush();

    let (mut sock, _peer) = match listener.accept() {
        Ok(x) => x,
        Err(e) => { eprintln!("FAIL[{tag}]: accept: {e}"); exit(1); }
    };

    // LIVE/message mode: srt_recv requires the buffer to be at least the
    // negotiated payload size (~1456 B), even though each message is only 564 B
    // — passing a smaller tail slice throws "Incorrect use of Message API". So
    // recv into a fixed full-size temp buffer each call, then copy into place.
    let mut buf = vec![0u8; STREAM_LEN];
    let mut tmp = [0u8; 2048];
    let mut got = 0usize;
    while got < STREAM_LEN {
        match sock.recv(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                let take = n.min(STREAM_LEN - got);
                buf[got..got + take].copy_from_slice(&tmp[..take]);
                got += take;
            }
            Err(e) => { eprintln!("FAIL[{tag}]: recv at {got}: {e}"); exit(1); }
        }
    }

    let len_ok = got == STREAM_LEN;
    let bytes_ok = len_ok && (0..REPEAT).all(|r| buf[r * GOLDEN_LEN..(r + 1) * GOLDEN_LEN] == GOLDEN[..]);
    if bytes_ok {
        let enc = if aes.is_some() { " (AES-128)" } else { "" };
        println!("PASS: {tag} (received {got} bytes = GOLDEN x {REPEAT} byte-exact{enc})");
        exit(0);
    } else {
        eprintln!("FAIL[{tag}]: len_ok={len_ok} got={got}/{STREAM_LEN} bytes_ok={bytes_ok}");
        exit(1);
    }
}
