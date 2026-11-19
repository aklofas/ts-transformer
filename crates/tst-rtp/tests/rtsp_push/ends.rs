//! How a push ends: the server stopping, its hard cancel and its publish
//! idle timeout end it from the other side; a closed push ends a mount
//! that ends with its publisher; and a cancel ends a close that waits on a
//! server that stopped reading.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use tst_core::transport::{BrokenCause, EndReason, Transport, TransportError};
use tst_rtp::{PushDescription, RtspError, RtspPush};

use crate::common::*;
use crate::fixtures::raw_rtsp::scaled;
use crate::fixtures::raw_rtsp_publisher::ts_fixture_packets;
use crate::fixtures::tcp_relay::TcpRelay;

fn clean_eof(e: &TransportError) -> bool {
    matches!(
        e,
        TransportError::Broken {
            cause: BrokenCause::CleanEof,
            ..
        }
    )
}

/// `stop()` writes a Notice ANNOUNCE to the push, then closes. The push
/// takes the request for none of its own (it ends cleanly, not on a broken
/// message) and ends as a server that closed the connection.
#[test]
fn server_stop_ends_the_push_broken_clean_eof() {
    let s = Served::new();
    let relay = TcpRelay::to(s.server.local_addr().unwrap());
    let mut push = RtspPush::connect(builder_via(&relay), &PushDescription::Mp2t).unwrap();
    push.send_ts(&ts_packets(7, 0x44)).unwrap();
    s.stats_when("the first packet", |st| st.rtp_packets_received >= 1);

    s.server.stop().unwrap();
    let e = refused_send(&mut push);
    assert!(clean_eof(&e), "{e:?}");
    assert_eq!(push.end_reason(), Some(EndReason::CleanTeardown));
    assert!(!push.is_session_alive());
    assert!(!Transport::is_alive(&push));
    let notice = String::from_utf8_lossy(&relay.downstream()).into_owned();
    assert!(
        notice.contains("ANNOUNCE ") && notice.contains("Notice: 5402"),
        "the server wrote no Notice: {notice}"
    );
    // The connection is gone: the TEARDOWN cannot be written.
    let r = push.close();
    assert!(matches!(&r, Err(RtspError::Io(_))), "{r:?}");
}

#[test]
fn the_servers_hard_cancel_ends_the_push() {
    let s = Served::new();
    let mut push = s.push(&PushDescription::Mp2t);
    push.send_ts(&ts_packets(7, 0x55)).unwrap();
    s.stats_when("the first packet", |st| st.rtp_packets_received >= 1);

    s.server.cancel_handle().cancel();
    let e = refused_send(&mut push);
    assert!(clean_eof(&e), "{e:?}");
    assert_eq!(push.end_reason(), Some(EndReason::CleanTeardown));
    assert!(!Transport::is_alive(&push));
}

/// The server's publish idle timeout counts only RTP on the announced
/// payload type. After its one packet the push sends a last report, a
/// PAUSE and keepalive pings: none of them keeps it, and the server ends
/// the connection without a word.
#[test]
fn publish_idle_timeout_ends_a_push_that_sends_only_reports_and_pings() {
    let s = Served::with(
        |b| {
            b.session_timeout(Duration::from_secs(60));
            b.publish_idle_timeout(scaled(Duration::from_secs(1)));
        },
        Default::default(),
    );
    let relay = TcpRelay::to(s.server.local_addr().unwrap());
    let pinging = builder_via(&relay).keepalive_interval(Duration::from_millis(100));
    let mut push = RtspPush::connect(pinging, &PushDescription::Mp2t).unwrap();
    push.send_ts(&ts_packets(7, 0x66)).unwrap();
    push.pause().unwrap();

    assert_eq!(end_reason_of(&push), EndReason::CleanTeardown);
    assert!(!push.is_session_alive());
    s.until_no_publisher();
    assert_eq!(s.mount.stats().rtp_packets_received, 1);
    let sent = String::from_utf8_lossy(&relay.upstream()).into_owned();
    let pings = sent.matches("OPTIONS ").count();
    assert!(pings >= 2, "{pings} pings went out before the end");
}

/// A mount that ends with its publisher: after the push closes, the
/// mount's receiver yields every byte the push sent, then ends; the mount
/// takes no other publisher.
#[test]
fn close_ends_the_mounts_receiver_with_end_of_stream() {
    let s = Served::ending_on_exit();
    let mut app = s.app();
    let mut push = s.push(&PushDescription::Mp2t);
    let ts: Vec<u8> = ts_fixture_packets(120).concat();
    for bundle in ts.chunks(7 * 188) {
        push.send_ts(bundle).unwrap();
    }
    push.close().unwrap();
    // `read_ts_to_end` reads to the receiver's `Closed`.
    assert_same_bytes(&read_ts_to_end(&mut app), &ts, "the mount's stream");
    let r = RtspPush::connect(s.builder(), &PushDescription::Mp2t);
    assert!(
        matches!(&r, Err(RtspError::Protocol { code: 403, .. })),
        "{:?}",
        r.map(|_| "a push")
    );
}

/// KLV units far larger, together, than the connection holds.
static UNIT: [u8; 60_000] = [0xAB; 60_000];

/// A server that stopped reading holds `close` in its wait for the queued
/// media; the cancel ends that wait. The client's own rule ends a
/// connection that took nothing for 5 s, so `close` returning well inside
/// that is the cancel's doing. The bound includes the TEARDOWN, which the
/// cancelled close still tries to write for up to 500 ms.
#[test]
fn a_cancelled_close_against_a_stalled_server_returns_within_a_second() {
    let s = Served::new();
    let relay = TcpRelay::to(s.server.local_addr().unwrap());
    let mut push = RtspPush::connect(builder_via(&relay), &elementary(true)).unwrap();
    relay.hold();
    // Fewer frames than the queue holds, so no send waits; tens of
    // megabytes, so most of them stay queued.
    for i in 0..1000 {
        push.send_klv(&UNIT, pts(i)).unwrap();
    }
    let cancel = push.cancel_handle();
    let (returned, at) = mpsc::channel();
    let closer = std::thread::spawn(move || {
        let r = push.close();
        returned.send(Instant::now()).unwrap();
        r
    });
    // Pacing: the close is waiting for its queue by now, not before it.
    std::thread::sleep(Duration::from_millis(100));
    let cancelled = Instant::now();
    cancel.cancel();
    let returned = at
        .recv_timeout(scaled(Duration::from_secs(10)))
        .expect("the cancelled close returned");
    let r = closer.join().unwrap();
    assert!(matches!(&r, Err(RtspError::LocalCancel)), "{r:?}");
    let took = returned.saturating_duration_since(cancelled);
    assert!(
        took <= scaled(Duration::from_secs(2)),
        "the cancelled close returned {took:?} after the cancel"
    );
}
