//! Provider plug-in point.
//!
//! The pool itself knows nothing about OpenAI. Everything that depends on who issued a
//! credential — how an account is recognised, which plan it is on, how its quota is read —
//! sits behind [`AccountProvider`]. Today there are two: ChatGPT accounts and OpenAI API keys.
//! Another vendor adds an [`AccountKind`] variant and one implementation of this trait.

use super::types::AccountKind;
use super::types::AccountUsage;
use crate::CodexAuth;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Reads the quota of one account. The caller supplies this (the backend client lives in a crate
/// that depends on this one).
pub type UsageFetcher =
    Arc<dyn Fn(CodexAuth) -> BoxFuture<'static, Result<AccountUsage, String>> + Send + Sync>;

/// What a credential says about itself. No e-mail address, ever.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountIdentity {
    pub id: String,
    pub plan: Option<String>,
}

pub trait AccountProvider: Send + Sync {
    fn kind(&self) -> AccountKind;

    /// Stable id and plan for a credential; `None` when it cannot be recognised.
    fn identify(&self, auth: &CodexAuth) -> Option<AccountIdentity>;

    /// Current usage. `Ok(None)` means this kind has no usage concept (API keys).
    fn fetch_usage(&self, auth: CodexAuth) -> BoxFuture<'static, Result<Option<AccountUsage>, String>>;
}

/// ChatGPT sign-ins.
#[derive(Default)]
pub struct OpenAiChatgptProvider {
    fetcher: Option<UsageFetcher>,
}

impl OpenAiChatgptProvider {
    pub fn new(fetcher: Option<UsageFetcher>) -> Self {
        Self { fetcher }
    }
}

impl AccountProvider for OpenAiChatgptProvider {
    fn kind(&self) -> AccountKind {
        AccountKind::Chatgpt
    }

    fn identify(&self, auth: &CodexAuth) -> Option<AccountIdentity> {
        let id = super::store::chatgpt_account_id(auth)?;
        let plan = auth
            .account_plan_type()
            .and_then(|plan| serde_json::to_value(plan).ok())
            .and_then(|value| value.as_str().map(str::to_string));
        Some(AccountIdentity { id, plan })
    }

    fn fetch_usage(&self, auth: CodexAuth) -> BoxFuture<'static, Result<Option<AccountUsage>, String>> {
        match self.fetcher.clone() {
            Some(fetch) => Box::pin(async move { fetch(auth).await.map(Some) }),
            None => Box::pin(async { Ok(None) }),
        }
    }
}

/// OpenAI API keys: no usage numbers, so they are ordered by policy instead.
#[derive(Default)]
pub struct OpenAiApiKeyProvider;

impl AccountProvider for OpenAiApiKeyProvider {
    fn kind(&self) -> AccountKind {
        AccountKind::ApiKey
    }

    fn identify(&self, auth: &CodexAuth) -> Option<AccountIdentity> {
        let key = auth.api_key()?;
        Some(AccountIdentity {
            id: super::store::api_key_account_id(key),
            plan: None,
        })
    }

    fn fetch_usage(&self, _auth: CodexAuth) -> BoxFuture<'static, Result<Option<AccountUsage>, String>> {
        Box::pin(async { Ok(None) })
    }
}
