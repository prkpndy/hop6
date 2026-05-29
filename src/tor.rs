//! Tor control-port integration.
//!
//! On startup we open a connection to the local Tor daemon's **control port**
//! (`127.0.0.1:9051`), authenticate with the cookie file, and publish an **ephemeral v3 onion
//! service** (`ADD_ONION NEW:ED25519-V3 ...`) that forwards virtual port 80 to our local
//! listener on `127.0.0.1:<local_port>`. Tor hands us back a freshly generated `.onion`
//! address which becomes this instance's identity.
//!
//! The service is created with `detach = false`, which means **Tor tears it down the moment
//! the control connection closes** — so the returned [`Control`] handle must be kept alive for
//! the whole program (we park it in a task in `main`).
//!
//! ## Persistent identity
//!
//! The `.onion` address is derived entirely from a v3 secret key. To keep a **stable address
//! across restarts**, we persist that key to a local file (default
//! `$HOME/.config/hop6/identity.key`, overridable with `$HOP6_IDENTITY`) and reload it on the
//! next run instead of generating a fresh one. The port mapping is independent of the key and
//! is still passed to `ADD_ONION` on every publish. Because the key is the whole identity, the
//! file is written `0600` in a `0700` directory — treat it like an SSH private key.

use std::fs;
use std::future::Ready;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use tokio::net::TcpStream;
use torut::control::{AsyncEvent, AuthenticatedConn, ConnError, UnauthenticatedConn};
use torut::onion::TorSecretKeyV3;

/// Length of a serialized v3 secret key (expanded ed25519 secret key).
const KEY_LEN: usize = 64;

/// torut's `AuthenticatedConn` is generic over an async-event-handler type `H` bound by
/// `H: Fn(AsyncEvent<'static>) -> impl Future<Output = Result<(), ConnError>>`. We never
/// subscribe to async events, but the type still has to be *nameable* so we can hold the
/// connection in a struct field / return it. A bare function pointer returning a
/// `std::future::Ready` is the simplest concrete type that satisfies the bound.
pub type NoopHandler = fn(AsyncEvent<'static>) -> Ready<Result<(), ConnError>>;

/// The live, authenticated control-port connection. Keep it alive for the program's lifetime.
pub type Control = AuthenticatedConn<TcpStream, NoopHandler>;

/// Default Tor control port (`ControlPort 9051` in torrc).
pub const CONTROL_ADDR: &str = "127.0.0.1:9051";

/// The result of publishing our onion service. `control` must be kept alive for the whole
/// program (dropping it tears the service down).
pub struct Identity {
    /// Our `.onion` address (with the `.onion` suffix).
    pub onion: String,
    /// The live, authenticated control-port connection.
    pub control: Control,
    /// Where the secret key is persisted.
    pub key_path: PathBuf,
    /// `true` if we generated a brand-new key this run, `false` if we reloaded an existing one.
    pub created: bool,
}

/// Connect to the control port, cookie-authenticate, and publish a v3 onion service mapping
/// `onion:80 -> 127.0.0.1:local_port`, using a **persisted** key so the address is stable
/// across restarts (see module docs).
pub async fn start_onion(local_port: u16) -> Result<Identity> {
    // 0. Load (or first-time generate) our persistent identity key.
    let key_path = identity_path()?;
    let (key, created) = load_or_generate_key(&key_path)?;

    // 1. Connect to the control port.
    let stream = TcpStream::connect(CONTROL_ADDR)
        .await
        .with_context(|| format!("could not reach Tor control port at {CONTROL_ADDR} — is Tor running with `ControlPort 9051`?"))?;
    let mut uconn = UnauthenticatedConn::new(stream);

    // 2. Ask Tor which auth methods it supports, then let torut build cookie auth data by
    //    reading the cookie file referenced in the PROTOCOLINFO reply.
    let proto_info = uconn
        .load_protocol_info()
        .await
        .context("failed to read Tor PROTOCOLINFO")?;
    let auth = proto_info
        .make_auth_data()
        .context("failed to read Tor auth cookie (is the cookie file readable by this user?)")?
        .ok_or_else(|| {
            anyhow!(
                "Tor offered no usable auth method — add `CookieAuthentication 1` to your torrc"
            )
        })?;
    uconn
        .authenticate(&auth)
        .await
        .context("Tor control-port authentication failed")?;

    // 3. Upgrade to an authenticated connection (no-op event handler — see NoopHandler).
    let mut aconn: Control = uconn.into_authenticated::<NoopHandler>().await;

    // 4. Publish the service for our persisted key. The port mapping is supplied here on every
    //    run — it is independent of the key (which fixes only the address).
    let local: SocketAddr = format!("127.0.0.1:{local_port}")
        .parse()
        .expect("valid loopback socket addr");
    // Map remote virtual port 80 -> our local listener.
    let listeners = [(80u16, local)];
    aconn
        .add_onion_v3(
            &key,
            /* detach */ false,
            /* non_anonymous */ false,
            /* max_streams_close_circuit */ false,
            /* max_num_streams */ None,
            &mut listeners.iter(),
        )
        .await
        .context("ADD_ONION failed (control port reachable but service publish rejected)")?;

    // 5. Derive our public .onion address from the key.
    let onion = key.public().get_onion_address().to_string();
    Ok(Identity { onion, control: aconn, key_path, created })
}

/// Resolve the identity-key file path: `$HOP6_IDENTITY` if set, else
/// `$HOME/.config/hop6/identity.key`.
pub fn identity_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("HOP6_IDENTITY") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow!("$HOME is not set — set $HOP6_IDENTITY to choose an identity file"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("hop6")
        .join("identity.key"))
}

/// Load the persisted secret key, or generate and save a new one if the file doesn't exist.
/// Returns the key and whether it was freshly created.
fn load_or_generate_key(path: &Path) -> Result<(TorSecretKeyV3, bool)> {
    match fs::read(path) {
        Ok(bytes) => {
            let arr: [u8; KEY_LEN] = bytes.as_slice().try_into().map_err(|_| {
                anyhow!(
                    "identity file {} is corrupt (expected {KEY_LEN} bytes, found {})",
                    path.display(),
                    bytes.len()
                )
            })?;
            Ok((TorSecretKeyV3::from(arr), false))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = TorSecretKeyV3::generate();
            save_key(path, &key)?;
            Ok((key, true))
        }
        Err(e) => Err(anyhow::Error::from(e)
            .context(format!("failed to read identity file {}", path.display()))),
    }
}

/// Persist the secret key, creating the parent directory and locking down permissions
/// (`0700` dir, `0600` file) so the key is not world-readable.
fn save_key(path: &Path, key: &TorSecretKeyV3) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)
            .with_context(|| format!("creating identity directory {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
    }
    write_private(path, &key.as_bytes())
        .with_context(|| format!("writing identity file {}", path.display()))
}

/// Write `bytes` to `path` with owner-only permissions where the platform supports it.
#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    f.write_all(bytes)?;
    Ok(())
}

/// Strip a trailing `.onion` (and any `:port`) from a user-entered address, returning the bare
/// 56-character v3 service id. Tolerant of whitespace and the optional suffix so users can
/// paste either `abc...xyz` or `abc...xyz.onion`.
pub fn normalize_onion(input: &str) -> String {
    let s = input.trim();
    // Drop an optional `:port` first, then an optional `.onion` suffix.
    let s = s.split(':').next().unwrap_or(s);
    s.strip_suffix(".onion").unwrap_or(s).to_string()
}

#[cfg(test)]
mod tests {
    use super::{load_or_generate_key, normalize_onion};

    #[test]
    fn strips_suffix_and_port_and_whitespace() {
        assert_eq!(normalize_onion("abc"), "abc");
        assert_eq!(normalize_onion("abc.onion"), "abc");
        assert_eq!(normalize_onion("  abc.onion  "), "abc");
        assert_eq!(normalize_onion("abc.onion:80"), "abc");
        assert_eq!(normalize_onion("abc:80"), "abc");
    }

    #[test]
    fn key_persists_the_same_onion_across_loads() {
        let path = std::env::temp_dir().join("hop6_test_identity_persist.key");
        let _ = std::fs::remove_file(&path); // start clean

        // First call generates and saves.
        let (k1, created1) = load_or_generate_key(&path).unwrap();
        assert!(created1, "first load should create the key");

        // Second call reloads the same key → same .onion address.
        let (k2, created2) = load_or_generate_key(&path).unwrap();
        assert!(!created2, "second load should reuse the existing key");
        assert_eq!(
            k1.public().get_onion_address().to_string(),
            k2.public().get_onion_address().to_string(),
            "reloaded key must yield the same onion address"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_key_file_is_rejected() {
        let path = std::env::temp_dir().join("hop6_test_identity_corrupt.key");
        std::fs::write(&path, b"too short").unwrap();
        assert!(load_or_generate_key(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
