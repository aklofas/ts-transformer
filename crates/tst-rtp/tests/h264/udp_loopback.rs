//! UDP loopback integration tests for `H264Receiver`.
//!
//! (a) Multi-AU roundtrip: `packetize` → real UDP socket pair → `H264Receiver`
//!     → assert Annex B identity, PTS deltas, and `key_frame` flags.
//!
//! (b) Randomized-loss soak: 200 AUs, each packet independently dropped with
//!     p=0.2, fixed LCG seed.  Asserts no panic, correct AU accounting, and
//!     byte-identity of every emitted AU.

use std::net::UdpSocket;

use tst_rtp::{H264DepayConfig, H264Receiver, ParameterSetInjection};

use crate::common::{Lcg, expected_annexb, packetize};

/// Twenty AUs, mix of single-NALU (fits MTU) and FU-A (exceeds MTU).
/// The receiver uses `ParameterSetInjection::None` so the Annex B output is
/// byte-identical to what the payloader produces.
///
/// Each AU uses a unique RTP timestamp; PTS delta between consecutive AUs
/// must match the timestamp delta.  Key-frame flag is set for NALU type 5.
#[test]
fn multi_au_roundtrip_byte_identical() {
    const PT: u8 = 96;
    const SSRC: u32 = 0xDEAD_BEEF;
    const MTU: usize = 1400;
    const N_AUS: usize = 20;
    const TS_STEP: u32 = 3003; // nominal 30 fps at 90 kHz

    // Build 20 AUs, alternating small (1 NALU, 8 bytes) and large (1 NALU, 3000 bytes)
    // so that half the AUs produce FU-A splits.
    let aus_data: Vec<(u32, Vec<Vec<u8>>)> = (0..N_AUS)
        .map(|i| {
            let ts = i as u32 * TS_STEP;
            let nalu: Vec<u8> = if i % 2 == 0 {
                // Small: 8-byte non-IDR slice (type 1 = 0x41)
                let mut v = vec![0u8; 8];
                v[0] = 0x41;
                // Fill with recognizable pattern per AU
                for (j, b) in v[1..].iter_mut().enumerate() {
                    *b = (i * 7 + j) as u8;
                }
                v
            } else {
                // Large: 3000-byte IDR slice (type 5 = 0x65) — will FU-A split
                let mut v = vec![0u8; 3000];
                v[0] = 0x65;
                // Fill with recognizable pattern per AU
                for (j, b) in v[1..].iter_mut().enumerate() {
                    *b = (i * 13 + j) as u8;
                }
                v
            };
            (ts, vec![nalu])
        })
        .collect();

    // Build expected Annex B per AU.
    let expected: Vec<Vec<u8>> = aus_data
        .iter()
        .map(|(_, nalus)| expected_annexb(nalus))
        .collect();

    let mut config = H264DepayConfig::default();
    config.payload_type = PT;
    config.parameter_set_injection = ParameterSetInjection::None;
    config.initial_parameter_sets = Vec::new();

    let mut rx = H264Receiver::listen_with(
        &tst_rtp::RtpUrl::parse(&format!("rtp://127.0.0.1:0?pt={PT}")).unwrap(),
        config,
    )
    .unwrap();
    let dst = rx.local_addr().unwrap();

    let pkts = packetize(&aus_data, MTU, 1, SSRC, PT);
    let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
    for pkt in &pkts {
        tx.send_to(pkt, dst).unwrap();
    }
    // Drop sender so we can drive recv_au until EOS below.
    drop(tx);

    // Collect AUs.
    let mut received: Vec<tst_rtp::H264Au> = Vec::new();
    let cancel = rx.cancel_handle();
    let h = std::thread::spawn(move || {
        loop {
            match rx.recv_au() {
                Ok(Some(au)) => received.push(au),
                Ok(None) => break,
                Err(e) => {
                    // If we hit a transport error (closed after drop), stop.
                    let _ = e;
                    break;
                }
            }
        }
        received
    });

    // Give the receiver a moment to drain all the UDP packets that were
    // already enqueued, then cancel.
    std::thread::sleep(std::time::Duration::from_millis(300));
    cancel.cancel();
    let received = h.join().expect("receiver thread panicked");

    assert_eq!(
        received.len(),
        N_AUS,
        "expected {N_AUS} AUs, got {}",
        received.len()
    );

    let first_ts = aus_data[0].0;
    for (i, au) in received.iter().enumerate() {
        // Byte-identity.
        assert_eq!(au.annexb, expected[i], "AU {i} Annex B mismatch");
        // PTS delta: AU 0 is at PTS 0; subsequent AUs differ by TS_STEP ticks.
        let expected_pts =
            tst_core::mpegts::common::Pts90khz::new((aus_data[i].0.wrapping_sub(first_ts)) as i64);
        assert_eq!(
            au.pts, expected_pts,
            "AU {i} PTS mismatch: {:?} != {:?}",
            au.pts, expected_pts
        );
        // Key-frame flag: set iff the NALU is type 5 (odd-indexed AUs).
        let want_key = i % 2 == 1;
        assert_eq!(
            au.key_frame, want_key,
            "AU {i} key_frame mismatch (want {want_key})"
        );
        // RTP timestamp matches.
        assert_eq!(
            au.rtp_timestamp, aus_data[i].0,
            "AU {i} rtp_timestamp mismatch"
        );
    }
}

/// Loss-soak: 200 AUs, each packet independently dropped with p=0.2 and a
/// fixed LCG seed.
///
/// # What is (and isn't) asserted
///
/// Structural invariants only:
///
/// 1. No panic during the run.
/// 2. Every emitted AU is byte-identical to its source AU (never partial).
/// 3. `emitted.len() == depay_stats().aus_emitted` (internal consistency).
/// 4. `aus_emitted + aus_dropped <= N_AUS`, and `aus_emitted > 0`.
///
/// We deliberately do NOT predict an exact `aus_emitted + aus_dropped` total:
/// the depacketizer only closes an AU when it sees *evidence* of the next AU
/// starting (a timestamp change or M=1), so an AU's boundary can remain
/// unresolved indefinitely when the subsequent packets are lost.  Predicting
/// the exact count would require re-implementing the depacketizer's state
/// machine.  The honest claim this test makes is: "every AU that IS emitted
/// is correct".
#[test]
fn randomized_loss_soak_no_panic_and_byte_identity() {
    const PT: u8 = 96;
    const SSRC: u32 = 0xC0FFEE;
    const MTU: usize = 1400;
    const N_AUS: usize = 200;
    const TS_STEP: u32 = 3003;
    const SEED: u32 = 0xDEAD_C0DE;
    const P_DROP: f64 = 0.2;

    // Build source AUs: alternate small and large NALUs to mix packet counts.
    let aus_data: Vec<(u32, Vec<Vec<u8>>)> = (0..N_AUS)
        .map(|i| {
            let ts = i as u32 * TS_STEP;
            let nalu: Vec<u8> = if i % 3 == 0 {
                // Small: 10-byte non-IDR slice
                let mut v = vec![0x41u8; 10];
                for (j, b) in v[1..].iter_mut().enumerate() {
                    *b = (i * 7 + j + 1) as u8;
                }
                v
            } else {
                // Large: 2500-byte IDR slice — will FU-A split across 2 packets
                let mut v = vec![0x65u8; 2500];
                for (j, b) in v[1..].iter_mut().enumerate() {
                    *b = (i * 11 + j + 1) as u8;
                }
                v
            };
            (ts, vec![nalu])
        })
        .collect();

    // Generate all packets.
    let all_pkts = packetize(&aus_data, MTU, 1, SSRC, PT);

    // Apply the LCG drop model: each packet dropped independently with p=P_DROP.
    let mut rng = Lcg::new(SEED);
    let mut surviving_pkt_indices: Vec<usize> = Vec::new();
    for idx in 0..all_pkts.len() {
        if !rng.should_drop(P_DROP) {
            surviving_pkt_indices.push(idx);
        }
    }

    // Build a lookup: au_index → source annexb bytes (for byte-identity check).
    // (See the function doc for why only structural invariants are asserted.)
    let au_annexb: Vec<Vec<u8>> = aus_data
        .iter()
        .map(|(_, nalus)| expected_annexb(nalus))
        .collect();

    // ── Run the receiver ──────────────────────────────────────────────────────

    let mut config = H264DepayConfig::default();
    config.payload_type = PT;
    config.parameter_set_injection = ParameterSetInjection::None;
    config.initial_parameter_sets = Vec::new();
    let mut rx = H264Receiver::listen_with(
        &tst_rtp::RtpUrl::parse(&format!("rtp://127.0.0.1:0?pt={PT}")).unwrap(),
        config,
    )
    .unwrap();
    let dst = rx.local_addr().unwrap();

    // Send only the surviving packets, in their original order.
    let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
    for &pkt_idx in &surviving_pkt_indices {
        tx.send_to(&all_pkts[pkt_idx], dst).unwrap();
    }
    drop(tx);

    // Collect AUs from the receiver (cancellable after a drain window).
    let cancel = rx.cancel_handle();
    let au_annexb_for_thread = au_annexb.clone();
    let h = std::thread::spawn(move || {
        let mut emitted: Vec<tst_rtp::H264Au> = Vec::new();
        loop {
            match rx.recv_au() {
                Ok(Some(au)) => emitted.push(au),
                Ok(None) => return (emitted, rx),
                Err(_) => return (emitted, rx),
            }
        }
    });

    // Give the receiver time to drain all enqueued UDP datagrams, then cancel.
    std::thread::sleep(std::time::Duration::from_millis(500));
    cancel.cancel();
    let (emitted, rx) = h.join().expect("receiver thread panicked");

    // ── Assertions ────────────────────────────────────────────────────────────

    // 1. Every emitted AU is byte-identical to its source AU (no partial AUs).
    for au in &emitted {
        let au_idx = (au.rtp_timestamp / TS_STEP) as usize;
        if au_idx < N_AUS {
            assert_eq!(
                au.annexb, au_annexb_for_thread[au_idx],
                "loss-soak: AU {au_idx} byte mismatch — receiver emitted a partial or wrong AU"
            );
        }
    }

    // 2. Emitted count matches the depacketizer's own stats counter.
    let stats = rx.depay_stats();
    assert_eq!(
        emitted.len() as u64,
        stats.aus_emitted,
        "emitted AU vector length doesn't match depay_stats().aus_emitted"
    );

    // 3. Sanity: emitted + dropped ≤ N_AUS (can be fewer if some AU boundaries
    //    remain unresolved at the time the run ends).
    assert!(
        stats.aus_emitted + stats.aus_dropped <= N_AUS as u64,
        "aus_emitted({}) + aus_dropped({}) exceeds N_AUS({})",
        stats.aus_emitted,
        stats.aus_dropped,
        N_AUS
    );

    // 4. At least one AU must have been emitted (with 200 AUs and 80% delivery
    //    the expected emitted count is well above zero).
    assert!(
        stats.aus_emitted > 0,
        "loss-soak: no AUs emitted at all — receiver or payloader is broken"
    );
}

/// `?pkt_size=` on an elementary-RTP receive URL is rejected the same
/// way as on the MP2T receive path.
#[test]
fn h264_listen_rejects_pkt_size_query() {
    let result = tst_rtp::H264Receiver::listen("rtp://127.0.0.1:0?pt=96&pkt_size=1316");
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("h264 recv URL with ?pkt_size= must be rejected"),
    };
    assert!(matches!(
        err,
        tst_rtp::ConnectError::Url(tst_rtp::RtpUrlError::RecvPktSize)
    ));
}

/// `?sndbuf=` is a send-side knob; the elementary-RTP receiver refuses it
/// like the MP2T receive path does.
#[test]
fn h264_listen_rejects_sndbuf_query() {
    let err = match tst_rtp::H264Receiver::listen("rtp://127.0.0.1:0?pt=96&sndbuf=1M") {
        Err(e) => e,
        Ok(_) => panic!("h264 recv URL with ?sndbuf= must be rejected"),
    };
    assert!(
        matches!(
            err,
            tst_rtp::ConnectError::Url(tst_rtp::RtpUrlError::RecvSndbuf)
        ),
        "wrong error variant: {err:?}"
    );
}

/// `?source=` on the elementary-RTP receiver drops datagrams from any other
/// peer IP and counts them in `RtpStats::source_rejected`. Needs a second
/// loopback address (127.0.0.2), which only Linux answers on by default.
#[cfg(target_os = "linux")]
#[test]
fn h264_source_pin_drops_and_counts_other_peers() {
    let mut rx = H264Receiver::listen("rtp://127.0.0.1:0?pt=96&source=127.0.0.1").unwrap();
    let addr = rx
        .local_addr()
        .expect("bound UDP receiver has a local addr");

    let foreign = UdpSocket::bind("127.0.0.2:0").expect("bind 127.0.0.2");
    let pkts = packetize(&[(0, vec![vec![0x65, 1, 2, 3]])], 1400, 1, 0xDEAD_BEEF, 96);
    for p in &pkts {
        foreign.send_to(p, addr).unwrap();
    }

    // The foreign datagrams are already queued; a short deadline lets the
    // receive loop drain and reject them, then return Backpressure.
    let res = rx.recv_au_timeout(std::time::Duration::from_millis(300));
    assert!(
        matches!(
            res,
            Err(tst_core::transport::TransportError::Backpressure { .. })
        ),
        "no AU may be assembled from a rejected peer: {res:?}"
    );
    assert_eq!(rx.rtp_stats().source_rejected, pkts.len() as u64);
    assert_eq!(rx.depay_stats().aus_emitted, 0);
}
