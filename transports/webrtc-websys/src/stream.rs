//! The WebRTC [Stream] over the Connection
use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::{future::poll_fn, AsyncRead, AsyncWrite};
use send_wrapper::SendWrapper;
use web_sys::RtcDataChannel;

use crate::Error;

use self::poll_data_channel::PollDataChannel;

pub(crate) mod poll_data_channel;

/// A stream over a WebRTC connection.
///
/// Backed by a WebRTC data channel.
pub struct Stream {
    /// Wrapper for the inner stream to make it Send
    inner: SendWrapper<libp2p_webrtc_utils::Stream<PollDataChannel>>,
}

pub(crate) type DropListener = SendWrapper<libp2p_webrtc_utils::DropListener<PollDataChannel>>;

/// Blocks until the data channel leaves `connecting` (ICE + DTLS must be up).
///
/// Returns the [`PollDataChannel`] so callers do not call [`PollDataChannel::new`] twice on the
/// same [`RtcDataChannel`] (that drops the first JS closures while the browser may still invoke them).
pub(crate) async fn wait_until_open(dc: &RtcDataChannel) -> Result<PollDataChannel, Error> {
    tracing::debug!(
        target: "libp2p_webrtc_mux",
        dc_id = ?dc.id(),
        ready_state = ?dc.ready_state(),
        "waiting for data channel open (ICE/DTLS)"
    );

    let mut poll_dc = PollDataChannel::new(dc.clone());
    poll_fn(|cx| -> Poll<Result<(), Error>> {
        match Pin::new(&mut poll_dc).poll_ready(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(Error::Connection(e.to_string()))),
            Poll::Pending => Poll::Pending,
        }
    })
    .await?;
    Ok(poll_dc)
}

impl Stream {
    pub(crate) fn new(data_channel: RtcDataChannel) -> (Self, DropListener) {
        Self::from_poll_channel(PollDataChannel::new(data_channel))
    }

    pub(crate) fn from_poll_channel(poll_channel: PollDataChannel) -> (Self, DropListener) {
        let dc_id = poll_channel.id();
        let (inner, drop_listener) = libp2p_webrtc_utils::Stream::new(poll_channel, dc_id);

        (
            Self {
                inner: SendWrapper::new(inner),
            },
            SendWrapper::new(drop_listener),
        )
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut *self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut *self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().inner).poll_close(cx)
    }
}
