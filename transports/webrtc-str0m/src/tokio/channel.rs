//! Module concerning WebRTC Data Channels.
//! Each Channel has an Id and a State.

use futures::task::AtomicWaker;
use std::collections::HashMap;
use std::sync::Arc;
use std::task::Context;
use str0m::channel::ChannelId;

/// Data Channel to keep track of identifier and state.
#[derive(Debug)]
pub struct DataChannel {
    /// Channel ID.
    channel_id: ChannelId,

    /// Channel state.
    state: RtcDataChannelState,

    /// Wakers for this [DataChannel].
    wakers: HashMap<WakerType, Waker>,
}

/// Enum of the variosu types of Wakers:
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
pub struct Waker {
    /// Waker
    waker: Arc<AtomicWaker>,
}

impl Waker {
    /// Create a new Waker.
    pub fn new(waker: Arc<AtomicWaker>) -> Self {
        Self { waker }
    }

    /// Register the Waker to the given Context.
    pub fn register(&self, cx: &mut Context) {
        self.waker.register(cx.waker());
    }
}

impl DataChannel {
    /// Create a new DataChannel.
    pub fn new(channel_id: ChannelId, state: RtcDataChannelState) -> Self {
        Self {
            channel_id,
            state,
            wakers: HashMap::new(),
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

    /// Sets the Open Waker.
    pub fn set_waker(&mut self, waker: Waker, waker_type: WakerType) {
        self.wakers.insert(waker_type, waker);
    }

    /// Register the Open Waker to the given Context.
    pub fn register_waker(&self, cx: &mut Context, waker_type: WakerType) {
        self.wakers.get(&waker_type).unwrap().register(cx);
    }

    /// Wake the Waker.
    pub fn wake(&self, waker_type: WakerType) {
        self.wakers.get(&waker_type).unwrap().waker.wake();
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
