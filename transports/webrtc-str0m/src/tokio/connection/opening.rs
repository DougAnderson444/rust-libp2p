//! Module for the Opening Connection Stage.

use futures::channel::mpsc::{channel, Receiver};

use super::*;

/// The Opening Connection state.
#[derive(Debug, Clone)]
pub struct Opening {
    /// The state of the opening connection handshake
    handshake_state: HandshakeState,
}

impl Default for Opening {
    fn default() -> Self {
        Self::new()
    }
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
    /// Creates a new `Connection` in the Opening state.
    pub fn new(rtc: Arc<Mutex<Rtc>>, socket: Arc<UdpSocket>, source: SocketAddr) -> Self {
        let local_address = socket.local_addr().unwrap();

        // Make the state_inquiry channel
        let (tx_state_inquiry, rx_state_inquiry) = mpsc::channel::<Inquiry>(4);

        let (tx_state_update, rx_state_update) = mpsc::channel::<StateUpdate>(1);

        state_loop(rx_state_update, rx_state_inquiry);

        Self {
            rtc,
            socket,
            stage: Opening::new(),
            peer_address: PeerAddress(source),
            local_address,
            channel_details: Default::default(),
            tx_state_inquiry,
            tx_state_update,
        }
    }

    /// Completes the connection opening process.
    /// The only way to get to Open is to go throguh Opening.
    /// Openin> to Open moves values into the Open state.
    pub fn open(self, peer_id: PeerId) -> (Connection<Open>, Receiver<mpsc::Sender<Vec<u8>>>) {
        let (notify_dgram_senders, dgram_senders) = channel::<mpsc::Sender<Vec<u8>>>(1);
        (
            Connection {
                rtc: self.rtc,
                channel_details: self.channel_details,
                peer_address: self.peer_address,
                local_address: self.local_address,
                socket: self.socket,
                tx_state_inquiry: self.tx_state_inquiry,
                tx_state_update: self.tx_state_update,
                stage: Open {
                    peer_id,
                    notify_dgram_senders,
                },
            },
            dgram_senders,
        )
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
    fn on_event_channel_open(&mut self, _channel_id: ChannelId, _name: String) -> Self::Output {
        // No Opening specific logic for channel open

        OpeningEvent::None
    }

    fn on_event_channel_close(&mut self, _channel_id: ChannelId) -> Self::Output {
        tracing::warn!(
            target: LOG_TARGET,
            // connection_id = ?self.connection_id,
            "channel closed",
        );
        Self::Output::default()
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

                self.handshake_state = HandshakeState::Opened;

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
        tracing::trace!(target: LOG_TARGET, ?event, "event");
        Self::Output::default()
    }
}
