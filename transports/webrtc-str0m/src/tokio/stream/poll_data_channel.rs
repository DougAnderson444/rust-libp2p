use std::cmp::min;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use futures::task::AtomicWaker;
use futures::{AsyncRead, AsyncWrite};
use libp2p_webrtc_utils::MAX_MSG_LEN;
use str0m::channel::ChannelId;
use str0m::Rtc;
use tokio_util::bytes::BytesMut;

use super::super::connection::Connection;
use crate::tokio::channel::{RtcDataChannelState, Waker, WakerType};
use crate::tokio::Error;

/// [`PollDataChannel`] is a wrapper around around [`Rtc`], [`ChannelId`], and [`Connection`] which are needed to read and write, and thenit also implements [`AsyncRead`] and [`AsyncWrite`] to satify libp2p.
// let channel = Arc::new(rtc.channel(*id).unwrap()).unwrap().write(data)
#[derive(Debug, Clone)]
pub(crate) struct PollDataChannel {
    /// Connection.rtc_poll_output gives us incoming data
    connection: Arc<Mutex<Connection>>,
    /// Rtc + ChannelId = Channel.write() for outgoing data
    channel_id: ChannelId,
}

impl PollDataChannel {
    /// Create a new [`PollDataChannel`] that creates and wires up the necessary wakers to the
    /// pollers for the given channel id.
    pub(crate) fn new(
        channel_id: str0m::channel::ChannelId,
        connection: Arc<Mutex<Connection>>,
    ) -> Result<Self, Error> {
        // The constructor sets up this channel to monitor:
        // - When the channel is ready to read or write
        // - Wake when then is new data to read
        // - Wake then there is new data to write
        // - Wake when the channel is to close

        // In the connection.channels HashMap, we set the Channel.open_waker to the open_waker
        // When the Event::ChannelOpen occurs, we wake the open_waker
        connection
            .lock()
            .unwrap()
            .channels()
            .get_mut(&channel_id)
            .ok_or(Error::ChannelDoesntExist)?
            .set_wakers();

        Ok(Self {
            connection,
            channel_id,
        })
    }

    /// Returns the [RtcDataChannelState] of the [RtcDataChannel]
    fn ready_state(&self) -> RtcDataChannelState {
        // Pull the RtcDataChannelState from the connection.channels(channel_id)
        self.connection
            .lock()
            .unwrap()
            .channels()
            .get(&self.channel_id)
            .unwrap()
            .state()
    }

    /// Whether the data channel is ready for reading or writing.
    fn poll_ready(&mut self, cx: &mut Context) -> Poll<io::Result<()>> {
        match self.ready_state() {
            RtcDataChannelState::InboundOpening | RtcDataChannelState::OutboundOpening => {
                // open_waker register(cx.waker());
                self.connection
                    .lock()
                    .unwrap()
                    .channels()
                    .get_mut(&self.channel_id)
                    .unwrap()
                    .register_waker(cx, WakerType::Open);
                return Poll::Pending;
            }
            RtcDataChannelState::Closing => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
            RtcDataChannelState::Open => Poll::Ready(Ok(())),
        }
    }

    /// Get this mutable connection
    fn connection(&mut self) -> MutexGuard<Connection> {
        self.connection.lock().unwrap()
    }
}

impl AsyncRead for PollDataChannel {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        futures::ready!(this.poll_ready(cx))?;

        // Get read_buffer
        let chid = this.channel_id;
        let read_buffer = this.connection().channel(chid).read_buffer();

        let mut read_buffer = read_buffer.lock().unwrap();

        if read_buffer.is_empty() {
            // Register WakerType::NewData with cx.waker()
            this.connection()
                .channel(chid)
                .register_waker(cx, WakerType::NewData);
            return Poll::Pending;
        }

        // Ensure that we:
        // - at most return what the caller can read (`buf.len()`)
        // - at most what we have (`read_buffer.len()`)
        let split_index = min(buf.len(), read_buffer.len());

        let bytes_to_return = read_buffer.split_to(split_index);
        let len = bytes_to_return.len();
        buf[..len].copy_from_slice(&bytes_to_return);

        Poll::Ready(Ok(len))
    }
}

impl AsyncWrite for PollDataChannel {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        futures::ready!(this.poll_ready(cx))?;

        todo!()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        todo!()
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        todo!()
    }
}
