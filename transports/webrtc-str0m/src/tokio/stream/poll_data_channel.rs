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
use str0m::Rtc;
use tokio_util::bytes::BytesMut;

use super::super::connection::Connection;

#[derive(Debug, Clone)]
pub(crate) struct PollDataChannel {
    inner: Arc<Rtc>,
    connection: Arc<Mutex<Connection>>,
    id: str0m::channel::ChannelId,
}

impl PollDataChannel {
    pub(crate) fn new(
        rtc: Rtc,
        data_channel: str0m::channel::ChannelId,
        connection: Connection,
    ) -> Self {
        Self {
            inner: Arc::new(rtc),
            connection: Arc::new(Mutex::new(connection)),
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
