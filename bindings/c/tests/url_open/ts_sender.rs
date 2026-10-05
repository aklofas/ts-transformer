//! C-ABI URL parsing tests for `tst_sender_*` (TS-bytes sender) plus the
//! URL-parser error paths and atomicity invariants, which all use
//! `tst_sender_open` as their probe.
//!
//! Test groups (in source order):
//!   1. streamid roundtrip
//!   2. latency negotiation
//!   3. local-side options (mss, payloadsize, lossmaxttl, fc)
//!   4. passphrase + pbkeylen handshake (encryption-gated)
//!   5. latency-family keys (rcvlatency, peerlatency)
//!   6. bandwidth-shaping keys (maxbw, inputbw, oheadbw)
//!   7. tlpktdrop, packetfilter, congestion
//!   8. ts-transformer extension keys (x-recvtimeout, x-sendtimeout)
//!   9. variant_ts_sender + variant_managed_ts_sender open-with-URL
//!  10. conflict precedence: URL wins over builder defaults
//!  11. malformed / unknown / unsupported / userinfo URL rejection
//!  12. cfg byte-unchanged after failed parse (atomicity)
//!  13. managed_ts_sender URL options persist across reconnect

use std::ffi::CString;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tst_srt::ListenerBuilder;
use tstrans::config::{tst_sender_config_free, tst_sender_config_new};
use tstrans::error::TstError;
use tstrans::sender::ts_sender::{
    tst_managed_sender_close, tst_managed_sender_get_reconnect_stats, tst_managed_sender_open,
    tst_managed_sender_send_ts, tst_sender_close, tst_sender_open,
};
use tstrans::stats::TstManagedTransportStats;

use super::last_error_msg;

// ============================================================================
// Group 1 streamid roundtrip
// ============================================================================

#[test]
fn ts_sender_streamid_observed_on_listener() {
    // Listener thread binds to :0, sends back the assigned port, then
    // accepts and sends back the negotiated stream_id.
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (sid_tx, sid_rx) = mpsc::channel::<Option<String>>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        let (accepted, _peer) = listener.accept().expect("accept");
        sid_tx
            .send(accepted.stream_id().map(str::to_string))
            .expect("send stream_id");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?streamid=front-camera")).unwrap();

    // All C-ABI pointers live on the main thread.
    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let observed = sid_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("listener did not send stream_id in time");
        assert_eq!(
            observed.as_deref(),
            Some("front-camera"),
            "expected stream_id 'front-camera', got {:?}",
            observed
        );

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

// ============================================================================
// Group 1 latency roundtrip — verify connect succeeds with latency=200.
//
// Socket::latency() does not exist in the current tst-srt public API; the
// negotiated value is only observable by calling srt_getsockflag directly on
// the accepted socket handle, which is not exposed. We verify end-to-end that
// the URL parameter is accepted by the parser + libsrt and the handshake
// completes — the unit-test layer (url_params.rs) covers typed overlay shape;
// here we cover "URL → libsrt option set → connection opens".
// ============================================================================

#[test]
fn ts_sender_latency_negotiated() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        let result = listener.accept();
        ok_tx.send(result.is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?latency=200")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("listener did not report accept status in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

// ============================================================================
// Group 1 — local-side options (mss, payloadsize, lossmaxttl, fc).
//
// These options are set on the caller socket before connect and do not
// require peer-observable verification. Each test verifies that the URL
// parameter passes through the parser + setsockopt + handshake without
// error (i.e. the connection opens successfully). The unit-test layer
// verifies typed overlay field population; this layer is end-to-end.
// ============================================================================

#[test]
fn ts_sender_mss_url_open_succeeds() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?mss=1316")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

#[test]
fn ts_sender_payloadsize_url_open_succeeds() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?payloadsize=1316")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

#[test]
fn ts_sender_lossmaxttl_url_open_succeeds() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?lossmaxttl=20")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

#[test]
fn ts_sender_fc_url_open_succeeds() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?fc=8192")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

// ============================================================================
// Group 1 passphrase + pbkeylen — listener uses the same passphrase so the
// encrypted handshake succeeds. The strongest test in this batch: exercises
// full AES-128 key exchange rather than just option forwarding.
// ============================================================================

// Encryption-gated: with --no-default-features, tst-srt/srt-sys build libsrt
// with ENABLE_ENCRYPTION=OFF, and SRTO_PBKEYLEN / SRTO_PASSPHRASE on the
// listener fail at bind time with "encryption not enabled at compile time".
// The URL parser still accepts these keys; they just can't actually negotiate
// against an encryption-disabled listener.
#[cfg(feature = "mbedtls")]
#[test]
fn ts_sender_passphrase_handshake_ok() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        use tst_srt::Passphrase;
        let mut listener = ListenerBuilder::new()
            .passphrase(Passphrase::new("hunter-too-long-thanks").unwrap())
            .key_length(tst_srt::KeyLength::Aes128)
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!(
        "srt://127.0.0.1:{port}?passphrase=hunter-too-long-thanks&pbkeylen=16"
    ))
    .unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

// ============================================================================
// Group 1 — latency-family keys (rcvlatency, peerlatency).
//
// Both are pre-connect options the caller sets on its own socket; peer
// negotiates the effective receive buffer delay. We verify the URL param
// passes through the parser + setsockopt + handshake without error.
// ============================================================================

#[test]
fn ts_sender_rcvlatency_url_open_succeeds() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?rcvlatency=120")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

#[test]
fn ts_sender_peerlatency_url_open_succeeds() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?peerlatency=80")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

// ============================================================================
// Group 1 — bandwidth-shaping keys (maxbw, inputbw, oheadbw).
//
// All three are caller-side-only options controlling the sender's packet
// scheduling; they don't appear in the SRT handshake extension fields.
// We verify they survive the URL parse → setsockopt → connect sequence.
// ============================================================================

#[test]
fn ts_sender_maxbw_url_open_succeeds() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?maxbw=10000000")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

#[test]
fn ts_sender_inputbw_url_open_succeeds() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?inputbw=5000000")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

#[test]
fn ts_sender_oheadbw_url_open_succeeds() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?oheadbw=25")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

// ============================================================================
// Group 1 — tlpktdrop (too-late packet drop).
//
// Caller-side option; controls whether the sender drops packets that have
// been waiting beyond the latency budget. Verify URL parse + connect succeeds.
// ============================================================================

#[test]
fn ts_sender_tlpktdrop_url_open_succeeds() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?tlpktdrop=1")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

// ============================================================================
// Group 1 — packetfilter (FEC).
//
// Both sides must agree on the filter configuration for the handshake to
// succeed — the listener is pre-configured with the same FEC spec string so
// libsrt's filter-negotiation exchange passes. Verifies that the URL parser
// correctly forwards the packetfilter string to SRTO_PACKETFILTER.
// ============================================================================

#[test]
fn ts_sender_packetfilter_url_open_succeeds() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        use tst_srt::PacketFilter;
        let mut listener = ListenerBuilder::new()
            .packet_filter(PacketFilter::new("fec,cols:10,rows:5,arq:onreq").unwrap())
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!(
        "srt://127.0.0.1:{port}?packetfilter=fec,cols:10,rows:5,arq:onreq"
    ))
    .unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

// ============================================================================
// Per-sender-variant roundtrip — confirms URL plumbing works in the ts_sender
// + managed_ts_sender entry points.
// ============================================================================

#[test]
fn variant_ts_sender_open_with_url() {
    // ts_sender_streamid_observed_on_listener in Group 1 already exercises
    // this entry point; this test is included here for completeness alongside
    // the other five variants.
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (sid_tx, sid_rx) = mpsc::channel::<Option<String>>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        let (accepted, _peer) = listener.accept().expect("accept");
        sid_tx
            .send(accepted.stream_id().map(str::to_string))
            .expect("send stream_id");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?streamid=ts-plain")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let observed = sid_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("listener did not send stream_id in time");
        assert_eq!(
            observed.as_deref(),
            Some("ts-plain"),
            "expected stream_id 'ts-plain', got {:?}",
            observed
        );

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

#[test]
fn variant_managed_ts_sender_open_with_url() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (sid_tx, sid_rx) = mpsc::channel::<Option<String>>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        let (accepted, _peer) = listener.accept().expect("accept");
        sid_tx
            .send(accepted.stream_id().map(str::to_string))
            .expect("send stream_id");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?streamid=ts-managed")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_managed_sender_open(url.as_ptr(), cfg, std::ptr::null());
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let observed = sid_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("listener did not send stream_id in time");
        assert_eq!(
            observed.as_deref(),
            Some("ts-managed"),
            "expected stream_id 'ts-managed', got {:?}",
            observed
        );

        tst_managed_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

#[test]
fn variant_managed_sender_get_reconnect_stats() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_managed_sender_open(url.as_ptr(), cfg, std::ptr::null());
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        // Freshly-opened, no reconnect has ever happened — every counter
        // is zero and the padding is zeroed by the fill path.
        let mut out = TstManagedTransportStats::default();
        let rc = tst_managed_sender_get_reconnect_stats(s, &mut out);
        assert_eq!(rc, 0, "get_reconnect_stats failed: {}", last_error_msg());
        assert_eq!(out.reconnect_attempts, 0);
        assert_eq!(out.reconnect_successes, 0);
        assert_eq!(out.gap_len, 0);
        assert_eq!(out.gap_messages_dropped, 0);
        assert_eq!(out.gap_bytes_dropped, 0);
        assert!(!out.reconnecting);
        assert_eq!(out._pad, [0u8; 7]);

        let rc_null_out = tst_managed_sender_get_reconnect_stats(s, std::ptr::null_mut());
        assert_eq!(rc_null_out, TstError::InvalidConfig as i32);

        tst_managed_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

// ============================================================================
// Group 1 — congestion controller selection.
//
// "live" is the only controller currently supported in our URL overlay.
// The option must be set before connect; verify end-to-end handshake succeeds.
// ============================================================================

#[test]
fn ts_sender_congestion_live_url_open_succeeds() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?congestion=live")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

// ============================================================================
// Group 2 extension keys (x-recvtimeout / x-sendtimeout).
//
// These are ts-transformer extensions — not standard SRT URL keys. They set
// SRTO_RCVTIMEO / SRTO_SNDTIMEO (in milliseconds) before connect. Because
// they control local I/O timeouts rather than SRT protocol behaviour, no
// special listener configuration is needed.
// ============================================================================

#[test]
fn ts_sender_x_recvtimeout_url_open_succeeds() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?x-recvtimeout=5000")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

#[test]
fn ts_sender_x_sendtimeout_url_open_succeeds() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (ok_tx, ok_rx) = mpsc::channel::<bool>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        ok_tx.send(listener.accept().is_ok()).expect("send ok");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?x-sendtimeout=2000")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

// ============================================================================
// Conflict precedence — URL wins over builder defaults.
// ============================================================================

#[test]
fn url_streamid_overrides_builder_streamid() {
    // Today's C ABI for ts_sender has no streamid builder setter — the URL
    // is the only path for setting streamid. This test documents the invariant
    // that the URL-supplied streamid is observed on the listener, so that when
    // a builder setter lands it can be tested for correct precedence.
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (sid_tx, sid_rx) = mpsc::channel::<Option<String>>();

    let listener_thread = thread::spawn(move || {
        let mut listener = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");
        let (accepted, _peer) = listener.accept().expect("accept");
        sid_tx
            .send(accepted.stream_id().map(str::to_string))
            .expect("send stream_id");
    });

    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url_c = CString::new(format!("srt://127.0.0.1:{port}?streamid=url-wins")).unwrap();

    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url_c.as_ptr(), cfg);
        assert!(!s.is_null(), "open failed: {}", last_error_msg());

        let observed = sid_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("listener did not send stream_id in time");
        assert_eq!(
            observed.as_deref(),
            Some("url-wins"),
            "expected stream_id 'url-wins', got {:?}",
            observed
        );

        tst_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}

// ============================================================================
// Malformed URLs return NULL with a useful last-error message.
// ============================================================================

#[test]
fn malformed_url_returns_null() {
    // "not-a-url" has no scheme — url::Url::parse rejects it, producing a
    // UrlError::Syntax. parse_c_srt_url wraps it as "invalid srt url: URL
    // parse failed: ...".
    let url_c = CString::new("not-a-url").unwrap();
    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url_c.as_ptr(), cfg);
        assert!(s.is_null(), "expected null for malformed URL");
        let msg = last_error_msg();
        assert!(msg.contains("invalid srt url"), "msg = {msg}");
        tst_sender_config_free(cfg);
    }
}

#[test]
fn url_unknown_key_returns_null() {
    // "lattency" is a misspelling of "latency" — the parser treats it as an
    // unknown key and returns UrlError::UnknownKey { key: "lattency" }.
    let url_c = CString::new("srt://127.0.0.1:9000?lattency=100").unwrap();
    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url_c.as_ptr(), cfg);
        assert!(s.is_null(), "expected null for unknown key");
        let msg = last_error_msg();
        assert!(msg.contains("unknown URL key"), "msg = {msg}");
        assert!(msg.contains("lattency"), "msg = {msg}");
        tst_sender_config_free(cfg);
    }
}

#[test]
fn url_unsupported_key_returns_null_with_srto() {
    // "transtype" is in the Group 3 reject table — it maps to SRTO_TRANSTYPE
    // but is not yet exposed. The error message names both the URL key and the
    // libsrt option so the caller can find documentation for it.
    let url_c = CString::new("srt://127.0.0.1:9000?transtype=live").unwrap();
    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url_c.as_ptr(), cfg);
        assert!(s.is_null(), "expected null for unsupported key");
        let msg = last_error_msg();
        assert!(msg.contains("transtype"), "msg = {msg}");
        assert!(msg.contains("SRTO_TRANSTYPE"), "msg = {msg}");
        tst_sender_config_free(cfg);
    }
}

#[test]
fn url_mode_listener_parse_accepted() {
    // mode=listener is now a valid URL form — the parser accepts it so that
    // the receiver-side entry points (tst_receiver_open etc.) can use it.
    // Verify that SrtUrl::parse no longer rejects it.
    let u = tst_srt::SrtUrl::parse("srt://127.0.0.1:9000?mode=listener").unwrap();
    assert_eq!(u.mode, tst_srt::url::Mode::Listener);
    assert_eq!(u.host, "127.0.0.1");
    assert_eq!(u.port, 9000);
}

#[test]
fn url_userinfo_returns_null_with_passphrase_hint() {
    // Embedding credentials as user:pass@ in the SRT URL is not supported.
    // The error message directs the caller to use ?passphrase=... instead.
    let url_c = CString::new("srt://op:hunter2@127.0.0.1:9000").unwrap();
    unsafe {
        let cfg = tst_sender_config_new();
        let s = tst_sender_open(url_c.as_ptr(), cfg);
        assert!(s.is_null(), "expected null for userinfo in URL");
        let msg = last_error_msg();
        assert!(
            msg.contains("userinfo") || msg.contains("user:pass"),
            "msg = {msg}"
        );
        assert!(
            msg.contains("passphrase"),
            "hint should mention passphrase: {msg}"
        );
        tst_sender_config_free(cfg);
    }
}

// ============================================================================
// Atomicity — caller's cfg is byte-unchanged after a failed parse.
//
// A failed URL parse must not poison the cfg. The same cfg must be usable for
// a subsequent successful open against a valid URL. This tests the invariant
// that parse errors are all-or-nothing: either the connection opens (options
// applied) or it doesn't (cfg untouched), never a partial-apply.
// ============================================================================

#[test]
fn cfg_byte_unchanged_after_failed_parse() {
    // "transtype=live" is an unsupported Group 3 key — _open returns null and
    // sets last-error without touching the caller's cfg.
    let url_bad = CString::new("srt://127.0.0.1:9000?transtype=live").unwrap();
    unsafe {
        let cfg = tst_sender_config_new();
        let s_bad = tst_sender_open(url_bad.as_ptr(), cfg);
        assert!(s_bad.is_null(), "expected null for unsupported key");

        // Now use the same cfg with a valid URL against a real listener.
        // If cfg were poisoned (e.g. partially applied or nulled out) the
        // second open would fail or crash — this would surface the bug.
        let (port_tx, port_rx) = mpsc::channel::<u16>();
        let (ok_tx, ok_rx) = mpsc::channel::<bool>();
        let listener_thread = thread::spawn(move || {
            let mut listener = ListenerBuilder::new()
                .recv_timeout(Duration::from_secs(5))
                .bind("127.0.0.1:0")
                .expect("bind");
            let port = listener.local_addr().expect("local_addr").port();
            port_tx.send(port).expect("send port");
            ok_tx.send(listener.accept().is_ok()).expect("send ok");
        });

        let port = port_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("listener did not bind in time");
        let url_good = CString::new(format!("srt://127.0.0.1:{port}")).unwrap();

        // All C-ABI pointers stay on the main thread — cfg is reused directly.
        let s = tst_sender_open(url_good.as_ptr(), cfg);
        assert!(
            !s.is_null(),
            "second open should succeed; cfg must not be poisoned by the prior failed parse: {}",
            last_error_msg()
        );

        let accepted_ok = ok_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no accept result in time");
        assert!(accepted_ok, "listener accept() failed");

        tst_sender_close(s);
        tst_sender_config_free(cfg);

        listener_thread.join().expect("listener thread panicked");
    }
}

// ============================================================================
// Managed-reconnect URL persistence.
//
// URL options are parsed once at construction. When the managed transport
// reconnects after a broken connection, the SocketConfig (with streamid)
// captured in the reconnect factory closure must be reused — not re-parsed
// from the URL string.
//
// Listener lifecycle (single background thread, two phases):
//   phase 1 — bind :0, accept the initial connection, validate streamid,
//              send port + sid1 back to main, then drop everything.
//   phase 2 — rebind to the SAME port (200ms after drop), accept the
//              reconnected sender, validate streamid, send sid2 back.
//
// Main thread triggers the reconnect by pushing valid TS data after phase 1
// is done. ManagedTransport is purely reactive: it only detects a broken
// connection (and attempts reconnect) when send_bytes() is called and
// returns Broken. Sending a 7-packet (1316-byte) bundle causes exactly one
// send_bytes() call, which detects the break and enters reconnect_and_drain().
// The reconnect blocks on the main thread until phase 2 listener accepts.
// ============================================================================

/// Build a 7 × 188 = 1316 byte MPEG-TS null-packet bundle (sync byte 0x47
/// at every 188-byte boundary). TsFraming in RECOVER mode acquires sync on
/// three consecutive 0x47 markers and immediately emits the 7-packet bundle.
fn make_ts_bundle() -> Vec<u8> {
    const TS_PACKET_SIZE: usize = 188;
    const BUNDLE_PACKETS: usize = 7;
    let mut buf = vec![0u8; TS_PACKET_SIZE * BUNDLE_PACKETS];
    for i in 0..BUNDLE_PACKETS {
        // Sync byte at the start of each 188-byte packet — all other bytes
        // remain zero (null PID 0x0000 payload, which is not a reserved PID
        // but is harmless for sync-acquisition purposes in RECOVER mode).
        buf[i * TS_PACKET_SIZE] = 0x47;
    }
    buf
}

#[test]
fn managed_ts_sender_url_options_persist_across_reconnect() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    // phase 1 result: streamid observed on first accept.
    let (sid1_tx, sid1_rx) = mpsc::channel::<Option<String>>();
    // phase 2 result: streamid observed on reconnect accept.
    let (sid2_tx, sid2_rx) = mpsc::channel::<Option<String>>();

    let listener_thread = thread::spawn(move || {
        // phase 1: bind to :0, learn the port, accept the initial connection.
        let mut listener1 = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(5))
            .bind("127.0.0.1:0")
            .expect("bind phase 1");
        let port = listener1.local_addr().expect("local_addr").port();
        port_tx.send(port).expect("send port");

        let (accepted1, _) = listener1.accept().expect("phase 1 accept");
        sid1_tx
            .send(accepted1.stream_id().map(str::to_string))
            .expect("send sid1");

        // Drop the accepted socket and listener to sever the connection.
        // The main thread will then call send_ts, which detects the break
        // and enters the reconnect loop.
        drop(accepted1);
        drop(listener1);

        // Brief pause so the UDP socket is fully released before rebinding.
        // SRT uses UDP; the kernel frees the port almost immediately on drop,
        // but 200ms avoids a tight race with the phase 2 bind below.
        thread::sleep(Duration::from_millis(200));

        // phase 2: rebind to the SAME port so the managed sender's reconnect
        // attempt reaches us. Allow up to 10s for the reconnect to complete.
        let mut listener2 = ListenerBuilder::new()
            .recv_timeout(Duration::from_secs(10))
            .bind(format!("127.0.0.1:{port}"))
            .expect("bind phase 2");

        let result = listener2.accept();
        sid2_tx
            .send(
                result
                    .ok()
                    .and_then(|(s, _)| s.stream_id().map(str::to_string)),
            )
            .expect("send sid2");
    });

    // Receive the port before opening the sender so the URL points at the
    // port listener phase 1 is already blocking on.
    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");

    let url_c = CString::new(format!("srt://127.0.0.1:{port}?streamid=persistent")).unwrap();

    // All C-ABI pointers live on the main thread.
    unsafe {
        let cfg = tst_sender_config_new();
        // null policy → default: exponential backoff 100ms..=10s, max 10 attempts.
        let s = tst_managed_sender_open(url_c.as_ptr(), cfg, std::ptr::null());
        assert!(!s.is_null(), "managed open failed: {}", last_error_msg());

        // Wait for phase 1 to complete — listener has accepted and dropped.
        let sid1 = sid1_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("phase 1 sid not received");
        assert_eq!(
            sid1.as_deref(),
            Some("persistent"),
            "phase 1: unexpected streamid {:?}",
            sid1
        );

        // The listener dropped, but ManagedTransport only detects a broken
        // connection when send_bytes() is called (it is purely reactive).
        // Poll: send TS bundles repeatedly until either:
        //   (a) a send returns Broken, triggering reconnect_and_drain() which
        //       blocks on this thread until phase 2 listener accepts, then
        //       the factory closure reuses the captured SocketConfig (with
        //       streamid); OR
        //   (b) sid2_rx already has data (reconnect happened during a send
        //       that appeared successful from this side).
        // SRT's broken-connection detection is not instantaneous — the peer
        // timeout typically surfaces within a few seconds of the drop.
        let ts_bundle = make_ts_bundle();
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        let sid2 = loop {
            // Each call either succeeds (SRT still buffering) or triggers the
            // reconnect path (Broken detected) — both move us toward phase 2.
            let _ = tst_managed_sender_send_ts(s, ts_bundle.as_ptr(), ts_bundle.len());

            // Check if the reconnect completed and phase 2 listener reported.
            match sid2_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(sid) => break sid,
                Err(_) if std::time::Instant::now() < deadline => continue,
                Err(_) => panic!("managed transport did not reconnect within 12s"),
            }
        };

        assert_eq!(
            sid2.as_deref(),
            Some("persistent"),
            "reconnect lost the URL-derived streamid: got {:?}",
            sid2
        );

        tst_managed_sender_close(s);
        tst_sender_config_free(cfg);
    }

    listener_thread.join().expect("listener thread panicked");
}
