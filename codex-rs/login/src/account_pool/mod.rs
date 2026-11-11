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
pub use pool::ClaudeCredential;
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
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::RwLock;

static GLOBAL: RwLock<BTreeMap<String, Arc<AccountPool>>> = RwLock::new(BTreeMap::new());

/// Makes `pool` the process-wide pool of its group. Hosts (app-server, CLI) call this at start-up.
pub fn install_global_for(group: &str, pool: Option<Arc<AccountPool>>) {
    let mut map = GLOBAL
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match pool {
        Some(pool) => {
            map.insert(group.to_string(), pool);
        }
        None => {
            map.remove(group);
        }
    }
}

pub fn global_for(group: &str) -> Option<Arc<AccountPool>> {
    GLOBAL
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(group)
        .cloned()
}

/// The OpenAI group (ChatGPT sign-ins and OpenAI API keys).
pub fn install_global(pool: Option<Arc<AccountPool>>) {
    install_global_for("openai", pool);
}

pub fn global() -> Option<Arc<AccountPool>> {
    global_for("openai")
}

/// The manager requests should use right now, when a pool is installed.
pub fn active_auth_manager() -> Option<Arc<AuthManager>> {
    global()?.active_auth_manager()
}
