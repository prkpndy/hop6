//! On-the-wire protocol.
//!
//! Every peer connection is a bidirectional stream of **newline-delimited JSON** frames (see
//! `network::run_connection`). Each line is one [`Frame`]: either a chat [`WireMsg`] or a
//! heartbeat ([`Frame::Ping`] / [`Frame::Pong`]). Newline-delimited JSON lets us use
//! `tokio_util::codec::LinesCodec` for framing and `serde_json` for (de)serialization, with no
//! hand-rolled length prefixes.

use serde::{Deserialize, Serialize};

/// A single chat message as it travels between two peers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireMsg {
    /// The sender's own `.onion` address, so the receiver can label the message even on a
    /// freshly accepted inbound connection (where Tor doesn't reveal who connected).
    pub from_onion: String,
    /// The message text.
    pub body: String,
    /// Unix epoch seconds when the sender created the message. Informational; not trusted.
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
}

/// A framed unit on the wire: a chat message, or a heartbeat used to detect dead connections
/// (e.g. after a laptop sleeps and the Tor circuit silently dies).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Frame {
    /// A chat message.
    #[serde(rename = "msg")]
    Msg(WireMsg),
    /// Liveness probe; the receiver answers with [`Frame::Pong`].
    #[serde(rename = "ping")]
    Ping,
    /// Reply to a [`Frame::Ping`].
    #[serde(rename = "pong")]
    Pong,
}

impl Frame {
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
    fn chat_frame_round_trips_through_a_line() {
        let frame = Frame::Msg(WireMsg::new("abc.onion", "hello, tor", 1_700_000_000));
        let line = frame.to_line().unwrap();
        assert!(!line.contains('\n'), "a line must not embed newlines");
        match Frame::from_line(&line).unwrap() {
            Frame::Msg(m) => {
                assert_eq!(m.from_onion, "abc.onion");
                assert_eq!(m.body, "hello, tor");
                assert_eq!(m.ts, 1_700_000_000);
            }
            other => panic!("expected Msg, got {other:?}"),
        }
    }

    #[test]
    fn heartbeat_frames_round_trip() {
        assert!(matches!(
            Frame::from_line(&Frame::Ping.to_line().unwrap()).unwrap(),
            Frame::Ping
        ));
        assert!(matches!(
            Frame::from_line(&Frame::Pong.to_line().unwrap()).unwrap(),
            Frame::Pong
        ));
    }
}
