//! Module for an Open Connection

use super::*;

/// The Open Connection state.
#[derive(Debug)]
pub struct Open {
    /// Remote peer ID.
    peer: PeerId,
    /// The state of the opening connection handshake
    handshake_state: HandshakeState,
}

impl Open {
    /// Creates a new `Open` state.
    pub fn new(config: OpenConfig) -> Self {
        Self {
            peer: config.peer_id,
            handshake_state: config.handshake_state,
        }
    }
}

/// Configure the Open stage:
#[derive(Debug)]
pub struct OpenConfig {
    /// Remote peer ID.
    pub peer_id: PeerId,
    /// The state of the opening connection handshake
    pub handshake_state: HandshakeState,
}

/// Impl Connectable for Open
impl Connectable for Open {
    /// When Connection is Open, the only output is [str0m::Output::Timeout] ( [Instant] ).
    type Output = Option<std::time::Instant>;

    /// Returns the [`HandshakeState`] of the connection.
    fn handshake_state(&self) -> HandshakeState {
        self.handshake_state.clone()
    }

    fn on_output_transmit(
        &mut self,
        socket: Arc<UdpSocket>,
        transmit: str0m::net::Transmit,
    ) -> Self::Output {
        tracing::trace!(
            target: LOG_TARGET,
            "transmit data",
        );

        socket
            .try_send_to(&transmit.contents, transmit.destination)
            .expect("send data");

        None
    }

    /// Return [WebRtcEvent::ConnectionClosed] when an error occurs.
    fn on_rtc_error(&mut self, error: str0m::RtcError) -> Self::Output {
        tracing::error!(
            target: LOG_TARGET,
            ?error,
            "WebRTC connection error",
        );
        None
    }

    /// Return [`Instant`] when a timeout occurs while [`Open`].
    fn on_output_timeout(&mut self, _rtc: Arc<Mutex<Rtc>>, timeout: Instant) -> Self::Output {
        Some(timeout)
    }

    /// If ICE Connection State is Disconnected, return [WebRtcEvent::ConnectionClosed].
    fn on_event_ice_disconnect(&self) -> Self::Output {
        tracing::trace!(target: LOG_TARGET, "ice connection closed");
        None
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

        None
    }

    fn on_event_channel_data(&mut self, _data: ChannelData) -> Self::Output {
        todo!()
    }

    fn on_event_channel_close(&mut self, _channel_id: ChannelId) -> Self::Output {
        todo!()
    }

    fn on_event(&self, _event: Event) -> Self::Output {
        todo!()
    }

    fn on_event_connected(&mut self, _rtc: Arc<Mutex<Rtc>>) -> Self::Output {
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

    /// Runs the main str0m input handler
    /// which is a connection loop to deal with Transmission and Events
    pub async fn run(&mut self) {
        loop {
            // Do something
            tokio::select! {
                            biased;
                            datagram = self.dgram_rx.recv() => match datagram {
                                Some(datagram) => {
                                    let input = Input::Receive(
                                        Instant::now(),
                                        Receive {
                                            proto: Str0mProtocol::Udp,
                                            source: *self.peer_address,
                                            destination: self.local_address,
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
