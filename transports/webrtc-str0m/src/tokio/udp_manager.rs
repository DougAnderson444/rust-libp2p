//! This module contains the `UDPManager` struct which is responsible for managing the UDP connections used by the WebRTC transport.

use crate::tokio::{
    connection::{Open, Opening},
    error, Connection, Error,
};
use libp2p_core::transport::ListenerId;
use libp2p_identity::PeerId;
use socket2::{Domain, Socket, Type};
use std::sync::{Arc, Mutex};
use std::{
    collections::HashMap,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use str0m::ice::{IceCreds, StunMessage};
use tokio::{io::ReadBuf, net::UdpSocket, sync::mpsc::Sender};

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
    pub(crate) stun_msg: Vec<u8>,
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

/// Connection context.
struct ConnectionContext {
    /// Remote peer ID.
    peer: PeerId,

    /// Connection ID.
    connection_id: ListenerId,

    /// TX channel for sending datagrams to the connection event loop.
    tx: Sender<Vec<u8>>,
}

enum ConnectionState {
    Open(Connection<Open>),
    Opening(Connection<Opening>),
}

/// The `UDPManager` struct is responsible for managing the UDP connections used by the WebRTC transport.
pub(crate) struct UDPManager {
    /// The UDP socket used for sending and receiving data.
    socket: Arc<UdpSocket>,

    /// The socket listen address.
    listen_addr: SocketAddr,

    /// All the address connections we are tracking.
    addr_conns: HashMap<SocketAddr, Arc<Mutex<Connection>>>,
}

/// Whether this is a new connection that should be Polled or not.
enum NewSource {
    Yes(IceCreds),
    No,
}

impl UDPManager {
    /// Getter for socket
    pub fn socket(&self) -> Arc<UdpSocket> {
        self.socket.clone()
    }

    /// Adds an address to addr_conns.
    pub fn add_connection(&mut self, addr: SocketAddr, connection: Arc<Mutex<Connection>>) {
        self.addr_conns.insert(addr, connection);
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

        Ok(Self {
            listen_addr: socket.local_addr()?,
            socket: Arc::new(socket),
            addr_conns: HashMap::new(),
        })
    }

    /// Returns the listen address of the UDP socket.
    pub(crate) fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
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

                    match this.handle_socket_input(source, &buf) {
                        Ok(NewSource::No) => { /* Existing connection, handled. */ }
                        Ok(NewSource::Yes(ice_creds)) => {
                            let ready =
                                Poll::Ready(UDPManagerEvent::NewRemoteAddress(NewRemoteAddress {
                                    addr: source,
                                    ufrag: ice_creds.ufrag,
                                    pass: ice_creds.pass,
                                    stun_msg: buf,
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
        return Poll::Pending;
    }

    /// Handle socket input.
    ///
    /// If the datagram was received from an active client, it's dispatched to the connection
    /// handler, if there is space in the queue. If the datagram opened a new connection or it
    /// belonged to a client who is opening, the event loop is instructed to poll the client
    /// until it timeouts.
    ///
    /// Returns `true` if the client should be polled.
    fn handle_socket_input(
        &mut self,
        source: SocketAddr,
        buffer: &[u8],
    ) -> Result<NewSource, error::Error> {
        // 1) If its Open or Opening, we have seen this addr before and we send data
        // There should be a connection_event_handler running already to receive this data,
        // which if there is a connection, will be true.
        if let Some(connection) = self.addr_conns.get_mut(&source) {
            connection
                .lock()
                .map_err(|_| Error::LockPoisoned)?
                .dgram_recv(buffer)?;
            return Ok(NewSource::No);
        }

        // 2) Otherwise we haven't seen this source address before, it should be Stun, and we return the ICE creds (ufrag, pass)
        let binding = StunMessage::parse(&buffer)
            .map_err(|op| Error::NetError(str0m::error::NetError::Stun(op)))?;

        let (ufrag, pass) = binding
            .split_username()
            .ok_or(Error::Authentication)
            .map_err(|_| Error::InvalidData)?;

        Ok(NewSource::Yes(IceCreds {
            ufrag: ufrag.to_owned(),
            pass: pass.to_owned(),
        }))
    }
}
