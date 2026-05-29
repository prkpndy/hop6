//! On-the-wire message format.
//!
//! Every peer connection is a bidirectional stream of **newline-delimited JSON** objects
//! (see `network::peer_loop`). Keeping one JSON value per line lets us use
//! `tokio_util::codec::LinesCodec` for framing and `serde_json` for (de)serialization, with no
//! hand-rolled length prefixes.

use serde::{Deserialize, Serialize};

/// A single chat message as it travels between two peers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireMsg {
    /// The sender's own `.onion` address (without the trailing `.onion`, as torut reports it),
    /// so the receiver can label the message even on a freshly accepted inbound connection.
    pub from_onion: String,
    /// The message text.
    pub body: String,
    /// Unix epoch seconds when the sender created the message. Purely informational; clocks are
    /// not trusted for ordering.
    pub ts: u64,
}

impl WireMsg {
    pub fn new(from_onion: impl Into<String>, body: impl Into<String>, ts: u64) -> Self {
        Self {
            from_onion: from_onion.into(),
            body: body.into(),
            ts,
        }
    }

    /// Serialize to a single JSON line (no trailing newline — `LinesCodec` adds the delimiter).
    pub fn to_line(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    /// Parse one JSON line received from a peer.
    pub fn from_line(line: &str) -> serde_json::Result<Self> {
        serde_json::from_str(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_a_line() {
        let msg = WireMsg::new("abc.onion", "hello, tor", 1_700_000_000);
        let line = msg.to_line().unwrap();
        assert!(!line.contains('\n'), "a line must not embed newlines");
        let back = WireMsg::from_line(&line).unwrap();
        assert_eq!(back.from_onion, "abc.onion");
        assert_eq!(back.body, "hello, tor");
        assert_eq!(back.ts, 1_700_000_000);
    }
}
