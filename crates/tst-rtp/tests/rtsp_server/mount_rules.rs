//! The per-path rules table: handle semantics against a live server, and
//! the builder's default rule.

use std::time::Duration;

use tst_rtp::{
    MountPolicy, MountRule, MountRuleError, RefuseStatus, RtspServer, RtspServerBuilder,
};

use crate::fixtures::raw_rtsp_publisher::*;

fn started(b: RtspServerBuilder) -> RtspServer {
    let server = b.build().unwrap();
    server.start().unwrap();
    server
}

/// Two handles are one table, and `stop()` closes it.
#[test]
fn mount_rules_handle_is_live_across_clones_and_closes_with_stop() {
    let server = started(RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap());
    let a = server.mount_rules();
    let b = server.mount_rules();
    a.set("/amber", MountRule::both(MountPolicy::Open)).unwrap();
    assert_eq!(b.len(), 1);
    assert!(!b.is_closed());

    server.stop().unwrap();

    assert!(matches!(
        a.set("/other", MountRule::default()),
        Err(MountRuleError::Closed)
    ));
    assert!(b.is_closed());
    assert_eq!(b.len(), 1);
}

/// The builder's default rule governs a path with no entry.
#[test]
#[ignore = "wired in Task 3"]
fn builder_default_mount_rule_seeds_the_table() {
    let mut b = RtspServerBuilder::new("rtsp://127.0.0.1:0").unwrap();
    b.accept_unregistered_publishers(true);
    b.default_mount_rule(MountRule::both(MountPolicy::Refuse(RefuseStatus::NotFound)));
    let server = started(b);
    let port = server.local_addr().unwrap().port();

    let mut p = RawPublisher::connect(port);
    assert_eq!(p.announce("/anything", SDP_MP2T), 404);
    assert!(
        server
            .next_publisher(Duration::from_millis(200))
            .unwrap()
            .is_none(),
        "a refused ANNOUNCE queues no mount"
    );
    server.stop().unwrap();
}
