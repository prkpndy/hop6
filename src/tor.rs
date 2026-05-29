//! Tor control-port integration.
//!
//! On startup we open a connection to the local Tor daemon's **control port**
//! (`127.0.0.1:9051`), authenticate with the cookie file, and publish an **ephemeral v3 onion
//! service** (`ADD_ONION NEW:ED25519-V3 ...`) that forwards virtual port 80 to our local
//! listener on `127.0.0.1:<local_port>`. Tor hands us back a freshly generated `.onion`
//! address which becomes this instance's identity.
//!
//! The service is created with `detach = false`, which means **Tor tears it down the moment
//! the control connection closes**. That is exactly the lifetime we want for an ephemeral
//! chat identity — so the returned [`Control`] handle must be kept alive for the whole program
//! (we park it in a task in `main`). Drop it and your `.onion` disappears.

use std::future::Ready;
use std::net::SocketAddr;

use anyhow::{anyhow, Context, Result};
use tokio::net::TcpStream;
use torut::control::{AsyncEvent, AuthenticatedConn, ConnError, UnauthenticatedConn};
use torut::onion::TorSecretKeyV3;

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

/// Connect to the control port, cookie-authenticate, and publish an ephemeral v3 onion service
/// mapping `onion:80 -> 127.0.0.1:local_port`.
///
/// Returns our `.onion` address (with the `.onion` suffix) and the control connection, which
/// the caller **must** keep alive.
pub async fn start_onion(local_port: u16) -> Result<(String, Control)> {
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

    // 4. Generate an ephemeral v3 key and publish the service.
    let key = TorSecretKeyV3::generate();
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
    Ok((onion, aconn))
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
    use super::normalize_onion;

    #[test]
    fn strips_suffix_and_port_and_whitespace() {
        assert_eq!(normalize_onion("abc"), "abc");
        assert_eq!(normalize_onion("abc.onion"), "abc");
        assert_eq!(normalize_onion("  abc.onion  "), "abc");
        assert_eq!(normalize_onion("abc.onion:80"), "abc");
        assert_eq!(normalize_onion("abc:80"), "abc");
    }
}
