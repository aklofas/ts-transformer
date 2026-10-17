//! The old names this crate still accepts compile and name the new types,
//! at the root re-export and at the defining module path.

#![allow(deprecated)]

#[test]
fn builders_and_configs_keep_their_old_names() {
    let _: tst_srt::SrtSocketBuilder = tst_srt::SocketBuilder::new();
    let _: tst_srt::SrtListenerBuilder = tst_srt::ListenerBuilder::new();
    let _: tst_srt::SrtSocketConfig = tst_srt::SocketConfig::default();
    let _: tst_srt::SrtListenerConfig = tst_srt::ListenerConfig::default();
    let _: tst_srt::SrtSocketBuilder = tst_srt::builder::SocketBuilder::new();
    let _: tst_srt::SrtListenerBuilder = tst_srt::builder::ListenerBuilder::new();
    let _: tst_srt::SrtSocketConfig = tst_srt::config::SocketConfig::default();
    let _: tst_srt::SrtListenerConfig = tst_srt::config::ListenerConfig::default();
}

#[test]
fn handle_stats_and_error_names_resolve_at_both_paths() {
    fn socket(_: Option<&tst_srt::Socket>) {}
    fn socket_mod(_: Option<&tst_srt::socket::Socket>) {}
    fn listener(_: Option<&tst_srt::Listener>) {}
    fn listener_mod(_: Option<&tst_srt::listener::Listener>) {}
    fn stats(_: Option<&tst_srt::Stats>) {}
    fn stats_mod(_: Option<&tst_srt::socket::Stats>) {}
    fn connect(_: Option<&tst_srt::ConnectError>) {}
    fn connect_mod(_: Option<&tst_srt::error::ConnectError>) {}
    fn url(_: Option<&tst_srt::UrlError>) {}
    fn url_mod(_: Option<&tst_srt::url::UrlError>) {}
    // Each old name is the new type: the new spelling is accepted where the old is named.
    let s: Option<&tst_srt::SrtSocket> = None;
    socket(s);
    socket_mod(s);
    let l: Option<&tst_srt::SrtListener> = None;
    listener(l);
    listener_mod(l);
    let st: Option<&tst_srt::SrtStats> = None;
    stats(st);
    stats_mod(st);
    let c: Option<&tst_srt::SrtConnectError> = None;
    connect(c);
    connect_mod(c);
    let u: Option<&tst_srt::SrtUrlError> = None;
    url(u);
    url_mod(u);
}
