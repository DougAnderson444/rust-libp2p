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

use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use libp2p_core::muxing::{StreamMuxer, StreamMuxerEvent};
use libp2p_identity::PeerId;
use std::cell::RefCell;
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

use super::channel::{
    ChannelDetails, ChannelWakers, RtcDataChannelState, StateInquiry, StateUpdate,
};
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

    /// Channel details by channel_id, such as wakers, data receivers, and state change receivers.
    channel_details: HashMap<ChannelId, Mutex<ChannelDetails>>,

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

    /// Channel for state inquiries for [ChannelId]. We need an inquiry [Sender] for
    /// each channel, as each channel has a unique state.
    ///
    /// When a [PollDataChannel] is created, it will be passed a clone of this [Sender]
    /// `state_inquiry_channel.0.clone()` and will use it to send inquiries about the state of the channel.
    tx_state_inquiry: Option<mpsc::Sender<StateInquiry>>,

    /// Receiver for state updates from a [PollDataChannel]. We only need one of these
    /// as this single Connection will be the only one sending updates.
    tx_state_update: Option<mpsc::Sender<StateUpdate>>,
}

impl<Stage> Unpin for Connection<Stage> {}

/// Implementations that apply to both [Stages].
impl<Stage: Connectable> Connection<Stage> {
    /// Receive a datagram from the socket and process it according to the stage of the connection.
    pub fn dgram_recv(&mut self, buf: &[u8]) -> Result<(), Error> {
        // use Open or Opening depending on the state
        self.relay_dgram
            .try_send(buf.to_vec())
            .map_err(|_| Error::Disconnected)
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

                        let channel_details = ChannelDetails {
                            state: RtcDataChannelState::Opening,
                            wakers: ChannelWakers::default(),
                            channel_data_rx: Mutex::new(futures::channel::mpsc::channel(1).1),
                            channel_state_rx: Mutex::new(futures::channel::mpsc::channel(1).1),
                        };

                        // set the channel state to Open RtcDataChannelState::Open;
                        match self.tx_state_update.as_ref() {
                            Some(tx) => {
                                let message = StateUpdate {
                                    channel_id,
                                    state: RtcDataChannelState::Open,
                                };
                                tx.try_send(message).unwrap();
                            }
                            None => {
                                tracing::error!(
                                    "No state update channel for channel_id: {:?}",
                                    channel_id
                                );
                            }
                        }

                        while let Ok(Some(reply)) =
                            channel_details.channel_state_rx.lock().unwrap().try_next()
                        {
                            reply.response.send(RtcDataChannelState::Open).unwrap();
                        }

                        // Call any Stage specific handler for Channel Open event
                        self.stage.on_event_channel_open(channel_id, name)
                    }
                    Event::ChannelData(data) => {
                        // Data goes from this Connection
                        // into the read_buffer for PollDataChannel for this channel_id

                        let channel_details = self.channel_details.get(&data.id).expect(
                            "ChannelDetails should be present in the Connection struct for the channel_id",
                        ).lock().unwrap();

                        channel_details.wakers.new_data.wake();

                        // Wait on the reply handle to reply with data
                        match channel_details.channel_data_rx.lock().unwrap().try_next() {
                            Ok(Some(rx)) => {
                                rx.response.send(data.data.clone()).unwrap();
                            }
                            _ => {
                                tracing::error!(
                                    "No channel data receiver for channel_id: {:?}",
                                    data.id
                                );
                            }
                        }

                        self.stage.on_event_channel_data(data)
                    }
                    Event::ChannelClose(channel_id) => {
                        let channel_details = self.channel_details.get(&channel_id).expect(
                            "ChannelDetails should be present in the Connection struct for the channel_id",
                        );

                        // State Change to Closed
                        match self.tx_state_update.as_ref() {
                            Some(tx) => {
                                let message = StateUpdate {
                                    channel_id,
                                    state: RtcDataChannelState::Closed,
                                };
                                tx.try_send(message).unwrap();
                            }
                            None => {
                                tracing::error!(
                                    "No state update channel for channel_id: {:?}",
                                    channel_id
                                );
                            }
                        }

                        // if we do this here, PollDataChannel will not be able to
                        // get the channel state
                        // self.channel_details.remove(&channel_id);

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

// TODO: Connectable trait?
impl<Stage> Connection<Stage> {
    /// Creates a new `Connection` in the Opening state.
    pub fn new(
        rtc: Arc<Mutex<Rtc>>,
        socket: Arc<UdpSocket>,
        source: SocketAddr,
        // as long as stage impl Connectable, we can use it here
        stage: Stage,
    ) -> Self {
        // Create a channel for sending datagrams to the connection event handler.
        let (relay_dgram, dgram_rx) = mpsc::channel(DATAGRAM_BUFFER_SIZE);
        let (tx_ondatachannel, rx_ondatachannel) = futures::channel::mpsc::channel(1);

        let local_address = socket.local_addr().unwrap();

        Self {
            rtc,
            socket,
            stage,
            relay_dgram,
            dgram_rx,
            peer_address: PeerAddress(source),
            local_address,
            tx_ondatachannel,
            rx_ondatachannel,
            drop_listeners: Default::default(),
            no_drop_listeners_waker: Default::default(),
            channel_details: Default::default(),
            tx_state_inquiry: None,
            tx_state_update: None,
        }
    }

    /// Getter for Rtc
    pub fn rtc(&self) -> Arc<Mutex<Rtc>> {
        Arc::clone(&self.rtc)
    }

    /// Spawns a tokio task which listens on the given mpsc receiver for incoming
    /// inquiries about self.state of a channel_id, and responds with the current state.
    pub fn state_loop(&mut self) {
        // Make the state_inquiry channel
        let (tx_state_inquiry, mut rx_state_inquiry) = mpsc::channel::<StateInquiry>(4);

        let (tx_state_update, mut rx_state_update) = mpsc::channel::<StateUpdate>(1);

        self.tx_state_inquiry = Some(tx_state_inquiry);
        self.tx_state_update = Some(tx_state_update);

        tokio::spawn(async move {
            let mut channel_details: HashMap<ChannelId, RefCell<ChannelDetails>> = HashMap::new();
            // lopp on tokio select on receiving state inquiries and updates
            loop {
                tokio::select! {
                    Some(update) = rx_state_update.recv() => {
                        // Update the state of the channel
                        let mut channel_details = channel_details.get(&update.channel_id).expect("ChannelDetails should be present in the Connection struct for the channel_id").borrow_mut();
                        channel_details.state = update.state;
                    }
                    Some(inquiry) = rx_state_inquiry.recv() => {
                        // Respond with the current state of the channel
                        let deets = channel_details.get(&inquiry.channel_id).expect("ChannelDetails should be present in the Connection struct for the channel_id").borrow();
                        match inquiry.response.send(deets.state.clone()) {
                            Ok(_) => {
                                if let RtcDataChannelState::Closed = deets.state {
                                    drop(deets);
                                    // Remove the channel from the channel_details
                                    channel_details.remove(&inquiry.channel_id);
                                }
                            }
                            Err(e) => {
                                tracing::error!("Failed to send channel state response: {:?}", e);
                            }
                        }

                    }
                }
            }
        });
    }

    /// New Stream from Data Channel ID
    pub fn new_stream_from_data_channel_id(
        &mut self,
        channel_id: ChannelId,
    ) -> Result<Stream, Error> {
        let (stream, drop_listener, mut waker_rx, reply_rx, channel_state_rx) =
            Stream::new(channel_id, RtcDataChannelState::Open, self.rtc.clone())
                .map_err(|_| Error::StreamCreationFailed)?;

        let mut channel_details = ChannelDetails {
            state: RtcDataChannelState::Opening,
            wakers: ChannelWakers::default(),
            channel_data_rx: reply_rx,
            channel_state_rx,
        };

        match waker_rx.try_next() {
            Ok(Some(wakers)) => {
                // Track the waker and the drop listener for this channel
                channel_details.wakers = wakers;
            }
            Ok(None) => {
                tracing::debug!("Channel sent no wakers, that's weird");
            }
            Err(e) => {
                tracing::error!("Failed to receive wakers from PollDataChanell: {:?}", e);
            }
        }

        self.channel_details
            .insert(channel_id, Mutex::new(channel_details));

        // The Stream also gives us the mpsc::Receiver<ReadReady>
        // to send data to the PollDataChannel, we need to store this
        // in self so we can rx.try_next() to know when to reply
        // with the data once the waker as woken the; PollDataChannel poller
        // self.channel_data_rx.insert(channel_id, reply_rx);

        // Track the drop listener
        // TODO: Verify that noise channels are dropped correctly from this list
        self.drop_listeners.push(drop_listener);

        // Wake .poll()
        if let Some(waker) = self.no_drop_listeners_waker.take() {
            waker.wake()
        }
        Ok(stream)
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
                let stream = self.new_stream_from_data_channel_id(channel_id)?;
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
