//! `tst-interop corpus receive`: the line protocol over a real child
//! process, with the sender replaying a generated capture through loopback.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Instant;

use tst_core::transport::Transport;
use tst_interop::corpus::Sha256Hex;
use tst_interop::corpus::digest::{FileDigest, digest_file, file_name};
use tst_interop::corpus::pace::{PcrPacer, send_paced};
use tst_interop::transport::make_send;

use crate::{Lines, gen_profile_bytes, sha_of, temp_dir_named};

struct Receiver {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines,
    stderr: Lines,
    seen: String,
    out_file: PathBuf,
}

impl Receiver {
    fn spawn(path: &str, dir: &Path) -> Self {
        Self::spawn_with(path, dir, &[])
    }

    fn spawn_with(path: &str, dir: &Path, env: &[(&str, &str)]) -> Self {
        let out_file = dir.join("verdicts.jsonl");
        let mut child = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
            .args(["corpus", "receive", "--path", path, "--digests"])
            .arg(dir)
            .arg("--out")
            .arg(&out_file)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tst-interop corpus receive");
        let stdin = child.stdin.take().unwrap();
        let stdout = Lines::new(child.stdout.take().unwrap());
        let stderr = Lines::new(child.stderr.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            stderr,
            seen: String::new(),
            out_file,
        }
    }

    fn say(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn line(&mut self) -> String {
        let Some(l) = self.stdout.next() else {
            panic!("the child closed stdout early; saw: {}", self.seen);
        };
        self.seen.push_str(&l);
        self.seen.push('\n');
        l
    }

    fn finish(mut self) -> (std::process::ExitStatus, String, String) {
        drop(self.stdin);
        let rest = self.stdout.rest();
        self.seen.push_str(&rest);
        let err = self.stderr.rest();
        (self.child.wait().unwrap(), self.seen, err)
    }
}

/// A generated capture of `seconds`, its sweep digest (of the first
/// `prefix` seconds, from the file as the sweep reads it) stored in a fresh
/// directory under the name the child looks for (the prefix digest's own
/// name with a prefix).
fn capture(
    name: &str,
    seconds: f64,
    prefix: Option<u32>,
) -> (Vec<u8>, Sha256Hex, FileDigest, PathBuf) {
    let dir = temp_dir_named(name);
    let bytes = gen_profile_bytes("baseline", seconds);
    let sha = sha_of(&bytes);
    let file = dir.join("capture.ts");
    std::fs::write(&file, &bytes).unwrap();
    let digest = digest_file(Some(&sha), &file, prefix).unwrap();
    std::fs::remove_file(&file).unwrap();
    std::fs::write(
        dir.join(file_name(&sha, prefix)),
        serde_json::to_string(&digest).unwrap(),
    )
    .unwrap();
    (bytes, sha, digest, dir)
}

/// One message per at most `max_payload` bytes, a whole number of packets.
fn send_in_messages(t: &mut dyn Transport, bytes: &[u8]) -> Result<(), String> {
    let step = (t.max_payload() / 188).max(1) * 188;
    for piece in bytes.chunks(step) {
        t.send_bytes(piece).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The announced sender URL out of a `ready` line.
fn ready_url(line: &str, sha: &Sha256Hex) -> String {
    let rest = line
        .strip_prefix(&format!("ready {sha} "))
        .unwrap_or_else(|| panic!("not a ready line: {line}"));
    rest.to_string()
}

#[test]
fn receive_over_udp_passes_a_faithful_replay() {
    let (bytes, sha, _, dir) = capture("recv-udp", 2.0, None);
    let mut rx = Receiver::spawn("udp", &dir);
    rx.say(&format!("file {sha} none"));
    let url = ready_url(&rx.line(), &sha);
    assert!(url.starts_with("udp://127.0.0.1:"), "{url}");

    let mut sender = make_send(&url).unwrap();
    let pacer = PcrPacer::new(&bytes, None).unwrap();
    let start = Instant::now();
    send_paced(
        &pacer,
        &mut || start.elapsed(),
        &mut |d| std::thread::sleep(d),
        |chunk| send_in_messages(sender.as_mut(), chunk),
    )
    .unwrap();
    sender.close();
    rx.say(&format!("sent {sha}"));

    let verdict = rx.line();
    assert!(
        verdict.starts_with(&format!("verdict {sha} PASS ")),
        "{verdict}"
    );
    let record = std::fs::read_to_string(&rx.out_file).unwrap();
    assert!(record.contains("\"pass\":true"), "{record}");
    rx.say("quit");
    assert_eq!(rx.line(), "bye");
    let (status, _, _) = rx.finish();
    assert!(status.success());
}

#[test]
fn receive_over_tcp_fails_a_truncated_replay() {
    let (bytes, sha, _, dir) = capture("recv-tcp-cut", 2.0, None);
    let mut rx = Receiver::spawn("tcp", &dir);
    rx.say(&format!("file {sha} none"));
    let url = ready_url(&rx.line(), &sha);
    assert!(url.starts_with("tcp://127.0.0.1:"), "{url}");

    let mut sender = make_send(&url).unwrap();
    let pacer = PcrPacer::new(&bytes, None).unwrap();
    let half = pacer.chunks().count() / 2;
    for chunk in pacer.chunks().take(half) {
        send_in_messages(sender.as_mut(), chunk.bytes).unwrap();
    }
    sender.close();
    rx.say(&format!("sent {sha}"));

    let verdict = rx.line();
    assert!(
        verdict.starts_with(&format!("verdict {sha} FAIL ")),
        "{verdict}"
    );
    assert!(verdict.contains("delivery incomplete"), "{verdict}");
    rx.say("quit");
    assert_eq!(rx.line(), "bye");
}

#[test]
fn receive_over_tcp_passes_a_faithful_replay() {
    let (bytes, sha, _, dir) = capture("recv-tcp", 2.0, None);
    let mut rx = Receiver::spawn("tcp", &dir);
    rx.say(&format!("file {sha} none"));
    let url = ready_url(&rx.line(), &sha);

    let mut sender = make_send(&url).unwrap();
    let pacer = PcrPacer::new(&bytes, None).unwrap();
    let start = Instant::now();
    send_paced(
        &pacer,
        &mut || start.elapsed(),
        &mut |d| std::thread::sleep(d),
        |chunk| send_in_messages(sender.as_mut(), chunk),
    )
    .unwrap();
    sender.close();
    rx.say(&format!("sent {sha}"));
    let verdict = rx.line();
    assert!(
        verdict.starts_with(&format!("verdict {sha} PASS ")),
        "{verdict}"
    );
    rx.say("quit");
    assert_eq!(rx.line(), "bye");
}

#[test]
fn a_file_without_a_sweep_digest_fails_in_place_of_ready_and_names_no_path() {
    let dir = temp_dir_named("recv-nodigest");
    let sha = sha_of(b"nothing was swept");
    let mut rx = Receiver::spawn("udp", &dir);
    rx.say(&format!("file {sha} 600"));
    let verdict = rx.line();
    assert!(
        verdict.starts_with(&format!("verdict {sha} FAIL ")),
        "{verdict}"
    );
    assert!(verdict.contains("sweep digest"), "{verdict}");
    rx.say("quit");
    assert_eq!(rx.line(), "bye");
    let (status, seen, _) = rx.finish();
    assert!(status.success());
    assert!(!seen.contains(&dir.display().to_string()), "{seen}");
}

#[test]
fn receive_quits_cleanly_and_prints_no_path() {
    let dir = temp_dir_named("recv-quit");
    let mut rx = Receiver::spawn("udp", &dir);
    rx.say("quit");
    assert_eq!(rx.line(), "bye");
    let (status, seen, err) = rx.finish();
    assert_eq!(status.code(), Some(0), "{err}");
    let tmp = std::env::temp_dir().display().to_string();
    assert!(!seen.contains(&tmp), "stdout named a path: {seen}");
    assert!(!err.contains(&tmp), "stderr named a path: {err}");
}

#[test]
fn a_line_outside_the_protocol_exits_2_naming_only_its_kind() {
    let dir = temp_dir_named("recv-bad");
    let mut rx = Receiver::spawn("udp", &dir);
    rx.say("hello from the parent");
    let (status, seen, err) = rx.finish();
    assert_eq!(status.code(), Some(2));
    assert!(seen.is_empty(), "{seen}");
    assert!(err.contains("unrecognised line"), "{err}");
    assert!(!err.contains("hello"), "{err}");
}

#[test]
fn the_die_after_hook_exits_0_after_the_first_verdict() {
    let dir = temp_dir_named("recv-die");
    let sha = sha_of(b"still nothing swept");
    let out_file = dir.join("verdicts.jsonl");
    let mut child = Command::new(env!("CARGO_BIN_EXE_tst-interop"))
        .args(["corpus", "receive", "--path", "udp", "--digests"])
        .arg(&dir)
        .arg("--out")
        .arg(&out_file)
        .env("TST_CORPUS_TEST_RECEIVE_DIE_AFTER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.as_mut().unwrap(), "file {sha} none").unwrap();
    let first = Lines::new(child.stdout.take().unwrap())
        .next()
        .expect("a verdict line");
    assert!(
        first.starts_with(&format!("verdict {sha} FAIL ")),
        "{first}"
    );
    // stdin stays open: the child leaves on its own.
    assert!(child.wait().unwrap().success());
}

#[test]
fn every_path_binds_a_kernel_picked_port_and_announces_its_sender_url() {
    let expect = [
        ("udp", "udp://127.0.0.1:"),
        ("tcp", "tcp://127.0.0.1:"),
        ("srt", "srt://127.0.0.1:"),
        ("rist", "rist://127.0.0.1:"),
        ("rtsp-publish", "rtsp-publish://127.0.0.1:"),
    ];
    for (path, prefix) in expect {
        let (_, sha, _, dir) = capture(&format!("recv-ready-{path}"), 1.0, None);
        let mut rx = Receiver::spawn(path, &dir);
        rx.say(&format!("file {sha} none"));
        let url = ready_url(&rx.line(), &sha);
        assert!(url.starts_with(prefix), "{path}: {url}");
        let port: u16 = url[prefix.len()..]
            .split(['/', '?'])
            .next()
            .unwrap()
            .parse()
            .unwrap_or_else(|_| panic!("{path}: no port in {url}"));
        assert_ne!(port, 0, "{path}: {url}");
        rx.say("quit");
        assert_eq!(rx.line(), "bye", "{path}");
    }
}

#[test]
fn receive_over_srt_passes_a_faithful_replay() {
    let (bytes, sha, _, dir) = capture("recv-srt", 2.0, None);
    let mut rx = Receiver::spawn("srt", &dir);
    rx.say(&format!("file {sha} none"));
    let url = ready_url(&rx.line(), &sha);

    let mut sender = make_send(&url).unwrap();
    let pacer = PcrPacer::new(&bytes, None).unwrap();
    let start = Instant::now();
    send_paced(
        &pacer,
        &mut || start.elapsed(),
        &mut |d| std::thread::sleep(d),
        |chunk| send_in_messages(sender.as_mut(), chunk),
    )
    .unwrap();
    sender.close();
    drop(sender);
    rx.say(&format!("sent {sha}"));
    let verdict = rx.line();
    assert!(
        verdict.starts_with(&format!("verdict {sha} PASS ")),
        "{verdict}"
    );
    rx.say("quit");
    assert_eq!(rx.line(), "bye");
}

/// Replays the first `prefix` seconds the way production does: a pacer cut
/// at the prefix, `file <sha> <prefix>`, a digest made with the same prefix.
fn prefixed_replay(path: &str, name: &str) {
    let (bytes, sha, _, dir) = capture(name, 4.0, Some(1));
    let mut rx = Receiver::spawn(path, &dir);
    rx.say(&format!("file {sha} 1"));
    let url = ready_url(&rx.line(), &sha);
    let mut sender = make_send(&url).unwrap();
    let pacer = PcrPacer::new(&bytes, Some(1)).unwrap();
    let start = Instant::now();
    send_paced(
        &pacer,
        &mut || start.elapsed(),
        &mut |d| std::thread::sleep(d),
        |chunk| send_in_messages(sender.as_mut(), chunk),
    )
    .unwrap();
    sender.close();
    drop(sender);
    rx.say(&format!("sent {sha}"));
    let verdict = rx.line();
    assert!(
        verdict.starts_with(&format!("verdict {sha} PASS ")),
        "{verdict}"
    );
    rx.say("quit");
    assert_eq!(rx.line(), "bye");
}

#[test]
fn a_prefixed_replay_over_udp_passes() {
    prefixed_replay("udp", "recv-prefix-udp");
}

#[test]
fn a_prefixed_replay_over_tcp_passes() {
    prefixed_replay("tcp", "recv-prefix-tcp");
}

#[test]
fn replaying_the_whole_capture_against_a_prefix_digest_fails() {
    let (bytes, sha, _, dir) = capture("recv-prefix-overshoot", 4.0, Some(1));
    let mut rx = Receiver::spawn("tcp", &dir);
    rx.say(&format!("file {sha} 1"));
    let url = ready_url(&rx.line(), &sha);
    let mut sender = make_send(&url).unwrap();
    send_in_messages(sender.as_mut(), &bytes).unwrap();
    sender.close();
    drop(sender);
    rx.say(&format!("sent {sha}"));
    let verdict = rx.line();
    assert!(
        verdict.starts_with(&format!("verdict {sha} FAIL ")),
        "{verdict}"
    );
}

const FAST_STALL: [(&str, &str); 1] = [("TST_CORPUS_TEST_STALL_SECONDS", "1")];

#[test]
fn a_receiver_that_never_gets_a_byte_stalls() {
    let (_, sha, _, dir) = capture("recv-stall-silent", 1.0, None);
    let mut rx = Receiver::spawn_with("udp", &dir, &FAST_STALL);
    rx.say(&format!("file {sha} none"));
    ready_url(&rx.line(), &sha);
    let verdict = rx.line();
    assert!(
        verdict.starts_with(&format!("verdict {sha} FAIL ")),
        "{verdict}"
    );
    assert!(verdict.contains("receiver stalled"), "{verdict}");
    rx.say("quit");
    assert_eq!(rx.line(), "bye");
}

#[test]
fn a_sender_that_goes_quiet_before_sent_stalls_the_receiver() {
    let (bytes, sha, _, dir) = capture("recv-stall-half", 2.0, None);
    let mut rx = Receiver::spawn_with("udp", &dir, &FAST_STALL);
    rx.say(&format!("file {sha} none"));
    let url = ready_url(&rx.line(), &sha);
    let mut sender = make_send(&url).unwrap();
    send_in_messages(sender.as_mut(), &bytes[..bytes.len() / 2 / 188 * 188]).unwrap();
    // No `sent`: the sender is still "alive" and silent.
    let verdict = rx.line();
    assert!(verdict.contains("receiver stalled"), "{verdict}");
    drop(sender);
    rx.say("quit");
    assert_eq!(rx.line(), "bye");
}

#[test]
fn a_closing_path_whose_sender_never_closes_stalls_after_sent() {
    let (bytes, sha, _, dir) = capture("recv-stall-open", 2.0, None);
    let mut rx = Receiver::spawn_with("tcp", &dir, &FAST_STALL);
    rx.say(&format!("file {sha} none"));
    let url = ready_url(&rx.line(), &sha);
    let mut sender = make_send(&url).unwrap();
    send_in_messages(sender.as_mut(), &bytes).unwrap();
    rx.say(&format!("sent {sha}"));
    let verdict = rx.line();
    assert!(verdict.contains("receiver stalled"), "{verdict}");
    drop(sender);
    rx.say("quit");
    assert_eq!(rx.line(), "bye");
}

#[test]
fn quitting_during_an_srt_accept_ends_the_child() {
    let (_, sha, _, dir) = capture("recv-srt-quit", 1.0, None);
    let mut rx = Receiver::spawn("srt", &dir);
    rx.say(&format!("file {sha} none"));
    ready_url(&rx.line(), &sha);
    rx.say("quit");
    assert_eq!(rx.line(), "bye");
    let (status, _, _) = rx.finish();
    assert!(status.success());
}
