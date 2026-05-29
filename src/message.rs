//! The two message types that flow over the Tokio channels connecting the network layer and
//! the TUI. This is the *only* coupling between the async network tasks and the render loop —
//! neither side ever touches the other's state directly, which is what keeps the UI from ever
//! blocking on network I/O.
//!
//! ```text
//!   network tasks ──NetEvent──▶ mpsc ──▶ UI loop (App::apply_net_event)
//!        ▲                                      │
//!        └──────────── mpsc ◀──UiCommand────────┘ (App::on_key)
//! ```

use crate::wire::WireMsg;

/// A locally-unique handle for a peer connection. We hand these out instead of using the
/// `.onion` string as a key so that the UI can refer to a connection stably even before (or
/// after) we learn the remote identity, and so re-connects get a fresh slot.
pub type PeerId = u32;

/// Network/Tor layer → UI. The UI loop owns the single receiver; every peer task and the
/// inbound acceptor hold a clone of the sender.
#[derive(Debug)]
pub enum NetEvent {
    /// Our own ephemeral onion service is published; this is our identity to share with peers.
    OwnOnionReady { onion: String },
    /// A peer connection was established (either we dialed out, or we accepted inbound).
    PeerConnected {
        peer: PeerId,
        onion: String,
        inbound: bool,
    },
    /// A peer connection closed cleanly (EOF).
    PeerDisconnected { peer: PeerId },
    /// A peer connection failed to establish or errored mid-stream.
    PeerError { peer: PeerId, err: String },
    /// A chat message arrived from a connected peer.
    Message { peer: PeerId, msg: WireMsg },
    /// Free-form status text for the status bar / system log (e.g. "dialing <onion>...").
    Status(String),
}

/// UI → network manager. The UI holds a single sender; the network manager owns the receiver
/// and routes per-peer commands to the right connection task.
#[derive(Debug)]
pub enum UiCommand {
    /// Dial a new outbound peer by `.onion` address (with or without the `.onion` suffix).
    Connect { onion: String },
    /// Send a chat message to an established peer.
    Send { peer: PeerId, body: String },
    /// Drop a peer connection.
    Disconnect { peer: PeerId },
    /// Tear everything down (user is quitting).
    Shutdown,
}
