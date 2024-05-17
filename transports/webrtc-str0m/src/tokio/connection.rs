//! Connection module.
//!
//! There are two Stages of the Connection: [`Opening`] and [`Open`].
//!
//! The [`Opening`] stage is responsible for the initial handshake with the remote peer. It goes
//! through several [`HandshakeState`]s until the connection is opened.

use crate::tokio::fingerprint::Fingerprint;
use crate::tokio::UdpSocket;

use std::{
    collections::HashMap,
    net::SocketAddr,
    ops::Deref,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use libp2p_identity::PeerId;
use str0m::{
    channel::{ChannelData, ChannelId},
    net::{DatagramSend, Protocol as Str0mProtocol, Receive},
    Event, IceConnectionState, Input, Output, Rtc,
};
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::{channel, Sender};

use crate::tokio::Error;

use super::channel::{DataChannel, RtcDataChannelState, WakerType};

const LOG_TARGET: &str = "libp2p_webrtc_str0m";

pub trait Connectable {
    type Output;

    /// Socket received some data, process it according to the State.
    fn recv_data(&mut self, buf: &[u8]) -> Result<(), Error>;

    /// Returns the [`HandshakeState`] of the connection.
    fn handshake_state(&self) -> HandshakeState;

    /// On transmit data, with associate type for the return output
    /// Handle [`str0m::Output::Transmit`] events.
    fn on_output_transmit(&mut self, transmit: str0m::net::Transmit) -> Self::Output;

    /// Handle Rtc Errors
    fn on_rtc_error(&mut self, error: str0m::RtcError) -> Self::Output;

    /// Handle Rtc Timeout
    fn on_output_timeout(&mut self, timeout: Instant) -> Self::Output;

    fn on_event_ice_disconnect(&self) -> Self::Output;

    /// Store the Channel Id
    fn on_event_channel_open(&mut self, channel_id: ChannelId, name: String) -> Self::Output;

    fn on_event_channel_data(&mut self, data: ChannelData) -> Self::Output;

    fn on_event_channel_close(&mut self, channel_id: ChannelId) -> Self::Output;

    fn on_event_connected(&mut self, rtc: Arc<Mutex<Rtc>>) -> Self::Output;

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
        remote_fingerprint: Fingerprint,
        // /// Endpoint.
        // endpoint: Endpoint,
    },
    None,
}

/// Opening Connection state.
#[derive(Debug, Clone)]
pub enum HandshakeState {
    /// Connection is poisoned.
    Poisoned,

    /// Connection is closed.
    Closed,

    /// Connection has been opened.
    Opened {
        // /// Noise context.
        // context: NoiseContext,
        remote_fingerprint: Fingerprint,
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

/// Events received from opening connections that are handled
/// by the [`WebRtcTransport`] event loop.
pub(crate) enum ConnectionEvent {
    /// Connection established.
    ConnectionEstablished {
        /// Remote peer ID.
        peer: PeerId,
        remote_fingerprint: Fingerprint,
        // // Endpoint.
        // endpoint: Endpoint,
    },

    /// Connection to peer closed.
    ConnectionClosed,

    /// Timeout.
    Timeout {
        /// Timeout duration.
        duration: Duration,
    },
}

/// The Opening Connection state.
#[derive(Debug, Clone)]
pub struct Opening {
    /// TX channel for sending datagrams to the connection event loop.
    tx: Sender<Vec<u8>>,

    /// The Noise Channel Id
    noise_channel_id: ChannelId,

    /// The state of the opening connection handshake
    handshake_state: HandshakeState,
}

impl Opening {
    /// Creates a new `Opening` state.
    pub fn new(noise_channel_id: ChannelId) -> Self {
        Self {
            tx: channel(1).0,
            noise_channel_id,
            handshake_state: HandshakeState::Closed,
        }
    }

    /// Handle timeouts while opening
    pub fn on_timeout(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// Peer Address
#[derive(Debug)]
pub struct PeerAddress(pub SocketAddr);

/// PeerAddress is a smart pointer, this gets the inner value easily:
impl Deref for PeerAddress {
    type Target = SocketAddr;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// The Open Connection state.
#[derive(Debug)]
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
    /// The state of the opening connection handshake
    handshake_state: HandshakeState,
}

impl Open {
    /// Creates a new `Open` state.
    pub fn new(
        tx: Sender<Vec<u8>>,
        socket: Arc<UdpSocket>,
        peer_address: PeerAddress,
        dgram_rx: Receiver<Vec<u8>>,
        peer: PeerId,
        handshake_state: HandshakeState,
    ) -> Self {
        Self {
            tx,
            socket,
            dgram_rx,
            peer_address,
            peer,
            handshake_state,
        }
    }

    /// Try to send buffer data over the channel to the dgram_rx.
    pub(crate) fn try_send<S: Unpin>(
        &self,
        message: Vec<u8>,
    ) -> Result<(), tokio::sync::mpsc::error::TrySendError<Vec<u8>>> {
        self.tx.try_send(message)
    }
}

/// The WebRTC Connections as each of the various states
#[derive(Debug)]
pub struct Connection<Stage = Opening> {
    // state: std::marker::PhantomData<Stage>,
    /// Stage goes from Opening to Open. Holds out stage-specific values.
    stage: Stage,

    /// The Channels associated with the Connection
    channels: HashMap<ChannelId, DataChannel>,

    /// Rtc object associated with the connection.
    rtc: Arc<Mutex<Rtc>>,
}

impl<Stage> Unpin for Connection<Stage> {}

/// Implementations that apply to both [Stages].
impl<Stage: Connectable> Connection<Stage> {
    /// Getter for all channels
    pub fn channels(&mut self) -> &mut HashMap<ChannelId, DataChannel> {
        &mut self.channels
    }

    /// Get a mutable Channel by its ID
    pub fn channel(&mut self, channel_id: ChannelId) -> &mut DataChannel {
        self.channels
            .get_mut(&channel_id)
            .expect("channel to exist")
    }

    pub fn dgram_recv(&mut self, buf: &[u8]) -> Result<(), Error> {
        // use Open or Opening depending on the state
        self.stage.recv_data(buf)
    }

    /// Rtc Poll Output
    fn rtc_poll_output(&mut self) -> <Stage as crate::tokio::connection::Connectable>::Output {
        let out = {
            let mut rtc = self.rtc.lock().unwrap();
            let output = rtc.poll_output();
            match output {
                Ok(output) => output,
                Err(error) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        // connection_id = ?self.connection_id,
                        ?error,
                        "`Connection::rtc_poll_output()` failed",
                    );

                    drop(rtc);
                    let ret = self.stage.on_rtc_error(error);

                    return ret;
                }
            }
        };
        match out {
            Output::Transmit(transmit) => self.stage.on_output_transmit(transmit),
            Output::Timeout(timeout) => self.stage.on_output_timeout(timeout),
            Output::Event(e) => match e {
                Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                    self.stage.on_event_ice_disconnect()
                }
                Event::ChannelOpen(channel_id, name) => {
                    // Set the channel state to Open
                    self.channels.insert(
                        channel_id,
                        DataChannel::new(channel_id, RtcDataChannelState::Open),
                    );

                    // Trigger read in PollDataChannel.
                    self.channels
                        .get(&channel_id)
                        .unwrap()
                        .wake(WakerType::Open);

                    // Call the Stage specific handler for Channel Open
                    self.stage.on_event_channel_open(channel_id, name)
                }
                Event::ChannelData(data) => {
                    // 1) data goes in the channel read_buffer for PollDataChannel
                    self.channels
                        .get_mut(&data.id)
                        .unwrap()
                        .set_read_buffer(&data);

                    // 2) Wake the PollDataChannel
                    self.channels
                        .get(&data.id)
                        .unwrap()
                        .wake(WakerType::NewData);

                    self.stage.on_event_channel_data(data)
                }
                Event::ChannelClose(channel_id) => self.stage.on_event_channel_close(channel_id),
                Event::Connected => {
                    let rtc = Arc::clone(&self.rtc);
                    self.stage.on_event_connected(rtc)
                }
                event => self.stage.on_event(event),
            },
        }
    }
}

/// Configure the Open stage:
#[derive(Debug)]
pub struct OpenConfig {
    /// Transport socket.
    pub socket: Arc<UdpSocket>,
    /// RX channel for receiving datagrams from the transport.
    pub dgram_rx: Receiver<Vec<u8>>,
    /// Remote peer ID.
    pub peer: PeerId,
    /// The state of the opening connection handshake
    pub handshake_state: HandshakeState,
    /// The Peer Socket Address
    pub peer_address: PeerAddress,
}

/// Implementations that apply only to the Opening Connection state.
impl Connection<Opening> {
    /// Creates a new `Connection` in the Opening state.
    pub fn new(rtc: Arc<Mutex<Rtc>>, opening: Opening) -> Self {
        Self {
            rtc,
            stage: opening,
            channels: HashMap::new(),
        }
    }

    /// Completes the connection opening process.
    /// The only way to get to Open is to go throguh Opening.
    /// Openin> to Open moves values into the Open state.
    pub fn open(self, config: OpenConfig) -> Connection<Open> {
        Connection {
            rtc: self.rtc,
            channels: self.channels,
            stage: Open {
                tx: self.stage.tx,
                socket: config.socket,
                dgram_rx: config.dgram_rx,
                peer_address: config.peer_address,
                peer: config.peer,
                handshake_state: config.handshake_state,
            },
        }
    }

    /// Handle timeout
    pub fn on_timeout(&mut self) -> Result<(), Error> {
        if let Err(error) = self
            .rtc
            .lock()
            .unwrap()
            .handle_input(Input::Timeout(Instant::now()))
        {
            tracing::error!(
                target: LOG_TARGET,
                ?error,
                "failed to handle timeout for `Rtc`"
            );

            self.rtc.lock().unwrap().disconnect();
            return Err(Error::Disconnected);
        }

        Ok(())
    }

    /// Progress the [`Connection`] opening process.
    pub(crate) fn poll_progress(&mut self) -> WebRtcEvent {
        if !self.rtc.lock().unwrap().is_alive() {
            tracing::debug!(
                target: LOG_TARGET,
                "`Rtc` is not alive, closing `WebRtcConnection`"
            );

            return WebRtcEvent::ConnectionClosed;
        }

        loop {
            match self.rtc_poll_output() {
                WebRtcEvent::None => {
                    continue;
                }
                evt => return evt,
            }
        }
    }
}

impl Connectable for Opening {
    type Output = WebRtcEvent;

    /// Returns the [`HandshakeState`] of the connection.
    fn handshake_state(&self) -> HandshakeState {
        self.handshake_state.clone()
    }

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

    /// Progress the opening of the channel, as applicable.
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

        // let Stage::Opened { mut context } = std::mem::replace(&mut self.state, State::Poisoned)
        // else {
        //     return Err(Error::InvalidState);
        // };
        //
        // let HandshakeState::Opened {} =
        //     std::mem::replace(&mut self.handshake, HandshakeState::Opened {});

        WebRtcEvent::None
    }

    fn on_event_channel_data(&mut self, data: ChannelData) -> Self::Output {
        todo!()
    }

    fn on_event_channel_close(&mut self, channel_id: ChannelId) -> Self::Output {
        todo!()
    }

    fn on_event_connected(&mut self, rtc: Arc<Mutex<Rtc>>) -> Self::Output {
        match std::mem::replace(&mut self.handshake_state, HandshakeState::Poisoned) {
            // Initial State should be Closed before we connect
            HandshakeState::Closed => {
                let remote_fp: Fingerprint = rtc
                    .lock()
                    .unwrap()
                    .direct_api()
                    .remote_dtls_fingerprint()
                    .clone()
                    .expect("fingerprint to exist")
                    .into();
                let remote_fingerprint_mh_bytes = remote_fp.to_multihash().to_bytes();

                let local_fp: Fingerprint = rtc
                    .lock()
                    .unwrap()
                    .direct_api()
                    .local_dtls_fingerprint()
                    .into();
                let local_fp_mh_bytes = local_fp.to_multihash().to_bytes();

                // let peer_id = noise::inbound(id_keys, data_channel, remote_fp, local_fp).await?;
                tracing::debug!(
                    target: LOG_TARGET,
                    // peer = ?self.peer_address,
                    "connection opened",
                );

                self.handshake_state = HandshakeState::Opened {
                    remote_fingerprint: remote_fp,
                };

                WebRtcEvent::ConnectionOpened {
                    peer: todo!(),
                    remote_fingerprint: remote_fp,
                }
            }
            state => {
                tracing::warn!(
                    target: LOG_TARGET,
                    // connection_id = ?self.connection_id,
                    ?state,
                    "unexpected handshake state, invalid state for connection, should be closed",
                );

                return WebRtcEvent::ConnectionClosed;
            }
        }
    }

    fn on_event(&self, event: Event) -> Self::Output {
        todo!()
    }
}

/// Impl Connectable for Open
impl Connectable for Open {
    type Output = WebRtcEvent;

    /// Returns the [`HandshakeState`] of the connection.
    fn handshake_state(&self) -> HandshakeState {
        self.handshake_state.clone()
    }

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

    /// Progress the opening of the channel, as applicable.
    fn on_event_channel_open(&mut self, channel_id: ChannelId, name: String) -> Self::Output {
        tracing::trace!(
            target: LOG_TARGET,
            // connection_id = ?self.connection_id,
            ?channel_id,
            ?name,
            "channel opened",
        );

        WebRtcEvent::None
    }

    fn on_event_channel_data(&mut self, data: ChannelData) -> Self::Output {
        todo!()
    }

    fn on_event_channel_close(&mut self, channel_id: ChannelId) -> Self::Output {
        todo!()
    }

    fn on_event(&self, event: Event) -> Self::Output {
        todo!()
    }

    fn on_event_connected(&mut self, rtc: Arc<Mutex<Rtc>>) -> Self::Output {
        todo!()
    }
}

/// Implementations that apply only to the Open Connection state.
impl Connection<Open> {
    /// Connection to peer has been closed.
    async fn on_connection_closed(&mut self) {
        tracing::trace!(
            target: LOG_TARGET,
            peer = ?self.stage.peer,
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
                            datagram = self.stage.dgram_rx.recv() => match datagram {
                                Some(datagram) => {
                                    let input = Input::Receive(
                                        Instant::now(),
                                        Receive {
                                            proto: Str0mProtocol::Udp,
                                            source: *self.stage.peer_address,
                                            destination: self.stage.socket.local_addr().unwrap(),
                                            contents: datagram.as_slice().try_into().unwrap(),
                                        },
                                    );

                                    self.rtc
                            .lock()
                            .unwrap()
                            .handle_input(input).unwrap();
                                }
                                None => {
                                    tracing::trace!(
                                        target: LOG_TARGET,
                                        peer = ?self.stage.peer,
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
