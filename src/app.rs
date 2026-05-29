//! Application state and *pure* logic. This module performs **no I/O** — it only mutates
//! in-memory state in response to key events ([`App::on_key`]) and network events
//! ([`App::apply_net_event`]), and produces [`AppAction`]s for the main loop to act on. That
//! separation is what lets the network layer and the renderer stay completely decoupled.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::contacts::Book;
use crate::message::{NetEvent, PeerId, UiCommand};
use crate::tor::normalize_onion;

/// What the main loop should do after handling a key.
pub enum AppAction {
    /// User wants to quit; the loop should shut the network down and restore the terminal.
    Quit,
    /// A command to forward to the network manager.
    Command(UiCommand),
    /// The contacts book changed; the main loop should persist it to disk.
    PersistContacts,
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
    /// Saved `name -> bare onion id` address book, loaded at startup.
    pub contacts: Book,
}

impl App {
    pub fn new(contacts: Book) -> Self {
        let mut app = App {
            own_onion: None,
            status: "starting — publishing onion service…".to_string(),
            peers: HashMap::new(),
            order: Vec::new(),
            active: None,
            system: Vec::new(),
            input: String::new(),
            scroll: 0,
            contacts,
        };
        app.sys("Welcome to hop6. Type /help for commands.");
        app
    }

    fn sys(&mut self, line: impl Into<String>) {
        self.system.push(line.into());
    }

    /// Label for our own messages in the chat log. Always "you" — you don't need to recognize
    /// your own onion address to know which lines are yours.
    fn me(&self) -> String {
        "you".to_string()
    }

    /// Human-friendly label for an onion address: the saved contact name if we know one, else a
    /// shortened onion, or `(incoming)` while a peer's identity is still unknown.
    pub fn display_name(&self, onion: &str) -> String {
        if onion.is_empty() {
            return "(incoming)".to_string();
        }
        let id = normalize_onion(onion);
        match self.contacts.iter().find(|(_, v)| **v == id) {
            Some((name, _)) => name.clone(),
            None => short_onion(onion),
        }
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
                    self.sys("usage: /connect <name|onion>");
                    None
                } else {
                    // Resolve a saved contact name; otherwise treat the arg as a raw onion.
                    let onion = self.contacts.get(arg).cloned().unwrap_or_else(|| arg.to_string());
                    self.sys(format!("connecting to {}…", self.display_name(&onion)));
                    Some(AppAction::Command(UiCommand::Connect { onion }))
                }
            }
            "disconnect" | "d" => match self.active {
                Some(peer) => Some(AppAction::Command(UiCommand::Disconnect { peer })),
                None => {
                    self.sys("no active conversation to disconnect");
                    None
                }
            },
            "add" => self.add_contact(arg),
            "remove" | "rm" => self.remove_contact(arg),
            "contacts" => {
                self.list_contacts();
                None
            }
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

    /// `/add <name> <onion>` — save a contact and request persistence.
    fn add_contact(&mut self, arg: &str) -> Option<AppAction> {
        let mut parts = arg.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("").trim();
        let onion = parts.next().unwrap_or("").trim();
        if name.is_empty() || onion.is_empty() {
            self.sys("usage: /add <name> <onion>");
            return None;
        }
        let id = normalize_onion(onion);
        self.contacts.insert(name.to_string(), id);
        self.sys(format!("saved contact '{name}'"));
        Some(AppAction::PersistContacts)
    }

    /// `/remove <name>` — forget a contact and request persistence.
    fn remove_contact(&mut self, name: &str) -> Option<AppAction> {
        if name.is_empty() {
            self.sys("usage: /remove <name>");
            None
        } else if self.contacts.remove(name).is_some() {
            self.sys(format!("removed contact '{name}'"));
            Some(AppAction::PersistContacts)
        } else {
            self.sys(format!("no contact named '{name}'"));
            None
        }
    }

    fn list_contacts(&mut self) {
        if self.contacts.is_empty() {
            self.sys("no saved contacts — add one with /add <name> <onion>");
            return;
        }
        let mut entries: Vec<(String, String)> =
            self.contacts.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        self.sys("contacts:");
        for (name, onion) in entries {
            self.sys(format!("  {name} → {}", short_onion(&onion)));
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
                    p.log.push(ChatLine {
                        from_me: true,
                        who: me,
                        body: body.clone(),
                    });
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
        // Snapshot first so we don't borrow self.peers while calling display_name(&self).
        let snap: Vec<(PeerId, String, bool, bool)> = self
            .order
            .iter()
            .filter_map(|id| {
                self.peers
                    .get(id)
                    .map(|p| (*id, p.onion.clone(), p.connected, p.inbound))
            })
            .collect();
        self.sys("peers:");
        for (id, onion, connected, inbound) in snap {
            let state = if connected { "online" } else { "offline" };
            let dir = if inbound { "in" } else { "out" };
            let label = self.display_name(&onion);
            self.sys(format!("  [{id}] {label} ({dir}, {state})"));
        }
    }

    fn print_help(&mut self) {
        for l in [
            "commands:",
            "  /connect <name|onion>  dial a saved contact or a raw onion (alias /c)",
            "  /add <name> <onion>    save a contact",
            "  /remove <name>         forget a contact (alias /rm)",
            "  /contacts              list saved contacts",
            "  /disconnect            drop the active peer (alias /d)",
            "  /peers                 list connected peers",
            "  /quit                  exit (alias /q; also Esc or Ctrl+C)",
            "  /help                  this help (alias /h)",
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
            Some(id) => self
                .order
                .iter()
                .position(|&p| p == id)
                .map(|i| i + 1)
                .unwrap_or(0),
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
            NetEvent::PeerConnected {
                peer,
                onion,
                inbound,
            } => {
                if !self.peers.contains_key(&peer) {
                    self.order_push(peer);
                    self.peers.insert(
                        peer,
                        Peer {
                            onion: String::new(),
                            inbound,
                            connected: false,
                            log: Vec::new(),
                        },
                    );
                }
                let entry = self.peers.get_mut(&peer).expect("just inserted");
                entry.connected = true;
                entry.inbound = inbound;
                if !onion.is_empty() {
                    entry.onion = onion.clone();
                }
                let label = self.display_name(&onion);
                let dir = if inbound { "incoming" } else { "outgoing" };
                self.sys(format!("[{peer}] connected ({dir}): {label}"));
                if self.active.is_none() {
                    self.active = Some(peer);
                }
                // If this connection's identity is already known (outbound), fold any stale
                // entry for the same onion into it. (Inbound onion is unknown until the first
                // message; that merge happens in the Message arm below.)
                self.merge_duplicates(peer);
            }
            NetEvent::PeerDisconnected { peer } => {
                let onion = self.peers.get_mut(&peer).map(|p| {
                    p.connected = false;
                    p.onion.clone()
                });
                if let Some(onion) = onion {
                    let label = self.display_name(&onion);
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
                // Resolve the sender's display name (contact name if known) before borrowing peers.
                let who = self.display_name(&msg.from_onion);
                // Learn an inbound peer's identity from its first message.
                let mut learned = false;
                if let Some(p) = self.peers.get_mut(&peer) {
                    if p.onion.is_empty() && !msg.from_onion.is_empty() {
                        p.onion = msg.from_onion.clone();
                        learned = true;
                    }
                    p.log.push(ChatLine {
                        from_me: false,
                        who,
                        body: msg.body,
                    });
                    if self.active == Some(peer) {
                        self.scroll = 0;
                    }
                } else {
                    self.sys(format!("[{peer}] message from unknown peer dropped"));
                }
                // Now that we know who this inbound peer is, reattach the history of any earlier
                // (now-disconnected) conversation with the same onion — e.g. after a reconnect.
                if learned {
                    self.merge_duplicates(peer);
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

    /// Fold any stale (disconnected) conversations that share `live`'s onion into `live`,
    /// preserving chronological order, then drop them. This consolidates the duplicate entries
    /// that otherwise appear on the *receiving* side after a reconnect: Tor hides the caller, so
    /// each reconnect arrives as a fresh `PeerId`, and we can only tell it's the same identity
    /// once a message reveals the onion. The live connection is kept (the network routes by its
    /// id); the older entries' history is prepended to it.
    fn merge_duplicates(&mut self, live: PeerId) {
        let onion = match self.peers.get(&live) {
            Some(p) if !p.onion.is_empty() => normalize_onion(&p.onion),
            _ => return,
        };
        let dups: Vec<PeerId> = self
            .order
            .iter()
            .copied()
            .filter(|&id| id != live)
            .filter(|id| {
                self.peers.get(id).is_some_and(|p| {
                    !p.connected && !p.onion.is_empty() && normalize_onion(&p.onion) == onion
                })
            })
            .collect();
        if dups.is_empty() {
            return;
        }

        // Collect the stale logs in roster order (oldest sessions first).
        let mut history: Vec<ChatLine> = Vec::new();
        for id in &dups {
            if let Some(p) = self.peers.remove(id) {
                history.extend(p.log);
            }
            self.order.retain(|o| o != id);
            if self.active == Some(*id) {
                self.active = Some(live);
            }
        }

        // Prepend that history to the live conversation.
        if let Some(p) = self.peers.get_mut(&live) {
            history.append(&mut p.log);
            p.log = history;
        }

        let label = self.display_name(&onion);
        self.sys(format!(
            "reattached earlier conversation with {label} into [{live}]"
        ));
    }

    /// No-op hook for periodic ticks (kept for future animations / timeouts).
    pub fn on_tick(&mut self) {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::WireMsg;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn app() -> App {
        App::new(Book::new())
    }

    fn type_line(app: &mut App, text: &str) -> Option<AppAction> {
        for c in text.chars() {
            app.on_key(key(c));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    }

    #[test]
    fn slash_connect_emits_connect_command() {
        let mut app = app();
        let action = type_line(&mut app, "/connect abc.onion");
        match action {
            Some(AppAction::Command(UiCommand::Connect { onion })) => {
                assert_eq!(onion, "abc.onion")
            }
            _ => panic!("expected a Connect command"),
        }
    }

    #[test]
    fn esc_and_ctrl_c_quit() {
        let mut app = app();
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
        let mut app = app();
        let action = type_line(&mut app, "hello");
        assert!(action.is_none(), "no active peer → no Send");
    }

    #[test]
    fn message_to_connected_peer_round_trips_through_state() {
        let mut app = app();
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
        assert_eq!(app.peers[&1].log[0].who, "you");

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
        let mut app = app();
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

    #[test]
    fn add_then_connect_resolves_name_and_labels_peer() {
        let mut app = app();

        // /add saves the contact (normalized, no .onion suffix) and asks to persist.
        let action = type_line(&mut app, "/add alice abc.onion");
        assert!(matches!(action, Some(AppAction::PersistContacts)));
        assert_eq!(app.contacts.get("alice").map(String::as_str), Some("abc"));

        // /connect by name resolves to the saved onion.
        let action = type_line(&mut app, "/connect alice");
        match action {
            Some(AppAction::Command(UiCommand::Connect { onion })) => assert_eq!(onion, "abc"),
            _ => panic!("expected a Connect command"),
        }

        // A peer at that onion is labeled with the contact name, in any onion form.
        assert_eq!(app.display_name("abc.onion"), "alice");
        assert_eq!(app.display_name("abc"), "alice");
        assert_eq!(app.display_name(""), "(incoming)");
    }

    #[test]
    fn inbound_reconnect_merges_into_one_conversation() {
        let mut app = app();

        // First inbound session from Alice: connects, sends a message (revealing identity).
        app.apply_net_event(NetEvent::PeerConnected { peer: 1, onion: String::new(), inbound: true });
        app.apply_net_event(NetEvent::Message {
            peer: 1,
            msg: WireMsg::new("alice.onion", "before reset", 1),
        });
        // Connection drops (Tor restart).
        app.apply_net_event(NetEvent::PeerDisconnected { peer: 1 });
        assert!(!app.peers[&1].connected);

        // Alice auto-reconnects: a brand-new inbound PeerId, identity unknown until her message.
        app.apply_net_event(NetEvent::PeerConnected { peer: 2, onion: String::new(), inbound: true });
        app.apply_net_event(NetEvent::Message {
            peer: 2,
            msg: WireMsg::new("alice.onion", "after reset", 2),
        });

        // The stale entry [1] is folded into the live entry [2]: one conversation, full history.
        assert!(!app.peers.contains_key(&1), "stale duplicate should be removed");
        assert_eq!(app.order, vec![2]);
        let bodies: Vec<&str> = app.peers[&2].log.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, vec!["before reset", "after reset"]);
        assert_eq!(app.active, Some(2), "active follows the surviving entry");
    }

    #[test]
    fn remove_contact_persists_and_forgets() {
        let mut app = app();
        type_line(&mut app, "/add bob def.onion");
        let action = type_line(&mut app, "/remove bob");
        assert!(matches!(action, Some(AppAction::PersistContacts)));
        assert!(!app.contacts.contains_key("bob"));

        // Removing a missing contact does not request persistence.
        let action = type_line(&mut app, "/remove nobody");
        assert!(action.is_none());
    }
}
