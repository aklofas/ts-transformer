//! Rust adapter integration tests for the cross-binding scenario harness.
//!
//! For each entry in `tests/fixtures/scenarios/scenarios.toml`:
//!  - Run the committed input artifact through tst-core (demux/mux per
//!    `kind`).
//!  - Normalise the result to a `Golden` using the Step-2 semantics.
//!  - Assert `struct PartialEq` equality against the committed `golden.json`.
//!
//! The tests fail loudly if any committed golden is stale or if the
//! normaliser output deviates from what was committed.

use std::path::PathBuf;

use serde::Deserialize;

use tst_integration::scenarios::demux_to_core_events;
use tst_integration::scenarios::golden::{CoreEvent, Golden};

/// Path to the committed scenario fixtures.
fn fixtures_dir() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is the crate root; fixtures live under tests/.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("scenarios")
}

// ─────────────────────────────────────────────────────────────────────────────
// Manifest deserialization
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ScenariosManifest {
    scenario: Vec<ScenarioEntry>,
}

#[derive(Deserialize)]
struct ScenarioEntry {
    id: String,
    kind: String,
    input: String,
    golden: String,
    #[allow(dead_code)]
    features: Vec<String>,
    #[allow(dead_code)]
    tier: String,
    #[allow(dead_code)]
    schema_version: u32,
}

fn load_manifest() -> ScenariosManifest {
    let path = fixtures_dir().join("scenarios.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read scenarios.toml at {}: {e}", path.display()));
    toml::from_str(&text).unwrap_or_else(|e| panic!("cannot parse scenarios.toml: {e}"))
}

fn load_golden(golden_rel: &str) -> Golden {
    let path = fixtures_dir().join(golden_rel);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("cannot parse {}: {e}", path.display()))
}

fn load_input(input_rel: &str) -> Vec<u8> {
    let path = fixtures_dir().join(input_rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-kind adapters
// ─────────────────────────────────────────────────────────────────────────────

/// Run a `demux` scenario: feed the input TS through tst-core and collect
/// normalised `CoreEvent`s.  The shared normaliser surfaces NonConformant
/// events as `CoreEvent::Error` alongside media events, so malformed-input
/// `demux` scenarios assert their diagnostic codes through this same path.
fn run_demux(input: &[u8]) -> Vec<CoreEvent> {
    demux_to_core_events(input)
}

/// Run a `roundtrip` scenario: re-run the generator's single-source-of-truth
/// mux recipe and assert byte-identity against the committed artifact.
///
/// The roundtrip golden has `core: []` and an `extensions.output_sha256` hex
/// digest of the whole TS output. This asserts:
///  1. The freshly-muxed bytes are byte-identical to the committed `output.ts`.
///  2. Their sha256 equals the golden's `extensions.output_sha256`.
///
/// `committed_output_ts` is the committed `output.ts` artifact (the scenario's
/// `input`). Returns `core: []` (roundtrip carries no media events).
///
/// Dispatches on the scenario `id` so each roundtrip scenario re-runs its own
/// single-source-of-truth mux recipe. The `_ => panic!` arm forces any future
/// roundtrip scenario to add an explicit recipe binding here.
fn run_roundtrip(id: &str, committed_output_ts: &[u8], golden: &Golden) -> Vec<CoreEvent> {
    use sha2::{Digest, Sha256};
    use tst_integration::scenarios::{
        audio_klv_roundtrip_ts_bytes, video_dts_roundtrip_ts_bytes, video_misp_roundtrip_ts_bytes,
        video_roundtrip_ts_bytes,
    };

    // Re-run the generator's exact recipe — no hand-retyped mux.
    let fresh = match id {
        "video-roundtrip" => video_roundtrip_ts_bytes(),
        "audio-klv-roundtrip" => audio_klv_roundtrip_ts_bytes(),
        "video-dts-roundtrip" => video_dts_roundtrip_ts_bytes(),
        "video-misp-roundtrip" => video_misp_roundtrip_ts_bytes(),
        other => panic!("unknown roundtrip scenario: {other}"),
    };

    // 1. Byte-identity against the committed artifact.
    assert_eq!(
        fresh, committed_output_ts,
        "roundtrip TS output changed: fresh mux bytes differ from committed output.ts"
    );

    // 2. sha256 against the golden's stored output digest.
    let digest: String = {
        use std::fmt::Write;
        Sha256::digest(&fresh)
            .iter()
            .fold(String::new(), |mut s, b| {
                write!(s, "{b:02x}").unwrap();
                s
            })
    };
    let expected = golden
        .extensions
        .get("output_sha256")
        .and_then(|v| v.as_str())
        .expect("roundtrip golden must carry extensions.output_sha256");
    assert_eq!(
        digest, expected,
        "roundtrip sha256 mismatch: TS output changed"
    );

    // 3. For video-misp-roundtrip: demux the TS, extract the MISP timestamp,
    //    and assert it matches the golden's committed extract fields.
    if id == "video-misp-roundtrip" {
        use tst_core::codec::misp_time::{MispTimeKind, extract as misp_extract};
        use tst_core::mpegts::demux::event::SamplePayload;
        use tst_core::mpegts::demux::{DemuxEvent, Demuxer};
        use tst_core::mpegts::mux::VideoCodec;

        let mut demuxer = Demuxer::new();
        demuxer.feed(&fresh).expect("misp demux feed");
        demuxer.flush();
        let mut found = false;
        while let Some(event) = demuxer.next_event() {
            if let DemuxEvent::Sample {
                payload: SamplePayload::Video { raw, .. },
                ..
            } = event
            {
                let extracted = misp_extract(&raw, VideoCodec::H264)
                    .expect("misp_extract error")
                    .expect("no MISP timestamp found in demuxed video AU");

                let kind_u8 = match extracted.kind {
                    MispTimeKind::Micro => 0u8,
                    MispTimeKind::Nano => 1u8,
                    _ => panic!("unknown MispTimeKind variant"),
                };

                let ext = &golden.extensions;
                let golden_kind = ext["misp_kind"]
                    .as_u64()
                    .expect("golden misp_kind must be u64") as u8;
                let golden_status = ext["misp_time_status"]
                    .as_u64()
                    .expect("golden misp_time_status must be u64")
                    as u8;
                let golden_value = ext["misp_value"]
                    .as_u64()
                    .expect("golden misp_value must be u64");

                assert_eq!(kind_u8, golden_kind, "misp kind mismatch");
                assert_eq!(
                    extracted.time_status, golden_status,
                    "misp time_status mismatch"
                );
                assert_eq!(extracted.value, golden_value, "misp value mismatch");
                found = true;
                break;
            }
        }
        assert!(found, "video-misp-roundtrip: no video AU found after demux");
    }

    // Roundtrip scenarios carry no media events.
    vec![]
}

/// Run a `binding_contract` scenario: exercise the non-media contract.
///
/// For `strict-rejection`: feed garbage bytes to a demuxer and assert the
/// error maps to the stable public code `STRICT_REJECTION`.  Also verify that
/// dropping the demuxer after an error is idempotent (no panic).
///
/// For `malformed-psi-strict`: feed a TS with a valid PAT but corrupted PMT
/// CRC to a `StrictMode::Full` demuxer. The `PsiChecksumMismatch` NonConformant
/// escalates to `DemuxError::StrictRejection`, which maps to `"STRICT_REJECTION"`.
///
/// For the lifecycle contracts (`drop-idempotence`, `forged-handle`,
/// `exception-kind-stability`) the `CoreEvent::Error` envelope carries a
/// contract sentinel code rather than a demux error; each arm asserts the
/// nearest honest pure-Rust guarantee and emits the sentinel. The
/// `extensions.contract` tag (verified by the caller via the committed golden)
/// names the contract. Some contracts are primarily exercised by the C
/// adapter — see the per-arm comments for what is deferred and why.
fn run_binding_contract(id: &str, input: &[u8]) -> Vec<CoreEvent> {
    use tst_core::mpegts::demux::Demuxer;
    use tst_integration::scenarios::demux_error_code_pub;

    match id {
        "strict-rejection" => {
            // Garbage bytes: no 0x47 sync byte → Unrecoverable → STRICT_REJECTION.
            let mut demuxer = Demuxer::new();
            let result = demuxer.feed(input);
            let code = match &result {
                Err(e) => demux_error_code_pub(e),
                Ok(()) => panic!("expected feed to return Err on garbage input, got Ok"),
            };
            drop(demuxer);
            vec![CoreEvent::Error { code }]
        }
        "malformed-psi-strict" => {
            use tst_core::mpegts::demux::{Demuxer, DemuxerConfig, StrictMode};
            // Use StrictMode::Full so PsiChecksumMismatch → StrictRejection.
            let mut demuxer =
                Demuxer::with_config(DemuxerConfig::builder().strict(StrictMode::Full).build());
            let result = demuxer.feed(input);
            let code = match &result {
                Err(e) => demux_error_code_pub(e),
                Ok(()) => {
                    // Unreachable for this input — the PMT CRC mismatch is
                    // detected within feed()'s packet loop, which always
                    // returns Err(StrictRejection) before returning Ok. A
                    // reached Ok means the malformation didn't trigger rejection.
                    panic!(
                        "malformed-psi-strict: expected StrictRejection error from feed, got Ok"
                    );
                }
            };
            drop(demuxer);
            vec![CoreEvent::Error { code }]
        }
        "drop-idempotence" => {
            // The pure-Rust `Demuxer` has no explicit `close()` — it relies on
            // `Drop`. "Double close" is expressed as: feed, then `flush()` (the
            // explicit end-of-stream finaliser) TWICE, then drop. The nearest
            // real guarantee is that a fresh `Demuxer` constructed afterwards
            // also works. None of this panics — Rust's ownership makes it safe
            // by construction. The binding-specific double-`close()` teeth (a C
            // `tst_demuxer_close` called twice must not double-free) live in the
            // C/Python adapters.
            let mut demuxer = Demuxer::new();
            demuxer
                .feed(input)
                .expect("minimal valid TS should feed cleanly");
            demuxer.flush();
            demuxer.flush(); // second "close" — must be a safe no-op.
            drop(demuxer);
            // A fresh instance still works after the prior was finalised+dropped.
            let mut fresh = Demuxer::new();
            fresh
                .feed(input)
                .expect("fresh demuxer works after prior drop");
            fresh.flush();
            // `fresh` drops implicitly at end of scope.
            vec![CoreEvent::Error {
                code: "DOUBLE_CLOSE_OK".to_string(),
            }]
        }
        "forged-handle" => {
            // Approach (a): a real pure-Rust trust-boundary guard exists.
            // `VideoStreamHandle::try_from_raw` rejects a forged `u32` whose
            // bits fall outside the documented 8-bit packed layout. The forged
            // value lives in the committed input artifact (4 LE bytes) so the
            // C/Python adapters reject the identical value. The raw-POINTER
            // deref teeth (a forged opaque pointer must not be dereferenced)
            // remain a C-adapter concern — pure Rust has no raw
            // handles, but the integer-rewrap guard is a genuine equivalent.
            use tst_core::mpegts::mux::VideoStreamHandle;
            use tst_integration::scenarios::FORGED_HANDLE_RAW;

            // The committed artifact must carry exactly the forged value we
            // assert against — keeps the cross-binding input single-sourced.
            assert_eq!(
                input.len(),
                4,
                "forged-handle input must be a 4-byte LE u32"
            );
            let from_artifact = u32::from_le_bytes([input[0], input[1], input[2], input[3]]);
            assert_eq!(
                from_artifact, FORGED_HANDLE_RAW,
                "forged-handle artifact value drifted from the constant"
            );

            let rejected = VideoStreamHandle::try_from_raw(from_artifact).is_err();
            assert!(
                rejected,
                "forged handle {from_artifact:#x} must be rejected by try_from_raw"
            );
            vec![CoreEvent::Error {
                code: "INVALID_HANDLE".to_string(),
            }]
        }
        "exception-kind-stability" => {
            // Same malformation + mechanism as malformed-psi-strict, asserting
            // the SAME stable public code surfaces here. PsiChecksumMismatch
            // under StrictMode::Full → StrictRejection → "STRICT_REJECTION".
            use tst_core::mpegts::demux::{Demuxer, DemuxerConfig, StrictMode};
            let mut demuxer =
                Demuxer::with_config(DemuxerConfig::builder().strict(StrictMode::Full).build());
            let code = match &demuxer.feed(input) {
                Err(e) => demux_error_code_pub(e),
                Ok(()) => panic!(
                    "exception-kind-stability: expected StrictRejection error from feed, got Ok"
                ),
            };
            drop(demuxer);
            vec![CoreEvent::Error { code }]
        }
        "cancelled-recv-kind" => {
            // One cancelled plain SRT receive; the kind projected by the shared
            // binding layer is the golden's code.
            // Choreography: listener on this thread, idle caller on a helper
            // thread, the recv parked on a worker thread, the cancel fired from
            // here. Every wait is bounded (10 s) and FAILS; the worker is never
            // joined after a failed verdict, so a regression reports instead of
            // hanging.
            use std::sync::mpsc;
            use std::time::Duration;
            use tst_core::transport::{RecvTransport, TransportError};
            use tst_pipeline::binding::BindingError;
            use tst_srt::{SrtListenerBuilder, SrtSocketBuilder, SrtTransport};

            let mut listener = SrtListenerBuilder::new()
                .recv_timeout(Duration::from_secs(5))
                .send_timeout(Duration::from_secs(5))
                .bind("127.0.0.1:0")
                .expect(
                    "cancelled-recv-kind: bind 127.0.0.1:0 (loopback is required, not optional)",
                );
            let port = listener.local_addr().expect("local_addr").port();
            let peer = std::thread::spawn(move || {
                SrtSocketBuilder::new()
                    .send_timeout(Duration::from_secs(5))
                    .connect(format!("127.0.0.1:{port}"))
                    .expect("idle peer connect")
            });
            let (accepted, _) = listener.accept().expect("accept");
            let peer = peer.join().expect("peer thread");
            let mut rx = SrtTransport::new(accepted);
            let cancel =
                RecvTransport::cancel_handle(&rx).expect("SrtTransport has a cancel handle");

            let (tx, result_rx) = mpsc::channel::<Result<usize, TransportError>>();
            let (entered_tx, entered_rx) = mpsc::channel::<()>();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; RecvTransport::max_payload(&rx)];
                let _ = entered_tx.send(());
                let r = loop {
                    match rx.recv_bytes(&mut buf) {
                        Err(TransportError::Backpressure { .. }) => continue,
                        other => break other,
                    }
                };
                let _ = tx.send(r);
            });
            entered_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("cancelled-recv-kind: recv worker never started");
            std::thread::sleep(Duration::from_millis(200)); // let it reach srt_recv
            cancel.cancel();
            let result = result_rx.recv_timeout(Duration::from_secs(10)).expect(
                "cancelled-recv-kind: the parked recv did not return within 10 s of cancel()",
            );
            drop(peer);
            let err = result.expect_err("a cancelled recv must not succeed");
            let projected = BindingError::from(err);
            assert_eq!(
                projected.detail,
                "cancelled from another thread",
                "cancelled-recv-kind: detail mismatch (kind {})",
                projected.kind.name()
            );
            vec![CoreEvent::Error {
                code: projected.kind.name().to_string(),
            }]
        }
        "peer-reset-recv-kind" => {
            // One plain TCP receive whose peer resets the connection; the kind
            // projected by the shared binding layer is the golden's code.
            // Choreography: the peer listener on this thread writes the
            // artifact and closes with SO_LINGER zero (an RST), the receive
            // runs on a worker until its first error, and a deadline that
            // misses cancels a pre-obtained handle and FAILS. The worker is
            // never joined after a failed verdict, so a regression reports
            // instead of hanging.
            use std::io::Write;
            use std::net::TcpListener;
            use std::sync::mpsc;
            use std::time::Duration;
            use tst_core::transport::{BrokenCause, RecvTransport, TransportError};
            use tst_pipeline::binding::BindingError;
            use tst_test_helpers::timing::scaled;

            let listener = TcpListener::bind("127.0.0.1:0").expect(
                "peer-reset-recv-kind: bind 127.0.0.1:0 (loopback is required, not optional)",
            );
            let port = listener.local_addr().expect("local_addr").port();
            let opened = tst_tcp::opener::open_recv(&format!("tcp://127.0.0.1:{port}"))
                .expect("peer-reset-recv-kind: open the plain TCP receiver");
            let cancel = opened.cancel.clone();
            let (mut conn, _) = listener.accept().expect("accept");
            conn.write_all(input).expect("peer write");
            socket2::SockRef::from(&conn)
                .set_linger(Some(Duration::ZERO))
                .expect("SO_LINGER zero");
            drop(conn); // RST

            let mut rx = opened.transport;
            let (tx, result_rx) = mpsc::channel::<TransportError>();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; rx.max_payload()];
                let err = loop {
                    match rx.recv_bytes(&mut buf) {
                        Ok(_) | Err(TransportError::Backpressure { .. }) => continue,
                        Err(e) => break e,
                    }
                };
                let _ = tx.send(err);
            });
            let err = match result_rx.recv_timeout(scaled(Duration::from_secs(10))) {
                Ok(err) => err,
                Err(_) => {
                    cancel.cancel();
                    panic!(
                        "peer-reset-recv-kind: the receive did not fail within 10 s of the reset"
                    );
                }
            };
            // tst-tcp reports a clean FIN as `Broken { cause: CleanEof }` too, and
            // the transport-level projection below names both BROKEN; only a
            // reset may pass.
            match &err {
                TransportError::Broken { cause, .. } if *cause != BrokenCause::CleanEof => {}
                other => panic!("peer-reset-recv-kind: expected a reset, got {other:?}"),
            }
            let projected = BindingError::from(err);
            vec![CoreEvent::Error {
                code: projected.kind.name().to_string(),
            }]
        }
        other => panic!("unknown binding_contract scenario: {other}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Main test
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn all_scenarios_match_committed_goldens() {
    let manifest = load_manifest();
    assert!(
        !manifest.scenario.is_empty(),
        "scenarios.toml must have at least one entry"
    );

    for entry in &manifest.scenario {
        let committed = load_golden(&entry.golden);
        let input = load_input(&entry.input);

        let observed_core: Vec<CoreEvent> = match entry.kind.as_str() {
            "demux" => run_demux(&input),
            "roundtrip" => run_roundtrip(&entry.id, &input, &committed),
            "binding_contract" => run_binding_contract(&entry.id, &input),
            "url-corpus" => {
                url_corpus_leg::run();
                vec![]
            }
            other => panic!("unknown scenario kind '{}' in manifest", other),
        };

        let observed = Golden {
            schema_version: committed.schema_version,
            lossy: committed.lossy,
            core: observed_core,
            extensions: committed.extensions.clone(),
        };

        assert_eq!(
            observed, committed,
            "scenario '{}': observed golden differs from committed",
            entry.id
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// URL corpus: the Rust leg
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn url_corpus_rows_match_their_records() {
    url_corpus_leg::run();
}

#[test]
fn url_corpus_covers_every_documented_key() {
    url_corpus_leg::check_coverage();
}

mod url_corpus_leg {
    use tst_integration::scenarios::url::{self, Record, Row};

    /// Parse every row through its scheme's parser and compare the record.
    pub(super) fn run() {
        let corpus = url::load();
        let mut mismatches = Vec::new();
        for row in &corpus.url {
            let observed = observe(row);
            if observed != row.expect {
                mismatches.push(format!(
                    "{}\n    expected {:?}\n    observed {:?}",
                    row.label(),
                    row.expect,
                    observed
                ));
            }
        }
        assert!(
            mismatches.is_empty(),
            "{} of {} URL corpus rows differ from their records:\n{}",
            mismatches.len(),
            corpus.url.len(),
            mismatches.join("\n")
        );
    }

    use std::collections::BTreeMap;
    use std::time::Duration;

    use tst_core::url::query::{KeyDoc, docs};
    use tst_pipeline::binding::{BindingError, BindingErrorKind};

    type Keys = BTreeMap<String, String>;

    fn record(scheme: &str, host: &str, port: u16, keys: Keys) -> Record {
        Record {
            scheme: Some(scheme.to_string()),
            host: Some(host.to_string()),
            port: Some(port),
            keys: Some(keys),
            ..Record::default()
        }
    }

    fn put(keys: &mut Keys, key: &str, value: impl ToString) {
        keys.insert(key.to_string(), value.to_string());
    }

    fn put_opt(keys: &mut Keys, key: &str, value: Option<impl ToString>) {
        if let Some(v) = value {
            put(keys, key, v);
        }
    }

    fn ms(d: Duration) -> u128 {
        d.as_millis()
    }

    fn bit(b: bool) -> &'static str {
        if b { "1" } else { "0" }
    }

    fn refused(kind: BindingErrorKind) -> Record {
        Record::error(kind)
    }

    /// The row's outcome through its scheme's parser; for `stage = "open"`
    /// the parse must succeed and the role's opener decides.
    fn observe(row: &Row) -> Record {
        let parsed = match row.family() {
            "srt" => observe_srt(&row.url),
            "rtp" => observe_rtp(&row.url),
            "rtsp" => observe_rtsp(&row.url),
            "udp" => observe_udp(&row.url),
            "tcp" => observe_tcp(&row.url),
            "hls" => observe_hls(&row.url),
            "rist" => observe_rist(&row.url),
            "whep" => observe_whep(&row.url),
            other => panic!("no Rust parser for {other}"),
        };
        if row.stage.as_deref() != Some("open") {
            return parsed;
        }
        assert!(
            !parsed.is_error(),
            "{}: a stage = \"open\" row must parse, got {parsed:?}",
            row.label()
        );
        match open(row) {
            Ok(()) => parsed,
            Err(e) => refused(e.kind),
        }
    }

    /// The role's shared opener, for the role checks that run at open.
    fn open(row: &Row) -> Result<(), BindingError> {
        match (row.family(), row.role.as_str()) {
            ("rtp", "send") => tst_rtp::opener::open_sender(&row.url).map(drop),
            ("rtp", "recv") => tst_rtp::opener::open_recv(&row.url).map(drop),
            ("udp", "recv") => tst_udp::opener::open_recv(&row.url).map(drop),
            (f, r) => panic!("{}: no opener for {f} {r}", row.label()),
        }
    }

    fn observe_srt(url: &str) -> Record {
        use tst_srt::options::{Congestion, KeyLength, MaxBandwidth};
        use tst_srt::url::Mode;
        let u = match tst_srt::SrtUrl::parse(url) {
            Ok(u) => u,
            Err(e) => return refused(BindingError::from(e).kind),
        };
        let c = &u.config;
        let mut k = Keys::new();
        put(
            &mut k,
            "mode",
            match u.mode {
                Mode::Caller => "caller",
                Mode::Listener => "listener",
            },
        );
        put_opt(
            &mut k,
            "congestion",
            c.congestion.map(|v| match v {
                Congestion::Live => "live",
                Congestion::File => "file",
            }),
        );
        put_opt(&mut k, "conntimeo", c.connect_timeout.map(ms));
        put_opt(&mut k, "fc", c.flow_window_packets);
        put_opt(&mut k, "inputbw", c.input_bandwidth);
        put_opt(&mut k, "latency", c.latency.map(ms));
        put_opt(&mut k, "linger", c.linger.map(|d| d.as_secs()));
        put_opt(&mut k, "lossmaxttl", c.loss_max_ttl);
        put_opt(
            &mut k,
            "maxbw",
            c.max_bandwidth.map(|v| match v {
                MaxBandwidth::Limited(n) => n.to_string(),
                other => format!("{other:?}"),
            }),
        );
        put_opt(&mut k, "mss", c.mss);
        put_opt(&mut k, "oheadbw", c.overhead_bandwidth_pct);
        put_opt(
            &mut k,
            "packetfilter",
            c.packet_filter.as_ref().map(|v| v.as_str()),
        );
        put_opt(
            &mut k,
            "passphrase",
            c.passphrase.as_ref().map(|v| v.as_str()),
        );
        put_opt(&mut k, "payloadsize", c.payload_size);
        put_opt(
            &mut k,
            "pbkeylen",
            c.key_length.map(|v| match v {
                KeyLength::Aes128 => "16",
                KeyLength::Aes192 => "24",
                KeyLength::Aes256 => "32",
            }),
        );
        put_opt(&mut k, "peerlatency", c.peer_latency.map(ms));
        put_opt(&mut k, "rcvlatency", c.recv_latency.map(ms));
        put_opt(&mut k, "streamid", c.stream_id.as_ref().map(|v| v.as_str()));
        put_opt(&mut k, "tlpktdrop", c.too_late_packet_drop.map(bit));
        put_opt(&mut k, "udprcvbuf", c.udp_recv_buffer_bytes);
        put_opt(&mut k, "udpsndbuf", c.udp_send_buffer_bytes);
        put_opt(&mut k, "x-recvtimeout", c.recv_timeout.map(ms));
        put_opt(&mut k, "x-sendtimeout", c.send_timeout.map(ms));
        record("srt", &u.host, u.port, k)
    }

    fn observe_rtp(url: &str) -> Record {
        let u = match tst_rtp::RtpUrl::parse(url) {
            Ok(u) => u,
            Err(e) => {
                return refused(BindingError::from(tst_rtp::RtpConnectError::Url(e)).kind);
            }
        };
        let mut k = Keys::new();
        put_opt(&mut k, "ttl", u.ttl);
        put_opt(&mut k, "iface", u.iface.as_deref());
        put_opt(&mut k, "pkt_size", u.pkt_size);
        put_opt(&mut k, "ssrc", u.ssrc);
        put_opt(&mut k, "pt", u.pt);
        put_opt(&mut k, "recv_timeout", u.recv_timeout.map(ms));
        put_opt(&mut k, "rcvbuf", u.rcvbuf);
        put_opt(&mut k, "sndbuf", u.sndbuf);
        put_opt(&mut k, "source", u.source);
        record("rtp", &u.host, u.port, k)
    }

    fn observe_rtsp(url: &str) -> Record {
        use tst_rtp::{RtspScheme, RtspTransportPref, RtspVersion};
        let u = match tst_rtp::RtspUrl::parse(url) {
            Ok(u) => u,
            Err(e) => return refused(BindingError::from(tst_rtp::RtspError::Url(e)).kind),
        };
        let scheme = match u.scheme() {
            RtspScheme::Rtsp => "rtsp",
            RtspScheme::Rtsps => "rtsps",
            _ => "other",
        };
        let mut k = Keys::new();
        put(
            &mut k,
            "rtsp_version",
            match u.rtsp_version {
                RtspVersion::V1_0 => "1.0",
                RtspVersion::V2_0 => "2.0",
                _ => "other",
            },
        );
        // Prefer-UDP is the absent key's behaviour; no URL value spells it.
        put_opt(
            &mut k,
            "transport",
            match u.transport_preference {
                RtspTransportPref::PreferUdp => None,
                RtspTransportPref::ForceUdp => Some("udp"),
                RtspTransportPref::ForceTcp => Some("tcp"),
                _ => Some("other"),
            },
        );
        put_opt(
            &mut k,
            "tcp_keepalive",
            u.tcp_keepalive.map(|d| d.as_secs()),
        );
        put_opt(&mut k, "recv_timeout", u.recv_timeout.map(ms));
        Record {
            path: Some(u.path.clone()),
            username: u.username.clone(),
            password_set: u.username.as_ref().map(|_| u.password.is_some()),
            ..record(scheme, &u.host, u.port, k)
        }
    }

    fn observe_udp(url: &str) -> Record {
        let u = match tst_udp::UdpUrl::parse(url) {
            Ok(u) => u,
            Err(e) => return refused(BindingError::from(tst_udp::UdpError::Url(e)).kind),
        };
        let c = &u.config;
        let mut k = Keys::new();
        put_opt(&mut k, "iface", c.iface.as_deref());
        put_opt(&mut k, "localaddr", c.localaddr);
        put_opt(&mut k, "ttl", c.ttl);
        put_opt(&mut k, "tos", c.tos);
        put_opt(&mut k, "rcvbuf", c.rcvbuf);
        put_opt(&mut k, "sndbuf", c.sndbuf);
        put_opt(&mut k, "pkt_size", c.pkt_size);
        Record {
            bind: u.recv_bind.then_some(true),
            ..record("udp", &u.addr.to_string(), u.port, k)
        }
    }

    fn observe_tcp(url: &str) -> Record {
        let u = match tst_tcp::url::TcpUrl::parse(url) {
            Ok(u) => u,
            Err(e) => return refused(BindingError::from(tst_tcp::error::TcpError::Url(e)).kind),
        };
        let c = &u.config;
        let path = |p: &Option<std::path::PathBuf>| p.as_ref().map(|p| p.display().to_string());
        let mut k = Keys::new();
        put(&mut k, "listen", bit(u.listen));
        put_opt(&mut k, "nodelay", c.nodelay.map(bit));
        put_opt(&mut k, "keepalive", c.keepalive.map(|d| d.as_secs()));
        put_opt(&mut k, "rcvbuf", c.rcvbuf);
        put_opt(&mut k, "sndbuf", c.sndbuf);
        put_opt(&mut k, "pkt_size", c.pkt_size);
        put_opt(
            &mut k,
            "connect_timeout",
            c.connect_timeout.map(|d| d.as_secs()),
        );
        put_opt(&mut k, "cert", path(&c.cert));
        put_opt(&mut k, "key", path(&c.key));
        put_opt(&mut k, "ca", path(&c.ca));
        record(if u.tls { "tcps" } else { "tcp" }, &u.host, u.port, k)
    }

    fn observe_hls(url: &str) -> Record {
        use tst_hls::config::{HlsConfig, HlsMode};
        // HlsError has no URL variant: `from_url`'s refusal is the domain's
        // URL kind in every binding.
        let c = match tst_hls::HlsUrl::parse(url) {
            Ok(u) => u.config,
            Err(_) => return refused(BindingErrorKind::HlsUrl),
        };
        let path = |p: &Option<std::path::PathBuf>| p.as_ref().map(|p| p.display().to_string());
        let mut k = Keys::new();
        if c.output_dir != HlsConfig::default().output_dir {
            put(&mut k, "output_dir", c.output_dir.display());
        }
        put(&mut k, "segment_duration", c.segment_duration.as_secs());
        put(&mut k, "playlist_window", c.playlist_window);
        put(
            &mut k,
            "mode",
            match c.mode {
                HlsMode::Live => "live",
                HlsMode::Event => "event",
                HlsMode::Vod => "vod",
            },
        );
        if let Some((user, pass)) = &c.basic_auth {
            put(&mut k, "auth_user", user);
            put(&mut k, "auth_pass", pass);
        }
        put_opt(&mut k, "cert", path(&c.tls_cert));
        put_opt(&mut k, "key", path(&c.tls_key));
        record(
            if c.tls { "hlss" } else { "hls" },
            &c.bind.ip().to_string(),
            c.bind.port(),
            k,
        )
    }

    fn observe_rist(url: &str) -> Record {
        use tst_rist::config::RistProfile;
        let u = match tst_rist::RistUrl::parse(url) {
            Ok(u) => u,
            Err(e) => return refused(BindingError::from(tst_rist::RistError::Url(e)).kind),
        };
        let c = &u.config;
        let mut k = Keys::new();
        put(
            &mut k,
            "profile",
            match c.profile {
                RistProfile::Simple => "simple",
                RistProfile::Main => "main",
                #[allow(unreachable_patterns)]
                _ => "other",
            },
        );
        put_opt(&mut k, "recovery_maxbitrate", c.recovery_maxbitrate_kbps);
        put_opt(&mut k, "bandwidth", c.bandwidth_kbps);
        put(&mut k, "buffer", ms(c.buffer));
        put_opt(&mut k, "session_timeout", c.session_timeout.map(ms));
        if let Some(key) = &c.encryption {
            put(&mut k, "aes-type", key.size_bits);
            put(&mut k, "secret", key.secret.expose());
        }
        put_opt(&mut k, "cname", c.cname.as_deref());
        put(&mut k, "compression", bit(c.compression));
        put(&mut k, "pkt_size", c.pkt_size);
        Record {
            bind: u.is_recv_bind.then_some(true),
            ..record("rist", &u.addr.to_string(), u.port, k)
        }
    }

    fn observe_whep(url: &str) -> Record {
        let u = match tst_webrtc::url::WhepUrl::parse(url) {
            Ok(u) => u,
            Err(e) => {
                return refused(
                    BindingError::from(tst_webrtc::WhepServerError::InvalidUrl(e)).kind,
                );
            }
        };
        let mut k = Keys::new();
        put(&mut k, "media_port", u.media_port);
        put_opt(&mut k, "advertise", u.advertise);
        put(&mut k, "max_viewers", u.max_viewers);
        put_opt(&mut k, "token", u.token.as_deref());
        put(&mut k, "cors_origin", &u.cors_origin);
        put_opt(&mut k, "cert", u.cert.as_deref());
        put_opt(&mut k, "key", u.key.as_deref());
        record(
            if u.tls { "wheps" } else { "whep" },
            &u.addr.to_string(),
            u.port,
            k,
        )
    }

    /// Every family's documented keys, from the parsers' own tables.
    fn tables() -> BTreeMap<&'static str, Vec<KeyDoc>> {
        let mut t: BTreeMap<&'static str, Vec<KeyDoc>> = BTreeMap::new();
        let mut srt: Vec<KeyDoc> = docs("srt", tst_srt::url::URL_KEYS).collect();
        srt.extend(tst_srt::url::URL_LEVEL_KEYS.iter().cloned());
        t.insert("srt", srt);
        t.insert("rtp", docs("rtp", tst_rtp::url::URL_KEYS_RTP).collect());
        t.insert("rtsp", docs("rtsp", tst_rtp::url::URL_KEYS_RTSP).collect());
        t.insert("udp", docs("udp", tst_udp::url::URL_KEYS).collect());
        t.insert("tcp", docs("tcp", tst_tcp::url::URL_KEYS).collect());
        t.insert("hls", docs("hls", tst_hls::url::URL_KEYS).collect());
        t.insert("rist", docs("rist", tst_rist::url::URL_KEYS).collect());
        t.insert("whep", docs("whep", tst_webrtc::url::URL_KEYS).collect());
        t
    }

    /// Whether `url`'s query names `key` (as `key=`).
    fn query_names(url: &str, key: &str) -> bool {
        url.split_once('?').is_some_and(|(_, q)| {
            q.split('&')
                .any(|pair| pair.split_once('=').map_or(pair, |(k, _)| k) == key)
        })
    }

    pub(super) fn check_coverage() {
        let corpus = url::load();
        let tables = tables();
        let mut gaps = Vec::new();
        assert_eq!(
            corpus
                .schemes
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            tables.keys().copied().collect::<Vec<_>>(),
            "the header lists a different set of scheme families than the parsers"
        );
        for (family, table) in &tables {
            let header = corpus
                .schemes
                .get(*family)
                .unwrap_or_else(|| panic!("the header has no [schemes.{family}]"));
            let aliases: usize = table.iter().map(|d| d.aliases.len()).sum();
            assert_eq!(
                (header.keys, header.aliases),
                (table.len(), aliases),
                "[schemes.{family}] counts differ from the parser's table"
            );
            let rows: Vec<&Row> = corpus
                .url
                .iter()
                .filter(|r| r.family() == *family)
                .collect();
            let has = |case: &str, key: &str, spelled: &str| {
                rows.iter().any(|r| {
                    r.case == case
                        && r.key.as_deref() == Some(key)
                        && (spelled.is_empty() || query_names(&r.url, spelled))
                })
            };
            for d in table {
                if !has("explicit", d.canonical, d.canonical) {
                    gaps.push(format!("{family}: no explicit row for {}", d.canonical));
                }
                for alias in d.aliases {
                    if !has("alias", d.canonical, alias) {
                        gaps.push(format!("{family}: no alias row spelling {alias}"));
                    }
                }
                let exempt = header.no_invalid.iter().any(|k| k == d.canonical);
                if !exempt && !has("invalid", d.canonical, "") {
                    gaps.push(format!("{family}: no invalid row for {}", d.canonical));
                }
            }
            for k in &header.no_invalid {
                if !table.iter().any(|d| d.canonical == k) {
                    gaps.push(format!("{family}: no_invalid names unknown key {k}"));
                }
            }
            for case in ["default", "credentials", "invalid", "conflict"] {
                if !rows.iter().any(|r| r.case == case) {
                    gaps.push(format!("{family}: no {case} row"));
                }
            }
        }
        // A wire probe accounts for every field of its family's records: each
        // is compared (`wire`) or says why not (`unobserved`, `name: reason`).
        for (leg, families) in &corpus.legs {
            for (family, table) in families {
                let Some(wire) = table.get("wire") else {
                    continue;
                };
                let unobserved = table.get("unobserved").map(String::as_str).unwrap_or("");
                let named: Vec<&str> = wire
                    .split_whitespace()
                    .chain(
                        unobserved
                            .split(';')
                            .filter_map(|e| e.split_once(':'))
                            .map(|(n, _)| n.trim()),
                    )
                    .collect();
                let mut fields: Vec<String> =
                    ["scheme", "host", "port", "path", "username", "password_set"]
                        .map(String::from)
                        .to_vec();
                fields.extend(
                    tables[family.as_str()]
                        .iter()
                        .map(|d| format!("keys.{}", d.canonical)),
                );
                for f in &fields {
                    if !named.contains(&f.as_str()) {
                        gaps.push(format!(
                            "{leg} {family}: wire probe neither compares nor explains {f}"
                        ));
                    }
                }
                for n in &named {
                    if !fields.iter().any(|f| f == n) {
                        gaps.push(format!(
                            "{leg} {family}: wire probe names unknown field {n}"
                        ));
                    }
                }
            }
        }
        for row in &corpus.url {
            const CASES: [&str; 6] = [
                "default",
                "explicit",
                "alias",
                "credentials",
                "invalid",
                "conflict",
            ];
            if !CASES.contains(&row.case.as_str()) {
                gaps.push(format!("{}: unknown case", row.label()));
            }
            if let Some(key) = &row.key
                && !tables[row.family()].iter().any(|d| d.canonical == key)
            {
                gaps.push(format!("{}: key {key} is not documented", row.label()));
            }
            let bare = Record {
                error_kind: row.expect.error_kind.clone(),
                ..Record::default()
            };
            if row.expect.is_error() && row.expect != bare {
                gaps.push(format!(
                    "{}: an error record carries other fields",
                    row.label()
                ));
            }
            // Every leg must declare the row's role.
            for leg in corpus.legs.keys() {
                let _ = corpus.entry(leg, row);
            }
        }
        assert!(
            gaps.is_empty(),
            "URL corpus coverage gaps:\n{}",
            gaps.join("\n")
        );
    }
}
