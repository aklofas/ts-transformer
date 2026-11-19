//! Sender shells over a push: what the shell hands the push reaches the
//! mount byte for byte, and a shell that closes its transport, also by
//! being dropped, has the push write its queue before the TEARDOWN.

use std::sync::{Arc, Mutex};

use tst_core::transport::{EndReason, SocketStats, Transport, TransportCancel, TransportError};
use tst_pipeline::{MuxSender, Sender, SenderConfig};
use tst_rtp::{PushDescription, RtspPush};

use crate::common::*;
use crate::fixtures::raw_rtsp::make_muxer_cfg;
use crate::fixtures::raw_rtsp_publisher::ts_fixture_packets;

/// What a push under a shell was given, and how its close left it.
#[derive(Default)]
struct Notes {
    /// Every byte the shell handed the push and the push took.
    sent: Vec<u8>,
    /// Frames the push dropped at its close, and the reason it ended:
    /// read right after the shell closed it.
    at_close: Option<(u64, Option<EndReason>)>,
}

/// A push that notes what passes through it.
struct Noted {
    push: RtspPush,
    notes: Arc<Mutex<Notes>>,
}

impl Noted {
    fn new(push: RtspPush) -> (Self, Arc<Mutex<Notes>>) {
        let notes = Arc::new(Mutex::new(Notes::default()));
        let noted = Self {
            push,
            notes: notes.clone(),
        };
        (noted, notes)
    }
}

impl Transport for Noted {
    fn send_bytes(&mut self, msg: &[u8]) -> Result<(), TransportError> {
        self.push.send_bytes(msg)?;
        self.notes.lock().unwrap().sent.extend_from_slice(msg);
        Ok(())
    }

    fn max_payload(&self) -> usize {
        self.push.max_payload()
    }

    fn is_alive(&self) -> bool {
        Transport::is_alive(&self.push)
    }

    fn close(&mut self) {
        Transport::close(&mut self.push);
        let at_close = (
            self.push.stats().frames_dropped_at_close,
            self.push.end_reason(),
        );
        self.notes.lock().unwrap().at_close = Some(at_close);
    }

    fn cancel_handle(&self) -> Option<Arc<dyn TransportCancel + Send + Sync>> {
        Transport::cancel_handle(&self.push)
    }

    fn socket_stats(&self) -> Option<SocketStats> {
        self.push.socket_stats()
    }
}

/// Push access units `0..aus` through a `MuxSender` over a noted push.
fn mux_sender(s: &Served, aus: usize) -> (MuxSender<Noted>, Arc<Mutex<Notes>>) {
    let (noted, notes) = Noted::new(s.push(&PushDescription::Mp2t));
    let sender = MuxSender::new(noted, make_muxer_cfg()).unwrap();
    for i in 0..aus {
        let at = pts(9_000 + i as i64 * AU_STEP);
        sender.send_video(&access_unit(i), at, i == 0).unwrap();
    }
    (sender, notes)
}

/// The mount yields what the shell handed the push, and the shell's close
/// left nothing dropped and the session ended cleanly.
fn assert_delivered_whole(s: &Served, app: &mut tst_rtp::RtpRecvTransport, notes: &Mutex<Notes>) {
    let got = read_ts_to_end(app);
    let notes = notes.lock().unwrap();
    assert_same_bytes(&got, &notes.sent, "the mount's stream");
    assert_eq!(notes.at_close, Some((0, Some(EndReason::CleanTeardown))));
    assert_eq!(s.mount.stats().malformed_packets, 0);
}

#[test]
fn mp2t_through_mux_sender_arrives_byte_identical() {
    let s = Served::ending_on_exit();
    let mut app = s.app();
    let (sender, notes) = mux_sender(&s, 90);
    sender.finish().unwrap();
    assert_delivered_whole(&s, &mut app, &notes);
}

#[test]
fn mp2t_through_sender_arrives_byte_identical() {
    let s = Served::ending_on_exit();
    let mut app = s.app();
    let (noted, notes) = Noted::new(s.push(&PushDescription::Mp2t));
    let mut sender = Sender::new(noted, SenderConfig::default());
    let ts: Vec<u8> = ts_fixture_packets(150).concat();
    // Pieces that are not whole packets: the shell bundles them.
    for piece in ts.chunks(1000) {
        sender.send_ts(piece).unwrap();
    }
    sender.flush().unwrap();
    sender.close();
    assert_same_bytes(&notes.lock().unwrap().sent, &ts, "what the shell sent");
    assert_delivered_whole(&s, &mut app, &notes);
}

/// The shell's drop closes the push, which writes what it still queues:
/// a push dropped instead would discard it.
#[test]
fn a_dropped_mux_sender_over_a_push_delivers_its_tail() {
    let s = Served::ending_on_exit();
    let mut app = s.app();
    let (sender, notes) = mux_sender(&s, 300);
    drop(sender);
    assert_delivered_whole(&s, &mut app, &notes);
}
