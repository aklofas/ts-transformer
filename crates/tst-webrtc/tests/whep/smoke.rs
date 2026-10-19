#[test]
fn default_mount_config_validates() {
    assert!(tst_webrtc::MountConfig::default().validate().is_ok());
}

#[test]
fn av1_mount_config_is_rejected() {
    let mut cfg = tst_webrtc::MountConfig::default();
    cfg.video = tst_webrtc::VideoCodec::Av1;
    assert!(matches!(
        cfg.validate(),
        Err(tst_webrtc::WhepServerError::InvalidConfig { .. })
    ));
}
