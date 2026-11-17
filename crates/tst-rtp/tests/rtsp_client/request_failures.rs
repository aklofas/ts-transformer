//! A request that fails on a TCP-interleaved session reports the cancel
//! when the client's cancel is set, and the failure itself otherwise.

use std::io::Write;
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use tst_rtp::{RtspCancelHandle, RtspClient, RtspError};

#[cfg(target_os = "linux")]
use crate::fixtures::raw_rtsp_server::force_reset;
use crate::fixtures::raw_rtsp_server::{answer_describe_and_setup, method_of, read_request};
#[cfg(target_os = "linux")]
use crate::fixtures::wait_for;

const INTERLEAVED: &str = "RTP/AVP/TCP;unicast;interleaved=0-1";

/// The peer resets the control connection after SETUP; once the reset is
/// recorded the client sends PLAY, with its cancel set first when
/// `cancel`. Linux only: the reset-then-request sequence has not been
/// measured on macOS or Windows.
#[cfg(target_os = "linux")]
fn play_after_a_reset(cancel: bool) -> Result<tst_rtp::RtpInfo, RtspError> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // The reset waits until SETUP's reply was read: a reset purges what
    // the client has not read yet.
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        answer_describe_and_setup(&mut sock, &mut leftover, INTERLEAVED);
        let _ = go_rx.recv_timeout(Duration::from_secs(10));
        force_reset(sock);
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let transport = client.setup_mp2t_auto(&sdp).unwrap().into_recv_transport();
    go_tx.send(()).unwrap();
    wait_for(Duration::from_secs(10), || transport.end_reason());
    if cancel {
        client.cancel_handle().cancel();
    }
    let r = client.play();
    peer.join().unwrap();
    r
}

#[cfg(target_os = "linux")]
#[test]
fn a_write_failure_on_a_cancelled_interleaved_client_reports_the_cancel() {
    let r = play_after_a_reset(true);
    assert!(matches!(r, Err(RtspError::LocalCancel)), "got {r:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn a_write_failure_on_an_interleaved_client_without_a_cancel_is_io() {
    let r = play_after_a_reset(false);
    assert!(matches!(r, Err(RtspError::Io(_))), "got {r:?}");
}

/// The peer reads PLAY and closes the connection without a reply. With
/// `cancel`, it fires the client's cancel after it has the request and
/// before it closes, so the cancel lands during the reply wait: whichever
/// the client sees first, the result is the cancel.
fn play_then_peer_closes(cancel: bool) -> Result<tst_rtp::RtpInfo, RtspError> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (handle_tx, handle_rx) = mpsc::channel::<RtspCancelHandle>();
    let peer = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut leftover = Vec::new();
        answer_describe_and_setup(&mut sock, &mut leftover, INTERLEAVED);
        let req = read_request(&mut sock, &mut leftover);
        assert_eq!(method_of(&req), "PLAY", "unexpected request: {req}");
        if let Ok(h) = handle_rx.recv_timeout(Duration::from_secs(10)) {
            h.cancel();
        }
        let _ = sock.flush();
        drop(sock);
    });

    let url = format!("rtsp://127.0.0.1:{port}/test?transport=tcp");
    let mut client = RtspClient::connect(&url).unwrap();
    let sdp = client.describe().unwrap();
    let _session = client.setup_mp2t_auto(&sdp).unwrap();
    if cancel {
        handle_tx.send(client.cancel_handle()).unwrap();
    }
    drop(handle_tx);
    let r = client.play();
    peer.join().unwrap();
    r
}

#[test]
fn an_eof_on_a_cancelled_interleaved_client_reports_the_cancel() {
    let r = play_then_peer_closes(true);
    assert!(matches!(r, Err(RtspError::LocalCancel)), "got {r:?}");
}

#[test]
fn an_eof_on_an_interleaved_client_without_a_cancel_is_io() {
    let r = play_then_peer_closes(false);
    assert!(
        matches!(r, Err(RtspError::Io(std::io::ErrorKind::UnexpectedEof))),
        "got {r:?}"
    );
}
