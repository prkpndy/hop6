//! Application state and *pure* logic. This module performs **no I/O** — it only mutates
//! in-memory state in response to key events ([`App::on_key`]) and network events
//! ([`App::apply_net_event`]), and produces [`AppAction`]s for the main loop to act on. That
//! separation is what lets the network layer and the renderer stay completely decoupled.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::message::{NetEvent, PeerId, UiCommand};

/// What the main loop should do after handling a key.
pub enum AppAction {
    /// User wants to quit; the loop should shut the network down and restore the terminal.
    Quit,
    /// A command to forward to the network manager.
    Command(UiCommand),
}

/// One rendered line in a conversation.
pub struct ChatLine {
    pub from_me: bool,
    /// Display name of the sender (a shortened onion).
    pub who: String,
    pub body: String,
}

/// A peer conversation in the roster.
pub struct Peer {
    /// The peer's `.onion` (empty until learned, e.g. for inbound connections).
    pub onion: String,
    pub inbound: bool,
    pub connected: bool,
    pub log: Vec<ChatLine>,
}

impl Peer {
    /// Roster label: the short onion, or a placeholder while the identity is still unknown.
    fn label(&self) -> String {
        if self.onion.is_empty() {
            "(incoming)".to_string()
        } else {
            short_onion(&self.onion)
        }
    }
}

/// Whole-application state.
pub struct App {
    /// Our own published `.onion`, once Tor reports it.
    pub own_onion: Option<String>,
    /// Latest status-bar text.
    pub status: String,
    /// Peer conversations, keyed by their local [`PeerId`].
    pub peers: HashMap<PeerId, Peer>,
    /// Stable display order for the roster (system pane is index "None").
    pub order: Vec<PeerId>,
    /// Currently focused conversation; `None` selects the system log.
    pub active: Option<PeerId>,
    /// System / status scrollback (shown when no peer is selected).
    pub system: Vec<String>,
    /// Current input-line buffer.
    pub input: String,
    /// Lines scrolled up from the bottom of the active conversation (0 = pinned to newest).
    pub scroll: usize,
}

impl App {
    pub fn new() -> Self {
        let mut app = App {
            own_onion: None,
            status: "starting — publishing onion service…".to_string(),
            peers: HashMap::new(),
            order: Vec::new(),
            active: None,
            system: Vec::new(),
            input: String::new(),
            scroll: 0,
        };
        app.sys("Welcome to hop6. Type /help for commands.");
        app
    }

    fn sys(&mut self, line: impl Into<String>) {
        self.system.push(line.into());
    }

    /// Short display label for our own identity.
    fn me(&self) -> String {
        self.own_onion.as_deref().map(short_onion).unwrap_or_else(|| "me".to_string())
    }

    // ── Input handling ──────────────────────────────────────────────────────────────────

    /// Handle a key press. Returns an action for the main loop, if any.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        // Ctrl+C quits from anywhere.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(AppAction::Quit);
        }
        match key.code {
            KeyCode::Esc => Some(AppAction::Quit),
            KeyCode::Enter => self.submit_input(),
            KeyCode::Backspace => {
                self.input.pop();
                None
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                None
            }
            // Cycle the focused conversation.
            KeyCode::Tab | KeyCode::Down => {
                self.cycle_active(1);
                None
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.cycle_active(-1);
                None
            }
            // Scroll the active conversation.
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(5);
                None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(5);
                None
            }
            _ => None,
        }
    }

    /// Process the current input buffer on Enter.
    fn submit_input(&mut self) -> Option<AppAction> {
        let raw = std::mem::take(&mut self.input);
        let line = raw.trim();
        if line.is_empty() {
            return None;
        }
        if let Some(rest) = line.strip_prefix('/') {
            self.handle_command(rest)
        } else {
            self.send_to_active(line.to_string())
        }
    }

    fn handle_command(&mut self, rest: &str) -> Option<AppAction> {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();
        match cmd {
            "connect" | "c" => {
                if arg.is_empty() {
                    self.sys("usage: /connect <onion-address>");
                    None
                } else {
                    self.sys(format!("connecting to {arg}…"));
                    Some(AppAction::Command(UiCommand::Connect { onion: arg.to_string() }))
                }
            }
            "disconnect" | "d" => match self.active {
                Some(peer) => Some(AppAction::Command(UiCommand::Disconnect { peer })),
                None => {
                    self.sys("no active conversation to disconnect");
                    None
                }
            },
            "peers" => {
                self.list_peers();
                None
            }
            "quit" | "q" => Some(AppAction::Quit),
            "help" | "h" => {
                self.print_help();
                None
            }
            other => {
                self.sys(format!("unknown command: /{other} (try /help)"));
                None
            }
        }
    }

    fn send_to_active(&mut self, body: String) -> Option<AppAction> {
        match self.active {
            Some(peer) => {
                let me = self.me();
                if let Some(p) = self.peers.get_mut(&peer) {
                    if !p.connected {
                        self.sys("that peer is disconnected");
                        return None;
                    }
                    p.log.push(ChatLine { from_me: true, who: me, body: body.clone() });
                    self.scroll = 0; // jump to newest
                    return Some(AppAction::Command(UiCommand::Send { peer, body }));
                }
                None
            }
            None => {
                self.sys("no active conversation — use /connect <onion> first");
                None
            }
        }
    }

    fn list_peers(&mut self) {
        if self.order.is_empty() {
            self.sys("no peers yet");
            return;
        }
        let lines: Vec<String> = self
            .order
            .iter()
            .filter_map(|id| self.peers.get(id).map(|p| {
                let state = if p.connected { "online" } else { "offline" };
                let dir = if p.inbound { "in" } else { "out" };
                format!("  [{id}] {} ({dir}, {state})", p.label())
            }))
            .collect();
        self.sys("peers:");
        for l in lines {
            self.sys(l);
        }
    }

    fn print_help(&mut self) {
        for l in [
            "commands:",
            "  /connect <onion>   dial a peer (alias /c)",
            "  /disconnect        drop the active peer (alias /d)",
            "  /peers             list known peers",
            "  /quit              exit (alias /q; also Esc or Ctrl+C)",
            "  /help              this help (alias /h)",
            "keys: Tab/↑/↓ switch conversation · PgUp/PgDn scroll · Enter send",
        ] {
            self.sys(l);
        }
    }

    /// Move the focused conversation by `delta` through the order list, with `None` (system
    /// log) sitting just before the first peer.
    fn cycle_active(&mut self, delta: i32) {
        self.scroll = 0;
        if self.order.is_empty() {
            self.active = None;
            return;
        }
        // Build a virtual list: [None, peer0, peer1, ...].
        let cur = match self.active {
            None => 0,
            Some(id) => self.order.iter().position(|&p| p == id).map(|i| i + 1).unwrap_or(0),
        };
        let len = self.order.len() as i32 + 1;
        let next = (cur as i32 + delta).rem_euclid(len);
        self.active = if next == 0 {
            None
        } else {
            Some(self.order[(next - 1) as usize])
        };
    }

    // ── Network event handling ────────────────────────────────────────────────────────────

    pub fn apply_net_event(&mut self, ev: NetEvent) {
        match ev {
            NetEvent::OwnOnionReady { onion } => {
                self.sys(format!("your identity: {onion}"));
                self.status = format!("online · {}", short_onion(&onion));
                self.own_onion = Some(onion);
            }
            NetEvent::PeerConnected { peer, onion, inbound } => {
                if !self.peers.contains_key(&peer) {
                    self.order_push(peer);
                    self.peers.insert(
                        peer,
                        Peer { onion: String::new(), inbound, connected: false, log: Vec::new() },
                    );
                }
                let entry = self.peers.get_mut(&peer).expect("just inserted");
                entry.connected = true;
                entry.inbound = inbound;
                if !onion.is_empty() {
                    entry.onion = onion.clone();
                }
                let label = self.peers.get(&peer).map(|p| p.label()).unwrap_or_default();
                let dir = if inbound { "incoming" } else { "outgoing" };
                self.sys(format!("[{peer}] connected ({dir}): {label}"));
                if self.active.is_none() {
                    self.active = Some(peer);
                }
            }
            NetEvent::PeerDisconnected { peer } => {
                if let Some(p) = self.peers.get_mut(&peer) {
                    p.connected = false;
                    let label = p.label();
                    self.sys(format!("[{peer}] disconnected: {label}"));
                }
            }
            NetEvent::PeerError { peer, err } => {
                if let Some(p) = self.peers.get_mut(&peer) {
                    p.connected = false;
                }
                self.sys(format!("[{peer}] error: {err}"));
            }
            NetEvent::Message { peer, msg } => {
                // Learn an inbound peer's identity from its first message.
                if let Some(p) = self.peers.get_mut(&peer) {
                    if p.onion.is_empty() && !msg.from_onion.is_empty() {
                        p.onion = msg.from_onion.clone();
                    }
                    let who = short_onion(&msg.from_onion);
                    p.log.push(ChatLine { from_me: false, who, body: msg.body });
                    if self.active == Some(peer) {
                        self.scroll = 0;
                    }
                } else {
                    self.sys(format!("[{peer}] message from unknown peer dropped"));
                }
            }
            NetEvent::Status(s) => {
                self.status = s.clone();
                self.sys(s);
            }
        }
    }

    fn order_push(&mut self, peer: PeerId) {
        if !self.order.contains(&peer) {
            self.order.push(peer);
        }
    }

    /// No-op hook for periodic ticks (kept for future animations / timeouts).
    pub fn on_tick(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::WireMsg;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn type_line(app: &mut App, text: &str) -> Option<AppAction> {
        for c in text.chars() {
            app.on_key(key(c));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    }

    #[test]
    fn slash_connect_emits_connect_command() {
        let mut app = App::new();
        let action = type_line(&mut app, "/connect abc.onion");
        match action {
            Some(AppAction::Command(UiCommand::Connect { onion })) => assert_eq!(onion, "abc.onion"),
            _ => panic!("expected a Connect command"),
        }
    }

    #[test]
    fn esc_and_ctrl_c_quit() {
        let mut app = App::new();
        assert!(matches!(
            app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(AppAction::Quit)
        ));
        assert!(matches!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(AppAction::Quit)
        ));
    }

    #[test]
    fn typing_without_active_peer_does_not_send() {
        let mut app = App::new();
        let action = type_line(&mut app, "hello");
        assert!(action.is_none(), "no active peer → no Send");
    }

    #[test]
    fn message_to_connected_peer_round_trips_through_state() {
        let mut app = App::new();
        // A peer connects (outbound) and becomes active automatically.
        app.apply_net_event(NetEvent::PeerConnected {
            peer: 1,
            onion: "abc.onion".into(),
            inbound: false,
        });
        assert_eq!(app.active, Some(1));

        // Sending appends our own line and emits a Send command.
        let action = type_line(&mut app, "hi there");
        match action {
            Some(AppAction::Command(UiCommand::Send { peer, body })) => {
                assert_eq!(peer, 1);
                assert_eq!(body, "hi there");
            }
            _ => panic!("expected a Send command"),
        }
        assert_eq!(app.peers[&1].log.len(), 1);
        assert!(app.peers[&1].log[0].from_me);

        // An inbound message appends to the same conversation and learns identity.
        app.apply_net_event(NetEvent::Message {
            peer: 1,
            msg: WireMsg::new("abc.onion", "hey back", 1),
        });
        assert_eq!(app.peers[&1].log.len(), 2);
        assert!(!app.peers[&1].log[1].from_me);
    }

    #[test]
    fn inbound_peer_learns_identity_from_first_message() {
        let mut app = App::new();
        app.apply_net_event(NetEvent::PeerConnected {
            peer: 2,
            onion: String::new(), // unknown for inbound
            inbound: true,
        });
        assert_eq!(app.peers[&2].onion, "");
        app.apply_net_event(NetEvent::Message {
            peer: 2,
            msg: WireMsg::new("xyz.onion", "hello", 1),
        });
        assert_eq!(app.peers[&2].onion, "xyz.onion");
    }
}

/// Shorten a `.onion` to `abcd1234…` for compact display.
pub fn short_onion(onion: &str) -> String {
    let id = onion.strip_suffix(".onion").unwrap_or(onion);
    if id.len() > 8 {
        format!("{}…", &id[..8])
    } else {
        id.to_string()
    }
}
