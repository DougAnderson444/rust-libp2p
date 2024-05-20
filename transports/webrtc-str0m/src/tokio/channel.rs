//! Module concerning WebRTC Data Channels.
//! Each Channel has an Id and a State.

use futures::task::AtomicWaker;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub(crate) struct ChannelDetails {
    /// The wakers for this channel_id
    pub(crate) wakers: ChannelWakers,
    /// [ReadReady] channel data receiver.
    pub(crate) channel_data_rx: Mutex<futures::channel::mpsc::Receiver<ReadReady>>,
    /// [StateChange] channel data receiver.
    pub(crate) channel_state_rx: Mutex<futures::channel::mpsc::Receiver<StateChange>>,
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

/// Encapsulates State changes for the DataChannel sent form the Connection.
#[derive(Debug)]
pub(crate) struct StateChange {
    /// The new state of the DataChannel.
    pub(crate) response: futures::channel::oneshot::Sender<RtcDataChannelState>,
}

/// Simple struct to indicate that the channel is ready to read. Has a reply oneshot channel
/// embedded so that [crate::tokio::Connection] can send the read_buffer back to the [PollDataChannel].
#[derive(Debug)]
pub(crate) struct ReadReady {
    // /// The channel id of the channel that is ready to read.
    // pub(crate) channel_id: ChannelId,
    /// The reply channel to send the read_buffer back to the [PollDataChannel].
    pub(crate) response: futures::channel::oneshot::Sender<Vec<u8>>,
}

/// Channel state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum RtcDataChannelState {
    /// First State, newly created
    Created,

    /// Second state, Channel is opening.
    Opening,

    /// Third state, Channel is open.
    Open,

    /// Channel is closing.
    Closing,

    /// Channel is closed.
    Closed,
}
