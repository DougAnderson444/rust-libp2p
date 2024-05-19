//! WebRTC Transport for libp2p, base on [str0m] WebRTC library.

use futures::future::BoxFuture;
use futures::prelude::*;
use futures::stream::SelectAll;
use futures::Stream;
use if_watch::tokio::IfWatcher;
use if_watch::IfEvent;
use libp2p_core::multiaddr::Protocol;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use tokio::sync::Mutex as AsyncMutex;

use libp2p_core::transport::{ListenerId, TransportError, TransportEvent};
use libp2p_core::Multiaddr;
use libp2p_identity as identity;
use libp2p_identity::PeerId;

use crate::tokio::certificate::Certificate;
use crate::tokio::connection::Connection;
use crate::tokio::error::Error;
use crate::tokio::fingerprint::Fingerprint;
use crate::tokio::udp_manager::{UDPManager, UDPManagerEvent};

use super::connection::Open;
use super::upgrade;

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
    type Output = (PeerId, Arc<AsyncMutex<Connection<Open>>>);

    type Error = Error;

    type ListenerUpgrade = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    type Dial = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    fn listen_on(
        &mut self,
        id: libp2p_core::transport::ListenerId,
        addr: libp2p_core::Multiaddr,
    ) -> Result<(), TransportError<Self::Error>> {
        let addr =
            parse_webrtc_listen_addr(&addr).ok_or(TransportError::MultiaddrNotSupported(addr))?;

        let udp_manager = Arc::new(Mutex::new(
            UDPManager::with_address(addr)
                .map_err(|e| TransportError::Other(Error::Disconnected))?,
        ));

        self.listeners.push(
            ListenStream::new(id, self.config.clone(), udp_manager)
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

    /// Poll all listeners.
    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<TransportEvent<Self::ListenerUpgrade, Self::Error>> {
        match self.listeners.poll_next_unpin(cx) {
            Poll::Ready(Some(ev)) => Poll::Ready(ev),
            _ => Poll::Pending,
        }
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

    /// The UDP Socket manager for this listener.
    udp_manager: Arc<Mutex<UDPManager>>,

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

    /// Pending event to reported. In our case, if a Item = [TransportEvent::NewAddress] event
    /// occurs,
    pending_event: Option<<Self as Stream>::Item>,

    /// The stream must be awaken after it has been closed to deliver the last event.
    close_listener_waker: Option<Waker>,
}

impl ListenStream {
    /// Creates a new [`ListenStream`] with the given listener id, config, and socket.
    fn new(
        listener_id: ListenerId,
        config: Config,
        udp_manager: Arc<Mutex<UDPManager>>,
    ) -> io::Result<Self> {
        let listen_addr = udp_manager.lock().unwrap().listen_addr();
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
            udp_manager,
            report_closed: None,
            if_watcher,
            pending_event,
            close_listener_waker: None,
        })
    }

    /// Report the listener as closed in a [`TransportEvent::ListenerClosed`] and
    /// terminate the stream.
    fn close(&mut self, reason: Result<(), Error>) {
        match self.report_closed {
            Some(_) => tracing::debug!("Listener was already closed"),
            None => {
                // Report the listener event as closed.
                self.report_closed = Some(Some(TransportEvent::ListenerClosed {
                    listener_id: self.listener_id,
                    reason,
                }));

                // Wake the stream to deliver the last event.
                if let Some(waker) = self.close_listener_waker.take() {
                    waker.wake();
                }
            }
        }
    }

    /// Polls the [ListenStream] for any [IfWatcher] events and relays any changes as
    /// [TransportEvent::NewAddress], [TransportEvent::AddressExpired], or [TransportEvent::ListenerError] events.
    fn poll_if_watcher(&mut self, cx: &mut Context<'_>) -> Poll<<Self as Stream>::Item> {
        let Some(if_watcher) = self.if_watcher.as_mut() else {
            return Poll::Pending;
        };

        while let Poll::Ready(event) = if_watcher.poll_if_event(cx) {
            match event {
                Ok(IfEvent::Up(inet)) => {
                    let ip = inet.addr();
                    if self.listen_addr.is_ipv4() == ip.is_ipv4()
                        || self.listen_addr.is_ipv6() == ip.is_ipv6()
                    {
                        return Poll::Ready(TransportEvent::NewAddress {
                            listener_id: self.listener_id,
                            listen_addr: self.listen_multiaddress(ip),
                        });
                    }
                }
                Ok(IfEvent::Down(inet)) => {
                    let ip = inet.addr();
                    if self.listen_addr.is_ipv4() == ip.is_ipv4()
                        || self.listen_addr.is_ipv6() == ip.is_ipv6()
                    {
                        return Poll::Ready(TransportEvent::AddressExpired {
                            listener_id: self.listener_id,
                            listen_addr: self.listen_multiaddress(ip),
                        });
                    }
                }
                Err(err) => {
                    return Poll::Ready(TransportEvent::ListenerError {
                        listener_id: self.listener_id,
                        error: Error::Io(err),
                    });
                }
            }
        }

        Poll::Pending
    }

    /// Constructs a [`Multiaddr`] for the given IP address that represents our listen address.
    fn listen_multiaddress(&self, ip: IpAddr) -> Multiaddr {
        let socket_addr = SocketAddr::new(ip, self.listen_addr.port());

        socketaddr_to_multiaddr(&socket_addr, Some(self.config.fingerprint))
    }

    // TODO: Move upgrade::inbound here
    // /// Upgrades this incound remote source
    // fn upgrade_inbound(&self, remote: NewRemoteAddress) {}
}

impl Stream for ListenStream {
    type Item = TransportEvent<<Transport as libp2p_core::Transport>::ListenerUpgrade, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // If a [TransportEvent::NewAddress] event is pending, return it.
            if let Some(event) = self.pending_event.take() {
                return Poll::Ready(Some(event));
            }
            // If a Listener has been closed
            if let Some(closed) = self.report_closed.as_mut() {
                // Listener was closed.
                // Report the transport event if there is one. On the next iteration, return
                // `Poll::Ready(None)` to terminate the stream.
                return Poll::Ready(closed.take());
            }
            if let Poll::Ready(event) = self.poll_if_watcher(cx) {
                return Poll::Ready(Some(event));
            }

            let mut udp = self.udp_manager.lock().unwrap();

            // UDP Manager will only bubble up new addresses for tracking, and
            // errors for closing. All other UDP Events are handled internally
            // within the upgraded connection.
            match udp.poll(cx) {
                Poll::Ready(UDPManagerEvent::NewRemoteAddress(remote)) => {
                    let local_addr =
                        socketaddr_to_multiaddr(&self.listen_addr, Some(self.config.fingerprint));
                    let send_back_addr = socketaddr_to_multiaddr(&remote.addr, None);

                    let upgrade = upgrade::inbound(
                        remote.addr,
                        self.config.inner.clone(),
                        self.udp_manager.clone(),
                        self.config.inner.dtls_cert().unwrap().clone(),
                        remote.ufrag,
                        self.config.id_keys.clone(),
                        remote.stun_msg,
                    )
                    .boxed();

                    return Poll::Ready(Some(TransportEvent::Incoming {
                        upgrade,
                        local_addr,
                        send_back_addr,
                        listener_id: self.listener_id,
                    }));
                }
                Poll::Ready(UDPManagerEvent::Error(err)) => {
                    tracing::error!("Error in UDPManager: {:?}", err);
                    drop(udp);
                    self.close(Err(err));
                    continue;
                }
                Poll::Pending => {}
            }
            return Poll::Pending;
        }
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
