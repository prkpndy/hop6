# hop6

A serverless, peer-to-peer, terminal (TUI) messenger that communicates **exclusively over the
Tor network** using **v3 onion services**. There is no central server: each instance publishes
its own `.onion` address and connects directly to peers' onion addresses through Tor.

- **TUI:** [`ratatui`] + [`crossterm`]
- **Async runtime:** [`tokio`]
- **Inbound:** a v3 onion service published at runtime via Tor's **control port** (`ADD_ONION`)
  from a persisted key (stable address across restarts), forwarding to a local TCP listener
- **Outbound:** TCP dialed through Tor's **SOCKS5 proxy** so Tor resolves the `.onion`
- **Wire format:** newline-delimited JSON

```
┌─ status ─ you: <your .onion> · online ─────────────────────┐
├─ peers ───┬─ chat · [2] abc12345… ────────────────────────┤
│ ▶ *system │ me: hello over tor                              │
│   [2] ab… │ abc12345…: hi back                              │
├───────────┴─ input (Enter=send · /help) ───────────────────┤
│ > _                                                         │
└─────────────────────────────────────────────────────────────┘
```

## Prerequisites

1. **Rust** (edition 2021; stable toolchain).
2. **A running Tor daemon** on the same machine, with the SOCKS port, control port, and cookie
   authentication enabled.

### Install Tor

- macOS (Homebrew): `brew install tor`
- Debian/Ubuntu: `sudo apt install tor`
- Arch: `sudo pacman -S tor`

### Configure Tor

A ready-to-use config is included as [`torrc.sample`](./torrc.sample). It enables exactly what
hop6 needs:

```
SocksPort 9050
ControlPort 9051
CookieAuthentication 1
```

Run Tor with it directly (easiest for testing):

```sh
tor -f ./torrc.sample
```

Leave that running in its own terminal. (Alternatively, merge those three directives into your
system `torrc` and restart the `tor` service.)

> **Cookie permissions:** hop6 reads Tor's control auth cookie. If Tor runs under a different
> system user than you (common with the packaged service), add `CookieAuthFileGroupReadable 1`
> to the config and make sure your user is in Tor's group — otherwise startup fails with
> `failed to read Tor auth cookie`.

## Build & run

```sh
cargo build
cargo run            # listens on 127.0.0.1:8080 by default
```

On startup hop6 connects to the control port, publishes its onion service, and shows **your
`.onion` address** in the status bar (and in the `*system` log). Publishing can take a few
seconds. Share that address with whoever you want to chat with (out-of-band — e.g. Signal, in
person; the address is your identity).

### Persistent identity

Your `.onion` address is derived from a secret key that hop6 **persists across restarts**, so
your address stays the same every run.

- On first run hop6 generates the key and saves it to `~/.config/hop6/identity.key`
  (directory `0700`, file `0600` — treat it like an SSH private key). Later runs reload it.
- The `*system` log shows whether the identity was `created new` or `loaded`, and from where.
- Override the location with the `HOP6_IDENTITY` environment variable:
  ```sh
  HOP6_IDENTITY=./peer-a.key cargo run -- --port 8080
  HOP6_IDENTITY=./peer-b.key cargo run -- --port 8081   # distinct identity on the same host
  ```
- To rotate your identity, delete the key file (a new address is generated next run).
- A persistent onion is a **stable, linkable identity** — that's the point, but anyone you've
  shared it with can recognize you across sessions. Delete the key to become unlinkable again.

### Contacts (address book)

So you don't paste 56-character onion addresses every time, hop6 keeps a saved `name -> onion`
address book.

- Save a friend once: `/add alice <their-onion>`. Then dial them by name: `/connect alice`.
- `/connect` falls back to treating its argument as a raw onion if it isn't a known name, so
  both `/connect alice` and `/connect <onion>` work.
- The roster, chat title, and incoming messages show the **contact's name** instead of a
  shortened onion whenever the address matches a saved contact — so even an inbound connection
  from a known friend shows up as "alice" rather than `(incoming)`.
- `/contacts` lists what you've saved; `/remove alice` forgets a contact.
- Stored at `~/.config/hop6/contacts.json` (override with `$HOP6_CONTACTS`), written `0600` in
  a `0700` directory. This file is your **social graph** (who you talk to) — sensitive metadata,
  so keep it private and don't commit it.

### Running two instances on one machine (loopback test)

Each instance needs its own local listener port, so give the second one `--port`:

```sh
# terminal A
cargo run -- --port 8080

# terminal B
cargo run -- --port 8081
```

Both publish separate onion services. In **terminal B**, dial A's address:

```
/connect <A's-onion-address>
```

Once connected, both rosters show the peer; type a line and press Enter to chat. Messages route
A → Tor → B and back.

## Using the app

Type in the input box at the bottom.

- **Plain text** → sent to the currently focused conversation.
- **Slash commands:**
  | command | alias | effect |
  |---|---|---|
  | `/connect <name\|onion>` | `/c` | dial a saved contact by name, or a raw `.onion` address (with or without the suffix) |
  | `/add <name> <onion>` | | save a contact under a memorable name |
  | `/remove <name>` | `/rm` | forget a saved contact |
  | `/contacts` | | list saved contacts in the system log |
  | `/disconnect` | `/d` | drop the focused conversation |
  | `/peers` | | list connected peers in the system log |
  | `/quit` | `/q` | exit |
  | `/help` | `/h` | show help |

- **Keys:**
  | key | effect |
  |---|---|
  | `Enter` | send message / run command |
  | `Tab`, `↑`, `↓` | switch conversation (incl. the `*system` log) |
  | `PageUp` / `PageDown` | scroll the chat history |
  | `Esc` or `Ctrl+C` | quit (terminal is restored cleanly) |

The `*system` pane (selected when no peer is focused) shows status messages: your identity,
connection events, dial progress, and errors.

## How it works

```
  ┌─────────────┐   NetEvent    ┌──────────────────┐
  │  network.rs │ ────────────▶ │  main UI loop    │ ──draw──▶ ui.rs
  │  (tokio     │               │  (main.rs)       │
  │   tasks)    │ ◀──UiCommand─ │  + app.rs state  │ ◀──keys── crossterm EventStream
  └─────────────┘   mpsc        └──────────────────┘
        ▲
        │ ADD_ONION (control) / SOCKS5 (dial)
  ┌─────┴───────┐
  │  local Tor  │
  └─────────────┘
```

| file | responsibility |
|---|---|
| `src/main.rs` | bootstrap, the `tokio::select!` terminal loop |
| `src/app.rs` | application state + pure logic (key handling, command parsing); no I/O |
| `src/ui.rs` | read-only `ratatui` rendering of the three panes |
| `src/tor.rs` | control-port connect, cookie auth, `ADD_ONION` v3 |
| `src/network.rs` | inbound acceptor, SOCKS5 dialer, per-peer connection tasks |
| `src/wire.rs` | newline-delimited JSON message type |
| `src/message.rs` | the `NetEvent` / `UiCommand` channel types |

All network I/O lives in spawned `tokio` tasks that talk to the UI **only** through `mpsc`
channels, so the interface never freezes while a slow Tor circuit is being built. The onion
service is published with `detach = false`, so it's torn down when hop6 exits — but because the
**secret key is persisted to disk** and reloaded on the next run, your `.onion` address stays
the same across restarts (see [Persistent identity](#persistent-identity)).

## Limitations (it's a prototype)

- **No authentication or encryption beyond Tor itself.** Tor onion services already give you
  end-to-end encryption and the peer's address authenticates the destination, but hop6 does not
  verify that the *sender* of an inbound message owns the onion it claims in `from_onion`. Don't
  treat identities as cryptographically proven.
- **No message persistence** — history is in-memory and lost on quit.
- **No offline delivery / retries** — if a peer is unreachable the dial simply fails.
- Inbound connections show as `(incoming)` until the peer's first message reveals its address.

## Troubleshooting

| symptom | fix |
|---|---|
| `could not reach Tor control port` | Tor isn't running, or `ControlPort 9051` isn't set. Start `tor -f ./torrc.sample`. |
| `Tor offered no usable auth method` | add `CookieAuthentication 1` to your torrc. |
| `failed to read Tor auth cookie` | Tor runs as another user; add `CookieAuthFileGroupReadable 1` and join Tor's group. |
| `could not connect to <onion>` | peer offline, wrong address, or its onion not yet published — wait and retry. |

[`ratatui`]: https://ratatui.rs
[`crossterm`]: https://docs.rs/crossterm
[`tokio`]: https://tokio.rs
