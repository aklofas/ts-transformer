//! Argument handling and output for `tst-interop corpus`. Output names
//! captures by SHA-256 only; no path is ever printed.

use std::path::PathBuf;

use super::judge::FileVerdict;
use super::replay::{self, ReplayArgs, ReplaySummary};
use super::sweep::{self, FileOutcome, SweepArgs, SweepSummary};
use super::{PATHS, REPLAY_BUDGET_SECONDS, RunDir, Sha256Hex, inventory};

fn fail(msg: &str) -> ! {
    eprintln!("corpus: {msg}");
    std::process::exit(2);
}

fn value(args: &[String], i: usize, flag: &str) -> String {
    match args.get(i + 1) {
        Some(v) if !v.starts_with("--") => v.clone(),
        _ => fail(&format!("{flag} requires a value")),
    }
}

/// Entry point for `corpus <subcommand> ...`; exits 0 on success, 1 when a
/// sweep or a replay does not pass, 2 on a usage or I/O error.
pub fn run(args: &[String]) -> ! {
    note_test_knobs();
    let Some(sub) = args.first() else {
        fail("a subcommand is required: inventory | sweep | replay | report");
    };
    let rest = &args[1..];
    match sub.as_str() {
        "inventory" => run_inventory(rest),
        "sweep" => run_sweep(rest),
        "replay" => run_replay(rest),
        "report" => run_report(rest),
        // Internal: the replay stage's child process, not in the usage text.
        "receive" => run_receive(rest),
        other => fail(&format!("unknown subcommand '{other}'")),
    }
}

/// The prefix of the variables that change the replay's rules for tests.
const TEST_KNOB_PREFIX: &str = "TST_CORPUS_TEST_";

/// One stderr line per test knob set in the environment, naming it (never
/// its value), so a stray knob on a release machine is visible.
fn note_test_knobs() {
    for (name, _) in std::env::vars_os() {
        if let Some(name) = name.to_str()
            && name.starts_with(TEST_KNOB_PREFIX)
        {
            eprintln!(
                "corpus: test knob {name} is set; the replay's rules are not the release ones"
            );
        }
    }
}

struct Common {
    dir: PathBuf,
    out: PathBuf,
    baseline: Option<PathBuf>,
    accepts: Vec<(Sha256Hex, String)>,
    annotations: Option<PathBuf>,
}

fn parse(args: &[String], sweep_flags: bool) -> Common {
    let mut dir = None;
    let mut out = None;
    let mut baseline = None;
    let mut accepts: Vec<(Sha256Hex, String)> = Vec::new();
    let mut annotations = None;
    let mut pending: Option<Sha256Hex> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                out = Some(PathBuf::from(value(args, i, "--out")));
                i += 2;
            }
            "--baseline" if sweep_flags => {
                baseline = Some(PathBuf::from(value(args, i, "--baseline")));
                i += 2;
            }
            "--annotations" if sweep_flags => {
                annotations = Some(PathBuf::from(value(args, i, "--annotations")));
                i += 2;
            }
            "--accept" if sweep_flags => {
                if pending.is_some() {
                    fail("--accept requires a following --because");
                }
                pending = Some(Sha256Hex(value(args, i, "--accept")));
                i += 2;
            }
            "--because" if sweep_flags => {
                let Some(sha) = pending.take() else {
                    fail("--because must follow an --accept");
                };
                let why = value(args, i, "--because");
                if why.trim().is_empty() {
                    fail("--because needs a non-empty reason");
                }
                accepts.push((sha, why));
                i += 2;
            }
            a if a.starts_with("--") => fail("unknown option"),
            _ => {
                if dir.is_some() {
                    fail("only one directory is accepted");
                }
                dir = Some(PathBuf::from(&args[i]));
                i += 1;
            }
        }
    }
    if pending.is_some() {
        fail("--accept requires a following --because");
    }
    Common {
        dir: dir.unwrap_or_else(|| fail("a directory is required")),
        out: out.unwrap_or_else(|| fail("--out is required")),
        baseline,
        accepts,
        annotations,
    }
}

fn run_inventory(args: &[String]) -> ! {
    let c = parse(args, false);
    let run = RunDir { root: c.out };
    let inv = inventory::build(&c.dir).unwrap_or_else(|e| fail(&e));
    inventory::write(&inv, &run).unwrap_or_else(|e| fail(&e));
    let bad = inv.files.iter().filter(|f| f.probe_error.is_some()).count();
    println!(
        "inventory: {} distinct files, {bad} unprobeable",
        inv.files.len()
    );
    std::process::exit(0);
}

fn outcome_label(o: &FileOutcome) -> String {
    match o {
        FileOutcome::Identical => "identical".into(),
        FileOutcome::Changed { explained: Some(_) } => "changed-explained".into(),
        FileOutcome::Changed { explained: None } => "changed-unexplained".into(),
        FileOutcome::NewNoBaseline => "new-no-baseline".into(),
        FileOutcome::ParseFailure(why) => format!("parse-failure {why}"),
    }
}

fn print_summary(s: &SweepSummary) {
    println!("files: {}", s.files);
    println!("identical: {}", s.identical);
    println!("changed-explained: {}", s.changed_explained);
    println!("changed-unexplained: {}", s.changed_unexplained);
    println!("new-no-baseline: {}", s.new_no_baseline);
    println!("parse-failures: {}", s.parse_failures);
    println!("invariant-failures: {}", s.invariant_failures);
    for f in &s.per_file {
        if f.outcome != FileOutcome::Identical {
            println!("{} {}", f.sha256, outcome_label(&f.outcome));
        }
    }
    for sha in &s.unused_accepts {
        println!("{sha} unused-accept");
    }
    if s.baseline_matched == Some(0) {
        println!("baseline matched 0 files");
    }
    println!("{}", if s.pass { "pass" } else { "fail" });
}

fn run_sweep(args: &[String]) -> ! {
    let c = parse(args, true);
    let sweep_args = SweepArgs {
        dir: c.dir,
        run: RunDir { root: c.out },
        baseline: c.baseline,
        accepts: c.accepts,
        annotations: c.annotations,
    };
    match sweep::run(&sweep_args) {
        Ok(s) => {
            print_summary(&s);
            std::process::exit(if s.pass { 0 } else { 1 });
        }
        Err(e) => fail(&e),
    }
}

fn verdict_line(v: &FileVerdict) -> String {
    let mut line = format!(
        "{} {} {}",
        v.sha256,
        v.path,
        if v.pass { "PASS" } else { "FAIL" }
    );
    if let Some(first) = v.failures.first() {
        line.push(' ');
        line.push_str(first);
    }
    line
}

fn print_replay_summary(s: &ReplaySummary) {
    println!("files: {}", s.files);
    println!("paths: {}", s.paths.join(","));
    println!("verdicts: {}", s.verdicts.len());
    println!("failed: {}", s.failed);
    for (path, max) in &s.rss_max_kb_by_path {
        let slope = s
            .rss_slope_kb_per_hour_by_path
            .get(path)
            .copied()
            .unwrap_or(0.0);
        println!("rss {path}: max {max} kB, slope {slope:.1} kB/h");
    }
    for (path, gate) in &s.rss_gate_by_path {
        println!("rss slope {path} {gate}");
    }
    println!("{}", if s.pass { "pass" } else { "fail" });
}

fn run_replay(args: &[String]) -> ! {
    let (mut dir, mut out) = (None, None);
    let mut paths: Vec<String> = PATHS.iter().map(|p| p.to_string()).collect();
    let mut budget_seconds = REPLAY_BUDGET_SECONDS;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => out = Some(PathBuf::from(value(args, i, "--out"))),
            "--paths" => {
                paths = value(args, i, "--paths")
                    .split(',')
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            "--budget-seconds" => {
                budget_seconds = value(args, i, "--budget-seconds")
                    .parse()
                    .unwrap_or_else(|_| fail("--budget-seconds must be a whole number of seconds"));
            }
            a if a.starts_with("--") => fail("unknown option"),
            _ => {
                if dir.is_some() {
                    fail("only one directory is accepted");
                }
                dir = Some(PathBuf::from(&args[i]));
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    let replay_args = ReplayArgs {
        dir: dir.unwrap_or_else(|| fail("a directory is required")),
        run: RunDir {
            root: out.unwrap_or_else(|| fail("--out is required")),
        },
        paths,
        budget_seconds,
        rss_sample_seconds: 10,
    };
    match replay::run_with(&replay_args, &mut |v| println!("{}", verdict_line(v))) {
        Ok(s) => {
            print_replay_summary(&s);
            std::process::exit(if s.pass { 0 } else { 1 });
        }
        Err(e) => fail(&e),
    }
}

fn run_report(args: &[String]) -> ! {
    let (mut run, mut markdown, mut archive) = (None, None, None);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--markdown" => markdown = Some(PathBuf::from(value(args, i, "--markdown"))),
            "--archive-row" => archive = Some(PathBuf::from(value(args, i, "--archive-row"))),
            a if a.starts_with("--") => fail("unknown option"),
            _ => {
                if run.is_some() {
                    fail("only one run directory is accepted");
                }
                run = Some(PathBuf::from(&args[i]));
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    let run = RunDir {
        root: run.unwrap_or_else(|| fail("a run directory is required")),
    };
    // A failed run is still reported: this exits 0 or 2 only.
    match super::report::run(&run, markdown.as_deref(), archive.as_deref()) {
        Ok(row) => {
            println!("{}", row.note);
            std::process::exit(0);
        }
        Err(e) => fail(&e),
    }
}

fn run_receive(args: &[String]) -> ! {
    let (mut path, mut digests, mut out) = (None, None, None);
    let mut i = 0;
    while i < args.len() {
        let slot = match args[i].as_str() {
            "--path" => &mut path,
            "--digests" => &mut digests,
            "--out" => &mut out,
            _ => fail("unknown option"),
        };
        *slot = Some(value(args, i, &args[i]));
        i += 2;
    }
    let (Some(path), Some(digests), Some(out)) = (path, digests, out) else {
        fail("--path, --digests and --out are required");
    };
    std::process::exit(super::receive::run(
        &path,
        std::path::Path::new(&digests),
        std::path::Path::new(&out),
    ));
}
