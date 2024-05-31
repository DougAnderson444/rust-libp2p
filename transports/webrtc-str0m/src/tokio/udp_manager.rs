//! This module contains the `UDPManager` struct which is responsible for managing the UDP connections used by the WebRTC transport.
use crate::tokio::fingerprint::Fingerprint;
use crate::tokio::stream::Stream;
use crate::tokio::{
    connection::{Open, Opening, WebRtcEvent},
    error, Connection, Error,
};
use futures::future::BoxFuture;
use futures::FutureExt;
use futures_timer::Delay;
use socket2::{Domain, Socket, Type};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::{
    collections::HashMap,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use std::{sync::Arc, time::Instant};
use str0m::change::DtlsCert;
use str0m::channel::{ChannelConfig, ChannelId};
use str0m::net::Protocol as Str0mProtocol;
use str0m::{
    ice::{IceCreds, StunMessage},
    net::{DatagramRecv, Receive},
    Input,
};
use str0m::{Candidate, Rtc};
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
pub(crate) struct NewRemote {
    pub(crate) addr: SocketAddr,
}

/// Events emitted by the [`UDPManager`].
/// The [`UDPManager`] handles every event internally except for new remote addresses that want to
/// connect on ths socket. For this event, we bubble up to
/// the [Transport](crate::tokio::transport) to handle the new remote so
/// it can be upgraded accordingly.
pub(crate) enum UDPManagerEvent {
    /// A new remote address wants to connect on this socket.
    NewRemoteAddress(Vec<NewRemote>),
    /// Error event.
    Error(error::Error),
}

/// A struct which handles Socket Open Connection process
/// Initial Sender is a futures mpsc Option which is taken for Initialization, The second Sender is the regular sender to send datagrams to the Connection
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

    /// Pending timeouts
    timeouts: HashMap<SocketAddr, BoxFuture<'static, ()>>,

    /// Dtls certificate
    dtls_cert: DtlsCert,
}

/// Whether this is a new connection that should be Polled or not.
enum PollConnection {
    Yes,
    No,
}

/// Events received from opening connections that are handled
/// by the [`WebRtcTransport`] event loop.
enum ConnectionEvent {
    /// Connection established.
    ConnectionEstablished, // { remote_fingerprint: Fingerprint },

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
    pub(crate) fn with_config(addr: SocketAddr, dtls_cert: DtlsCert) -> Result<Self, Error> {
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
            timeouts: Default::default(),
            dtls_cert,
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

        let mut pending_events = Vec::new();
        let mut pending_noise_streams: HashMap<SocketAddr, Stream> = HashMap::new();

        loop {
            tracing::trace!(
                target: LOG_TARGET,
                "Start udp manager poll loop"
            );
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
                        Ok(PollConnection::No) => {
                            /* Existing connection, handled. */
                            tracing::trace!(
                                target: LOG_TARGET,
                                "existing connection, handled: {}",
                                source,
                            );
                        }
                        Ok(PollConnection::Yes) => loop {
                            tracing::trace!(
                                target: LOG_TARGET,
                                "Start poll connection loop"
                            );
                            match this.poll_connection(&source) {
                                ConnectionEvent::ConnectionEstablished => {
                                    tracing::trace!(
                                        target: LOG_TARGET,
                                        "new remote connection established: {}",
                                        source,
                                    );
                                    if pending_events
                                        .iter()
                                        .all(|evt: &NewRemote| evt.addr != source)
                                    {
                                        let evt = NewRemote { addr: source };
                                        pending_events.push(evt);
                                    }
                                }
                                ConnectionEvent::ConnectionClosed => {
                                    this.opening.remove(&source);
                                    this.timeouts.remove(&source);

                                    break;
                                }
                                ConnectionEvent::Timeout { duration } => {
                                    this.timeouts.insert(
                                        source,
                                        Box::pin(async move { Delay::new(duration).await }),
                                    );

                                    tracing::trace!(
                                        target: LOG_TARGET,
                                        "poll connection loop 1 timeout: {:?}",
                                        duration
                                    );

                                    break;
                                }
                            }
                        },
                        Err(error) => {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?source,
                                ?error,
                                "failed to handle datagram",
                            );
                            // return Poll::Ready(UDPManagerEvent::Error(error));
                        }
                    } // match
                } // Poll::Ready(Ok(source))
            } // match
        } // outter loop

        // go over all pending timeouts to see if any of them have expired
        // and if any of them have, poll the connection until it registers another timeout
        let more_pending_events = this
            .timeouts
            .iter_mut()
            .filter_map(
                |(source, mut delay)| match Pin::new(&mut delay).poll_unpin(cx) {
                    Poll::Pending => None,
                    Poll::Ready(_) => Some(*source),
                },
            )
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|source| {
                let mut pending_event = None;

                loop {
                    match this.poll_connection(&source) {
                        ConnectionEvent::ConnectionEstablished => {
                            pending_event = Some(NewRemote { addr: source });
                        }
                        ConnectionEvent::ConnectionClosed => {
                            this.opening.remove(&source);
                            return None;
                        }
                        ConnectionEvent::Timeout { duration } => {
                            this.timeouts.insert(
                                source,
                                Box::pin(async move {
                                    Delay::new(duration);
                                }),
                            );
                            break;
                        }
                    }
                }

                pending_event
            })
            .collect::<VecDeque<_>>();
        // this.timeouts
        //     .retain(|source, _| this.opening.contains_key(source));
        pending_events.extend(more_pending_events);
        // if no pending_events, Poll;, else, return the pending_events
        if pending_events.is_empty() {
            Poll::Pending
        } else {
            Poll::Ready(UDPManagerEvent::NewRemoteAddress(pending_events))
        }
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
    ) -> Result<PollConnection, error::Error> {
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
            return Ok(PollConnection::No);
        }

        if buffer.is_empty() {
            return Err(Error::InvalidData);
        }

        // if the peer doesn't exist, decode the message and expect to receive `Stun`
        // so that a new connection can be initialized
        let contents: DatagramRecv = buffer.try_into().map_err(|_| Error::InvalidData)?;

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

                let destination = self.listen_addr;

                // Create new `Rtc` object for this source
                let rtc =
                    make_rtc_client(ufrag, ufrag, source, destination, self.dtls_cert.clone());
                // Cast the datagram into a str0m::Receive and pass it to the str0m client
                rtc.lock()
                    .map_err(|_| Error::LockPoisoned)?
                    .handle_input(Input::Receive(
                        Instant::now(),
                        Receive {
                            source,
                            proto: Str0mProtocol::Udp,
                            destination,
                            contents,
                        },
                    ))?;

                // make the connecton and insert it into self.opening
                // New Opening Connection
                let connection = Connection::new(rtc.clone(), destination, self.socket(), source);

                // A relay tp this new Open Connection needs to be added to udp_manager
                self.opening.insert(source, connection);

                Ok(PollConnection::Yes)
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

                tracing::trace!(
                    target: LOG_TARGET,
                    "Returning PollCOnnection::YES for source: {}",
                    source,
                );

                Ok(PollConnection::Yes)
            }
        }
    }

    /// Poll opening connection.
    fn poll_connection(&mut self, source: &SocketAddr) -> ConnectionEvent {
        let Some(connection) = self.opening.get_mut(source) else {
            tracing::warn!(
                target: LOG_TARGET,
                ?source,
                "connection doesn't exist",
            );
            return ConnectionEvent::ConnectionClosed;
        };

        loop {
            match connection.poll_progress() {
                WebRtcEvent::Timeout { timeout } => {
                    let duration = timeout - Instant::now();

                    match duration.is_zero() {
                        true => match connection.on_timeout() {
                            Ok(()) => continue,
                            Err(error) => {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?source,
                                    ?error,
                                    "failed to handle timeout",
                                );

                                return ConnectionEvent::ConnectionClosed;
                            }
                        },
                        false => return ConnectionEvent::Timeout { duration },
                    }
                }
                WebRtcEvent::Transmit {
                    destination,
                    datagram,
                } => {
                    if let Err(error) = self.socket.try_send_to(&datagram, destination) {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?source,
                            ?error,
                            "failed to send datagram",
                        );
                    }
                }
                WebRtcEvent::ConnectionClosed => return ConnectionEvent::ConnectionClosed,
                WebRtcEvent::ConnectionOpened => return ConnectionEvent::ConnectionEstablished,
                _ => {}
            }
        }
    }
}

/// Create RTC client and open channel for Noise handshake.
pub(crate) fn make_rtc_client(
    ufrag: &str,
    pass: &str,
    source: SocketAddr,
    destination: SocketAddr,
    dtls_cert: DtlsCert,
) -> Arc<Mutex<Rtc>> {
    let mut rtc = Rtc::builder()
        .set_ice_lite(true)
        .set_dtls_cert(dtls_cert)
        .set_fingerprint_verification(false)
        .build();
    tracing::trace!(target: LOG_TARGET, "source: {}, destination: {}", source, destination);
    rtc.add_local_candidate(Candidate::host(destination, Str0mProtocol::Udp).unwrap());
    rtc.add_remote_candidate(Candidate::host(source, Str0mProtocol::Udp).unwrap());
    rtc.direct_api()
        .set_remote_fingerprint(Fingerprint::from(libp2p_webrtc_utils::Fingerprint::FF).into());
    rtc.direct_api().set_remote_ice_credentials(IceCreds {
        ufrag: ufrag.to_owned(),
        pass: pass.to_owned(),
    });
    rtc.direct_api().set_local_ice_credentials(IceCreds {
        ufrag: ufrag.to_owned(),
        pass: pass.to_owned(),
    });
    rtc.direct_api().set_ice_controlling(false);
    rtc.direct_api().start_dtls(false).unwrap();
    rtc.direct_api().start_sctp(false);

    // let noise_channel_id = rtc.direct_api().create_data_channel(ChannelConfig {
    //     label: "noise".to_string(),
    //     ordered: false,
    //     reliability: Default::default(),
    //     negotiated: Some(0),
    //     protocol: "".to_string(),
    // });

    // (Arc::new(Mutex::new(rtc)), noise_channel_id)
    Arc::new(Mutex::new(rtc))
}

fn is_stun_packet(bytes: &[u8]) -> bool {
    // 20 bytes for the header, then follows attributes.
    bytes.len() >= 20 && bytes[0] < 2
}
