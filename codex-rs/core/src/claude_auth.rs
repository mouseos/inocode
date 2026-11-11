//! Claude OAuth credentials for the Messages wire.
//!
//! A provider with `wire_api = "anthropic_messages"` and no key of its own (`env_key`,
//! `experimental_bearer_token`, `auth`) signs in through the account pool's `claude` group:
//! the pool picks the sign-in, this file turns it into request authentication.

use codex_api::AuthProviderFutureOf;
use codex_api::AuthRefreshFuture;
use codex_api::BearerSource;
use codex_api::BearerSourceAuth;
use codex_api::SharedAuthProvider;
use codex_login::AuthManager;
use codex_login::account_pool;
use codex_login::account_pool::ClaudeCredential;
use codex_login::claude_oauth::ClaudeAuthManager;
use codex_model_provider_info::ModelProviderInfo;
use std::sync::Arc;

pub(crate) const CLAUDE_GROUP: &str = "claude";

struct ClaudeBearer(Arc<ClaudeAuthManager>);

impl BearerSource for ClaudeBearer {
    fn token(&self) -> AuthProviderFutureOf<'_, Result<String, String>> {
        Box::pin(async move { self.0.access_token().await.map_err(|err| err.to_string()) })
    }

    fn refresh(&self) -> AuthRefreshFuture<'_> {
        Box::pin(async move { self.0.refresh_now().await.unwrap_or(false) })
    }
}

/// A pooled API key: sent as the bearer the wire client turns into `x-api-key`.
struct KeyBearer(Arc<AuthManager>);

impl BearerSource for KeyBearer {
    fn token(&self) -> AuthProviderFutureOf<'_, Result<String, String>> {
        Box::pin(async move {
            self.0
                .auth()
                .await
                .and_then(|auth| auth.api_key().map(str::to_string))
                .ok_or_else(|| "the pooled API key is gone".to_string())
        })
    }

    fn refresh(&self) -> AuthRefreshFuture<'_> {
        Box::pin(async { false })
    }
}

/// The pool's active Claude credential as request authentication, when the provider has no key of
/// its own. The flag is true for an OAuth sign-in (bearer + OAuth beta), false for an API key.
pub(crate) fn pooled_auth(info: &ModelProviderInfo) -> Option<(SharedAuthProvider, bool)> {
    if info.env_key.is_some() || info.experimental_bearer_token.is_some() || info.auth.is_some() {
        return None;
    }
    match account_pool::global_for(CLAUDE_GROUP)?.active_claude_credential()? {
        ClaudeCredential::Oauth(manager) => Some((
            Arc::new(BearerSourceAuth(Arc::new(ClaudeBearer(manager)))),
            true,
        )),
        ClaudeCredential::ApiKey(manager) => Some((
            Arc::new(BearerSourceAuth(Arc::new(KeyBearer(manager)))),
            false,
        )),
    }
}
