//! One SRT listener, many stream IDs: per-ID rules edited while it runs.
//!
//! A single bound port serves several publishers. Each publisher names itself
//! with the SRT stream ID (`srt://host:9000?streamid=publish:amber`), and the
//! listener decides during the handshake, before any data flows:
//!
//!   - an ID with a rule is accepted (optionally with its own passphrase) or
//!     refused with a reason code the caller reads straight away;
//!   - every other ID gets the default rule (accept, unless one is set).
//!
//!   # terminal A: only `publish:amber` (passphrase "amber-secret-1") and
//!   # `publish:cobalt` (no passphrase) may connect
//!   cargo run -p tst-examples --example srt_listener_rules -- \
//!       publish:amber=amber-secret-1 publish:cobalt
//!
//!   # terminal B: a caller that sets the stream ID through an SRT URL (a
//!   # second sender with the same ID is refused with "conflict")
//!   srt-live-transmit file://con \
//!       "srt://127.0.0.1:9000?streamid=publish:amber&passphrase=amber-secret-1" < input.ts
//!   # or
//!   ffmpeg -re -i input.ts -c copy -f mpegts \
//!       "srt://127.0.0.1:9000?streamid=publish:cobalt"
//!
//! With no arguments the listener accepts every ID and still refuses a
//! second publisher that reuses a name already live. Runs until interrupted.
//! Stream IDs that start with `#!` are printed as parsed access-control fields.

use std::collections::HashMap;
use std::env;
use std::thread;
use std::time::Duration;
use tst_srt::error::{RecvError, RejectReason};
use tst_srt::{AccessControl, ListenRule, Passphrase, SrtListenerBuilder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut lb = SrtListenerBuilder::new();
    // Same conventional live latency as the other SRT examples.
    lb.latency(Duration::from_millis(120));
    let mut listener = lb.bind("127.0.0.1:9000")?;

    // Take the rules handle BEFORE the accept loop: `accept()` borrows the
    // listener mutably and parks, so nothing could ask it for the handle
    // afterwards. The handle is a cheap clone of the same table and may be
    // moved to any thread.
    let rules = listener.rules();

    // Optional `NAME=PASSPHRASE` (or bare `NAME`) arguments become the
    // allow-list. Matching is an exact string compare on the raw stream ID.
    // Remember each operator rule: locking a name for its live publisher
    // overwrites the entry, so the original must be put back afterwards.
    let mut originals: HashMap<String, ListenRule> = HashMap::new();
    let mut any_rule = false;
    for arg in env::args().skip(1) {
        let (name, rule) = match arg.split_once('=') {
            // `Passphrase::new` enforces libsrt's 10..=80 byte limit.
            Some((name, pass)) => (
                name.to_string(),
                ListenRule::AcceptWith {
                    passphrase: Passphrase::new(pass)?,
                },
            ),
            None => (arg, ListenRule::Accept),
        };
        rules.set(&name, rule.clone())?;
        originals.insert(name, rule);
        any_rule = true;
    }
    if any_rule {
        // Without a default every unlisted ID would be accepted (libsrt's
        // behavior, and ours). With an allow-list, turn that around: an
        // unknown name is refused with 1404 "not found". Reject codes must
        // be 1000 or higher; lower ones belong to libsrt and `set` refuses
        // them with `ListenRuleError::InternalReason`.
        rules.set_default(ListenRule::Reject(RejectReason::NotFound))?;
    }
    eprintln!("listening on 127.0.0.1:9000 ({} rules)", rules.len());

    let mut workers = Vec::new();
    // Numbers each connection: IDs that sanitize alike (`a-b`, `a_b`) or a
    // name reused after its publisher left would otherwise share one file.
    let mut connection: u64 = 0;
    loop {
        // A caller the rules refuse never reaches this point: libsrt answers
        // it inside the handshake and `accept` keeps waiting.
        let (mut socket, peer) = listener.accept()?;
        let id = socket.stream_id().map(str::to_owned);
        eprintln!("accepted {peer} with stream id {id:?}");

        if let Some(id) = &id {
            // Free-form IDs fail to parse with `NotAccessControl`; that is
            // normal, not an error. A `#!::u=...,r=...` ID yields fields.
            if id.starts_with("#!") {
                match AccessControl::parse(id) {
                    Ok(ac) => eprintln!(
                        "  access control: user={:?} resource={:?} mode={:?}",
                        ac.user(),
                        ac.resource(),
                        ac.mode()
                    ),
                    Err(e) => eprintln!("  access control: {e}"),
                }
            }

            // One live publisher per name: refuse the next caller that uses
            // it. Race window: a rule installed here, after `accept` has
            // returned, does not cover a second caller whose handshake
            // already completed in the meantime (the accept thread's wake-up
            // latency, normally well under a millisecond). That duplicate is
            // accepted; an application that must never serve two feeds
            // under one name keeps its own post-accept check as well.
            rules.set(id, ListenRule::Reject(RejectReason::Conflict))?;
        }

        let rules = rules.clone();
        let original = id.as_ref().and_then(|id| originals.get(id)).cloned();
        connection += 1;
        let path = env::temp_dir().join(format!(
            "srt_rules_{}_{connection}.ts",
            id.as_deref()
                .unwrap_or("anonymous")
                .replace(|c: char| !c.is_ascii_alphanumeric(), "_")
        ));
        workers.retain(|w: &thread::JoinHandle<()>| !w.is_finished());
        workers.push(thread::spawn(move || {
            let result = write_until_closed(&mut socket, &path);
            eprintln!("{peer}: {result:?}; wrote {}", path.display());
            // The name is free again once its publisher is gone. A name the
            // operator listed gets its original rule back; plain `remove`
            // would drop it and the default `Reject(NotFound)` would then
            // lock the publisher out for good. Only a name that had no rule
            // is removed.
            if let Some(id) = id {
                match original {
                    Some(rule) => {
                        let _ = rules.set(&id, rule);
                    }
                    None => {
                        rules.remove(&id);
                    }
                }
            }
        }));
    }
}

/// Copy every received message into `path` until the peer goes away.
fn write_until_closed(
    socket: &mut tst_srt::SrtSocket,
    path: &std::path::Path,
) -> Result<u64, Box<dyn std::error::Error>> {
    use std::io::Write;
    let mut out = std::fs::File::create(path)?;
    // Above the 1316-byte default SRT live payload, so one `recv` is one
    // whole message.
    let mut buf = [0u8; 1500];
    let mut total = 0u64;
    loop {
        match socket.recv(&mut buf) {
            Ok(n) => {
                out.write_all(&buf[..n])?;
                total += n as u64;
            }
            Err(RecvError::ConnectionBroken) => return Ok(total),
            Err(RecvError::TimedOut) => continue,
            Err(e) => return Err(Box::new(e)),
        }
    }
}
