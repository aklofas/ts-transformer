//! An unset key length leaves SRTO_PBKEYLEN to libsrt's handshake
//! negotiation instead of forcing AES-128.
#![cfg(feature = "mbedtls")]

use std::time::Duration;
use tst_srt::{KeyLength, Passphrase, Role, SrtListenerBuilder, SrtSocket, SrtSocketBuilder};

const PASSPHRASE: &str = "secretsecretsecret";

fn passphrase() -> Passphrase {
    Passphrase::new(PASSPHRASE).unwrap()
}

/// Connect `caller` to a listener built from `listener`, returning the
/// caller socket and the accepted socket.
fn connect_pair(
    listener: &mut SrtListenerBuilder,
    caller: &mut SrtSocketBuilder,
) -> (SrtSocket, SrtSocket) {
    let lb = crate::common::Loopback::bind_with(std::mem::take(listener));
    let port = lb.port;
    let accept = lb.spawn_accept(|sock| sock);
    accept.wait_ready();
    let socket = caller
        .send_timeout(Duration::from_secs(5))
        .connect(("127.0.0.1", port))
        .expect("connect");
    (socket, accept.join())
}

/// libsrt core.cpp checkUpdateCryptoKeyLen: an agent with PBKEYLEN 0 adopts
/// the peer's advertised length. A passphrase-only sending caller must
/// therefore end on the listener's 32, not force 16.
#[test]
fn passphrase_only_sender_adopts_listener_aes256() {
    require_loopback!();
    let mut l = SrtListenerBuilder::new();
    l.passphrase(passphrase()).key_length(KeyLength::Aes256);
    let mut c = SrtSocketBuilder::new();
    c.passphrase(passphrase()).role(Role::Sender);
    let (caller, accepted) = connect_pair(&mut l, &mut c);
    assert_eq!(crate::common::pbkeylen(caller.raw_handle()), 32);
    assert_eq!(crate::common::pbkeylen(accepted.raw_handle()), 32);
}

/// The reverse: a caller asking for 32 (receiver role) against a
/// passphrase-only listener keeps 32.
#[test]
fn explicit_aes256_caller_keeps_32_against_passphrase_only_listener() {
    require_loopback!();
    let mut l = SrtListenerBuilder::new();
    l.passphrase(passphrase());
    let mut c = SrtSocketBuilder::new();
    c.passphrase(passphrase()).key_length(KeyLength::Aes256);
    let (caller, accepted) = connect_pair(&mut l, &mut c);
    assert_eq!(crate::common::pbkeylen(caller.raw_handle()), 32);
    assert_eq!(crate::common::pbkeylen(accepted.raw_handle()), 32);
}

/// Both ends passphrase-only still connect, on AES-128.
#[test]
fn passphrase_only_both_ends_connect_on_aes128() {
    require_loopback!();
    let mut l = SrtListenerBuilder::new();
    l.passphrase(passphrase());
    let mut c = SrtSocketBuilder::new();
    c.passphrase(passphrase()).role(Role::Sender);
    let (mut caller, mut accepted) = connect_pair(&mut l, &mut c);
    assert_eq!(crate::common::pbkeylen(caller.raw_handle()), 16);
    assert_eq!(crate::common::pbkeylen(accepted.raw_handle()), 16);

    accepted
        .set_recv_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let payload = [0x47u8; 188];
    assert_eq!(caller.send(&payload).expect("send"), 188);
    let mut buf = [0u8; 1500];
    let n = accepted.recv(&mut buf).expect("recv");
    assert_eq!(&buf[..n], &payload[..]);
}
