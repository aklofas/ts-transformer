//! `report step` / `report stress`: the stress harness's verdicts.
//!
//! One STEP is one load level of the sweep (N process-triple streams
//! of one transport at one `--au-scale`) held for a fixed window, or
//! the 24 h hold. `build_step_results` judges a step from the sampler
//! CSVs (`rss.csv`, `proc.csv`, `host.csv` — the same shapes `soak.sh`
//! writes) plus the per-stream send/recv/proxy reports; `run_step` is
//! its file-driven wrapper. `build_stress_results` folds every step's
//! `step-results.json` into one `stress-results.json` with the ceiling
//! rule (last PASS before the first FAIL on each axis).
//!
//! Resource verdicts exclude a declared warm-up and use percentiles,
//! never averages. A missing or malformed artifact is an `Err`, never a
//! vacuous PASS (the same stance as `super::soak`).

use serde::{Deserialize, Serialize};

const PROC_HEADER: &str = "elapsed_s,leg,process,pid,utime_ticks,stime_ticks,threads,fds";
const HOST_HEADER: &str = "elapsed_s,load1,load5,load15,procs_running,mem_available_kb";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcSample {
    pub elapsed_s: f64,
    pub leg: String,
    pub process: String,
    pub pid: u32,
    pub utime_ticks: Option<u64>,
    pub stime_ticks: Option<u64>,
    pub threads: Option<u64>,
    pub fds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostSample {
    pub elapsed_s: f64,
    pub load1: Option<f64>,
    pub load5: Option<f64>,
    pub load15: Option<f64>,
    pub procs_running: Option<u64>,
    pub mem_available_kb: Option<u64>,
}

fn opt<T: std::str::FromStr>(field: &str, name: &str, line_no: usize) -> Result<Option<T>, String> {
    if field.is_empty() {
        return Ok(None);
    }
    field
        .parse::<T>()
        .map(Some)
        .map_err(|_| format!("line {line_no}: {name} must be numeric or empty, got {field:?}"))
}

fn csv_rows<'a>(
    text: &'a str,
    header: &str,
    columns: usize,
    file: &str,
) -> Result<Vec<(usize, Vec<&'a str>)>, String> {
    let mut lines = text.lines().enumerate();
    let Some((_, first)) = lines.next() else {
        return Err(format!("{file}: empty file (expected a header line)"));
    };
    if first.trim() != header {
        return Err(format!(
            "{file} line 1: expected header `{header}`, got: {first:?}"
        ));
    }
    let mut rows = Vec::new();
    for (idx, raw) in lines {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() != columns {
            return Err(format!(
                "{file} line {}: expected {columns} fields, got {}: {line:?}",
                idx + 1,
                fields.len()
            ));
        }
        rows.push((idx + 1, fields));
    }
    Ok(rows)
}

pub fn parse_proc_csv(text: &str) -> Result<Vec<ProcSample>, String> {
    csv_rows(text, PROC_HEADER, 8, "proc.csv")?
        .into_iter()
        .map(|(n, f)| {
            Ok(ProcSample {
                elapsed_s: f[0]
                    .parse()
                    .map_err(|_| format!("proc.csv line {n}: bad elapsed_s"))?,
                leg: f[1].to_string(),
                process: f[2].to_string(),
                pid: f[3]
                    .parse()
                    .map_err(|_| format!("proc.csv line {n}: bad pid"))?,
                utime_ticks: opt(f[4], "utime_ticks", n)?,
                stime_ticks: opt(f[5], "stime_ticks", n)?,
                threads: opt(f[6], "threads", n)?,
                fds: opt(f[7], "fds", n)?,
            })
        })
        .collect()
}

pub fn parse_host_csv(text: &str) -> Result<Vec<HostSample>, String> {
    csv_rows(text, HOST_HEADER, 6, "host.csv")?
        .into_iter()
        .map(|(n, f)| {
            Ok(HostSample {
                elapsed_s: f[0]
                    .parse()
                    .map_err(|_| format!("host.csv line {n}: bad elapsed_s"))?,
                load1: opt(f[1], "load1", n)?,
                load5: opt(f[2], "load5", n)?,
                load15: opt(f[3], "load15", n)?,
                procs_running: opt(f[4], "procs_running", n)?,
                mem_available_kb: opt(f[5], "mem_available_kb", n)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_csv_parses_rows_and_empty_fields() {
        let text = "elapsed_s,leg,process,pid,utime_ticks,stime_ticks,threads,fds\n\
                    0,srt-0,send,100,12,3,4,5\n\
                    30,srt-0,send,100,,,,\n";
        let rows = parse_proc_csv(text).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].utime_ticks, Some(12));
        assert_eq!(rows[0].fds, Some(5));
        assert_eq!(rows[1].utime_ticks, None);
        assert_eq!(rows[1].leg, "srt-0");
    }

    #[test]
    fn proc_csv_rejects_wrong_header_and_short_rows() {
        assert!(
            parse_proc_csv("elapsed_s,leg\n")
                .unwrap_err()
                .contains("line 1")
        );
        let bad = "elapsed_s,leg,process,pid,utime_ticks,stime_ticks,threads,fds\n0,srt,send,1,2\n";
        assert!(parse_proc_csv(bad).unwrap_err().contains("line 2"));
    }

    #[test]
    fn host_csv_parses_load_and_memory() {
        let text = "elapsed_s,load1,load5,load15,procs_running,mem_available_kb\n\
                    30,0.50,0.40,0.30,2,1000000\n60,,,,,\n";
        let rows = parse_host_csv(text).unwrap();
        assert_eq!(rows[0].load1, Some(0.5));
        assert_eq!(rows[0].mem_available_kb, Some(1_000_000));
        assert_eq!(rows[1].load1, None);
    }
}
