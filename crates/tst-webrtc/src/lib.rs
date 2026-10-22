#![doc = include_str!("../README.md")]
//!
//! The server's closing contract is documented under the `# Closing`
//! heading of [`WhepServer`].
//!
//! The `klv` data channel is opened by the server (DCEP, in-band on the
//! SCTP association). The viewer's offer must carry a data-channel section
//! (create any data channel before `createOffer`), and the channel arrives
//! in `ondatachannel` with the label `klv`. The channel is reliable and
//! unordered: a record never waits behind an earlier one, and each
//! message names its frame's RTP timestamp. A viewer whose offer has no
//! data-channel section gets video, with KLV in SEI, and no channel.
//!
//! A KLV record travels on the data channel when it is at most
//! [`KLV_DC_LIMIT`] bytes (256 KiB); a larger one is refused by
//! `push_klv`. The server's SCTP send buffer is sized for a record at the
//! limit. A record is dropped for a viewer whose send buffer has no room
//! for it. A record is sent as one data-channel message of the record plus
//! a 13-byte header; for a viewer whose `max-message-size` is smaller than
//! that message the record is dropped. The size is 64 KiB when the viewer
//! states none and 256 KiB in current Chrome and Safari, so their largest
//! record is 262 131 bytes; Firefox takes any record up to the limit.
//! Every such drop is counted in
//! [`MountStats::klv_dc_dropped_viewer`] and
//! [`SessionStats::dc_messages_dropped`]. The video is never held back for
//! the data channel.
//!
//! A session's media goes only to the address that carried its DTLS
//! handshake: the source of the first DTLS record is fixed, a later DTLS
//! record from another address is dropped, and once the handshake completes
//! that address is pinned. Nothing is sent to any other address, and
//! datagrams from any other address are dropped and counted in
//! [`ServerStats::datagrams_unrouted`].
//! A NAT rebinding or a move to another network interface therefore needs
//! a new session (a new POST). Before the handshake completes, a session
//! admits datagrams from at most 16 distinct source addresses.
#![forbid(unsafe_code)]

pub mod clock;
pub mod config;
pub mod error;
pub mod mount;
pub mod packetize;
pub mod sei;
pub mod stats;
pub mod url;

pub mod server;

mod binding_kind;

#[cfg(feature = "tls")]
mod tls;

#[doc(hidden)]
pub use clock::ManualClock;
pub use clock::{Clock, SystemClock};
pub use config::{AudioPolicy, KlvCarriage, MountConfig, MountKind, VideoCodec};
pub use error::{PushError, WhepServerError};
pub use mount::{KLV_DC_LIMIT, MountHandle};
pub use server::WhepServer;
pub use stats::{IceState, MountStats, ServerStats, SessionStats};

/// Installs str0m's process-wide crypto provider. The first install wins and
/// later calls are ignored, so calling this on every bind is safe.
pub(crate) fn ensure_crypto() {
    str0m::crypto::from_feature_flags().install_process_default();
}
