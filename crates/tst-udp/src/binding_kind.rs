//! `From<…> for BindingError` — the UDP rows of the kind table (spec §3.3);
//! see `tst-srt/src/binding_kind.rs` for the why.

use crate::error::UdpError;
use crate::url::UdpUrlError;
use tst_pipeline::binding::{BindingError, BindingErrorKind as K};

impl From<UdpUrlError> for BindingError {
    fn from(e: UdpUrlError) -> Self {
        BindingError::new(K::UdpUrl, e.to_string())
    }
}

impl From<UdpError> for BindingError {
    fn from(e: UdpError) -> Self {
        let kind = match &e {
            UdpError::Url(_) => K::UdpUrl,
            UdpError::Io(_) => K::UdpIo,
            UdpError::InvalidConfig(_) => K::UdpInvalidConfig,
        };
        BindingError::new(kind, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use crate::error::UdpError;
    use std::io;
    use tst_pipeline::binding::{BindingError, BindingErrorKind as K};

    #[test]
    fn udp_kinds() {
        assert_eq!(
            BindingError::from(UdpError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "tick"
            )))
            .kind,
            K::UdpIo
        );
        assert_eq!(
            BindingError::from(UdpError::InvalidConfig("x".into())).kind,
            K::UdpInvalidConfig
        );
        // Both ways into UDP_URL: the URL error on its own, and wrapped.
        assert_eq!(
            BindingError::from(crate::url::UdpUrlError::MissingPort).kind,
            K::UdpUrl
        );
        assert_eq!(
            BindingError::from(UdpError::Url(crate::url::UdpUrlError::MissingPort)).kind,
            K::UdpUrl
        );
        assert_eq!(K::UdpUrl.c_projection(), -27);
        assert_eq!(K::UdpUrl.name(), "URL");
    }
}
