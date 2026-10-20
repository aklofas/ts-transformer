//! The reapers and the per-viewer queue on a server whose media thread
//! runs on a manual clock: the test moves the clock and waits for one full
//! pass of the thread (`wake_for_test`), so every deadline is crossed
//! exactly when the test says, never by waiting.

use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_webrtc::url::WhepUrl;
use tst_webrtc::{IceState, ManualClock, MountConfig, WhepServer};

use crate::fixtures::{STOP_REPORT, TestViewer, connect_all, h264_gop, join, whep_post};

const DELIVER: Duration = Duration::from_secs(10);

/// Payload count of every P-frame the backpressure test pushes: above the
/// smallest viewer queue (16), so such a frame never fits it.
const P_NALS: usize = 20;

/// Slices appended to the fixture's IDR, so the keyframe alone has more
/// payloads than the smallest viewer queue.
const EXTRA_IDR_SLICES: usize = 20;

fn manual_server() -> (WhepServer, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new());
    let url = WhepUrl::parse("whep://127.0.0.1:0").unwrap();
    let srv = WhepServer::bind_with_clock(url, clock.clone()).unwrap();
    (srv, clock)
}

/// A mount whose viewers queue at most `packets` payloads.
fn queue_of(packets: u32) -> MountConfig {
    let mut cfg = MountConfig::default();
    cfg.viewer_queue_packets = packets;
    cfg
}

/// Moves the clock by `d` and waits for the media thread to act on it.
fn advance(srv: &WhepServer, clock: &ManualClock, d: Duration) {
    clock.advance(d);
    assert!(srv.wake_for_test(), "the media thread did not answer");
}

fn pts(i: usize) -> Pts90khz {
    Pts90khz::new(i as i64 * 3000)
}

/// The fixture's IDR access unit (SPS, PPS, IDR) with more IDR slices
/// after it: one keyframe of more payloads than a 16-entry queue holds.
fn big_keyframe() -> Vec<u8> {
    let mut au = h264_gop()[0].0.clone();
    for i in 0..EXTRA_IDR_SLICES {
        au.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, i as u8]);
    }
    au
}

/// A P-frame of [`P_NALS`] slices, each one RTP payload.
fn p_frame(i: usize) -> Vec<u8> {
    let mut au = Vec::new();
    for k in 0..P_NALS {
        au.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9a, i as u8, k as u8]);
    }
    au
}

#[test]
fn a_viewer_that_never_sends_a_datagram_is_reaped_at_the_ice_timeout() {
    let (srv, clock) = manual_server();
    let _m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    // The viewer takes the answer and is never pumped: not one STUN check
    // or DTLS record reaches the server.
    let mut v = TestViewer::new();
    let offer = v.offer(false);
    let (status, _, answer) = whep_post(srv.local_addr(), "/cam", &offer, None);
    assert_eq!(status, 201, "{answer}");
    v.accept_answer(&answer);

    advance(&srv, &clock, Duration::from_secs(9));
    let s = srv.stats();
    assert_eq!((s.active_sessions, s.sessions_reaped_ice_timeout), (1, 0));

    advance(&srv, &clock, Duration::from_millis(1_500));
    let s = srv.stats();
    assert_eq!(s.sessions_reaped_ice_timeout, 1);
    assert_eq!(s.active_sessions, 0);
    assert_eq!(s.sessions_reaped_disconnected, 0);
}

#[test]
fn a_viewer_whose_socket_closes_is_reaped_as_disconnected() {
    let (srv, clock) = manual_server();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut viewers = [TestViewer::new()];
    join(srv.local_addr(), "/cam", &mut viewers[0], false);
    connect_all(&m, &mut viewers);
    let [v] = viewers;
    drop(v);

    // From here nothing reaches the server: its consent check lapses, the
    // session leaves Connected, and the disconnect timeout (30 s) runs from
    // that instant. Step a second at a time to find both instants.
    let mut left_connected = None;
    let mut reaped_at = None;
    for second in 1..=60u64 {
        advance(&srv, &clock, Duration::from_secs(1));
        if srv.stats().sessions_reaped_disconnected == 1 {
            reaped_at = Some(second);
            break;
        }
        let ice = m.sessions().pop().expect("the session").ice_state;
        if ice != IceState::Connected && left_connected.is_none() {
            left_connected = Some(second);
        }
    }
    let left = left_connected.expect("the session never left Connected");
    let reaped = reaped_at.expect("the session was never reaped");
    assert_eq!(reaped, left + 30, "left Connected at {left} s");
    let s = srv.stats();
    assert_eq!(s.active_sessions, 0);
    assert_eq!(s.sessions_reaped_ice_timeout, 0);
}

#[test]
fn a_keyframe_larger_than_the_viewer_queue_still_starts_the_viewer() {
    let (srv, _clock) = manual_server();
    let m = srv.add_mount("/cam", queue_of(16)).unwrap();
    let mut viewers = [TestViewer::new()];
    join(srv.local_addr(), "/cam", &mut viewers[0], false);
    connect_all(&m, &mut viewers);
    let [v] = &mut viewers;

    m.push_video(&big_keyframe(), pts(0), true).unwrap();
    m.push_video(&big_keyframe(), pts(1), true).unwrap();
    assert!(v.pump(|v| v.frames.len() >= 2, DELIVER), "{}", v.trace());
    let session = m.sessions().pop().expect("the session");
    assert!(session.rtp_packets > 32, "{session:?}");
    assert_eq!(session.packets_dropped_queue_full, 0, "{session:?}");
}

#[test]
fn a_viewer_whose_queue_overflows_is_reaped_and_the_other_gets_everything() {
    const FRAMES: usize = 250;
    const BATCH: usize = 10;
    let (srv, clock) = manual_server();
    // The two viewers differ only in how much their queue holds: the slow
    // one cannot take a single P-frame, the fast one can take every payload
    // of the run at once, however the media thread's passes fall.
    let slow_m = srv.add_mount("/slow", queue_of(16)).unwrap();
    let fast_m = srv
        .add_mount("/fast", queue_of(2 * (FRAMES * P_NALS) as u32))
        .unwrap();
    let mut slow = [TestViewer::new()];
    let mut fast = [TestViewer::new()];
    join(srv.local_addr(), "/slow", &mut slow[0], false);
    join(srv.local_addr(), "/fast", &mut fast[0], false);
    connect_all(&slow_m, &mut slow);
    connect_all(&fast_m, &mut fast);
    let ([slow], [fast]) = (&mut slow, &mut fast);

    // Both start on the keyframe; the pass that hands it over runs before
    // the first P-frame is pushed, so the slow queue is empty again.
    for m in [&slow_m, &fast_m] {
        m.push_video(&big_keyframe(), pts(0), true).unwrap();
    }
    assert!(srv.wake_for_test());
    assert!(
        slow.pump(|v| !v.frames.is_empty(), DELIVER),
        "{}",
        slow.trace()
    );
    assert!(
        fast.pump(|v| !v.frames.is_empty(), DELIVER),
        "{}",
        fast.trace()
    );

    // The slow viewer stops pumping. 5000 payloads follow; the fast viewer
    // reads them in batches so its own socket buffer never overflows.
    for start in (1..=FRAMES).step_by(BATCH) {
        for i in start..start + BATCH {
            let au = p_frame(i);
            slow_m.push_video(&au, pts(i), false).unwrap();
            fast_m.push_video(&au, pts(i), false).unwrap();
        }
        let want = start + BATCH;
        assert!(
            fast.pump(|v| v.frames.len() >= want, DELIVER),
            "{}",
            fast.trace()
        );
    }
    assert!(srv.wake_for_test());
    let slow_s = slow_m.sessions().pop().expect("the slow session");
    let fast_s = fast_m.sessions().pop().expect("the fast session");
    // One P-frame is dropped whole; every later one is skipped while the
    // slow viewer waits for a keyframe that never comes.
    assert_eq!(
        slow_s.packets_dropped_queue_full, P_NALS as u64,
        "{slow_s:?}"
    );
    assert_eq!(fast_s.packets_dropped_queue_full, 0, "{fast_s:?}");
    assert_eq!(fast.frames.len(), 1 + FRAMES);
    assert_eq!(
        fast_s.rtp_packets,
        slow_s.rtp_packets + (FRAMES * P_NALS) as u64,
        "the fast viewer got the slow one's keyframe plus every P-frame"
    );

    // Nothing drains the slow queue again, so 30 s after the drop it is
    // closed. The fast viewer keeps answering consent checks throughout:
    // after each step two responses reach it, so one of its checks reached
    // the server after the clock moved.
    let mut keep_fast_fresh = |d: Duration| {
        advance(&srv, &clock, d);
        let seen = fast.stun_responses;
        assert!(
            fast.pump(|v| v.stun_responses >= seen + 2, DELIVER),
            "{}",
            fast.trace()
        );
        assert!(srv.wake_for_test());
    };
    keep_fast_fresh(Duration::from_secs(10));
    keep_fast_fresh(Duration::from_secs(10));
    keep_fast_fresh(Duration::from_millis(9_900));
    let s = srv.stats();
    assert_eq!((s.active_sessions, s.sessions_reaped_disconnected), (2, 0));
    // The slow viewer's own consent lapsed by now (15 s after its last
    // check), but a lapsed session gets 30 s from the instant it left
    // Connected, so a close at 31 s can only be the queue's.
    keep_fast_fresh(Duration::from_millis(1_100));
    let s = srv.stats();
    assert_eq!(s.sessions_reaped_disconnected, 1, "{s:?}");
    assert_eq!(s.sessions_reaped_ice_timeout, 0);
    assert_eq!(s.active_sessions, 1);
    assert!(slow_m.sessions().is_empty());
    assert_eq!(fast_m.sessions().len(), 1);
}

#[test]
fn stop_with_a_viewer_parked_before_ice_returns_and_joins_both_threads() {
    let (srv, _clock) = manual_server();
    let srv = Arc::new(srv);
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    // The manual clock never moves, so the ICE timeout cannot close the
    // session first: it is still waiting for its first check at stop.
    let mut v = TestViewer::new();
    let offer = v.offer(true);
    let (status, _, answer) = whep_post(srv.local_addr(), "/cam", &offer, None);
    assert_eq!(status, 201, "{answer}");
    v.accept_answer(&answer);
    assert_eq!(srv.stats().active_sessions, 1);

    let (stopped_tx, stopped_rx) = mpsc::channel();
    let stopper = Arc::clone(&srv);
    thread::spawn(move || {
        stopper.stop();
        let _ = stopped_tx.send(());
    });
    stopped_rx
        .recv_timeout(STOP_REPORT)
        .expect("stop() returned");
    assert!(srv.threads_joined_for_test());
    assert!(m.sessions().is_empty());
    assert!(!srv.wake_for_test(), "the media thread is gone");
}
