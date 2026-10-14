//! Property-based tests for `mpegts::mux::Muxer` invariants.

use crate::common::mux_demux::drain_mux as drain_all;
use proptest::prelude::*;
use tst_core::mpegts::common::Pts90khz;
use tst_core::mpegts::mux::{Muxer, MuxerConfig};
use tst_test_helpers::synthetic_nal;
use tst_test_helpers::ts_parser;

#[derive(Debug, Clone)]
struct PushSpec {
    is_video: bool,
    body_size: usize,
    pts: i64,
    key_frame: bool,
}

fn push_spec_strategy() -> impl Strategy<Value = PushSpec> {
    (
        any::<bool>(),
        16usize..2_000,
        0i64..10_000_000,
        any::<bool>(),
    )
        .prop_map(|(is_video, body_size, pts, key_frame)| PushSpec {
            is_video,
            body_size,
            pts,
            key_frame,
        })
}

proptest! {
    /// All packets emitted by the muxer must be 188 bytes long with sync
    /// byte 0x47 — a basic structural invariant.
    #[test]
    fn all_packets_are_well_formed(specs in proptest::collection::vec(push_spec_strategy(), 1..16)) {
        let mut cfg = MuxerConfig::default();
        cfg.buffer_packets = 50_000;
        let mut mux = Muxer::new(cfg).unwrap();
        let mut last_pts = 0i64;
        let mut sorted = specs;
        sorted.sort_by_key(|s| s.pts);
        for s in &sorted {
            let pts = (s.pts).max(last_pts);
            last_pts = pts + 3000; // 33ms forward
            if s.is_video {
                let nal = synthetic_nal::h264_au(s.body_size, s.key_frame);
                mux.push_video(&nal, Pts90khz::new(pts), s.key_frame).unwrap();
            } else {
                let klv = synthetic_nal::klv_blob(s.body_size);
                mux.push_klv(&klv, Pts90khz::new(pts), 0x00).unwrap();
            }
        }
        let bytes = drain_all(&mut mux);
        prop_assert_eq!(bytes.len() % 188, 0);
        for pkt in bytes.chunks_exact(188) {
            prop_assert_eq!(pkt[0], 0x47);
        }
    }

    /// Continuity counter on a single PID must increment monotonically (mod 16)
    /// across all payload-bearing packets.
    #[test]
    fn continuity_counter_monotonic_per_pid(spec_count in 5usize..30) {
        let mut cfg = MuxerConfig::default();
        cfg.buffer_packets = 50_000;
        let mut mux = Muxer::new(cfg).unwrap();
        for i in 0..spec_count {
            let nal = synthetic_nal::h264_au(500, i == 0);
            mux.push_video(&nal, Pts90khz::new((i as i64) * 3000), i == 0).unwrap();
        }
        let bytes = drain_all(&mut mux);

        // Track per-PID CC progression.
        use std::collections::HashMap;
        let mut last: HashMap<u16, u8> = HashMap::new();
        for pkt in bytes.chunks_exact(188) {
            let pid = (((pkt[1] as u16) & 0x1F) << 8) | (pkt[2] as u16);
            let afc = (pkt[3] >> 4) & 0x3;
            // Only payload-bearing packets advance CC.
            if afc & 0x1 == 0 {
                continue;
            }
            let cc = pkt[3] & 0x0F;
            if let Some(prev) = last.get(&pid) {
                let expected = (prev + 1) & 0x0F;
                prop_assert_eq!(cc, expected, "PID {:04x}", pid);
            }
            last.insert(pid, cc);
        }
    }

    /// Round-trip preservation: KLV blobs pushed at distinct PTS values
    /// must be recovered in the same order with the same bytes.
    #[test]
    fn klv_roundtrip_preserves_bytes(blobs in proptest::collection::vec(16usize..500, 1..8)) {
        let mut cfg = MuxerConfig::default();
        cfg.buffer_packets = 50_000;
        let mut mux = Muxer::new(cfg).unwrap();
        // Need at least one video frame so PSI emits and the KLV PID
        // becomes known to the parser.
        let nal = synthetic_nal::h264_au(500, true);
        mux.push_video(&nal, Pts90khz::new(0), true).unwrap();

        let mut originals: Vec<Vec<u8>> = Vec::new();
        for (i, sz) in blobs.iter().enumerate() {
            let blob = synthetic_nal::klv_blob(*sz);
            originals.push(blob.clone());
            mux.push_klv(&blob, Pts90khz::new((i as i64 + 1) * 3000), 0x00).unwrap();
        }
        let bytes = drain_all(&mut mux);
        let parsed = ts_parser::parse(&bytes);
        let klv_pid = parsed.streams.iter().find(|s| s.klva).map(|s| s.pid)
            .expect("KLVA stream present");
        let recovered = parsed.pes_by_pid.get(&klv_pid).unwrap();
        prop_assert_eq!(recovered.len(), originals.len());
        for (i, (_pts, body)) in recovered.iter().enumerate() {
            prop_assert_eq!(body.as_slice(), originals[i].as_slice());
        }
    }
}
