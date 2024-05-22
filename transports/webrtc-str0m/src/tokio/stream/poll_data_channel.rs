use futures::{AsyncRead, AsyncWrite, FutureExt};
use libp2p_webrtc_utils::MAX_MSG_LEN;
use std::cmp::min;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use str0m::channel::ChannelId;
use str0m::Rtc;
use tokio::sync::mpsc;
use tokio_util::bytes::BytesMut;

use crate::tokio::channel::{ChannelWakers, ReadReady, RtcDataChannelState, StateInquiry};
use crate::tokio::Error;

/// [`PollDataChannel`] is a wrapper around around [`Rtc`], [`ChannelId`], and [`Connection`] which are needed to read and write, and thenit also implements [`AsyncRead`] and [`AsyncWrite`] to satify libp2p.
// let channel = Arc::new(rtc.channel(*id).unwrap()).unwrap().write(data)
#[derive(Debug, Clone)]
pub(crate) struct PollDataChannel {
    /// ChannelId is needed for: Rtc + ChannelId = Channel.write() for outgoing data
    channel_id: ChannelId,

    /// str0m Rtc instance for writing
    rtc: Arc<Mutex<Rtc>>,

    /// Local state
    state: RtcDataChannelState,

    /// Wakers for this [DataChannel].
    wakers: ChannelWakers,

    /// Read Buffer is where the incoming data is stored in case it's bigger than the buffer.
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

    /// Used to send a signal back to the Connection to indicate we are ready to read.
    read_ready_signal: futures::channel::mpsc::Sender<ReadReady>,

    /// State change signal.
    tx_state_inquiry: mpsc::Sender<StateInquiry>,
}

impl PollDataChannel {
    /// Create a new [`PollDataChannel`] that creates and wires up the necessary wakers to the
    /// pollers for the given channel id.
    pub(crate) fn new(
        channel_id: ChannelId,
        rtc: Arc<Mutex<Rtc>>,
        send_wakers: futures::channel::mpsc::Sender<ChannelWakers>,
        read_ready_signal: futures::channel::mpsc::Sender<ReadReady>,
        tx_state_inquiry: mpsc::Sender<StateInquiry>,
    ) -> Result<Self, Error> {
        // We purposely don't use `with_capacity` so we don't eagerly allocate `MAX_READ_BUFFER` per stream.
        let read_buffer = Arc::new(Mutex::new(BytesMut::new()));
        let overloaded = Arc::new(AtomicBool::new(false));

        // Send Wakers to Connection struct for triggering by the Event loop.
        let wakers = ChannelWakers::default();
        if let Err(e) = send_wakers.clone().try_send(wakers.clone()) {
            tracing::error!("Failed to send wakers to Connection: {:?}", e);
        }

        Ok(Self {
            channel_id,
            rtc,
            wakers,
            read_buffer,
            overloaded,
            read_ready_signal,
            tx_state_inquiry,
            state: RtcDataChannelState::Opening,
        })
    }

    /// Polls the state of the data channel.
    fn poll_state(&mut self, cx: &mut Context) -> Poll<Result<RtcDataChannelState, Error>> {
        let (tx, mut rx) = futures::channel::oneshot::channel();

        let state_change = StateInquiry {
            response: tx,
            channel_id: self.channel_id,
        };

        if let Err(e) = self.tx_state_inquiry.try_send(state_change) {
            tracing::error!("Failed to send state_change signal: {:?}", e);
            return Poll::Ready(Err(Error::new(
                io::ErrorKind::Other,
                "Failed to send state_inquiry signal",
            )));
        }

        rx.poll_unpin(cx).map_err(Error::OneshotCanceled)
    }

    /// Whether the data channel is ready for reading or writing (Open).
    fn poll_ready(&mut self, cx: &mut Context) -> Poll<io::Result<()>> {
        match self.poll_state(cx) {
            Poll::Ready(Ok(RtcDataChannelState::Opening)) => {
                self.state = RtcDataChannelState::Opening;
                self.wakers.open.register(cx.waker());
                Poll::Pending
            }
            Poll::Ready(Ok(RtcDataChannelState::Open)) => {
                self.state = RtcDataChannelState::Open;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Ok(RtcDataChannelState::Closed)) => {
                self.state = RtcDataChannelState::Closed;
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "DataChannel is closed",
                )))
            }
            Poll::Ready(Err(e)) => {
                tracing::error!("Failed to get state from Connection: {:?}", e);
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Other,
                    "Failed to get state from Connection",
                )))
            }
            Poll::Pending => Poll::Pending,
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

        // Get read_buffer.
        // Send request (with embedded reply handle) to the Connection to get any new data.
        let (tx, mut rx) = futures::channel::oneshot::channel();
        let read_ready = ReadReady {
            // channel_id: this.channel_id,
            response: tx,
        };

        // Send it!
        if let Err(e) = this.read_ready_signal.try_send(read_ready) {
            tracing::error!("Failed to send read_ready signal: {:?}", e);
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Other,
                "Failed to send read_ready signal",
            )));
        }

        // listen on rx for the data from Connection response
        let input = futures::ready!(rx.poll_unpin(cx)).unwrap();

        // There are two scenarios here:
        // 1. We are asking the Connection if they have any data for us, and they don't.
        // In which case they will reply with an empty Vec<u8> and we will
        // register the waker and return Poll::Pending
        //
        // 2. We are asking the Connection if they have any data for us, and they do.
        // In which case they will reply with a Vec<u8> and we will return Poll::Ready(Ok(len))

        // Handle empty scenario
        if input.is_empty() {
            this.wakers.new_data.register(cx.waker());
            return Poll::Pending;
        }

        let mut read_buffer = this.read_buffer.lock().unwrap();

        if read_buffer.len() + input.len() > MAX_MSG_LEN {
            this.overloaded.store(true, Ordering::SeqCst);
            tracing::warn!("Remote is overloading us with messages, resetting stream",);
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "remote overloaded us with messages",
            )));
        }

        read_buffer.extend_from_slice(&input);

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

    /// Attempt to close the object.
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.state == RtcDataChannelState::Closed {
            return Poll::Ready(Ok(()));
        }

        let mut rtc = self.rtc.lock().unwrap();
        rtc.direct_api().close_data_channel(self.channel_id);
        self.wakers.close.register(cx.waker());

        Poll::Pending
    }
}
