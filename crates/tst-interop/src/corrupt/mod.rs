//! Seeded sender-side TS corruption tap (spec §4.1) and the receiver-side
//! attribution engine that judges what the corruption did (spec §4.3).
//!
//! The two halves are deliberately split by a FILE, not by a shared
//! in-memory object: [`Corrupter`] wraps the send-side [`Transport`](tst_core::transport::Transport) and
//! writes one JSONL line per injection; [`Attribution`] reads that log
//! back and matches it against the error events a receiver actually
//! surfaced. Nothing is carried across in process memory, so the same
//! judgement can be made live (`recv`), offline (`verify` over a capture),
//! or days later from an archived soak directory — and a receiver can
//! never "know" about an injection except through the same evidence a
//! human would read.
//!
//! # Why a packet-coordinate instead of a byte offset
//!
//! Sender byte offsets are useless at the receiver: the transport loses,
//! duplicates and reorders, and the corruption itself changes the wire
//! length (truncation, inserted garbage). Every injection is therefore
//! logged at a [`Coord`] — the most recent PCR base seen *before* the
//! packet, plus the number of packets since that PCR packet. PCR bases are
//! carried in the stream itself, so both ends can name the same instant
//! without a shared clock, and a receiver resolves a coordinate the moment
//! it sees that base (or, if the injection destroyed the packet carrying
//! it, the first base after it — see [`Attribution::on_pcr`]). An
//! injection whose log line is read only AFTER its anchor base was seen —
//! a zero-latency link can deliver the PCR before the sender logs the
//! injection behind it — is resolved by replaying the recent PCR history
//! (see [`Attribution::append`]).
//!
//! Determinism is the other invariant: given the same seed, the same
//! config and the same input bytes, the tap emits byte-identical wire
//! output and a byte-identical log. Nothing here reads the clock.

mod attribution;
mod coord;
mod tap;
#[doc(hidden)]
pub mod testing;

pub use attribution::*;
pub use coord::*;
pub use tap::*;

#[cfg(test)]
use std::io::Write;
#[cfg(test)]
use std::sync::{Arc, Mutex};
#[cfg(test)]
use tst_core::transport::{Transport, TransportError};

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory transport that records every `send_bytes` payload
    /// separately — the per-push view the splitting test needs.
    struct Capture(Arc<Mutex<Vec<Vec<u8>>>>);
    impl Transport for Capture {
        fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
            self.0.lock().unwrap().push(msg.to_vec());
            Ok(())
        }
        fn max_payload(&self) -> usize {
            1316
        }
        fn is_alive(&self) -> bool {
            true
        }
        fn close(&mut self) {}
    }

    fn baseline_bytes(seconds: f64, tag: &str) -> Vec<u8> {
        let p = crate::profiles::by_name("baseline").unwrap();
        let path = std::env::temp_dir().join(format!(
            "tst-interop-corrupt-{tag}-{}.ts",
            std::process::id()
        ));
        crate::r#gen::run(
            p,
            seconds,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .unwrap();
        let b = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        b
    }

    /// Push `bytes` through a `Corrupter` in 1316-byte pushes; return (wire bytes, log text, stats).
    fn run_tap(bytes: &[u8], cfg: CorruptConfig) -> (Vec<u8>, String, CorruptionStats) {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut tap = Corrupter::new(
            Capture(Arc::clone(&sink)),
            cfg,
            Box::new(testing::VecWriter(Arc::clone(&log))),
        )
        .unwrap();
        for chunk in bytes.chunks(1316) {
            tap.send_bytes(chunk).unwrap();
        }
        let stats = tap.stats();
        drop(tap);
        let wire: Vec<u8> = sink.lock().unwrap().iter().flatten().copied().collect();
        let text = String::from_utf8(log.lock().unwrap().clone()).unwrap();
        (wire, text, stats)
    }

    fn cfg(classes: &[Class], rate: u32, min_gap: u64) -> CorruptConfig {
        CorruptConfig {
            rate_per_10k: rate,
            min_gap,
            classes: classes.to_vec(),
            seed: 7,
            geometry_scale: 1,
        }
    }

    /// A restarted receiver re-attaches to a log the sender has been
    /// writing for hours. The tail must stream it: memory bounded by the
    /// longest line, not the archive (the previous whole-file carry with a
    /// front drain per line was quadratic and held the archive in RAM).
    #[test]
    fn log_tail_streams_an_archive_with_bounded_carry() {
        let b = baseline_bytes(60.0, "tail-capacity");
        let (_, text, _) = run_tap(&b, cfg(&Class::ALL, 10_000, 1000));
        let mut lines = text.lines();
        let header = lines.next().unwrap();
        let first = lines.next().expect("fixture must inject");
        let mut archive = format!("{header}\n");
        for _ in 0..4096 {
            archive.push_str(first);
            archive.push('\n');
        }
        let path = std::env::temp_dir().join(format!(
            "tst-interop-tail-capacity-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, &archive).unwrap();
        let mut tail = LogTail::open(&path).unwrap();
        let history = tail.poll().unwrap();
        let capacity = tail.carry.capacity();
        assert!(tail.carry.is_empty());
        drop(tail);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(history.len(), 4096);
        assert!(
            capacity < first.len() * 2 + 2,
            "carry must hold at most one line, got capacity {capacity} for a {}-byte archive",
            archive.len()
        );
    }

    /// The sender appends; a poll can land mid-line. The partial line is
    /// held, never parsed, and comes back exactly once when terminated.
    #[test]
    fn partial_last_line_is_held_until_terminated() {
        use std::io::Write;
        let b = baseline_bytes(60.0, "tail-partial");
        let (_, text, _) = run_tap(&b, cfg(&Class::ALL, 10_000, 1000));
        let mut lines = text.lines();
        let header = lines.next().unwrap();
        let first = lines.next().expect("fixture must inject");
        let path = std::env::temp_dir().join(format!(
            "tst-interop-tail-partial-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (head, rest) = first.split_at(first.len() / 2);
        std::fs::write(&path, format!("{header}\n{first}\n{head}")).unwrap();
        let mut tail = LogTail::open(&path).unwrap();
        assert_eq!(tail.poll().unwrap().len(), 1, "one complete line");
        assert_eq!(tail.poll().unwrap().len(), 0, "the partial line is held");
        assert!(!tail.carry.is_empty(), "held bytes live in carry");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{rest}").unwrap();
        drop(f);
        let got = tail.poll().unwrap();
        drop(tail);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(got.len(), 1, "the completed line arrives exactly once");
    }

    /// A log writer that refuses exactly one whole line: the `nth`
    /// JSON write (1 = the header, 2 = the first injection). `write_line`
    /// writes the JSON then the newline, so failing the JSON write drops
    /// the whole line cleanly — no torn line, no trace in the file.
    struct FailOneLine {
        inner: testing::VecWriter,
        nth: usize,
        lines: usize,
    }
    impl Write for FailOneLine {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if b != b"\n" {
                self.lines += 1;
                if self.lines == self.nth {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "disk full",
                    ));
                }
            }
            self.inner.write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    /// A log writer whose WRITES always succeed and whose `flush` fails
    /// only after `arm`ing — the close-time shape. `write_line` flushes
    /// per line, so leaving it disarmed until the run is over is what
    /// isolates `Corrupter::close`'s own flush.
    struct FailFlushOnClose {
        inner: testing::VecWriter,
        armed: Arc<Mutex<bool>>,
    }
    impl Write for FailFlushOnClose {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.inner.write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if *self.armed.lock().unwrap() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "disk full",
                ));
            }
            self.inner.flush()
        }
    }

    /// Close-time half: a buffering writer can hold a whole
    /// line until the final flush, so `Corrupter::close` discarding that
    /// flush error loses evidence exactly like the per-line write arm
    /// would. It must latch the same flag — and `report soak` must fail
    /// `corruption_coverage` on it even though every line the receiver
    /// DID read parses and resolves.
    #[test]
    fn a_close_time_flush_failure_is_latched_and_fails_coverage() {
        let b = baseline_bytes(6.0, "close-flush");
        let sink = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::new(Mutex::new(Vec::<u8>::new()));
        let armed = Arc::new(Mutex::new(false));
        let mut tap = Corrupter::new(
            Capture(Arc::clone(&sink)),
            cfg(&[Class::Dup], 10_000, 1000),
            Box::new(FailFlushOnClose {
                inner: testing::VecWriter(Arc::clone(&log)),
                armed: Arc::clone(&armed),
            }),
        )
        .unwrap();
        for chunk in b.chunks(1316) {
            tap.send_bytes(chunk).unwrap();
        }
        // Every per-line flush succeeded, so nothing is latched yet.
        assert!(
            !tap.stats().log_write_failed,
            "the run itself wrote cleanly: {:?}",
            tap.stats()
        );
        let injections = tap.stats().injections;
        assert!(injections >= 1, "6 s of baseline injects at least once");

        *armed.lock().unwrap() = true;
        tap.close();

        let stats = tap.stats();
        assert!(
            stats.log_write_failed,
            "the tap must record that the final flush failed: {stats:?}"
        );
        // …and the log on disk still looks complete, which is exactly why
        // the latch has to carry the failure rather than the receiver
        // tripping over a gap.
        let text = String::from_utf8(log.lock().unwrap().clone()).unwrap();
        let (_, logged) = parse_log(&text).expect("the written lines still parse");
        assert_eq!(logged.len() as u64, injections);
    }

    /// A lost injection line is not necessarily an
    /// unexplained event later — a `Dup` obliges the receiver to notice
    /// nothing — so the tap must latch the failure into its own stats
    /// rather than rely on the receiver to trip over the gap.
    #[test]
    fn corruption_log_write_failure_is_latched_in_the_stats() {
        let b = baseline_bytes(6.0, "log-fail");
        let sink = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut tap = Corrupter::new(
            Capture(Arc::clone(&sink)),
            cfg(&[Class::Dup], 10_000, 1000),
            Box::new(FailOneLine {
                inner: testing::VecWriter(Arc::clone(&log)),
                nth: 2,
                lines: 0,
            }),
        )
        .unwrap();
        for chunk in b.chunks(1316) {
            tap.send_bytes(chunk).unwrap();
        }
        let stats = tap.stats();
        // `garbage_inserts_1_to_300_non_sync_bytes…` proves 6 s of compact
        // baseline yields at least one injection at this rate/min_gap;
        // the failing line is the FIRST injection's (JSON write #2), so
        // one injection is all the latch needs.
        assert!(
            stats.injections >= 1,
            "6 s of baseline at min_gap 1000 injects at least once"
        );
        let text = String::from_utf8(log.lock().unwrap().clone()).unwrap();
        let (_, logged) = parse_log(&text).expect("the surviving lines still parse");
        assert_eq!(
            logged.len() as u64,
            stats.injections - 1,
            "exactly one line is missing"
        );
        assert!(
            stats.log_write_failed,
            "the tap must record that a line was lost: {stats:?}"
        );
    }

    /// On a PAT/PMT packet the `Header` class must use ONLY the sync-byte
    /// sub-kind, because the other three are invisible to a receiver
    /// there (see the `Class::Header` arm's own comment) and an
    /// invisible-but-`detectable` injection makes the report fail a
    /// conformant library.
    ///
    /// Driven over a stream of nothing but PAT packets rather than a
    /// normal capture: PSI is ~1% of a real multiplex, so with `min_gap`
    /// at its 1000-packet floor a normal stream would have to run for
    /// minutes before an injection happened to land on one, and the test
    /// would pass vacuously long before that.
    #[test]
    fn header_class_on_a_psi_packet_only_ever_kills_the_sync_byte() {
        let b = baseline_bytes(2.0, "psihdr");
        let pat: [u8; PKT] = b[..PKT].try_into().unwrap();
        assert_eq!(
            crate::rawts::classify_packet(&pat).unwrap().pid,
            0,
            "the generator emits the PAT first"
        );
        let mut stream = Vec::with_capacity(PKT * 5000);
        for _ in 0..5000 {
            stream.extend_from_slice(&pat);
        }

        let (_, text, _) = run_tap(&stream, cfg(&[Class::Header], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        assert!(inj.len() >= 4, "{} injection(s)", inj.len());
        for i in &inj {
            assert_eq!(i.pid, 0);
            assert_eq!(
                i.offsets,
                vec![0],
                "sub-kind {:?} on a PSI packet",
                i.offsets
            );
            assert_ne!(i.after[0], 0x47, "the sync byte must actually be gone");
        }
    }

    /// The flip side: on MEDIA PIDs all four sub-kinds stay in play, so
    /// the PSI rule above cannot have quietly collapsed the class into
    /// "always kill the sync byte" everywhere.
    #[test]
    fn header_class_on_media_packets_still_uses_every_sub_kind() {
        let b = baseline_bytes(600.0, "mediahdr");
        let (_, text, _) = run_tap(&b, cfg(&[Class::Header], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        let media: Vec<&Injection> = inj
            .iter()
            .filter(|i| !matches!(i.pid, 0 | 0x1000))
            .collect();
        assert!(media.len() >= 8, "{} media injection(s)", media.len());
        let shapes: std::collections::BTreeSet<Vec<usize>> =
            media.iter().map(|i| i.offsets.clone()).collect();
        assert!(
            shapes.len() >= 2,
            "every media injection took the same sub-kind: {shapes:?}"
        );
    }

    #[test]
    fn parse_corrupt_accepts_the_documented_grammar_and_rejects_bad_gaps() {
        let c = parse_corrupt("rate=5,min_gap=1000", 3, 1).unwrap();
        assert_eq!(
            c,
            CorruptConfig {
                rate_per_10k: 5,
                min_gap: 1000,
                classes: Class::ALL.to_vec(),
                seed: 3,
                geometry_scale: 1,
            }
        );
        let c = parse_corrupt("rate=50,min_gap=1200,classes=header+psi_flip", 3, 1).unwrap();
        assert_eq!(c.classes, vec![Class::Header, Class::PsiFlip]);
        assert!(
            parse_corrupt("rate=5,min_gap=999", 3, 1).is_err(),
            "min_gap < 2*window"
        );
        assert!(parse_corrupt("rate=5,min_gap=1000,classes=bogus", 3, 1).is_err());
        assert!(
            parse_corrupt("rate=0,min_gap=1000", 3, 1).is_err(),
            "rate 0 is 'disabled', reject"
        );
        assert!(
            parse_corrupt("min_gap=1000", 3, 1).is_err(),
            "rate required"
        );
        assert!(
            parse_corrupt("rate=5,min_gap=1000,bogus=1", 3, 1).is_err(),
            "unknown keys fail closed"
        );
    }

    /// The packet geometry follows `--au-scale`: the phenomena it bounds
    /// (next PES boundary, next PCR, next packet on a PID) are fixed in
    /// time, so at scale F they are F× further apart in packets. An
    /// omitted `min_gap` scales with it; an explicit one is held to the
    /// scaled floor, so the soak's literal `min_gap=1000` cannot be
    /// carried unchanged into a scaled run and silently make its
    /// attribution ambiguous.
    #[test]
    fn parse_corrupt_scales_the_packet_geometry_with_the_au_scale() {
        let c = parse_corrupt("rate=5", 3, 4).unwrap();
        assert_eq!(c.geometry_scale, 4);
        assert_eq!(c.min_gap, 4000, "default min_gap scales");
        assert_eq!(c.attribution_window(), 2000);
        assert_eq!(c.recovery_bound(), 2400);
        assert_eq!(c.approx_slack(), 512);
        assert_eq!(parse_corrupt("rate=5", 3, 1).unwrap().min_gap, 1000);
        let e = parse_corrupt("rate=5,min_gap=1000", 3, 4).unwrap_err();
        assert!(
            e.contains("4000") && e.contains("2400") && e.contains("--au-scale 4"),
            "the floor must name the scaled numbers: {e}"
        );
        assert!(parse_corrupt("rate=5,min_gap=4000", 3, 4).is_ok());
        assert!(
            parse_corrupt("rate=5", 3, 0).is_err(),
            "scale 0 is not a geometry"
        );
    }

    /// What the tap writes is what the receiver judges with: the header
    /// carries the SCALED geometry, not the scale-1 constants.
    #[test]
    fn the_log_header_carries_the_scaled_geometry() {
        let b = baseline_bytes(10.0, "geometry");
        let mut c = cfg(&Class::ALL, 10_000, 4000);
        c.geometry_scale = 4;
        let (_, text, _) = run_tap(&b, c);
        let first = text.lines().next().unwrap();
        let LogLine::Header(h) = serde_json::from_str::<LogLine>(first).unwrap() else {
            panic!("first line is not a header: {first}");
        };
        assert_eq!(
            (
                h.min_gap,
                h.attribution_window,
                h.recovery_bound,
                h.approx_slack
            ),
            (4000, 2000, 2400, 512)
        );
        // Scale 1 is byte-for-byte the geometry every archived log used.
        let (_, text, _) = run_tap(&b, cfg(&Class::ALL, 10_000, 1000));
        let LogLine::Header(h) =
            serde_json::from_str::<LogLine>(text.lines().next().unwrap()).unwrap()
        else {
            panic!("no header");
        };
        assert_eq!(
            (
                h.min_gap,
                h.attribution_window,
                h.recovery_bound,
                h.approx_slack
            ),
            (1000, 500, 600, 128)
        );
    }

    /// Archived logs (every soak before the geometry scaled) have no
    /// `approx_slack`; they were all scale 1, so they must read as 128.
    #[test]
    fn an_archived_header_without_approx_slack_reads_as_scale_one() {
        let json = r#"{"header":{"tap_version":1,"seed":12,"rate_per_10k":5,"min_gap":1000,"classes":["body_flip"],"attribution_window":500,"recovery_bound":600}}"#;
        let LogLine::Header(h) = serde_json::from_str::<LogLine>(json).unwrap() else {
            panic!("not a header");
        };
        assert_eq!(h.approx_slack, APPROX_SLACK);
    }

    #[test]
    fn same_seed_same_input_gives_byte_identical_wire_and_log() {
        let b = baseline_bytes(6.0, "det");
        let (w1, l1, _) = run_tap(&b, cfg(&Class::ALL, 200, 1000));
        let (w2, l2, _) = run_tap(&b, cfg(&Class::ALL, 200, 1000));
        assert_eq!(w1, w2);
        assert_eq!(l1, l2);
        let (w3, _, _) = run_tap(
            &b,
            CorruptConfig {
                seed: 8,
                ..cfg(&Class::ALL, 200, 1000)
            },
        );
        assert_ne!(w1, w3);
    }

    #[test]
    fn log_header_then_one_line_per_injection_and_min_gap_is_honored() {
        // 60 s of baseline is 3600 packets (the profile muxes 60 packets
        // per second — see `rawts`'s own 180-packets-in-3 s assertion), so
        // a 5% rate with a 1000-packet floor lands ~3 injections.
        let b = baseline_bytes(60.0, "gap");
        let (_, text, stats) = run_tap(&b, cfg(&Class::ALL, 500, 1000));
        let (hdr, inj) = parse_log(&text).unwrap();
        assert_eq!(hdr.min_gap, 1000);
        assert_eq!(inj.len() as u64, stats.injections);
        assert!(
            stats.injections >= 2,
            "3600 packets at 5% -> expect >= 2 with gap 1000: {stats:?}"
        );
        for w in inj.windows(2) {
            assert!(w[1].ordinal - w[0].ordinal >= 1000, "{:?}", w);
        }
    }

    /// Each class, forced (rate 10_000 = every eligible packet, min_gap
    /// 1000 so exactly one injection lands in the first ~1000 packets),
    /// produces the documented wire mutation.
    #[test]
    fn body_flip_changes_only_bytes_past_the_header() {
        let b = baseline_bytes(6.0, "bf");
        let (w, text, _) = run_tap(&b, cfg(&[Class::BodyFlip], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        let i = &inj[0];
        assert_eq!(i.class, Class::BodyFlip);
        assert!(i.offsets.iter().all(|&o| (4..188).contains(&o)));
        let orig = &b[i.ordinal as usize * 188..][..188];
        let got = &w[i.ordinal as usize * 188..][..188];
        assert_eq!(&orig[..4], &got[..4]);
        assert_ne!(orig, got);
        assert_eq!(w.len(), b.len());
    }

    /// A body flip must never land inside a PES header.
    ///
    /// The bytes there are undetectable by contract (tst-core's PES
    /// parser accepts `stream_id`, `PES_packet_length`,
    /// `header_data_length` and 33 of the 40 PTS bits as-is) but they are
    /// read STRUCTURALLY by the wire oracles, so a flip that silently
    /// rewrites a PTS or a stream_id fails the capture for damage the tap
    /// itself declared nobody has to notice. Measured before this rule:
    /// `pts_wrap_unexpected` on 5 seeds and `av1_carriage_wire` on 3,
    /// across a 6-profile × 7-class × 40-seed sweep.
    ///
    /// Swept over 64 seeds rather than one because the offset is a PRNG
    /// draw; the assertion that PES-start injections actually occurred is
    /// what stops the sweep passing vacuously.
    #[test]
    fn a_body_flip_never_lands_in_a_pes_header() {
        let b = baseline_bytes(300.0, "bfpes");
        let mut pes_starts = 0;
        for seed in 1..=64u64 {
            let (_, text, _) = run_tap(
                &b,
                CorruptConfig {
                    seed,
                    ..cfg(&[Class::BodyFlip], 10_000, 1000)
                },
            );
            let (_, inj) = parse_log(&text).unwrap();
            for i in inj.iter().filter(|i| i.pes_start) {
                pes_starts += 1;
                let pkt: [u8; PKT] = i.before[..].try_into().expect("a whole packet is logged");
                let info = crate::rawts::classify_packet(&pkt).unwrap();
                // §2.4.3.7: the ES payload begins after the 9 fixed PES
                // header bytes plus `header_data_length` optional ones.
                let es = info.payload_off + 9 + usize::from(pkt[info.payload_off + 8]);
                for &o in &i.offsets {
                    assert!(
                        o >= es,
                        "seed {seed}: offset {o} is inside the PES header \
                         (payload_off {}, ES payload starts at {es})",
                        info.payload_off
                    );
                }
            }
        }
        assert!(
            pes_starts >= 8,
            "only {pes_starts} PES-start injection(s) across 64 seeds — the rule above \
             is barely exercised"
        );
    }

    /// A body flip is only claimed detectable when it lands under a
    /// section CRC. This pins the one span that decides that.
    #[test]
    fn sensitive_span_covers_the_psi_section_and_nothing_else() {
        let b = baseline_bytes(2.0, "span");
        let pkt_at = |n: usize| -> [u8; 188] { b[n * 188..][..188].try_into().unwrap() };

        // The PAT: section body through the end of the CRC. Everything
        // after it is 0xFF stuffing, where a flip is invisible — and
        // everything BEFORE it (pointer field, table_id, section_length)
        // is read to find the section, so a flip there makes the receiver
        // discard it before the CRC is ever checked. See
        // `sensitive_span`'s own doc comment for the measured evidence.
        let pat = pkt_at(0);
        let info = crate::rawts::classify_packet(&pat).unwrap();
        assert_eq!(info.pid, 0, "first packet of a fresh mux is the PAT");
        let (lo, hi) = sensitive_span(&pat, &info, true).unwrap();
        // pointer field at payload_off (0 here), then the 3-byte section
        // header, then the body.
        assert_eq!(
            lo,
            info.payload_off + 1 + usize::from(pat[info.payload_off]) + 3
        );
        assert!(hi < 188, "a PAT section is far shorter than its payload");
        assert!(
            pat[hi..].iter().all(|&x| x == 0xFF),
            "stuffing past the CRC"
        );
        // The three header bytes and the pointer field sit OUTSIDE the
        // span: a flip there is not required to be noticed.
        assert!(
            (info.payload_off..lo).len() == 4,
            "pointer field + 3-byte section header are excluded"
        );

        // No video packet has a required-to-notice span, PES start or
        // not: tst-core's PES parser tolerates most of the header, and
        // nothing at all checks the access-unit bytes after it.
        let (mut starts, mut video) = (0, 0);
        for n in 0..b.len() / 188 {
            let pkt = pkt_at(n);
            let info = crate::rawts::classify_packet(&pkt).unwrap();
            if info.pid != 0x1011 {
                continue;
            }
            video += 1;
            starts += usize::from(pes_start(&pkt, &info));
            assert_eq!(sensitive_span(&pkt, &info, false), None, "packet {n}");
        }
        assert!(
            starts > 0,
            "{starts} PES starts among {video} video packets"
        );

        // A PSI packet still needs PUSI: a section continuation carries no
        // section header to locate.
        let mut pat = pkt_at(0);
        pat[1] &= !0x40;
        let info = crate::rawts::classify_packet(&pat).unwrap();
        assert_eq!(sensitive_span(&pat, &info, true), None);
    }

    #[test]
    fn header_class_is_always_detectable_and_keeps_packet_length() {
        let b = baseline_bytes(6.0, "hdr");
        let (w, text, _) = run_tap(&b, cfg(&[Class::Header], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        assert!(inj[0].detectable);
        assert!(inj[0].offsets.iter().all(|&o| o <= 4));
        assert_eq!(w.len(), b.len());
    }

    #[test]
    fn truncate_shortens_the_wire_by_the_cut() {
        let b = baseline_bytes(6.0, "tr");
        let (w, text, _) = run_tap(&b, cfg(&[Class::Truncate], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        let cut = 188 - inj[0].after.len();
        assert!((1..=148).contains(&cut), "emit 40..=187 bytes");
        assert_eq!(
            w.len(),
            b.len() - inj.iter().map(|i| 188 - i.after.len()).sum::<usize>()
        );
    }

    #[test]
    fn garbage_inserts_1_to_300_non_sync_bytes_and_splits_oversize_pushes() {
        let b = baseline_bytes(6.0, "gb");
        let sink = Arc::new(Mutex::new(Vec::new()));
        let mut tap = Corrupter::new(
            Capture(Arc::clone(&sink)),
            cfg(&[Class::Garbage], 10_000, 1000),
            Box::new(std::io::sink()),
        )
        .unwrap();
        for chunk in b.chunks(1316) {
            tap.send_bytes(chunk).unwrap();
        }
        let stats = tap.stats();
        let pushes = sink.lock().unwrap().clone();
        assert!(
            pushes.iter().all(|p| p.len() <= 1316),
            "every push <= max_payload"
        );
        assert!(stats.bytes_out > stats.bytes_in);
        assert!(stats.bytes_out - stats.bytes_in <= 300 * stats.injections);
        let wire: Vec<u8> = pushes.iter().flatten().copied().collect();
        // No inserted byte is 0x47 (garbage never fakes a sync).
        let mut r = crate::rawts::Reader::new();
        r.set_resync_mode(true);
        r.feed(&wire).unwrap();
        assert_eq!(r.resync_count(), stats.injections);
    }

    #[test]
    fn drop_removes_and_dup_repeats_a_packet() {
        let b = baseline_bytes(6.0, "dd");
        let (w, text, _) = run_tap(&b, cfg(&[Class::Drop], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        assert_eq!(w.len(), b.len() - 188 * inj.len());
        // The forced first injection lands on packet 0, which is the PAT —
        // where a drop is invisible. Detectability is
        // `drop_is_detectable_only_on_a_payload_carrying_media_packet`'s
        // subject; this test is about the wire arithmetic.
        assert_eq!(inj[0].pid, 0);
        assert!(!inj[0].detectable);
        let (w, text, _) = run_tap(&b, cfg(&[Class::Dup], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        assert_eq!(w.len(), b.len() + 188 * inj.len());
        assert!(!inj[0].detectable);
        let o = inj[0].ordinal as usize;
        assert_eq!(&w[o * 188..(o + 1) * 188], &w[(o + 1) * 188..(o + 2) * 188]);
    }

    /// A dropped packet is only ever noticed as a continuity jump, so it
    /// is only `detectable` where a continuity jump can exist: on a media
    /// PID, on a packet that carries payload. Claiming otherwise fails a
    /// conformant receiver for staying silent about a lost PAT repetition
    /// or a lost PCR-only catch-up packet, neither of which a receiver is
    /// required — or in tst-core's case even able — to report.
    ///
    /// Driven over the `audio` profile because its third stream is what
    /// makes the muxer emit adaptation-field-only catch-up packets on a
    /// media PID (`mux/scheduling.rs`'s `pcr_only_due`); the assertion
    /// below that all three shapes occurred is what keeps this from
    /// passing vacuously.
    #[test]
    fn drop_is_detectable_only_on_a_payload_carrying_media_packet() {
        let p = crate::profiles::by_name("audio").unwrap();
        let path = std::env::temp_dir().join(format!(
            "tst-interop-corrupt-dropdet-{}.ts",
            std::process::id()
        ));
        crate::r#gen::run(
            p,
            300.0,
            &path,
            crate::fixtures::KlvSet::Compact,
            0,
            crate::fixtures::AuSizeMode::Compact,
        )
        .unwrap();
        let b = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let (_, text, _) = run_tap(&b, cfg(&[Class::Drop], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        let (mut psi, mut af_only, mut with_payload) = (0, 0, 0);
        for i in &inj {
            let pkt: [u8; PKT] = i.before[..].try_into().expect("a whole packet is logged");
            let info = crate::rawts::classify_packet(&pkt).unwrap();
            let is_psi = matches!(i.pid, 0 | 0x1000);
            assert_eq!(
                i.detectable,
                !is_psi && info.has_payload,
                "pid {:#06x} has_payload {} -> detectable {}",
                i.pid,
                info.has_payload,
                i.detectable
            );
            match (is_psi, info.has_payload) {
                (true, _) => psi += 1,
                (false, false) => af_only += 1,
                (false, true) => with_payload += 1,
            }
        }
        assert!(
            psi > 0 && af_only > 0 && with_payload > 0,
            "all three shapes must occur or the assertion above is vacuous: \
             psi {psi}, af-only {af_only}, with-payload {with_payload}"
        );
    }

    /// A PAT/PMT PID carries no media, so "media on the injected PID"
    /// can never arrive there — `Sample`/`Metadata` events only ever name
    /// an elementary stream. Recovery from a PSI injection is therefore
    /// proved by media on ANY PID, or every psi_flip in a soak run would
    /// be reported unrecovered against a perfectly healthy stream.
    #[test]
    fn a_psi_injection_recovers_on_media_from_any_pid() {
        let mut psi_inj = inj(1000, 0, Class::PsiFlip, 0, true);
        psi_inj.psi = true;
        let mut a = Attribution::strict(vec![psi_inj], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5010, Some(0), Signal::PsiChecksum);
        a.on_media(5020, 0x1011); // an elementary stream, not pid 0
        let r = a.finish(10_000);
        assert!(r.unrecovered.is_empty(), "{:?}", r.unrecovered);

        // A MEDIA-PID injection still has to be answered on its own PID:
        // the relaxation is scoped to PSI, not a blanket "any event
        // anywhere counts".
        let mut a = Attribution::strict(vec![inj(1000, 0, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5010, Some(0x1011), Signal::ContinuityJump);
        a.on_media(5020, 0x1100);
        let r = a.finish(10_000);
        assert_eq!(r.unrecovered.len(), 1, "media on a different PID");
    }

    /// `Class::Header`'s PID-rewrite sub-kind moves the packet to
    /// `REWRITTEN_PID`, so whatever the receiver reports about it, it
    /// reports THERE. Run 3 (2026-09-17): ~50 events on 0x1FFE per leg
    /// that nothing could explain because `reaches` only knew the
    /// original PID.
    #[test]
    fn a_pid_rewrite_explains_a_signal_on_the_pid_it_moved_the_packet_to() {
        let mut moved = inj(1000, 10, Class::Header, 0x1011, true);
        moved.offsets = vec![1, 2];
        let mut a = Attribution::strict(vec![moved], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5015, Some(REWRITTEN_PID), Signal::OtherNonConformant);
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 1, "{r:?}");
        assert!(
            r.unexplained_events.is_empty(),
            "{:?}",
            r.unexplained_events
        );

        // Every other header shape stays confined to its own PID: a CC
        // rewrite ([3]) and — the PSI shape, where the tap only ever
        // destroys the sync byte ([0]) — must not reach 0x1FFE.
        for (offsets, psi) in [(vec![3usize], false), (vec![0usize], true)] {
            let mut other = inj(1000, 10, Class::Header, if psi { 0 } else { 0x1011 }, true);
            other.offsets = offsets.clone();
            other.psi = psi;
            let mut a = Attribution::strict(vec![other], &hdr());
            a.on_pcr(1000, 5000);
            a.on_signal(5015, Some(REWRITTEN_PID), Signal::OtherNonConformant);
            let r = a.finish(10_000);
            // [0] is framing-wide and reaches everything by design; [3] is not.
            let want_unexplained = u64::from(offsets != vec![0]);
            assert_eq!(
                r.unexplained_nonconformant, want_unexplained,
                "{offsets:?}: {r:?}"
            );
        }
    }

    /// A derived anomaly (a PTS that stepped backwards) is asked about,
    /// not fed in — and only a TRUNCATION answers for it. Asking must not
    /// move any of the counters an event would.
    #[test]
    fn only_a_truncation_explains_a_derived_anomaly() {
        let mut a = Attribution::strict(vec![inj(1000, 10, Class::Truncate, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000); // resolves to receiver ordinal 5010
        // The window opens at `r - since_pcr` (see `backward_reach`), not
        // at `r`: with `since_pcr = 10` the injected packet's true
        // receiver ordinal is somewhere in 5000..=5010, depending on how
        // much of that span the transport lost.
        assert!(!a.truncation_explains(4999), "before the window opens");
        assert!(a.truncation_explains(5000), "earliest the injection can be");
        assert!(a.truncation_explains(5010), "at the injection");
        assert!(
            a.truncation_explains(5010 + ATTRIBUTION_WINDOW),
            "last packet in the window"
        );
        assert!(
            !a.truncation_explains(5011 + ATTRIBUTION_WINDOW),
            "past the window"
        );
        let r = a.finish(10_000);
        assert_eq!(
            (r.events, r.attributed_events, r.attributed_discontinuities),
            (0, 0, 0),
            "asking is not an event"
        );

        // Every other class leaves the anomaly unexplained: truncation is
        // the only one that misaligns the byte stream, so it is the only
        // one whose window may contain a PTS that was never a timestamp.
        for class in Class::ALL.iter().filter(|&&c| c != Class::Truncate) {
            let mut a = Attribution::strict(vec![inj(1000, 10, *class, 0x1011, true)], &hdr());
            a.on_pcr(1000, 5000);
            assert!(
                !a.truncation_explains(5010),
                "{class:?} must not excuse a derived anomaly"
            );
        }
    }

    #[test]
    fn psi_flip_defers_until_a_pat_or_pmt_packet_and_never_touches_the_crc() {
        let b = baseline_bytes(6.0, "psi");
        let (w, text, _) = run_tap(&b, cfg(&[Class::PsiFlip], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        assert!(!inj.is_empty());
        for i in &inj {
            assert!(i.psi && i.detectable && matches!(i.pid, 0 | 0x1000));
            let pkt = &w[i.ordinal as usize * 188..][..188];
            let info = crate::rawts::classify_packet(pkt.try_into().unwrap()).unwrap();
            // Section body = after pointer field + 3-byte header, excluding the 4 CRC bytes.
            let sec = info.payload_off + 1 + usize::from(pkt[info.payload_off]);
            let len = (usize::from(pkt[sec + 1] & 0x0F) << 8) | usize::from(pkt[sec + 2]);
            let body = sec + 3..sec + 3 + len - 4;
            assert!(i.offsets.iter().all(|o| body.contains(o)), "{i:?}");
        }
    }

    #[test]
    fn read_log_round_trips_and_the_parser_fails_closed() {
        let b = baseline_bytes(6.0, "rt");
        let (_, text, _) = run_tap(&b, cfg(&Class::ALL, 10_000, 1000));
        let path = std::env::temp_dir().join(format!(
            "tst-interop-corrupt-rt-{}.jsonl",
            std::process::id()
        ));
        std::fs::write(&path, &text).unwrap();
        let from_file = read_log(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let from_text = parse_log(&text).unwrap();
        assert_eq!(from_file.1, from_text.1);
        assert!(!from_file.1.is_empty());

        let mut lines = text.lines();
        let header = lines.next().unwrap();
        let injection = lines.next().expect("at least one injection");
        // Every rejection names the offending line.
        for (bad, want) in [
            (format!("{header}\n{header}"), "line 2"),
            (injection.to_string(), "line 1"),
            (format!("{header}\nnot json"), "line 2"),
            (
                header.replace("\"tap_version\":1", "\"tap_version\":2"),
                "line 1",
            ),
        ] {
            let e = parse_log(&bad).unwrap_err();
            assert!(e.contains(want), "{e}");
        }
        assert!(parse_log("").unwrap_err().contains("no header"));
    }

    /// `LogTail` must read a file that is STILL GROWING: only complete
    /// lines, never the same injection twice, and a half-written line
    /// held back until the rest of it lands.
    #[test]
    fn log_tail_reads_a_growing_file_one_complete_line_at_a_time() {
        use std::io::Write as _;

        // 60s (~3600 packets) so `min_gap`'s 1000-packet floor still
        // leaves several injections to hand out one at a time.
        let b = baseline_bytes(60.0, "tail");
        let (_, text, _) = run_tap(&b, cfg(&Class::ALL, 10_000, 1000));
        let mut lines = text.lines();
        let header = lines.next().unwrap().to_string();
        let injections: Vec<String> = lines.map(str::to_string).collect();
        assert!(injections.len() >= 2, "need two injections to tail");

        let path = std::env::temp_dir().join(format!(
            "tst-interop-corrupt-tail-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{header}").unwrap();
        f.flush().unwrap();

        let mut tail = LogTail::open(&path).unwrap();
        assert_eq!(tail.header().tap_version, TAP_VERSION);
        assert!(tail.poll().unwrap().is_empty(), "nothing appended yet");

        writeln!(f, "{}", injections[0]).unwrap();
        f.flush().unwrap();
        let first = tail.poll().unwrap();
        assert_eq!(first.len(), 1);
        assert!(tail.poll().unwrap().is_empty(), "no injection twice");

        // A torn line: the sender is mid-write. Nothing is returned until
        // the newline lands, and then the whole line parses.
        let (head, rest) = injections[1].split_at(injections[1].len() / 2);
        write!(f, "{head}").unwrap();
        f.flush().unwrap();
        assert!(tail.poll().unwrap().is_empty(), "a torn line is held back");
        writeln!(f, "{rest}").unwrap();
        f.flush().unwrap();
        let second = tail.poll().unwrap();
        assert_eq!(second.len(), 1);
        assert!(
            second[0].ordinal > first[0].ordinal,
            "injections come back in log order"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// `LogTail::open` fails closed on a file with no complete header
    /// line yet — the caller's cue to wait and retry, not to proceed with
    /// an empty judgement.
    #[test]
    fn log_tail_open_refuses_a_headerless_file() {
        let path = std::env::temp_dir().join(format!(
            "tst-interop-corrupt-tail-empty-{}.jsonl",
            std::process::id()
        ));
        std::fs::write(&path, b"").unwrap();
        let e = LogTail::open(&path).unwrap_err();
        assert!(e.contains("no header line yet"), "{e}");
        // A header line without its newline is not a line yet either.
        std::fs::write(&path, b"{\"header\":{").unwrap();
        assert!(LogTail::open(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }

    fn archive_of(n: usize, tag: &str) -> (std::path::PathBuf, usize) {
        let b = baseline_bytes(60.0, tag);
        let (_, text, _) = run_tap(&b, cfg(&Class::ALL, 10_000, 1000));
        let mut lines = text.lines();
        let header = lines.next().unwrap();
        let first = lines.next().expect("fixture must inject");
        let mut archive = format!("{header}\n");
        for _ in 0..n {
            archive.push_str(first);
            archive.push('\n');
        }
        let path = std::env::temp_dir().join(format!(
            "tst-interop-{tag}-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, &archive).unwrap();
        (path, first.len())
    }

    /// A receiver attaching to a log that already holds hours of injections
    /// (a hold's restarted leg) keeps only the newest `keep_last`: older
    /// anchors are ones it never listened for, and the oldest would sit
    /// past the 33-bit PCR half-range and block resolution outright.
    #[test]
    fn skip_history_keeps_the_newest_injections() {
        let (path, _) = archive_of(5000, "skip-history");
        let mut tail = LogTail::open(&path).unwrap();
        let skipped = tail.skip_history(4096).unwrap();
        let kept = tail.poll().unwrap();
        assert_eq!(
            tail.poll().unwrap().len(),
            0,
            "nothing left after the kept tail"
        );
        drop(tail);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(skipped, 904);
        assert_eq!(kept.len(), 4096);
    }

    /// Fewer injections than the bound (every first receiver, which attaches
    /// to a log the sender has barely started): nothing is skipped and the
    /// order is kept.
    #[test]
    fn skip_history_keeps_everything_under_the_bound() {
        let (path, _) = archive_of(10, "skip-small");
        let mut tail = LogTail::open(&path).unwrap();
        assert_eq!(tail.skip_history(4096).unwrap(), 0);
        let kept = tail.poll().unwrap();
        drop(tail);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(kept.len(), 10);
    }

    /// `keep_last = 0` is a real input (a judgement to keep no history at
    /// all), and the bound has to hold there too: every injection counts
    /// as skipped and `held` never grows past empty.
    #[test]
    fn skip_history_zero_keeps_nothing() {
        let (path, _) = archive_of(10, "skip-zero");
        let mut tail = LogTail::open(&path).unwrap();
        let skipped = tail.skip_history(0).unwrap();
        assert!(tail.held.is_empty(), "keep_last=0 must hold nothing");
        assert_eq!(tail.poll().unwrap().len(), 0, "nothing left to return");
        drop(tail);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(skipped, 10);
    }

    /// The skipped count rides in the report so a coverage check can add it
    /// back to what the receiver ingested.
    #[test]
    fn history_skipped_is_reported() {
        let mut a = Attribution::lossy(vec![inj(1_000, 2, Class::Drop, 0x1011, true)], &hdr());
        a.note_history_skipped(904);
        a.on_pcr(1_000, 10);
        a.on_signal(13, Some(0x1011), Signal::ContinuityJump);
        a.on_media(20, 0x1011);
        let r = a.finish(2_000);
        assert_eq!(r.history_skipped, 904);
        assert_eq!(r.injected, 1);
    }

    /// A restarted receiver keeps the newest history in the log at attach,
    /// but its first anchor is far past every one of them: they strand.
    /// They stay `unresolved`, and are also named as history it could
    /// never have heard, so coverage can judge only what it could.
    #[test]
    fn attach_history_stranded_before_the_first_anchor_is_unheard() {
        let live = 1_000 + 10 * MAX_APPROX_TICKS;
        let mut a = Attribution::lossy(
            vec![
                inj(1_000, 2, Class::Drop, 0x1011, true),
                inj(1_000, 40, Class::Drop, 0x1011, true),
                inj(1_000, 80, Class::Drop, 0x1011, true),
                inj(live, 2, Class::Drop, 0x1011, true),
            ],
            &hdr(),
        );
        a.on_pcr(live, 10);
        a.on_signal(13, Some(0x1011), Signal::ContinuityJump);
        a.on_media(20, 0x1011);
        let r = a.finish(2_000);
        assert_eq!(r.history_unheard, 3, "{r:?}");
        assert_eq!(r.resolved, 1);
        assert_eq!(r.unresolved, 3);
        assert_eq!(r.injected, 4);
    }

    /// Only the batch present at attach is history. An injection appended
    /// later that strands is a live one the receiver failed to place.
    #[test]
    fn appended_injections_that_strand_are_not_history() {
        let mut a = Attribution::lossy(vec![inj(400_000, 2, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(400_000, 10);
        a.on_signal(13, Some(0x1011), Signal::ContinuityJump);
        a.on_media(20, 0x1011);
        let mut anchorless = inj(0, 0, Class::Drop, 0x1011, true);
        anchorless.coord.pcr_base = None;
        a.append(vec![
            inj(
                400_000 - 10 * MAX_APPROX_TICKS,
                2,
                Class::Drop,
                0x1011,
                true,
            ),
            anchorless,
        ]);
        let r = a.finish(2_000);
        assert_eq!(r.unresolved, 2, "{r:?}");
        assert_eq!(r.history_unheard, 0, "{r:?}");
    }

    /// A full restart history retires through `prune` long before
    /// `finish`; the count must survive that.
    #[test]
    fn unheard_history_is_counted_when_it_is_pruned() {
        let live = 1_000 + 10 * MAX_APPROX_TICKS;
        let n = PRUNE_BATCH + 5;
        let mut batch: Vec<Injection> = (0..n)
            .map(|k| inj(1_000, k as u64, Class::Drop, 0x1011, true))
            .collect();
        batch.push(inj(live, 2, Class::Drop, 0x1011, true));
        let mut a = Attribution::lossy(batch, &hdr());
        a.on_pcr(live, 10);
        a.on_signal(13, Some(0x1011), Signal::ContinuityJump);
        a.on_media(20, 0x1011);
        assert!(a.base >= PRUNE_BATCH, "the history was not pruned");
        let r = a.finish(2_000);
        assert_eq!(r.history_unheard, n as u64, "{r:?}");
        assert_eq!(r.unresolved, n as u64);
        assert_eq!(r.resolved, 1);
    }

    /// An injection appended mid-capture is judged like any other once a
    /// later PCR places it — but one whose coordinate has NO PCR anchor
    /// arrives too late to be placed at all, and must be left unresolved
    /// rather than blamed on a receiver that never saw that part of the
    /// stream.
    #[test]
    fn appended_injections_resolve_forward_and_anchorless_ones_strand() {
        let hdr = LogHeader {
            tap_version: TAP_VERSION,
            seed: 1,
            rate_per_10k: 5,
            min_gap: 1000,
            classes: Class::ALL.to_vec(),
            attribution_window: ATTRIBUTION_WINDOW,
            recovery_bound: RECOVERY_BOUND,
            approx_slack: APPROX_SLACK,
        };
        let mk = |pcr_base: Option<u64>, since: u64| Injection {
            ordinal: 0,
            coord: Coord {
                pcr_base,
                since_pcr: since,
            },
            class: Class::Drop,
            pid: 0x1011,
            offsets: vec![],
            before: vec![],
            after: vec![],
            detectable: true,
            psi: false,
            pes_start: false,
        };

        let mut a = Attribution::strict(vec![mk(Some(100), 2)], &hdr);
        a.on_pcr(100, 10);
        a.on_signal(13, Some(0x1011), Signal::ContinuityJump);
        a.on_media(20, 0x1011);

        // Appended after the capture is under way: anchored at a base
        // still to come, and at none at all.
        a.append(vec![mk(Some(200), 3), mk(None, 0)]);
        a.on_pcr(200, 500);
        a.on_signal(504, Some(0x1011), Signal::ContinuityJump);
        a.on_media(510, 0x1011);

        let rep = a.finish(2000);
        assert_eq!(rep.injected, 3);
        assert_eq!(rep.resolved, 2, "both anchored injections placed");
        assert_eq!(rep.unresolved, 1, "the anchorless late arrival is not");
        assert_eq!(rep.attributed_events, 2);
        assert!(rep.undetected.is_empty(), "{rep:?}");
        assert!(
            rep.unexplained_events.is_empty(),
            "{:?}",
            rep.unexplained_events
        );
    }

    /// A zero-latency transport (UDP/TCP straight over loopback) hands the
    /// receiver a PCR packet before the sender has even logged the
    /// injection anchored at it: the tap logs per `send_bytes` message,
    /// so an injection in the message AFTER the one carrying its PCR is
    /// written only once that PCR is already on the receiver's side. The
    /// late line must still resolve against the base it names, not
    /// against whichever base the receiver sees next — that is a full PCR
    /// interval downstream, past the event the injection caused.
    #[test]
    fn an_injection_logged_after_its_anchor_pcr_arrived_still_resolves_exactly() {
        let mut a = Attribution::strict(Vec::new(), &hdr());
        a.on_pcr(100, 10);
        a.append(vec![inj(100, 2, Class::Drop, 0x1011, true)]);
        a.on_signal(13, Some(0x1011), Signal::ContinuityJump);
        a.on_media(20, 0x1011);
        // The next base, one 40 ms PCR interval and 80 packets later.
        a.on_pcr(100 + 3600, 90);
        let rep = a.finish(2000);
        assert_eq!(rep.resolved, 1);
        assert_eq!(rep.attributed_events, 1, "{rep:?}");
        assert!(rep.undetected.is_empty(), "{rep:?}");
        assert!(rep.unexplained_events.is_empty(), "{rep:?}");
    }

    /// The order in which the log tail and the wire deliver the same
    /// evidence must not change the verdict: an injection read before its
    /// anchor PCR and one read just after it are the same injection.
    #[test]
    fn appending_before_or_after_the_anchor_pcr_gives_the_same_report() {
        let i1 = || inj(100, 2, Class::Drop, 0x1011, true);
        let i2 = || inj(3700, 3, Class::Header, 0x1011, true);
        let run = |log_first: bool| {
            let mut a = Attribution::strict(Vec::new(), &hdr());
            for (i, base, at) in [(i1(), 100, 10), (i2(), 3700, 1500)] {
                if log_first {
                    a.append(vec![i]);
                    a.on_pcr(base, at);
                } else {
                    a.on_pcr(base, at);
                    a.append(vec![i]);
                }
                a.on_signal(at + 3, Some(0x1011), Signal::ContinuityJump);
                a.on_media(at + 10, 0x1011);
            }
            a.on_pcr(7300, 3000);
            a.finish(4000)
        };
        let (log_first, pcr_first) = (run(true), run(false));
        assert_eq!(log_first.attributed_events, 2, "{log_first:?}");
        assert!(log_first.undetected.is_empty(), "{log_first:?}");
        assert_eq!(log_first, pcr_first);
    }

    /// Before the first PCR there is nothing to strand against, so an
    /// anchorless injection appended then still resolves at ordinal 0 —
    /// the same treatment the constructors give it.
    #[test]
    fn an_anchorless_injection_appended_before_any_pcr_still_resolves() {
        let hdr = LogHeader {
            tap_version: TAP_VERSION,
            seed: 1,
            rate_per_10k: 5,
            min_gap: 1000,
            classes: Class::ALL.to_vec(),
            attribution_window: ATTRIBUTION_WINDOW,
            recovery_bound: RECOVERY_BOUND,
            approx_slack: APPROX_SLACK,
        };
        let mut a = Attribution::strict(Vec::new(), &hdr);
        a.append(vec![Injection {
            ordinal: 0,
            coord: Coord {
                pcr_base: None,
                since_pcr: 4,
            },
            class: Class::Drop,
            pid: 0x1011,
            offsets: vec![],
            before: vec![],
            after: vec![],
            detectable: true,
            psi: false,
            pes_start: false,
        }]);
        a.on_signal(6, Some(0x1011), Signal::ContinuityJump);
        a.on_media(10, 0x1011);
        let rep = a.finish(2000);
        assert_eq!(rep.resolved, 1);
        assert_eq!(rep.unresolved, 0);
        assert_eq!(rep.attributed_events, 1);
        assert!(rep.undetected.is_empty(), "{rep:?}");
    }

    #[test]
    fn psi_flip_drawn_on_a_media_packet_defers_to_the_next_psi_packet() {
        let b = baseline_bytes(60.0, "defer");
        let pid_at = |n: usize| -> u16 {
            crate::rawts::classify_packet(b[n * 188..][..188].try_into().unwrap())
                .unwrap()
                .pid
        };
        let is_psi = |n: usize| matches!(pid_at(n), 0 | 0x1000);

        let (_, text, _) = run_tap(&b, cfg(&[Class::PsiFlip], 10_000, 1000));
        let (_, inj) = parse_log(&text).unwrap();
        assert!(inj.len() >= 2, "{} injection(s)", inj.len());
        // Whatever packet the draw fired on, every injection landed on a
        // PAT or PMT packet.
        for i in &inj {
            assert!(i.psi && matches!(i.pid, 0 | 0x1000), "{i:?}");
            assert!(is_psi(i.ordinal as usize), "ordinal {}", i.ordinal);
        }
        // The first draw fires at ordinal 0, which IS the PAT.
        assert_eq!(inj[0].ordinal, 0);
        // The second can only fire at ordinal >= min_gap, and that packet
        // is media — so the class was held and landed on the next PAT/PMT
        // instead of being re-drawn away.
        assert!(!is_psi(1000), "vacuous unless packet 1000 carries media");
        let next_psi = (1000..b.len() / 188)
            .find(|&n| is_psi(n))
            .expect("a PSI packet follows packet 1000");
        assert!(next_psi > 1000);
        assert_eq!(inj[1].ordinal as usize, next_psi);
    }

    // ---- Attribution ----

    fn hdr() -> LogHeader {
        LogHeader {
            tap_version: 1,
            seed: 1,
            rate_per_10k: 5,
            min_gap: 1000,
            classes: Class::ALL.to_vec(),
            attribution_window: ATTRIBUTION_WINDOW,
            recovery_bound: RECOVERY_BOUND,
            approx_slack: APPROX_SLACK,
        }
    }
    fn inj(coord_pcr: u64, since: u64, class: Class, pid: u16, detectable: bool) -> Injection {
        Injection {
            ordinal: 0,
            coord: Coord {
                pcr_base: Some(coord_pcr),
                since_pcr: since,
            },
            class,
            pid,
            offsets: vec![],
            before: vec![],
            after: vec![],
            detectable,
            psi: false,
            pes_start: false,
        }
    }

    /// The exact PMT packet that failed `corruption_detected` on the
    /// 1-hour re-smoke of main `6f0fbeb7` — injection ordinal 2938545,
    /// reconstructed byte for byte from that run's `corruption.jsonl`.
    ///
    /// 37 meaningful bytes then 151 of 0xFF stuffing: a 4-byte TS header,
    /// a zero pointer field, and a 29-byte PMT section (`table_id` 0x02
    /// at offset 5) carrying the video and KLV elementary streams.
    fn resmoke_pmt_packet() -> [u8; PKT] {
        let mut p = [0xffu8; PKT];
        p[..37].copy_from_slice(&[
            0x47, 0x50, 0x00, 0x14, 0x00, 0x02, 0xb0, 0x1d, 0x00, 0x01, 0xc1, 0x00, 0x00, 0xf0,
            0x11, 0xf0, 0x00, 0x1b, 0xf0, 0x11, 0xf0, 0x00, 0x06, 0xf0, 0x31, 0xf0, 0x06, 0x05,
            0x04, 0x4b, 0x4c, 0x56, 0x41, 0x44, 0x55, 0x53, 0x36,
        ]);
        p
    }

    /// A `body_flip` on a PSI packet must never touch the bytes a decoder
    /// reads to FIND the section — the pointer field, the `table_id`, or
    /// either `section_length` byte.
    ///
    /// The re-smoke's failing injection flipped offset 5, which is the
    /// `table_id`. tst-core's `parse_pmt` rejects that before it ever
    /// reaches the CRC, and `psi_topology.rs` drops such a rejection
    /// silently (only `CrcMismatch` and `MultiSectionUnsupported` become
    /// `NonConformant` events) — so the same injection's flip at offset
    /// 14, inside the CRC'd span and therefore `detectable`, could never
    /// be observed. The tap was over-claiming.
    #[test]
    fn a_psi_body_flip_never_lands_on_the_section_header() {
        let pkt = resmoke_pmt_packet();
        let info = crate::rawts::classify_packet(&pkt).expect("the reconstructed packet parses");
        assert!(info.pusi, "it carries a section start");
        let sec = info.payload_off + 1 + usize::from(pkt[info.payload_off]);
        assert_eq!(sec, 5, "pointer field 0, so the section starts at 5");
        assert_eq!(pkt[sec], 0x02, "and byte 5 really is the PMT table_id");

        let body = psi_section_body_start(&pkt, &info, true).expect("a PSI section body");
        assert_eq!(body, sec + 3, "one past table_id + section_length");

        // And the same rule through the real tap, over 64 seeds of
        // generated traffic, so the PSI PIDs are learned from the PAT the
        // way a live run learns them.
        let b = baseline_bytes(300.0, "bfpsi");
        let mut psi_flips = 0;
        for seed in 1..=64u64 {
            let (_, text, _) = run_tap(
                &b,
                CorruptConfig {
                    seed,
                    ..cfg(&[Class::BodyFlip], 10_000, 1000)
                },
            );
            let (_, inj) = parse_log(&text).unwrap();
            for i in inj.iter().filter(|i| i.psi) {
                psi_flips += 1;
                let pkt: [u8; PKT] = i.before[..].try_into().expect("a whole packet is logged");
                let info = crate::rawts::classify_packet(&pkt).unwrap();
                let Some(body) = psi_section_body_start(&pkt, &info, true) else {
                    continue; // a section continuation, not a start
                };
                for &o in &i.offsets {
                    assert!(
                        o >= body,
                        "seed {seed}: offset {o} is in the section header \
                         (section at {}, body at {body})",
                        info.payload_off + 1 + usize::from(pkt[info.payload_off])
                    );
                }
            }
        }
        assert!(
            psi_flips >= 8,
            "only {psi_flips} PSI body flips across 64 seeds — the sweep is too thin to mean much"
        );
    }

    /// The other half: the flip is still DETECTABLE when it lands inside
    /// the CRC'd span. Narrowing the draw range must not have narrowed
    /// what the tap claims a receiver has to notice — offset 14, the very
    /// byte the re-smoke's injection also hit, still counts.
    #[test]
    fn a_psi_body_flip_inside_the_crc_span_is_still_detectable() {
        let pkt = resmoke_pmt_packet();
        let info = crate::rawts::classify_packet(&pkt).expect("parses");
        let span = sensitive_span(&pkt, &info, true).expect("a PSI sensitive span");
        assert!(
            (span.0..span.1).contains(&14),
            "offset 14 is inside the CRC'd span {span:?}"
        );
        // …and the span still starts at the section body and ends past
        // the CRC, unchanged by this fix.
        assert_eq!(span.0, 8, "body start");
        assert_eq!(span.1, 5 + 3 + 29, "through the end of the CRC");
    }

    /// Ruling A. Under transport loss an injection's own evidence can
    /// surface BEFORE `resolved_at`, because that ordinal is a receiver
    /// count plus a sender-side offset and the packets between the two
    /// are exactly what the network may have eaten. A forward-only window
    /// rejects it and reports the injection undetected for damage the
    /// receiver noticed perfectly well.
    ///
    /// Measured live: 2 of 429 detectable `garbage`/`header` injections
    /// on a 1-hour smoke run's rist leg, whose 100 ms PCR cadence gives the
    /// widest `since_pcr` spans.
    #[test]
    fn an_injections_own_event_may_surface_before_its_resolved_position() {
        // since_pcr 40: 25 of those 40 packets were lost in transit, so
        // the injected packet actually arrived at receiver ordinal 4975
        // while its coordinate resolves to 5000.
        let mut a = Attribution::strict(vec![inj(1000, 40, Class::Garbage, 0x1011, true)], &hdr());
        a.on_pcr(1000, 4960);
        a.on_signal(4975, None, Signal::Resync);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);

        assert_eq!(r.attributed_events, 1, "{r:?}");
        assert_eq!(r.undetected_count, 0, "{:?}", r.undetected);
        assert_eq!(r.unexplained_total(), 0, "{:?}", r.unexplained_events);
    }

    /// The backward half is bounded by the injection's OWN `since_pcr`,
    /// not by the whole window: an event further back than the anchor
    /// span can reach is still unattributable.
    #[test]
    fn the_backward_reach_stops_at_the_anchor_span() {
        let mut a = Attribution::strict(vec![inj(1000, 40, Class::Garbage, 0x1011, true)], &hdr());
        a.on_pcr(1000, 4960); // resolves to 5000, window opens at 4960
        a.on_signal(4959, None, Signal::Resync);
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 0, "{r:?}");
        assert_eq!(r.unexplained_resyncs, 1, "{r:?}");
    }

    /// The clamp that keeps windows disjoint. Two injections one
    /// `min_gap` apart must not both cover one event, or `hit` would hand
    /// it to the newer and rob the older of its detection — the exact
    /// ambiguity `CorruptConfig::validate`'s `min_gap` floor exists to
    /// prevent.
    ///
    /// `since_pcr` really does reach this far: measured at 302 packets on
    /// a 40 ms-PCR profile and 393 on a 100 ms one over a 1-hour smoke
    /// run, against the ~130 a one-PCR-interval model predicts. So the
    /// bound has to come from `min_gap`, not from a cadence assumption.
    #[test]
    fn a_wide_anchor_span_cannot_reach_into_its_predecessors_window() {
        let h = hdr();
        let budget = h.min_gap - (h.attribution_window + APPROX_SLACK) - 1;

        // The worst case for disjointness, built deliberately: the
        // PREDECESSOR resolves APPROXIMATELY, which widens its forward
        // edge by APPROX_SLACK, and the SUCCESSOR carries a `since_pcr`
        // wider than the budget, which is what the clamp has to cut back.
        // Anchoring the first at a base the receiver never saw (1000)
        // and delivering a LATER one (1090) is what makes it approx.
        let wide = budget + 200;
        let mut a = Attribution::strict(
            vec![
                inj(1000, 10, Class::Header, 0x1011, true),
                inj(2000, wide, Class::Header, 0x1011, true),
            ],
            &h,
        );
        // The predecessor's own base (1000) never arrives; the next one
        // does, so it resolves approximately to 5010 with a forward edge
        // widened by APPROX_SLACK.
        a.on_pcr(1090, 5000);
        // The successor resolves exactly, and — as two real consecutive
        // injections always are — exactly `min_gap` further down the
        // stream: 5010 + 1000 = 6010.
        a.on_pcr(2000, 6010 - wide);
        assert!(
            a.state(0).approx,
            "the predecessor must resolve approximately"
        );
        assert!(!a.state(1).approx, "and the successor exactly");
        assert_eq!(a.state(0).resolved_at, Some(5010));
        assert_eq!(a.state(1).resolved_at, Some(6010));

        // The clamp bit at all: the declared span really is wider than
        // the reach the config can fund.
        let reach = a.backward_reach(1);
        assert!(reach < wide, "the wide span must be clamped, got {reach}");

        // Both edges of `window_contains` are inclusive, so the budget
        // has to leave one packet between a widened forward edge and the
        // next window's lower edge. Sweep every ordinal either window can
        // touch and require that none is covered twice.
        let (lo0, hi1) = (
            a.window_lo(0).unwrap(),
            a.state(1).resolved_at.unwrap() + h.attribution_window,
        );
        for at in lo0..=hi1 {
            let covered = (0..2).filter(|&i| a.window_contains(i, at)).count();
            assert!(covered <= 1, "packet {at} covered by {covered} windows");
        }
        // …and the sweep is not vacuous: the two windows really do come
        // within a packet or two of each other.
        let gap = a.window_lo(1).unwrap()
            - (a.state(0).resolved_at.unwrap() + h.attribution_window + APPROX_SLACK);
        assert!(
            gap <= 2,
            "windows must be adjacent for this to test anything, gap {gap}"
        );
    }

    /// The same disjointness at AU scale 4, where every number in the
    /// header is 4× — including the slack an approximate resolution
    /// widens by. An engine that kept the scale-1 slack would fund a
    /// backward reach 384 packets too generous and let a successor's
    /// window overlap its predecessor's widened edge.
    #[test]
    fn backward_reach_is_clamped_with_the_headers_scaled_slack() {
        let mut h = hdr();
        h.min_gap = 4000;
        h.attribution_window = 2000;
        h.recovery_bound = 2400;
        h.approx_slack = 512;
        let budget = h.min_gap - (h.attribution_window + h.approx_slack) - 1;
        let wide = budget + 200;
        let mut a = Attribution::strict(
            vec![
                inj(1000, 10, Class::Header, 0x1011, true),
                inj(2000, wide, Class::Header, 0x1011, true),
            ],
            &h,
        );
        a.on_pcr(1090, 5000);
        a.on_pcr(2000, 5010 + h.min_gap - wide);
        assert!(a.state(0).approx);
        assert_eq!(a.state(1).resolved_at, Some(5010 + h.min_gap));
        assert_eq!(
            a.backward_reach(1),
            budget,
            "the clamp must use the header's slack, not the scale-1 constant"
        );
        let (lo0, hi1) = (
            a.window_lo(0).unwrap(),
            a.state(1).resolved_at.unwrap() + h.attribution_window,
        );
        for at in lo0..=hi1 {
            let covered = (0..2).filter(|&i| a.window_contains(i, at)).count();
            assert!(covered <= 1, "packet {at} covered by {covered} windows");
        }
        let gap = a.window_lo(1).unwrap()
            - (a.state(0).resolved_at.unwrap() + h.attribution_window + h.approx_slack);
        assert!(gap <= 2, "windows must be adjacent, gap {gap}");
    }

    /// Ruling B. A continuity jump inside an injection's window, on the
    /// PID it damaged, excuses that injection when its class does NOT
    /// expect a jump: there the jump means the datagram carrying the
    /// injection was lost in transit, so the damage never reached the
    /// receiver to be noticed.
    ///
    /// The excusal survives; the ATTRIBUTION does not. A signal
    /// of a class the injection cannot cause is no longer explained by it
    /// (`can_explain`), so the jump is unexplained — transport loss under
    /// lossy judgement, a discontinuity finding under strict — while
    /// still marking the injection's packet as plausibly missing.
    #[test]
    fn a_cc_jump_a_class_does_not_expect_stays_unexplained_but_still_excuses() {
        let build = |lossy: bool| {
            let log = vec![inj(1000, 10, Class::PsiFlip, 0, true)];
            let mut a = if lossy {
                Attribution::lossy(log, &hdr())
            } else {
                Attribution::strict(log, &hdr())
            };
            a.on_pcr(1000, 5000);
            // Its own jump, inside its own window, attributed to it — and
            // NOT a signal `expects` accepts for a psi_flip.
            a.on_signal(5100, Some(0), Signal::ContinuityJump);
            a.on_media(5200, 0x1011);
            a.finish(10_000)
        };

        let lossy = build(true);
        assert_eq!(
            lossy.attributed_events, 0,
            "class-incompatible: not explained"
        );
        assert_eq!(lossy.unexplained_transport_loss, 1, "{lossy:?}");
        assert_eq!(lossy.undetected_lost, 1, "{lossy:?}");
        assert_eq!(lossy.undetected_count, 0, "{:?}", lossy.undetected);

        // Strict has no transport to blame, so the finding stands.
        let strict = build(false);
        assert_eq!(strict.undetected_count, 1, "{strict:?}");
        assert_eq!(strict.undetected_lost, 0, "{strict:?}");
        assert_eq!(strict.unexplained_discontinuities, 1, "{strict:?}");
    }

    /// The other half of Ruling B, and the reason the blanket exclusion
    /// existed: for a class whose expected signals INCLUDE a continuity
    /// jump, its own jump is the detection and must never also excuse it.
    #[test]
    fn an_attributed_cc_jump_still_detects_a_class_that_expects_one() {
        for class in [Class::Drop, Class::Header, Class::Truncate, Class::Garbage] {
            let mut a = Attribution::lossy(vec![inj(1000, 10, class, 0x1011, true)], &hdr());
            a.on_pcr(1000, 5000);
            a.on_signal(5100, Some(0x1011), Signal::ContinuityJump);
            a.on_media(5200, 0x1011);
            let r = a.finish(10_000);
            assert_eq!(r.undetected_count, 0, "{class:?}: {r:?}");
            assert_eq!(
                r.undetected_lost, 0,
                "{class:?}: excused, not detected: {r:?}"
            );
        }
    }

    /// C2, the regression this exists for: on a lossy multi-day run the
    /// excused continuity jumps must not be able to crowd an unexplained
    /// RESYNC out of the evidence.
    ///
    /// A resync is the one signal with no second detection path — it is
    /// not a `DemuxEvent`, and resync mode is exactly what suppresses the
    /// `rawts_sync_loss` failure that would otherwise catch it — so if
    /// the sample cap swallows it, `corruption_attributed` passes a run
    /// whose receiver lost packet sync for reasons nothing explains.
    #[test]
    fn excused_jumps_never_crowd_out_an_unexplained_resync() {
        let mut a = Attribution::lossy(vec![inj(1000, 10, Class::Header, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        // Comfortably more excusable jumps than the sample cap, none of
        // them inside any injection's window.
        for k in 0..(MAX_SAMPLES as u64 * 4) {
            a.on_signal(20_000 + k, Some(0x1011), Signal::ContinuityJump);
        }
        a.on_signal(90_000, None, Signal::Resync);
        let r = a.finish(100_000);

        assert_eq!(r.unexplained_transport_loss, MAX_SAMPLES as u64 * 4);
        assert_eq!(r.unexplained_resyncs, 1);
        assert_eq!(
            r.unexplained_total(),
            1,
            "the resync is the only corruption evidence here: {r:?}"
        );
        assert!(
            r.unexplained_events.iter().any(|e| e.contains("Resync")),
            "and it must be quotable in the failure: {:?}",
            r.unexplained_events
        );
    }

    /// A continuity jump on a PID an injection never touched is not that
    /// injection's evidence. Today `hit` places by
    /// window position alone, so the foreign jump is "attributed" and
    /// the `Drop` counts as detected.
    #[test]
    fn attribution_rejects_unrelated_pid() {
        let mut a = Attribution::strict(vec![inj(1000, 10, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5020, Some(0x1031), Signal::ContinuityJump);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 0, "{r:?}");
        assert_eq!(r.undetected_count, 1, "{r:?}");
        assert_eq!(r.unexplained_discontinuities, 1, "{r:?}");
    }

    /// PR review finding: `reaches` treated `Signal::PsiChecksum` as
    /// PID-unconstrained for ANY injection (its own doc claimed "a PSI
    /// checksum failure is the PSI PID's own", but the code never
    /// checked that `inj` was itself PSI-related), so a same-window
    /// `Drop` on an unrelated media PID could "explain" a PAT/PMT
    /// checksum failure it never came near — `Class::Drop`'s class check
    /// is a vacuous `_ => true` for `PsiChecksum`, so only the PID rule
    /// can be doing the rejecting here.
    #[test]
    fn attribution_rejects_a_psi_checksum_signal_from_a_non_psi_injection() {
        let mut a = Attribution::strict(vec![inj(1000, 10, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5020, Some(0), Signal::PsiChecksum);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 0, "{r:?}");
        assert_eq!(r.undetected_count, 1, "{r:?}");
        assert_eq!(r.unexplained_nonconformant, 1, "{r:?}");

        // Positive control: an injection that DID touch a PSI table still
        // gets the "PSI checksum failure is the PSI PID's own" escape.
        let mut psi_inj = inj(1000, 10, Class::PsiFlip, 0, true);
        psi_inj.psi = true;
        let mut a = Attribution::strict(vec![psi_inj], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5020, Some(0), Signal::PsiChecksum);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 1, "{r:?}");
        assert_eq!(r.undetected_count, 0, "{r:?}");
    }

    /// A PMT truncated inside its section reaches the demuxer as a section
    /// whose CRC fails: `PsiChecksum` is the receiver noticing, and it must
    /// count for a framing-class injection whose own PID is a PSI PID (six
    /// RC-soak RIST events were "unexplained" for exactly this).
    #[test]
    fn a_framing_injection_on_a_psi_pid_is_noticed_by_a_psi_checksum() {
        let mut pmt_cut = inj(1_000, 2, Class::Truncate, 0x1000, true);
        pmt_cut.psi = true;
        let mut a = Attribution::lossy(vec![pmt_cut], &hdr());
        a.on_pcr(1_000, 10);
        a.on_signal(16, Some(0x1000), Signal::PsiChecksum);
        a.on_media(30, 0x1011);
        let r = a.finish(2_000);
        assert_eq!(r.attributed_events, 1);
        assert_eq!(r.unexplained_total(), 0);
        assert_eq!(r.undetected_total(), 0);

        // The same signal on a NON-PSI framing injection is still not its
        // expected effect (nothing in a media packet has a CRC to fail).
        let mut a = Attribution::lossy(vec![inj(1_000, 2, Class::Truncate, 0x1011, true)], &hdr());
        a.on_pcr(1_000, 10);
        a.on_signal(16, Some(0x1011), Signal::PsiChecksum);
        a.on_media(30, 0x1011);
        let r = a.finish(2_000);
        assert_eq!(r.attributed_events, 0);
    }

    /// Same PID, wrong class: a `Drop` can only surface as a continuity
    /// jump, so a malformed-PES report on its own PID inside its window
    /// is somebody else's problem, not its detection.
    #[test]
    fn attribution_rejects_a_class_incompatible_signal_on_the_same_pid() {
        let mut a = Attribution::strict(vec![inj(1000, 10, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5020, Some(0x1011), Signal::MalformedPes);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 0, "{r:?}");
        assert_eq!(r.undetected_count, 1, "{r:?}");
        assert_eq!(r.unexplained_nonconformant, 1, "{r:?}");
    }

    /// The loss-excusal follows the same rule: a jump on a foreign PID
    /// says nothing about whether THIS injection's packet was lost, so
    /// under lossy judgement it neither detects nor excuses a PSI flip
    /// (whose packet would have to be missing from PID 0 to be excused).
    #[test]
    fn loss_excusal_needs_a_jump_on_the_injections_own_pid() {
        let mut a = Attribution::lossy(vec![inj(1000, 10, Class::PsiFlip, 0, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5020, Some(0x1031), Signal::ContinuityJump);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            r.undetected_lost, 0,
            "a foreign jump must not excuse: {r:?}"
        );
        assert_eq!(r.undetected_count, 1, "{r:?}");
        assert_eq!(r.unexplained_transport_loss, 1, "{r:?}");

        // Positive control: the jump on the PSI PID itself does excuse.
        let mut a = Attribution::lossy(vec![inj(1000, 10, Class::PsiFlip, 0, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5020, Some(0), Signal::ContinuityJump);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(r.undetected_lost, 1, "{r:?}");
        assert_eq!(r.undetected_count, 0, "{r:?}");
    }

    /// Framing-wide classes reach every PID: a truncation, a garbage
    /// run or a destroyed sync byte breaks the multiplex, so a jump on
    /// any PID inside the window is theirs. A `Header` that rewrote the
    /// CC (offset 3) is confined to its own PID.
    #[test]
    fn framing_wide_injections_explain_signals_on_any_pid() {
        for class in [Class::Truncate, Class::Garbage] {
            let mut a = Attribution::strict(vec![inj(1000, 10, class, 0x1011, true)], &hdr());
            a.on_pcr(1000, 5000);
            a.on_signal(5020, Some(0x1031), Signal::ContinuityJump);
            a.on_media(5100, 0x1031);
            let r = a.finish(10_000);
            assert_eq!(r.attributed_events, 1, "{class:?}: {r:?}");
            assert_eq!(r.undetected_count, 0, "{class:?}: {r:?}");
        }
        let sync_byte = |offsets: Vec<usize>| {
            let mut i = inj(1000, 10, Class::Header, 0x1011, true);
            i.offsets = offsets;
            let mut a = Attribution::strict(vec![i], &hdr());
            a.on_pcr(1000, 5000);
            a.on_signal(5020, Some(0x1031), Signal::ContinuityJump);
            a.on_media(5100, 0x1031);
            a.finish(10_000)
        };
        assert_eq!(
            sync_byte(vec![0]).attributed_events,
            1,
            "sync-byte header damage is framing-wide"
        );
        assert_eq!(
            sync_byte(vec![3]).attributed_events,
            0,
            "a CC rewrite is PID-local"
        );
    }

    /// A resync is the HARNESS's raw reader noticing, not the receiver.
    /// A garbage run detected only by a resync is counted
    /// apart from a drop the demuxer itself reported.
    #[test]
    fn detection_credit_splits_reader_resyncs_from_demux_events() {
        let mut a = Attribution::strict(
            vec![
                inj(1000, 10, Class::Garbage, 0x1011, true),
                inj(1000, 2010, Class::Drop, 0x1011, true),
                inj(1000, 4010, Class::Garbage, 0x1011, true),
            ],
            &hdr(),
        );
        a.on_pcr(1000, 5000);
        a.on_signal(5015, None, Signal::Resync); // garbage #1: reader only
        a.on_media(5100, 0x1011);
        a.on_signal(7015, Some(0x1011), Signal::ContinuityJump); // drop: demux
        a.on_media(7100, 0x1011);
        a.on_signal(9012, None, Signal::Resync); // garbage #2: reader …
        a.on_signal(9015, Some(0x1011), Signal::ContinuityJump); // … then demux too
        a.on_media(9100, 0x1011);
        let r = a.finish(20_000);
        assert_eq!(r.undetected_count, 0, "{r:?}");
        assert_eq!(r.detected_by_demux, 2, "{r:?}");
        assert_eq!(r.detected_by_reader_only, 1, "{r:?}");
        assert_eq!(
            r.detected_by_reader_only_per_class.get("garbage"),
            Some(&1),
            "{r:?}"
        );
    }

    /// The uncapped counters keep counting after the sample list stops.
    /// A verdict reading `unexplained_events.len()` would report 64 for
    /// any run above the cap, which is how a badly-broken run reads as a
    /// mildly-broken one.
    #[test]
    fn unexplained_counts_are_uncapped_while_the_sample_list_is_capped() {
        let mut a = Attribution::strict(vec![], &hdr());
        let n = MAX_SAMPLES as u64 * 10;
        for k in 0..n {
            a.on_signal(1000 + k, Some(0x1011), Signal::PsiChecksum);
        }
        let r = a.finish(100_000);
        assert_eq!(r.unexplained_events.len(), MAX_SAMPLES);
        assert_eq!(r.unexplained_nonconformant, n);
        assert_eq!(r.unexplained_total(), n);
    }

    /// The same cap/count split for the per-injection verdicts (I4):
    /// `undetected`/`unrecovered` are samples, `*_count` are the truth.
    #[test]
    fn undetected_and_unrecovered_lists_are_capped_and_their_counts_are_not() {
        let n = MAX_SAMPLES as u64 * 3;
        // Detectable injections, spaced past every window, none of which
        // ever produces an event or any media afterwards.
        let log: Vec<Injection> = (0..n)
            .map(|k| inj(1000, 10 + k * 2000, Class::Drop, 0x1011, true))
            .collect();
        let mut a = Attribution::strict(log, &hdr());
        a.on_pcr(1000, 0);
        // Walk the cursors past every window so the retirement path does
        // the counting, not just the final sweep.
        a.on_media(10 + n * 2000 + 10_000, 0x2222);
        let r = a.finish(10 + n * 2000 + 20_000);

        assert_eq!(r.undetected_count, n);
        assert_eq!(r.unrecovered_count, n);
        assert_eq!(r.undetected.len(), MAX_SAMPLES);
        assert_eq!(r.unrecovered.len(), MAX_SAMPLES);
    }

    /// C1, the memory half: the engine must stop holding the log.
    ///
    /// A 72-hour soak logs a quarter of a million injections, each
    /// carrying `before`/`after` byte images, against a receive process
    /// whose own `rss_slope_*_recv` verdict gates at 200 KiB/h. Retained
    /// state has to be bounded by the window the cursors are actually
    /// working in, not by the length of the run.
    #[test]
    fn a_long_run_retires_injections_instead_of_accumulating_them() {
        let n = 20_000u64;
        let mut a = Attribution::strict(vec![], &hdr());
        let mut peak = 0usize;
        for k in 0..n {
            let at = 10 + k * 2000;
            a.append(vec![inj(1000, at, Class::Drop, 0x1011, true)]);
            // A live receiver feeds every PCR it decodes, which is what
            // turns a logged coordinate into a receiver position — and
            // therefore what lets the resolution cursor move on.
            a.on_pcr(1000, 0);
            // One event per injection, inside its own window, which is
            // what walks the scan cursors forward.
            a.on_signal(at + 5, Some(0x1011), Signal::ContinuityJump);
            a.on_media(at + 10, 0x1011);
            peak = peak.max(a.retained());
        }
        assert!(
            peak <= PRUNE_BATCH + 16,
            "retained {peak} injections at peak against a {PRUNE_BATCH}-injection prune batch"
        );

        // …and the verdict is unchanged by the pruning: every one of them
        // was detected and recovered.
        let r = a.finish(10 + n * 2000 + 10_000);
        assert_eq!(r.injected, n);
        assert_eq!(r.resolved, n);
        assert_eq!(r.undetected_count, 0, "{:?}", r.undetected);
        assert_eq!(r.unrecovered_count, 0, "{:?}", r.unrecovered);
    }

    /// Pruning must not change a verdict. The same event stream is judged
    /// twice — once long enough to retire most of the log, once short
    /// enough that nothing retires — and the two reports must agree on
    /// every counter.
    #[test]
    fn retiring_injections_does_not_change_the_verdict() {
        let judge = |n: u64| {
            let log: Vec<Injection> = (0..n)
                .map(|k| inj(1000, 10 + k * 2000, Class::Drop, 0x1011, true))
                .collect();
            let mut a = Attribution::strict(log, &hdr());
            a.on_pcr(1000, 0);
            for k in 0..n {
                let at = 10 + k * 2000;
                // Every third injection is left with no event at all, so
                // both the passing and the failing paths are exercised.
                if k % 3 != 0 {
                    a.on_signal(at + 5, Some(0x1011), Signal::ContinuityJump);
                }
                a.on_media(at + 10, 0x1011);
            }
            a.finish(10 + n * 2000 + 10_000)
        };
        // Below the batch nothing is ever retired; well above it, most of
        // the log is. Both must produce the arithmetically exact verdict.
        for n in [100u64, PRUNE_BATCH as u64 * 3] {
            let r = judge(n);
            assert_eq!(r.injected, n, "n={n}: {r:?}");
            assert_eq!(r.resolved, n, "n={n}: {r:?}");
            assert_eq!(r.detectable, n, "n={n}: {r:?}");
            // Exactly the every-third injections that got no event.
            assert_eq!(r.undetected_count, n.div_ceil(3), "n={n}: {r:?}");
            assert_eq!(r.unrecovered_count, 0, "n={n}: {r:?}");
        }
    }

    #[test]
    fn attribution_explains_events_inside_the_window_and_flags_the_rest() {
        let mut a = Attribution::strict(vec![inj(1000, 10, Class::Header, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000); // injection resolves to receiver ordinal 5010
        a.on_signal(5020, Some(0x1011), Signal::ContinuityJump); // explained
        a.on_media(5100, 0x1011); // recovered
        a.on_signal(9000, Some(0x1011), Signal::ContinuityJump); // unexplained
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 1);
        assert_eq!(r.attributed_discontinuities, 1);
        assert_eq!(r.unexplained_events.len(), 1);
        assert!(r.undetected.is_empty() && r.unrecovered.is_empty());
        assert_eq!(r.resolved, 1);
    }

    /// Transport-loss excusal, the Lossy half. An unexplained
    /// `ContinuityJump` says packets went missing, which on a live
    /// impaired link is the link doing its job — it is already counted in
    /// the verifier's own `discontinuities`, and it is not corruption
    /// evidence. `Strict` keeps failing on the identical input, because a
    /// file has no transport to lose anything.
    #[test]
    fn lossy_excuses_an_unexplained_continuity_jump_and_strict_does_not() {
        let build = |lossy: bool| {
            let log = vec![inj(1000, 10, Class::Header, 0x1011, true)];
            let mut a = if lossy {
                Attribution::lossy(log, &hdr())
            } else {
                Attribution::strict(log, &hdr())
            };
            a.on_pcr(1000, 5000); // resolves to receiver ordinal 5010
            a.on_signal(5020, Some(0x1011), Signal::ContinuityJump); // explained
            a.on_media(5100, 0x1011);
            // Far outside every window: nothing in the log explains it.
            a.on_signal(9000, Some(0x1011), Signal::ContinuityJump);
            a
        };

        let lossy = build(true).finish(10_000);
        assert!(
            lossy.unexplained_events.is_empty(),
            "a transport gap is not corruption evidence: {:?}",
            lossy.unexplained_events
        );
        assert_eq!(lossy.unexplained_transport_loss, 1);
        // The raw tallies still reconcile: the event was counted, just not
        // held against the tap.
        assert_eq!(lossy.events, 2);
        assert_eq!(lossy.attributed_events, 1);

        let strict = build(false).finish(10_000);
        assert_eq!(strict.unexplained_events.len(), 1);
        assert_eq!(strict.unexplained_discontinuities, 1);
        assert_eq!(strict.unexplained_transport_loss, 0);
    }

    /// The other families are NOT excused: a dropped packet cannot forge a
    /// bad PSI CRC or a malformed PES header, so an unexplained one of
    /// those still fails in Lossy exactly as in Strict.
    #[test]
    fn lossy_still_fails_an_unexplained_nonconformance_or_resync() {
        for sig in [
            Signal::PsiChecksum,
            Signal::MalformedPes,
            Signal::OtherNonConformant,
            Signal::Resync,
        ] {
            let mut a =
                Attribution::lossy(vec![inj(1000, 10, Class::Header, 0x1011, true)], &hdr());
            a.on_pcr(1000, 5000);
            a.on_signal(9000, Some(0x1011), sig);
            let r = a.finish(10_000);
            assert_eq!(
                r.unexplained_events.len(),
                1,
                "{sig:?} must survive transport-loss excusal"
            );
            assert_eq!(r.unexplained_transport_loss, 0, "{sig:?}");
        }
    }

    /// A forward PCR jump of two seconds, the shape a span of lost packets
    /// leaves behind.
    const PCR_FORWARD: Signal = Signal::PcrAnomaly { delta: 54_000_000 };

    /// A lossy or strict attribution over `log` whose receiver reported
    /// one program with its time base on the video PID.
    fn with_video_pcr(lossy: bool, log: Vec<Injection>) -> Attribution {
        let mut a = if lossy {
            Attribution::lossy(log, &hdr())
        } else {
            Attribution::strict(log, &hdr())
        };
        a.on_program_map(1, 0x1000, 0x1011);
        a
    }

    /// A forward PCR jump that coincides with a continuity gap on its own
    /// PID is that gap's timestamp signature — packets went missing for
    /// longer than the anomaly threshold — not corruption. Under lossy
    /// judgement it is transport loss; strict has no transport and charges
    /// it. Run 3 (2026-09-17): ~17 such jumps on the srt video PID, 1–3 per
    /// outage window, every one beside an excused continuity jump.
    #[test]
    fn a_pcr_jump_beside_a_continuity_gap_is_transport_loss_only_under_lossy() {
        // Far-away, never-resolved injection so the log is non-empty.
        let bystander = || vec![inj(1000, 0, Class::Drop, 0x1011, true)];
        for (lossy, want_excused) in [(true, 1u64), (false, 0u64)] {
            let mut a = with_video_pcr(lossy, bystander());
            // Gap first, jump inside the window after it.
            a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
            a.on_signal(5000 + ATTRIBUTION_WINDOW, Some(0x1011), PCR_FORWARD);
            let r = a.finish(10_000);
            assert_eq!(
                r.pcr_anomalies_excused, want_excused,
                "lossy={lossy}: {r:?}"
            );
            assert_eq!(
                r.unexplained_nonconformant,
                1 - want_excused,
                "lossy={lossy}: {r:?}"
            );
        }

        // Same packet, jump QUEUED FIRST (tst_core's check_pcr runs before
        // check_continuity): the gap evidence arrives one event late.
        let mut a = with_video_pcr(true, bystander());
        a.on_signal(5000, Some(0x1011), PCR_FORWARD);
        a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (1, 0),
            "{r:?}"
        );

        // A gap on a DIFFERENT PID is not this PID's signature.
        let mut a = with_video_pcr(true, bystander());
        a.on_signal(5000, Some(0x1100), Signal::ContinuityJump);
        a.on_signal(5010, Some(0x1011), PCR_FORWARD);
        assert_eq!(a.finish(10_000).unexplained_nonconformant, 1);

        // Beyond the window, before or after: a finding.
        let mut a = with_video_pcr(true, bystander());
        a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
        a.on_signal(5001 + ATTRIBUTION_WINDOW, Some(0x1011), PCR_FORWARD);
        assert_eq!(a.finish(10_000).unexplained_nonconformant, 1);
        let mut a = with_video_pcr(true, bystander());
        a.on_signal(5000, Some(0x1011), PCR_FORWARD);
        a.on_signal(
            5001 + ATTRIBUTION_WINDOW,
            Some(0x1011),
            Signal::ContinuityJump,
        );
        let r = a.finish(10_000);
        assert_eq!(r.unexplained_nonconformant, 1, "{r:?}");
        assert!(
            r.unexplained_events
                .iter()
                .any(|e| e.starts_with("PcrAnomaly { delta: 54000000 } on pid Some(4113)")),
            "a charged pending anomaly keeps its sample text: {:?}",
            r.unexplained_events
        );

        // A pending anomaly at the end of the capture is charged by finish,
        // never dropped.
        let mut a = with_video_pcr(true, bystander());
        a.on_signal(9990, Some(0x1011), PCR_FORWARD);
        let r = a.finish(10_000);
        assert_eq!((r.events, r.unexplained_nonconformant), (1, 1), "{r:?}");
    }

    /// The shape run 3 measured, 1–3 anomalies per outage window: every
    /// anomaly beside a gap of its own, so every one is excused. One gap,
    /// one anomaly is a bound on what ONE loss explains, not on the run.
    #[test]
    fn each_pcr_jump_beside_its_own_gap_is_excused() {
        let mut a = with_video_pcr(true, vec![inj(1000, 0, Class::Drop, 0x1011, true)]);
        for at in [5000, 5040, 5090] {
            // The order tst_core emits them in, on one packet.
            a.on_signal(at, Some(0x1011), PCR_FORWARD);
            a.on_signal(at, Some(0x1011), Signal::ContinuityJump);
        }
        // And the other order: the gap a few packets before its PCR.
        a.on_signal(5200, Some(0x1011), Signal::ContinuityJump);
        a.on_signal(5205, Some(0x1011), PCR_FORWARD);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (4, 0),
            "{r:?}"
        );
    }

    /// One continuity gap is one span of lost packets. The first PCR after
    /// it carries the whole jump and the demuxer re-bases on it, so the gap
    /// is the cause of that one anomaly and of no later one: a further
    /// anomaly on the PID, with no further gap, is a clock misbehaving on
    /// packets that all arrived.
    #[test]
    fn one_gap_excuses_at_most_one_pcr_anomaly() {
        let mut a = with_video_pcr(true, vec![inj(1000, 0, Class::Drop, 0x1011, true)]);
        a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
        a.on_signal(5010, Some(0x1011), PCR_FORWARD);
        a.on_signal(5020, Some(0x1011), PCR_FORWARD);
        a.on_signal(5030, Some(0x1011), PCR_FORWARD);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (1, 2),
            "{r:?}"
        );

        // The same bound when the anomaly arrives first and the gap on its
        // packet claims it: the gap is spent, and the next anomaly is not
        // excused against it.
        let mut a = with_video_pcr(true, vec![inj(1000, 0, Class::Drop, 0x1011, true)]);
        a.on_signal(5000, Some(0x1011), PCR_FORWARD);
        a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
        a.on_signal(5010, Some(0x1011), PCR_FORWARD);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (1, 1),
            "{r:?}"
        );
    }

    /// Packets that go missing take their share of the timeline with
    /// them, so loss moves the clock FORWARD. A PCR behind its predecessor
    /// is never a gap's signature, however close the gap: before it, or on
    /// the same packet.
    #[test]
    fn a_backward_pcr_jump_beside_a_gap_is_a_finding() {
        let back = Signal::PcrAnomaly { delta: -27_000_000 };
        let mut a = with_video_pcr(true, vec![inj(1000, 0, Class::Drop, 0x1011, true)]);
        a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
        a.on_signal(5010, Some(0x1011), back);
        a.on_signal(6000, Some(0x1011), back);
        a.on_signal(6000, Some(0x1011), Signal::ContinuityJump);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (0, 2),
            "{r:?}"
        );
    }

    /// The demuxer keeps a PCR timeline only for a PID some PMT declares
    /// as `PCR_PID`, so it can never honestly report a `PcrAnomaly` on
    /// `REWRITTEN_PID`. A PID rewrite explains a packet APPEARING there —
    /// a non-conformance of that one packet, say — not a timeline being
    /// kept for it, so it must not be able to claim the event in either
    /// tier.
    #[test]
    fn a_pcr_anomaly_on_a_pid_no_pmt_declares_is_never_attributed() {
        let mut moved = inj(1000, 10, Class::Header, 0x1011, true);
        moved.offsets = vec![1, 2];
        for lossy in [false, true] {
            let mut a = with_video_pcr(lossy, vec![moved.clone()]);
            a.on_pcr(1000, 5000); // resolves at 5010
            a.on_signal(5010, Some(REWRITTEN_PID), PCR_FORWARD);
            a.on_media(5100, 0x1011);
            let r = a.finish(10_000);
            assert_eq!(
                (r.attributed_events, r.unexplained_nonconformant),
                (0, 1),
                "lossy={lossy}: a PCR anomaly on a PID no PMT declares is a library finding: {r:?}"
            );

            // Nor excused: a gap on that PID does not make it loss.
            let mut a = with_video_pcr(lossy, vec![]);
            a.on_signal(5000, Some(REWRITTEN_PID), Signal::ContinuityJump);
            a.on_signal(5010, Some(REWRITTEN_PID), PCR_FORWARD);
            let r = a.finish(10_000);
            assert_eq!(
                (r.pcr_anomalies_excused, r.unexplained_nonconformant),
                (0, 1),
                "lossy={lossy}: {r:?}"
            );

            // The same injection still explains an anomaly on the PID it
            // left, which IS declared, and a non-conformance on the PID it
            // moved the packet to, which needs no timeline.
            let mut a = with_video_pcr(lossy, vec![moved.clone()]);
            a.on_pcr(1000, 5000);
            a.on_signal(5010, Some(REWRITTEN_PID), Signal::OtherNonConformant);
            a.on_signal(5012, Some(0x1011), PCR_FORWARD);
            let r = a.finish(10_000);
            assert_eq!(
                (r.attributed_events, r.unexplained_nonconformant),
                (2, 0),
                "lossy={lossy}: {r:?}"
            );
        }

        // Before any PMT is reported nothing is declared at all.
        let mut a = Attribution::lossy(vec![], &hdr());
        a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
        a.on_signal(5010, Some(0x1011), PCR_FORWARD);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (0, 1),
            "{r:?}"
        );
    }

    /// Everything the sender logged between the last media the receiver
    /// saw and a reconnect marker never arrived — its PCR anchor included,
    /// so it resolves approximately, against the first base the rebuilt
    /// connection delivers. An injection placed inside that gap this way
    /// is lost in transit, not undetected — for the PAT/PMT shapes that no continuity jump can
    /// ever excuse (the demuxer reports none on a PSI PID), and for the
    /// media shapes a 90 s outage strands far outside a 500-packet window.
    /// Run 3 (2026-09-17): all 12 `undetected` on the srt leg sat at the
    /// 6-hour outage boundaries.
    #[test]
    fn an_injection_resolved_inside_a_reconnect_gap_is_lost_in_transit() {
        // A PAT flip anchored at PCR base 1000, +10 packets.
        let mut pat_flip = inj(1000, 10, Class::PsiFlip, 0, true);
        pat_flip.psi = true;
        let run = |lossy: bool| {
            let mut a = if lossy {
                Attribution::lossy(vec![pat_flip.clone()], &hdr())
            } else {
                Attribution::strict(vec![pat_flip.clone()], &hdr())
            };
            a.on_media(4000, 0x1011); // last media before the link died
            a.on_reconnect(4900); // the marker
            // Base 1000 went down with the link. The first base the
            // rebuilt connection delivers is a later one, so the injection
            // resolves approximately, at 5010.
            a.on_pcr(1500, 5000);
            a.on_media(5100, 0x1011);
            a.finish(10_000)
        };
        let r = run(true);
        assert_eq!(
            (
                r.reconnects_seen,
                r.lost_in_reconnect_gap,
                r.undetected_lost
            ),
            (1, 1, 1),
            "{r:?}"
        );
        assert_eq!(r.undetected_count, 0, "{r:?}");
        let r = run(false);
        assert_eq!(
            (
                r.reconnects_seen,
                r.lost_in_reconnect_gap,
                r.undetected_count
            ),
            (1, 0, 1),
            "{r:?}"
        );

        // Two markers with no media between them (a rebuild that broke
        // again): both gaps open at the same last media, and an injection
        // between them is still lost.
        let mut a = Attribution::lossy(vec![pat_flip.clone()], &hdr());
        a.on_media(4000, 0x1011);
        a.on_reconnect(4500);
        a.on_reconnect(4900);
        a.on_pcr(1500, 5000);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            (r.reconnects_seen, r.lost_in_reconnect_gap),
            (2, 1),
            "{r:?}"
        );

        // The anchor arrived, and the link died between it and the
        // injection: resolved exactly, but from BEFORE the marker, so
        // nothing says the injection itself came through.
        let mut a = Attribution::lossy(vec![pat_flip.clone()], &hdr());
        a.on_pcr(1000, 3995); // resolves at 4005
        a.on_media(4000, 0x1011);
        a.on_reconnect(4900);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            (r.lost_in_reconnect_gap, r.undetected_count),
            (1, 0),
            "{r:?}"
        );

        // Resolved AFTER the gap closes (marker + window + slack): judged
        // on its own merits — here, undetected.
        let mut a = Attribution::lossy(vec![pat_flip.clone()], &hdr());
        a.on_media(4000, 0x1011);
        a.on_reconnect(4000);
        a.on_pcr(1000, 4000 + ATTRIBUTION_WINDOW + 200);
        a.on_media(6000, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            (r.lost_in_reconnect_gap, r.undetected_count),
            (0, 1),
            "{r:?}"
        );
    }

    /// An unexplained event's sample names the nearest resolved injection,
    /// so a report can be read offline for "what was nearby" — the
    /// question run 3's six PMT checksum failures could not answer.
    #[test]
    fn an_unexplained_sample_names_the_nearest_injection() {
        let mut a = Attribution::strict(vec![inj(1000, 10, Class::Dup, 0x1011, false)], &hdr());
        a.on_pcr(1000, 5000); // resolves at 5010
        a.on_signal(7000, Some(0x1000), Signal::PsiChecksum);
        let r = a.finish(10_000);
        assert_eq!(
            r.unexplained_events,
            vec![
                "PsiChecksum on pid Some(4096) at packet 7000 (nearest injection: dup on pid \
                 0x1011 at packet 5010, 1990 packets before)"
                    .to_string()
            ],
        );
    }

    /// The excusal is evidence-driven, not blanket: with NO continuity
    /// jump in its window, an undetected injection still fails in Lossy.
    /// Without this the whole corruption suite would go vacuous.
    #[test]
    fn lossy_without_a_cc_jump_in_the_window_still_reports_undetected() {
        let mut a = Attribution::lossy(vec![inj(1000, 10, Class::PsiFlip, 0, true)], &hdr());
        a.on_pcr(1000, 5000);
        // A jump far outside the window explains nothing about it.
        a.on_signal(9000, Some(0x1011), Signal::ContinuityJump);
        let r = a.finish(10_000);
        assert_eq!(r.undetected.len(), 1, "{r:?}");
        assert_eq!(r.undetected_lost, 0);
    }

    /// An injection's OWN continuity jump is its own damage surfacing, not
    /// evidence that the network ate it, so it must not excuse that
    /// injection's recovery obligation. `Drop` is the class this protects:
    /// its only observable IS a continuity jump, so a self-excusing rule
    /// would let every drop arrive pre-excused and never have to show the
    /// stream recovering.
    #[test]
    fn an_injections_own_cc_jump_does_not_excuse_its_recovery() {
        let mut a = Attribution::lossy(vec![inj(1000, 10, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000); // resolves to 5010
        // Its own jump, inside its own window, attributed to it.
        a.on_signal(5100, Some(0x1011), Signal::ContinuityJump);
        // No media on 0x1011 afterwards, so it never recovers.
        let r = a.finish(10_000);
        assert_eq!(r.attributed_events, 1, "the jump IS attributed: {r:?}");
        assert_eq!(
            r.unrecovered.len(),
            1,
            "a drop keeps its full recovery obligation: {r:?}"
        );
        assert_eq!(r.unrecovered_lost, 0, "{r:?}");
    }

    /// The other half of the same rule: a jump from a DIFFERENT injection
    /// still excuses. `explainer` attributes a jump to the newest covering
    /// injection that can cause it, so an older one whose window also
    /// covers it is excused — which needs overlapping windows, the case a
    /// real tap config forbids (`min_gap >= 2 * ATTRIBUTION_WINDOW`) but
    /// an `approx` resolution's widened window can still produce.
    ///
    /// Both injections sit on the PSI PID because the excusal is
    /// PID-constrained too: a gap on a PID an injection never touched says
    /// nothing about whether ITS packet went missing, so
    /// only a jump the older injection could itself have suffered counts.
    #[test]
    fn a_foreign_cc_jump_in_the_window_still_excuses() {
        // Two injections 100 packets apart: deliberately closer than
        // `min_gap` so their windows overlap and the newer one can own a
        // jump that also falls inside the older one's window.
        let build = |lossy: bool| {
            let log = vec![
                inj(1000, 10, Class::PsiFlip, 0, true),
                inj(1000, 110, Class::Drop, 0, true),
            ];
            let mut a = if lossy {
                Attribution::lossy(log, &hdr())
            } else {
                Attribution::strict(log, &hdr())
            };
            a.on_pcr(1000, 5000); // resolve to 5010 and 5110
            // Inside BOTH windows and on a PID both injections damaged;
            // `explainer` gives it to the newer (the Drop, the only one of
            // the two whose class expects a jump), so the PsiFlip sees it
            // as foreign.
            a.on_signal(5200, Some(0), Signal::ContinuityJump);
            a
        };

        // The psi_flip is detectable and expects a PsiChecksum event it
        // never got, so it is BOTH undetected and unrecovered — one
        // fixture covering both excused counters.
        let lossy = build(true).finish(10_000);
        assert_eq!(
            (lossy.undetected_lost, lossy.unrecovered_lost),
            (1, 1),
            "the psi_flip is excused by the drop's foreign jump: {lossy:?}"
        );
        assert!(
            lossy.undetected.is_empty(),
            "nothing else was undetected: {lossy:?}"
        );
        assert_eq!(
            lossy.unrecovered.len(),
            1,
            "and the drop itself is NOT excused by its own jump: {lossy:?}"
        );
        assert!(
            lossy.unrecovered[0].contains("drop"),
            "the surviving one must be the drop: {lossy:?}"
        );

        // Strict excuses neither, as always.
        let strict = build(false).finish(10_000);
        assert_eq!(strict.undetected.len(), 1, "{strict:?}");
        assert_eq!(strict.unrecovered.len(), 2, "{strict:?}");
        assert_eq!((strict.undetected_lost, strict.unrecovered_lost), (0, 0));
    }

    /// `explains_damage` answers for a content oracle, records nothing,
    /// and covers exactly two cases: same PID any class, or Truncate/
    /// Garbage on any PID (both destroy framing multiplex-wide).
    #[test]
    fn explains_damage_covers_same_pid_any_class_and_framing_classes_anywhere() {
        let mut a = Attribution::strict(
            vec![
                inj(1000, 10, Class::BodyFlip, 0x1031, false),
                inj(2000, 10, Class::Truncate, 0x1011, true),
            ],
            &hdr(),
        );
        a.on_pcr(1000, 5000); // body flip on 0x1031 resolves to 5010
        a.on_pcr(2000, 7000); // truncate on 0x1011 resolves to 7010

        // Same PID, any class.
        assert!(a.explains_damage(5010, 0x1031));
        assert!(a.explains_damage(5010 + ATTRIBUTION_WINDOW, 0x1031));
        // A body flip on another PID explains nothing there.
        assert!(!a.explains_damage(5010, 0x1011));
        // Truncation breaks framing for the whole multiplex.
        assert!(a.explains_damage(7010, 0x1031));
        assert!(a.explains_damage(7010, 0x1011));
        // Past the window, nothing is explained.
        assert!(!a.explains_damage(7011 + ATTRIBUTION_WINDOW, 0x1031));

        // It recorded nothing: no events, no detection, no recovery.
        let r = a.finish(10_000);
        assert_eq!(r.events, 0);
        assert_eq!(r.attributed_events, 0);
    }

    #[test]
    fn attribution_flags_undetected_and_unrecovered_injections() {
        let mut a = Attribution::strict(
            vec![
                inj(1000, 10, Class::PsiFlip, 0, true),
                inj(2000, 0, Class::Dup, 0x1011, false),
            ],
            &hdr(),
        );
        a.on_pcr(1000, 5000);
        a.on_pcr(2000, 7000);
        a.on_media(7100, 0x1011); // dup recovered (no detection required)
        let r = a.finish(10_000);
        assert_eq!(r.undetected.len(), 1, "psi_flip had no PsiChecksum event");
        assert_eq!(
            r.unrecovered.len(),
            1,
            "psi_flip (pid 0) never saw media after it"
        );
    }

    #[test]
    fn attribution_resolves_an_unseen_pcr_base_to_the_next_seen_one_wrap_aware() {
        let near_wrap = (1u64 << 33) - 10;
        let mut a = Attribution::strict(
            vec![inj(near_wrap, 3, Class::Truncate, 0x1011, true)],
            &hdr(),
        );
        a.on_pcr(5, 8000); // wrapped past 2^33; first base "at or after" near_wrap
        a.on_signal(8010, None, Signal::Resync);
        a.on_media(8020, 0x1011);
        let r = a.finish(9000);
        assert_eq!(r.resolved, 1);
        assert_eq!(r.resyncs, 1);
        assert!(
            r.unexplained_events.is_empty() && r.undetected.is_empty() && r.unrecovered.is_empty()
        );
    }

    #[test]
    fn approximate_resolution_is_bounded_so_an_outage_strands_an_injection() {
        // Just inside the bound: the logged anchor base never arrived (the
        // corruption destroyed the packet carrying it), but the next base
        // is close enough to place the injection approximately.
        let mut a = Attribution::strict(vec![inj(2000, 0, Class::Header, 0x1011, true)], &hdr());
        a.on_pcr(2000 + MAX_APPROX_TICKS, 5000);
        a.on_signal(5010, Some(0x1011), Signal::ContinuityJump);
        a.on_media(5020, 0x1011);
        let r = a.finish(10_000);
        assert_eq!((r.resolved, r.unresolved), (1, 0));
        assert!(r.undetected.is_empty() && r.unrecovered.is_empty());

        // One tick further — a reconnect outage, through which the sender
        // kept logging and the receiver saw nothing. Resolving here would
        // pile every injection logged during the outage onto this one
        // ordinal and report all but one as undetected, so the injection
        // stays unresolved and is judged for nothing.
        let mut a = Attribution::strict(vec![inj(2000, 0, Class::Header, 0x1011, true)], &hdr());
        a.on_pcr(2000 + MAX_APPROX_TICKS + 1, 5000);
        let r = a.finish(10_000);
        assert_eq!((r.resolved, r.unresolved), (0, 1));
        assert!(r.undetected.is_empty() && r.unrecovered.is_empty());
    }

    #[test]
    fn a_stranded_injection_does_not_block_the_ones_after_it() {
        // The cursor must step over an injection it can never place, or
        // everything logged after the outage becomes unattributable too.
        let mut a = Attribution::strict(
            vec![
                inj(2000, 0, Class::Header, 0x1011, true),
                inj(2000 + 2 * MAX_APPROX_TICKS, 0, Class::Header, 0x1011, true),
            ],
            &hdr(),
        );
        a.on_pcr(2000 + 2 * MAX_APPROX_TICKS, 5000); // strands #0, resolves #1
        a.on_signal(5010, Some(0x1011), Signal::ContinuityJump);
        let r = a.finish(10_000);
        assert_eq!((r.resolved, r.unresolved), (1, 1));
        assert_eq!(
            r.attributed_events, 1,
            "the surviving injection is reachable"
        );
        assert!(r.undetected.is_empty());
    }

    /// A "PCR" decoded from a mis-framed packet (a framing injection's
    /// aftermath) is a random 33-bit number. Fed as an anchor it stranded
    /// every injection pending behind it. One implausible sample is held,
    /// not accepted; the real anchor that follows resolves the injection.
    #[test]
    fn a_wild_pcr_sample_between_an_injection_and_its_anchor_is_not_an_anchor() {
        let mut a = Attribution::lossy(
            vec![inj(5_000_000, 3, Class::Garbage, 0x1011, true)],
            &hdr(),
        );
        a.on_pcr(4_900_000, 100); // 1.1 s before the injection's base: accepted
        a.on_pcr(3_000_000_000, 150); // 9 h away in one step: held as a candidate
        a.on_pcr(5_000_000, 200); // the injection's own anchor
        a.on_signal(204, Some(0x1011), Signal::ContinuityJump);
        a.on_media(220, 0x1011);
        let r = a.finish(2_000);
        assert_eq!((r.resolved, r.unresolved), (1, 0));
        assert_eq!(r.attributed_events, 1);
        assert_eq!(r.anchors_rejected, 1);
    }

    /// A genuine clock discontinuity (the source restarted) is confirmed by
    /// the next sample agreeing with the candidate: both become anchors.
    #[test]
    fn a_clock_jump_confirmed_by_the_next_sample_re_anchors() {
        let mut a = Attribution::strict(
            vec![inj(2_001_000_000, 2, Class::Drop, 0x1011, true)],
            &hdr(),
        );
        a.on_pcr(1_000_000, 10);
        a.on_pcr(2_001_000_000, 50); // ≈ 6.2 h jump: held
        a.on_pcr(2_001_009_000, 60); // 100 ms later on the new clock: confirms
        a.on_signal(53, Some(0x1011), Signal::ContinuityJump);
        a.on_media(70, 0x1011);
        let r = a.finish(2_000);
        assert_eq!((r.resolved, r.unresolved), (1, 0));
        assert_eq!(r.attributed_events, 1);
        assert_eq!(
            r.anchors_rejected, 0,
            "a confirmed candidate was not rejected"
        );
    }

    /// Two implausible samples that also disagree with each other (two
    /// mis-framed packets in a row) anchor nothing; the plausible third one
    /// resolves normally.
    #[test]
    fn two_disagreeing_wild_samples_anchor_nothing() {
        let mut a = Attribution::lossy(vec![inj(5_000_000, 3, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(4_900_000, 100);
        a.on_pcr(3_000_000_000, 150);
        a.on_pcr(7_000_000_000, 160);
        a.on_pcr(5_000_000, 200);
        a.on_signal(204, Some(0x1011), Signal::ContinuityJump);
        a.on_media(220, 0x1011);
        let r = a.finish(2_000);
        assert_eq!((r.resolved, r.unresolved), (1, 0));
        assert_eq!(r.attributed_events, 1);
        assert_eq!(r.anchors_rejected, 2);
    }

    /// A receiver that ingests injections logged long before it listened
    /// (history from before a restart, or a unit test feeding one) must not
    /// let the oldest block the live ones: a 14-hour-old anchor is past the
    /// 33-bit PCR half-range, so the naive "ahead ⇒ still in the future"
    /// test read it as future and stopped the cursor in front of it.
    #[test]
    fn old_restart_history_must_not_block_live_injections() {
        let current = 14 * 3600 * 90_000u64;
        let mut a = Attribution::lossy(
            vec![
                inj(1_000, 2, Class::Drop, 0x1011, true),
                inj(current, 3, Class::Drop, 0x1011, true),
            ],
            &hdr(),
        );
        a.on_pcr(current, 10);
        a.on_signal(14, Some(0x1011), Signal::ContinuityJump);
        a.on_media(20, 0x1011);
        let report = a.finish(2_000);
        assert_eq!(report.resolved, 1, "current injection must be placed");
        assert_eq!(
            report.unresolved, 1,
            "the ancient one is stranded, not waited for"
        );
        assert_eq!(report.attributed_events, 1);
    }

    #[test]
    fn expected_signal_sets_cover_the_non_conformance_shapes() {
        // An adaptation-field-length overrun (a `header` sub-kind) is
        // reported as a plain non-conformance, not a resync, so it must
        // still count as having noticed the injection.
        let mut a = Attribution::strict(vec![inj(1000, 0, Class::Header, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5010, Some(0x1011), Signal::OtherNonConformant);
        a.on_media(5020, 0x1011);
        let r = a.finish(10_000);
        assert!(r.undetected.is_empty(), "{:?}", r.undetected);
        assert_eq!(r.attributed_nonconformant, 1);

        // A flipped pointer field or section length surfaces as a
        // table-id/section-length non-conformance before the CRC is
        // reached.
        let mut a = Attribution::strict(vec![inj(1000, 0, Class::PsiFlip, 0, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5010, Some(0), Signal::OtherNonConformant);
        a.on_media(5020, 0);
        let r = a.finish(10_000);
        assert!(r.undetected.is_empty(), "{:?}", r.undetected);

        // But the sets are not wide open: a dropped packet is a continuity
        // jump and nothing else, so a resync must not be mistaken for
        // having noticed it.
        let mut a = Attribution::strict(vec![inj(1000, 0, Class::Drop, 0x1011, true)], &hdr());
        a.on_pcr(1000, 5000);
        a.on_signal(5010, None, Signal::Resync);
        a.on_media(5020, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            r.undetected.len(),
            1,
            "a resync does not prove a drop was seen"
        );
    }

    #[test]
    fn attribution_reports_an_injection_whose_window_never_arrived_as_unresolved() {
        let a = Attribution::strict(vec![inj(1000, 0, Class::Header, 0x1011, true)], &hdr());
        let r = a.finish(100);
        assert_eq!(r.unresolved, 1);
        assert!(
            r.undetected.is_empty(),
            "an unresolved injection is not judged"
        );
    }

    /// The reconnect excusal is for injections that never arrived. This
    /// one did: its own anchor base arrives, exactly, AFTER the marker,
    /// and media flows on both sides of its resolved position with no
    /// continuity jump anywhere. A demuxer that said nothing about the
    /// damaged PAT has missed it, inside the gap's window or not.
    #[test]
    fn an_injection_resolved_exactly_after_a_reconnect_marker_is_not_lost() {
        let mut pat_flip = inj(1000, 10, Class::PsiFlip, 0, true);
        pat_flip.psi = true;
        // 4900 is the first packet after the marker: a marker is stamped
        // with the count of packets received, a PCR with its 0-based
        // ordinal.
        for anchor_at in [5000, 4900] {
            let mut a = Attribution::lossy(vec![pat_flip.clone()], &hdr());
            a.on_media(4000, 0x1011); // last media before the link died
            a.on_reconnect(4900); // the marker
            a.on_pcr(1000, anchor_at); // the injection's OWN anchor, exact
            a.on_media(anchor_at + 5, 0x1011); // media just before the injected packet...
            a.on_media(anchor_at + 15, 0x1011); // ...and just after it
            a.on_media(anchor_at + 100, 0x1011);
            let r = a.finish(10_000);
            assert_eq!(
                (
                    r.undetected_count,
                    r.lost_in_reconnect_gap,
                    r.undetected_lost
                ),
                (1, 0, 0),
                "anchor at {anchor_at}: an injection anchored exactly on a post-reconnect PCR \
                 arrived and must be detected: {r:?}"
            );
        }
    }

    /// A reconnect resets the demuxer, and until it has re-acquired what
    /// a class is noticed by, an injection that arrived still could not
    /// have been noticed. Each shape twice: placed before the media that
    /// shows the demuxer able (lost in the gap), and after it (held).
    #[test]
    fn a_post_reconnect_injection_is_held_only_once_the_demuxer_could_notice_it() {
        const VIDEO: u16 = 0x1011;
        const KLV: u16 = 0x1100;
        // Marker 4900, exact anchor 5000, injection at 5010, on `inj`'s
        // PID; `media_before` is fed between the anchor and the injection.
        let run = |inj: &Injection, media_before: &[u16]| {
            let mut a = Attribution::lossy(vec![inj.clone()], &hdr());
            a.on_media(4000, VIDEO); // before the marker: says nothing about after it
            a.on_media(4000, KLV);
            a.on_reconnect(4900);
            a.on_pcr(1000, 5000);
            for &pid in media_before {
                a.on_media(5005, pid);
            }
            a.on_media(5015, VIDEO); // after the injection: too late to show anything
            a.on_media(5015, KLV);
            let r = a.finish(10_000);
            (r.undetected_count, r.lost_in_reconnect_gap)
        };
        const HELD: (u64, u64) = (1, 0);
        const LOST: (u64, u64) = (0, 1);

        // Noticed as a continuity jump on its own PID: needs a previous
        // counter THERE. Media on the other PID is not that.
        let drop = inj(1000, 10, Class::Drop, KLV, true);
        let mut pid_rewrite = inj(1000, 10, Class::Header, KLV, true);
        pid_rewrite.offsets = vec![1, 2];
        let mut cc_rewrite = inj(1000, 10, Class::Header, KLV, true);
        cc_rewrite.offsets = vec![3];
        for (name, i) in [
            ("drop", &drop),
            ("pid rewrite", &pid_rewrite),
            ("cc rewrite", &cc_rewrite),
        ] {
            assert_eq!(run(i, &[]), LOST, "{name}, no media");
            assert_eq!(run(i, &[VIDEO]), LOST, "{name}, media on another PID");
            assert_eq!(run(i, &[KLV]), HELD, "{name}, media on its own PID");
        }

        // Noticed without per-PID history: media on any PID shows the
        // PAT and a PMT read and the packets in sync.
        let mut pat_flip = inj(1000, 10, Class::PsiFlip, 0, true);
        pat_flip.psi = true;
        let mut pmt_body_flip = inj(1000, 10, Class::BodyFlip, 0x1000, true);
        pmt_body_flip.psi = true;
        let mut af_overrun = inj(1000, 10, Class::Header, KLV, true);
        af_overrun.offsets = vec![4];
        let mut sync_byte = inj(1000, 10, Class::Header, KLV, true);
        sync_byte.offsets = vec![0];
        let truncate = inj(1000, 10, Class::Truncate, KLV, true);
        let garbage = inj(1000, 10, Class::Garbage, KLV, true);
        for (name, i) in [
            ("pat flip", &pat_flip),
            ("pmt body flip", &pmt_body_flip),
            ("adaptation-field overrun", &af_overrun),
            ("sync byte", &sync_byte),
            ("truncate", &truncate),
            ("garbage", &garbage),
        ] {
            assert_eq!(run(i, &[]), LOST, "{name}, no media");
            assert_eq!(run(i, &[VIDEO]), HELD, "{name}, media on any PID");
        }

        // A second reconnect resets the demuxer again: media after the
        // first marker shows nothing about the state after the second.
        let mut a = Attribution::lossy(vec![drop.clone()], &hdr());
        a.on_reconnect(4900);
        a.on_media(4950, KLV);
        a.on_reconnect(4990);
        a.on_pcr(1000, 5000);
        a.on_media(5015, KLV);
        let r = a.finish(10_000);
        assert_eq!((r.undetected_count, r.lost_in_reconnect_gap), LOST, "{r:?}");
    }

    /// A gap belongs to one reconnect epoch. Two reconnects in quick
    /// succession leave the older gap's window reaching past the newer
    /// marker, and the older gap saw no media of its own — so if it still
    /// had a say there, a second reconnect would switch detection off for
    /// everything the newer epoch demonstrably delivered.
    #[test]
    fn an_older_reconnect_gap_does_not_excuse_what_the_newest_epoch_holds() {
        let mut pat_flip = inj(1000, 10, Class::PsiFlip, 0, true);
        pat_flip.psi = true;
        let mut a = Attribution::lossy(vec![pat_flip.clone()], &hdr());
        a.on_media(4000, 0x1011);
        a.on_reconnect(4900);
        a.on_reconnect(4950);
        a.on_pcr(1000, 5000); // exact, after the newest marker: resolves at 5010
        a.on_media(5005, 0x1011); // the reset demuxer is parsing again
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            (r.undetected_count, r.lost_in_reconnect_gap),
            (1, 0),
            "arrived in the newest epoch, after its media, and nothing was reported: {r:?}"
        );

        // The twin that IS lost: placed between the two markers, in an
        // epoch that ended before the demuxer produced any media.
        let mut a = Attribution::lossy(vec![pat_flip], &hdr());
        a.on_media(4000, 0x1011);
        a.on_reconnect(4900);
        a.on_pcr(1000, 4905); // exact, after the first marker: resolves at 4915
        a.on_reconnect(4950);
        a.on_media(5005, 0x1011);
        a.on_media(5100, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            (r.undetected_count, r.lost_in_reconnect_gap),
            (0, 1),
            "{r:?}"
        );
    }

    /// The declared `PCR_PID`s are the demuxer's CURRENT topology. A
    /// reconnect resets the demuxer, so a PID declared before it is
    /// declared no longer, and a `PcrAnomaly` there means a timeline
    /// outlived its declaration: never excused, never attributed.
    #[test]
    fn a_pcr_pid_declared_before_a_reconnect_is_not_declared_after_it() {
        let mut on_video = inj(1000, 10, Class::Header, 0x1011, true);
        on_video.offsets = vec![4];
        for lossy in [false, true] {
            // Program 1 before the reconnect, only program 2 after it.
            let run = |log: Vec<Injection>| {
                let mut a = with_video_pcr(lossy, log);
                a.on_reconnect(4000);
                a.on_program_map(2, 0x1001, 0x1100);
                a
            };

            // Beside a gap nothing else claims.
            let mut a = run(vec![]);
            a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
            a.on_signal(5010, Some(0x1011), PCR_FORWARD);
            let r = a.finish(10_000);
            assert_eq!(
                (r.pcr_anomalies_excused, r.unexplained_nonconformant),
                (0, 1),
                "lossy={lossy}: {r:?}"
            );

            // Inside the window of an injection that could otherwise
            // explain it.
            let mut a = run(vec![on_video.clone()]);
            a.on_pcr(1000, 5000); // resolves at 5010
            a.on_media(5005, 0x1100);
            a.on_signal(5012, Some(0x1011), PCR_FORWARD);
            let r = a.finish(10_000);
            assert_eq!(
                (r.attributed_events, r.unexplained_nonconformant),
                (0, 1),
                "lossy={lossy}: {r:?}"
            );
        }

        // The healthy twin: the anomaly is on the PID the new topology
        // declares, beside its own gap.
        let mut a = with_video_pcr(true, vec![]);
        a.on_reconnect(4000);
        a.on_program_map(2, 0x1001, 0x1100);
        a.on_signal(5000, Some(0x1100), Signal::ContinuityJump);
        a.on_signal(5010, Some(0x1100), PCR_FORWARD);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (1, 0),
            "{r:?}"
        );

        // After a reconnect nothing is declared until a program map says
        // so — the same fail-closed rule as at the start of a capture.
        let mut a = with_video_pcr(true, vec![]);
        a.on_reconnect(4000);
        a.on_signal(5000, Some(0x1011), Signal::ContinuityJump);
        a.on_signal(5010, Some(0x1011), PCR_FORWARD);
        a.on_program_map(1, 0x1000, 0x1011);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (0, 1),
            "{r:?}"
        );
    }

    /// Without a reconnect the declarations follow the program maps: a
    /// later map for the same program replaces its `PCR_PID`, and a map
    /// for a different program on the same PMT PID replaces the program
    /// (the demuxer keeps one program per PMT PID).
    #[test]
    fn a_program_map_replaces_the_declaration_it_supersedes() {
        let excused_on = |a: &mut Attribution, at: u64, pid: u16| {
            a.on_signal(at, Some(pid), Signal::ContinuityJump);
            a.on_signal(at + 10, Some(pid), PCR_FORWARD);
        };

        // Program 1 moves its time base from 0x1011 to 0x1100.
        let mut a = with_video_pcr(true, vec![]);
        a.on_program_map(1, 0x1000, 0x1100);
        excused_on(&mut a, 5000, 0x1011);
        excused_on(&mut a, 6000, 0x1100);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (1, 1),
            "{r:?}"
        );

        // Program 2 takes over program 1's PMT PID.
        let mut a = with_video_pcr(true, vec![]);
        a.on_program_map(2, 0x1000, 0x1100);
        excused_on(&mut a, 5000, 0x1011);
        excused_on(&mut a, 6000, 0x1100);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (1, 1),
            "{r:?}"
        );

        // A second program on its own PMT PID removes nothing.
        let mut a = with_video_pcr(true, vec![]);
        a.on_program_map(2, 0x1001, 0x1100);
        excused_on(&mut a, 5000, 0x1011);
        excused_on(&mut a, 6000, 0x1100);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (2, 0),
            "{r:?}"
        );
    }

    /// A continuity gap is evidence about the connection it was seen on.
    /// After a reconnect the demuxer holds no PCR baseline from before it,
    /// so a jump it reports is between two PCRs of the NEW connection and
    /// a gap from the old one is not its cause.
    #[test]
    fn a_continuity_gap_from_before_a_reconnect_excuses_no_pcr_anomaly_after_it() {
        let mut a = with_video_pcr(true, vec![]);
        a.on_signal(3990, Some(0x1011), Signal::ContinuityJump);
        a.on_reconnect(4000);
        a.on_program_map(1, 0x1000, 0x1011);
        a.on_signal(4010, Some(0x1011), PCR_FORWARD);
        let r = a.finish(10_000);
        assert_eq!(
            (r.pcr_anomalies_excused, r.unexplained_nonconformant),
            (0, 1),
            "{r:?}"
        );
    }
}
