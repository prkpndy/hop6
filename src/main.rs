//! hop6 — a serverless, peer-to-peer terminal messenger that speaks exclusively over Tor v3
//! onion services.
//!
//! # How the pieces fit together
//!
//! ```text
//!   ┌─────────────┐   NetEvent    ┌──────────────────┐
//!   │  network.rs │ ────────────▶ │  main UI loop    │ ──draw──▶ ui.rs
//!   │  (tokio     │               │  (this file)     │
//!   │   tasks)    │ ◀──UiCommand─ │  + app.rs state  │ ◀──keys── crossterm EventStream
//!   └─────────────┘   mpsc        └──────────────────┘
//!         ▲
//!         │ ADD_ONION / SOCKS5
//!   ┌─────┴───────┐
//!   │  local Tor  │
//!   └─────────────┘
//! ```
//!
//! `main` publishes our onion via [`tor::start_onion`], spawns the network manager, then runs
//! the terminal loop. The UI thread and the network tasks share nothing but the two mpsc
//! channels, so a slow Tor circuit can never freeze the interface — the [`tokio::select!`] in
//! [`run_ui`] simply keeps servicing keystrokes and redraws while network events trickle in.

mod app;
mod message;
mod network;
mod tor;
mod ui;
mod wire;

use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::{FutureExt, StreamExt};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use app::{App, AppAction};
use message::{NetEvent, UiCommand};

/// Default local port that Tor forwards inbound onion traffic to (overridable with `--port`).
const DEFAULT_LOCAL_PORT: u16 = 8080;

#[tokio::main]
async fn main() -> Result<()> {
    let local_port = parse_port_arg().unwrap_or(DEFAULT_LOCAL_PORT);

    // Channels bridging network ⇄ UI.
    let (net_tx, net_rx) = mpsc::unbounded_channel::<NetEvent>();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();

    // Publish our onion BEFORE entering the alternate screen, so any Tor error is printed
    // plainly to the normal terminal instead of being clobbered by the TUI. The key is loaded
    // from disk (or generated on first run) so the address is stable across restarts.
    let tor::Identity { onion, control, key_path, created } = tor::start_onion(local_port)
        .await
        .context("failed to publish onion service")?;

    // Keep the control connection alive for the whole program: dropping it tears down the
    // onion service (created with detach=false). Parking it in a task that owns it is the
    // simplest way to tie its lifetime to the process.
    tokio::spawn(async move {
        let _control = control;
        std::future::pending::<()>().await;
    });

    // Spawn the network manager (inbound listener + outbound dialer + per-peer tasks).
    {
        let net_tx = net_tx.clone();
        let onion = onion.clone();
        tokio::spawn(async move {
            if let Err(e) = network::run(local_port, onion, net_tx.clone(), cmd_rx).await {
                let _ = net_tx.send(NetEvent::Status(format!("network manager stopped: {e}")));
            }
        });
    }

    // Tell the UI our identity right away, plus where it's persisted.
    let _ = net_tx.send(NetEvent::OwnOnionReady { onion });
    let verb = if created { "created new" } else { "loaded" };
    let _ = net_tx.send(NetEvent::Status(format!(
        "identity {verb} from {}",
        key_path.display()
    )));

    // Enter the TUI. ratatui::init() switches to the alternate screen + raw mode; restore()
    // undoes it. We run the loop inside a closure so we always restore, even on error.
    let terminal = ratatui::init();
    let result = run_ui(terminal, App::new(), net_rx, cmd_tx.clone()).await;
    ratatui::restore();

    // Best-effort: ask the network manager to shut down.
    let _ = cmd_tx.send(UiCommand::Shutdown);

    result
}

/// The terminal event loop. Draws on every iteration and races three sources with
/// `tokio::select!`: terminal input, network events, and a render tick. None of the arms block
/// — network I/O lives entirely in the spawned tasks.
async fn run_ui(
    mut terminal: DefaultTerminal,
    mut app: App,
    mut net_rx: mpsc::UnboundedReceiver<NetEvent>,
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));

    loop {
        terminal.draw(|f| ui::render(f, &app))?;

        tokio::select! {
            // (a) Terminal input.
            maybe_event = events.next().fuse() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        match app.on_key(key) {
                            Some(AppAction::Quit) => break,
                            Some(AppAction::Command(cmd)) => { let _ = cmd_tx.send(cmd); }
                            None => {}
                        }
                    }
                    // Resize and other events: just redraw next iteration.
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => break, // stdin closed
                }
            }

            // (b) Network → UI events.
            Some(ev) = net_rx.recv() => {
                app.apply_net_event(ev);
            }

            // (c) Periodic tick (keeps the UI lively / future-proofs timeouts).
            _ = ticker.tick() => {
                app.on_tick();
            }
        }
    }

    Ok(())
}

/// Minimal `--port <N>` parser (no external arg-parsing dependency for a prototype).
fn parse_port_arg() -> Option<u16> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" | "-p" => return args.next().and_then(|v| v.parse().ok()),
            other => {
                if let Some(v) = other.strip_prefix("--port=") {
                    return v.parse().ok();
                }
            }
        }
    }
    None
}
