//! Provide a interface wrapper over the fingerprinting functionality.

use str0m::change::Fingerprint as Str0mFingerprint;

const SHA256: &str = "sha-256";

type Multihash = multihash::Multihash<64>;

/// A certificate fingerprint that is assumed to be created using the SHA256 hash algorithm.
#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub struct Fingerprint(libp2p_webrtc_utils::Fingerprint);

impl Fingerprint {
    #[cfg(test)]
    pub fn raw(bytes: [u8; 32]) -> Self {
        Self(libp2p_webrtc_utils::Fingerprint::raw(bytes))
    }

    /// Creates a fingerprint from a raw certificate.
    pub fn from_certificate(bytes: &[u8]) -> Self {
        Fingerprint(libp2p_webrtc_utils::Fingerprint::from_certificate(bytes))
    }

    /// Converts [`Str0mFingerprint`] to [`Fingerprint`].
    pub fn try_from_rtc_dtls(fp: &Str0mFingerprint) -> Option<Self> {
        if fp.hash_func != SHA256 {
            return None;
        }

        let buf: [u8; 32] = fp.bytes.clone().try_into().ok()?;

        Some(Self(libp2p_webrtc_utils::Fingerprint::raw(buf)))
    }

    /// Converts [`Multihash`](multihash::Multihash) to [`Fingerprint`].
    pub fn try_from_multihash(hash: Multihash) -> Option<Self> {
        Some(Self(libp2p_webrtc_utils::Fingerprint::try_from_multihash(
            hash,
        )?))
    }

    /// Converts this fingerprint to [`Multihash`](multihash::Multihash).
    pub fn to_multihash(self) -> Multihash {
        self.0.to_multihash()
    }

    /// Formats this fingerprint as uppercase hex, separated by colons (`:`).
    ///
    /// This is the format described in <https://www.rfc-editor.org/rfc/rfc4572#section-5>.
    pub fn to_sdp_format(self) -> String {
        self.0.to_sdp_format()
    }

    /// Returns the algorithm used (e.g. "sha-256").
    /// See <https://datatracker.ietf.org/doc/html/rfc8122#section-5>
    pub fn algorithm(&self) -> String {
        self.0.algorithm()
    }

    pub(crate) fn into_inner(self) -> libp2p_webrtc_utils::Fingerprint {
        self.0
    }
}

#[cfg(test)]
mod test_fingerprint {

    use super::*;
    use rand::*;

    #[test]
    fn test_try_from_rtc_dtls() {
        let bytes = rand::thread_rng().gen::<[u8; 32]>();
        let fp = Str0mFingerprint {
            hash_func: SHA256.to_string(),
            bytes: bytes.to_vec(),
        };
        let fingerprint = Fingerprint::try_from_rtc_dtls(&fp).unwrap();
        assert_eq!(fingerprint.0, libp2p_webrtc_utils::Fingerprint::raw(bytes));
    }
}
