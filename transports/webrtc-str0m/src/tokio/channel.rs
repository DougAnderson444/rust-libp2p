//! Module concerning WebRTC Data Channels.
//! Each Channel has an Id and a State.

use futures::task::AtomicWaker;
use std::sync::{Arc, Mutex};
use str0m::channel::ChannelId;
use tokio_util::bytes::BytesMut;

#[derive(Debug)]
pub(crate) struct ChannelDetails {
    /// The wakers for this channel_id
    pub(crate) wakers: ChannelWakers,
    /// [ReadReady] channel data receiver.
    pub(crate) channel_data_rx: Mutex<futures::channel::mpsc::Receiver<ReadReady>>,
    /// The current state of this channel id
    pub(crate) state: RtcDataChannelState,
    /// Read buffer where incoming data is stored until it is polled by PollDataChannel.
    pub(crate) read_buffer: Arc<Mutex<BytesMut>>,
}

/// Wakers for the different types of wakers
#[derive(Debug, Clone, Default)]
pub(crate) struct ChannelWakers {
    /// Waker for when we have new data.
    pub(crate) new_data: Arc<AtomicWaker>,
    /// New state of the DataChannel.
    pub(crate) open: Arc<AtomicWaker>,
    /// Waker for when we are waiting for the DC to be written to.
    pub(crate) write: Arc<AtomicWaker>,
    /// CLose waker, wakes when state is Closed.
    pub(crate) close: Arc<AtomicWaker>,
}

/// Encapsulates State changes for the DataChannel sent form the Connection.
#[derive(Debug)]
pub(crate) struct StateInquiry {
    /// The channel id we want to know about
    pub(crate) channel_id: ChannelId,
    /// The new state of the DataChannel.
    pub(crate) response: futures::channel::oneshot::Sender<RtcDataChannelState>,
    /// The Type of State Inquiry.
    pub(crate) inquiry_type: RequestState,
}

/// Enum to Update the state type: Either RtcState, ReadBuffer
#[derive(Debug)]
pub(crate) enum RequestState {
    /// The state of the DataChannel.
    RtcState,
    /// The read buffer of the DataChannel.
    ReadBuffer,
}

/// Enum for the response of [RequestState] containing the request type and the response value
#[derive(Debug)]
pub(crate) enum StateValues {
    /// The state of the DataChannel.
    RtcState(RtcDataChannelState),
    /// The read buffer of the DataChannel.
    ReadBuffer(Vec<u8>),
}

/// State Update, includes the channel id and the new state.
#[derive(Debug)]
pub(crate) struct StateUpdate {
    /// The channel id we want to know about
    pub(crate) channel_id: ChannelId,
    /// The new state of the DataChannel.
    pub(crate) state: StateValues,
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
    /// Second state, Channel is opening.
    Opening,

    /// Third state, Channel is open.
    Open,

    /// Channel is closed.
    Closed,
}
