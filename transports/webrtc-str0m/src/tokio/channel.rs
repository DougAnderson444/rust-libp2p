//! Module concerning WebRTC Data Channels.
//! Each Channel has an Id and a State.

use futures::task::AtomicWaker;
use libp2p_webrtc_utils::MAX_MSG_LEN;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Context;
use str0m::channel::{ChannelData, ChannelId};
use tokio_util::bytes::BytesMut;

/// Data Channel to keep track of identifier and state.
#[derive(Debug)]
pub struct DataChannel {
    /// Channel ID.
    channel_id: ChannelId,

    /// Channel state.
    state: RtcDataChannelState,

    /// Wakers for this [DataChannel].
    wakers: HashMap<WakerType, Waker>,

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

/// Enum of the various types of Wakers:
/// - new data
/// - open
/// - write
/// - close
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum WakerType {
    /// Waker for when we have new data.
    NewData,

    /// Waker for when we are waiting for the DC to be opened.
    Open,

    /// Waker for when we are waiting for the DC to be closed.
    Close,

    /// Waker for when we are waiting for the DC to be written to.
    Write,
}

/// Waker Struct for the different types of wakers
#[derive(Debug, Clone)]
pub(crate) struct Waker {
    /// Waker
    waker: Arc<AtomicWaker>,
}

impl Waker {
    /// Create a new Waker.
    pub(crate) fn new(waker: Arc<AtomicWaker>) -> Self {
        Self { waker }
    }

    /// Register the Waker to the given Context.
    pub(crate) fn register(&self, cx: &mut Context) {
        self.waker.register(cx.waker());
    }
}

impl DataChannel {
    /// Create a new DataChannel.
    pub fn new(channel_id: ChannelId, state: RtcDataChannelState) -> Self {
        let read_buffer = Arc::new(Mutex::new(BytesMut::new())); // We purposely don't use `with_capacity` so we don't eagerly allocate `MAX_READ_BUFFER` per stream.
        let overloaded = Arc::new(AtomicBool::new(false));

        Self {
            channel_id,
            state,
            wakers: HashMap::new(),
            read_buffer,
            overloaded,
        }
    }

    /// Get the channel ID.
    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// Get the channel state.
    pub fn state(&self) -> RtcDataChannelState {
        self.state.clone()
    }

    /// Sets the Open Waker for each WakerType
    pub fn set_wakers(&mut self) {
        self.wakers
            .insert(WakerType::NewData, Waker::new(Arc::new(AtomicWaker::new())));
        self.wakers
            .insert(WakerType::Open, Waker::new(Arc::new(AtomicWaker::new())));
        self.wakers
            .insert(WakerType::Close, Waker::new(Arc::new(AtomicWaker::new())));
        self.wakers
            .insert(WakerType::Write, Waker::new(Arc::new(AtomicWaker::new())));
    }

    /// Register the Open Waker to the given Context.
    pub fn register_waker(&self, cx: &mut Context, waker_type: WakerType) {
        self.wakers.get(&waker_type).unwrap().register(cx);
    }

    /// Wake the Waker.
    pub fn wake(&self, waker_type: WakerType) {
        self.wakers.get(&waker_type).unwrap().waker.wake();
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
}

/// Channel state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RtcDataChannelState {
    /// Channel is closing.
    Closing,

    /// Inbound channel is opening.
    InboundOpening,

    /// Outbound channel is opening.
    OutboundOpening,

    /// Channel is open.
    Open,
}
