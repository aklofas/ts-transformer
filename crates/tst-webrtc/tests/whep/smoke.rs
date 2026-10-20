use tst_webrtc::{AudioPolicy, MountConfig, MountKind, VideoCodec, WhepServerError};

fn is_invalid_config(cfg: &MountConfig) -> bool {
    matches!(cfg.validate(), Err(WhepServerError::InvalidConfig { .. }))
}

#[test]
fn default_mount_config_validates() {
    assert!(MountConfig::default().validate().is_ok());
}

#[test]
fn h265_mount_config_validates() {
    let mut cfg = MountConfig::default();
    cfg.video = VideoCodec::H265;
    assert!(cfg.validate().is_ok());
}

#[test]
fn av1_mount_config_is_rejected() {
    let mut cfg = MountConfig::default();
    cfg.video = VideoCodec::Av1;
    assert!(is_invalid_config(&cfg));
}

#[test]
fn h266_mount_config_is_rejected() {
    let mut cfg = MountConfig::default();
    cfg.video = VideoCodec::H266;
    assert!(is_invalid_config(&cfg));
}

#[test]
fn sei_budget_bounds() {
    let mut cfg = MountConfig::default();
    cfg.sei_budget_bytes = 0;
    assert!(is_invalid_config(&cfg));
    cfg.sei_budget_bytes = 1024 * 1024 + 1;
    assert!(is_invalid_config(&cfg));
    cfg.sei_budget_bytes = 1024 * 1024;
    assert!(cfg.validate().is_ok());
}

#[test]
fn viewer_queue_bounds() {
    let mut cfg = MountConfig::default();
    cfg.viewer_queue_packets = 15;
    assert!(is_invalid_config(&cfg));
    cfg.viewer_queue_packets = 16;
    assert!(cfg.validate().is_ok());
}

/// The refusal's text, for a configuration `validate` refuses.
fn refusal(cfg: &MountConfig) -> String {
    match cfg.validate() {
        Err(WhepServerError::InvalidConfig { detail }) => detail,
        other => panic!("expected InvalidConfig, got {other:?}"),
    }
}

#[test]
fn an_mp2t_mount_is_refused_until_it_is_served() {
    let mut cfg = MountConfig::default();
    cfg.kind = MountKind::Mp2t;
    let detail = refusal(&cfg);
    assert!(detail.contains("Mp2t"), "{detail}");
    assert!(detail.contains("not served in this release"), "{detail}");
}

#[test]
fn opus_passthrough_is_refused_until_it_is_served() {
    let mut cfg = MountConfig::default();
    cfg.audio = AudioPolicy::PassthroughOpus;
    let detail = refusal(&cfg);
    assert!(detail.contains("PassthroughOpus"), "{detail}");
    assert!(detail.contains("not served in this release"), "{detail}");
}
