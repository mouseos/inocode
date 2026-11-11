//! Native account pool: several ChatGPT sign-ins and OpenAI API keys, used by policy.
//!
//! New module on purpose: upstream files only get thin call-ins (see `model-provider` and
//! `core::session::turn`). Contract with clients: `ref/docs/16-account-pool-contract.md`.

mod pool;
mod provider;
mod select;
mod store;
mod types;

#[cfg(test)]
mod pool_tests;

pub use pool::AccountPool;
pub use pool::PoolEnv;
pub use provider::AccountIdentity;
pub use provider::AccountProvider;
pub use provider::BoxFuture;
pub use provider::OpenAiApiKeyProvider;
pub use provider::OpenAiChatgptProvider;
pub use provider::UsageFetcher;
pub use select::Candidate;
pub use select::SelectInput;
pub use select::select;
pub use store::AccountStore;
pub use store::DEFAULT_ID;
pub use store::PENDING_PREFIX;
pub use store::api_key_account_id;
pub use store::chatgpt_account_id;
pub use store::valid_id;
pub use types::*;

use crate::AuthManager;
use std::sync::Arc;
use std::sync::RwLock;

static GLOBAL: RwLock<Option<Arc<AccountPool>>> = RwLock::new(None);

/// Makes `pool` the process-wide pool. Hosts (app-server, CLI) call this once at start-up.
pub fn install_global(pool: Option<Arc<AccountPool>>) {
    *GLOBAL
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = pool;
}

pub fn global() -> Option<Arc<AccountPool>> {
    GLOBAL
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The manager requests should use right now, when a pool is installed.
pub fn active_auth_manager() -> Option<Arc<AuthManager>> {
    global()?.active_auth_manager()
}
