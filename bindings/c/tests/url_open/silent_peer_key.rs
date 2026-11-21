//! C-ABI tests for the `x-silentpeertimeout` URL key: a managed sender opened
//! from a URL takes it, every other SRT open refuses it as `TST_E_CONFIG_INVALID`
//! with the key named in the message.
//!
//! Nothing a C caller can read tells an armed sender from an unarmed one
//! without a multi-second live trip (the managed stats count reconnects, and
//! an armed sender that is healthy reconnects no earlier than an unarmed
//! one), so the open test asserts what a caller sees: the open succeeds, one
//! send arrives and no reconnect was counted. That the window trips is
//! pinned at the Rust level in `tst-srt`.

use std::ffi::CString;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tst_srt::SrtListenerBuilder;
use tstrans::config::{tst_raw_sender_config_free, tst_raw_sender_config_new};
use tstrans::error::{TstError, tst_get_last_error};
use tstrans::receiver::ts_receiver::{
    tst_managed_receiver_open, tst_managed_receiver_open_listener, tst_receiver_open,
};
use tstrans::sender::raw_sender::{
    tst_managed_raw_sender_close, tst_managed_raw_sender_get_reconnect_stats,
    tst_managed_raw_sender_open, tst_managed_raw_sender_send, tst_raw_sender_open,
};
use tstrans::stats::TstManagedTransportStats;

use super::last_error_msg;

const KEY: &str = "x-silentpeertimeout";

/// The calling thread's last error must be CONFIG_INVALID and name the key.
fn assert_config_invalid_naming_the_key(what: &str) {
    let code = unsafe { tst_get_last_error() };
    assert_eq!(
        code,
        TstError::InvalidConfig as i32,
        "{what}: {}",
        last_error_msg()
    );
    let msg = last_error_msg();
    assert!(
        msg.contains(KEY),
        "{what}: message does not name the key: {msg}"
    );
}

#[test]
fn managed_sender_open_takes_the_key_and_one_send_arrives() {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let listener_thread = thread::spawn(move || {
        let mut listener = SrtListenerBuilder::new()
            .recv_timeout(Duration::from_secs(10))
            .bind("127.0.0.1:0")
            .expect("bind");
        port_tx
            .send(listener.local_addr().expect("local_addr").port())
            .expect("send port");
        let (mut accepted, _peer) = listener.accept().expect("accept");
        let mut buf = [0u8; 1500];
        let n = accepted.recv(&mut buf).expect("recv");
        buf[..n].to_vec()
    });
    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("listener did not bind in time");
    let url = CString::new(format!("srt://127.0.0.1:{port}?{KEY}=1500")).unwrap();

    unsafe {
        let cfg = tst_raw_sender_config_new();
        let s = tst_managed_raw_sender_open(url.as_ptr(), cfg, std::ptr::null());
        assert!(!s.is_null(), "open failed: {}", last_error_msg());
        let rc = tst_managed_raw_sender_send(s, b"armed".as_ptr(), 5);
        assert_eq!(rc, 0, "send: {}", last_error_msg());

        let received = listener_thread.join().expect("listener thread panicked");
        assert_eq!(received, b"armed");

        let mut st = std::mem::zeroed::<TstManagedTransportStats>();
        assert_eq!(tst_managed_raw_sender_get_reconnect_stats(s, &mut st), 0);
        assert_eq!(
            st.reconnect_attempts, 0,
            "a healthy send must not reconnect"
        );

        tst_managed_raw_sender_close(s);
        tst_raw_sender_config_free(cfg);
    }
}

#[test]
fn managed_sender_open_refuses_a_window_below_the_floor() {
    // Refused while the URL is parsed: nothing listens on the port.
    let url = CString::new(format!("srt://127.0.0.1:9?{KEY}=999")).unwrap();
    unsafe {
        let cfg = tst_raw_sender_config_new();
        let s = tst_managed_raw_sender_open(url.as_ptr(), cfg, std::ptr::null());
        assert!(s.is_null(), "a window under 1000 ms must be refused");
        assert_config_invalid_naming_the_key("managed sender, 999");
        tst_raw_sender_config_free(cfg);
    }
}

#[test]
fn a_plain_sender_open_refuses_the_key() {
    let url = CString::new(format!("srt://127.0.0.1:9?{KEY}=1500")).unwrap();
    unsafe {
        let cfg = tst_raw_sender_config_new();
        let s = tst_raw_sender_open(url.as_ptr(), cfg);
        assert!(s.is_null(), "a plain sender must refuse the key");
        assert_config_invalid_naming_the_key("plain sender");
        tst_raw_sender_config_free(cfg);
    }
}

#[test]
fn receiver_opens_refuse_the_key() {
    let caller = CString::new(format!("srt://127.0.0.1:9?conntimeo=500&{KEY}=1500")).unwrap();
    let listener = CString::new(format!("srt://127.0.0.1:0?mode=listener&{KEY}=1500")).unwrap();
    unsafe {
        let r = tst_managed_receiver_open(caller.as_ptr(), std::ptr::null());
        assert!(r.is_null(), "a managed receiver must refuse the key");
        assert_config_invalid_naming_the_key("managed receiver");

        let r = tst_managed_receiver_open_listener(listener.as_ptr(), std::ptr::null());
        assert!(
            r.is_null(),
            "a managed listener receiver must refuse the key"
        );
        assert_config_invalid_naming_the_key("managed listener receiver");

        let r = tst_receiver_open(caller.as_ptr());
        assert!(r.is_null(), "a plain receiver must refuse the key");
        assert_config_invalid_naming_the_key("plain receiver");
    }
}
