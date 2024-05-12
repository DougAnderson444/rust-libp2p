//! Transport module

use futures::future::BoxFuture;
use futures::stream::SelectAll;

use futures::Stream;
use libp2p_core::transport::{TransportError, TransportEvent};
use libp2p_identity as identity;
use libp2p_identity::PeerId;

use crate::tokio::certificate::Certificate;
use crate::tokio::connection::Connection;
use crate::tokio::error::Error;
use crate::tokio::fingerprint::Fingerprint;

/// A WebRTC transport with direct p2p communication (without a STUN server).
pub struct Transport {
    /// The config which holds this peer's keys and certificate.
    config: Config,
    /// All the active listeners.
    listeners: SelectAll<ListenStream>,
}

impl Transport {
    /// Creates a new WebRTC transport.
    ///
    /// # Example
    ///
    /// ```
    /// use libp2p_identity as identity;
    /// use rand::thread_rng;
    /// use libp2p_webrtc_str0m::tokio::{Transport, Certificate};
    ///
    /// let id_keys = identity::Keypair::generate_ed25519();
    /// let transport = Transport::new(id_keys, Certificate::generate());
    /// ```
    pub fn new(id_keys: identity::Keypair, certificate: Certificate) -> Self {
        Self {
            config: Config::new(id_keys, certificate),
            listeners: SelectAll::new(),
        }
    }
}

impl libp2p_core::Transport for Transport {
    type Output = (PeerId, Connection);

    type Error = Error;

    type ListenerUpgrade = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    type Dial = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    fn listen_on(
        &mut self,
        id: libp2p_core::transport::ListenerId,
        addr: libp2p_core::Multiaddr,
    ) -> Result<(), libp2p_core::transport::TransportError<Self::Error>> {
        todo!()
    }

    fn remove_listener(&mut self, id: libp2p_core::transport::ListenerId) -> bool {
        todo!()
    }

    fn dial(
        &mut self,
        addr: libp2p_core::Multiaddr,
    ) -> Result<Self::Dial, libp2p_core::transport::TransportError<Self::Error>> {
        todo!()
    }

    fn dial_as_listener(
        &mut self,
        addr: libp2p_core::Multiaddr,
    ) -> Result<Self::Dial, libp2p_core::transport::TransportError<Self::Error>> {
        todo!()
    }

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<libp2p_core::transport::TransportEvent<Self::ListenerUpgrade, Self::Error>>
    {
        todo!()
    }

    fn address_translation(
        &self,
        listen: &libp2p_core::Multiaddr,
        observed: &libp2p_core::Multiaddr,
    ) -> Option<libp2p_core::Multiaddr> {
        todo!()
    }
}

/// A stream of incoming connections on one or more interfaces.
struct ListenStream {}

impl Stream for ListenStream {
    type Item = TransportEvent<<Transport as libp2p_core::Transport>::ListenerUpgrade, Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        todo!()
    }
}

/// A config which holds peer's keys and a x509Cert used to authenticate WebRTC communications.
#[derive(Clone)]
struct Config {
    inner: str0m::RtcConfig,
    fingerprint: Fingerprint,
    id_keys: identity::Keypair,
}
impl Config {
    /// Returns a new [`Config`] with the given keys and certificate.
    fn new(id_keys: identity::Keypair, certificate: Certificate) -> Self {
        let fingerprint = certificate
            .fingerprint()
            .expect("Failed to generate fingerprint");
        let inner = str0m::RtcConfig::new().set_dtls_cert(certificate.extract());
        Self {
            inner,
            fingerprint,
            id_keys,
        }
    }
}
