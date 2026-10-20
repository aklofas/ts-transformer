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

#[test]
fn an_mp2t_mount_is_accepted() {
    let mut cfg = MountConfig::default();
    cfg.kind = MountKind::Mp2t;
    cfg.validate().unwrap();
}

#[test]
fn an_opus_passthrough_mount_is_accepted() {
    let mut cfg = MountConfig::default();
    cfg.audio = AudioPolicy::PassthroughOpus;
    cfg.validate().unwrap();
}

#[test]
fn opus_passthrough_on_an_mp2t_mount_is_refused() {
    let mut cfg = MountConfig::default();
    cfg.kind = MountKind::Mp2t;
    cfg.audio = AudioPolicy::PassthroughOpus;
    match cfg.validate() {
        Err(WhepServerError::InvalidConfig { detail }) => {
            assert!(detail.contains("Opus mapping"), "{detail}");
        }
        other => panic!("expected InvalidConfig, got {other:?}"),
    }
    cfg.audio = AudioPolicy::Drop;
    cfg.validate().unwrap();
}
