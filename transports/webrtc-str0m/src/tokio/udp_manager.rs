//! This module contains the `UDPManager` struct which is responsible for managing the UDP connections used by the WebRTC transport.

use crate::tokio::{
    connection::{Open, Opening},
    error, Connection, Error,
};
use futures::channel::mpsc::{Receiver, Sender};
use libp2p_core::transport::ListenerId;
use libp2p_identity::PeerId;
use socket2::{Domain, Socket, Type};
use std::{
    collections::HashMap,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use std::{sync::Arc, time::Instant};
use str0m::net::Protocol as Str0mProtocol;
use str0m::{
    ice::{IceCreds, StunMessage},
    net::{DatagramRecv, Receive},
    Input,
};
use tokio::{
    io::ReadBuf,
    net::UdpSocket,
    sync::{mpsc, Mutex as AsyncMutex},
};

/// Logging target for the file.
const LOG_TARGET: &str = "libp2p_webrtc_str0m";

/// The maximum transmission unit (MTU) for the UDP connections.
const RECEIVE_MTU: usize = 8192;

/// The [`SocketAddr`] of a new remote address that wants to connect on this socket.
#[derive(Debug)]
pub(crate) struct NewRemoteAddress {
    pub(crate) addr: SocketAddr,
    pub(crate) ufrag: String,
    pub(crate) pass: String,
    pub(crate) contents: Vec<u8>,
}

/// Events emitted by the [`UDPManager`].
/// The [`UDPManager`] handles every event internally except for new remote addresses that want to
/// connect on ths socket. For this event, we bubble up to
/// the [Transport](crate::tokio::transport) to handle the new remote so
/// it can be upgraded accordingly.
#[derive(Debug)]
pub(crate) enum UDPManagerEvent {
    /// A new remote address wants to connect on this socket.
    NewRemoteAddress(NewRemoteAddress),
    /// Error event.
    Error(error::Error),
}

/// A struct which handles Socket Open Connection process
/// Initial Sender is a futures mpsc Option which is taken for Initialization,
/// The second Sender is the regular sender to send datagrams to the Connection
#[derive(Debug)]
pub(crate) struct ConnectionContext {
    /// TX channel for sending datagrams to the connection event loop.
    pub(crate) tx: mpsc::Sender<Vec<u8>>,
}

/// The `UDPManager` struct is responsible for managing the UDP connections used by the WebRTC transport.
pub(crate) struct UDPManager {
    /// The UDP socket used for sending and receiving data.
    socket: Arc<UdpSocket>,

    /// The socket listen address.
    listen_addr: SocketAddr,

    /// Opening connections.
    pub(crate) opening: HashMap<SocketAddr, Connection<Opening>>,

    /// Mapping of socket addresses to Open connections we have.
    pub(crate) open: HashMap<SocketAddr, ConnectionContext>,
}

/// Whether this is a new connection that should be Polled or not.
enum NewSource {
    Yes(IceCreds),
    No,
}

/// Events received from opening connections that are handled
/// by the [`WebRtcTransport`] event loop.
enum ConnectionEvent {
    /// Connection established.
    ConnectionEstablished,

    /// Connection to peer closed.
    ConnectionClosed,

    /// Timeout.
    Timeout {
        /// Timeout duration.
        duration: Duration,
    },
}

impl UDPManager {
    /// Getter for socket
    pub(crate) fn socket(&self) -> Arc<UdpSocket> {
        self.socket.clone()
    }

    /// Creates a new `UDPManager` with the given address.
    pub(crate) fn with_address(addr: SocketAddr) -> Result<Self, Error> {
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

        tracing::info!(
            target: LOG_TARGET,
            "webrtc udp socket bound to: {}",
            socket.local_addr()?,
        );

        Ok(Self {
            listen_addr: socket.local_addr()?,
            socket: Arc::new(socket),
            open: Default::default(),
            opening: Default::default(),
        })
    }

    /// Returns the listen address of the UDP socket.
    pub(crate) fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// Set the address
    pub(crate) fn set_listen_addr(&mut self, addr: SocketAddr) {
        self.listen_addr = addr;
    }

    /// Reads from the underlying UDP socket and processes the received data.
    pub(crate) fn poll(&mut self, cx: &mut Context) -> Poll<UDPManagerEvent> {
        let mut this = Pin::new(self);

        loop {
            let mut buf = vec![0u8; RECEIVE_MTU];
            let mut read_buf = ReadBuf::new(&mut buf);

            match this.socket.poll_recv_from(cx, &mut read_buf) {
                // TODO: Revisit this: either break with return Poll::Pending outside the loop, or continue with Poll::Pending inside the loop
                Poll::Pending => break,
                Poll::Ready(Err(error)) => {
                    tracing::info!(
                        target: LOG_TARGET,
                        ?error,
                        "webrtc udp socket closed",
                    );

                    return Poll::Ready(UDPManagerEvent::Error(Error::IoErr(error.kind())));
                }
                Poll::Ready(Ok(source)) => {
                    let nread = read_buf.filled().len();
                    buf.truncate(nread);

                    match this.on_socket_input(source, &buf) {
                        Ok(NewSource::No) => { /* Existing connection, handled. */ }
                        Ok(NewSource::Yes(ice_creds)) => {
                            tracing::trace!(
                                target: LOG_TARGET,
                                "new remote address: {} with ice creds: {:?}",
                                source,
                                ice_creds
                            );
                            let ready =
                                Poll::Ready(UDPManagerEvent::NewRemoteAddress(NewRemoteAddress {
                                    addr: source,
                                    ufrag: ice_creds.ufrag,
                                    pass: ice_creds.pass,
                                    contents: buf,
                                }));
                            return ready;
                        }
                        Err(error) => {
                            return Poll::Ready(UDPManagerEvent::Error(error));
                        }
                    }
                }
            }
        }
        Poll::Pending
    }

    /// Handle socket input.
    ///
    /// If the datagram was received from an active client, it's dispatched to the connection
    /// handler, if there is space in the queue. If the datagram opened a new connection or it
    /// belonged to a client who is opening, the event loop is instructed to poll the client
    /// until it timeouts.
    ///
    /// Returns `true` if the client should be polled.
    fn on_socket_input(
        &mut self,
        source: SocketAddr,
        buffer: &[u8],
    ) -> Result<NewSource, error::Error> {
        // get the notified from the notifier in open connections
        if let Some(ConnectionContext { tx }) = self.open.get_mut(&source) {
            match tx.try_send(buffer.to_vec()) {
                Ok(_) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!(
                        target: LOG_TARGET,
                        "connection buffer full, dropping packet from source: {}",
                        source
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    tracing::warn!(
                        target: LOG_TARGET,
                        "connection closed, dropping packet from source: {}",
                        source
                    );
                }
            }
            return Ok(NewSource::No);
        }

        if buffer.is_empty() {
            return Err(Error::InvalidData);
        }

        // 2) Otherwise we haven't seen this source address before, it should be Stun, and we return the ICE creds (ufrag, pass)
        match StunMessage::parse(buffer) {
            // .map_err(|op| Error::NetError(str0m::error::NetError::Stun(op)))?;
            Ok(stun) => {
                let (ufrag, pass) = stun
                    .split_username()
                    .ok_or(Error::Authentication)
                    .map_err(|_| Error::InvalidData)?;

                tracing::trace!(
                    target: LOG_TARGET,
                    "received STUN message from source: {}, ufrag: {}, pass: {}",
                    source,
                    ufrag,
                    pass
                );

                Ok(NewSource::Yes(IceCreds {
                    ufrag: ufrag.to_owned(),
                    pass: pass.to_owned(),
                }))
            }
            Err(err) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    "StunMessage didn't parse, received non-stun message while opening from source: {}, \n\n err: {} \n\n msg: {:?}",
                    source,
                    err,
                    String::from_utf8_lossy(buffer)
                );
                // TODO: Input::Receive and handle_input? of non-stun
                let contents: DatagramRecv = buffer.try_into().map_err(|_| Error::InvalidData)?;

                // Pretty rpitn contents
                tracing::trace!(
                    target: LOG_TARGET,
                    "received non-stun message from source: {}, contents: {:?}",
                    source,
                    contents
                );

                if let Some(conn) = self.opening.get_mut(&source) {
                    tracing::trace!(
                        target: LOG_TARGET,
                        peer = ?conn.peer_address,
                        "handle input from peer",
                    );

                    let message = Input::Receive(
                        Instant::now(),
                        Receive {
                            source: *conn.peer_address,
                            proto: Str0mProtocol::Udp,
                            destination: conn.local_addr,
                            contents,
                        },
                    );

                    match conn.rtc().lock().unwrap().accepts(&message) {
                        true => conn.rtc().lock().unwrap().handle_input(message).map_err(|error| {
                            tracing::debug!(target: LOG_TARGET, source = ?conn.peer_address, ?error, "failed to handle incomign non-stun data");
                            Error::InputRejected
                        })?,
                        false => {
                            tracing::warn!(
                                target: LOG_TARGET,
                                peer = ?conn.peer_address,
                                "input rejected",
                            );
                            return Err(Error::InputRejected);
                        }
                    }
                }

                Ok(NewSource::No)
            }
        }
    }

    /// Poll opening connection.
    fn poll_connection(&mut self, source: &SocketAddr) -> ConnectionEvent {
        todo!()
    }
}

fn is_stun_packet(bytes: &[u8]) -> bool {
    // 20 bytes for the header, then follows attributes.
    bytes.len() >= 20 && bytes[0] < 2
}
