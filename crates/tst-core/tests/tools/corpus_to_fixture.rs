//! corpus-to-fixture — extract a minimal TS-packet sub-sequence from a `.ts`
//! file as a committed regression fixture. Optional Rust shim generation.
//!
//! See `docs/cookbook/operations/capture-regression-fixture.md` and
//! `crates/tst-core/tests/fixtures/regression/README.md` for the workflow.
//!
//! Usage:
//!
//!   cargo run -p tst-core --bin corpus-to-fixture -- \
//!     --input /path/to/sample.ts \
//!     --pid 0x1011 \
//!     --packets 1000..2000 \
//!     --out crates/tst-core/tests/fixtures/regression/bug_xyz.bin \
//!     --emit-shim
//!
//! Flag reference:
//!   --input PATH           input .ts file (188-byte-aligned)
//!   --pid HEXDEC           filter to packets with this PID (optional)
//!   --packets START..END   packet-index range, 0-indexed, half-open (optional)
//!   --out PATH             output .bin file (must be under tests/fixtures/regression/)
//!   --emit-shim            also emit `tests/regression_<slug>.rs` with a
//!                          no-panic smoke test (optional)
//!   -h, --help             print this help

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

// Used in run() for 188-byte alignment validation and packet slicing.
const TS_PACKET_SIZE: usize = 188;

#[derive(Debug, PartialEq, Eq)]
struct Args {
    input: PathBuf,
    out: PathBuf,
    pid: Option<u16>,
    packets: Option<(usize, usize)>, // half-open [start, end)
    emit_shim: bool,
}

fn parse_args(raw: &[String]) -> Result<Args, String> {
    let mut input: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut pid: Option<u16> = None;
    let mut packets: Option<(usize, usize)> = None;
    let mut emit_shim = false;
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--input" => {
                input = Some(PathBuf::from(
                    raw.get(i + 1).ok_or("--input needs a value")?,
                ));
                i += 2;
            }
            "--out" => {
                out = Some(PathBuf::from(raw.get(i + 1).ok_or("--out needs a value")?));
                i += 2;
            }
            "--pid" => {
                let s = raw.get(i + 1).ok_or("--pid needs a value")?;
                pid = Some(parse_u16_dec_or_hex(s)?);
                i += 2;
            }
            "--packets" => {
                let s = raw.get(i + 1).ok_or("--packets needs a value")?;
                packets = Some(parse_range(s)?);
                i += 2;
            }
            "--emit-shim" => {
                emit_shim = true;
                i += 1;
            }
            "-h" | "--help" => return Err("HELP".to_string()),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Args {
        input: input.ok_or("missing --input")?,
        out: out.ok_or("missing --out")?,
        pid,
        packets,
        emit_shim,
    })
}

fn parse_u16_dec_or_hex(s: &str) -> Result<u16, String> {
    let (radix, body) = if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        (16, rest)
    } else {
        (10, s)
    };
    u16::from_str_radix(body, radix).map_err(|e| format!("invalid PID {s:?}: {e}"))
}

fn parse_range(s: &str) -> Result<(usize, usize), String> {
    let (start, end) = s
        .split_once("..")
        .ok_or_else(|| format!("range must be START..END, got {s:?}"))?;
    let start: usize = start.parse().map_err(|e| format!("bad range start: {e}"))?;
    let end: usize = end.parse().map_err(|e| format!("bad range end: {e}"))?;
    if start >= end {
        return Err(format!("range start {start} >= end {end}"));
    }
    Ok((start, end))
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(e) if e == "HELP" => {
            print_help();
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}");
            print_help();
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn print_help() {
    eprintln!("{}", include_str!("corpus_to_fixture_help.txt"));
}

fn run(args: &Args) -> Result<(), String> {
    let bytes = fs::read(&args.input).map_err(|e| format!("read {}: {e}", args.input.display()))?;
    if bytes.is_empty() {
        return Err(format!("{} is empty", args.input.display()));
    }
    if bytes.len() % TS_PACKET_SIZE != 0 {
        return Err(format!(
            "{} is not a multiple of 188 bytes (got {})",
            args.input.display(),
            bytes.len()
        ));
    }
    let total_packets = bytes.len() / TS_PACKET_SIZE;
    let (range_start, range_end) = args.packets.unwrap_or((0, total_packets));
    if range_end > total_packets {
        return Err(format!(
            "--packets end {range_end} exceeds total packet count {total_packets}"
        ));
    }

    let mut out_buf: Vec<u8> = Vec::new();
    let mut copied = 0usize;
    for (i, chunk) in bytes.chunks(TS_PACKET_SIZE).enumerate() {
        if i < range_start || i >= range_end {
            continue;
        }
        if let Some(filter_pid) = args.pid
            && extract_pid(chunk) != filter_pid
        {
            continue;
        }
        out_buf.extend_from_slice(chunk);
        copied += 1;
    }

    if let Some(parent) = args.out.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::write(&args.out, &out_buf).map_err(|e| format!("write {}: {e}", args.out.display()))?;

    eprintln!(
        "wrote {} ({} packets, {} bytes) from {} ({} packets total)",
        args.out.display(),
        copied,
        out_buf.len(),
        args.input.display(),
        total_packets
    );

    if args.emit_shim {
        emit_shim(args)?;
    }
    Ok(())
}

fn extract_pid(packet: &[u8]) -> u16 {
    // H.222.0 §2.4.3.2: sync (1) + flags+pid_hi (1, low 5 bits) + pid_lo (1)
    debug_assert_eq!(packet.len(), TS_PACKET_SIZE);
    (((packet[1] as u16) & 0x1F) << 8) | (packet[2] as u16)
}

fn emit_shim(args: &Args) -> Result<(), String> {
    let slug = args
        .out
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("--out has no file stem")?;
    if slug.is_empty()
        || !slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(format!(
            "slug {slug:?} must match [a-z0-9_]+ (lowercase letters, digits, underscore only)"
        ));
    }

    // Locate tests/ root: walk up from --out until we find the parent of
    // tests/fixtures/regression/. Equivalently, the grand-grand-parent of
    // <out>. Caller is expected to place --out under tests/fixtures/regression/.
    let regression_dir = args.out.parent().ok_or("--out has no parent dir")?;
    let fixtures_dir = regression_dir
        .parent()
        .ok_or("--out parent has no parent (expected tests/fixtures/regression/<x>.bin)")?;
    let tests_dir = fixtures_dir
        .parent()
        .ok_or("--out has no tests/ ancestor (expected tests/fixtures/regression/<x>.bin)")?;
    if tests_dir.file_name().and_then(|s| s.to_str()) != Some("tests") {
        return Err(format!(
            "expected --out under .../tests/fixtures/regression/, got {}",
            args.out.display()
        ));
    }

    let shim_path = tests_dir.join(format!("regression_{slug}.rs"));
    let rel_bin = format!("fixtures/regression/{slug}.bin");
    // Note: Demuxer::feed_aligned takes a single &[u8; 188] packet.
    // The shim uses Demuxer::feed(&[u8]) which handles sync internally and
    // accepts an arbitrary byte slice — simpler for fixture playback.
    let source = format!(
        r#"//! Auto-generated regression shim from corpus-to-fixture.
//!
//! To regenerate after a parser bugfix:
//!   1. Re-run corpus-to-fixture against the same input + same flags.
//!   2. `cargo test -p tst-core --test regression_{slug}`.
//!
//! Add domain-specific assertions below the smoke-test as the bug fix
//! lands — the smoke-test alone only verifies no panic + at least one
//! Demuxer event.

use tst_core::mpegts::demux::Demuxer;

const FIXTURE: &[u8] = include_bytes!("{rel_bin}");

#[test]
fn {slug}_smoke() {{
    assert_eq!(
        FIXTURE.len() % 188,
        0,
        "regression fixture must be 188-byte aligned"
    );
    let mut demux = Demuxer::new();
    let mut events = 0usize;
    // feed() handles TS sync recovery internally; simpler than looping
    // feed_aligned() for fixture playback.
    demux.feed(FIXTURE).ok();
    while let Some(_event) = demux.next_event() {{
        events += 1;
    }}
    // Smoke baseline: at least one event emitted, no panic.
    assert!(events > 0, "no demuxer events from {slug} fixture");
}}
"#
    );

    fs::write(&shim_path, source)
        .map_err(|e| format!("write shim {}: {e}", shim_path.display()))?;
    eprintln!("wrote shim {}", shim_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_args() {
        let argv = vec![
            "--input".into(),
            "/tmp/x.ts".into(),
            "--out".into(),
            "/tmp/o.bin".into(),
        ];
        let args = parse_args(&argv).unwrap();
        assert_eq!(args.input, PathBuf::from("/tmp/x.ts"));
        assert_eq!(args.out, PathBuf::from("/tmp/o.bin"));
        assert_eq!(args.pid, None);
        assert_eq!(args.packets, None);
        assert!(!args.emit_shim);
    }

    #[test]
    fn parse_all_args() {
        let argv = vec![
            "--input".into(),
            "/tmp/x.ts".into(),
            "--out".into(),
            "/tmp/o.bin".into(),
            "--pid".into(),
            "0x1011".into(),
            "--packets".into(),
            "100..200".into(),
            "--emit-shim".into(),
        ];
        let args = parse_args(&argv).unwrap();
        assert_eq!(args.pid, Some(0x1011));
        assert_eq!(args.packets, Some((100, 200)));
        assert!(args.emit_shim);
    }

    #[test]
    fn parse_pid_decimal() {
        let argv = vec![
            "--input".into(),
            "i".into(),
            "--out".into(),
            "o".into(),
            "--pid".into(),
            "256".into(),
        ];
        assert_eq!(parse_args(&argv).unwrap().pid, Some(256));
    }

    #[test]
    fn parse_range_empty_rejected() {
        assert!(parse_range("100..100").is_err());
        assert!(parse_range("200..100").is_err());
    }

    #[test]
    fn missing_required_input() {
        let argv = vec!["--out".into(), "/tmp/o.bin".into()];
        assert!(parse_args(&argv).is_err());
    }

    fn write_synthetic_ts(packets: usize, tag: &str) -> PathBuf {
        // Synthesize `packets` distinguishable TS packets: each starts with 0x47
        // sync byte; remaining 187 bytes encode the packet index in the first 4
        // bytes (big-endian) and 0xFF padding. Real PIDs aren't set — this is
        // for byte-level extraction tests only.
        let mut buf = Vec::with_capacity(packets * TS_PACKET_SIZE);
        for i in 0..packets {
            buf.push(0x47); // sync
            buf.extend_from_slice(&(i as u32).to_be_bytes());
            buf.extend(std::iter::repeat_n(0xFF, TS_PACKET_SIZE - 5));
        }
        let path = std::env::temp_dir().join(format!(
            "corpus_to_fixture_test_{}_{}.ts",
            std::process::id(),
            tag
        ));
        fs::write(&path, &buf).unwrap();
        path
    }

    #[test]
    fn extract_full_copy() {
        let input = write_synthetic_ts(10, "full");
        let output = std::env::temp_dir().join(format!("c2f_test_full_{}.bin", std::process::id()));
        let args = Args {
            input: input.clone(),
            out: output.clone(),
            pid: None,
            packets: None,
            emit_shim: false,
        };
        run(&args).unwrap();
        let in_bytes = fs::read(&input).unwrap();
        let out_bytes = fs::read(&output).unwrap();
        assert_eq!(in_bytes, out_bytes, "no filter, should be byte-identical");
        let _ = fs::remove_file(&input);
        let _ = fs::remove_file(&output);
    }

    fn write_pid_tagged_ts(packets: &[u16], tag: &str) -> PathBuf {
        // Synthesize packets with deliberately-set PIDs in the H.222.0 field.
        let mut buf = Vec::with_capacity(packets.len() * TS_PACKET_SIZE);
        for &pid in packets {
            buf.push(0x47); // sync
            // byte 1: transport_error=0, payload_unit_start_indicator=0,
            //   transport_priority=0, PID[12:8] (low 5 bits)
            buf.push(((pid >> 8) & 0x1F) as u8);
            // byte 2: PID[7:0]
            buf.push((pid & 0xFF) as u8);
            // byte 3: scrambling=0, adaptation=01 (payload only), cc=0
            buf.push(0x10);
            buf.extend(std::iter::repeat_n(0xFF, TS_PACKET_SIZE - 4));
        }
        let path =
            std::env::temp_dir().join(format!("c2f_pid_test_{}_{}.ts", std::process::id(), tag));
        fs::write(&path, &buf).unwrap();
        path
    }

    #[test]
    fn filter_by_pid_keeps_only_matches() {
        let input = write_pid_tagged_ts(&[0x0000, 0x1011, 0x1031, 0x1011, 0x0000], "filter");
        let output = std::env::temp_dir().join(format!("c2f_pid_{}.bin", std::process::id()));
        let args = Args {
            input: input.clone(),
            out: output.clone(),
            pid: Some(0x1011),
            packets: None,
            emit_shim: false,
        };
        run(&args).unwrap();
        let out = fs::read(&output).unwrap();
        assert_eq!(out.len(), 2 * TS_PACKET_SIZE, "two 0x1011 packets expected");
        // Both retained packets must show PID 0x1011 in bytes [1..=2].
        for chunk in out.chunks(TS_PACKET_SIZE) {
            assert_eq!(extract_pid(chunk), 0x1011);
        }
        let _ = fs::remove_file(&input);
        let _ = fs::remove_file(&output);
    }

    #[test]
    fn filter_by_packet_range_half_open() {
        let input = write_synthetic_ts(10, "range");
        let output = std::env::temp_dir().join(format!("c2f_range_{}.bin", std::process::id()));
        let args = Args {
            input: input.clone(),
            out: output.clone(),
            pid: None,
            packets: Some((3, 7)),
            emit_shim: false,
        };
        run(&args).unwrap();
        let out = fs::read(&output).unwrap();
        assert_eq!(out.len(), 4 * TS_PACKET_SIZE);
        // First retained packet's index field (bytes 1..=4) should be 3.
        assert_eq!(&out[1..5], &3u32.to_be_bytes());
        // Last retained packet's index field should be 6.
        let last_start = (4 - 1) * TS_PACKET_SIZE;
        assert_eq!(&out[last_start + 1..last_start + 5], &6u32.to_be_bytes());
        let _ = fs::remove_file(&input);
        let _ = fs::remove_file(&output);
    }

    #[test]
    fn filter_pid_and_range_composed() {
        let input = write_pid_tagged_ts(&[0x100, 0x200, 0x100, 0x200, 0x100, 0x200], "compose");
        // Range 1..5 covers indices 1,2,3,4 → PIDs 0x200, 0x100, 0x200, 0x100.
        // PID filter 0x100 retains indices 2 and 4. Result: 2 packets.
        let output = std::env::temp_dir().join(format!("c2f_compose_{}.bin", std::process::id()));
        let args = Args {
            input: input.clone(),
            out: output.clone(),
            pid: Some(0x100),
            packets: Some((1, 5)),
            emit_shim: false,
        };
        run(&args).unwrap();
        let out = fs::read(&output).unwrap();
        assert_eq!(out.len(), 2 * TS_PACKET_SIZE);
        let _ = fs::remove_file(&input);
        let _ = fs::remove_file(&output);
    }

    #[test]
    fn range_out_of_bounds_rejected() {
        let input = write_synthetic_ts(5, "oob");
        let output = std::env::temp_dir().join(format!("c2f_oob_{}.bin", std::process::id()));
        let args = Args {
            input: input.clone(),
            out: output.clone(),
            pid: None,
            packets: Some((0, 10)),
            emit_shim: false,
        };
        let err = run(&args).unwrap_err();
        assert!(
            err.contains("exceeds"),
            "expected out-of-bounds message, got: {err}"
        );
        let _ = fs::remove_file(&input);
    }

    #[test]
    fn non_188_aligned_input_rejected() {
        let path = std::env::temp_dir().join(format!("c2f_unaligned_{}.ts", std::process::id()));
        fs::write(&path, b"not aligned to 188 bytes").unwrap();
        let args = Args {
            input: path.clone(),
            out: std::env::temp_dir().join("never.bin"),
            pid: None,
            packets: None,
            emit_shim: false,
        };
        let err = run(&args).unwrap_err();
        assert!(err.contains("not a multiple of 188"), "got: {err}");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn emit_shim_creates_compilable_test_file() {
        let input = write_pid_tagged_ts(&[0x1011, 0x1011], "shim");
        // The shim path is derived: tests/fixtures/regression/<slug>.bin →
        // tests/regression_<slug>.rs (both relative to crate root).
        let temp_root = std::env::temp_dir().join(format!("c2f_shim_root_{}", std::process::id()));
        fs::create_dir_all(temp_root.join("tests/fixtures/regression")).unwrap();
        let out_bin = temp_root.join("tests/fixtures/regression/bug_demo.bin");
        let args = Args {
            input: input.clone(),
            out: out_bin.clone(),
            pid: None,
            packets: None,
            emit_shim: true,
        };
        run(&args).unwrap();
        let shim_path = temp_root.join("tests/regression_bug_demo.rs");
        assert!(
            shim_path.exists(),
            "shim file should be created at {shim_path:?}"
        );
        let shim_source = fs::read_to_string(&shim_path).unwrap();
        assert!(shim_source.contains("include_bytes!"));
        assert!(shim_source.contains("bug_demo.bin"));
        assert!(shim_source.contains("#[test]"));
        assert!(shim_source.contains("Demuxer"));

        // Clean up.
        let _ = fs::remove_file(&input);
        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn emit_shim_rejects_non_slug_filename() {
        // The slug must be [a-z0-9_]+ to be a valid Rust identifier component
        // and a valid Cargo integration-test target.
        let input = write_synthetic_ts(2, "badslug");
        let temp_root =
            std::env::temp_dir().join(format!("c2f_badslug_root_{}", std::process::id()));
        fs::create_dir_all(temp_root.join("tests/fixtures/regression")).unwrap();
        let out_bin = temp_root.join("tests/fixtures/regression/Bug-Demo.bin"); // bad: uppercase + hyphen
        let args = Args {
            input: input.clone(),
            out: out_bin,
            pid: None,
            packets: None,
            emit_shim: true,
        };
        let err = run(&args).unwrap_err();
        assert!(err.contains("slug"), "got: {err}");
        let _ = fs::remove_file(&input);
        let _ = fs::remove_dir_all(&temp_root);
    }
}
