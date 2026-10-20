//! Test helpers for the socket-opening WHEP tests. The viewer, the HTTP
//! client and the pump helpers live in `tst_test_helpers::whep_viewer`
//! (re-exported here); this module adds the media fixtures (a 30-frame
//! H.264 GOP, ST 0601 records) and the tests' own bounds.

use std::time::Duration;

use tst_core::klv::st0601::{UasDatalinkLs, encode_to_vec};
pub use tst_test_helpers::whep_viewer::*;
use tst_webrtc::MountHandle;

#[cfg(feature = "tls")]
pub mod tls_certs;
pub mod ts;

/// How long a test waits for `WhepServer::stop` to report back: its
/// contract, a 1 s connect that wakes the accept thread plus at most 2 s
/// for each of its two threads. The test's own bound, not a timing
/// assertion on `stop`.
pub const STOP_REPORT: Duration = Duration::from_secs(5);

/// How long the viewers get to connect.
const CONNECT_WAIT: Duration = Duration::from_secs(5);

/// Pumps the viewers until each is connected and the server reports every
/// session on `m` connected.
pub fn connect_all(m: &MountHandle, viewers: &mut [TestViewer]) {
    let n = viewers.len();
    let ok = pump_all(
        viewers,
        |vs| {
            vs.iter().all(|v| v.connected)
                && m.sessions()
                    .iter()
                    .filter(|s| s.connected_for.is_some())
                    .count()
                    == n
        },
        CONNECT_WAIT,
    );
    let traces: Vec<String> = viewers.iter().map(TestViewer::trace).collect();
    assert!(ok, "viewers did not connect: {traces:?}");
}

/// The 30-frame GOP: IDR first, then 29 P frames, each opened by an AUD.
const GOP: &[u8] = include_bytes!("h264/gop.h264");

/// The GOP fixture as (Annex-B access unit, is keyframe) pairs.
pub fn h264_gop() -> Vec<(Vec<u8>, bool)> {
    let starts: Vec<usize> = GOP
        .windows(4)
        .enumerate()
        .filter(|(_, w)| *w == [0, 0, 0, 1])
        .map(|(i, _)| i)
        .collect();
    let mut aus: Vec<(Vec<u8>, bool)> = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let end = starts.get(k + 1).copied().unwrap_or(GOP.len());
        let nal = &GOP[s..end];
        let nal_type = nal[4] & 0x1f;
        if nal_type == 9 || aus.is_empty() {
            aus.push((Vec::new(), false));
        }
        let au = aus.last_mut().expect("an access unit");
        au.0.extend_from_slice(nal);
        au.1 |= nal_type == 5;
    }
    aus
}

/// A minimal ST 0601 record whose platform heading is `i` degrees.
pub fn klv_record(i: u8) -> Vec<u8> {
    let ls = UasDatalinkLs {
        platform_heading_deg: Some(f64::from(i)),
        ..Default::default()
    };
    encode_to_vec(&ls).expect("the record encodes")
}
