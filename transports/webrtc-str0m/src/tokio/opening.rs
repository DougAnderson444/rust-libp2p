/// Opening WebRTC connection.
///
/// This object is used to track an opening connection which starts with a Noise handshake.
/// After the handshake is done, this object is destroyed and a new WebRTC connection object
/// is created which implements a normal connection event loop dealing with substreams.
pub struct OpeningWebRtcConnection {}
