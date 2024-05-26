//! This module contains the `UDPManager` struct which is responsible for managing the UDP connections used by the WebRTC transport.

use crate::tokio::{
    connection::{Open, Opening},
    error, Connection, Error,
};
use futures::channel::mpsc::{Receiver, Sender};
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

/// A struct which handles Socket Open Connection process
/// Initial Sender is a futures mpsc Option which is taken for Initialization,
/// The second Sender is the regular sender to send datagrams to the Connection
#[derive(Debug)]
pub(crate) struct SocketOpenConnection {
    /// The initial sender to notify the Connection that it's open.
    pub(crate) notifier: Option<Receiver<mpsc::Sender<Vec<u8>>>>,

    /// The sender to send datagrams to the Connection.
    pub(crate) sender: Option<mpsc::Sender<Vec<u8>>>,
}

/// The `UDPManager` struct is responsible for managing the UDP connections used by the WebRTC transport.
pub(crate) struct UDPManager {
    /// The UDP socket used for sending and receiving data.
    socket: Arc<UdpSocket>,

    /// The socket listen address.
    listen_addr: SocketAddr,

    /// Mapping of socket addresses to Open connections we have.
    pub(crate) socket_open_conns: HashMap<SocketAddr, SocketOpenConnection>,
}

/// Whether this is a new connection that should be Polled or not.
enum NewSource {
    Yes(IceCreds),
    No,
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

        Ok(Self {
            listen_addr: socket.local_addr()?,
            socket: Arc::new(socket),
            socket_open_conns: HashMap::new(),
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
        // get the notified from the notifier in open connections
        if let Some(SocketOpenConnection { notifier, sender }) =
            self.socket_open_conns.get_mut(&source)
        {
            // take the notifier out of the Option, leaving None in it;s place
            // if is Some, then recv from it before using the sender int he next step
            if let Some(mut notifier) = notifier.take() {
                let sndr = notifier.try_next().map_err(|_| Error::Disconnected)?;
                *sender = sndr;
            }

            // If Open Connection exists, send the datagram to Input::Receive
            match sender {
                Some(sender) => {
                    sender
                        .try_send(buffer.to_vec())
                        .map_err(|_| Error::Disconnected)?;
                    return Ok(NewSource::No);
                }
                None => {
                    tracing::warn!(
                        target: LOG_TARGET,
                        "notifier was None for open connection from source: {}",
                        source
                    );
                    return Err(Error::Disconnected);
                }
            }
        }

        // 2) Otherwise we haven't seen this source address before, it should be Stun, and we return the ICE creds (ufrag, pass)
        match StunMessage::parse(buffer) {
            // .map_err(|op| Error::NetError(str0m::error::NetError::Stun(op)))?;
            Ok(stun) => {
                let (ufrag, pass) = stun
                    .split_username()
                    .ok_or(Error::Authentication)
                    .map_err(|_| Error::InvalidData)?;

                Ok(NewSource::Yes(IceCreds {
                    ufrag: ufrag.to_owned(),
                    pass: pass.to_owned(),
                }))
            }
            Err(err) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    "received non-stun message while opening from source: {}, \n\n err: {} \n\n msg: {:?}",
                    source,
                    err,
                    String::from_utf8_lossy(buffer)
                );
                // TODO: Input::Receive and handle_input? of non-stun?

                Ok(NewSource::No)
            }
        }
        // let (ufrag, pass) = binding
        //     .split_username()
        //     .ok_or(Error::Authentication)
        //     .map_err(|_| Error::InvalidData)?;
        //
        // Ok(NewSource::Yes(IceCreds {
        //     ufrag: ufrag.to_owned(),
        //     pass: pass.to_owned(),
        // }))
    }
}
