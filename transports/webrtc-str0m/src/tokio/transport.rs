//! Transport module

use futures::future::BoxFuture;
use futures::stream::SelectAll;
use futures::Stream;
use if_watch::tokio::IfWatcher;
use libp2p_core::multiaddr::Protocol;
use socket2::{Domain, Socket, Type};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::task::Waker;
use tokio::net::UdpSocket;

use libp2p_core::transport::{ListenerId, TransportError, TransportEvent};
use libp2p_core::Multiaddr;
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
        let addr =
            parse_webrtc_listen_addr(&addr).ok_or(TransportError::MultiaddrNotSupported(addr))?;

        let socket =
            bind_socket(addr).map_err(|e| TransportError::Other(Error::IoError(e.kind())))?;

        self.listeners.push(
            ListenStream::new(id, self.config.clone(), socket)
                .map_err(|e| TransportError::Other(Error::Io(e)))?,
        );

        Ok(())
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
struct ListenStream {
    /// The ID of this listener.
    listener_id: ListenerId,

    /// The socket address that the listening socket is bound to,
    /// which may be a "wildcard address" like `INADDR_ANY` or `IN6ADDR_ANY`
    /// when listening on all interfaces for IPv4 respectively IPv6 connections.
    listen_addr: SocketAddr,

    /// The config which holds this peer's certificate(s).
    config: Config,

    /// The Socket for this listener.
    socket: UdpSocket,

    /// Set to `Some` if this listener should close.
    ///
    /// Optionally contains a [`TransportEvent::ListenerClosed`] that should be
    /// reported before the listener's stream is terminated.
    report_closed: Option<Option<<Self as Stream>::Item>>,

    /// Watcher for network interface changes.
    /// Reports [`IfEvent`]s for new / deleted ip-addresses when interfaces
    /// become or stop being available.
    ///
    /// `None` if the socket is only listening on a single interface.
    if_watcher: Option<IfWatcher>,

    /// Pending event to reported.
    pending_event: Option<<Self as Stream>::Item>,

    /// The stream must be awaken after it has been closed to deliver the last event.
    close_listener_waker: Option<Waker>,
}
impl ListenStream {
    /// Creates a new [`ListenStream`] with the given listener id, config, and socket.
    fn new(listener_id: ListenerId, config: Config, socket: UdpSocket) -> io::Result<Self> {
        let listen_addr = socket.local_addr()?;
        let if_watcher;
        let pending_event;
        if listen_addr.ip().is_unspecified() {
            if_watcher = Some(IfWatcher::new()?);
            pending_event = None;
        } else {
            if_watcher = None;
            let ma = socketaddr_to_multiaddr(&listen_addr, Some(config.fingerprint));
            pending_event = Some(TransportEvent::NewAddress {
                listener_id,
                listen_addr: ma,
            })
        }

        Ok(ListenStream {
            listener_id,
            listen_addr,
            config,
            socket,
            report_closed: None,
            if_watcher,
            pending_event,
            close_listener_waker: None,
        })
    }
}

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

/// Turns an IP address and port into the corresponding WebRTC multiaddr.
fn socketaddr_to_multiaddr(socket_addr: &SocketAddr, certhash: Option<Fingerprint>) -> Multiaddr {
    let addr = Multiaddr::empty()
        .with(socket_addr.ip().into())
        .with(Protocol::Udp(socket_addr.port()))
        .with(Protocol::WebRTCDirect);

    if let Some(fp) = certhash {
        return addr.with(Protocol::Certhash(fp.to_multihash()));
    }

    addr
}
/// Parse the given [`Multiaddr`] into a [`SocketAddr`] for listening.
fn parse_webrtc_listen_addr(addr: &Multiaddr) -> Option<SocketAddr> {
    let mut iter = addr.iter();

    let ip = match iter.next()? {
        Protocol::Ip4(ip) => IpAddr::from(ip),
        Protocol::Ip6(ip) => IpAddr::from(ip),
        _ => return None,
    };

    let Protocol::Udp(port) = iter.next()? else {
        return None;
    };
    let Protocol::WebRTCDirect = iter.next()? else {
        return None;
    };

    if iter.next().is_some() {
        return None;
    }

    Some(SocketAddr::new(ip, port))
}

/// Create and bind a socket address using the given [`SocketAddr`].
fn bind_socket(addr: SocketAddr) -> Result<UdpSocket, std::io::Error> {
    let socket = match addr.is_ipv4() {
        true => {
            let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(socket2::Protocol::UDP))?;

            socket.bind(&addr.into())?;
            socket
        }
        false => {
            let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(socket2::Protocol::UDP))?;
            socket.set_only_v6(true)?;
            socket.bind(&addr.into())?;
            socket
        }
    };

    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;

    let socket = UdpSocket::from_std(socket.into())?;
    Ok(socket)
}
