//! Verifies connect walks past an address that never answers, within one
//! connect budget shared across the resolved addresses.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};
use tst_srt::{SrtConnectError, SrtSocketBuilder};

/// A bound but silent UDP socket: a handshake sent to it gets no reply, so
/// a connect to it waits out its whole timeout.
fn silent_socket() -> (UdpSocket, SocketAddr) {
    let silent = UdpSocket::bind("127.0.0.1:0").expect("bind silent socket");
    let addr = silent.local_addr().expect("silent addr");
    (silent, addr)
}

#[test]
fn connect_walks_to_v4_when_v6_first_and_unbindable() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let live: SocketAddr = ([127, 0, 0, 1], lb.port).into();

    // A bound but silent UDP socket: the handshake to it gets no reply, so
    // the walk must time out on it and move to the live address.
    let silent = UdpSocket::bind("127.0.0.1:0").expect("bind silent socket");
    let dead = silent.local_addr().expect("silent addr");

    let accept = lb.spawn_accept(|sock| sock);
    accept.wait_ready();

    // libsrt checks the connect deadline once a second, so a 500 ms budget
    // is spent on the silent address and never reaches the live one.
    let socket = SrtSocketBuilder::new()
        .connect_timeout(Duration::from_secs(3))
        .send_timeout(Duration::from_secs(5))
        .connect(&[dead, live][..])
        .expect("connect should walk past the silent address to the live one");

    assert_eq!(socket.peer_addr().unwrap(), live);
    let _ = accept.join();
    drop(socket);
    drop(silent);
}

#[test]
fn dead_address_gets_only_its_share_of_the_connect_budget() {
    require_loopback!();
    let lb = crate::common::Loopback::bind();
    let live: SocketAddr = ([127, 0, 0, 1], lb.port).into();
    let (silent, dead) = silent_socket();

    let accept = lb.spawn_accept(|sock| sock);
    accept.wait_ready();

    let budget = Duration::from_secs(4);
    let started = Instant::now();
    let socket = SrtSocketBuilder::new()
        .connect_timeout(budget)
        .send_timeout(Duration::from_secs(5))
        .connect(&[dead, live][..])
        .expect("connect should walk past the silent address to the live one");
    let elapsed = started.elapsed();

    assert_eq!(socket.peer_addr().unwrap(), live);
    // The silent address gets half the budget (two addresses left), 2 s,
    // which libsrt spends as two one-second slices, so the whole connect
    // ends inside the budget. Unscaled on purpose: a connect that gave the
    // silent address the whole budget takes at least the budget, and the
    // live share is the margin for the loopback handshake.
    assert!(
        elapsed < budget,
        "the whole connect must end within its 4 s budget; elapsed={elapsed:?}",
    );
    // The silent address was dialled and waited out its 2 s share (two 1 s
    // slices), then the live address connected.
    assert!(
        elapsed >= Duration::from_millis(1800),
        "the silent address should have been given half the 4 s budget (two \
         1 s slices); elapsed={elapsed:?}",
    );
    let _ = accept.join();
    drop(socket);
    drop(silent);
}

#[test]
fn walk_over_silent_addresses_fails_within_one_budget() {
    require_loopback!();
    let (silent_a, a) = silent_socket();
    let (silent_b, b) = silent_socket();
    let (silent_c, c) = silent_socket();

    let budget = Duration::from_millis(1500);
    let started = Instant::now();
    let result = SrtSocketBuilder::new()
        .connect_timeout(budget)
        .connect(&[a, b, c][..]);
    let elapsed = started.elapsed();

    match result {
        Err(SrtConnectError::TimedOut) => {}
        other => panic!("expected TimedOut, got {other:?}"),
    }
    // The first address gets a third of the budget and each later one an
    // even split of what is left, but libsrt checks the connect deadline
    // once a second, so the last attempt can run up to a second past the
    // budget. Unscaled on purpose: the wrong behaviour (a full budget per
    // address) takes at least 4.5 s, so the bound is the budget plus that
    // second plus 1.5 s of scheduling slack.
    assert!(
        elapsed < budget + Duration::from_secs(1) + Duration::from_millis(1500),
        "three silent addresses must share one 1.5 s budget (plus libsrt's \
         one-second deadline check and scheduling slack); elapsed={elapsed:?}",
    );
    assert!(
        elapsed >= Duration::from_millis(1200),
        "at least two silent addresses should have been dialled; elapsed={elapsed:?}",
    );
    drop(silent_a);
    drop(silent_b);
    drop(silent_c);
}

#[test]
fn single_silent_address_gets_the_whole_budget() {
    require_loopback!();
    let (silent, dead) = silent_socket();

    let budget = Duration::from_secs(1);
    let started = Instant::now();
    let result = SrtSocketBuilder::new()
        .connect_timeout(budget)
        .connect(dead);
    let elapsed = started.elapsed();

    match result {
        Err(SrtConnectError::TimedOut) => {}
        other => panic!("expected TimedOut, got {other:?}"),
    }
    assert!(
        elapsed >= Duration::from_millis(900),
        "a single address should wait out the whole 1 s budget; elapsed={elapsed:?}",
    );
    drop(silent);
}

#[test]
fn walk_dials_the_first_address_even_with_no_budget_left() {
    require_loopback!();
    let (silent_a, a) = silent_socket();
    let (silent_b, b) = silent_socket();

    let started = Instant::now();
    let result = SrtSocketBuilder::new()
        .connect_timeout(Duration::ZERO)
        .connect(&[a, b][..]);
    let elapsed = started.elapsed();

    match result {
        Err(SrtConnectError::TimedOut) => {}
        other => panic!("expected TimedOut, got {other:?}"),
    }
    // libsrt checks a blocking connect's deadline once a second, so a dial
    // that happened costs about a second even with nothing left to spend.
    assert!(
        elapsed >= Duration::from_millis(800),
        "the first address must be dialled even with a spent budget; elapsed={elapsed:?}",
    );
    // A second dial would cost at least two more one-second slices (about
    // 4 s), so 3.5 s still proves the second address was not dialled while
    // leaving 1.5 s for slow runners.
    assert!(
        elapsed < Duration::from_millis(3500),
        "a spent budget must not dial the second address (a second dial would \
         cost at least two more one-second slices, about 4 s); elapsed={elapsed:?}",
    );
    // Direct evidence: the first silent socket received the handshake, the
    // second received nothing.
    let mut buf = [0u8; 1500];
    silent_a
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("read timeout a");
    silent_b
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("read timeout b");
    assert!(
        silent_a.recv_from(&mut buf).is_ok(),
        "the first address must have received the handshake datagram",
    );
    let second = silent_b.recv_from(&mut buf);
    assert!(
        matches!(&second, Err(e) if matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )),
        "the second address must receive nothing; got {second:?}",
    );
    drop(silent_a);
    drop(silent_b);
}
