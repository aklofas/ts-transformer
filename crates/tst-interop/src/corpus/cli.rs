//! Argument handling and output for `tst-interop corpus`. Output names
//! captures by SHA-256 only; no path is ever printed.

use std::path::PathBuf;

use super::sweep::{self, FileOutcome, SweepArgs, SweepSummary};
use super::{RunDir, Sha256Hex, inventory};

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
/// sweep does not pass, 2 on a usage or I/O error.
pub fn run(args: &[String]) -> ! {
    let Some(sub) = args.first() else {
        fail("a subcommand is required: inventory | sweep");
    };
    let rest = &args[1..];
    match sub.as_str() {
        "inventory" => run_inventory(rest),
        "sweep" => run_sweep(rest),
        // Internal: the replay stage's child process, not in the usage text.
        "receive" => run_receive(rest),
        other => fail(&format!("unknown subcommand '{other}'")),
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
                accepts.push((sha, value(args, i, "--because")));
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
