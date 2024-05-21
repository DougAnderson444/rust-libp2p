//! Module for the Opening Connection Stage.

use super::*;

/// The Opening Connection state.
#[derive(Debug, Clone)]
pub struct Opening {
    /// The state of the opening connection handshake
    handshake_state: HandshakeState,
}

impl Opening {
    /// Creates a new `Opening` state.
    pub fn new() -> Self {
        Self {
            handshake_state: HandshakeState::Closed,
        }
    }

    /// Handle timeouts while opening
    pub fn on_timeout(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// Implementations that apply only to the Opening Connection state.
impl Connection<Opening> {
    /// Completes the connection opening process.
    /// The only way to get to Open is to go throguh Opening.
    /// Openin> to Open moves values into the Open state.
    pub fn open(self, config: OpenConfig) -> Connection<Open> {
        Connection {
            rtc: self.rtc,
            channel_details: self.channel_details,
            relay_dgram: self.relay_dgram,
            dgram_rx: self.dgram_rx,
            peer_address: self.peer_address,
            local_address: self.local_address,
            socket: self.socket,
            tx_ondatachannel: self.tx_ondatachannel,
            rx_ondatachannel: self.rx_ondatachannel,
            no_drop_listeners_waker: self.no_drop_listeners_waker,
            drop_listeners: self.drop_listeners,
            tx_state_inquiry: self.tx_state_inquiry,
            tx_state_update: self.tx_state_update,
            stage: Open::new(OpenConfig {
                peer_id: config.peer_id,
                handshake_state: config.handshake_state,
            }),
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
}

impl Connectable for Opening {
    type Output = OpeningEvent;

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
            "transmit opening data",
        );

        // socket().try_send_to
        if let Err(error) = socket.try_send_to(&transmit.contents, transmit.destination) {
            tracing::warn!(
                target: LOG_TARGET,
                ?error,
                "failed to send connection<opening> datagram",
            );

            // return WebRtcEvent::ConnectionClosed; // Should we assume this?
        }

        OpeningEvent::None
    }

    /// Handle error for Opening connection.
    fn on_rtc_error(&mut self, error: str0m::RtcError) -> Self::Output {
        tracing::error!(
            target: LOG_TARGET,
            ?error,
            "WebRTC connection error",
        );
        OpeningEvent::ConnectionClosed
    }

    /// Return [WebRtcEvent::Timeout] when an error occurs while [`Opening`].
    fn on_output_timeout(&mut self, rtc: Arc<Mutex<Rtc>>, timeout: Instant) -> Self::Output {
        let duration = timeout - Instant::now();
        match duration.is_zero() {
            true => {
                if let Err(error) = rtc
                    .lock()
                    .unwrap()
                    .handle_input(Input::Timeout(Instant::now()))
                {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?error,
                        "failed to handle timeout for `Rtc`"
                    );

                    rtc.lock().unwrap().disconnect();
                    return OpeningEvent::ConnectionClosed;
                }

                OpeningEvent::None
            }
            false => OpeningEvent::Timeout { timeout },
        }
    }

    /// If ICE Connection State is Disconnected, return [WebRtcEvent::ConnectionClosed].
    fn on_event_ice_disconnect(&self) -> Self::Output {
        tracing::trace!(target: LOG_TARGET, "ice connection closed");
        OpeningEvent::ConnectionClosed
    }

    /// Progress the opening of the channel, as applicable.
    fn on_event_channel_open(&mut self, channel_id: ChannelId, name: String) -> Self::Output {
        // No Opening specific logic for channel open

        OpeningEvent::None
    }

    /// When Opening, ChannelData should be the Noise Handshake,
    /// so data needs to be passed to the PollDataChannel AsyncRead/Write so the
    /// Noise Protocol has it during `upgrade::inbound` or `upgrade::outbound`
    fn on_event_channel_data(&mut self, _data: ChannelData) -> Self::Output {
        tracing::trace!(
            target: LOG_TARGET,
            "(noise protocol?) data received over channel",
        );

        OpeningEvent::None
    }

    fn on_event_channel_close(&mut self, _channel_id: ChannelId) -> Self::Output {
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

                tracing::debug!(
                    target: LOG_TARGET,
                    // peer = ?self.peer_address,
                    "connection opened",
                );

                self.handshake_state = HandshakeState::Opened {
                    remote_fingerprint: remote_fp,
                };

                OpeningEvent::ConnectionOpened {
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

                OpeningEvent::ConnectionClosed
            }
        }
    }

    fn on_event(&self, event: Event) -> Self::Output {
        todo!()
    }
}
