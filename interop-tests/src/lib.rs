use std::{str::FromStr, time::Duration};

use anyhow::{bail, Context, Result};
use futures::{FutureExt, StreamExt};
use libp2p::{
    identify,
    identity::Keypair,
    ping,
    swarm::{NetworkBehaviour, SwarmEvent},
    Multiaddr,
};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

mod arch;
#[cfg(target_arch = "wasm32")]
mod host_log;

use arch::{build_swarm, init_logger, Instant, RedisClient};

/// Batched log lines from the WASM dialer (`POST /log` on the harness).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct WasmLogBatch {
    pub lines: Vec<String>,
}

pub async fn run_test(
    transport: &str,
    ip: &str,
    is_dialer: bool,
    test_timeout_seconds: u64,
    redis_addr: &str,
    sec_protocol: Option<String>,
    muxer: Option<String>,
) -> Result<Report> {
    init_logger(redis_addr);

    let test_timeout = Duration::from_secs(test_timeout_seconds);
    let transport = transport.parse().context("Couldn't parse transport")?;
    let sec_protocol = sec_protocol
        .map(|sec_protocol| {
            sec_protocol
                .parse()
                .context("Couldn't parse security protocol")
        })
        .transpose()?;
    let muxer = muxer
        .map(|sec_protocol| {
            sec_protocol
                .parse()
                .context("Couldn't parse muxer protocol")
        })
        .transpose()?;

    let redis_client = RedisClient::new(redis_addr).context("Could not connect to redis")?;

    // Build the transport from the passed ENV var.
    let (mut swarm, local_addr) =
        build_swarm(ip, transport, sec_protocol, muxer, build_behaviour).await?;

    tracing::info!(local_peer=%swarm.local_peer_id(), "Running ping test");

    // See https://github.com/libp2p/rust-libp2p/issues/4071.
    #[cfg(not(target_arch = "wasm32"))]
    let maybe_id = if transport == Transport::WebRtcDirect {
        Some(swarm.listen_on(local_addr.parse()?)?)
    } else {
        None
    };
    #[cfg(target_arch = "wasm32")]
    let maybe_id = None;

    // Run a ping interop test. Based on `is_dialer`, either dial the address
    // retrieved via `listenAddr` key over the redis connection. Or wait to be pinged and have
    // `dialerDone` key ready on the redis connection.
    // Number of successful pings required before the test is considered passing.
    // Using more than one exercises that subsequent messages (not just the first)
    // can flow over the connection — important for WebRTC where the Noise substream
    // close semantics caused later bytes to stall.
    const PING_COUNT: u32 = 12;

    match is_dialer {
        true => {
            let result: Vec<String> = redis_client
                .blpop("listenerAddr", test_timeout.as_secs())
                .await?;
            let other = result
                .get(1)
                .context("Failed to wait for listener to be ready")?;

            let handshake_start = Instant::now();

            swarm.dial(other.parse::<Multiaddr>()?)?;

            let mut ping_count = 0u32;
            let mut handshake_plus_first_rtt: Option<f32> = None;
            let mut last_rtt = 0f32;

            loop {
                match swarm.next().await {
                    Some(SwarmEvent::Behaviour(BehaviourEvent::Ping(ping::Event {
                        result: Ok(rtt),
                        ..
                    }))) => {
                        ping_count += 1;
                        last_rtt = rtt.as_micros() as f32 / 1000.;
                        tracing::info!(?rtt, "{ping_count}/{PING_COUNT} pings successful");
                        handshake_plus_first_rtt.get_or_insert_with(|| {
                            handshake_start.elapsed().as_micros() as f32 / 1000.
                        });
                        if ping_count >= PING_COUNT {
                            break;
                        }
                    }
                    Some(SwarmEvent::Behaviour(BehaviourEvent::Ping(ping::Event {
                        result: Err(e),
                        ..
                    }))) => {
                        tracing::warn!(?e, "ping failed ({ping_count}/{PING_COUNT} so far)");
                    }
                    Some(ev) => tracing::debug!("{ev:?}"),
                    None => bail!("Swarm stream ended unexpectedly"),
                }
            }

            Ok(Report {
                handshake_plus_one_rtt_millis: handshake_plus_first_rtt.unwrap_or(0.),
                ping_rtt_millis: last_rtt,
            })
        }
        false => {
            // Listen if we haven't done so already.
            // This is a hack until https://github.com/libp2p/rust-libp2p/issues/4071 is fixed at which point we can do this unconditionally here.
            let id = match maybe_id {
                None => swarm.listen_on(local_addr.parse()?)?,
                Some(id) => id,
            };

            tracing::info!(
                address=%local_addr,
                "Test instance, listening for incoming connections on address"
            );

            loop {
                if let Some(SwarmEvent::NewListenAddr {
                    listener_id,
                    address,
                }) = swarm.next().await
                {
                    if address.to_string().contains("127.0.0.1") {
                        continue;
                    }
                    if listener_id == id {
                        let ma = format!("{address}/p2p/{}", swarm.local_peer_id());
                        // BLPOP pops the oldest list element; drop any leftover rows from prior runs.
                        redis_client.del("listenerAddr").await?;
                        redis_client.rpush("listenerAddr", ma.clone()).await?;
                        break;
                    }
                }
            }

            // Exit once PING_COUNT pings succeed — exercises that bytes beyond the first
            // message can still flow over the connection.
            let mut handshake_start = None;
            match futures::future::select(
                async move {
                    let mut ping_count = 0u32;
                    loop {
                        let event = swarm
                            .next()
                            .await
                            .context("Swarm stream ended unexpectedly")?;

                        tracing::debug!("{event:?}");

                        if let SwarmEvent::ConnectionEstablished { .. } = &event {
                            handshake_start.get_or_insert_with(Instant::now);
                        }

                        match event {
                            SwarmEvent::Behaviour(BehaviourEvent::Ping(ping::Event {
                                result: Ok(rtt),
                                ..
                            })) => {
                                ping_count += 1;
                                tracing::info!(?rtt, "{ping_count}/{PING_COUNT} pings successful");
                                if ping_count >= PING_COUNT {
                                    let start = handshake_start.unwrap_or_else(Instant::now);
                                    return Ok(Report {
                                        handshake_plus_one_rtt_millis: start.elapsed().as_micros()
                                            as f32
                                            / 1000.,
                                        ping_rtt_millis: rtt.as_micros() as f32 / 1000.,
                                    });
                                }
                            }
                            SwarmEvent::Behaviour(BehaviourEvent::Ping(ping::Event {
                                result: Err(e),
                                ..
                            })) => {
                                tracing::warn!(?e, "ping failed ({ping_count}/{PING_COUNT} so far)");
                            }
                            ev => tracing::debug!("{ev:?}"),
                        }
                    }
                }
                .boxed(),
                arch::sleep(test_timeout),
            )
            .await
            {
                futures::future::Either::Left((report, _)) => report,
                futures::future::Either::Right(_) => {
                    bail!("Timed out waiting for dialer ping")
                }
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub async fn run_test_wasm(
    transport: &str,
    ip: &str,
    is_dialer: bool,
    test_timeout_secs: u64,
    base_url: &str,
    sec_protocol: Option<String>,
    muxer: Option<String>,
) -> Result<(), JsValue> {
    let result = run_test(
        transport,
        ip,
        is_dialer,
        test_timeout_secs,
        base_url,
        sec_protocol,
        muxer,
    )
    .await;
    reqwest::Client::new()
        .post(&format!("http://{}/results", base_url))
        .json(&result.map_err(|e| e.to_string()))
        .send()
        .await?
        .error_for_status()
        .map_err(|e| format!("Sending test result failed: {e}"))?;

    Ok(())
}

/// A request to redis proxy that will pop the value from the list
/// and will wait for it being inserted until a timeout is reached.
#[derive(serde::Deserialize, serde::Serialize)]
pub struct BlpopRequest {
    pub key: String,
    pub timeout: u64,
}

/// A report generated by the test
#[derive(Copy, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Report {
    #[serde(rename = "handshakePlusOneRTTMillis")]
    handshake_plus_one_rtt_millis: f32,
    #[serde(rename = "pingRTTMilllis")]
    ping_rtt_millis: f32,
}

/// Supported transports by rust-libp2p.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Transport {
    Tcp,
    QuicV1,
    WebRtcDirect,
    Ws,
    Webtransport,
}

impl FromStr for Transport {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "tcp" => Self::Tcp,
            "quic-v1" => Self::QuicV1,
            "webrtc-direct" => Self::WebRtcDirect,
            "ws" => Self::Ws,
            "webtransport" => Self::Webtransport,
            other => bail!("unknown transport {other}"),
        })
    }
}

/// Supported stream multiplexers by rust-libp2p.
#[derive(Clone, Debug)]
pub enum Muxer {
    Mplex,
    Yamux,
}

impl FromStr for Muxer {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "mplex" => Self::Mplex,
            "yamux" => Self::Yamux,
            other => bail!("unknown muxer {other}"),
        })
    }
}

/// Supported security protocols by rust-libp2p.
#[derive(Clone, Debug)]
pub enum SecProtocol {
    Noise,
    Tls,
}

impl FromStr for SecProtocol {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "noise" => Self::Noise,
            "tls" => Self::Tls,
            other => bail!("unknown security protocol {other}"),
        })
    }
}

#[derive(NetworkBehaviour)]
pub(crate) struct Behaviour {
    ping: ping::Behaviour,
    identify: identify::Behaviour,
}

pub(crate) fn build_behaviour(key: &Keypair) -> Behaviour {
    Behaviour {
        ping: ping::Behaviour::new(ping::Config::new().with_interval(Duration::from_secs(1))),
        // Need to include identify until https://github.com/status-im/nim-libp2p/issues/924 is resolved.
        identify: identify::Behaviour::new(identify::Config::new(
            "/interop-tests".to_owned(),
            key.public(),
        )),
    }
}
