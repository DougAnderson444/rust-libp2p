mod poll_data_channel;

use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use futures::prelude::*;
use poll_data_channel::PollDataChannel;
use send_wrapper::SendWrapper;
use str0m::{channel::ChannelId, Rtc};

pub(crate) use super::channel::{ReadReady, StateInquiry};

use super::{
    channel::{ChannelWakers, RtcDataChannelState},
    Error,
};

/// A substream on top of a WebRTC data channel.
///
/// To be a proper libp2p substream, we need to implement [`AsyncRead`] and [`AsyncWrite`] as well
/// as support a half-closed state which we do by framing messages in a protobuf envelope.
pub struct Stream {
    inner: libp2p_webrtc_utils::Stream<PollDataChannel>,
}

pub(crate) type DropListener = libp2p_webrtc_utils::DropListener<PollDataChannel>;

impl Stream {
    pub(crate) fn new(
        channel_id: ChannelId,
        state: RtcDataChannelState,
        rtc: Arc<Mutex<Rtc>>,
    ) -> Result<
        (
            Self,
            DropListener,
            futures::channel::mpsc::Receiver<ChannelWakers>,
            Mutex<futures::channel::mpsc::Receiver<ReadReady>>,
            Mutex<futures::channel::mpsc::Receiver<StateInquiry>>,
        ),
        Error,
    > {
        let (send_wakers, wakers_rx) = futures::channel::mpsc::channel(1);
        let (read_ready_signal, read_ready_rx) = futures::channel::mpsc::channel(1);
        let (channel_state_signal, channel_state_rx) = futures::channel::mpsc::channel(4);
        let (inner, drop_listener) = libp2p_webrtc_utils::Stream::new(PollDataChannel::new(
            channel_id,
            rtc,
            send_wakers,
            read_ready_signal,
            channel_state_signal,
        )?);

        Ok((
            Self {
                inner, // : SendWrapper::new(inner),
            },
            drop_listener,
            wakers_rx,
            Mutex::new(read_ready_rx),
            Mutex::new(channel_state_rx),
        ))
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}
