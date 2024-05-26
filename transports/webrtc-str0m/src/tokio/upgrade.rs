//! Upgrades new connections with Noise Protocol.

use crate::tokio::udp_manager::ConnectionContext;
use crate::tokio::{
    connection::{Connection, OpeningEvent},
    error::Error,
    udp_manager::UDPManager,
};
use identity::PeerId;
use libp2p_identity as identity;
use libp2p_webrtc_utils::noise;
use libp2p_webrtc_utils::Fingerprint;
use std::sync::Mutex;
use std::{net::SocketAddr, sync::Arc, time::Instant};
use str0m::{
    change::DtlsCert,
    channel::{ChannelConfig, ChannelId},
    net::{DatagramRecv, Receive},
    IceCreds, Input, Rtc, RtcConfig,
};
use str0m::{net::Protocol as Str0mProtocol, Candidate};

use super::connection::FullConnection;
use super::fingerprint;
use super::transport::Config;
use super::udp_manager::NewRemoteAddress;

/// The log target for this module.
const LOG_TARGET: &str = "libp2p_webrtc_str0m";

/// Upgrades a new inbound WebRTC connection ith Noise Protocol.
pub(crate) async fn inbound(
    remote: NewRemoteAddress, // id_keys: identity::Keypair,
    destination: SocketAddr,
    config: Config,
    udp_manager: Arc<Mutex<UDPManager>>,
) -> Result<(PeerId, FullConnection), Error> {
    let source = remote.addr;

    tracing::debug!(target: LOG_TARGET, address=%source, ufrag=%remote.ufrag, "new inbound connection from address");

    let contents: DatagramRecv = remote.contents.as_slice().try_into()?;

    // Create new `Rtc` object for this source
    let (rtc, noise_channel_id) = make_rtc_client(
        &remote.ufrag,
        &remote.ufrag,
        source,
        destination,
        config.dtls_cert().unwrap().clone(),
    );

    // New Opening Connection
    let mut connection = Connection::new(rtc.clone(), udp_manager.lock().unwrap().socket(), source);

    // A relay tp this new Open Connection needs to be added to udp_manager
    udp_manager
        .lock()
        .unwrap()
        .socket_opening_conns
        .insert(source, connection.clone());

    let (noise_stream, drop_listener) = connection
        .new_stream_from_data_channel_id(noise_channel_id)
        .map_err(|_| Error::StreamCreationFailed)?;

    // we don't track noise drops
    drop(drop_listener);

    // Cast the datagram into a str0m::Receive and pass it to the str0m client
    rtc.lock()
        .map_err(|_| Error::LockPoisoned)?
        .handle_input(Input::Receive(
            Instant::now(),
            Receive {
                source,
                proto: Str0mProtocol::Udp,
                destination,
                contents,
            },
        ))?;

    // Poll the connection to make progress towards an OpeningEvent.
    let event = loop {
        match connection.poll_progress() {
            OpeningEvent::None => {
                continue;
            }
            OpeningEvent::Timeout { timeout } => {
                tracing::debug!(target: LOG_TARGET, "inbound opening connection upgrade timed out: {:?}", timeout);

                let contents: DatagramRecv = remote.contents.as_slice().try_into()?;
                // Cast the datagram into a str0m::Receive and pass it to the str0m client
                rtc.lock()
                    .map_err(|_| Error::LockPoisoned)?
                    .handle_input(Input::Receive(
                        Instant::now(),
                        Receive {
                            source,
                            proto: Str0mProtocol::Udp,
                            destination,
                            contents,
                        },
                    ))?;
                continue;
            }
            // Opened or Closed
            val => {
                break val;
            }
        }
    };

    let OpeningEvent::ConnectionOpened { remote_fingerprint } = event else {
        return Err(Error::Disconnected);
    };

    let client_fingerprint: crate::tokio::Fingerprint =
        config.dtls_cert().unwrap().fingerprint().into();

    let peer_id = noise::inbound(
        config.id_keys().clone(),
        noise_stream,
        client_fingerprint.into(),
        *remote_fingerprint,
    )
    .await
    .map_err(|_| Error::NoiseHandshakeFailed)?;

    let (connection, mut notify) = connection.open(peer_id);

    let mut connection_clone = connection.clone();

    // now that the connection is opened, we need to spawn an ongoing loop
    // for the connection events to be handled in an ongoing manner
    tokio::spawn(async move {
        connection_clone.run().await;
    });

    // receive on notify listenr
    let tx = notify.try_next().map_err(|err| {
        tracing::error!(target: LOG_TARGET, ?err, "failed to receive on notify connection that we are ready");
        Error::Receive
    })?
    .ok_or(Error::Receive)?;

    // A relay tp this new Open Connection needs to be added to udp_manager
    udp_manager
        .lock()
        .unwrap()
        .open
        .insert(source, ConnectionContext { tx });

    Ok((peer_id, connection.into()))
}

/// Upgrades Outbound WebRTC connection with Noise Protocol.
pub(crate) async fn outbound(
    destination: SocketAddr,
    config: RtcConfig,
    udp_manager: Arc<Mutex<UDPManager>>,
    dtls_cert: DtlsCert,
    remote_fingerprint: Fingerprint,
    id_keys: identity::Keypair,
) -> Result<(PeerId, FullConnection), Error> {
    tracing::debug!(target: LOG_TARGET, address=%destination, "new outbound connection to address");

    let source = udp_manager.lock().unwrap().socket().local_addr()?;
    let (rtc, noise_channel_id) =
        outbound_rtc_client(remote_fingerprint, source, destination, dtls_cert);

    // New Opening Connection
    let mut connection = Connection::new(rtc.clone(), udp_manager.lock().unwrap().socket(), source);

    let (noise_stream, drop_listener) = connection
        .new_stream_from_data_channel_id(noise_channel_id)
        .map_err(|_| Error::StreamCreationFailed)?;

    // we don't track noise drops
    drop(drop_listener);

    // Poll the connection to make progress towards an OpeningEvent.
    let event = loop {
        match connection.poll_progress() {
            OpeningEvent::None => {
                continue;
            }
            OpeningEvent::Timeout { timeout } => {
                tracing::debug!(target: LOG_TARGET, "opening connection upgrade timed out: {:?}", timeout);
                continue;
            }
            // Opened or Closed
            val => {
                break val;
            }
        }
    };

    let OpeningEvent::ConnectionOpened { remote_fingerprint } = event else {
        return Err(Error::Disconnected);
    };

    let client_fingerprint: crate::tokio::Fingerprint =
        config.dtls_cert().unwrap().fingerprint().into();

    let peer_id = noise::outbound(
        id_keys,
        noise_stream,
        *remote_fingerprint,
        client_fingerprint.into(),
    )
    .await
    .map_err(|_| Error::NoiseHandshakeFailed)?;

    let (connection, mut notify) = connection.open(peer_id);

    let mut connection_clone = connection.clone();

    // now that the connection is opened, we need to spawn an ongoing loop
    // for the connection events to be handled in an ongoing manner
    tokio::spawn(async move {
        connection_clone.run().await;
    });

    // receive on notify connection listener that we are ready to receive the Sender
    let tx = notify.try_next().map_err(|err| {
        tracing::error!(target: LOG_TARGET, ?err, "failed to receive on notify connection that we are ready");
        Error::Receive
    })?
    .ok_or(Error::Receive)?;

    // This new Open Connection needs to be added to udp_manager.addr_conns
    udp_manager
        .lock()
        .unwrap()
        .open
        .insert(source, ConnectionContext { tx });

    Ok((peer_id, connection.into()))
}

/// Create RTC client and open channel for Noise handshake.
pub(crate) fn make_rtc_client(
    ufrag: &str,
    pass: &str,
    source: SocketAddr,
    destination: SocketAddr,
    dtls_cert: DtlsCert,
) -> (Arc<Mutex<Rtc>>, ChannelId) {
    let mut rtc = Rtc::builder()
        .set_ice_lite(true)
        .set_dtls_cert(dtls_cert)
        .set_fingerprint_verification(false)
        .build();
    tracing::trace!(target: LOG_TARGET, "source: {}, destination: {}", source, destination);
    rtc.add_local_candidate(Candidate::host(destination, Str0mProtocol::Udp).unwrap());
    rtc.add_remote_candidate(Candidate::host(source, Str0mProtocol::Udp).unwrap());
    rtc.direct_api()
        .set_remote_fingerprint(fingerprint::Fingerprint::from(Fingerprint::FF).into());
    rtc.direct_api().set_remote_ice_credentials(IceCreds {
        ufrag: ufrag.to_owned(),
        pass: pass.to_owned(),
    });
    rtc.direct_api().set_local_ice_credentials(IceCreds {
        ufrag: ufrag.to_owned(),
        pass: pass.to_owned(),
    });
    rtc.direct_api().set_ice_controlling(false);
    rtc.direct_api().start_dtls(false).unwrap();
    rtc.direct_api().start_sctp(false);

    let noise_channel_id = rtc.direct_api().create_data_channel(ChannelConfig {
        label: "noise".to_string(),
        ordered: false,
        reliability: Default::default(),
        negotiated: Some(0),
        protocol: "".to_string(),
    });

    (Arc::new(Mutex::new(rtc)), noise_channel_id)
}

/// Make an Rtc client for an outbound connection.
/// Given: remote_fingerprint, id_keys, and destination SocketAddr
/// Set:
/// - negotiated is true
/// - protocol is "noise"
/// - label is ""
/// - ufrag is random using `libp2p_webrtc_utils::sdp::random_ufrag()`
pub(crate) fn outbound_rtc_client(
    remote_fingerprint: Fingerprint,
    source: SocketAddr,
    destination: SocketAddr,
    dtls_cert: DtlsCert,
) -> (Arc<Mutex<Rtc>>, ChannelId) {
    let ufrag = libp2p_webrtc_utils::sdp::random_ufrag();

    let mut rtc = Rtc::builder()
        .set_ice_lite(true)
        .set_dtls_cert(dtls_cert)
        .set_fingerprint_verification(false)
        .build();
    // We are the source, they are the destination since we are going outbound
    rtc.add_local_candidate(Candidate::host(source, Str0mProtocol::Udp).unwrap());
    rtc.add_remote_candidate(Candidate::host(destination, Str0mProtocol::Udp).unwrap());
    rtc.direct_api()
        .set_remote_fingerprint(fingerprint::Fingerprint::from(remote_fingerprint).into());
    // We are dialing outbound, so we do not yet have remote ICE credentials
    rtc.direct_api().set_local_ice_credentials(IceCreds {
        ufrag: ufrag.clone(),
        // NOTE: ufrag is equal to pwd.
        pass: ufrag.clone(),
    });
    // for ICR controlling, since we are dialing outbound we are controlling?
    rtc.direct_api().set_ice_controlling(true);

    // TODO: Determine if these are the right settings for outbound!!
    rtc.direct_api().start_dtls(false).unwrap();
    // TODO: Determine if these are the right settings for outbound!!
    rtc.direct_api().start_sctp(false);

    let noise_channel_id = rtc.direct_api().create_data_channel(ChannelConfig {
        label: "".to_string(),
        ordered: false,
        reliability: Default::default(),
        negotiated: None,
        protocol: "".to_string(),
    });

    (Arc::new(Mutex::new(rtc)), noise_channel_id)
}
