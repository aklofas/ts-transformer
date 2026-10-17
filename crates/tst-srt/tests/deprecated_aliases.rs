//! The old names this crate still accepts compile and name the new types.

#![allow(deprecated)]

#[test]
fn builders_and_configs_keep_their_old_names() {
    let _ = tst_srt::SrtSocketBuilder::new();
    let _ = tst_srt::SrtListenerBuilder::new();
    let _ = tst_srt::SrtSocketConfig::default();
    let _ = tst_srt::SrtListenerConfig::default();
    let _ = tst_srt::builder::SrtSocketBuilder::new();
    let _ = tst_srt::builder::SrtListenerBuilder::new();
    let _ = tst_srt::config::SrtSocketConfig::default();
    let _ = tst_srt::config::SrtListenerConfig::default();
}

#[test]
fn handle_stats_and_error_names_resolve_at_both_paths() {
    fn socket(_: Option<&tst_srt::SrtSocket>) {}
    fn socket_mod(_: Option<&tst_srt::socket::SrtSocket>) {}
    fn listener(_: Option<&tst_srt::SrtListener>) {}
    fn listener_mod(_: Option<&tst_srt::listener::SrtListener>) {}
    fn stats(_: Option<&tst_srt::SrtStats>) {}
    fn stats_mod(_: Option<&tst_srt::socket::SrtStats>) {}
    fn connect(_: Option<&tst_srt::SrtConnectError>) {}
    fn connect_mod(_: Option<&tst_srt::error::SrtConnectError>) {}
    fn url(_: Option<&tst_srt::SrtUrlError>) {}
    fn url_mod(_: Option<&tst_srt::url::SrtUrlError>) {}
    socket(None);
    socket_mod(None);
    listener(None);
    listener_mod(None);
    stats(None);
    stats_mod(None);
    connect(None);
    connect_mod(None);
    url(None);
    url_mod(None);
}
