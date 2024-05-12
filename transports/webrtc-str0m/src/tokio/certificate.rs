//! Certificate module.

use crate::tokio::fingerprint::Fingerprint;

use str0m::change::DtlsCert;

#[derive(Debug, Clone)]
pub struct Certificate {
    inner: DtlsCert,
}

impl Certificate {
    /// Generate new certificate.
    pub fn generate() -> Self {
        let cert = DtlsCert::new_openssl();
        Self { inner: cert }
    }

    /// Returns SHA-256 fingerprint of this certificate.
    pub fn fingerprint(&self) -> Option<Fingerprint> {
        let fp = self.inner.fingerprint();
        Fingerprint::try_from_rtc_dtls(&fp)
    }

    /// Extract the [`RTCCertificate`] from this wrapper.
    ///
    /// This function is `pub(crate)` to avoid leaking the `str0m` dependency to our users.
    pub(crate) fn extract(&self) -> DtlsCert {
        self.inner.clone()
    }
}
