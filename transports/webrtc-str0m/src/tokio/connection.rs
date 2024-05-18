//! Connection module.
//!
//! There are two Stages of the Connection: [`Opening`] and [`Open`],
//! which are each their own modules.
//!
//! The [`Opening`] stage is responsible for the initial handshake with the remote peer. It goes
//! through several [`HandshakeState`]s until the connection is opened. Then Opening connection
//! is moved to Open stage once Noise upgrade is complete.

mod open;
mod opening;

pub(crate) use self::open::{Open, OpenConfig};
pub(crate) use self::opening::Opening;
use crate::tokio::fingerprint::Fingerprint;
use crate::tokio::UdpSocket;

use std::{
    collections::HashMap,
    net::SocketAddr,
    ops::Deref,
    sync::{Arc, Mutex},
    time::Instant,
};

use libp2p_identity::PeerId;
use str0m::{
    channel::{ChannelData, ChannelId},
    net::{DatagramSend, Protocol as Str0mProtocol, Receive},
    Event, IceConnectionState, Input, Output, Rtc,
};
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::{self, Receiver};

use crate::tokio::Error;

use super::channel::{DataChannel, RtcDataChannelState, WakerType};

/// The size of the buffer for incoming datagrams.
const DATAGRAM_BUFFER_SIZE: usize = 1024;

/// The log target for this module.
const LOG_TARGET: &str = "libp2p_webrtc_str0m";

pub trait Connectable {
    type Output;

    /// Returns the [`HandshakeState`] of the connection.
    fn handshake_state(&self) -> HandshakeState;

    /// On transmit data, with associate type for the return output
    /// Handle [`str0m::Output::Transmit`] events.
    fn on_output_transmit(
        &mut self,
        socket: Arc<UdpSocket>,
        transmit: str0m::net::Transmit,
    ) -> Self::Output;

    /// Handle Rtc Errors
    fn on_rtc_error(&mut self, error: str0m::RtcError) -> Self::Output;

    /// Handle Rtc Timeout
    fn on_output_timeout(&mut self, rtc: Arc<Mutex<Rtc>>, timeout: Instant) -> Self::Output;

    fn on_event_ice_disconnect(&self) -> Self::Output;

    /// Store the Channel Id
    fn on_event_channel_open(&mut self, channel_id: ChannelId, name: String) -> Self::Output;

    /// Handles [`str0m::Event::ChannelData`] events
    fn on_event_channel_data(&mut self, data: ChannelData) -> Self::Output;

    /// Handles [`str0m::Event::ChannelClose`] events
    fn on_event_channel_close(&mut self, channel_id: ChannelId) -> Self::Output;

    /// Handles [`str0m::Event::Connected`] events
    fn on_event_connected(&mut self, rtc: Arc<Mutex<Rtc>>) -> Self::Output;

    /// Handles all other [`str0m`] events
    fn on_event(&self, event: Event) -> Self::Output;
}

/// WebRTC Connection Opening Events
#[derive(Debug)]
pub enum OpeningEvent {
    /// Register timeout for the connection.
    Timeout {
        /// Timeout.
        timeout: Instant,
    },

    /// Connection closed.
    ConnectionClosed,

    /// Connection established.
    ConnectionOpened {
        remote_fingerprint: Fingerprint,
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

/// Peer Address
#[derive(Debug)]
pub(crate) struct PeerAddress(pub(crate) SocketAddr);

/// PeerAddress is a smart pointer, this gets the inner value easily:
impl Deref for PeerAddress {
    type Target = SocketAddr;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// The WebRTC Connections as each of the various states
#[derive(Debug)]
pub struct Connection<Stage = Opening> {
    // state: std::marker::PhantomData<Stage>,
    /// Stage goes from Opening to Open. Holds out stage-specific values.
    stage: Stage,

    /// Transport socket.
    socket: Arc<UdpSocket>,

    /// The Channels associated with the Connection
    channels: HashMap<ChannelId, DataChannel>,

    /// Rtc object associated with the connection.
    rtc: Arc<Mutex<Rtc>>,

    /// TX channel for passing along received datagrams by relaying them to the connection event handler.
    relay_dgram: Sender<Vec<u8>>,

    /// RX channel for receiving datagrams from the transport.
    dgram_rx: Receiver<Vec<u8>>,

    /// Peer address Newtype
    peer_address: PeerAddress,

    /// This peer's local address.
    local_address: SocketAddr,
}

impl<Stage> Unpin for Connection<Stage> {}

/// Implementations that apply to both [Stages].
impl<Stage: Connectable> Connection<Stage> {
    /// Getter for Rtc
    pub fn rtc(&self) -> Arc<Mutex<Rtc>> {
        Arc::clone(&self.rtc)
    }

    /// Getter for all channels
    pub fn channels(&mut self) -> &mut HashMap<ChannelId, DataChannel> {
        &mut self.channels
    }

    /// Get a mutable Channel by its ID
    pub fn channel(&mut self, channel_id: &ChannelId) -> &mut DataChannel {
        self.channels.get_mut(channel_id).expect("channel to exist")
    }

    /// Receive a datagram from the socket and process it according to the stage of the connection.
    pub fn dgram_recv(&mut self, buf: &[u8]) -> Result<(), Error> {
        // use Open or Opening depending on the state
        Ok(self.relay_dgram.try_send(buf.to_vec())?)
    }

    /// Progress the [`Connection`] process.
    /// <Stage as ::tokio::connection::Connectable>::Output
    pub(crate) fn poll_progress(
        &mut self,
    ) -> <Stage as crate::tokio::connection::Connectable>::Output {
        if !self.rtc.lock().unwrap().is_alive() {
            tracing::debug!(
                target: LOG_TARGET,
                "`Rtc` is not alive, closing `WebRtcConnection`"
            );

            return self.stage.on_event_ice_disconnect();
        }
        self.rtc_poll_output()
    }

    /// Rtc Poll Output
    fn rtc_poll_output(&mut self) -> <Stage as crate::tokio::connection::Connectable>::Output {
        let out = {
            let mut rtc = self.rtc.lock().unwrap();
            let polled_output = rtc.poll_output();
            match polled_output {
                Ok(output) => output,
                Err(error) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        // connection_id = ?self.connection_id,
                        ?error,
                        "`Connection::rtc_poll_output()` failed",
                    );

                    drop(rtc);
                    return self.stage.on_rtc_error(error);
                }
            }
        };
        match out {
            Output::Transmit(transmit) => {
                self.stage.on_output_transmit(self.socket.clone(), transmit)
            }
            Output::Timeout(timeout) => self.stage.on_output_timeout(self.rtc(), timeout),
            Output::Event(e) => match e {
                Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                    self.stage.on_event_ice_disconnect()
                }
                Event::ChannelOpen(channel_id, name) => {
                    // Create, save a new Channel, set the state to Open
                    self.channels.insert(
                        channel_id,
                        DataChannel::new(channel_id, RtcDataChannelState::Open),
                    );

                    // Trigger ready in PollDataChannel.
                    self.channel(&channel_id).wake(WakerType::Open);

                    // Call the Stage specific handler for Channel Open
                    self.stage.on_event_channel_open(channel_id, name)
                }
                Event::ChannelData(data) => {
                    // 1) data goes in the channel read_buffer for PollDataChannel
                    self.channel(&data.id).set_read_buffer(&data);

                    // 2) Wake the PollDataChannel
                    self.channel(&data.id).wake(WakerType::NewData);

                    self.stage.on_event_channel_data(data)
                }
                Event::ChannelClose(channel_id) => {
                    // 1) Set the channel state to Closed
                    self.channel(&channel_id)
                        .set_state(RtcDataChannelState::Closed);

                    // 2) Wake the PollDataChannel to actually close the channel
                    self.channel(&channel_id).wake(WakerType::Close);

                    // remove the channel from the channels HashMap
                    self.channels.remove(&channel_id);

                    // 3) Deal with the Stage specific handler for Channel Closed event
                    self.stage.on_event_channel_close(channel_id)
                }
                Event::Connected => {
                    let rtc = Arc::clone(&self.rtc);
                    self.stage.on_event_connected(rtc)
                }
                event => self.stage.on_event(event),
            },
        }
    }
}
