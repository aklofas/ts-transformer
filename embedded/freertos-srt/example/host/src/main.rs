//! freertos-srt host harness — two modes sharing one crate/golden:
//!
//! - default (no args): the `example` gate's tst-srt SRT LISTENER, receiving
//!   the firmware caller's GOLDEN×N stream over SLIRP and verifying it
//!   byte-exact. The authoritative verdict for the `example` gate.
//!   `FREERTOS_SRT_PASSPHRASE` env (set by the gate in Phase B) enables
//!   AES-128.
//! - `--send <host:port>`: the `srt-recv` gate's tst-srt SRT CALLER (the
//!   reverse role) — connects to the firmware's SRT LISTENER on the guest NIC
//!   and streams the golden once. The firmware is the authoritative verifier
//!   there (it demuxes on-device), so this mode only needs to prove the send
//!   succeeded — see `run_send`'s doc comment.
use std::io::Write;
use std::process::exit;
use std::time::Duration;
use tst_srt::{KeyLength, ListenerBuilder, Passphrase, SocketBuilder};

include!(concat!(env!("OUT_DIR"), "/golden.rs")); // pub static GOLDEN + GOLDEN_LEN

const REPEAT: usize = 64;
const STREAM_LEN: usize = GOLDEN_LEN * REPEAT;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--send" {
        run_send(&args[2]);
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
// driver only needs to prove the bytes actually left the host.
fn run_send(addr: &str) {
    // sender_defaults() (connect_timeout=15s, linger=5s, role=Sender) is the
    // house preset for a pure caller-sender socket — see its doc example in
    // crates/tst-srt/src/builder.rs and crates/tst-interop/src/transport.rs's
    // rationale for why a SHARED send/recv builder can't use it (this
    // function is send-only, so nothing here blocks using it directly).
    // Explicit over SocketBuilder::new()'s raw (receiver-shaped) defaults —
    // the golden's LIVE handshake proved fine either way over lossless SLIRP
    // loopback, but a real radio-link deployment of this driver shape
    // shouldn't be timing-sensitive on the caller-role/connect-timeout knobs.
    let mut b = SocketBuilder::new();
    b.sender_defaults();
    let mut sock = match b.connect(addr) {
        Ok(s) => s,
        Err(e) => { eprintln!("FAIL[srt_recv_host_send]: connect {addr}: {e}"); exit(1); }
    };
    if let Err(e) = sock.send(&GOLDEN) {
        eprintln!("FAIL[srt_recv_host_send]: send: {e}");
        exit(1);
    }
    // Deterministic drain: poll the send buffer until libsrt reports it
    // empty, bounded to 5s in 50ms steps (the golden is a single small LIVE
    // message, 564 B — should drain in well under a second over lossless
    // SLIRP loopback; the bound is a backstop, not the expected path). The
    // firmware-side listener closes its accepted socket immediately once it
    // has read all the bytes it expects (it has nothing to send back), so a
    // stats() error mid-poll most often means the peer already closed after
    // reading everything — treated as "nothing left to drain", not fatal:
    // the data already left via the successful send() above, and the
    // firmware is the authoritative verifier (it demuxes on-device and
    // prints its own PASS/FAIL token).
    let mut drained = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match sock.stats() {
            Ok(s) if s.send_buffer_packets == 0 => { drained = true; break; }
            Ok(_) => {}
            Err(_) => { drained = true; break; }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !drained {
        eprintln!("WARN[srt_recv_host_send]: send buffer did not report empty within 5s; proceeding anyway");
    }
    sock.close();
    println!("PASS: srt_recv_host_send ({GOLDEN_LEN} bytes streamed to {addr})");
}

fn run_recv() {
    let aes = std::env::var("FREERTOS_SRT_PASSPHRASE").ok();
    let tag = if aes.is_some() { "s4_host_aes" } else { "s4_host_plain" };

    let mut lb = ListenerBuilder::new();
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
