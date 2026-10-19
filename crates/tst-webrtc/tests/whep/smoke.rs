use tst_webrtc::{MountConfig, VideoCodec, WhepServerError};

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
