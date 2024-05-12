use libp2p_identity::PeerId;
use thiserror::Error;

/// Error in WebRTC.
#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error")]
    Io(#[from] std::io::Error),

    #[error("failed to authenticate peer")]
    Authentication(#[from] libp2p_noise::Error),

    // Authentication errors.
    #[error("invalid peer ID (expected {expected}, got {got})")]
    InvalidPeerID { expected: PeerId, got: PeerId },

    #[error("no active listeners, can not dial without a previous listen")]
    NoListeners,

    #[error("internal error: {0} (see debug logs)")]
    Internal(String),
}
