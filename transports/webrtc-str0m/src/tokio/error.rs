use libp2p_identity::PeerId;
use std::io::{self, ErrorKind};
use thiserror::Error;
use tokio::sync::mpsc::error::TrySendError;

/// Error in WebRTC.
#[derive(Error, Debug)]
pub enum Error {
    /// Error in the WebRTC transport.
    #[error("`str0m` error: `{0}`")]
    WebRtc(#[from] str0m::RtcError),

    /// IO Error.
    #[error("IO error")]
    Io(#[from] std::io::Error),

    #[error("I/O error: `{0}`")]
    IoError(ErrorKind),
    /// Authentication error.
    #[error("failed to authenticate peer")]
    Authentication(#[from] libp2p_noise::Error),

    // Invalid peer ID.
    #[error("invalid peer ID (expected {expected}, got {got})")]
    InvalidPeerID { expected: PeerId, got: PeerId },

    #[error("no active listeners, can not dial without a previous listen")]
    NoListeners,

    #[error("internal error: {0} (see debug logs)")]
    Internal(String),

    #[error("Invalid data")]
    InvalidData,

    /// Send Error
    #[error("send error: `{0}`")]
    SendError(#[from] TrySendError<Vec<u8>>),

    /// DTLS certificate error.
    #[error("DTLS certificate error, it is missing or invalid")]
    DtlsCert,

    /// Str0m Stun Error
    #[error("WebRTC Network error: `{0}`")]
    NetError(#[from] str0m::error::NetError),

    /// Disconnected while Opening
    #[error("disconnected while opening")]
    Disconnected,
}
