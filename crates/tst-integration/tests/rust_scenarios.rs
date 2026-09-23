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
/// names the contract. Some contracts are primarily exercised by the C adapter
/// (Task 13) — see the per-arm comments for what is deferred and why.
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
            // C/Python adapters (Task 13).
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
            // remain a C-adapter concern (Task 13) — pure Rust has no raw
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
            // binding layer is the golden's code (Arc 2 spec 3.3 / 6).
            // Choreography: listener on this thread, idle caller on a helper
            // thread, the recv parked on a worker thread, the cancel fired from
            // here. Every wait is bounded (10 s) and FAILS; the worker is never
            // joined after a failed verdict, so a regression reports instead of
            // hanging.
            use std::sync::mpsc;
            use std::time::Duration;
            use tst_core::transport::{RecvTransport, TransportError};
            use tst_pipeline::binding::BindingError;
            use tst_srt::{ListenerBuilder, SocketBuilder, SrtTransport};

            let mut listener = ListenerBuilder::new()
                .recv_timeout(Duration::from_secs(5))
                .send_timeout(Duration::from_secs(5))
                .bind("127.0.0.1:0")
                .expect(
                    "cancelled-recv-kind: bind 127.0.0.1:0 (loopback is required, not optional)",
                );
            let port = listener.local_addr().expect("local_addr").port();
            let peer = std::thread::spawn(move || {
                SocketBuilder::new()
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
