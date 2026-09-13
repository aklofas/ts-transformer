use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use tst_interop::cli;
use tst_interop::fixtures::AuSizeMode;
use tst_interop::r#gen;
use tst_interop::impair::ImpairConfig;
use tst_interop::profiles;
use tst_interop::proxy;
use tst_interop::recv;
use tst_interop::report;
use tst_interop::send;
use tst_interop::serve;
use tst_interop::verify;

fn usage() -> String {
    "usage: tst-interop <subcommand> [options...]

Subcommands:
  gen       Generate synthetic test fixtures
  send      Send test data to endpoint (hls:// and rtsp:// URLs BIND and
            serve instead of connecting — see `send`'s own doc comment)
  recv      Receive test data from endpoint
  verify    Verify interop test results
  proxy     UDP impairment relay (loss/dup/reorder/jitter/scheduled outage)
  report    Generate interop report

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
        _ => {
            eprintln!("Unknown subcommand: {}", subcommand);
            println!("{}", usage());
            std::process::exit(2);
        }
    }
}

/// `gen --profile NAME --seconds N --out PATH`
///
/// Generates `N` seconds of profile `NAME`'s synthetic MPEG-TS/KLV traffic
/// (offline pacing, no transport) and writes it to `PATH`. Exits 0 on
/// success, 2 on usage/IO error.
fn run_gen(args: &[String]) -> ! {
    let mut profile: Option<String> = None;
    let mut seconds: Option<f64> = None;
    let mut out: Option<PathBuf> = None;

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

    if let Err(e) = r#gen::run(p, seconds, &out) {
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
/// [--no-klv-digest] [--au-sizes compact|realistic]`
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
fn run_send(args: &[String]) -> ! {
    let mut profile: Option<String> = None;
    let mut url: Option<String> = None;
    let mut seconds: Option<f64> = None;
    let mut json_out: Option<String> = None;
    let mut managed = false;
    let mut no_klv_digest = false;
    let mut au_sizes = AuSizeMode::Compact;

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
            "--no-klv-digest" => {
                no_klv_digest = true;
                i += 1;
            }
            "--au-sizes" => {
                au_sizes = match require_value(args, i, "send: --au-sizes").as_str() {
                    "compact" => AuSizeMode::Compact,
                    "realistic" => AuSizeMode::Realistic,
                    other => {
                        eprintln!(
                            "send: --au-sizes must be 'compact' or 'realistic', got '{other}'"
                        );
                        std::process::exit(2);
                    }
                };
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

    // hls:// / rtsp:// (+ TLS variants) are serve (bind) modes — branch
    // out before the connect-side transport path below.
    if let Some(scheme) = serve::serve_scheme_of(&url) {
        if managed {
            eprintln!(
                "send: --managed is not meaningful for {url} (hls/rtsp are serve/bind modes, \
                 not connect modes — nothing to reconnect)"
            );
            std::process::exit(2);
        }
        let result = match scheme {
            serve::ServeScheme::Hls => serve::run_hls_url(p, &url, seconds),
            serve::ServeScheme::Rtsp => serve::run_rtsp_url(p, &url, seconds),
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
        )
    } else {
        send::run(
            p,
            &url,
            seconds,
            json_out.as_deref(),
            no_klv_digest,
            au_sizes,
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
/// [--managed] [--no-klv-digest]`
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
fn run_recv(args: &[String]) -> ! {
    let mut url: Option<String> = None;
    let mut expect: Option<String> = None;
    let mut seconds: Option<f64> = None;
    let mut json_out: Option<String> = None;
    let mut managed = false;
    let mut no_klv_digest = false;

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

    let report = if managed {
        recv::run_managed(&url, profile, seconds, json_out.as_deref(), no_klv_digest)
    } else {
        recv::run(&url, profile, seconds, json_out.as_deref(), no_klv_digest)
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

/// `verify --file F --expect PROFILE --seconds N [--json OUT]`
///
/// Demuxes `F` and checks it against `PROFILE`'s invariants for an
/// `N`-second capture. Exits 0 on pass, 1 on fail, 2 on usage/IO error.
/// `--json OUT` additionally writes the full `VerifyReport` as JSON to
/// `OUT` (or stdout, if `OUT` is `-`).
fn run_verify(args: &[String]) -> ! {
    let mut file: Option<PathBuf> = None;
    let mut expect: Option<String> = None;
    let mut seconds: Option<f64> = None;
    let mut json_out: Option<String> = None;

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

    let report = verify::verify_file(&file, profile, seconds).unwrap_or_else(|e| {
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
/// [--outage period=DUR,dur=DUR] [--stats-json PATH] [--run-seconds N]`
///
/// `--delay` is a constant base delay applied to every non-dropped
/// packet (a link's one-way WAN latency), on top of which `--jitter`
/// varies — see `ImpairConfig::base_delay_ms`.
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
                i += 2;
            }
            "--jitter" => {
                jitter_ms_max = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| bad_arg("jitter", "a non-negative integer (milliseconds)"));
                i += 2;
            }
            "--delay" => {
                base_delay_ms = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| bad_arg("delay", "a non-negative integer (milliseconds)"));
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

    match proxy::run(listen, forward, cfg, stats_json, run_seconds, None, None) {
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

/// `report merge --cells-dir DIR --expectations FILE --meta FILE --out
/// results.json`
///
/// Reads every per-cell JSON file in `--cells-dir`, applies
/// `--expectations`, embeds `--meta` verbatim, and writes `--out`. Exits
/// 1 iff any cell's `FAIL` matched no expectation (see
/// `tst_interop::report`'s module doc for why this is the load-bearing
/// property of the whole subcommand); exits 2 on a usage/IO/parse error.
fn run_report_merge(args: &[String]) -> ! {
    let mut cells_dir: Option<PathBuf> = None;
    let mut expectations: Option<PathBuf> = None;
    let mut meta: Option<PathBuf> = None;
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
    let out = out.unwrap_or_else(|| {
        eprintln!("report merge: --out is required");
        std::process::exit(2);
    });

    let results = report::merge(&cells_dir, &expectations, &meta, &out).unwrap_or_else(|e| {
        eprintln!("report merge: {e}");
        std::process::exit(2);
    });

    for stale in &results.summary.stale_expectations {
        eprintln!(
            "report merge: WARNING stale expectation: cell={} profile={} reason={}",
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

    std::process::exit(if results.summary.fail > 0 { 1 } else { 0 });
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
