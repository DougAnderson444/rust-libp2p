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
use crate::tokio::stream::ReadReady;
use crate::tokio::UdpSocket;

use futures::stream::FuturesUnordered;
use futures::StreamExt;
use libp2p_core::muxing::{StreamMuxer, StreamMuxerEvent};
use libp2p_identity::PeerId;
use std::task::{ready, Context, Poll, Waker};
use std::{
    collections::HashMap,
    net::SocketAddr,
    ops::Deref,
    sync::{Arc, Mutex},
    time::Instant,
};
use str0m::{
    channel::{ChannelData, ChannelId},
    net::{Protocol as Str0mProtocol, Receive},
    Event, IceConnectionState, Input, Output, Rtc,
};
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::{self, Receiver};

use crate::tokio::Error;

use super::channel::{ChannelWakers, RtcDataChannelState};
use super::stream::{DropListener, Stream};

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

    /// Handles [`str0m::Event::IceConnectionStateChange`] `IceConnectionStateChange::disonnected` event.
    fn on_event_ice_disconnect(&self) -> Self::Output;

    /// Handles [`str0m::Event::ChannelOpen`] events
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
    channel_wakers: HashMap<ChannelId, ChannelWakers>,

    /// [ReadReady] channel data receivers for each channel
    channel_data_rx: HashMap<ChannelId, Mutex<futures::channel::mpsc::Receiver<ReadReady>>>,

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

    /// Inbound Data Channels, a future that notifies the StreamMuxer that there are incoming channels ready
    pub(crate) rx_ondatachannel: futures::channel::mpsc::Receiver<ChannelId>,

    /// Transmitter to notify StreamMuxer that there is a new channel opened
    tx_ondatachannel: futures::channel::mpsc::Sender<ChannelId>,

    /// A list of futures, which, once completed, signal that a [`Stream`] has been dropped.
    drop_listeners: FuturesUnordered<DropListener>,

    /// Is set when there are no drop listeners,
    no_drop_listeners_waker: Option<Waker>,
}

impl<Stage> Unpin for Connection<Stage> {}

/// Implementations that apply to both [Stages].
impl<Stage: Connectable> Connection<Stage> {
    /// Receive a datagram from the socket and process it according to the stage of the connection.
    pub fn dgram_recv(&mut self, buf: &[u8]) -> Result<(), Error> {
        // use Open or Opening depending on the state
        Ok(self
            .relay_dgram
            .try_send(buf.to_vec())
            .map_err(|_| Error::Disconnected)?)
    }

    /// Report the connection as closed.
    pub fn report_connection_closed(&self) {
        todo!()
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

            // First handle connection level close
            self.report_connection_closed();
            // Next handle the Stage specific close
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
            Output::Event(e) => {
                match e {
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        // First handle connection level close
                        self.report_connection_closed();
                        // Next handle the Stage specific close
                        self.stage.on_event_ice_disconnect()
                    }
                    Event::ChannelOpen(channel_id, name) => {
                        tracing::trace!(
                            target: LOG_TARGET,
                            // connection_id = ?self.connection_id,
                            ?channel_id,
                            ?name,
                            "channel opened",
                        );
                        // Use the waker to notify the PollDataChannel that the channel is open
                        // (ready to read and write data)
                        // This needs to be a waker, as the PollDataChannel can't have
                        // Receiver<> in it's state (must be :Clone).
                        if let Some(w) = self.channel_wakers.get(&channel_id) {
                            w.open.wake()
                        }

                        // Call the Stage specific handler for Channel Open (if any)
                        self.stage.on_event_channel_open(channel_id, name)
                    }
                    Event::ChannelData(data) => {
                        // Data goes from this Connection
                        // into the read_buffer for PollDataChannel for this channel_id

                        // Wake the PollDataChannel for new data
                        if let Some(w) = self.channel_wakers.get(&data.id) {
                            w.new_data.wake()
                        }

                        // Wait on the reply handle to reply with data
                        match self.channel_data_rx.get(&data.id) {
                            Some(rx) => {
                                match rx.lock().unwrap().try_next() {
                                    Ok(Some(ready)) => {
                                        ready.response.send(data.data.clone()).unwrap();
                                    }
                                    Ok(None) => {
                                        tracing::debug!("No data to send to PollDataChannel, sending empty data");
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            "Connection event loop failed to send channel data to PollDataChannel: {:?}",
                                            e
                                        );
                                    }
                                }
                            }
                            None => {
                                tracing::error!(
                                    "No channel data receiver for channel_id: {:?}",
                                    data.id
                                );
                            }
                        }

                        self.stage.on_event_channel_data(data)
                    }
                    Event::ChannelClose(channel_id) => {
                        // Wake the PollDataChannel to actually close the channel
                        self.channel_wakers.get(&channel_id).map(|w| w.close.wake());

                        // remove the channel wakers from the wakers HashMap
                        self.channel_wakers.remove(&channel_id);

                        // Deal with the Stage specific handler for Channel Closed event
                        self.stage.on_event_channel_close(channel_id)
                    }
                    Event::Connected => {
                        let rtc = Arc::clone(&self.rtc);
                        self.stage.on_event_connected(rtc)
                    }
                    event => self.stage.on_event(event),
                }
            }
        }
    }
}

impl<Stage> Connection<Stage> {
    /// Getter for Rtc
    pub fn rtc(&self) -> Arc<Mutex<Rtc>> {
        Arc::clone(&self.rtc)
    }

    /// New Stream from Data Channel ID
    fn new_stream_from_data_channel_id(&mut self, channel_id: ChannelId) -> Stream {
        let (stream, drop_listener, mut rx, reply_rx) =
            Stream::new(channel_id, RtcDataChannelState::Open, self.rtc.clone()).unwrap();

        match rx.try_next() {
            Ok(Some(wakers)) => {
                // Track the waker and the drop listener for this channel
                self.channel_wakers.insert(channel_id, wakers);
                // The Stream also gives us the mpsc::Receiver<ReadReady>
                // to send data to the PollDataChannel, we need to store this
                // in self so we can rx.try_next() to know when to reply
                // with the data once the waker as woken the; PollDataChannel poller
                self.channel_data_rx.insert(channel_id, reply_rx);
            }
            Ok(None) => {
                tracing::debug!("Channel sent no wakers, that's weird");
            }
            Err(e) => {
                tracing::error!("Failed to receive wakers from PollDataChanell: {:?}", e);
            }
        }

        // Track the drop listener
        self.drop_listeners.push(drop_listener);

        // Wake .poll()
        if let Some(waker) = self.no_drop_listeners_waker.take() {
            waker.wake()
        }
        stream
    }
}

/// WebRTC native multiplexing of [Open] [Connection]s.
/// Allow users to open their substreams
impl<Stage> StreamMuxer for Connection<Stage> {
    type Substream = Stream;
    type Error = Error;

    fn poll_inbound(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<Self::Substream, Self::Error>> {
        // wait for inbound data channels to be ready
        match ready!(self.rx_ondatachannel.poll_next_unpin(cx)) {
            Some(channel_id) => {
                let stream = self.new_stream_from_data_channel_id(channel_id);
                Poll::Ready(Ok(stream))
            }
            None => {
                // No more channels to poll
                tracing::debug!("`Sender` for inbound data channels has been dropped");
                Poll::Ready(Err(Error::Disconnected))
            }
        }
    }

    fn poll_outbound(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::Substream, Self::Error>> {
        todo!()
    }

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        todo!()
    }

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<StreamMuxerEvent, Self::Error>> {
        loop {
            match ready!(self.drop_listeners.poll_next_unpin(cx)) {
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    tracing::debug!("a DropListener failed: {e}")
                }
                None => {
                    self.no_drop_listeners_waker = Some(cx.waker().clone());
                    return Poll::Pending;
                }
            }
        }
    }
}
