//! The network layer: a single **connection-manager** task plus one **peer task** per live
//! connection. None of this code ever touches the TUI — it speaks only in [`NetEvent`]s (out)
//! and [`UiCommand`]s (in), so the render loop can never be blocked by a slow Tor circuit.
//!
//! ```text
//!                          ┌───────────────── manager task ─────────────────┐
//!   TcpListener (inbound) ─┤ accept → assign PeerId → spawn peer task         │
//!   UiCommand::Connect ────┤ dial via SOCKS5 (own task) → spawn peer task     │
//!   UiCommand::Send ───────┤ build WireMsg → route to that peer's line_tx     │
//!   UiCommand::Disconnect ─┤ drop the peer's line_tx (closes its task)        │
//!   peer task finished ────┤ remove from roster map                           │
//!                          └─────────────────────────────────────────────────┘
//! ```

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_socks::tcp::Socks5Stream;
use tokio_util::codec::{Framed, LinesCodec};

use crate::message::{NetEvent, PeerId, UiCommand};
use crate::tor;
use crate::wire::WireMsg;

/// Tor's local SOCKS5 proxy (`SocksPort 9050` in torrc).
pub const SOCKS_ADDR: &str = "127.0.0.1:9050";
/// Virtual port our onion service exposes (and the port we dial peers on).
pub const ONION_VIRTUAL_PORT: u16 = 80;

/// Current Unix time in whole seconds (best-effort; only used as a display hint).
fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Entry point for the network layer. Binds the inbound listener, then runs the manager loop
/// until the UI sends [`UiCommand::Shutdown`] (or its command channel closes).
///
/// `own_onion` is our published `.onion`, stamped into every outgoing [`WireMsg`] so peers can
/// label us — important for inbound connections, where Tor doesn't reveal who connected.
pub async fn run(
    local_port: u16,
    own_onion: String,
    to_ui: mpsc::UnboundedSender<NetEvent>,
    mut from_ui: mpsc::UnboundedReceiver<UiCommand>,
) -> Result<()> {
    // Bind the loopback listener that Tor forwards inbound onion traffic to.
    let listener = TcpListener::bind(("127.0.0.1", local_port)).await?;
    let _ = to_ui.send(NetEvent::Status(format!(
        "listening for inbound peers on 127.0.0.1:{local_port}"
    )));

    // Inbound accepts arrive here from a dedicated accept task so the manager loop never blocks.
    let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel::<TcpStream>();
    tokio::spawn(async move {
        // Ends when the listener errors; `break` when the manager has gone away.
        while let Ok((stream, _addr)) = listener.accept().await {
            if inbound_tx.send(stream).is_err() {
                break; // manager gone
            }
        }
    });

    // Peer tasks report their own termination here so the manager can prune the roster map.
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<PeerId>();

    // PeerId -> sender of already-serialized JSON lines destined for that peer.
    let mut peers: HashMap<PeerId, mpsc::UnboundedSender<String>> = HashMap::new();
    let mut next_id: PeerId = 1;

    loop {
        tokio::select! {
            // ── UI commands ────────────────────────────────────────────────────────────
            cmd = from_ui.recv() => match cmd {
                Some(UiCommand::Connect { onion }) => {
                    let id = next_id; next_id += 1;
                    let (line_tx, line_rx) = mpsc::unbounded_channel::<String>();
                    peers.insert(id, line_tx);
                    spawn_dialer(id, onion, line_rx, to_ui.clone(), done_tx.clone());
                }
                Some(UiCommand::Send { peer, body }) => {
                    if let Some(tx) = peers.get(&peer) {
                        match WireMsg::new(own_onion.clone(), body, now_ts()).to_line() {
                            Ok(line) => { let _ = tx.send(line); }
                            Err(e) => { let _ = to_ui.send(NetEvent::Status(format!("encode error: {e}"))); }
                        }
                    }
                }
                Some(UiCommand::Disconnect { peer }) => {
                    // Dropping the sender closes the peer task's line channel → it exits.
                    if peers.remove(&peer).is_some() {
                        let _ = to_ui.send(NetEvent::PeerDisconnected { peer });
                    }
                }
                Some(UiCommand::Shutdown) | None => break,
            },

            // ── Inbound connections ───────────────────────────────────────────────────
            Some(stream) = inbound_rx.recv() => {
                let id = next_id; next_id += 1;
                let (line_tx, line_rx) = mpsc::unbounded_channel::<String>();
                peers.insert(id, line_tx);
                // onion is unknown until the remote sends its first message (Tor hides the
                // caller's identity); App fills it in from WireMsg::from_onion.
                let _ = to_ui.send(NetEvent::PeerConnected { peer: id, onion: String::new(), inbound: true });
                let framed = Framed::new(stream, LinesCodec::new());
                tokio::spawn(peer_loop(framed, id, to_ui.clone(), line_rx, done_tx.clone()));
            }

            // ── Peer task finished ──────────────────────────────────────────────────────
            Some(id) = done_rx.recv() => {
                // peer_loop already emitted PeerDisconnected / PeerError; just prune the map.
                peers.remove(&id);
            }
        }
    }

    Ok(())
}

/// Dial an outbound peer through Tor's SOCKS5 proxy, then run its [`peer_loop`]. Runs in its
/// own task so a slow circuit (or an unreachable onion) never stalls the manager.
fn spawn_dialer(
    id: PeerId,
    raw_onion: String,
    line_rx: mpsc::UnboundedReceiver<String>,
    to_ui: mpsc::UnboundedSender<NetEvent>,
    done_tx: mpsc::UnboundedSender<PeerId>,
) {
    tokio::spawn(async move {
        let service_id = tor::normalize_onion(&raw_onion);
        let target = format!("{service_id}.onion:{ONION_VIRTUAL_PORT}");
        let _ = to_ui.send(NetEvent::Status(format!("dialing {target} via Tor…")));

        // Hand the "host:port" string straight to SOCKS5 so *Tor* resolves the .onion — never
        // resolve it locally.
        match Socks5Stream::connect(SOCKS_ADDR, target.as_str()).await {
            Ok(stream) => {
                let _ = to_ui.send(NetEvent::PeerConnected {
                    peer: id,
                    onion: format!("{service_id}.onion"),
                    inbound: false,
                });
                let framed = Framed::new(stream, LinesCodec::new());
                peer_loop(framed, id, to_ui, line_rx, done_tx.clone()).await;
            }
            Err(e) => {
                let _ = to_ui.send(NetEvent::PeerError {
                    peer: id,
                    err: format!("could not connect to {service_id}.onion: {e}"),
                });
                let _ = done_tx.send(id);
            }
        }
    });
}

/// Pump one peer connection: forward inbound JSON lines to the UI as [`NetEvent::Message`], and
/// write outgoing lines (handed over `line_rx` by the manager) to the socket. Generic over the
/// underlying stream so it serves both inbound `TcpStream`s and outbound `Socks5Stream`s.
async fn peer_loop<S>(
    framed: Framed<S, LinesCodec>,
    id: PeerId,
    to_ui: mpsc::UnboundedSender<NetEvent>,
    mut line_rx: mpsc::UnboundedReceiver<String>,
    done_tx: mpsc::UnboundedSender<PeerId>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = framed.split();

    loop {
        tokio::select! {
            incoming = stream.next() => match incoming {
                Some(Ok(line)) => match WireMsg::from_line(&line) {
                    Ok(msg) => { let _ = to_ui.send(NetEvent::Message { peer: id, msg }); }
                    Err(e) => { let _ = to_ui.send(NetEvent::Status(format!("dropped malformed message: {e}"))); }
                },
                Some(Err(e)) => {
                    let _ = to_ui.send(NetEvent::PeerError { peer: id, err: e.to_string() });
                    break;
                }
                None => {
                    let _ = to_ui.send(NetEvent::PeerDisconnected { peer: id });
                    break;
                }
            },
            outgoing = line_rx.recv() => match outgoing {
                Some(line) => {
                    if let Err(e) = sink.send(line).await {
                        let _ = to_ui.send(NetEvent::PeerError { peer: id, err: e.to_string() });
                        break;
                    }
                }
                // Manager dropped our line channel (explicit disconnect / shutdown). Exit quietly.
                None => break,
            }
        }
    }

    let _ = done_tx.send(id);
}
