//! Module for an Open Connection

use super::*;

/// The Open Connection state.
#[derive(Debug, Clone)]
pub struct Open {
    /// Remote peer ID.
    pub(crate) peer_id: PeerId,

    /// Notifies any dgram sender that the Connection is ready to receive datagrams
    /// Includes a mpsc Sender for the dgram sender to send datagrams to the Connection
    pub(crate) notify_dgram_senders: Sender<mpsc::Sender<Vec<u8>>>,
}

/// Impl Connectable for Open
impl Connectable for Open {
    /// When Connection is Open, the only output is [str0m::Output::Timeout] ( [Instant] ).
    type Output = Option<std::time::Instant>;

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
    fn on_connection_closed(&mut self) {
        tracing::trace!(
            target: LOG_TARGET,
            peer = ?self.stage.peer_id,
            "connection closed",
        );

        // TODO: Report Connection Closed
    }

    /// Runs the main str0m input handler
    /// which is a connection loop to deal with Transmission and Events
    pub async fn run(&mut self) {
        tracing::trace!(
            target: LOG_TARGET,
            peer = ?self.stage.peer_id,
            "start webrtc connection event loop",
        );

        let (tx_dgram, mut dgram_rx) = mpsc::channel(DATAGRAM_BUFFER_SIZE);
        // Notify senders that we are ready to rx their dgrams
        if let Err(e) = self.stage.notify_dgram_senders.try_send(tx_dgram) {
            tracing::error!(
                target: LOG_TARGET,
                ?e,
                "failed to notify dgram senders",
            );
            return self.on_connection_closed();
        }

        loop {
            // poll output until we get a timeout
            let Some(timeout) = self.rtc_poll_output() else {
                tracing::trace!(
                    target: LOG_TARGET,
                    peer = ?self.stage.peer_id,
                    "connection closed",
                );
                return self.on_connection_closed();
            };

            let duration = timeout - Instant::now();
            if duration.is_zero() {
                self.rtc()
                    .lock()
                    .unwrap()
                    .handle_input(Input::Timeout(Instant::now()))
                    .unwrap();
                continue;
            }

            // Do something
            tokio::select! {
                            biased;
                            datagram = dgram_rx.recv() => match datagram {
                                Some(datagram) => {
                                    let input = Input::Receive(
                                        Instant::now(),
                                        Receive {
                                            proto: Str0mProtocol::Udp,
                                            source: *self.peer_address,
                                            destination: self.local_addr,
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
                                        peer = ?self.stage.peer_id,
                                        "read `None` from `dgram_rx`",
                                    );
                                    return self.on_connection_closed();
                                }
                            },
                            _ = tokio::time::sleep(duration) => {
                                self.rtc().lock().unwrap().handle_input(Input::Timeout(Instant::now())).unwrap();
                            }
            }
        }
    }
}
