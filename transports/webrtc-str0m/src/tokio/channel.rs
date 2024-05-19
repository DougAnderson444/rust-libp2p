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

/// Wakers for the different types of wakers
#[derive(Debug, Clone, Default)]
pub(crate) struct ChannelWakers {
    /// Waker for when we have new data.
    pub(crate) new_data: Arc<AtomicWaker>,
    /// Waker for when we are waiting for the DC to be opened.
    pub(crate) open: Arc<AtomicWaker>,
    /// Waker for when we are waiting for the DC to be closed.
    pub(crate) close: Arc<AtomicWaker>,
    /// Waker for when we are waiting for the DC to be written to.
    pub(crate) write: Arc<AtomicWaker>,
}

/// Waker Struct for the different types of wakers
#[derive(Debug, Clone, Default)]
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

/// Channel state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RtcDataChannelState {
    /// Channel is closed.
    Closed,

    /// Channel is closing.
    Closing,

    /// Inbound channel is opening.
    InboundOpening,

    /// Outbound channel is opening.
    OutboundOpening,

    /// Channel is open.
    Open,
}
