//! `From<…> for BindingError` — the TCP rows of the kind table (spec §3.3);
//! see `tst-srt/src/binding_kind.rs` for the why.

use crate::error::TcpError;
use crate::url::TcpUrlError;
use tst_pipeline::binding::{BindingError, BindingErrorKind as K};

impl From<TcpUrlError> for BindingError {
    fn from(e: TcpUrlError) -> Self {
        BindingError::new(K::TcpUrl, e.to_string())
    }
}

impl From<TcpError> for BindingError {
    fn from(e: TcpError) -> Self {
        let kind = match &e {
            TcpError::Url(_) => K::TcpUrl,
            TcpError::Io(_) => K::TcpIo,
            TcpError::Closed => K::Closed,
            TcpError::ConnectTimeout { .. } => K::TcpConnectTimeout,
            TcpError::InvalidConfig(_) => K::TcpInvalidConfig,
            #[cfg(feature = "tls")]
            TcpError::Tls(_) => K::TcpTls,
            TcpError::TlsDisabled => K::TcpTlsDisabled,
        };
        BindingError::new(kind, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use crate::error::TcpError;
    use std::io;
    use tst_pipeline::binding::{BindingError, BindingErrorKind as K};

    #[test]
    fn tcp_kinds() {
        assert_eq!(
            BindingError::from(TcpError::Io(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "no"
            )))
            .kind,
            K::TcpIo
        );
        assert_eq!(BindingError::from(TcpError::Closed).kind, K::Closed);
        assert_eq!(
            BindingError::from(TcpError::ConnectTimeout { seconds: 10 }).kind,
            K::TcpConnectTimeout
        );
        assert_eq!(
            BindingError::from(TcpError::InvalidConfig("x".into())).kind,
            K::TcpInvalidConfig
        );
        assert_eq!(
            BindingError::from(TcpError::TlsDisabled).kind,
            K::TcpTlsDisabled
        );
        // Both ways into TCP_URL: the URL error on its own, and wrapped.
        assert_eq!(
            BindingError::from(crate::url::TcpUrlError::MissingPort).kind,
            K::TcpUrl
        );
        assert_eq!(
            BindingError::from(TcpError::Url(crate::url::TcpUrlError::MissingPort)).kind,
            K::TcpUrl
        );
        assert_eq!(K::TcpUrl.c_projection(), -31);
        assert_eq!(K::TcpTlsDisabled.c_projection(), -33);
        assert_eq!(K::TcpTlsDisabled.name(), "TLS_DISABLED");
        #[cfg(feature = "tls")]
        assert_eq!(
            BindingError::from(TcpError::Tls("handshake".into())).kind,
            K::TcpTls
        );
    }
}
