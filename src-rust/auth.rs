//! Path-prefix access-token gate for Stremio addon endpoints.
//!
//! When `cfg.users` is non-empty, the router mounts addon routes under
//! `/{user_token}/...`. This middleware extracts the token from the path,
//! validates it against `AppState.cfg`, stashes it in the request
//! extensions for downstream handlers, and forwards. Unknown tokens get
//! 404 (not 401 — at this layer we don't want to advertise that auth
//! lives here).
//!
//! `/v/{stream-token}.mkv` lives at root and is NOT gated by this
//! middleware. Its 128-bit opaque UUID is its own auth, and Stremio
//! constructs that URL directly from what `/{user_token}/stream/...`
//! returned, so it lives independently of the user-token namespace.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, Path, Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};

use crate::AppState;

/// Friendly user name resolved from the URL token. Inserted into
/// request extensions so handlers can `cfg.resolve(Some(&name.0))` to
/// get that user's merged config. Handlers take this as
/// `Option<Extension<UserName>>` — `None` means the route is mounted
/// unprefixed (auth disabled).
#[derive(Clone, Debug)]
pub struct UserName(pub String);

/// Axum middleware: extract the `:user_token` path segment, look up
/// the friendly user name whose `key` matches, 404 on no match,
/// otherwise stash a `UserName` extension and forward.
pub async fn require_access_token(
    State(state): State<Arc<AppState>>,
    Path(params): Path<HashMap<String, String>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Nested route param map should contain `user_token`. If it doesn't,
    // the router was mis-wired — fail closed.
    let token = match params.get("user_token") {
        Some(t) => t.clone(),
        None => {
            tracing::error!("[auth] middleware ran but :user_token not in path params");
            return Err(StatusCode::NOT_FOUND);
        }
    };

    let cfg = state.cfg.read().await;
    match cfg.user_for_key(&token) {
        Some(name) => {
            // Log the friendly name + path. We deliberately don't log
            // the token (it's in the URL anyway, but downstream log
            // shippers might be a different trust boundary).
            let name = name.to_string();
            tracing::info!("[auth] user={name} path={}", request.uri().path());
            drop(cfg);
            request.extensions_mut().insert(UserName(name));
            Ok(next.run(request).await)
        }
        None => {
            drop(cfg);
            // Determine the client IP for the ban tracker. Honors
            // `TRUST_PROXY_HEADERS=1` for behind-a-proxy setups.
            let ip = crate::client_ip(
                request.headers(),
                Some(&addr),
                state.trust_proxy_headers,
            )
            .unwrap_or(addr.ip());
            let triggered = state.ban_list.record_failure(ip);
            // 8-char prefix in the reject log so token typos can be
            // diagnosed without the full secret showing up.
            let prefix = token.chars().take(8).collect::<String>();
            if triggered {
                tracing::warn!(
                    "[auth] BAN ip={ip} after repeated bad tokens; latest={prefix}… path={}",
                    request.uri().path()
                );
            } else {
                tracing::warn!(
                    "[auth] reject unknown token={prefix}… ip={ip} path={}",
                    request.uri().path()
                );
            }
            Err(StatusCode::NOT_FOUND)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{AddonConfig, UserConfig};

    #[test]
    fn empty_users_means_no_auth() {
        let cfg = AddonConfig::default();
        assert!(!cfg.requires_auth());
    }

    #[test]
    fn populated_users_means_auth_required() {
        let mut cfg = AddonConfig::default();
        cfg.users.insert("alice".into(), UserConfig::default());
        assert!(cfg.requires_auth());
    }

    #[test]
    fn user_for_key_finds_by_secret_not_name() {
        let mut cfg = AddonConfig::default();
        let mut alice = UserConfig::default();
        alice.key = "x9k2-secret".into();
        cfg.users.insert("alice".into(), alice);

        // Lookup is by the secret value, not the friendly map key.
        assert_eq!(cfg.user_for_key("x9k2-secret"), Some("alice"));
        assert_eq!(cfg.user_for_key("alice"), None); // friendly name isn't the secret
        assert_eq!(cfg.user_for_key("X9K2-SECRET"), None); // case-sensitive
        assert_eq!(cfg.user_for_key(""), None); // empty rejected
    }

    #[test]
    fn validate_rejects_user_with_empty_key() {
        let mut cfg = AddonConfig::default();
        // Defaults populated so we get past the indexer/nntp checks.
        cfg.defaults.indexers.push(crate::config::Indexer {
            url: "https://x".into(),
            api_key: "k".into(),
        });
        cfg.defaults.nntp_servers.push(crate::config::NntpServer {
            server: "nntps://x".into(),
        });
        cfg.users.insert("alice".into(), UserConfig::default());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_duplicate_keys() {
        let mut cfg = AddonConfig::default();
        cfg.defaults.indexers.push(crate::config::Indexer {
            url: "https://x".into(),
            api_key: "k".into(),
        });
        cfg.defaults.nntp_servers.push(crate::config::NntpServer {
            server: "nntps://x".into(),
        });
        let mut alice = UserConfig::default();
        alice.key = "shared".into();
        let mut bob = UserConfig::default();
        bob.key = "shared".into();
        cfg.users.insert("alice".into(), alice);
        cfg.users.insert("bob".into(), bob);
        assert!(cfg.validate().is_err());
    }
}
