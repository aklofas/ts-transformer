//! Live-socket corpus replay — every `tests/fixtures/local/*.ts` is
//! streamed through `pipeline::Sender` over an SRT loopback pair into
//! `pipeline::DemuxReceiver` and the receiver's demux events are compared
//! against a ground-truth count produced by feeding the same bytes
//! through a stand-alone `Demuxer` directly.
//!
//! Why this exists: `pipeline_receiver_live.rs` proves the wire format
//! survives a real handshake using *synthetic* NAL units + a hand-rolled
//! KLV blob. `mpegts_demux_local.rs` and `mpegts_mux_local.rs` exercise
//! the corpus but never go near a socket. This file is the third leg of
//! the triangle: real captures, real socket pair, end-to-end parity
//! check between an off-wire reference demux and the on-wire receive
//! pipeline.
//!
//! Silently passes when the fixtures dir is absent (CI case).
//!
//! Per-file flow:
//!
//!   1. Read up to `MAX_BYTES` of the `.ts` file. Capping keeps test
//!      runtime bounded — the corpus has 2 GB outliers and we don't
//!      need them transferred end-to-end to validate the pipeline.
//!   2. Reference pass: feed those bytes through a fresh `Demuxer`,
//!      flush, count `Sample{Video}` and `Metadata{Klv*}` events.
//!      A `DemuxError::Unrecoverable` here means "not really a TS
//!      capture" — skip with a warning, matching `mpegts_demux_local`.
//!   3. Loopback pass: `Loopback::bind_with` + `spawn_accept` host the
//!      recv pipeline on the accept thread; main thread connects and
//!      pushes the same buffer via `Sender::send_ts` in 64 KB chunks,
//!      then `flush` + close. Recv counts come back via an mpsc
//!      channel (not `accept.join()`'s return value) so the main
//!      thread can use `recv_timeout` as a hung-pipeline guard.
//!   4. Compare: assert ≥ 1 ProgramMap, and that recv-side video /
//!      KLV counts hit `≥ TOLERANCE * reference`. The slack absorbs
//!      benign edge cases (a trailing AU that reference flush
//!      catches but the loopback close races past, end-of-file PSI
//!      skew, etc.) without letting a real wire-side regression
//!      slip through.
//!
//! Linux x86_64 only — same gate as the other live-socket tests.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tst_core::error::DemuxError;
use tst_core::mpegts::demux::{DemuxEvent, Demuxer, MetadataKind, SamplePayload};
use tst_pipeline::{DemuxReceiver, DemuxReceiverErrorSource, Sender, SenderConfig, TransportError};
use tst_srt::SrtTransport;
use tst_srt::{ListenerBuilder, SocketBuilder};

/// Per-file byte cap. 16 MiB on a typical 8 Mbps capture is roughly 16
/// seconds of content — enough for a comfortably large PMT + many AUs +
/// many KLV records, while keeping each per-file roundtrip well under
/// a second on loopback.
const MAX_BYTES: usize = 16 * 1024 * 1024;

/// Send-side chunk granularity. Doesn't affect correctness — `Sender`
/// re-bundles internally to 7-packet UDP payloads regardless. Picked
/// large enough that we don't pay 100k+ syscalls per file.
const CHUNK: usize = 64 * 1024;

/// Loose tolerance on recv-side counts vs. the reference. Loopback SRT
/// in blocking mode shouldn't drop, but the close handshake can clip
/// the very last AU — we'd rather have a forgiving floor than a flaky
/// suite. A real wire-side regression would crater the count, not
/// nibble at the tail.
const TOLERANCE: f64 = 0.85;

/// Latency budget on both ends. Higher than `pipeline_receiver_live.rs`'s
/// 120 ms because we're pushing 16 MB bursts, not 15 frames — TSBPD
/// wants headroom or the recv side starts late-dropping.
const LATENCY: Duration = Duration::from_millis(500);

fn fixtures_dir() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("local");
    if p.exists() { Some(p) } else { None }
}

/// Truncate `bytes` to the largest prefix that ends on a TS packet
/// boundary aligned to the first 0x47 sync byte. Without this, a
/// mid-packet cut leaves the demuxer's HUNT/VERIFY/LOCKED state machine
/// resyncing inside the truncated tail and the reference vs. wire
/// counts can diverge on the final packet's events.
fn truncate_to_packet_boundary(bytes: &[u8]) -> &[u8] {
    let start = match bytes.iter().position(|&b| b == 0x47) {
        Some(s) => s,
        None => return &[],
    };
    let usable = &bytes[start..];
    let n = (usable.len() / 188) * 188;
    &usable[..n]
}

#[derive(Default, Debug)]
struct Counts {
    pmaps: usize,
    video: usize,
    klv: usize,
    nonconformant: usize,
}

/// Reference demux: feed once, flush, tally. Mirrors the event filter
/// used on the wire side so the two are directly comparable.
fn reference_counts(bytes: &[u8]) -> Result<Counts, DemuxError> {
    let mut d = Demuxer::new();
    d.feed(bytes)?;
    d.flush();
    let mut c = Counts::default();
    while let Some(e) = d.next_event() {
        match e {
            DemuxEvent::ProgramMap(_) => c.pmaps += 1,
            DemuxEvent::Sample {
                payload: SamplePayload::Video { .. },
                ..
            } => c.video += 1,
            DemuxEvent::Metadata {
                kind: MetadataKind::KlvSyncAuCell { .. } | MetadataKind::KlvAsync,
                ..
            } => c.klv += 1,
            DemuxEvent::NonConformant { .. } => c.nonconformant += 1,
            _ => {}
        }
    }
    Ok(c)
}

#[test]
fn corpus_replay_over_srt_loopback() {
    require_loopback!();
    let dir = match fixtures_dir() {
        Some(d) => d,
        None => {
            eprintln!("no local fixtures; skipping");
            return;
        }
    };

    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("read_dir on existing fixtures dir")
        .filter_map(|r| r.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("ts"))
        .collect();
    // Stable order so failures are reproducible across runs.
    paths.sort();

    let mut processed = 0usize;
    let mut skipped = 0usize;

    for path in &paths {
        match run_one(path) {
            RunOutcome::Ok => processed += 1,
            RunOutcome::SkippedNotTs => {
                eprintln!(
                    "{}: skipped — reference demux unrecoverable",
                    path.display()
                );
                skipped += 1;
            }
        }
    }

    eprintln!("loopback-replayed {processed} corpus file(s); skipped {skipped}");
}

enum RunOutcome {
    Ok,
    SkippedNotTs,
}

fn run_one(path: &Path) -> RunOutcome {
    // 1. Load + boundary-truncate.
    let raw = fs::read(path).expect("read corpus file");
    let capped = if raw.len() > MAX_BYTES {
        &raw[..MAX_BYTES]
    } else {
        &raw[..]
    };
    let aligned = truncate_to_packet_boundary(capped);
    if aligned.is_empty() {
        eprintln!("{}: empty after alignment, skipping", path.display());
        return RunOutcome::SkippedNotTs;
    }

    // 2. Reference counts. Unrecoverable here means "not a TS capture",
    // matching `mpegts_demux_local`.
    let reference = match reference_counts(aligned) {
        Ok(c) => c,
        Err(DemuxError::Unrecoverable { .. }) => return RunOutcome::SkippedNotTs,
        Err(e) => panic!("{}: reference demux failed: {e}", path.display()),
    };
    assert!(
        reference.pmaps >= 1,
        "{}: reference demux saw no PMT — fixture is malformed?",
        path.display()
    );
    assert!(
        reference.video >= 1,
        "{}: reference demux saw no video samples — fixture is malformed?",
        path.display()
    );

    // 3. Loopback. Recv pipeline runs in the accept-thread closure; main
    // thread owns the sender so the panic on a wire-side mismatch points
    // at the right file in the test report.
    let mut builder = ListenerBuilder::new();
    builder.recv_latency(LATENCY);
    let lb = crate::common::Loopback::bind_with(builder);
    let port = lb.port;

    let payload = aligned.to_vec();

    // Channel for the recv-side counts. Done this way (rather than
    // relying on `accept.join()`'s return value) so the main thread
    // can use `recv_timeout` as a hung-pipeline guard — `AcceptHandle::join`
    // blocks indefinitely.
    let (tx, rx) = mpsc::channel::<Counts>();

    let accept = lb.spawn_accept(move |server_socket| {
        let mut receiver = DemuxReceiver::new(SrtTransport::new(server_socket));
        let mut c = Counts::default();
        for item in &mut receiver {
            let event = match item {
                Ok(e) => e,
                // `Broken` = peer hangup. libsrt commonly surfaces a
                // sender close as Broken on the recv side; treat as
                // a clean stream end. Any other error is a real bug.
                Err(ref err)
                    if matches!(
                        err.source,
                        DemuxReceiverErrorSource::Transport(TransportError::Broken { .. })
                    ) =>
                {
                    break;
                }
                Err(other) => panic!("unexpected receiver error: {other:?}"),
            };
            match event {
                DemuxEvent::ProgramMap(_) => c.pmaps += 1,
                DemuxEvent::Sample {
                    payload: SamplePayload::Video { .. },
                    ..
                } => c.video += 1,
                DemuxEvent::Metadata {
                    kind: MetadataKind::KlvSyncAuCell { .. } | MetadataKind::KlvAsync,
                    ..
                } => c.klv += 1,
                DemuxEvent::NonConformant { .. } => c.nonconformant += 1,
                _ => {}
            }
        }
        let _ = tx.send(c);
    });
    accept.wait_ready();

    let socket = SocketBuilder::new()
        .latency(LATENCY)
        .connect(format!("127.0.0.1:{port}"))
        .expect("connect");
    let mut sender = Sender::new(SrtTransport::new(socket), SenderConfig::default());

    for chunk in payload.chunks(CHUNK) {
        sender.send_ts(chunk).expect("send_ts");
    }
    sender.flush().expect("flush");

    // Drain pause before close — SRT's send queue is async w.r.t.
    // close, and on 16 MB bursts the last few packets need a real
    // moment to clear TSBPD on the peer before we close. 1 s covers
    // the latency budget plus loopback transit on every platform.
    //
    // Not 500 ms: Darwin scheduling on Apple Silicon (macOS arm64)
    // needs more headroom for the corpus test's
    // burst pattern; the extra headroom is platform-stable.
    thread::sleep(Duration::from_secs(1));
    sender.close();

    // Wait for recv thread, with a generous timeout. If recv hangs
    // forever we want the test to fail with a clear message rather
    // than the harness 60s default.
    let wire = rx
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|e| panic!("{}: recv-side hung or panicked: {e}", path.display()));
    accept.join();

    // 4. Compare.
    let want_video = ((reference.video as f64) * TOLERANCE) as usize;
    let want_klv = ((reference.klv as f64) * TOLERANCE) as usize;

    eprintln!(
        "{}: ref(video={} klv={} nc={}) wire(pmap={} video={} klv={} nc={})",
        path.display(),
        reference.video,
        reference.klv,
        reference.nonconformant,
        wire.pmaps,
        wire.video,
        wire.klv,
        wire.nonconformant,
    );

    assert!(
        wire.pmaps >= 1,
        "{}: receiver observed no PMT (reference saw {})",
        path.display(),
        reference.pmaps
    );
    assert!(
        wire.video >= want_video,
        "{}: video count {} < {:.0}% of reference {} (= {})",
        path.display(),
        wire.video,
        TOLERANCE * 100.0,
        reference.video,
        want_video,
    );
    if reference.klv > 0 {
        assert!(
            wire.klv >= want_klv,
            "{}: klv count {} < {:.0}% of reference {} (= {})",
            path.display(),
            wire.klv,
            TOLERANCE * 100.0,
            reference.klv,
            want_klv,
        );
    }

    RunOutcome::Ok
}
