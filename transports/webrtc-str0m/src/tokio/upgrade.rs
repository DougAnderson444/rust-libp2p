//! Upgrades new connections with Noise Protocol.

use crate::tokio::{
    connection::{Connectable, Connection},
    error::Error,
    udp_manager::UDPManager,
};
use identity::PeerId;
use libp2p_identity as identity;
use libp2p_webrtc_utils::noise;
use libp2p_webrtc_utils::sdp;
use libp2p_webrtc_utils::Fingerprint;
use std::{net::SocketAddr, sync::Arc, time::Instant};
use str0m::{
    change::DtlsCert,
    channel::{ChannelConfig, ChannelId},
    net::{DatagramRecv, Receive},
    IceCreds, Input, Rtc, RtcConfig,
};
use str0m::{net::Protocol as Str0mProtocol, Candidate};
use tokio::net::UdpSocket;

use super::{connection::Open, fingerprint};

/// Creates a new inbound WebRTC connection.
pub(crate) async fn inbound(
    source: SocketAddr,
    config: RtcConfig,
    socket: Arc<UdpSocket>,
    dtls_cert: DtlsCert,
    remote_ufrag: String,
    id_keys: identity::Keypair,
    contents: Vec<u8>,
) -> Result<(PeerId, Connection<Open>), Error> {
    tracing::debug!(address=%source, ufrag=%remote_ufrag, "new inbound connection from address");

    // using str0m:
    // 1) Create a new inbound connection.
    // 2) Set the remote description.
    // 3) Get offer, create an answer.
    // 4) Handle noise
    // 5) Poll connection for events
    // 6) Connection opened if successful

    let destination = socket.local_addr().unwrap();
    let contents: DatagramRecv = contents.as_slice().try_into().unwrap();

    // create new `Rtc` object for the peer and give it the received STUN message
    let (mut rtc, noise_channel_id) =
        make_rtc_client(&remote_ufrag, &remote_ufrag, source, destination, dtls_cert);

    rtc.handle_input(Input::Receive(
        Instant::now(),
        Receive {
            source,
            proto: Str0mProtocol::Udp,
            destination,
            contents,
        },
    ))
    .expect("client to handle input successfully");

    todo!()
}

/// Create RTC client and open channel for Noise handshake.
pub fn make_rtc_client(
    ufrag: &str,
    pass: &str,
    source: SocketAddr,
    destination: SocketAddr,
    dtls_cert: DtlsCert,
) -> (Rtc, ChannelId) {
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

    (rtc, noise_channel_id)
}
