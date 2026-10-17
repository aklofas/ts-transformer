//! Maintainer tool: renders the Python and JVM projections of every record
//! declared with `tst_core::binding_record!` and listed in the registry.
//!
//!   print-records --list        each record, then the record and target counts
//!   print-records --write       write every generated file and block
//!   print-records --check       exit 1 (listing every path) if a committed copy
//!                               differs from a fresh render, or generated text
//!                               is left that no registered record renders
//!   --root `<dir>`              the workspace root (default: the current directory)
//!
//! Not learner code. `scripts/check/repo/binding-records.sh` runs `--check`.

use std::path::PathBuf;

#[path = "print_records/emit_java.rs"]
mod emit_java;
#[path = "print_records/emit_jvm_rs.rs"]
mod emit_jvm_rs;
#[path = "print_records/emit_py.rs"]
mod emit_py;
#[path = "print_records/emit_pyclass.rs"]
mod emit_pyclass;
#[path = "print_records/emit_pyi.rs"]
mod emit_pyi;
#[path = "print_records/schema.rs"]
mod schema;
#[path = "print_records/stats.rs"]
mod stats;
#[path = "print_records/typemap.rs"]
mod typemap;
#[path = "print_records/write.rs"]
mod write;

fn usage(msg: &str) -> ! {
    eprintln!("print-records: {msg}");
    eprintln!("usage: print-records [--list | --write | --check] [--root <dir>]");
    std::process::exit(2);
}

fn main() {
    let mut mode = "--list".to_string();
    let mut root = PathBuf::from(".");
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--list" | "--write" | "--check" => mode = a,
            "--root" => match it.next() {
                Some(d) => root = PathBuf::from(d),
                None => usage("--root takes a directory"),
            },
            other => usage(&format!("unknown argument `{other}`")),
        }
    }
    let reg = schema::records();
    if mode == "--write" {
        let result = write::targets(&root, &reg)
            .and_then(|t| write::formatted(t, &root))
            .and_then(|t| write::write_all(&t));
        if let Err(e) = result {
            eprintln!("print-records: {e}");
            std::process::exit(2);
        }
    } else if mode == "--check" {
        let targets = match write::targets(&root, &reg).and_then(|t| write::formatted(t, &root)) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("print-records: {e}");
                std::process::exit(2);
            }
        };
        let stale = write::stale(&targets);
        let orphans = write::orphans(&root, &targets);
        for (p, why) in stale.iter().chain(&orphans) {
            eprintln!("binding-records: {} {why}", p.display());
        }
        if !stale.is_empty() {
            eprintln!("Regenerate: cargo run -p tst-binding-gen --bin print-records -- --write");
        }
        if !orphans.is_empty() {
            eprintln!("Delete the generated files and blocks no registered record renders.");
        }
        if !stale.is_empty() || !orphans.is_empty() {
            std::process::exit(1);
        }
        println!("binding-records: OK ({} records)", reg.len());
    } else {
        let targets = match write::targets(&root, &reg) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("print-records: {e}");
                std::process::exit(2);
            }
        };
        for e in &reg {
            println!("{} {}", e.family, e.schema.name);
        }
        println!("{} records → {} targets", reg.len(), targets.len());
    }
}
