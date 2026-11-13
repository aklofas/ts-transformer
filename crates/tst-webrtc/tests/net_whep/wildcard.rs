//! A server bound to the wildcard address with no `advertise=`: each
//! viewer is offered the local address its signalling connection arrived
//! on, and loopback, so the server needs no knowledge of the host's
//! addresses.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use tst_core::mpegts::common::Pts90khz;
use tst_test_helpers::capability::skip_unless_required;
use tst_webrtc::{MountConfig, MountHandle, WhepServer};

use crate::fixtures::{TestViewer, connect_all, h264_gop, pump_all, whep_post};

const DELIVER: Duration = Duration::from_secs(10);

/// The addresses of the answer's `a=candidate` lines.
fn candidate_ips(answer: &str) -> Vec<IpAddr> {
    answer
        .lines()
        .filter_map(|l| l.strip_prefix("a=candidate:"))
        .map(|l| {
            l.split(' ')
                .nth(4)
                .and_then(|ip| ip.parse().ok())
                .unwrap_or_else(|| panic!("candidate line without an address: {l}"))
        })
        .collect()
}

/// POSTs `v`'s offer to the server's signalling port on `ip`, applies the
/// answer and returns the addresses it offers.
fn join_on(srv: &WhepServer, ip: IpAddr, v: &mut TestViewer) -> Vec<IpAddr> {
    let offer = v.offer(false);
    let to = SocketAddr::new(ip, srv.local_addr().port());
    let (status, _, answer) = whep_post(to, "/cam", &offer, None);
    assert_eq!(status, 201, "offer refused: {answer}");
    v.accept_answer(&answer);
    candidate_ips(&answer)
}

/// Pushes the fixture GOP, with timestamps starting at frame `first`, and
/// pumps until each viewer has received video.
fn play(m: &MountHandle, first: i64, viewers: &mut [TestViewer]) {
    for (i, (au, key)) in h264_gop().iter().enumerate() {
        let pts = Pts90khz::new((first + i as i64) * 3000);
        m.push_video(au, pts, *key).unwrap();
    }
    let got = pump_all(
        viewers,
        |vs| vs.iter().all(|v| !v.frames.is_empty()),
        DELIVER,
    );
    let traces: Vec<String> = viewers.iter().map(TestViewer::trace).collect();
    assert!(got, "a viewer received no video: {traces:?}");
}

/// A non-loopback IPv4 address of this host: the source address the
/// routing table picks for a documentation address (RFC 5737). No packet
/// is sent.
fn lan_ipv4() -> Result<Ipv4Addr, String> {
    let probe = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| e.to_string())?;
    probe
        .connect((Ipv4Addr::new(192, 0, 2, 1), 9))
        .map_err(|e| format!("no IPv4 route: {e}"))?;
    match probe.local_addr().map_err(|e| e.to_string())?.ip() {
        // An ICE candidate cannot be a link-local address.
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() && !ip.is_link_local() => {
            Ok(ip)
        }
        other => Err(format!("the route's source address is {other}")),
    }
}

#[test]
fn a_wildcard_bind_still_serves_a_loopback_viewer_on_loopback() {
    let srv = WhepServer::bind("whep://0.0.0.0:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut v = [TestViewer::new()];
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    assert_eq!(join_on(&srv, loopback, &mut v[0]), [loopback]);
    connect_all(&m, &mut v);
    play(&m, 0, &mut v);
}

#[test]
fn a_wildcard_bind_with_advertise_offers_the_advertised_and_loopback_addresses() {
    let srv = WhepServer::bind("whep://0.0.0.0:0?advertise=203.0.113.5").unwrap();
    srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut v = TestViewer::new();
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let advertised: IpAddr = "203.0.113.5".parse().unwrap();
    assert_eq!(join_on(&srv, loopback, &mut v), [advertised, loopback]);
}

/// The embedder binds the wildcard address and names no address. A viewer
/// that reached the host on its LAN address is offered that address and
/// plays, at the same time as a loopback viewer. A viewer on the host
/// itself whose page named the LAN address still connects over loopback.
#[test]
fn a_wildcard_bind_offers_each_viewer_the_address_it_reached() {
    let lan = match lan_ipv4() {
        Ok(ip) => IpAddr::V4(ip),
        Err(why) => return skip_unless_required("LAN_ADDR", why),
    };
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let srv = WhepServer::bind("whep://0.0.0.0:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut vs = [
        TestViewer::bound_to(lan),
        TestViewer::new(),
        TestViewer::new(),
    ];
    assert_eq!(join_on(&srv, lan, &mut vs[0]), [lan, loopback]);
    assert_eq!(join_on(&srv, loopback, &mut vs[1]), [loopback]);
    assert_eq!(join_on(&srv, lan, &mut vs[2]), [lan, loopback]);
    connect_all(&m, &mut vs);
    play(&m, 0, &mut vs);
    let port = srv.media_addr().port();
    assert_eq!(
        vs[0].sources,
        HashSet::from([SocketAddr::new(lan, port)]),
        "the LAN viewer's media came from another address"
    );
    assert_eq!(
        vs[2].sources,
        HashSet::from([SocketAddr::new(loopback, port)]),
        "the same-host viewer was not served over loopback"
    );
}

/// Every address of 127.0.0.0/8 is local on Linux, and Debian maps the
/// host's own name to 127.0.1.1. Replies to any of them leave from
/// 127.0.0.1, so that is the address such a viewer is offered.
#[cfg(target_os = "linux")]
#[test]
fn a_viewer_that_reached_another_loopback_address_is_offered_the_loopback_candidate() {
    let srv = WhepServer::bind("whep://0.0.0.0:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut v = [TestViewer::new()];
    let named: IpAddr = "127.0.1.1".parse().unwrap();
    assert_eq!(
        join_on(&srv, named, &mut v[0]),
        [IpAddr::V4(Ipv4Addr::LOCALHOST)]
    );
    connect_all(&m, &mut v);
    play(&m, 0, &mut v);
}

/// `::` serves IPv6 viewers. An IPv4 viewer that reaches a dual-stack `::`
/// socket arrives on an IPv4-mapped address, which the IPv6 media path does
/// not serve: it is offered IPv6 loopback only, as before.
#[test]
fn a_wildcard_ipv6_bind_serves_an_ipv6_viewer_and_offers_an_ipv4_one_nothing_new() {
    let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
    if let Err(e) = UdpSocket::bind((v6, 0)) {
        return skip_unless_required("IPV6", format!("no IPv6 loopback: {e}"));
    }
    let srv = WhepServer::bind("whep://[::]:0").unwrap();
    let m = srv.add_mount("/cam", MountConfig::default()).unwrap();
    let mut v = [TestViewer::bound_to(v6)];
    assert_eq!(join_on(&srv, v6, &mut v[0]), [v6]);
    connect_all(&m, &mut v);
    play(&m, 0, &mut v);

    // Only where the OS makes `::` dual-stack does an IPv4 request arrive.
    let v4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), srv.local_addr().port());
    if std::net::TcpStream::connect(v4).is_ok() {
        let mut w = TestViewer::new();
        assert_eq!(join_on(&srv, v4.ip(), &mut w), [v6]);
    }
}

#[cfg(target_os = "linux")]
mod address_change {
    use std::process::Command;

    use super::*;

    /// Set in the copy of this test that runs inside the network namespace.
    const CHILD: &str = "TST_WHEP_NETNS_CHILD";
    const TEST: &str = "wildcard::address_change::a_wildcard_bind_follows_the_hosts_address";

    fn ip(args: &[&str]) {
        let out = Command::new("ip").args(args).output().expect("run ip");
        assert!(
            out.status.success(),
            "ip {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The scenario, in a namespace whose only address is loopback: the
    /// server binds before the host has an address, serves a viewer on the
    /// address that appears, and after that address is replaced serves a
    /// new viewer on the new one. Nothing is called on the server between.
    /// The addresses go on the loopback interface, which needs no kernel
    /// module; to the sockets they are ordinary local addresses.
    fn scenario() {
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        ip(&["link", "set", "lo", "up"]);
        let srv = WhepServer::bind("whep://0.0.0.0:0").unwrap();
        let m = srv.add_mount("/cam", MountConfig::default()).unwrap();

        ip(&["addr", "add", "192.0.2.10/24", "dev", "lo"]);
        let first: IpAddr = "192.0.2.10".parse().unwrap();
        let mut a = [TestViewer::bound_to(first)];
        assert_eq!(join_on(&srv, first, &mut a[0]), [first, loopback]);
        connect_all(&m, &mut a);
        play(&m, 0, &mut a);
        let media = SocketAddr::new(first, srv.media_addr().port());
        assert_eq!(a[0].sources, HashSet::from([media]));

        ip(&["addr", "del", "192.0.2.10/24", "dev", "lo"]);
        ip(&["addr", "add", "198.51.100.7/24", "dev", "lo"]);
        let second: IpAddr = "198.51.100.7".parse().unwrap();
        let mut b = [TestViewer::bound_to(second)];
        assert_eq!(join_on(&srv, second, &mut b[0]), [second, loopback]);
        // The first session is still open on the address that is gone, so
        // the mount has two; only the new one has to connect.
        let connected = pump_all(&mut b, |vs| vs[0].connected, Duration::from_secs(5));
        assert!(connected, "{}", b[0].trace());
        play(&m, 30, &mut b);
    }

    /// Runs this test again inside a new network namespace: unprivileged
    /// (a user namespace) where the host allows it, through `sudo -n` where
    /// `TST_NETNS_SUDO=1` says the host has password-less sudo.
    #[test]
    fn a_wildcard_bind_follows_the_hosts_address() {
        if std::env::var_os(CHILD).is_some() {
            return scenario();
        }
        if let Err(e) = Command::new("ip").arg("-V").output() {
            return skip_unless_required("NETNS", format!("no ip tool: {e}"));
        }
        let exe = std::env::current_exe().unwrap();
        let test = [TEST, "--exact", "--nocapture", "--test-threads=1"];
        let sudo = std::env::var("TST_NETNS_SUDO").is_ok_and(|v| v == "1");
        let mut cmd = if sudo {
            let mut c = Command::new("sudo");
            c.args(["-n", "unshare", "--net", "--", "env"])
                .arg(format!("{CHILD}=1"));
            c
        } else {
            let mut c = Command::new("unshare");
            c.args(["--user", "--map-root-user", "--net", "--"])
                .env(CHILD, "1");
            c
        };
        let out = match cmd.arg(&exe).args(test).output() {
            Ok(out) => out,
            Err(e) => return skip_unless_required("NETNS", format!("cannot run unshare: {e}")),
        };
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        // The child ran the scenario only if the harness reports this test.
        if out.status.success() {
            assert!(text.contains("1 passed"), "the child ran no test: {text}");
            return;
        }
        if !text.contains("running 1 test") {
            return skip_unless_required("NETNS", format!("no network namespace: {text}"));
        }
        panic!("the scenario failed inside the namespace:\n{text}");
    }
}
