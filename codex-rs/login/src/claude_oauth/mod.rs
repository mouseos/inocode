//! Claude (Pro/Max) OAuth: PKCE sign-in, token storage and refresh.
//!
//! Ported from OpenCodex `oauth/anthropic.ts` and `oauth/anthropic-identity.ts` (MIT).
//! Credentials live in `claude.json` next to the pool entry's other files; the access token is
//! used as a bearer by the Messages client (`codex-api` `WireKind::AnthropicMessages`).

mod consts;
mod flow;
mod manager;
mod store;

pub use consts::CALLBACK_PORT;
pub use consts::CODE_REDIRECT_URI;
pub use flow::Pending;
pub use flow::begin;
pub use flow::exchange;
pub use flow::refresh_credentials;
pub use flow::split_code_input;
pub use flow::wait_for_callback;
pub use manager::ClaudeAuthManager;
pub use store::ClaudeCredentials;
pub use store::claude_account_id;
