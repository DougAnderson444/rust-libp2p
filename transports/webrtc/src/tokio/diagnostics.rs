//! PeerConnection lifecycle logging for webrtc-direct interop debugging.
//!
//! All events use `target: "libp2p_webrtc_mux"` so one `RUST_LOG` filter covers native + WASM.

use std::sync::Arc;

use futures::lock::Mutex as FutMutex;
use webrtc::{
    ice_transport::ice_connection_state::RTCIceConnectionState,
    peer_connection::{
        peer_connection_state::RTCPeerConnectionState,
        signaling_state::RTCSignalingState,
        RTCPeerConnection,
    },
};

/// Register state-transition handlers and SCTP error logging on `pc`.
pub(crate) fn attach_peer_connection_diagnostics(pc: &RTCPeerConnection, role: &'static str) {
    pc.on_signaling_state_change(Box::new({
        let role = role.to_owned();
        move |state: RTCSignalingState| {
            let role = role.clone();
            Box::pin(async move {
                tracing::debug!(
                    target: "libp2p_webrtc_mux",
                    role = %role,
                    ?state,
                    "PeerConnection signaling state"
                );
            })
        }
    }));

    pc.on_ice_connection_state_change(Box::new({
        let role = role.to_owned();
        move |state: RTCIceConnectionState| {
            let role = role.clone();
            Box::pin(async move {
                tracing::debug!(
                    target: "libp2p_webrtc_mux",
                    role = %role,
                    ?state,
                    "PeerConnection ICE connection state"
                );
            })
        }
    }));

    pc.on_peer_connection_state_change(Box::new({
        let role = role.to_owned();
        move |state: RTCPeerConnectionState| {
            let role = role.clone();
            Box::pin(async move {
                tracing::debug!(
                    target: "libp2p_webrtc_mux",
                    role = %role,
                    ?state,
                    "PeerConnection connection state"
                );
            })
        }
    }));

    pc.sctp().on_error(Box::new({
        let role = role.to_owned();
        move |err| {
            let role = role.clone();
            Box::pin(async move {
                tracing::error!(
                    target: "libp2p_webrtc_mux",
                    role = %role,
                    error = %err,
                    "SCTP transport error"
                );
            })
        }
    }));
}

/// One-line snapshot of PC + SCTP state at an upgrade milestone.
pub(crate) fn log_upgrade_step(pc: &RTCPeerConnection, role: &'static str, step: &'static str) {
    tracing::debug!(
        target: "libp2p_webrtc_mux",
        role = %role,
        step = %step,
        signaling_state = ?pc.signaling_state(),
        ice_connection_state = ?pc.ice_connection_state(),
        connection_state = ?pc.connection_state(),
        sctp_state = ?pc.sctp().state(),
        "upgrade milestone"
    );
}

/// Log SCTP association state when muxer polls (helps spot DCEP vs handler gaps).
pub(crate) fn log_sctp_snapshot(
    peer_conn: &Arc<FutMutex<RTCPeerConnection>>,
    context: &'static str,
) {
    let Some(pc) = peer_conn.try_lock() else {
        tracing::trace!(
            target: "libp2p_webrtc_mux",
            context = %context,
            "SCTP snapshot skipped (peer_conn locked)"
        );
        return;
    };

    tracing::trace!(
        target: "libp2p_webrtc_mux",
        context = %context,
        sctp_state = ?pc.sctp().state(),
        connection_state = ?pc.connection_state(),
        ice_connection_state = ?pc.ice_connection_state(),
        "SCTP snapshot"
    );
}
