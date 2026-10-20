//! Socket tests for the `corpus` module family: the replay stage's child
//! processes, driven over their real stdin and stdout against loopback
//! sockets.

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

#[path = "corpus_support/mod.rs"]
mod support;
pub use support::{gen_profile_bytes, sha_of, temp_dir_named};

#[path = "net_corpus/receive.rs"]
mod receive;
#[path = "net_corpus/replay.rs"]
mod replay;

/// The longest any single read of a child's output may take before the test
/// fails: far above every rule the child enforces itself, so only a hang
/// reaches it.
pub const READ_LIMIT: Duration = Duration::from_secs(90);

/// A child's stdout read on its own thread, one line at a time, so a child
/// that stops answering fails the test instead of hanging it.
pub struct Lines {
    rx: Receiver<String>,
}

impl Lines {
    pub fn new(out: impl Read + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { rx }
    }

    /// The next line, `None` once the child closed its stdout. Panics after
    /// [`READ_LIMIT`].
    pub fn next(&self) -> Option<String> {
        match self.rx.recv_timeout(READ_LIMIT) {
            Ok(l) => Some(l),
            Err(RecvTimeoutError::Disconnected) => None,
            Err(RecvTimeoutError::Timeout) => {
                panic!("no line from the child within {READ_LIMIT:?}")
            }
        }
    }

    /// Every line left until the child closes its stdout, bounded the same way.
    pub fn rest(&self) -> String {
        let mut s = String::new();
        while let Some(l) = self.next() {
            s.push_str(&l);
            s.push('\n');
        }
        s
    }
}

/// Runs `cmd` to completion with its output captured; kills it and fails
/// the test when it outlives `limit`.
pub fn run_bounded(cmd: &mut Command, limit: Duration) -> Output {
    let mut child: Child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tst-interop");
    let out = Lines::new(child.stdout.take().unwrap());
    let err = Lines::new(child.stderr.take().unwrap());
    let until = Instant::now() + limit;
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the command outlived {limit:?}");
        }
        std::thread::park_timeout(Duration::from_millis(50));
    };
    Output {
        status,
        stdout: out.rest().into_bytes(),
        stderr: err.rest().into_bytes(),
    }
}
