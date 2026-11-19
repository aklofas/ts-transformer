//! How a push ends: the server stopping, its hard cancel and its publish
//! idle timeout end it from the other side; a closed push ends a mount
//! that ends with its publisher; and a cancel ends a close that waits on a
//! server that stopped reading.

use std::sync::mpsc;
use std::time::Duration;

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
/// payload type. After its one packet the push pauses and then sends only
/// keepalive pings: neither the pause nor the pings keep it, and the
/// server ends the connection without a word. (The pause's last report
/// goes out at once, so this does not show that reports are not counted.)
#[test]
fn pings_and_a_pause_do_not_keep_a_push_past_the_publish_idle_timeout() {
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

/// A server that reads slowly, with tens of megabytes queued: the close
/// would wait minutes for the queue, and since the connection keeps taking
/// bytes, the client's rule that ends a connection taking nothing for 5 s
/// does not end that wait. Only the cancel can, so the close returning at
/// all, with the cancel's error, is the cancel's doing; no duration is
/// asserted.
#[test]
fn a_cancel_ends_a_close_waiting_on_a_server_that_reads_slowly() {
    let s = Served::new();
    let relay = TcpRelay::to(s.server.local_addr().unwrap());
    let mut push = RtspPush::connect(builder_via(&relay), &elementary(true)).unwrap();
    relay.slow_down();
    // Fewer frames than the queue holds, so no send waits.
    for i in 0..1000 {
        push.send_klv(&UNIT, pts(i)).unwrap();
    }
    let cancel = push.cancel_handle();
    let (began, beginning) = mpsc::channel();
    let (result, results) = mpsc::channel();
    std::thread::spawn(move || {
        began.send(()).unwrap();
        let _ = result.send(push.close());
    });
    beginning
        .recv_timeout(scaled(Duration::from_secs(5)))
        .expect("the close began");
    cancel.cancel();
    // A bound for a failure only.
    let r = results
        .recv_timeout(scaled(Duration::from_secs(20)))
        .expect("the cancelled close returned");
    assert!(matches!(&r, Err(RtspError::LocalCancel)), "{r:?}");
}
