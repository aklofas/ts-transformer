use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use tst_interop::cli;
use tst_interop::fixtures::{AuSizeMode, KlvSet};
use tst_interop::r#gen;
use tst_interop::impair::ImpairConfig;
use tst_interop::profiles;
use tst_interop::proxy;
use tst_interop::recv;
use tst_interop::report;
use tst_interop::send;
use tst_interop::serve;
use tst_interop::verify::{self, KlvExpect};

fn usage() -> String {
    "usage: tst-interop <subcommand> [options...]

Subcommands:
  gen       Generate synthetic test fixtures
  send      Send test data to endpoint (hls:// and rtsp:// URLs BIND and
            serve instead of connecting — see `send`'s own doc comment;
            --corrupt rate=N[,min_gap=N][,classes=a+b] --corruption-log PATH
            [--seed N] turn on the seeded TS corruption tap)
  recv      Receive test data from endpoint (--corruption-log PATH judges the
            capture against a `send --corrupt` peer's log — start recv FIRST)
  verify    Verify interop test results
  proxy     UDP impairment relay (loss/dup/reorder/jitter/scheduled outage;
            --schedule seed=N,phases=K,phase_s=DUR walks a seeded phase
            table instead of one fixed impairment level)
  report    Generate interop report
  pick-profiles --seed N --legs K
            Print K distinct profile names, drawn deterministically from
            the seed (soak.sh --profile auto's per-leg selection)

Options:
  -h, --help   Show this help message"
        .to_string()
}

/// Returns the value following a value-taking flag at `args[i]`
/// (i.e. `args[i + 1]`), or exits with an actionable error (never
/// returns) if that slot is missing entirely OR looks like the start
/// of another flag (`--...`). Every subcommand below except `proxy`
/// (which already gets equivalent protection for free: its flags all
/// route through a typed parser that rejects a flag-shaped string,
/// e.g. `"--forward".parse::<SocketAddr>()` fails) used to fetch a
/// flag's value with a bare `args.get(i + 1)`, which — for a flag
/// given with NO value — silently consumes the FOLLOWING flag's own
/// name as if it were this flag's value, then desyncs every argument
/// after it. The user sees a misleading "unknown argument" error many
/// tokens later instead of anything pointing at the flag that was
/// actually missing its value. `context` is the full "subcommand:
/// --flag" prefix for the error message (e.g. `"gen: --profile"`).
fn require_value(args: &[String], i: usize, context: &str) -> String {
    match args.get(i + 1) {
        None => {
            eprintln!("{context} requires a value");
            std::process::exit(2);
        }
        Some(v) if v.starts_with("--") => {
            eprintln!("{context} requires a value, got '{v}' (looks like another flag)");
            std::process::exit(2);
        }
        Some(v) => v.clone(),
    }
}

/// Parse a `--klv-set compact|rich` value, or exit 2 naming the
/// subcommand. `context` is the subcommand name (e.g. `"gen"`), so the
/// error reads the same way `--au-sizes`'s does.
fn parse_klv_set(raw: &str, context: &str) -> KlvSet {
    KlvSet::parse(raw).unwrap_or_else(|| {
        eprintln!("{context}: --klv-set must be 'compact' or 'rich', got '{raw}'");
        std::process::exit(2);
    })
}

/// Parse an `--au-sizes compact|realistic` value, or exit 2 naming the
/// subcommand. `context` is the subcommand name (e.g. `"gen"`).
fn parse_au_sizes(raw: &str, context: &str) -> AuSizeMode {
    match raw {
        "compact" => AuSizeMode::Compact,
        "realistic" => AuSizeMode::Realistic,
        other => {
            eprintln!("{context}: --au-sizes must be 'compact' or 'realistic', got '{other}'");
            std::process::exit(2);
        }
    }
}

/// Parse a `--klv-seed N` value, or exit 2 naming the subcommand.
fn parse_klv_seed(raw: &str, context: &str) -> u64 {
    raw.parse::<u64>().unwrap_or_else(|e| {
        eprintln!("{context}: --klv-seed must be a non-negative integer, got '{raw}': {e}");
        std::process::exit(2);
    })
}

/// Wires `tracing` events (e.g. `tst_pipeline::managed_receive`'s /
/// `tst_pipeline::managed_demux_receiver`'s reconnect-attempt logs) to
/// stderr, gated by `RUST_LOG` (silent — no subscriber overhead beyond
/// the check itself — when unset). Load-bearing for diagnosing a stuck
/// `--managed` reconnect loop on a live soak run: without this, every
/// `info!`/`warn!`/`debug!` call in `tst-pipeline`'s reconnect
/// decorators is silently discarded (no subscriber installed = no
/// output), leaving zero visibility into attempt counts/backoff timing
/// from this binary's own logs.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
}

fn main() {
    init_tracing();
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        println!("{}", usage());
        std::process::exit(0);
    }

    let subcommand = &args[1];

    match subcommand.as_str() {
        "-h" | "--help" => {
            println!("{}", usage());
            std::process::exit(0);
        }
        "gen" => run_gen(&args[2..]),
        "send" => run_send(&args[2..]),
        "recv" => run_recv(&args[2..]),
        "verify" => run_verify(&args[2..]),
        "proxy" => run_proxy(&args[2..]),
        "report" => run_report(&args[2..]),
        "pick-profiles" => run_pick_profiles(&args[2..]),
        _ => {
            eprintln!("Unknown subcommand: {}", subcommand);
            println!("{}", usage());
            std::process::exit(2);
        }
    }
}

/// `gen --profile NAME --seconds N --out PATH
/// [--klv-set compact|rich] [--klv-seed N]
/// [--au-sizes compact|realistic]`
///
/// Generates `N` seconds of profile `NAME`'s synthetic MPEG-TS/KLV traffic
/// (offline pacing, no transport) and writes it to `PATH`. Exits 0 on
/// success, 2 on usage/IO error.
///
/// `--klv-set rich` (default `compact`) swaps the 4-tag fixture record
/// for an ST 0601 record of up to 36 tags (mean ~27) carrying a nested
/// ST 0102 security set,
/// whose tag set varies record to record on a schedule seeded by
/// `--klv-seed N` (default 0). The default is byte-identical to what
/// this subcommand has always written, so every interop-matrix cell is
/// unaffected. A receiver judging a rich capture must be told the same
/// `--klv-set`/`--klv-seed` — see `recv`/`verify`.
///
/// `--au-sizes realistic` (default `compact`) swaps the tens-of-bytes
/// video AUs for GOP-structured ones (keyframes tens of KB, inter
/// frames single-digit KB — see `fixtures::AuSizeMode`). The default is
/// byte-identical to what this subcommand has always written; the
/// matrix runner passes `realistic` so its cells exercise a real
/// encoder's size regime.
fn run_gen(args: &[String]) -> ! {
    let mut profile: Option<String> = None;
    let mut seconds: Option<f64> = None;
    let mut out: Option<PathBuf> = None;
    let mut klv_set = KlvSet::Compact;
    let mut klv_seed: u64 = 0;
    let mut au_sizes = AuSizeMode::Compact;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--profile" => {
                profile = Some(require_value(args, i, "gen: --profile"));
                i += 2;
            }
            "--seconds" => {
                seconds = cli::parse_seconds(&require_value(args, i, "gen: --seconds"));
                i += 2;
            }
            "--out" => {
                out = Some(PathBuf::from(require_value(args, i, "gen: --out")));
                i += 2;
            }
            "--klv-set" => {
                klv_set = parse_klv_set(&require_value(args, i, "gen: --klv-set"), "gen");
                i += 2;
            }
            "--klv-seed" => {
                klv_seed = parse_klv_seed(&require_value(args, i, "gen: --klv-seed"), "gen");
                i += 2;
            }
            "--au-sizes" => {
                au_sizes = parse_au_sizes(&require_value(args, i, "gen: --au-sizes"), "gen");
                i += 2;
            }
            other => {
                eprintln!("gen: unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let profile_name = profile.unwrap_or_else(|| {
        eprintln!("gen: --profile is required");
        std::process::exit(2);
    });
    let seconds = seconds.unwrap_or_else(|| {
        eprintln!("gen: --seconds is required and must be a finite, positive number");
        std::process::exit(2);
    });
    let out = out.unwrap_or_else(|| {
        eprintln!("gen: --out is required");
        std::process::exit(2);
    });
    let p = profiles::by_name(&profile_name).unwrap_or_else(|| {
        eprintln!("gen: unknown profile: {profile_name}");
        std::process::exit(2);
    });

    if let Err(e) = r#gen::run(p, seconds, &out, klv_set, klv_seed, au_sizes) {
        eprintln!("gen: {e}");
        std::process::exit(2);
    }

    eprintln!(
        "gen: wrote {seconds}s of {profile_name} to {}",
        out.display()
    );
    std::process::exit(0);
}

/// `send --profile NAME --url URL --seconds N [--json OUT] [--managed]
/// [--reconnect-mode blocking|background] [--no-klv-digest]
/// [--au-sizes compact|realistic]
/// [--klv-set compact|rich] [--klv-seed N]
/// [--corrupt SPEC --corruption-log PATH] [--seed N]`
///
/// Builds a live transport from `URL` and pushes `N` seconds of profile
/// `NAME`'s synthetic MPEG-TS/KLV traffic through it, paced to real
/// time. Exits 0 on success, 2 on usage/transport error. `--json OUT`
/// additionally writes the sent-side `CellMetrics` as JSON to `OUT`
/// (or stdout, if `OUT` is `-`).
///
/// `hls://`/`hlss://` and `rtsp://`/`rtsps://` URLs are serve (BIND)
/// modes, not connect modes: this subcommand binds a real HLS HTTP
/// server / RTSP server at the URL's host:port and waits for a peer to
/// pull, instead of connecting out to one (see `tst_interop::serve`'s
/// doc comment for why these two schemes work this way). `--json` is
/// ignored for these two schemes — there is no sent-side `CellMetrics`
/// to write (no wire-level Transport tee; see `serve.rs`'s scope notes).
///
/// `--managed` wraps the transport in `tst_pipeline::ManagedTransport`
/// (see `send::run_managed`'s doc comment) so a transport break
/// reconnects by re-dialing `URL` instead of failing the push loop —
/// `soak.sh`'s SRT leg uses this to survive scheduled proxy outage
/// windows. Rejected (exit 2) for the `hls://`/`rtsp://` serve schemes,
/// which have no connect-mode transport to reconnect.
///
/// `--reconnect-mode` picks how a managed transport spends an outage —
/// meaningful only with `--managed`, and `blocking` (the default) is the
/// soak's shape: the producer stalls for the outage and replays it as a
/// burst afterwards. `background` keeps the producer moving and drains
/// the gap buffer beside live traffic.
///
/// `--no-klv-digest` skips the per-record KLV digest accumulation
/// `CellMetrics::klv_set_sha256` needs — that field comes back `null`
/// in the JSON instead. `soak.sh` passes this on both legs: a multi-day
/// run would otherwise accumulate one digest string per KLV record for
/// the ENTIRE run, an unbounded, harness-only allocation (see
/// `CellMetrics::klv_set_sha256`'s own doc comment for the measured
/// impact). Video/KLV/audio counts and every other metric are
/// unaffected.
///
/// `--au-sizes realistic` switches the video AU factory to GOP-
/// structured multi-KB sizes (~1.7 Mb/s at the schedule's 30 fps) —
/// `soak.sh`'s true-bandwidth regime. The default (`compact`) is
/// byte-identical to what this subcommand has always sent, so every
/// interop-matrix invocation is unaffected. See
/// `fixtures::AuSizeMode`.
///
/// `--klv-set rich` / `--klv-seed N` pick the ST 0601 record factory —
/// see `gen`'s own doc comment. Forwarded unchanged to the `hls://` /
/// `rtsp://` serve modes below. Independent of `--seed` (the corruption
/// tap's), so a run can vary one without disturbing the other.
///
/// `--corrupt SPEC` turns on the seeded corruption tap between the muxer
/// and the wire: `rate=PER_10K[,min_gap=PKTS][,classes=a+b+c]` (see
/// `corrupt::parse_corrupt`, which rejects a typo rather than silently
/// degrading to "no corruption"). `--corruption-log PATH` is REQUIRED
/// with it and names the JSONL evidence file the tap writes — one line
/// per injection, which a `recv --corruption-log PATH` peer reads back
/// to judge whether the receiver noticed. Requiring the path rather than
/// defaulting one is deliberate: corruption nobody recorded is
/// indistinguishable from a library bug. Both flags are rejected (exit
/// 2) for the `hls://`/`rtsp://` serve schemes, which have no
/// `Transport` to wrap.
///
/// `--seed N` (default 0) is the run seed the tap draws from, salted so
/// its stream is independent of every other seeded component sharing the
/// same number. The same seed, config and input bytes produce a
/// byte-identical wire and a byte-identical log.
fn run_send(args: &[String]) -> ! {
    let mut profile: Option<String> = None;
    let mut url: Option<String> = None;
    let mut seconds: Option<f64> = None;
    let mut json_out: Option<String> = None;
    let mut managed = false;
    let mut reconnect_mode = tst_pipeline::ReconnectMode::Blocking;
    // Tracked separately from the value: only `--managed` reads the mode,
    // so passing it without `--managed` is a silently-ignored flag, not a
    // default. Rejected below rather than dropped.
    let mut reconnect_mode_set = false;
    let mut no_klv_digest = false;
    let mut au_sizes = AuSizeMode::Compact;
    let mut klv_set = KlvSet::Compact;
    let mut klv_seed: u64 = 0;
    let mut corrupt_spec: Option<String> = None;
    let mut corruption_log: Option<PathBuf> = None;
    let mut seed: u64 = 0;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--profile" => {
                profile = Some(require_value(args, i, "send: --profile"));
                i += 2;
            }
            "--url" => {
                url = Some(require_value(args, i, "send: --url"));
                i += 2;
            }
            "--seconds" => {
                seconds = cli::parse_seconds(&require_value(args, i, "send: --seconds"));
                i += 2;
            }
            "--json" => {
                json_out = Some(require_value(args, i, "send: --json"));
                i += 2;
            }
            "--managed" => {
                managed = true;
                i += 1;
            }
            "--reconnect-mode" => {
                reconnect_mode = match require_value(args, i, "send: --reconnect-mode").as_str() {
                    "blocking" => tst_pipeline::ReconnectMode::Blocking,
                    "background" => tst_pipeline::ReconnectMode::Background,
                    other => {
                        eprintln!(
                            "send: --reconnect-mode must be blocking|background, got {other}"
                        );
                        std::process::exit(2);
                    }
                };
                reconnect_mode_set = true;
                i += 2;
            }
            "--no-klv-digest" => {
                no_klv_digest = true;
                i += 1;
            }
            "--au-sizes" => {
                au_sizes = parse_au_sizes(&require_value(args, i, "send: --au-sizes"), "send");
                i += 2;
            }
            "--klv-set" => {
                klv_set = parse_klv_set(&require_value(args, i, "send: --klv-set"), "send");
                i += 2;
            }
            "--klv-seed" => {
                klv_seed = parse_klv_seed(&require_value(args, i, "send: --klv-seed"), "send");
                i += 2;
            }
            "--corrupt" => {
                corrupt_spec = Some(require_value(args, i, "send: --corrupt"));
                i += 2;
            }
            "--corruption-log" => {
                corruption_log = Some(PathBuf::from(require_value(
                    args,
                    i,
                    "send: --corruption-log",
                )));
                i += 2;
            }
            "--seed" => {
                let raw = require_value(args, i, "send: --seed");
                seed = raw.parse::<u64>().unwrap_or_else(|e| {
                    eprintln!("send: --seed must be a non-negative integer, got '{raw}': {e}");
                    std::process::exit(2);
                });
                i += 2;
            }
            other => {
                eprintln!("send: unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let profile_name = profile.unwrap_or_else(|| {
        eprintln!("send: --profile is required");
        std::process::exit(2);
    });
    let url = url.unwrap_or_else(|| {
        eprintln!("send: --url is required");
        std::process::exit(2);
    });
    let seconds = seconds.unwrap_or_else(|| {
        eprintln!("send: --seconds is required and must be a finite, positive number");
        std::process::exit(2);
    });
    let p = profiles::by_name(&profile_name).unwrap_or_else(|| {
        eprintln!("send: unknown profile: {profile_name}");
        std::process::exit(2);
    });
    // Only the managed path builds a `ReconnectPolicy`, so the mode is
    // read nowhere else. Accepting it without `--managed` would silently
    // drop it and let a drill believe it ran in a mode it never did.
    if reconnect_mode_set && !managed {
        eprintln!("send: --reconnect-mode is meaningful only with --managed");
        std::process::exit(2);
    }

    // A log with no tap would be an empty file nobody wrote to, and a
    // tap with no log would corrupt a stream whose damage nothing could
    // ever explain. Both directions are usage errors.
    let corrupt = match (corrupt_spec, corruption_log) {
        (Some(spec), Some(path)) => {
            let cfg = tst_interop::corrupt::parse_corrupt(&spec, seed).unwrap_or_else(|e| {
                eprintln!("send: {e}");
                std::process::exit(2);
            });
            Some((cfg, path))
        }
        (Some(_), None) => {
            eprintln!(
                "send: --corrupt requires --corruption-log PATH (corruption nobody recorded cannot be judged)"
            );
            std::process::exit(2);
        }
        (None, Some(_)) => {
            eprintln!("send: --corruption-log is only meaningful with --corrupt SPEC");
            std::process::exit(2);
        }
        (None, None) => None,
    };

    // hls:// / rtsp:// (+ TLS variants) are serve (bind) modes — branch
    // out before the connect-side transport path below.
    if let Some(scheme) = serve::serve_scheme_of(&url) {
        if corrupt.is_some() {
            eprintln!(
                "send: --corrupt is not supported for {url} (hls/rtsp serve their own sessions \
                 — there is no Transport for the tap to wrap)"
            );
            std::process::exit(2);
        }
        if managed {
            eprintln!(
                "send: --managed is not meaningful for {url} (hls/rtsp are serve/bind modes, \
                 not connect modes — nothing to reconnect)"
            );
            std::process::exit(2);
        }
        let result = match scheme {
            serve::ServeScheme::Hls => {
                serve::run_hls_url(p, &url, seconds, klv_set, klv_seed, au_sizes)
            }
            serve::ServeScheme::Rtsp => {
                serve::run_rtsp_url(p, &url, seconds, klv_set, klv_seed, au_sizes)
            }
        };
        if let Err(e) = result {
            eprintln!("send: {e}");
            std::process::exit(2);
        }
        eprintln!("send: served {seconds}s of {profile_name} at {url}");
        std::process::exit(0);
    }

    let metrics = if managed {
        send::run_managed(
            p,
            &url,
            seconds,
            json_out.as_deref(),
            no_klv_digest,
            au_sizes,
            klv_set,
            klv_seed,
            corrupt,
            reconnect_mode,
        )
    } else {
        send::run(
            p,
            &url,
            seconds,
            json_out.as_deref(),
            no_klv_digest,
            au_sizes,
            klv_set,
            klv_seed,
            corrupt,
        )
    }
    .unwrap_or_else(|e| {
        eprintln!("send: {e}");
        std::process::exit(2);
    });

    eprintln!(
        "send: pushed {} video AUs, {} klv records to {url}",
        metrics.video_aus, metrics.klv_records
    );
    std::process::exit(0);
}

/// `recv --url URL --expect PROFILE --seconds N [--json OUT]
/// [--managed] [--no-klv-digest] [--strict]
/// [--klv-set compact|rich] [--klv-seed N] [--corruption-log PATH]`
///
/// Builds a live transport from `URL` and receives `N` seconds of
/// traffic from it, checking the result against `PROFILE`'s invariants.
/// Exits 0 on pass, 1 on fail, 2 on usage/transport error. `--json OUT`
/// additionally writes the full `VerifyReport` as JSON to `OUT` (or
/// stdout, if `OUT` is `-`).
///
/// `--managed` drives the capture through
/// `tst_pipeline::ManagedDemuxReceiver`/`ManagedRecvTransport` (see
/// `recv::run_managed`'s doc comment) instead of a plain
/// `DemuxReceiver`, so a transport break rebuilds (or, for a listener-
/// mode SRT URL, re-binds + re-accepts) instead of ending the capture
/// — `soak.sh`'s SRT leg uses this to survive scheduled proxy outage
/// windows on the RECEIVE side (the send side already had this via
/// `send --managed`; a plain recv against a listener-mode SRT URL only
/// ever accepts ONE connection for the whole process lifetime, so it
/// alone would end the capture at the first outage even with a managed
/// sender retrying forever on the other end). `VerifyReport.reconnects`
/// comes back `Some(n)` instead of `null`.
///
/// `--no-klv-digest` — see `send`'s own doc comment for the shared
/// rationale (`soak.sh` passes it on both sides of both legs);
/// `VerifyReport.metrics.klv_set_sha256` comes back `null` instead of
/// the hash, everything else unaffected.
///
/// `--strict` additionally fails the check on any `Discontinuity` demux
/// event — for lossless transparent-tier cells; default `Lossy` counts
/// them (in `VerifyReport.metrics.discontinuities`) without failing.
/// `NonConformant` events always fail, in either mode.
///
/// `--klv-set rich` / `--klv-seed N` must MATCH what the sender's
/// `gen`/`send` used: the report then gains `metrics.klv_rich` plus the
/// `klv_rich_decode_clean` / `klv_rich_census` /
/// `klv_rich_security_nested` verdicts, which decode every ST 0601
/// record and check its tag set against the presence schedule that seed
/// declares. A mismatched seed is a real failure, not a configuration
/// nuisance — it means the records on the wire are not the ones the
/// sender was supposed to emit.
///
/// `--corruption-log PATH` reads the JSONL log a `send --corrupt` peer
/// wrote and judges this capture AGAINST it: the report gains
/// `metrics.corruption_attribution` plus the `corruption_attributed` /
/// `corruption_detected` / `corruption_recovered` verdicts (did every
/// error event have a cause, did the receiver notice every injection it
/// was required to, did the stream produce media again afterwards), and
/// the whole-capture count floors are discounted by the injected
/// fraction. Deliberately destroyed packets also stop failing the
/// capture as raw sync loss — they are the premise, not the finding. The
/// log must be the one written by the sender feeding THIS capture; a log
/// from a different run shares no packet coordinates and every verdict
/// would be noise.
///
/// The file need not exist when `recv` starts — it waits for the sender
/// to create it — and is re-read throughout the capture, so injections
/// logged while this process is already running are judged too. START
/// `recv` BEFORE `send`: see `recv::run`'s doc comment for what a
/// receiver that joins mid-stream can and cannot place.
fn run_recv(args: &[String]) -> ! {
    let mut url: Option<String> = None;
    let mut expect: Option<String> = None;
    let mut seconds: Option<f64> = None;
    let mut json_out: Option<String> = None;
    let mut managed = false;
    let mut no_klv_digest = false;
    let mut strict = false;
    let mut klv_set = KlvSet::Compact;
    let mut klv_seed: u64 = 0;
    let mut corruption_log: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--url" => {
                url = Some(require_value(args, i, "recv: --url"));
                i += 2;
            }
            "--expect" => {
                expect = Some(require_value(args, i, "recv: --expect"));
                i += 2;
            }
            "--seconds" => {
                seconds = cli::parse_seconds(&require_value(args, i, "recv: --seconds"));
                i += 2;
            }
            "--json" => {
                json_out = Some(require_value(args, i, "recv: --json"));
                i += 2;
            }
            "--managed" => {
                managed = true;
                i += 1;
            }
            "--no-klv-digest" => {
                no_klv_digest = true;
                i += 1;
            }
            "--strict" => {
                strict = true;
                i += 1;
            }
            "--klv-set" => {
                klv_set = parse_klv_set(&require_value(args, i, "recv: --klv-set"), "recv");
                i += 2;
            }
            "--klv-seed" => {
                klv_seed = parse_klv_seed(&require_value(args, i, "recv: --klv-seed"), "recv");
                i += 2;
            }
            "--corruption-log" => {
                corruption_log = Some(PathBuf::from(require_value(
                    args,
                    i,
                    "recv: --corruption-log",
                )));
                i += 2;
            }
            other => {
                eprintln!("recv: unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let url = url.unwrap_or_else(|| {
        eprintln!("recv: --url is required");
        std::process::exit(2);
    });
    let expect = expect.unwrap_or_else(|| {
        eprintln!("recv: --expect is required");
        std::process::exit(2);
    });
    let seconds = seconds.unwrap_or_else(|| {
        eprintln!("recv: --seconds is required and must be a finite, positive number");
        std::process::exit(2);
    });
    let profile = profiles::by_name(&expect).unwrap_or_else(|| {
        eprintln!("recv: unknown profile: {expect}");
        std::process::exit(2);
    });

    let klv = KlvExpect {
        set: klv_set,
        seed: klv_seed,
    };
    let report = if managed {
        recv::run_managed(
            &url,
            profile,
            seconds,
            json_out.as_deref(),
            no_klv_digest,
            strict,
            klv,
            corruption_log.as_deref(),
        )
    } else {
        recv::run(
            &url,
            profile,
            seconds,
            json_out.as_deref(),
            no_klv_digest,
            strict,
            klv,
            corruption_log.as_deref(),
        )
    }
    .unwrap_or_else(|e| {
        eprintln!("recv: {e}");
        std::process::exit(2);
    });

    if report.pass {
        eprintln!("recv: PASS ({expect})");
    } else {
        eprintln!("recv: FAIL ({expect}): {}", report.failures.join("; "));
    }

    std::process::exit(if report.pass { 0 } else { 1 });
}

/// `verify --file F --expect PROFILE --seconds N [--json OUT]
/// [--klv-set compact|rich] [--klv-seed N]`
///
/// Demuxes `F` and checks it against `PROFILE`'s invariants for an
/// `N`-second capture. Exits 0 on pass, 1 on fail, 2 on usage/IO error.
/// `--json OUT` additionally writes the full `VerifyReport` as JSON to
/// `OUT` (or stdout, if `OUT` is `-`).
///
/// `--klv-set rich` / `--klv-seed N` must match what generated `F` — see
/// `recv`'s own doc comment for what the rich verdicts check.
fn run_verify(args: &[String]) -> ! {
    let mut file: Option<PathBuf> = None;
    let mut expect: Option<String> = None;
    let mut seconds: Option<f64> = None;
    let mut json_out: Option<String> = None;
    let mut klv_set = KlvSet::Compact;
    let mut klv_seed: u64 = 0;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--file" => {
                file = Some(PathBuf::from(require_value(args, i, "verify: --file")));
                i += 2;
            }
            "--expect" => {
                expect = Some(require_value(args, i, "verify: --expect"));
                i += 2;
            }
            "--seconds" => {
                seconds = cli::parse_seconds(&require_value(args, i, "verify: --seconds"));
                i += 2;
            }
            "--json" => {
                json_out = Some(require_value(args, i, "verify: --json"));
                i += 2;
            }
            "--klv-set" => {
                klv_set = parse_klv_set(&require_value(args, i, "verify: --klv-set"), "verify");
                i += 2;
            }
            "--klv-seed" => {
                klv_seed = parse_klv_seed(&require_value(args, i, "verify: --klv-seed"), "verify");
                i += 2;
            }
            other => {
                eprintln!("verify: unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let file = file.unwrap_or_else(|| {
        eprintln!("verify: --file is required");
        std::process::exit(2);
    });
    let expect = expect.unwrap_or_else(|| {
        eprintln!("verify: --expect is required");
        std::process::exit(2);
    });
    let seconds = seconds.unwrap_or_else(|| {
        eprintln!("verify: --seconds is required and must be a finite, positive number");
        std::process::exit(2);
    });
    let profile = profiles::by_name(&expect).unwrap_or_else(|| {
        eprintln!("verify: unknown profile: {expect}");
        std::process::exit(2);
    });

    let report = verify::verify_file_with(
        &file,
        profile,
        seconds,
        KlvExpect {
            set: klv_set,
            seed: klv_seed,
        },
    )
    .unwrap_or_else(|e| {
        eprintln!("verify: {e}");
        std::process::exit(2);
    });

    if report.pass {
        eprintln!("verify: PASS ({expect})");
    } else {
        eprintln!("verify: FAIL ({expect}): {}", report.failures.join("; "));
    }

    if let Some(target) = json_out {
        if let Err(e) = cli::write_json(&target, &report) {
            eprintln!("verify: {e}");
            std::process::exit(2);
        }
    }

    std::process::exit(if report.pass { 0 } else { 1 });
}

/// `proxy --listen ADDR --forward ADDR [--loss PCT] [--dup PCT]
/// [--reorder PCT,HOLD_MS] [--jitter MS] [--delay MS] [--seed N]
/// [--outage period=DUR,dur=DUR] [--schedule seed=N,phases=K,phase_s=DUR]
/// [--stats-json PATH] [--run-seconds N]`
///
/// `--delay` is a constant base delay applied to every non-dropped
/// packet (a link's one-way WAN latency), on top of which `--jitter`
/// varies — see `ImpairConfig::base_delay_ms`.
///
/// `--schedule` switches the relay from ONE fixed impairment level to a
/// seeded sequence of `K` phases, each in force for `phase_s` (a link
/// whose quality changes over a long run — see `proxy::run`). It is
/// therefore mutually exclusive with the four flags it would override
/// (`--loss`, `--jitter`, `--reorder`, `--delay`): passing both is a
/// usage error (exit 2) rather than a silently ignored flag. `--dup`,
/// `--seed` and `--outage` stay per-run and combine freely with it.
/// `--schedule`'s own `seed=` is the SCHEDULE's seed, separate from
/// `--seed` (the per-packet engine's) — the two are salted apart, so
/// passing the same number to both is fine.
///
/// Binds a UDP impairment relay at `--listen` (an ephemeral `:0` port is
/// printed as `{"listening": "..."}` on stdout as soon as it's bound —
/// see `proxy::run`'s doc comment) and relays to `--forward` under the
/// configured impairment. Every impairment knob defaults to fully
/// transparent (`ImpairConfig::default()`) when its flag is omitted.
/// `--run-seconds` bounds how long the relay runs before exiting (the
/// default, omitted, runs until the process is killed — this
/// subcommand's normal long-running CLI mode). Exits 0 on a clean
/// finish, 2 on a usage or IO error.
fn run_proxy(args: &[String]) -> ! {
    let mut listen: Option<SocketAddr> = None;
    let mut forward: Option<SocketAddr> = None;
    let mut loss_pct = 0.0f64;
    let mut dup_pct = 0.0f64;
    let mut reorder_pct = 0.0f64;
    let mut reorder_hold = 0u32;
    let mut jitter_ms_max = 0u32;
    let mut base_delay_ms = 0u32;
    let mut seed = 0u64;
    let mut outage_period_s: Option<u64> = None;
    let mut outage_dur_s = 0u64;
    let mut stats_json: Option<PathBuf> = None;
    let mut run_seconds: Option<u64> = None;
    let mut schedule: Option<(u64, u32, u64)> = None;
    // Which of the four per-phase-overridden impairment flags were
    // actually passed — named, so the mutual-exclusion error below can
    // say which one conflicts instead of just that something did.
    let mut fixed_flags: Vec<&str> = Vec::new();

    let bad_arg = |flag: &str, expected: &str| -> ! {
        eprintln!("proxy: --{flag} must be {expected}");
        std::process::exit(2);
    };

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--listen" => {
                listen = Some(
                    args.get(i + 1)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or_else(|| bad_arg("listen", "a socket address (host:port)")),
                );
                i += 2;
            }
            "--forward" => {
                forward = Some(
                    args.get(i + 1)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or_else(|| bad_arg("forward", "a socket address (host:port)")),
                );
                i += 2;
            }
            "--loss" => {
                loss_pct = args
                    .get(i + 1)
                    .and_then(|s| proxy::parse_percent(s))
                    .unwrap_or_else(|| bad_arg("loss", "a percent in 0..=100"));
                fixed_flags.push("--loss");
                i += 2;
            }
            "--dup" => {
                dup_pct = args
                    .get(i + 1)
                    .and_then(|s| proxy::parse_percent(s))
                    .unwrap_or_else(|| bad_arg("dup", "a percent in 0..=100"));
                i += 2;
            }
            "--reorder" => {
                let (pct, hold) = args
                    .get(i + 1)
                    .and_then(|s| proxy::parse_reorder(s))
                    .unwrap_or_else(|| bad_arg("reorder", "PCT,HOLD_MS (e.g. 1,200)"));
                reorder_pct = pct;
                reorder_hold = hold;
                fixed_flags.push("--reorder");
                i += 2;
            }
            "--jitter" => {
                jitter_ms_max = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| bad_arg("jitter", "a non-negative integer (milliseconds)"));
                fixed_flags.push("--jitter");
                i += 2;
            }
            "--delay" => {
                base_delay_ms = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| bad_arg("delay", "a non-negative integer (milliseconds)"));
                fixed_flags.push("--delay");
                i += 2;
            }
            "--seed" => {
                seed = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| bad_arg("seed", "a non-negative integer"));
                i += 2;
            }
            "--outage" => {
                let (period, dur) = args
                    .get(i + 1)
                    .and_then(|s| proxy::parse_outage(s))
                    .unwrap_or_else(|| {
                        bad_arg("outage", "period=DUR,dur=DUR (e.g. period=6h,dur=90s)")
                    });
                outage_period_s = Some(period);
                outage_dur_s = dur;
                i += 2;
            }
            "--schedule" => {
                schedule = Some(
                    args.get(i + 1)
                        .and_then(|s| proxy::parse_schedule(s))
                        .unwrap_or_else(|| {
                            bad_arg(
                                "schedule",
                                "seed=N,phases=K,phase_s=DUR with K and DUR nonzero \
                                 (e.g. seed=7,phases=6,phase_s=10m)",
                            )
                        }),
                );
                i += 2;
            }
            "--stats-json" => {
                stats_json = Some(
                    args.get(i + 1)
                        .map(PathBuf::from)
                        .unwrap_or_else(|| bad_arg("stats-json", "a file path")),
                );
                i += 2;
            }
            "--run-seconds" => {
                run_seconds = Some(
                    args.get(i + 1)
                        .and_then(|s| proxy::parse_run_seconds(s))
                        .unwrap_or_else(|| bad_arg("run-seconds", "a positive integer")),
                );
                i += 2;
            }
            other => {
                eprintln!("proxy: unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let listen = listen.unwrap_or_else(|| {
        eprintln!("proxy: --listen is required (host:port)");
        std::process::exit(2);
    });
    let forward = forward.unwrap_or_else(|| {
        eprintln!("proxy: --forward is required (host:port)");
        std::process::exit(2);
    });

    // A scheduled run's phases OVERRIDE loss/jitter/reorder/delay, so
    // accepting both would silently ignore whatever the caller typed for
    // them — refuse instead of quietly running something else.
    if schedule.is_some() && !fixed_flags.is_empty() {
        eprintln!(
            "proxy: --schedule sets loss/jitter/reorder/delay per phase, so it cannot be \
             combined with {} (--dup, --seed and --outage stay per-run and are fine)",
            fixed_flags.join(", ")
        );
        std::process::exit(2);
    }

    let cfg = ImpairConfig {
        loss_pct,
        dup_pct,
        reorder_pct,
        reorder_hold,
        jitter_ms_max,
        base_delay_ms,
        seed,
        outage_period_s,
        outage_dur_s,
    };

    match proxy::run(
        listen,
        forward,
        cfg,
        schedule,
        stats_json,
        run_seconds,
        None,
        None,
    ) {
        Ok(stats) => {
            eprintln!(
                "proxy: forwarded={} dropped={} duped={} reordered={}",
                stats.forwarded, stats.dropped, stats.duped, stats.reordered
            );
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("proxy: {e}");
            std::process::exit(2);
        }
    }
}

/// `pick-profiles --seed N --legs K`
///
/// Prints `K` distinct profile names, one per line, drawn
/// deterministically from `N` (see `profiles::pick`). Exists so
/// `soak.sh --profile auto` can select a per-leg profile without
/// reimplementing this crate's PRNG in bash — the shell reads the lines,
/// and the same seed reproduces the same run. Exits 0 on success, 2 on a
/// usage error (including `--legs` larger than the registry).
fn run_pick_profiles(args: &[String]) -> ! {
    let mut seed: Option<u64> = None;
    let mut legs: Option<usize> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seed" => {
                let raw = require_value(args, i, "pick-profiles: --seed");
                seed = Some(raw.parse().unwrap_or_else(|e| {
                    eprintln!(
                        "pick-profiles: --seed must be a non-negative integer, got '{raw}': {e}"
                    );
                    std::process::exit(2);
                }));
                i += 2;
            }
            "--legs" => {
                let raw = require_value(args, i, "pick-profiles: --legs");
                legs = Some(raw.parse().unwrap_or_else(|e| {
                    eprintln!("pick-profiles: --legs must be a positive integer, got '{raw}': {e}");
                    std::process::exit(2);
                }));
                i += 2;
            }
            other => {
                eprintln!("pick-profiles: unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let seed = seed.unwrap_or_else(|| {
        eprintln!("pick-profiles: --seed is required");
        std::process::exit(2);
    });
    let legs = legs.unwrap_or_else(|| {
        eprintln!("pick-profiles: --legs is required");
        std::process::exit(2);
    });
    if legs == 0 {
        eprintln!("pick-profiles: --legs must be at least 1");
        std::process::exit(2);
    }

    match profiles::pick(seed, legs) {
        Ok(names) => {
            for name in names {
                println!("{name}");
            }
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("pick-profiles: {e}");
            std::process::exit(2);
        }
    }
}

/// `report merge|render|soak` — dispatches to the `report` sub-subcommands.
fn run_report(args: &[String]) -> ! {
    if args.is_empty() {
        eprintln!("report: expected a subcommand (merge|render|soak)");
        std::process::exit(2);
    }
    match args[0].as_str() {
        "merge" => run_report_merge(&args[1..]),
        "render" => run_report_render(&args[1..]),
        "soak" => run_report_soak(&args[1..]),
        other => {
            eprintln!("report: unknown subcommand: {other}");
            std::process::exit(2);
        }
    }
}

/// `report merge --cells-dir DIR --expectations FILE --meta FILE
/// --inventory FILE --out results.json`
///
/// Reads every per-cell JSON file in `--cells-dir`, applies
/// `--expectations`, embeds `--meta` verbatim, validates the produced
/// cells against `--inventory`'s declared multiset, and writes `--out`.
/// Exits 1 iff any FAIL matched no expectation OR any expectation row is
/// stale (see `tst_interop::report`'s module doc for why the FAIL case
/// is the load-bearing property of the whole subcommand); exits 2 on a
/// usage/IO/parse error, including an inventory mismatch — in every exit-2
/// case, `--out` is not written.
fn run_report_merge(args: &[String]) -> ! {
    let mut cells_dir: Option<PathBuf> = None;
    let mut expectations: Option<PathBuf> = None;
    let mut meta: Option<PathBuf> = None;
    let mut inventory: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--cells-dir" => {
                cells_dir = Some(PathBuf::from(require_value(
                    args,
                    i,
                    "report merge: --cells-dir",
                )));
                i += 2;
            }
            "--expectations" => {
                expectations = Some(PathBuf::from(require_value(
                    args,
                    i,
                    "report merge: --expectations",
                )));
                i += 2;
            }
            "--meta" => {
                meta = Some(PathBuf::from(require_value(
                    args,
                    i,
                    "report merge: --meta",
                )));
                i += 2;
            }
            "--inventory" => {
                inventory = Some(PathBuf::from(require_value(
                    args,
                    i,
                    "report merge: --inventory",
                )));
                i += 2;
            }
            "--out" => {
                out = Some(PathBuf::from(require_value(args, i, "report merge: --out")));
                i += 2;
            }
            other => {
                eprintln!("report merge: unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let cells_dir = cells_dir.unwrap_or_else(|| {
        eprintln!("report merge: --cells-dir is required");
        std::process::exit(2);
    });
    let expectations = expectations.unwrap_or_else(|| {
        eprintln!("report merge: --expectations is required");
        std::process::exit(2);
    });
    let meta = meta.unwrap_or_else(|| {
        eprintln!("report merge: --meta is required");
        std::process::exit(2);
    });
    let inventory = inventory.unwrap_or_else(|| {
        eprintln!("report merge: --inventory is required");
        std::process::exit(2);
    });
    let out = out.unwrap_or_else(|| {
        eprintln!("report merge: --out is required");
        std::process::exit(2);
    });

    let results =
        report::merge(&cells_dir, &expectations, &meta, &inventory, &out).unwrap_or_else(|e| {
            eprintln!("report merge: {e}");
            std::process::exit(2);
        });

    for stale in &results.summary.stale_expectations {
        eprintln!(
            "report merge: ERROR stale expectation: cell={} profile={} reason={}",
            stale.cell, stale.profile, stale.reason
        );
    }

    eprintln!(
        "report merge: total={} pass={} fail={} expected_unsupported={} skipped={}",
        results.summary.total,
        results.summary.pass,
        results.summary.fail,
        results.summary.expected_unsupported,
        results.summary.skipped_tool_missing
    );

    std::process::exit(
        if results.summary.fail > 0 || !results.summary.stale_expectations.is_empty() {
            1
        } else {
            0
        },
    );
}

/// `report render --in results.json --out results.md [--github-summary]`
///
/// Renders `--in`'s `Results` JSON to markdown and writes it to `--out`.
/// `--github-summary` additionally appends the same markdown to the file
/// named by the `GITHUB_STEP_SUMMARY` environment variable, exiting 2 if
/// that variable is unset. Exits 2 on any usage/IO/parse error.
fn run_report_render(args: &[String]) -> ! {
    let mut in_path: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut github_summary = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--in" => {
                in_path = Some(PathBuf::from(require_value(args, i, "report render: --in")));
                i += 2;
            }
            "--out" => {
                out = Some(PathBuf::from(require_value(
                    args,
                    i,
                    "report render: --out",
                )));
                i += 2;
            }
            "--github-summary" => {
                github_summary = true;
                i += 1;
            }
            other => {
                eprintln!("report render: unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let in_path = in_path.unwrap_or_else(|| {
        eprintln!("report render: --in is required");
        std::process::exit(2);
    });
    let out = out.unwrap_or_else(|| {
        eprintln!("report render: --out is required");
        std::process::exit(2);
    });

    let md = report::render(&in_path, &out).unwrap_or_else(|e| {
        eprintln!("report render: {e}");
        std::process::exit(2);
    });

    if github_summary {
        let summary_path = env::var("GITHUB_STEP_SUMMARY").unwrap_or_else(|_| {
            eprintln!("report render: --github-summary given but GITHUB_STEP_SUMMARY is unset");
            std::process::exit(2);
        });
        if let Err(e) = report::append_github_summary(Path::new(&summary_path), &md) {
            eprintln!("report render: {e}");
            std::process::exit(2);
        }
    }

    eprintln!("report render: wrote {}", out.display());
    std::process::exit(0);
}

/// `report soak --rss FILE --config FILE --exits FILE --proxy-stats FILE
/// --recv-report FILE --send-report FILE --outage-period-s N
/// [--rist-proxy-stats FILE --rist-recv-report FILE --rist-send-report FILE]
/// [--rss-slope-threshold-kb-per-hour F] --out FILE`
///
/// Turns `soak.sh`'s raw artifacts into `soak-results.json` — see
/// `tst_interop::report::soak`'s module doc for the verdict shapes and
/// their documented telemetry limitations.
///
/// `--proxy-stats`/`--recv-report`/`--send-report`/`--outage-period-s`
/// describe the `srt` leg (scheduled outage + managed-reconnect
/// sender). The three `--rist-*` flags describe the second,
/// sustained-impairment-only leg and must be given together or not at
/// all (that leg has no outage schedule, hence no matching
/// `--rist-outage-period-s` flag) — omit all three for a single-leg
/// (srt-only) run, e.g. a local smoke test.
///
/// `report soak --config FILE --validate-only` (no other flag accepted)
/// parses and validates `soak-config.json` alone — `soak.sh` calls this
/// right after writing that file, at launch, so a config that could
/// never pass `duration_coverage`/`rss_sample_coverage_*` (e.g. an
/// `--hours` value so small the sampler's end slack and warmup consume
/// the whole run) fails fast instead of only being discovered hours
/// later when the real `report soak` invocation runs at teardown. Same
/// validation `parse_soak_config` always runs — one source of truth.
/// Exits 0 if the config is valid, 2 otherwise.
///
/// Exits 1 iff the resulting `SoakResults::overall_pass` is false, 2 on
/// a usage/IO/parse error.
fn run_report_soak(args: &[String]) -> ! {
    let mut rss: Option<PathBuf> = None;
    let mut config: Option<PathBuf> = None;
    let mut exits: Option<PathBuf> = None;
    let mut proxy_stats: Option<PathBuf> = None;
    let mut recv_report: Option<PathBuf> = None;
    let mut send_report: Option<PathBuf> = None;
    let mut outage_period_s: Option<u64> = None;
    let mut rist_proxy_stats: Option<PathBuf> = None;
    let mut rist_recv_report: Option<PathBuf> = None;
    let mut rist_send_report: Option<PathBuf> = None;
    let mut rss_slope_threshold: Option<f64> = None;
    let mut out: Option<PathBuf> = None;
    let mut validate_only = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--rss" => {
                rss = Some(PathBuf::from(require_value(args, i, "report soak: --rss")));
                i += 2;
            }
            "--config" => {
                config = Some(PathBuf::from(require_value(
                    args,
                    i,
                    "report soak: --config",
                )));
                i += 2;
            }
            "--exits" => {
                exits = Some(PathBuf::from(require_value(
                    args,
                    i,
                    "report soak: --exits",
                )));
                i += 2;
            }
            "--proxy-stats" => {
                proxy_stats = Some(PathBuf::from(require_value(
                    args,
                    i,
                    "report soak: --proxy-stats",
                )));
                i += 2;
            }
            "--recv-report" => {
                recv_report = Some(PathBuf::from(require_value(
                    args,
                    i,
                    "report soak: --recv-report",
                )));
                i += 2;
            }
            "--send-report" => {
                send_report = Some(PathBuf::from(require_value(
                    args,
                    i,
                    "report soak: --send-report",
                )));
                i += 2;
            }
            "--outage-period-s" => {
                let v = require_value(args, i, "report soak: --outage-period-s");
                outage_period_s = Some(v.parse().unwrap_or_else(|_| {
                    eprintln!(
                        "report soak: --outage-period-s must be a non-negative integer, got '{v}'"
                    );
                    std::process::exit(2);
                }));
                i += 2;
            }
            "--rist-proxy-stats" => {
                rist_proxy_stats = Some(PathBuf::from(require_value(
                    args,
                    i,
                    "report soak: --rist-proxy-stats",
                )));
                i += 2;
            }
            "--rist-recv-report" => {
                rist_recv_report = Some(PathBuf::from(require_value(
                    args,
                    i,
                    "report soak: --rist-recv-report",
                )));
                i += 2;
            }
            "--rist-send-report" => {
                rist_send_report = Some(PathBuf::from(require_value(
                    args,
                    i,
                    "report soak: --rist-send-report",
                )));
                i += 2;
            }
            "--rss-slope-threshold-kb-per-hour" => {
                let v = require_value(args, i, "report soak: --rss-slope-threshold-kb-per-hour");
                rss_slope_threshold = Some(v.parse().unwrap_or_else(|_| {
                    eprintln!(
                        "report soak: --rss-slope-threshold-kb-per-hour must be a number, got '{v}'"
                    );
                    std::process::exit(2);
                }));
                i += 2;
            }
            "--out" => {
                out = Some(PathBuf::from(require_value(args, i, "report soak: --out")));
                i += 2;
            }
            "--validate-only" => {
                validate_only = true;
                i += 1;
            }
            other => {
                eprintln!("report soak: unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    if validate_only {
        let other_flag_given = rss.is_some()
            || exits.is_some()
            || proxy_stats.is_some()
            || recv_report.is_some()
            || send_report.is_some()
            || outage_period_s.is_some()
            || rist_proxy_stats.is_some()
            || rist_recv_report.is_some()
            || rist_send_report.is_some()
            || rss_slope_threshold.is_some()
            || out.is_some();
        if other_flag_given {
            eprintln!("report soak: --validate-only accepts no flag other than --config");
            std::process::exit(2);
        }
        let config = config.unwrap_or_else(|| {
            eprintln!("report soak: --validate-only requires --config");
            std::process::exit(2);
        });
        let text = std::fs::read_to_string(&config).unwrap_or_else(|e| {
            eprintln!("report soak: read {}: {e}", config.display());
            std::process::exit(2);
        });
        match report::soak::parse_soak_config(&text) {
            Ok(_) => {
                eprintln!(
                    "report soak: --validate-only: {} is valid",
                    config.display()
                );
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("report soak: {e}");
                std::process::exit(2);
            }
        }
    }

    let rss = rss.unwrap_or_else(|| {
        eprintln!("report soak: --rss is required");
        std::process::exit(2);
    });
    let config = config.unwrap_or_else(|| {
        eprintln!("report soak: --config is required");
        std::process::exit(2);
    });
    let exits = exits.unwrap_or_else(|| {
        eprintln!("report soak: --exits is required");
        std::process::exit(2);
    });
    let proxy_stats = proxy_stats.unwrap_or_else(|| {
        eprintln!("report soak: --proxy-stats is required");
        std::process::exit(2);
    });
    let recv_report = recv_report.unwrap_or_else(|| {
        eprintln!("report soak: --recv-report is required");
        std::process::exit(2);
    });
    let send_report = send_report.unwrap_or_else(|| {
        eprintln!("report soak: --send-report is required");
        std::process::exit(2);
    });
    let outage_period_s = outage_period_s.unwrap_or_else(|| {
        eprintln!("report soak: --outage-period-s is required");
        std::process::exit(2);
    });
    let out = out.unwrap_or_else(|| {
        eprintln!("report soak: --out is required");
        std::process::exit(2);
    });

    let rist_given = [&rist_proxy_stats, &rist_recv_report, &rist_send_report]
        .iter()
        .filter(|f| f.is_some())
        .count();
    if rist_given != 0 && rist_given != 3 {
        eprintln!(
            "report soak: --rist-proxy-stats/--rist-recv-report/--rist-send-report must be \
             given together or not at all"
        );
        std::process::exit(2);
    }
    let rist = if rist_given == 3 {
        Some((
            rist_proxy_stats.as_deref().expect("checked above"),
            rist_recv_report.as_deref().expect("checked above"),
            rist_send_report.as_deref().expect("checked above"),
        ))
    } else {
        None
    };

    let results = report::soak::run(
        &rss,
        &config,
        &exits,
        &proxy_stats,
        &recv_report,
        &send_report,
        outage_period_s,
        rist,
        rss_slope_threshold,
        &out,
    )
    .unwrap_or_else(|e| {
        eprintln!("report soak: {e}");
        std::process::exit(2);
    });

    eprintln!(
        "report soak: overall_pass={} ({} verdict(s), {} provisional)",
        results.overall_pass,
        results.verdicts.len(),
        results.verdicts.iter().filter(|v| v.provisional).count()
    );
    std::process::exit(if results.overall_pass { 0 } else { 1 });
}
