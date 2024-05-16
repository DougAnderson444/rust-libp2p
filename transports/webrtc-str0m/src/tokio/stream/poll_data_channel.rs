use std::cmp::min;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::task::AtomicWaker;
use futures::{AsyncRead, AsyncWrite};
use libp2p_webrtc_utils::MAX_MSG_LEN;
use str0m::channel::ChannelId;
use str0m::Rtc;
use tokio_util::bytes::BytesMut;

use super::super::connection::Connection;

/// [`PollDataChannel`] is a wrapper around around [`Rtc`], [`ChannelId`], and [`Connection`] which are needed to read and write, and thenit also implements [`AsyncRead`] and [`AsyncWrite`] to satify libp2p.
// let channel = Arc::new(rtc.channel(*id).unwrap());
#[derive(Debug, Clone)]
pub(crate) struct PollDataChannel {
    /// Connection.rtc_poll_output gives us incoming data
    connection: Arc<Mutex<Connection>>,
    /// Rtc + ChannelId = Channel.write() for outgoing data
    id: ChannelId,
    inner: Arc<Mutex<Rtc>>,
}

impl PollDataChannel {
    pub(crate) fn new(
        rtc: Arc<Mutex<Rtc>>,
        data_channel: str0m::channel::ChannelId,
        connection: Arc<Mutex<Connection>>,
    ) -> Self {
        Self {
            inner: rtc,
            connection,
            id: data_channel,
        }
    }

    /// Whether the data channel is ready for reading or writing.
    fn poll_ready(&mut self, cx: &mut Context) -> Poll<io::Result<()>> {
        todo!()
    }
}

impl AsyncRead for PollDataChannel {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        todo!()
    }
}

impl AsyncWrite for PollDataChannel {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        todo!()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        todo!()
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        todo!()
    }
}
