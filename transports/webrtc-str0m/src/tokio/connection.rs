//! Connection module.
use std::{
    net::{SocketAddr, UdpSocket},
    ops::Deref,
    sync::Arc,
    time::Instant,
};

use libp2p_identity::PeerId;
use str0m::{
    channel::{ChannelData, ChannelId},
    net::{DatagramSend, Protocol as Str0mProtocol, Receive},
    Event, IceConnectionState, Input, Output, Rtc,
};
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::{channel, error::TrySendError, Sender};

use crate::tokio::Error;

const LOG_TARGET: &str = "libp2p_webrtc_str0m";

pub trait Connectable {
    type Output;

    /// Socket received some data, process it according to the State.
    fn recv_data(&mut self, buf: &[u8]) -> Result<(), Error>;

    /// On transmit data, with associate type for the return output
    /// Handle [`str0m::Output::Transmit`] events.
    fn on_output_transmit(&mut self, transmit: str0m::net::Transmit) -> Self::Output;

    /// Handle Rtc Errors
    fn on_rtc_error(&mut self, error: str0m::RtcError) -> Self::Output;

    /// Handle Rtc Timeout
    fn on_output_timeout(&mut self, timeout: Instant) -> Self::Output;

    fn on_event_ice_disconnect(&self) -> Self::Output;

    fn on_event_channel_open(&mut self, channel_id: ChannelId, name: String) -> Self::Output;

    fn on_event_channel_data(&mut self, data: ChannelData) -> Self::Output;

    fn on_event_channel_close(&mut self, channel_id: ChannelId) -> Self::Output;

    fn on_event_connected(&self) -> Self::Output;

    fn on_event(&self, event: Event) -> Self::Output;
}

/// WebRTC Connection Opening Events
#[derive(Debug)]
pub enum WebRtcEvent {
    /// Register timeout for the connection.
    Timeout {
        /// Timeout.
        timeout: Instant,
    },

    /// Transmit data to remote peer.
    Transmit {
        /// Destination.
        destination: SocketAddr,

        /// Datagram to transmit.
        datagram: DatagramSend,
    },

    /// Connection closed.
    ConnectionClosed,

    /// Connection established.
    ConnectionOpened {
        /// Remote peer ID.
        peer: PeerId,
        // /// Endpoint.
        // endpoint: Endpoint,
    },
    None,
}

/// Opening Connection state.
#[derive(Debug)]
enum HandshakeState {
    /// Connection is poisoned.
    Poisoned,

    /// Connection is closed.
    Closed,

    /// Connection has been opened.
    Opened {
        // /// Noise context.
        // context: NoiseContext,
    },

    /// Local Noise handshake has been sent to peer and the connection
    /// is waiting for an answer.
    HandshakeSent {
        // /// Noise context.
        // context: NoiseContext,
    },

    /// Response to local Noise handshake has been received and the connection
    /// is being validated by `TransportManager`.
    Validating {
        // /// Noise context.
        // context: NoiseContext,
    },
}

/// The Opening Connection state.
pub struct Opening {
    /// TX channel for sending datagrams to the connection event loop.
    tx: Sender<Vec<u8>>,

    /// The Noise Channel Id
    noise_channel_id: ChannelId,

    /// The state of the opening connection handshake
    handshake: HandshakeState,
}

impl Opening {
    /// Creates a new `Opening` state.
    pub fn new(noise_id: ChannelId) -> Self {
        Self {
            tx: channel(1).0,
            noise_channel_id: noise_id,
            handshake: HandshakeState::Closed,
        }
    }
}

/// Peer Address
pub struct PeerAddress(SocketAddr);

/// PeerAddress is a smart pointer, this gets the inner value easily:
impl Deref for PeerAddress {
    type Target = SocketAddr;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// The Open Connection state.
pub struct Open {
    // TX channel for sending datagrams to the connection event loop.
    tx: Sender<Vec<u8>>,
    /// Peer address Newtype
    peer_address: PeerAddress,
    /// Transport socket.
    socket: Arc<UdpSocket>,
    /// RX channel for receiving datagrams from the transport.
    dgram_rx: Receiver<Vec<u8>>,
    /// Remote peer ID.
    peer: PeerId,
}

impl Open {
    /// Creates a new `Open` state.
    pub fn new(
        tx: Sender<Vec<u8>>,
        socket: Arc<UdpSocket>,
        peer_address: PeerAddress,
        dgram_rx: Receiver<Vec<u8>>,
        peer: PeerId,
    ) -> Self {
        Self {
            tx,
            socket,
            dgram_rx,
            peer_address,
            peer,
        }
    }

    /// Try to send buffer data over the channel to the dgram_rx.
    pub(crate) fn try_send<S: Unpin>(
        &self,
        message: Vec<u8>,
    ) -> Result<(), tokio::sync::mpsc::error::TrySendError<Vec<u8>>> {
        Ok(self.tx.try_send(message)?)
    }
}

/// The WebRTC Connections as each of the various states
pub struct Connection<State = Opening> {
    // state: std::marker::PhantomData<State>,
    /// State holds out state-specific values.
    state: State,

    // Shared values
    /// `str0m` WebRTC object.
    rtc: Rtc,
}

impl<State> Unpin for Connection<State> {}

/// Implementations that apply to all Connection states.
impl<State: Connectable> Connection<State> {
    pub fn dgram_recv(&mut self, buf: &[u8]) -> Result<(), Error> {
        // use Open or Opening try_send depending on the state
        self.state.recv_data(buf)
    }

    /// Rtc Poll Output
    fn rtc_poll_output(&mut self) -> <State as crate::tokio::connection::Connectable>::Output {
        let output = match self.rtc.poll_output() {
            Ok(output) => output,
            Err(error) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    // connection_id = ?self.connection_id,
                    ?error,
                    "`WebRtcConnection::poll_progress()` failed",
                );

                return self.state.on_rtc_error(error);
            }
        };
        match output {
            Output::Transmit(transmit) => self.state.on_output_transmit(transmit),
            Output::Timeout(timeout) => self.state.on_output_timeout(timeout),
            Output::Event(e) => match e {
                Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                    self.state.on_event_ice_disconnect()
                }
                Event::ChannelOpen(channel_id, name) => {
                    self.state.on_event_channel_open(channel_id, name)
                }
                Event::ChannelData(data) => self.state.on_event_channel_data(data),
                Event::ChannelClose(channel_id) => self.state.on_event_channel_close(channel_id),
                Event::Connected => self.state.on_event_connected(),
                event => self.state.on_event(event),
            },
        }
    }
    // self.rtc.poll_output().map_err(|e| e.into())
}

/// Implementations that apply only to the Opening Connection state.
impl Connection<Opening> {
    /// Creates a new `Connection` in the Opening state.
    pub fn new(rtc: Rtc, opening: Opening) -> Self {
        Self {
            // state: std::marker::PhantomData,
            state: opening,
            rtc,
        }
    }

    /// Completes the connection opening process.
    /// The only way to get to Open is to go throguh Opening.
    /// Openin> to Open moves values into the Open state.
    pub fn open(self, open: Open) -> Connection<Open> {
        Connection {
            state: open,
            rtc: self.rtc,
        }
    }

    /// Progress the [`Connection`] opening process.
    pub(crate) fn poll_progress(&mut self) -> WebRtcEvent {
        if !self.rtc.is_alive() {
            tracing::debug!(
                target: LOG_TARGET,
                "`Rtc` is not alive, closing `WebRtcConnection`"
            );

            return WebRtcEvent::ConnectionClosed;
        }

        loop {
            let output = match self.rtc.poll_output() {
                Ok(output) => output,
                Err(error) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        // connection_id = ?self.connection_id,
                        ?error,
                        "`WebRtcConnection::poll_progress()` failed",
                    );

                    return WebRtcEvent::ConnectionClosed;
                }
            };
            match output {
                Output::Transmit(transmit) => {
                    tracing::trace!(
                        target: LOG_TARGET,
                        "transmit data",
                    );

                    return WebRtcEvent::Transmit {
                        destination: transmit.destination,
                        datagram: transmit.contents,
                    };
                }
                Output::Timeout(timeout) => return WebRtcEvent::Timeout { timeout },
                Output::Event(e) => {}
            }
        }

        todo!()
    }
}

impl Connectable for Opening {
    type Output = WebRtcEvent;

    fn on_output_transmit(&mut self, transmit: str0m::net::Transmit) -> Self::Output {
        tracing::trace!(
            target: LOG_TARGET,
            "transmit data",
        );
        WebRtcEvent::Transmit {
            destination: transmit.destination,
            datagram: transmit.contents,
        }
    }

    /// Return [WebRtcEvent::ConnectionClosed] when an error occurs.
    fn on_rtc_error(&mut self, error: str0m::RtcError) -> Self::Output {
        tracing::error!(
            target: LOG_TARGET,
            ?error,
            "WebRTC connection error",
        );
        WebRtcEvent::ConnectionClosed
    }

    /// Return [WebRtcEvent::Timeout] when an error occurs while [`Opening`].
    fn on_output_timeout(&mut self, timeout: Instant) -> Self::Output {
        WebRtcEvent::Timeout { timeout }
    }

    /// If ICE Connection State is Disconnected, return [WebRtcEvent::ConnectionClosed].
    fn on_event_ice_disconnect(&self) -> Self::Output {
        tracing::trace!(target: LOG_TARGET, "ice connection closed");
        WebRtcEvent::ConnectionClosed
    }

    /// Socket received some data, process it according to Opening State.
    /// When the [State] is [Opening], we either are getting a [StunMessage] or
    /// some other opening data. After we usher along the data we received, we
    /// need to poll the opening Rtc connection to see what the next event is
    /// and react accordingly.
    fn recv_data(&mut self, message: &[u8]) -> std::result::Result<(), crate::tokio::error::Error> {
        self.tx.try_send(message.to_vec()).map_err(|e| e.into())
    }

    fn on_event_channel_open(&mut self, channel_id: ChannelId, name: String) -> Self::Output {
        tracing::trace!(
            target: LOG_TARGET,
            // connection_id = ?self.connection_id,
            ?channel_id,
            ?name,
            "channel opened",
        );

        if channel_id != self.noise_channel_id {
            tracing::warn!(
                target: LOG_TARGET,
                // connection_id = ?self.connection_id,
                ?channel_id,
                "ignoring opened channel",
            );
            return WebRtcEvent::None;
        }

        // TODO: no expect
        tracing::trace!(target: LOG_TARGET, "send initial noise handshake");

        WebRtcEvent::None
    }

    fn on_event_channel_data(&mut self, data: ChannelData) -> Self::Output {
        todo!()
    }

    fn on_event_channel_close(&mut self, channel_id: ChannelId) -> Self::Output {
        todo!()
    }

    fn on_event_connected(&self) -> Self::Output {
        todo!()
    }

    fn on_event(&self, event: Event) -> Self::Output {
        todo!()
    }
}

/// Implementations that apply only to the Open Connection state.
impl Connection<Open> {
    /// Connection to peer has been closed.
    async fn on_connection_closed(&mut self) {
        tracing::trace!(
            target: LOG_TARGET,
            peer = ?self.state.peer,
            "connection closed",
        );

        // TODO: Report Connection Closed
    }

    /// Runs the connection loop to deal with Transmission and Events
    pub async fn run(mut self) {
        loop {
            // Do something
            tokio::select! {
                            biased;
                            datagram = self.state.dgram_rx.recv() => match datagram {
                                Some(datagram) => {
                                    let input = Input::Receive(
                                        Instant::now(),
                                        Receive {
                                            proto: Str0mProtocol::Udp,
                                            source: *self.state.peer_address,
                                            destination: self.state.socket.local_addr().unwrap(),
                                            contents: datagram.as_slice().try_into().unwrap(),
                                        },
                                    );

                                    self.rtc.handle_input(input).unwrap();
                                }
                                None => {
                                    tracing::trace!(
                                        target: LOG_TARGET,
                                        peer = ?self.state.peer,
                                        "read `None` from `dgram_rx`",
                                    );
                                    return self.on_connection_closed().await;
                                }
                            },
            }
            todo!();
        }
    }
}
