use std::cmp::min;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::{AsyncRead, AsyncWrite};
use libp2p_webrtc_utils::MAX_MSG_LEN;
use str0m::channel::{ChannelData, ChannelId};
use str0m::Rtc;
use tokio_util::bytes::BytesMut;

use crate::tokio::channel::{ChannelWakers, RtcDataChannelState, WakerType};
use crate::tokio::Error;

/// [`PollDataChannel`] is a wrapper around around [`Rtc`], [`ChannelId`], and [`Connection`] which are needed to read and write, and thenit also implements [`AsyncRead`] and [`AsyncWrite`] to satify libp2p.
// let channel = Arc::new(rtc.channel(*id).unwrap()).unwrap().write(data)
#[derive(Debug, Clone)]
pub(crate) struct PollDataChannel {
    /// ChannelId is needed for: Rtc + ChannelId = Channel.write() for outgoing data
    channel_id: ChannelId,

    /// str0m Rtc instance for writing
    rtc: Arc<Mutex<Rtc>>,

    /// Channel state.
    state: RtcDataChannelState,

    /// Wakers for this [DataChannel].
    wakers: ChannelWakers,

    /// Read Buffer is where the incoming data is stored.
    read_buffer: Arc<Mutex<BytesMut>>,

    /// Whether we've been overloaded with data by the remote.
    ///
    /// This is set to `true` in case `read_buffer` overflows,
    /// i.e. the remote is sending us messages faster than we can read them.
    /// In that case, we return an [`std::io::Error`] from [`AsyncRead`]
    /// or [`AsyncWrite`], depending which one gets called earlier.
    /// Failing these will (very likely), cause the application developer
    /// to drop the stream which resets it.
    overloaded: Arc<AtomicBool>,
}

impl PollDataChannel {
    /// Create a new [`PollDataChannel`] that creates and wires up the necessary wakers to the
    /// pollers for the given channel id.
    pub(crate) fn new(
        channel_id: ChannelId,
        state: RtcDataChannelState,
        rtc: Arc<Mutex<Rtc>>,
    ) -> Result<Self, Error> {
        // We purposely don't use `with_capacity` so we don't eagerly allocate `MAX_READ_BUFFER` per stream.
        let read_buffer = Arc::new(Mutex::new(BytesMut::new()));
        let overloaded = Arc::new(AtomicBool::new(false));

        // TODO: Send Wakers to Connection struct (via Sender?)
        let wakers = ChannelWakers::default();

        Ok(Self {
            channel_id,
            rtc,
            state,
            wakers,
            read_buffer,
            overloaded,
        })
    }

    /// Get the channel ID.
    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// Get the channel state.
    pub fn state(&self) -> RtcDataChannelState {
        self.state.clone()
    }

    /// Returns the [RtcDataChannelState] of the [RtcDataChannel]
    fn state_mut(&mut self) -> &mut RtcDataChannelState {
        // Pull the RtcDataChannelState from the connection.channels(channel_id)
        &mut self.state
    }

    /// Wake the Waker.
    pub fn wake(&self, waker_type: WakerType) {
        match waker_type {
            WakerType::NewData => self.wakers.new_data.wake(),
            WakerType::Open => self.wakers.open.wake(),
            WakerType::Close => self.wakers.close.wake(),
            WakerType::Write => self.wakers.write.wake(),
        }
    }

    /// Set the value of read_buffer.
    pub fn set_read_buffer(&self, input: &ChannelData) {
        let mut read_buffer = self.read_buffer.lock().unwrap();

        if read_buffer.len() + input.data.len() > MAX_MSG_LEN {
            self.overloaded.store(true, Ordering::SeqCst);
            tracing::warn!("Remote is overloading us with messages, resetting stream",);
            return;
        }

        read_buffer.extend_from_slice(&input.data.to_vec());
    }

    /// Get the value of read_buffer.
    pub fn read_buffer(&self) -> Arc<Mutex<BytesMut>> {
        self.read_buffer.clone()
    }

    /// Sets the state of the [DataChannel].
    pub(crate) fn set_state(&mut self, state: RtcDataChannelState) {
        self.state = state;
    }
    /// Whether the data channel is ready for reading or writing.
    fn poll_ready(&mut self, cx: &mut Context) -> Poll<io::Result<()>> {
        match self.state_mut() {
            RtcDataChannelState::InboundOpening | RtcDataChannelState::OutboundOpening => {
                // open_waker register(cx.waker());
                // We need to signal to the connection that we are waiting for the channel to open.
                self.wakers.open.register(cx.waker());
                return Poll::Pending;
            }
            RtcDataChannelState::Closing | RtcDataChannelState::Closed => {
                Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
            }
            RtcDataChannelState::Open => Poll::Ready(Ok(())),
        }
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
        let read_buffer = this.read_buffer();

        let mut read_buffer = read_buffer.lock().unwrap();

        if read_buffer.is_empty() {
            this.wakers.new_data.register(cx.waker());

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

        // TODO: Buffer data?

        // write data to the channel
        let mut rtc = this.rtc.lock().unwrap();
        let mut channel = rtc.channel(this.channel_id).unwrap();
        let binary = true;
        match channel.write(binary, buf) {
            Ok(len) => {
                let len: usize = len;
                Poll::Ready(Ok(len))
            }
            Err(e) => Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // There is no write buffer to flush in str0m
        // So we just return Ok?
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        if this.state() == RtcDataChannelState::Closed {
            return Poll::Ready(Ok(()));
        }

        if this.state() == RtcDataChannelState::Closing {
            let mut rtc = this.rtc.lock().unwrap();
            rtc.direct_api().close_data_channel(this.channel_id);
        }

        // Register WakerType::Close with cx.waker()
        this.wakers.close.register(cx.waker());

        Poll::Pending
    }
}
