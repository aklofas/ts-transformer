//! The C leg of the URL corpus (`crates/tst-integration/tests/fixtures/
//! scenarios/url-corpus/corpus.toml`): every row's URL goes through the C entry point
//! the corpus header names for its scheme and role, and the outcome must match
//! the row's record. C exposes no parsed URL, so only the outcome is compared:
//! a refused row must fail with the expected kind's `TST_E_*` code, an
//! accepted row must not fail with the code of one of the scheme's URL kinds.

#![cfg(all(
    feature = "srt",
    feature = "rtp",
    feature = "udp",
    feature = "tcp",
    feature = "hls",
    feature = "rist"
))]

#[allow(dead_code)]
#[path = "common/mod.rs"]
mod common;

use std::ffi::CString;
use std::time::Duration;

use tst_integration::scenarios::url::{self, Row};
use tst_srt::{SrtListenerBuilder, SrtSocketBuilder};
use tstrans::config::{tst_sender_config_free, tst_sender_config_new};
use tstrans::error::{tst_clear_last_error, tst_get_last_error};
use tstrans::hls::{
    tst_hls_publisher_builder_free, tst_hls_publisher_builder_from_url,
    tst_hls_publisher_builder_new,
};
use tstrans::receiver::ts_receiver::{tst_receiver_close, tst_receiver_open_listener};
use tstrans::rist::{
    tst_rist_receiver_close, tst_rist_recv_open, tst_rist_sender_close, tst_rist_sender_open,
};
use tstrans::rtp::receiver::{tst_rtp_receiver_close, tst_rtp_recv_open};
use tstrans::rtp::sender::{tst_rtp_sender_close, tst_rtp_sender_open};
use tstrans::sender::ts_sender::{tst_sender_close, tst_sender_open};
use tstrans::tcp::{
    tst_tcp_listener_free, tst_tcp_listener_from_url, tst_tcp_sender_close, tst_tcp_sender_open,
};
use tstrans::udp::{
    tst_udp_receiver_close, tst_udp_recv_open, tst_udp_sender_close, tst_udp_sender_open,
};
use tstrans::{tst_rtsp_client_builder_free, tst_rtsp_client_builder_new};

/// The loopback address of an srt row's URL, when the URL parses. The peer
/// goes up whatever the record says: should C accept a URL the record calls
/// invalid, the open still returns (and the mismatch is reported) instead of
/// waiting in accept forever.
fn peer_addr(row: &Row) -> Option<String> {
    let u = tst_srt::SrtUrl::parse(&row.url).ok()?;
    let host = if u.host.is_empty() {
        "127.0.0.1"
    } else {
        u.host.as_str()
    };
    Some(if host.contains(':') {
        format!("[{host}]:{}", u.port)
    } else {
        format!("{host}:{}", u.port)
    })
}

/// The last-error code after an open returned null (or a negative status).
fn last_code() -> Option<i32> {
    Some(unsafe { tst_get_last_error() })
}

/// Run an open that returns a handle, close the handle, and report the
/// outcome: `None` when it returned one.
fn open_close<T>(open: impl FnOnce() -> *mut T, close: impl FnOnce(*mut T)) -> Option<i32> {
    unsafe { tst_clear_last_error() };
    let h = open();
    if h.is_null() {
        last_code()
    } else {
        close(h);
        None
    }
}

/// Open `row`'s URL through `entry`; `None` when the open returned.
fn probe(entry: &str, row: &Row) -> Option<i32> {
    let c_url = CString::new(row.url.as_str()).expect("corpus URLs hold no NUL");
    let u = c_url.as_ptr();
    unsafe {
        match entry {
            // An SRT caller would wait out its connect timeout against a
            // closed port, so the URL's address gets a loopback listener.
            // The peer sets no options: a handshake it rejects (a passphrase
            // it lacks) still returns at once, as a connect failure.
            "tst_sender_open" => {
                let _peer = peer_addr(row).map(|addr| {
                    SrtListenerBuilder::new()
                        .bind(addr)
                        .unwrap_or_else(|e| panic!("{}: peer listener: {e}", row.label()))
                });
                let cfg = tst_sender_config_new();
                let r = open_close(|| tst_sender_open(u, cfg), |h| tst_sender_close(h));
                tst_sender_config_free(cfg);
                r
            }
            // The listener's open blocks in accept, so the URL's address gets
            // a loopback caller; its handshake retries until the bind lands.
            "tst_receiver_open_listener" => {
                let dial = peer_addr(row).map(|addr| {
                    std::thread::spawn(move || {
                        SrtSocketBuilder::new()
                            .connect_timeout(Duration::from_secs(5))
                            .connect(addr)
                    })
                });
                let r = open_close(|| tst_receiver_open_listener(u), |h| tst_receiver_close(h));
                if let Some(dial) = dial {
                    let _ = dial.join().expect("peer caller thread");
                }
                r
            }
            "tst_rtp_sender_open" => {
                open_close(|| tst_rtp_sender_open(u), |h| tst_rtp_sender_close(h))
            }
            "tst_rtp_recv_open" => {
                open_close(|| tst_rtp_recv_open(u), |h| tst_rtp_receiver_close(h))
            }
            "tst_rtsp_client_builder_new" => open_close(
                || tst_rtsp_client_builder_new(u),
                |h| tst_rtsp_client_builder_free(h),
            ),
            "tst_udp_sender_open" => {
                open_close(|| tst_udp_sender_open(u), |h| tst_udp_sender_close(h))
            }
            "tst_udp_recv_open" => {
                open_close(|| tst_udp_recv_open(u), |h| tst_udp_receiver_close(h))
            }
            "tst_tcp_sender_open" => {
                open_close(|| tst_tcp_sender_open(u), |h| tst_tcp_sender_close(h))
            }
            "tst_tcp_listener_from_url" => open_close(
                || tst_tcp_listener_from_url(u),
                |h| tst_tcp_listener_free(h),
            ),
            "tst_hls_publisher_builder_from_url" => {
                tst_clear_last_error();
                let b = tst_hls_publisher_builder_new();
                assert!(!b.is_null(), "tst_hls_publisher_builder_new returned null");
                let rc = tst_hls_publisher_builder_from_url(b, u);
                tst_hls_publisher_builder_free(b);
                (rc != 0).then_some(rc)
            }
            "tst_rist_sender_open" => {
                open_close(|| tst_rist_sender_open(u), |h| tst_rist_sender_close(h))
            }
            "tst_rist_recv_open" => {
                open_close(|| tst_rist_recv_open(u), |h| tst_rist_receiver_close(h))
            }
            other => panic!("{}: the C leg has no entry point {other:?}", row.label()),
        }
    }
}

#[test]
fn url_corpus_rows_match_their_records_through_c() {
    let corpus = url::load();
    let mut mismatches = Vec::new();
    let mut absent = 0;
    for row in &corpus.url {
        let (entry, _probe) = corpus.entry("c", row);
        if entry == "absent" {
            absent += 1;
            continue;
        }
        let observed = probe(entry, row);
        if let Err(why) = url::check_probe_code(&corpus, row, observed) {
            mismatches.push(format!(
                "{} via {entry}: {why}: {}",
                row.label(),
                common::last_error_msg()
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} URL corpus rows ({absent} have no C entry point) differ through C:\n{}",
        mismatches.len(),
        corpus.url.len(),
        mismatches.join("\n")
    );
}
