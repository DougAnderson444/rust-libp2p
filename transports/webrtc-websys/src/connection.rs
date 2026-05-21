//! A libp2p connection backed by an [RtcPeerConnection](https://developer.mozilla.org/en-US/docs/Web/API/RTCPeerConnection).

use std::{
    pin::Pin,
    task::{ready, Context, Poll, Waker},
    time::Duration,
};

use futures::{channel::mpsc, stream::FuturesUnordered, FutureExt, StreamExt};
use futures_timer::Delay;
use js_sys::{Object, Reflect};
use libp2p_core::muxing::{StreamMuxer, StreamMuxerEvent};
use libp2p_webrtc_utils::Fingerprint;
use send_wrapper::SendWrapper;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    RtcConfiguration, RtcDataChannel, RtcDataChannelEvent, RtcDataChannelInit, RtcDataChannelType,
    RtcSessionDescriptionInit,
};

use super::{Error, Stream};
use crate::stream::{poll_data_channel::PollDataChannel, DropListener};

/// A WebRTC Connection.
///
/// All connections need to be [`Send`] which is why some fields are wrapped in [`SendWrapper`].
/// This is safe because WASM is single-threaded.
pub struct Connection {
    /// The [RtcPeerConnection] that is used for the WebRTC Connection
    inner: SendWrapper<RtcPeerConnection>,

    /// Whether the connection is closed
    closed: bool,
    /// An [`mpsc::channel`] for all inbound data channels.
    ///
    /// Because the browser's WebRTC API is event-based, we need to use a channel to obtain all
    /// inbound data channels.
    inbound_data_channels: SendWrapper<mpsc::Receiver<RtcDataChannel>>,
    /// A list of futures, which, once completed, signal that a [`Stream`] has been dropped.
    drop_listeners: FuturesUnordered<DropListener>,
    no_drop_listeners_waker: Option<Waker>,
    /// Monotonic muxer substream counter (independent of SCTP id).
    mux_inbound_count: u32,
    mux_outbound_count: u32,
    /// Outbound data channel waiting for `open` before handing to the muxer (like native webrtc).
    outbound_opening: Option<(SendWrapper<RtcDataChannel>, SendWrapper<PollDataChannel>)>,
    /// Browser (DTLS client) opens even SCTP ids; defer so the answerer's odd channels land first.
    outbound_defer: Option<Pin<Box<Delay>>>,
    outbound_defer_done: bool,
    outbound_defer_waker: Option<Waker>,

    _ondatachannel_closure: SendWrapper<Closure<dyn FnMut(RtcDataChannelEvent)>>,
}

impl Connection {
    /// Create a new inner WebRTC Connection
    pub(crate) fn new(peer_connection: RtcPeerConnection) -> Self {
        // An ondatachannel Future enables us to poll for incoming data channel events in
        // poll_incoming
        let (tx_ondatachannel, rx_ondatachannel) = mpsc::channel(4); // we may get more than one data channel opened on a single peer connection

        let ondatachannel_closure = Closure::new(move |ev: RtcDataChannelEvent| {
            let channel = ev.channel();
            let mut tx = tx_ondatachannel.clone();
            let deliver = Closure::once(move || {
                tracing::trace!("New data channel");
                if let Err(e) = tx.try_send(channel) {
                    if e.is_full() {
                        tracing::warn!(
                            "Remote is opening too many data channels, we can't keep up!"
                        );
                    } else if e.is_disconnected() {
                        tracing::warn!("Receiver is gone, are we shutting down?");
                    }
                }
            });
            if let Some(window) = web_sys::window() {
                let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                    deliver.as_ref().unchecked_ref(),
                    0,
                );
            }
            deliver.forget();
        });
        peer_connection
            .inner
            .set_ondatachannel(Some(ondatachannel_closure.as_ref().unchecked_ref()));

        Self {
            inner: SendWrapper::new(peer_connection),
            closed: false,
            drop_listeners: FuturesUnordered::default(),
            no_drop_listeners_waker: None,
            inbound_data_channels: SendWrapper::new(rx_ondatachannel),
            mux_inbound_count: 0,
            mux_outbound_count: 0,
            outbound_opening: None,
            outbound_defer: None,
            outbound_defer_done: false,
            outbound_defer_waker: None,
            _ondatachannel_closure: SendWrapper::new(ondatachannel_closure),
        }
    }

    fn new_stream_from_poll_channel(&mut self, poll_channel: PollDataChannel) -> Stream {
        let (stream, drop_listener) = Stream::from_poll_channel(poll_channel);

        self.drop_listeners.push(drop_listener);
        if let Some(waker) = self.no_drop_listeners_waker.take() {
            waker.wake()
        }
        stream
    }

    /// Closes the Peer Connection.
    ///
    /// This closes the data channels also and they will return an error
    /// if they are used.
    fn close_connection(&mut self) {
        if !self.closed {
            tracing::trace!("connection::close_connection");
            self.inner.inner.close();
            self.closed = true;
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close_connection();
    }
}

/// WebRTC native multiplexing
/// Allows users to open substreams
impl StreamMuxer for Connection {
    type Substream = Stream;
    type Error = Error;

    fn poll_inbound(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::Substream, Self::Error>> {
        loop {
            match ready!(self.inbound_data_channels.poll_next_unpin(cx)) {
                Some(data_channel) => {
                    if data_channel.id() == Some(0) {
                        tracing::debug!(
                            target: "libp2p_webrtc_mux",
                            "ignoring inbound data channel id=0 (negotiated noise, not muxer)"
                        );
                        continue;
                    }
                    self.mux_inbound_count += 1;
                    tracing::debug!(
                        target: "libp2p_webrtc_mux",
                        mux_index = self.mux_inbound_count,
                        dc_id = ?data_channel.id(),
                        ready_state = ?data_channel.ready_state(),
                        "muxer inbound substream ready"
                    );
                    let stream = self.new_stream_from_poll_channel(PollDataChannel::new(data_channel));
                    if self.mux_inbound_count >= 2 && !self.outbound_defer_done {
                        self.outbound_defer_done = true;
                        self.outbound_defer = None;
                        if let Some(waker) = self.outbound_defer_waker.take() {
                            waker.wake();
                        }
                    }
                    return Poll::Ready(Ok(stream));
                }
                None => {
                    // This only happens if the [`RtcPeerConnection::ondatachannel`] closure gets freed
                    // which means we are most likely shutting down the connection.
                    tracing::debug!("`Sender` for inbound data channels has been dropped");
                    return Poll::Ready(Err(Error::Connection("connection closed".to_owned())));
                }
            }
        }
    }

    fn poll_outbound(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::Substream, Self::Error>> {
        if !self.outbound_defer_done {
            if self.outbound_defer.is_none() {
                self.outbound_defer = Some(Box::pin(Delay::new(Duration::from_millis(400))));
                tracing::debug!(
                    target: "libp2p_webrtc_mux",
                    "offerer: deferring first outbound mux channel for answerer DCEP"
                );
            }
            self.outbound_defer_waker = Some(cx.waker().clone());
            if self
                .outbound_defer
                .as_mut()
                .expect("set above")
                .poll_unpin(cx)
                .is_pending()
            {
                return Poll::Pending;
            }
            self.outbound_defer = None;
            self.outbound_defer_waker = None;
            self.outbound_defer_done = true;
            tracing::debug!(
                target: "libp2p_webrtc_mux",
                "offerer: opening first outbound mux channel"
            );
        }

        loop {
            if self.outbound_opening.is_none() {
                let dc = self.inner.new_regular_data_channel();
                tracing::debug!(
                    target: "libp2p_webrtc_mux",
                    dc_id = ?dc.id(),
                    ready_state = ?dc.ready_state(),
                    "opening muxer outbound data channel"
                );
                let poll_dc = PollDataChannel::new(dc.clone());
                self.outbound_opening = Some((SendWrapper::new(dc), SendWrapper::new(poll_dc)));
            }

            let (_dc, poll_dc) = self
                .outbound_opening
                .as_mut()
                .expect("outbound_opening set above");

            match Pin::new(&mut **poll_dc).poll_ready(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => {
                    self.outbound_opening = None;
                    return Poll::Ready(Err(Error::Connection(format!("{e}"))));
                }
                Poll::Ready(Ok(())) => {
                    let (_dc, poll_dc) = self.outbound_opening.take().expect("just polled");
                    let poll_dc = poll_dc.take();
                    self.mux_outbound_count += 1;
                    tracing::debug!(
                        target: "libp2p_webrtc_mux",
                        mux_index = self.mux_outbound_count,
                        dc_id = ?poll_dc.id(),
                        ready_state = ?poll_dc.ready_state(),
                        "muxer outbound substream ready"
                    );
                    return Poll::Ready(Ok(self.new_stream_from_poll_channel(poll_dc)));
                }
            }
        }
    }

    /// Closes the Peer Connection.
    fn poll_close(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        tracing::trace!("connection::poll_close");

        self.close_connection();
        Poll::Ready(Ok(()))
    }

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<StreamMuxerEvent, Self::Error>> {
        loop {
            match ready!(self.drop_listeners.poll_next_unpin(cx)) {
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    tracing::debug!("a DropListener failed: {e}")
                }
                None => {
                    self.no_drop_listeners_waker = Some(cx.waker().clone());
                    return Poll::Pending;
                }
            }
        }
    }
}

pub(crate) struct RtcPeerConnection {
    inner: web_sys::RtcPeerConnection,
    _ice_state_log: SendWrapper<Closure<dyn FnMut(web_sys::Event)>>,
}

impl RtcPeerConnection {
    pub(crate) async fn new(algorithm: String) -> Result<Self, Error> {
        let algo: Object = Object::new();
        Reflect::set(&algo, &"name".into(), &"ECDSA".into()).unwrap();
        Reflect::set(&algo, &"namedCurve".into(), &"P-256".into()).unwrap();
        Reflect::set(&algo, &"hash".into(), &algorithm.into()).unwrap();

        let certificate_promise =
            web_sys::RtcPeerConnection::generate_certificate_with_object(&algo)
                .expect("certificate to be valid");

        let certificate = JsFuture::from(certificate_promise).await?;

        let config = RtcConfiguration::default();
        // No STUN/TURN — webrtc-direct uses explicit host candidates in the synthetic SDP answer.
        config.set_ice_servers(&js_sys::Array::new());
        // wrap certificate in a js Array first before adding it to the config object
        let certificate_arr = js_sys::Array::new();
        certificate_arr.push(&certificate);
        config.set_certificates(&certificate_arr);

        let inner = web_sys::RtcPeerConnection::new_with_configuration(&config)?;
        let ice_state_log = attach_ice_state_logging(&inner);

        Ok(Self {
            inner,
            _ice_state_log: SendWrapper::new(ice_state_log),
        })
    }

    /// Creates negotiated data channel 0 for Noise (must exist before creating the SDP offer).
    pub(crate) fn create_handshake_data_channel(&self) -> RtcDataChannel {
        tracing::debug!(
            target: "libp2p_webrtc_mux",
            dc_id = 0u16,
            negotiated = true,
            "creating noise handshake data channel (not a muxer substream)"
        );
        self.new_data_channel(true)
    }

    /// Wraps an open handshake channel as a libp2p [`Stream`].
    pub(crate) fn handshake_stream_from_poll_channel(
        poll_channel: crate::stream::poll_data_channel::PollDataChannel,
    ) -> (Stream, DropListener) {
        Stream::from_poll_channel(poll_channel)
    }

    /// Blocks until the channel leaves `connecting` (ICE + DTLS must be up).
    pub(crate) async fn wait_data_channel_open(
        dc: &RtcDataChannel,
    ) -> Result<crate::stream::poll_data_channel::PollDataChannel, Error> {
        crate::stream::wait_until_open(dc).await
    }

    /// Creates a regular data channel for when the connection is already established.
    pub(crate) fn new_regular_data_channel(&self) -> RtcDataChannel {
        self.new_data_channel(false)
    }

    /// Creates a data channel.
    ///
    /// - `negotiated = true`: fixed SCTP stream **id 0** for Noise (both peers must create it).
    /// - `negotiated = false`: browser/stack assigns the next id (1, 2, 3, …); each muxer
    ///   substream is a **new** id, not “the first of its kind” as 0.
    fn new_data_channel(&self, negotiated: bool) -> RtcDataChannel {
        const LABEL: &str = "";

        let dc = match negotiated {
            true => {
                let options = RtcDataChannelInit::new();
                options.set_negotiated(true);
                options.set_id(0); // id is only ever set to zero when negotiated is true

                self.inner
                    .create_data_channel_with_data_channel_dict(LABEL, &options)
            }
            false => self.inner.create_data_channel(LABEL),
        };
        dc.set_binary_type(RtcDataChannelType::Arraybuffer); // Hardcoded here, it's the only type we use

        tracing::debug!(
            target: "libp2p_webrtc_mux",
            negotiated,
            dc_id = ?dc.id(),
            ready_state = ?dc.ready_state(),
            "created data channel"
        );

        dc
    }

    pub(crate) async fn create_offer(&self) -> Result<String, Error> {
        let offer = JsFuture::from(self.inner.create_offer()).await?;

        let offer = Reflect::get(&offer, &JsValue::from_str("sdp"))
            .expect("sdp should be valid")
            .as_string()
            .expect("sdp string should be valid string");

        Ok(offer)
    }

    pub(crate) async fn set_local_description(
        &self,
        sdp: RtcSessionDescriptionInit,
    ) -> Result<(), Error> {
        let promise = self.inner.set_local_description(&sdp);
        JsFuture::from(promise).await?;

        Ok(())
    }

    pub(crate) fn local_fingerprint(&self) -> Result<Fingerprint, Error> {
        let sdp = &self
            .inner
            .local_description()
            .ok_or_else(|| Error::Js("No local description".to_string()))?
            .sdp();

        let fingerprint =
            parse_fingerprint(sdp).ok_or_else(|| Error::Js("No fingerprint in SDP".to_string()))?;

        Ok(fingerprint)
    }

    pub(crate) async fn set_remote_description(
        &self,
        sdp: RtcSessionDescriptionInit,
    ) -> Result<(), Error> {
        let promise = self.inner.set_remote_description(&sdp);
        JsFuture::from(promise).await?;

        Ok(())
    }
}

fn attach_ice_state_logging(pc: &web_sys::RtcPeerConnection) -> Closure<dyn FnMut(web_sys::Event)> {
    let pc_log = pc.clone();
    let closure = Closure::wrap(Box::new(move |_: web_sys::Event| {
        tracing::debug!(
            target: "libp2p_webrtc_mux",
            ice_connection_state = ?pc_log.ice_connection_state(),
            "RtcPeerConnection ICE state"
        );
    }) as Box<dyn FnMut(web_sys::Event)>);
    pc.set_oniceconnectionstatechange(Some(closure.as_ref().unchecked_ref()));
    closure
}

/// Parse Fingerprint from a SDP.
fn parse_fingerprint(sdp: &str) -> Option<Fingerprint> {
    // split the sdp by new lines / carriage returns
    let lines = sdp.split("\r\n");

    // iterate through the lines to find the one starting with a=fingerprint:
    // get the value after the first space
    // return the value as a Fingerprint
    for line in lines {
        if line.starts_with("a=fingerprint:") {
            let fingerprint = line.split(' ').nth(1).unwrap();
            let bytes = hex::decode(fingerprint.replace(':', "")).unwrap();
            let arr: [u8; 32] = bytes.as_slice().try_into().unwrap();
            return Some(Fingerprint::raw(arr));
        }
    }
    None
}

#[cfg(test)]
mod sdp_tests {
    use super::*;

    #[test]
    fn test_fingerprint() {
        let sdp = "v=0\r\no=- 0 0 IN IP6 ::1\r\ns=-\r\nc=IN IP6 ::1\r\nt=0 0\r\na=ice-lite\r\nm=application 61885 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:0\r\na=setup:passive\r\na=ice-ufrag:libp2p+webrtc+v1/YwapWySn6fE6L9i47PhlB6X4gzNXcgFs\r\na=ice-pwd:libp2p+webrtc+v1/YwapWySn6fE6L9i47PhlB6X4gzNXcgFs\r\na=fingerprint:sha-256 A8:17:77:1E:02:7E:D1:2B:53:92:70:A6:8E:F9:02:CC:21:72:3A:92:5D:F4:97:5F:27:C4:5E:75:D4:F4:31:89\r\na=sctp-port:5000\r\na=max-message-size:16384\r\na=candidate:1467250027 1 UDP 1467250027 ::1 61885 typ host\r\n";

        let fingerprint = parse_fingerprint(sdp).unwrap();

        assert_eq!(fingerprint.algorithm(), "sha-256");
        assert_eq!(fingerprint.to_sdp_format(), "A8:17:77:1E:02:7E:D1:2B:53:92:70:A6:8E:F9:02:CC:21:72:3A:92:5D:F4:97:5F:27:C4:5E:75:D4:F4:31:89");
    }
}
