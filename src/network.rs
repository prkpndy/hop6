//! The network layer: a single **connection-manager** task plus one **peer task** per
//! conversation. None of this code touches the TUI — it speaks only in [`NetEvent`]s (out) and
//! [`UiCommand`]s (in), so the render loop can never be blocked by a slow Tor circuit.
//!
//! ## Resilience
//!
//! Each live connection runs a **heartbeat** ([`Frame::Ping`]/[`Frame::Pong`]): if no frame
//! arrives within [`LIVENESS_TIMEOUT`], the link is declared dead. This is what lets us notice a
//! connection that silently died while the laptop slept (no I/O ⇒ no EOF otherwise).
//!
//! **Outbound** peers (the ones we dialed, whose onion we know) **auto-reconnect** with
//! exponential backoff after any non-deliberate drop, so a conversation heals itself once Tor
//! recovers / the peer comes back. **Inbound** peers can't be redialed by us (Tor hides the
//! caller), so they simply end; the remote's auto-reconnect re-establishes them.
//!
//! ```text
//!                          ┌───────────────── manager task ────────────────────┐
//!   TcpListener (inbound) ─┤ accept → PeerId → spawn inbound peer task         │
//!   UiCommand::Connect ────┤ PeerId → spawn outbound peer task (dial+retry)    │
//!   UiCommand::Send ───────┤ build Frame::Msg → route to that peer's line_tx   │
//!   UiCommand::Disconnect ─┤ drop the peer's line_tx (stops its task for good) │
//!   peer task finished ────┤ remove from roster map                            │
//!                          └───────────────────────────────────────────────────┘
//! ```

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{self, Duration, Instant};
use tokio_socks::tcp::Socks5Stream;
use tokio_util::codec::{Framed, LinesCodec};

use crate::message::{NetEvent, PeerId, UiCommand};
use crate::tor;
use crate::wire::{Frame, WireMsg};

/// Tor's local SOCKS5 proxy (`SocksPort 9050` in torrc).
pub const SOCKS_ADDR: &str = "127.0.0.1:9050";
/// Virtual port our onion service exposes (and the port we dial peers on).
pub const ONION_VIRTUAL_PORT: u16 = 80;

/// How often each connection sends a heartbeat ping.
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// Declare a connection dead if nothing is received within this window (~3 missed pings).
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(45);
/// First outbound reconnect delay; doubles up to [`RECONNECT_BACKOFF_MAX`].
const RECONNECT_BACKOFF_START: Duration = Duration::from_secs(2);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Current Unix time in whole seconds (best-effort display hint only).
fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ping_line() -> String {
    Frame::Ping.to_line().expect("serialize ping")
}
fn pong_line() -> String {
    Frame::Pong.to_line().expect("serialize pong")
}

/// Why a single connection ended.
enum ConnEnd {
    /// The manager closed our line channel — a deliberate `/disconnect` or shutdown. Stop.
    LocalClosed,
    /// The connection broke (EOF, I/O error, or heartbeat timeout). Outbound peers reconnect.
    Lost(String),
}

/// Entry point for the network layer. Binds the inbound listener, then runs the manager loop
/// until the UI sends [`UiCommand::Shutdown`] (or its command channel closes).
///
/// `own_onion` is stamped into every outgoing [`WireMsg`] so peers can label us — important for
/// inbound connections, where Tor doesn't reveal who connected.
pub async fn run(
    local_port: u16,
    own_onion: String,
    to_ui: mpsc::UnboundedSender<NetEvent>,
    mut from_ui: mpsc::UnboundedReceiver<UiCommand>,
) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", local_port)).await?;
    let _ = to_ui.send(NetEvent::Status(format!(
        "listening for inbound peers on 127.0.0.1:{local_port}"
    )));

    // Inbound accepts arrive here from a dedicated accept task so the manager never blocks.
    let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel::<TcpStream>();
    tokio::spawn(async move {
        while let Ok((stream, _addr)) = listener.accept().await {
            if inbound_tx.send(stream).is_err() {
                break; // manager gone
            }
        }
    });

    // Peer tasks report their own (final) termination here so the manager can prune the map.
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<PeerId>();

    // PeerId -> sender of already-serialized JSON lines destined for that peer.
    let mut peers: HashMap<PeerId, mpsc::UnboundedSender<String>> = HashMap::new();
    let mut next_id: PeerId = 1;

    loop {
        tokio::select! {
            cmd = from_ui.recv() => match cmd {
                Some(UiCommand::Connect { onion }) => {
                    let id = next_id; next_id += 1;
                    let (line_tx, line_rx) = mpsc::unbounded_channel::<String>();
                    peers.insert(id, line_tx);
                    spawn_outbound(id, onion, line_rx, to_ui.clone(), done_tx.clone());
                }
                Some(UiCommand::Send { peer, body }) => {
                    if let Some(tx) = peers.get(&peer) {
                        match Frame::Msg(WireMsg::new(own_onion.clone(), body, now_ts())).to_line() {
                            Ok(line) => { let _ = tx.send(line); }
                            Err(e) => { let _ = to_ui.send(NetEvent::Status(format!("encode error: {e}"))); }
                        }
                    }
                }
                Some(UiCommand::Disconnect { peer }) => {
                    // Dropping the sender closes the peer task's line channel → it stops for good.
                    if peers.remove(&peer).is_some() {
                        let _ = to_ui.send(NetEvent::PeerDisconnected { peer });
                    }
                }
                Some(UiCommand::Shutdown) | None => break,
            },

            Some(stream) = inbound_rx.recv() => {
                let id = next_id; next_id += 1;
                let (line_tx, line_rx) = mpsc::unbounded_channel::<String>();
                peers.insert(id, line_tx);
                // onion is unknown until the remote's first message (Tor hides the caller).
                let _ = to_ui.send(NetEvent::PeerConnected { peer: id, onion: String::new(), inbound: true });
                spawn_inbound(stream, id, line_rx, to_ui.clone(), done_tx.clone());
            }

            Some(id) = done_rx.recv() => {
                peers.remove(&id);
            }
        }
    }

    Ok(())
}

/// Run an accepted inbound connection once. Inbound peers are not redialed (we can't reach
/// them); when the link dies the task simply ends and the remote's reconnect re-establishes it.
fn spawn_inbound(
    stream: TcpStream,
    id: PeerId,
    mut line_rx: mpsc::UnboundedReceiver<String>,
    to_ui: mpsc::UnboundedSender<NetEvent>,
    done_tx: mpsc::UnboundedSender<PeerId>,
) {
    tokio::spawn(async move {
        let framed = Framed::new(stream, LinesCodec::new());
        if let ConnEnd::Lost(e) = run_connection(framed, id, &to_ui, &mut line_rx).await {
            let _ = to_ui.send(NetEvent::PeerError { peer: id, err: e });
        }
        let _ = done_tx.send(id);
    });
}

/// Dial an outbound peer through Tor's SOCKS5 proxy and run it, **reconnecting with backoff**
/// after any non-deliberate drop. Loops until the manager closes our line channel (deliberate
/// `/disconnect` or shutdown). Runs in its own task so slow circuits never stall the manager.
fn spawn_outbound(
    id: PeerId,
    raw_onion: String,
    mut line_rx: mpsc::UnboundedReceiver<String>,
    to_ui: mpsc::UnboundedSender<NetEvent>,
    done_tx: mpsc::UnboundedSender<PeerId>,
) {
    tokio::spawn(async move {
        let service_id = tor::normalize_onion(&raw_onion);
        // "host:port" string → Tor resolves the .onion (never resolve locally).
        let target = format!("{service_id}.onion:{ONION_VIRTUAL_PORT}");
        let display = format!("{service_id}.onion");
        let mut backoff = RECONNECT_BACKOFF_START;

        loop {
            let _ = to_ui.send(NetEvent::Status(format!("dialing {target} via Tor…")));
            match Socks5Stream::connect(SOCKS_ADDR, target.as_str()).await {
                Ok(stream) => {
                    backoff = RECONNECT_BACKOFF_START; // reset after a successful connect
                    let _ = to_ui.send(NetEvent::PeerConnected {
                        peer: id,
                        onion: display.clone(),
                        inbound: false,
                    });
                    let framed = Framed::new(stream, LinesCodec::new());
                    match run_connection(framed, id, &to_ui, &mut line_rx).await {
                        ConnEnd::LocalClosed => break, // deliberate disconnect / shutdown
                        ConnEnd::Lost(e) => {
                            let _ = to_ui.send(NetEvent::PeerError {
                                peer: id,
                                err: format!("{e} — reconnecting…"),
                            });
                        }
                    }
                }
                Err(e) => {
                    let _ = to_ui.send(NetEvent::PeerError {
                        peer: id,
                        err: format!("could not connect to {display}: {e} — retrying in {}s", backoff.as_secs()),
                    });
                }
            }

            // Wait out the backoff, but stop immediately if the manager closed our channel.
            if wait_or_closed(&mut line_rx, backoff).await {
                break;
            }
            backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
        }

        let _ = done_tx.send(id);
    });
}

/// Sleep for `dur`, returning `true` if the line channel was closed during the wait (the manager
/// dropped our sender ⇒ deliberate disconnect). Any stray queued lines are dropped — the App
/// blocks sends to offline peers, so this is only a rare in-flight straggler.
async fn wait_or_closed(line_rx: &mut mpsc::UnboundedReceiver<String>, dur: Duration) -> bool {
    let sleep = time::sleep(dur);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return false,
            msg = line_rx.recv() => match msg {
                Some(_dropped) => continue,
                None => return true,
            }
        }
    }
}

/// Pump one connection: forward inbound chat frames to the UI, write outgoing lines, answer
/// pings, and enforce the heartbeat liveness deadline. Generic over the stream so it serves both
/// inbound `TcpStream`s and outbound `Socks5Stream`s.
async fn run_connection<S>(
    framed: Framed<S, LinesCodec>,
    id: PeerId,
    to_ui: &mpsc::UnboundedSender<NetEvent>,
    line_rx: &mut mpsc::UnboundedReceiver<String>,
) -> ConnEnd
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut sink, mut stream) = framed.split();
    let mut heartbeat = time::interval(PING_INTERVAL);
    heartbeat.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut last_seen = Instant::now();

    loop {
        tokio::select! {
            incoming = stream.next() => match incoming {
                Some(Ok(line)) => {
                    last_seen = Instant::now();
                    match Frame::from_line(&line) {
                        Ok(Frame::Msg(msg)) => { let _ = to_ui.send(NetEvent::Message { peer: id, msg }); }
                        Ok(Frame::Ping) => {
                            if sink.send(pong_line()).await.is_err() {
                                return ConnEnd::Lost("write failed".into());
                            }
                        }
                        Ok(Frame::Pong) => {}
                        Err(e) => { let _ = to_ui.send(NetEvent::Status(format!("dropped malformed frame: {e}"))); }
                    }
                }
                Some(Err(e)) => return ConnEnd::Lost(e.to_string()),
                None => return ConnEnd::Lost("connection closed by peer".into()),
            },

            outgoing = line_rx.recv() => match outgoing {
                Some(line) => {
                    if sink.send(line).await.is_err() {
                        return ConnEnd::Lost("write failed".into());
                    }
                }
                None => return ConnEnd::LocalClosed,
            },

            _ = heartbeat.tick() => {
                if last_seen.elapsed() > LIVENESS_TIMEOUT {
                    return ConnEnd::Lost("peer unresponsive (heartbeat timeout)".into());
                }
                if sink.send(ping_line()).await.is_err() {
                    return ConnEnd::Lost("write failed".into());
                }
            }
        }
    }
}
