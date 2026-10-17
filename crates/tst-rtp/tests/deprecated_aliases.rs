//! The old names this crate still accepts compile and name the new types,
//! at the defining module path, and at the crate root where the old name had one.

#![allow(deprecated)]

#[test]
fn error_names_resolve_at_both_paths() {
    fn url_mod(_: Option<&tst_rtp::url::UrlError>) {}
    fn connect(_: Option<&tst_rtp::ConnectError>) {}
    fn connect_mod(_: Option<&tst_rtp::transport::ConnectError>) {}
    // Each old name is the new type: the new spelling is accepted where the old is named.
    let u: Option<&tst_rtp::RtpUrlError> = None;
    url_mod(u);
    let c: Option<&tst_rtp::RtpConnectError> = None;
    connect(c);
    connect_mod(c);
}
