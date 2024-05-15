//! Connection module.
use std::{
    net::{SocketAddr, UdpSocket},
    ops::Deref,
    sync::Arc,
    time::Instant,
};

use libp2p_identity::PeerId;
use str0m::{
    net::{Protocol as Str0mProtocol, Receive},
    Event, IceConnectionState, Input, Output, Rtc,
};
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::{channel, error::TrySendError, Sender};

use crate::tokio::Error;

const LOG_TARGET: &str = "libp2p_webrtc_str0m";

pub trait Connectable {
    /// Socket received some data, process it according to the State.
    fn recv_data(&mut self, buf: &[u8]) -> Result<(), Error>;
}

/// The Opening Connection state.
pub struct Opening {
    /// TX channel for sending datagrams to the connection event loop.
    tx: Sender<Vec<u8>>,
}

impl Opening {
    /// Creates a new `Opening` state.
    pub fn new() -> Self {
        Self { tx: channel(1).0 }
    }

    /// Socket received some data, process it according to Opening State.
    /// When the [State] is [Opening], we either are getting a [StunMessage] or
    /// some other opening data. After we usher along the data we received, we
    /// need to poll the opening Rtc connection to see what the next event is
    /// and react accordingly.
    pub(crate) fn recv_data<S: Unpin>(
        &self,
        message: Vec<u8>,
    ) -> Result<(), tokio::sync::mpsc::error::TrySendError<Vec<u8>>> {
        Ok(self.tx.try_send(message)?)
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
}

/// Implementations that apply only to the Opening Connection state.
impl Connection<Opening> {
    /// Creates a new `Connection` in the Opening state.
    pub fn new(rtc: Rtc) -> Self {
        Self {
            // state: std::marker::PhantomData,
            state: Opening::new(),
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
