//! Upgrades new connections with Noise Protocol.

use crate::tokio::connection::OpenConfig;
use crate::tokio::{
    connection::{Connection, HandshakeState, OpeningEvent},
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
use tokio::sync::Mutex as AsyncMutex;

use super::{connection::Open, fingerprint};

/// The log target for this module.
const LOG_TARGET: &str = "libp2p_webrtc_str0m";

/// Upgrades a new inbound WebRTC connection ith Noise Protocol.
pub(crate) async fn inbound(
    source: SocketAddr,
    config: RtcConfig,
    udp_manager: Arc<Mutex<UDPManager>>,
    dtls_cert: DtlsCert,
    remote_ufrag: String,
    id_keys: identity::Keypair,
    contents: Vec<u8>,
) -> Result<(PeerId, Arc<AsyncMutex<Connection<Open>>>), Error> {
    tracing::debug!(target: LOG_TARGET, address=%source, ufrag=%remote_ufrag, "new inbound connection from address");

    let destination = udp_manager.lock().unwrap().socket().local_addr()?;
    let contents: DatagramRecv = contents.as_slice().try_into()?;

    // Create new `Rtc` object for this source
    let (rtc, noise_channel_id) =
        make_rtc_client(&remote_ufrag, &remote_ufrag, source, destination, dtls_cert);

    let mut connection = Connection::new(rtc.clone(), udp_manager.lock().unwrap().socket(), source);

    let noise_stream = connection
        .new_stream_from_data_channel_id(noise_channel_id)
        .map_err(|_| Error::StreamCreationFailed)?;

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
    // For Opening, we only care about
    // 1) Timeout,
    // 2) ConnectionClosed,
    // 3) ConnectionOpened
    // 4) None
    let event = loop {
        match connection.poll_progress() {
            // Keep looping
            OpeningEvent::None => {
                continue;
            }
            // Timeout, track them to revisit
            OpeningEvent::Timeout { timeout } => {
                tracing::debug!(target: LOG_TARGET, "opening connection upgrade timed out: {:?}", timeout);
                continue;
            }
            // Closed, or Established
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
        id_keys,
        noise_stream,
        client_fingerprint.into(),
        *remote_fingerprint,
    )
    .await
    .map_err(|_| Error::NoiseHandshakeFailed)?;

    let handshake_state = HandshakeState::Opened { remote_fingerprint };

    let connection = Arc::new(AsyncMutex::new(connection.open(OpenConfig {
        peer_id,
        handshake_state,
    })));

    let connection_clone = Arc::clone(&connection);

    // now that the connection is opened, we need to spawn an ongoing loop
    // for the connection events to be handled in an ongoing manner
    tokio::spawn(async move {
        let mut connection = connection_clone.lock().await;
        connection.run().await;
    });

    // This new Open Connection needs to be added to udp_manager.addr_conns
    udp_manager
        .lock()
        .unwrap()
        .socket_open_conns
        .insert(source, connection.clone());

    Ok((peer_id, connection))
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
