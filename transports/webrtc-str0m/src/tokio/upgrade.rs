//! Upgrades new connections with Noise Protocol.

use crate::tokio::connection::OpenConfig;
use crate::tokio::UdpSocket;
use crate::tokio::{
    connection::{
        Connectable, Connection, ConnectionEvent, HandshakeState, Opening, PeerAddress, WebRtcEvent,
    },
    error::Error,
    udp_manager::UDPManager,
};
use identity::PeerId;
use libp2p_identity as identity;
use libp2p_webrtc_utils::noise;
use libp2p_webrtc_utils::sdp;
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
use tokio::sync;

use super::{connection::Open, fingerprint, stream::Stream};

const LOG_TARGET: &str = "libp2p_webrtc_str0m";
const DATAGRAM_BUFFER_SIZE: usize = 1024;

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

    rtc.lock()
        .unwrap()
        .handle_input(Input::Receive(
            Instant::now(),
            Receive {
                source,
                proto: Str0mProtocol::Udp,
                destination,
                contents,
            },
        ))
        .expect("client to handle input successfully");

    // A handle to send datagrams to the UDP socket
    let (tx, dgram_rx) = sync::mpsc::channel(DATAGRAM_BUFFER_SIZE);

    // Open a new Connection and poll on the next event
    let mut connection: Arc<Mutex<Connection<Opening>>> = Arc::new(Mutex::new(Connection::new(
        Opening::new(Arc::clone(&rtc), noise_channel_id),
    )));

    // loop until we get a Connection event
    let event = loop {
        match connection
            .lock()
            .map_err(|error| Error::Disconnected)?
            .poll_progress()
        {
            WebRtcEvent::Timeout { timeout } => {
                let duration = timeout - Instant::now();

                match duration.is_zero() {
                    true => match connection
                        .lock()
                        .map_err(|error| Error::Disconnected)?
                        .on_timeout()
                    {
                        Ok(()) => continue,
                        Err(error) => {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?source,
                                ?error,
                                "failed to handle timeout",
                            );

                            break ConnectionEvent::ConnectionClosed;
                        }
                    },
                    false => break ConnectionEvent::Timeout { duration },
                }
            }
            WebRtcEvent::Transmit {
                destination,
                datagram,
            } => {
                if let Err(error) = socket.try_send_to(&datagram, destination) {
                    tracing::warn!(
                        target: LOG_TARGET,
                        ?source,
                        ?error,
                        "failed to send datagram",
                    );
                }
            }
            WebRtcEvent::ConnectionClosed => break ConnectionEvent::ConnectionClosed,
            WebRtcEvent::ConnectionOpened {
                peer,
                remote_fingerprint,
            } => {
                break ConnectionEvent::ConnectionEstablished {
                    peer,
                    remote_fingerprint,
                };
            }
            _ => { // noise::inbound(id_keys, stream, client_fingerprint, server_fingerprint),
            }
        }
    };

    let ConnectionEvent::ConnectionEstablished {
        peer,
        remote_fingerprint,
    } = event
    else {
        return Err(Error::Disconnected);
    };

    let stream =
        create_substream_for_noise_handshake(&noise_channel_id, Arc::clone(&connection)).await?;

    let client_fingerprint: crate::tokio::Fingerprint =
        config.dtls_cert().unwrap().fingerprint().into();

    let peer_id = noise::inbound(
        id_keys,
        stream,
        client_fingerprint.into(),
        *remote_fingerprint,
    )
    .await?;

    let peer_address = PeerAddress(source);
    let handshake_state = HandshakeState::Opened { remote_fingerprint };

    let lock = Arc::try_unwrap(connection).expect("Connection Lock still has multiple owners");
    let connection_unlocked = lock.into_inner().expect("Mutex cannot be locked");

    let connection = connection_unlocked.open(OpenConfig {
        socket,
        peer_address,
        dgram_rx,
        peer,
        handshake_state,
    });
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

async fn create_substream_for_noise_handshake(
    id: &ChannelId,
    connection: Arc<Mutex<Connection>>,
) -> Result<Stream, Error> {
    // use Rtc to create a channel for Noise, negotiated fag set to true
    let (substream, drop_listener) = Stream::new(*id, connection)?;
    drop(drop_listener);
    Ok(substream)
}
