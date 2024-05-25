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
use crate::tokio::channel::{InquiryType, StateValues};
use crate::tokio::fingerprint::Fingerprint;
use crate::tokio::UdpSocket;

use futures::stream::FuturesUnordered;
use futures::StreamExt;
use libp2p_core::muxing::{StreamMuxer, StreamMuxerEvent};
use libp2p_identity::PeerId;
use std::borrow::BorrowMut;
use std::cell::RefCell;
use std::sync::MutexGuard;
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

use super::channel::{ChannelDetails, ChannelWakers, Inquiry, RtcDataChannelState, StateUpdate};
use super::stream::{DropListener, Stream};

/// The size of the buffer for incoming datagrams.
const DATAGRAM_BUFFER_SIZE: usize = 1024;

/// The log target for this module.
const LOG_TARGET: &str = "libp2p_webrtc_str0m";

pub trait Connectable {
    type Output: Default;

    // enable implementations to return Output default
    fn default() -> Self::Output {
        Default::default()
    }

    /// Handle Rtc Errors
    fn on_rtc_error(&mut self, error: str0m::RtcError) -> Self::Output;

    /// Handle Rtc Timeout
    fn on_output_timeout(&mut self, rtc: Arc<Mutex<Rtc>>, timeout: Instant) -> Self::Output;

    /// Handles [`str0m::Event::IceConnectionStateChange`] `IceConnectionStateChange::disonnected` event.
    fn on_event_ice_disconnect(&self) -> Self::Output;

    /// Handles [`str0m::Event::ChannelOpen`] events.
    /// Opening handles remote fingerprint, open does nothing.
    fn on_event_channel_open(&mut self, channel_id: ChannelId, name: String) -> Self::Output;

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
    ConnectionOpened { remote_fingerprint: Fingerprint },
    /// This is the default
    None,
}

impl Default for OpeningEvent {
    fn default() -> Self {
        Self::None
    }
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
    // / `state_inquiry_channel.0.clone()` and will use it to send inquiries about the state of the channel.
    tx_state_inquiry: mpsc::Sender<Inquiry>,

    /// Receiver for state updates from a [PollDataChannel]. We only need one of these
    /// as this single Connection will be the only one sending updates.
    tx_state_update: mpsc::Sender<StateUpdate>,
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
                if let Err(error) = self
                    .socket
                    .try_send_to(&transmit.contents, transmit.destination)
                {
                    tracing::warn!(
                        target: LOG_TARGET,
                        ?error,
                        "failed to send connection datagram",
                    );

                    // TODO: return Broken Pipe?
                }

                <Stage as crate::tokio::connection::Connectable>::Output::default()
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

                        // set the channel state to Open RtcDataChannelState::Open;
                        let message = StateUpdate {
                            channel_id,
                            state: StateValues::RtcState(RtcDataChannelState::Open),
                        };

                        if let Err(e) = self.tx_state_update.try_send(message) {
                            tracing::error!(
                                "No state update channel for channel_id: {:?}, {:?}",
                                channel_id,
                                e
                            );
                        }

                        // notify StreamMuxer using tx_ondatachannel
                        if let Err(e) = self
                            .tx_ondatachannel
                            .try_send(channel_id)
                            .map_err(|_| Error::Disconnected)
                        {
                            tracing::error!("Failed to send channel_id to StreamMuxer: {:?}", e);
                        }

                        // shake open waker to prompt PollDataChannel to re-poll
                        if let Some(ch) = self.channel(channel_id) {
                            ch.wakers.open.wake()
                        }

                        // Call any Stage specific handler for Channel Open event
                        <Stage as crate::tokio::connection::Connectable>::Output::default()
                    }
                    Event::ChannelData(data) => {
                        // Data goes from this Connection
                        // into the read_buffer for PollDataChannel for this channel_id
                        // through the state_loop.
                        tracing::trace!(
                            ?data.id,
                            "channel data",
                        );

                        match self.channel_details.get(&data.id) {
                            None => {
                                tracing::error!("No channel details for channel_id: {:?}", data.id);
                            }
                            Some(deets) => {
                                // send to state_loop then wake the PollDataChannel
                                let message = StateUpdate {
                                    channel_id: data.id,
                                    state: StateValues::ReadBuffer(data.data.clone()),
                                };

                                if let Err(e) = self.tx_state_update.try_send(message) {
                                    tracing::error!(
                                        "No state update channel for channel_id: {:?}, {:?}",
                                        data.id,
                                        e
                                    );
                                }

                                // wake the PollDataChannel using deets
                                deets.lock().unwrap().wakers.new_data.wake();
                            }
                        }

                        <Stage as crate::tokio::connection::Connectable>::Output::default()
                    }
                    Event::ChannelClose(channel_id) => {
                        tracing::trace!(
                            target: LOG_TARGET,
                            // connection_id = ?self.connection_id,
                            ?channel_id,
                            "channel closed",
                        );

                        match self.channel_details.get(&channel_id) {
                            None => {
                                tracing::error!(
                                    "No channel details for channel_id: {:?}",
                                    channel_id
                                );
                            }
                            Some(deets) => {
                                // send to state_loop then wake the PollDataChannel
                                let message = StateUpdate {
                                    channel_id,
                                    state: StateValues::RtcState(RtcDataChannelState::Closed),
                                };

                                if let Err(e) = self.tx_state_update.try_send(message) {
                                    tracing::error!(
                                        "No state update channel for channel_id: {:?}, {:?}",
                                        channel_id,
                                        e
                                    );
                                }

                                // wake the PollDataChannel using deets
                                deets.lock().unwrap().wakers.open.wake();
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
    /// Getter for Rtc
    pub fn rtc(&self) -> Arc<Mutex<Rtc>> {
        Arc::clone(&self.rtc)
    }

    /// Get the channel details for a channel_id.
    ///
    /// Convenience method for:
    /// ```
    ///self.channel_details
    /// .get(&channel_id)?
    /// .lock()
    /// .unwrap()
    /// ```
    pub(crate) fn channel(&self, id: ChannelId) -> Option<MutexGuard<ChannelDetails>> {
        Some(self.channel_details.get(&id)?.lock().unwrap())
    }

    /// New Stream from Data Channel ID
    pub fn new_stream_from_data_channel_id(
        &mut self,
        channel_id: ChannelId,
    ) -> Result<Stream, Error> {
        let wakers = ChannelWakers::default();

        let (stream, drop_listener) = Stream::new(
            channel_id,
            self.rtc.clone(),
            self.tx_state_inquiry.clone(),
            wakers.clone(),
        )
        .map_err(|_| Error::StreamCreationFailed)?;

        self.channel_details.insert(
            channel_id,
            Mutex::new(ChannelDetails {
                state: RtcDataChannelState::Opening,
                wakers,
                read_buffer: Default::default(),
            }),
        );

        // TODO: Verify that noise channels are dropped correctly from this list
        self.drop_listeners.push(drop_listener);

        // Wake .poll()
        if let Some(waker) = self.no_drop_listeners_waker.take() {
            waker.wake()
        }
        Ok(stream)
    }
}

/// Spawns a tokio task which listens on the given mpsc receiver for incoming
/// inquiries about self.state of a channel_id, and responds with the current state.
pub(crate) fn state_loop(
    mut rx_state_update: Receiver<StateUpdate>,
    mut rx_state_inquiry: Receiver<Inquiry>,
) {
    tokio::spawn(async move {
        let mut channel_details: HashMap<ChannelId, RefCell<ChannelDetails>> = HashMap::new();
        loop {
            tokio::select! {
                Some(update) = rx_state_update.recv() => {
                    // Update the state of the channel
                    let mut channel_details = channel_details.get(&update.channel_id).expect("ChannelDetails should be present in the Connection struct for the channel_id").borrow_mut();
                    match update.state {
                        StateValues::RtcState(state) => {
                            channel_details.state = state;
                        }
                        StateValues::ReadBuffer(data) => {
                            let mut read_buffer = channel_details.read_buffer.lock().unwrap();
                            read_buffer.extend_from_slice(&data);
                        }
                    }
                }
                Some(inquiry) = rx_state_inquiry.recv() => {
                    // match on the inquiry type first,
                    // if no channel_details, return an error using the inquiry response channel of
                    // the ty.
                    match inquiry.ty {
                        InquiryType::State(state_inquiry) => {
                            match channel_details.get(&inquiry.channel_id) {
                                None => {
                                    if let Err(err) = state_inquiry.response.send(RtcDataChannelState::Closed) {
                                        tracing::error!("Failed to send state inquiry response: {:?}", err);
                                    }
                                }
                                Some(deets) => {
                                    let borrowed_details = deets.borrow_mut();
                                    // Err if fail, remove channel_id from HashMap if Closed was sent
                                    match state_inquiry.response.send(borrowed_details.state.clone()) {
                                        Err(err) => {
                                            tracing::error!("Failed to send state inquiry response: {:?}", err);
                                        }
                                        Ok(_) => {
                                            if borrowed_details.state == RtcDataChannelState::Closed {
                                                drop(borrowed_details);
                                                channel_details.borrow_mut().remove(&inquiry.channel_id);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        InquiryType::ReadBuffer(read_inquiry) => {
                            match channel_details.get(&inquiry.channel_id) {
                                None => {
                                    if let Err(err) = read_inquiry.response.send(Vec::new()) {
                                        tracing::error!("Failed to send read inquiry response: {:?}", err);
                                    }
                                }
                                Some(channel_details) => {
                                    let channel_details = channel_details.borrow();
                                    // split the bead_buffer at inquiry.max_bytes
                                    let mut read_buffer = channel_details.read_buffer.lock().unwrap();
                                    let split_index = std::cmp::min(read_buffer.len(), read_inquiry.max_bytes);
                                    let bytes_to_return = read_buffer.split_to(split_index);
                                    if let Err(err) = read_inquiry.response.send(bytes_to_return.to_vec()) {
                                        tracing::error!("Failed to send read inquiry response: {:?}", err);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    });
}
/// WebRTC native multiplexing of [Open] [Connection]s.
/// Allow users to open their substreams
impl StreamMuxer for Connection<Open> {
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
