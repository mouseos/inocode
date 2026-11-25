//! Interactive logins for subscription-backed providers and the credential
//! store they fill.
//!
//! Reference implementation: lidge-jun/opencodex `src/oauth/` (see
//! docs/evidence/oauth-providers.md for the recorded wire facts). Every
//! endpoint URL is injectable so tests can point a flow at a mock server; the
//! defaults are the production endpoints. Credentials are kept by
//! [`crate::secrets`] for the store at `~/.inocode/auth.json`, and their
//! `Debug` output is redacted.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// How a provider authenticates its HTTP requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthKind {
    /// A static API key from the credential store.
    #[default]
    ApiKey,
    /// Claude Pro/Max OAuth; refreshes before requests.
    ClaudeOauth,
    /// ChatGPT/Codex OAuth; refreshes before requests.
    GptOauth,
    /// Meta Muse Code subscription; the stored `LLM|` key is the bearer.
    MuseSubscription,
    /// DeepAI Code account; the stored API key is sent as `api-key`.
    DeepAi,
}

impl AuthKind {
    /// The provider kind name that selects it, or `None` for the static-key
    /// kinds handled by `LlmConfig` itself.
    pub fn from_provider_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "claude-oauth" => Some(Self::ClaudeOauth),
            "gpt-oauth" => Some(Self::GptOauth),
            "muse-code" => Some(Self::MuseSubscription),
            "deepai" => Some(Self::DeepAi),
            _ => None,
        }
    }

    /// The key under which this kind's credential is stored.
    pub fn store_name(self) -> &'static str {
        match self {
            Self::ApiKey => "api-key",
            Self::ClaudeOauth => "claude-oauth",
            Self::GptOauth => "gpt-oauth",
            Self::MuseSubscription => "muse-code",
            Self::DeepAi => "deepai",
        }
    }
}

/// The credential one provider account holds. Which fields matter depends on
/// the flow: OAuth flows fill `access`/`refresh`/`expires_at`; Muse fills
/// `api_key` (the request bearer) and keeps the Meta account token in
/// `oauth_access_token`, which must never be sent to the model API.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    pub access: String,
    pub refresh: String,
    pub expires_at_ms: Option<u64>,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub api_key: Option<String>,
    pub oauth_access_token: Option<String>,
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("access", &"[redacted]")
            .field(
                "refresh",
                if self.refresh.is_empty() {
                    &"none"
                } else {
                    &"[redacted]"
                },
            )
            .field("expires_at_ms", &self.expires_at_ms)
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .field(
                "api_key",
                match &self.api_key {
                    Some(_) => &"[redacted]",
                    None => &"none",
                },
            )
            .field(
                "oauth_access_token",
                match &self.oauth_access_token {
                    Some(_) => &"[redacted]",
                    None => &"none",
                },
            )
            .finish()
    }
}

/// One stored account of a provider kind as the account registry shows it:
/// what identifies and describes it, never its secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub info: crate::secrets::AccountInfo,
    pub email: Option<String>,
    pub account_id: Option<String>,
}

/// What storing a finished sign-in did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedIn {
    pub account: String,
    /// `false` when it replaced the credential of an account with the same
    /// identity.
    pub added: bool,
}

/// The credentials of the store at `~/.inocode/auth.json`, by provider name
/// and account, and what is known about each account. The manual `Debug`
/// keeps credentials out of any derived output.
pub struct Store {
    path: PathBuf,
    entries: BTreeMap<String, BTreeMap<String, Credential>>,
    info: crate::secrets::Index,
    vault: crate::secrets::Vault,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.path)
            .field("providers", &self.entries.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Store {
    /// Opens (or creates) the default store at `~/.inocode/auth.json`.
    pub fn default_path() -> Result<PathBuf, String> {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .map_err(|_| "neither USERPROFILE nor HOME is set".to_string())?;
        Ok(PathBuf::from(home).join(".inocode").join("auth.json"))
    }

    /// Opens the store at `path` with [`crate::secrets::mode`].
    pub fn load(path: PathBuf) -> Result<Self, String> {
        Self::open(path, crate::secrets::mode())
    }

    /// Opens the store at `path`, keeping its secrets where `mode` selects
    /// and moving them there first when they are elsewhere. A missing file
    /// is an empty store; a store that cannot be read, parsed or decrypted
    /// is an error, because treating it as empty would make the next save
    /// discard every other provider's credential.
    pub fn open(path: PathBuf, mode: crate::secrets::StoreMode) -> Result<Self, String> {
        let default_location = Self::default_path()
            .is_ok_and(|default| crate::secrets::same_location(&default, &path));
        let vault = crate::secrets::Vault::new(path.clone(), mode, default_location);
        let contents = vault.load_contents()?;
        let entries = credentials(&contents.accounts, &path)?;
        Ok(Self {
            path,
            entries,
            info: contents.info,
            vault,
        })
    }

    /// A credential of `provider`: its default account's, else its first
    /// account's. Enough to tell whether the provider is signed in at all.
    pub fn credential(&self, provider: &str) -> Option<&Credential> {
        let accounts = self.entries.get(provider)?;
        accounts
            .get(crate::secrets::DEFAULT_ACCOUNT)
            .or_else(|| accounts.values().next())
    }

    /// The credential of one account of `provider`.
    pub fn account_credential(&self, provider: &str, account: &str) -> Option<&Credential> {
        self.entries.get(provider)?.get(account)
    }

    /// The accounts of `provider`, the earliest added first (those added
    /// before the time was recorded before all others).
    pub fn accounts(&self, provider: &str) -> Vec<Account> {
        let mut accounts: Vec<Account> = self
            .entries
            .get(provider)
            .into_iter()
            .flatten()
            .map(|(id, credential)| Account {
                id: id.clone(),
                info: self
                    .info
                    .get(provider)
                    .and_then(|info| info.get(id))
                    .cloned()
                    .unwrap_or_default(),
                email: credential.email.clone(),
                account_id: credential.account_id.clone(),
            })
            .collect();
        // Accounts added in the same millisecond: `default` is always the
        // first of a kind, so it stays first.
        let order = |account: &Account| {
            (
                account.info.added_ms,
                account.id != crate::secrets::DEFAULT_ACCOUNT,
            )
        };
        accounts.sort_by(|a, b| (order(a), &a.id).cmp(&(order(b), &b.id)));
        accounts
    }

    /// The account of `provider` a request uses: see [`choose_account`].
    pub fn choose(&self, provider: &str, active: Option<&str>) -> Result<String, crate::ErrorInfo> {
        choose_account(provider, &self.accounts(provider), active)
    }

    /// The file this store persists to.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Persists the credential of `provider`'s default account.
    ///
    /// The store is re-read first and only this account is replaced, so
    /// entries written by another process since this store was loaded (a
    /// login running next to a server that refreshed a token) are kept.
    pub fn save(&mut self, provider: &str, credential: Credential) -> Result<(), String> {
        self.update(provider, |stored| *stored = credential)
    }

    /// Edits the credential of `provider`'s default account as currently
    /// stored and persists it the same way as [`Store::save`].
    pub fn update<F>(&mut self, provider: &str, edit: F) -> Result<(), String>
    where
        F: FnOnce(&mut Credential),
    {
        self.update_account(provider, crate::secrets::DEFAULT_ACCOUNT, edit)
    }

    /// Edits the credential of one account of `provider`, made empty first
    /// when there is none, as [`Store::update`] does.
    pub fn update_account<F>(
        &mut self,
        provider: &str,
        account: &str,
        edit: F,
    ) -> Result<(), String>
    where
        F: FnOnce(&mut Credential),
    {
        let path = self.path.clone();
        self.change(provider, |contents| {
            let mut credential = match contents
                .accounts
                .get(provider)
                .and_then(|accounts| accounts.get(account))
            {
                Some(stored) => parse_credential(stored, provider, &path)?,
                None => empty_credential(),
            };
            edit(&mut credential);
            let value = serde_json::to_value(&credential).map_err(|e| e.to_string())?;
            contents
                .accounts
                .entry(provider.to_string())
                .or_default()
                .insert(account.to_string(), value);
            Ok((account.to_string(), ()))
        })
    }

    /// Changes what is known about an existing account of `provider`.
    pub fn edit_info<F>(&mut self, provider: &str, account: &str, edit: F) -> Result<(), String>
    where
        F: FnOnce(&mut crate::secrets::AccountInfo),
    {
        self.change(provider, |contents| {
            if !contents
                .accounts
                .get(provider)
                .is_some_and(|accounts| accounts.contains_key(account))
            {
                return Err(format!("{provider} has no account {account}"));
            }
            edit(
                contents
                    .info
                    .entry(provider.to_string())
                    .or_default()
                    .entry(account.to_string())
                    .or_default(),
            );
            Ok((account.to_string(), ()))
        })
    }

    /// Removes one account of `provider`, its secret included.
    pub fn remove_account(&mut self, provider: &str, account: &str) -> Result<(), String> {
        self.change(provider, |contents| {
            let removed = contents
                .accounts
                .get_mut(provider)
                .and_then(|accounts| accounts.remove(account));
            if removed.is_none() {
                return Err(format!("{provider} has no account {account}"));
            }
            if contents
                .accounts
                .get(provider)
                .is_some_and(BTreeMap::is_empty)
            {
                contents.accounts.remove(provider);
            }
            if let Some(info) = contents.info.get_mut(provider) {
                info.remove(account);
                if info.is_empty() {
                    contents.info.remove(provider);
                }
            }
            Ok((account.to_string(), ()))
        })
    }

    /// Stores the credential of a finished sign-in of `provider`: over the
    /// account that signs in the same person ([`same_identity`]), which
    /// then no longer needs a new sign-in, else as a new account. The first
    /// account of a provider is its default account.
    pub fn add_signed_in(
        &mut self,
        provider: &str,
        credential: Credential,
    ) -> Result<SignedIn, String> {
        let path = self.path.clone();
        self.change(provider, |contents| {
            let accounts = contents.accounts.entry(provider.to_string()).or_default();
            let mut matched = None;
            for (id, stored) in accounts.iter() {
                if same_identity(&parse_credential(stored, provider, &path)?, &credential) {
                    matched = Some(id.clone());
                    break;
                }
            }
            let (account, added, credential) = match matched {
                Some(id) => {
                    let old = parse_credential(&accounts[&id], provider, &path)?;
                    let mut credential = credential;
                    credential.email = credential.email.or(old.email);
                    credential.account_id = credential.account_id.or(old.account_id);
                    (id, false, credential)
                }
                None => (new_account_id(accounts)?, true, credential),
            };
            let value = serde_json::to_value(&credential).map_err(|e| e.to_string())?;
            accounts.insert(account.clone(), value);
            let info = contents
                .info
                .entry(provider.to_string())
                .or_default()
                .entry(account.clone())
                .or_default();
            info.needs_reauth = false;
            if added {
                info.added_ms = Some(now_ms());
            }
            Ok((account.clone(), SignedIn { account, added }))
        })
    }

    /// Applies `edit` to the store as currently stored, under its lock,
    /// and persists it; `edit` names the account it changed.
    fn change<T>(
        &mut self,
        provider: &str,
        edit: impl FnOnce(&mut crate::secrets::Contents) -> Result<(String, T), String>,
    ) -> Result<T, String> {
        let _lock = crate::secrets::lock_store(&self.path)?;
        let mut contents = self.vault.load_locked()?;
        let (account, result) = edit(&mut contents)?;
        self.vault.save_locked(&contents, provider, &account)?;
        self.entries = credentials(&contents.accounts, &self.path)?;
        self.info = contents.info;
        Ok(result)
    }
}

fn empty_credential() -> Credential {
    Credential {
        access: String::new(),
        refresh: String::new(),
        expires_at_ms: None,
        account_id: None,
        email: None,
        api_key: None,
        oauth_access_token: None,
    }
}

/// An unused id for a new account: the default account's name for the
/// first, else eight random hex digits.
fn new_account_id(accounts: &BTreeMap<String, Value>) -> Result<String, String> {
    if accounts.is_empty() {
        return Ok(crate::secrets::DEFAULT_ACCOUNT.to_string());
    }
    loop {
        let mut bytes = [0u8; 4];
        getrandom::getrandom(&mut bytes).map_err(|e| e.to_string())?;
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        if !accounts.contains_key(&id) {
            return Ok(id);
        }
    }
}

/// Whether two credentials sign in the same person. Every identity field
/// both know (email, ignoring case; account id) must agree, and at least
/// one must be known to both. Both are compared because neither is enough
/// alone: a ChatGPT account id names a workspace, which several people
/// share, and one person may sign in to several workspaces with one email.
pub fn same_identity(a: &Credential, b: &Credential) -> bool {
    let email = |credential: &Credential| {
        credential
            .email
            .as_deref()
            .map(str::trim)
            .filter(|email| !email.is_empty())
            .map(str::to_lowercase)
    };
    let id = |credential: &Credential| {
        credential
            .account_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
    };
    let emails = (email(a), email(b));
    let ids = (id(a), id(b));
    let agree = |pair: &(Option<String>, Option<String>)| match pair {
        (Some(x), Some(y)) => Some(x == y),
        _ => None,
    };
    matches!(
        (agree(&emails), agree(&ids)),
        (Some(true), None | Some(true)) | (None, Some(true))
    )
}

/// The account of `provider` a request uses: `active` when it is set, else
/// the only account that is neither paused nor waiting for a new sign-in.
/// A paused account, one that needs a new sign-in, an unknown `active`, or
/// several usable accounts with none chosen are errors that say what to do.
pub fn choose_account(
    provider: &str,
    accounts: &[Account],
    active: Option<&str>,
) -> Result<String, crate::ErrorInfo> {
    use crate::ErrorInfo;
    let unusable = |account: &Account| {
        let name = describe_account(account);
        if account.info.needs_reauth {
            Some(ErrorInfo::new(
                "account_needs_reauth",
                format!(
                    "the {provider} account {name} was refused and needs a new sign-in: \
                     run /login {provider}, or choose another account with /accounts use"
                ),
            ))
        } else if account.info.paused {
            Some(ErrorInfo::new(
                "account_paused",
                format!(
                    "the {provider} account {name} is paused: resume it with /accounts resume, \
                     or choose another account with /accounts use"
                ),
            ))
        } else {
            None
        }
    };
    if accounts.is_empty() {
        return Err(ErrorInfo::new(
            "provider_config",
            format!(
                "no stored credential for {provider}; sign in with /login {provider} \
                 (or `inocode-server login`)"
            ),
        ));
    }
    if let Some(active) = active {
        let account = accounts
            .iter()
            .find(|account| account.id == active)
            .ok_or_else(|| {
                ErrorInfo::new(
                    "account_unknown",
                    format!(
                        "the active account {active} of {provider} is not stored; \
                         choose one of /accounts list with /accounts use"
                    ),
                )
            })?;
        return match unusable(account) {
            Some(error) => Err(error),
            None => Ok(account.id.clone()),
        };
    }
    let usable: Vec<&Account> = accounts
        .iter()
        .filter(|account| unusable(account).is_none())
        .collect();
    match usable.as_slice() {
        [only] => Ok(only.id.clone()),
        [] if accounts.len() == 1 => Err(unusable(&accounts[0]).expect("unusable")),
        [] => Err(ErrorInfo::new(
            "account_unavailable",
            format!(
                "every {provider} account is paused or needs a new sign-in; \
                 see /accounts list"
            ),
        )),
        several => Err(ErrorInfo::new(
            "account_ambiguous",
            format!(
                "{provider} has several accounts ({}) and none is chosen; \
                 choose one with /accounts use",
                several
                    .iter()
                    .map(|account| describe_account(account))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

/// The account's id, with its alias or email when it has one.
fn describe_account(account: &Account) -> String {
    match account.info.alias.as_deref().or(account.email.as_deref()) {
        Some(name) => format!("{} ({name})", account.id),
        None => account.id.clone(),
    }
}

/// Each provider's accounts' credentials.
fn credentials(
    accounts: &crate::secrets::Accounts,
    path: &std::path::Path,
) -> Result<BTreeMap<String, BTreeMap<String, Credential>>, String> {
    accounts
        .iter()
        .map(|(provider, accounts)| {
            let parsed = accounts
                .iter()
                .map(|(account, stored)| {
                    Ok((account.clone(), parse_credential(stored, provider, path)?))
                })
                .collect::<Result<_, String>>()?;
            Ok((provider.clone(), parsed))
        })
        .collect()
}

fn parse_credential(
    stored: &Value,
    provider: &str,
    path: &std::path::Path,
) -> Result<Credential, String> {
    Credential::deserialize(stored).map_err(|e| {
        format!(
            "the credential store {} is not valid: {provider}: {e}",
            path.display()
        )
    })
}

/// The account of `auth` at `path` a request uses ([`choose_account`] with
/// `active`) and its credential, refreshed first when it has expired.
/// `token_url` replaces the provider's token endpoint in tests.
pub(crate) async fn fresh(
    path: PathBuf,
    auth: AuthKind,
    active: Option<&str>,
    token_url: Option<&str>,
) -> Result<(String, Credential), crate::ErrorInfo> {
    renewed(path, auth, active, None, token_url).await
}

/// [`fresh`] after refreshing the credential even though it has not
/// expired, unless it no longer holds `rejected`: another request already
/// replaced the token a service refused. Refreshing asks for the current
/// scopes, which is how a sign-in made before a scope was added gains it.
/// `token_url` replaces the provider's token endpoint in tests.
pub(crate) async fn renew(
    path: PathBuf,
    auth: AuthKind,
    active: Option<&str>,
    rejected: &str,
    token_url: Option<&str>,
) -> Result<(String, Credential), crate::ErrorInfo> {
    renewed(path, auth, active, Some(rejected), token_url).await
}

async fn renewed(
    path: PathBuf,
    auth: AuthKind,
    active: Option<&str>,
    rejected: Option<&str>,
    token_url: Option<&str>,
) -> Result<(String, Credential), crate::ErrorInfo> {
    use crate::ErrorInfo;
    let store =
        Store::load(path.clone()).map_err(|error| ErrorInfo::new("provider_config", error))?;
    let name = auth.store_name();
    let account = store.choose(name, active)?;
    let credential = store
        .account_credential(name, &account)
        .cloned()
        .ok_or_else(|| ErrorInfo::new("internal", "the chosen account has no credential"))?;
    if is_fresh(&credential, now_ms()) && rejected.is_none() {
        return Ok((account, credential));
    }
    if matches!(
        auth,
        AuthKind::MuseSubscription | AuthKind::ApiKey | AuthKind::DeepAi
    ) {
        // The Muse and DeepAI keys are static; the Muse account token
        // exists only to mint its key.
        return Ok((account, credential));
    }
    let mut endpoints = if auth == AuthKind::GptOauth {
        FlowEndpoints::chatgpt()
    } else {
        FlowEndpoints::anthropic()
    };
    if let Some(url) = token_url {
        endpoints.token_url = url.to_string();
    }
    let refreshed = refresh_stored(
        path.clone(),
        name,
        &account,
        rejected,
        |refresh| async move {
            if auth == AuthKind::GptOauth {
                refresh_gpt(&endpoints, &refresh).await
            } else {
                refresh_claude(&endpoints, &refresh).await
            }
        },
    )
    .await;
    match refreshed {
        Ok(credential) => Ok((account, credential)),
        Err(RefreshError::Store(error)) => Err(ErrorInfo::new("provider_config", error)),
        Err(RefreshError::Refresh(error)) if refresh_refused(&error) => {
            let _ = mark_needs_reauth(&path, name, &account);
            Err(ErrorInfo::new(
                "provider_http",
                format!(
                    "credential refresh failed: {error}; the {name} account {account} needs a \
                     new sign-in: run /login {name}"
                ),
            ))
        }
        Err(RefreshError::Refresh(error)) => Err(ErrorInfo::new(
            "provider_http",
            format!("credential refresh failed: {error}"),
        )),
    }
}

/// Whether the token endpoint refused the refresh token itself (HTTP 400,
/// 401 or 403, as for `invalid_grant`), not just failed to answer: only
/// then can a new sign-in alone fix it.
fn refresh_refused(error: &str) -> bool {
    error
        .strip_prefix(TOKEN_STATUS_PREFIX)
        .and_then(|status| status.parse::<u16>().ok())
        .is_some_and(|status| matches!(status, 400 | 401 | 403))
}

/// Marks one account as refused, so requests stop using it until it signs
/// in again.
pub(crate) fn mark_needs_reauth(
    path: &std::path::Path,
    provider: &str,
    account: &str,
) -> Result<(), String> {
    Store::load(path.to_path_buf())?.edit_info(provider, account, |info| info.needs_reauth = true)
}

/// Whether `credential` can be used at `now_ms` without a refresh.
pub(crate) fn is_fresh(credential: &Credential, now_ms: u64) -> bool {
    credential
        .expires_at_ms
        .is_none_or(|expires| now_ms < expires)
}

/// How long a request waits for another refresh of the same credential
/// (reference `OAUTH_REFRESH_LOCK_WAIT_MS`).
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(60);

/// Why [`refresh_stored`] failed.
#[derive(Debug)]
pub(crate) enum RefreshError {
    /// The store or its lock could not be used.
    Store(String),
    /// The token endpoint refused or could not be reached.
    Refresh(String),
}

/// Refreshes one account's stored credential, at most once at a time across
/// this process and others.
///
/// Rotating refresh tokens are single-use: two requests that both found the
/// credential expired must not both spend it. The second one waits for the
/// refresh lock, finds the credential the first one stored, and uses it. The
/// result is the stored credential, so fields the token response does not
/// carry (account id, email) are kept. With `rejected`, a credential that
/// has not expired is refreshed too while it still holds that access token.
pub(crate) async fn refresh_stored<F, Fut>(
    path: PathBuf,
    provider: &str,
    account: &str,
    rejected: Option<&str>,
    refresh: F,
) -> Result<Credential, RefreshError>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<Credential, String>>,
{
    // The default account keeps the lock name of the single account a
    // provider had, so an older process refreshing it still takes the same
    // lock.
    let suffix = if account == crate::secrets::DEFAULT_ACCOUNT {
        format!(".refresh.{provider}.lock")
    } else {
        format!(".refresh.{provider}.{account}.lock")
    };
    let lock = crate::secrets::open_lock_file(&crate::secrets::sibling(&path, &suffix))
        .map_err(RefreshError::Store)?;
    let deadline = tokio::time::Instant::now() + REFRESH_LOCK_WAIT;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(RefreshError::Store(format!(
                        "another refresh of {provider} did not finish in time"
                    )));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(RefreshError::Store(format!(
                    "cannot lock the {provider} refresh: {error}"
                )))
            }
        }
    }
    let mut store = Store::load(path).map_err(RefreshError::Store)?;
    let current = store
        .account_credential(provider, account)
        .cloned()
        .ok_or_else(|| {
            RefreshError::Store(format!(
                "no stored credential for {provider} account {account}; run the login first"
            ))
        })?;
    let replaced = rejected.is_some_and(|token| token != current.access);
    if is_fresh(&current, now_ms()) && (rejected.is_none() || replaced) {
        return Ok(current);
    }
    let new = refresh(current.refresh.clone())
        .await
        .map_err(RefreshError::Refresh)?;
    store
        .update_account(provider, account, |stored| {
            stored.access = new.access;
            stored.refresh = new.refresh;
            stored.expires_at_ms = new.expires_at_ms;
        })
        .map_err(RefreshError::Store)?;
    store
        .account_credential(provider, account)
        .cloned()
        .ok_or_else(|| {
            RefreshError::Store(format!("the {provider} credential vanished after saving"))
        })
}

// PKCE verifier/challenge pair (RFC 7636 `S256`).
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn generate_pkce() -> Pkce {
    // RFC 7636 requires an unpredictable verifier: the entropy must come from
    // the OS CSPRNG, not from time- or address-derived hashes.
    let mut verifier_bytes = [0u8; 32];
    getrandom::getrandom(&mut verifier_bytes)
        .map_err(|e| e.to_string())
        .expect("OS CSPRNG");
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

/// One grant's endpoints; defaults point at the production hosts, tests
/// override them with local mock servers.
#[derive(Debug, Clone)]
pub struct FlowEndpoints {
    pub authorize_url: String,
    pub token_url: String,
    pub callback_port: u16,
    pub callback_path: String,
    /// Used by code-display modes where no local port can receive the browser.
    pub code_redirect_uri: Option<String>,
}

impl FlowEndpoints {
    pub fn anthropic() -> Self {
        Self {
            authorize_url: "https://claude.ai/oauth/authorize".to_string(),
            token_url: "https://api.anthropic.com/v1/oauth/token".to_string(),
            callback_port: 54545,
            callback_path: "/callback".to_string(),
            code_redirect_uri: Some("https://platform.claude.com/oauth/code/callback".to_string()),
        }
    }

    pub fn chatgpt() -> Self {
        Self {
            authorize_url: "https://auth.openai.com/oauth/authorize".to_string(),
            token_url: "https://auth.openai.com/oauth/token".to_string(),
            callback_port: 1455,
            callback_path: "/auth/callback".to_string(),
            code_redirect_uri: None,
        }
    }
}

const ANTHROPIC_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const GPT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ANTHROPIC_SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload user:plugins";
/// The scopes a refresh asks for, Claude Code's list without
/// `org:create_api_key`; a refresh that names them widens an older grant.
const ANTHROPIC_REFRESH_SCOPES: &str = "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload user:plugins";
/// Anthropic's OAuth beta opt-in; requests with an OAuth bearer are rejected
/// without it.
pub const CLAUDE_CODE_BETA: &str = "claude-code-20250219,oauth-2025-04-20";
/// Identity block Claude Code sends ahead of the conversation; the OAuth
/// backend expects it (reference implementation `src/claude/anthropic.ts`).
pub const CLAUDE_CODE_SYSTEM_INSTRUCTION: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
/// Public client id of Meta's own Muse Code CLI; published in the device URL.
const MUSE_CLIENT_ID: &str = "1031625952748946";
const MUSE_DEVICE_AUTH_URL: &str = "https://auth.meta.com/oidc/device/authorization/";
const MUSE_DEVICE_TOKEN_URL: &str = "https://auth.meta.com/oidc/device/token/";
const MUSE_KEY_URL: &str = "https://api.meta.ai/muse-code/key";
const MUSE_DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Meta's Model API clients send these two headers on every request.
pub const MUSE_API_VERSION: &str = "1.0.0";
pub const MUSE_USER_AGENT: &str = "muse-build/1.3.0";

/// Splits a pasted `code#state` (or a plain code) per the Anthropic code mode.
pub fn split_code_fragment(code: &str) -> (String, Option<String>) {
    match code.split_once('#') {
        Some((code, state)) if !state.is_empty() => (code.to_string(), Some(state.to_string())),
        _ => (code.to_string(), None),
    }
}

/// Decodes a JWT payload segment for routing claims; not an authenticity proof.
pub fn jwt_payload(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The ChatGPT account id from the id/access token claims.
pub fn chatgpt_account_id(token: &str) -> Option<String> {
    let payload = jwt_payload(token)?;
    let claims = payload.get("https://api.openai.com/auth")?;
    let id = claims.get("chatgpt_account_id")?.as_str()?;
    let trimmed = id.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub(crate) struct Callback {
    pub(crate) code: String,
    pub(crate) state: String,
}

/// Interprets a pasted code. A `code#state` paste is an authorization
/// response and must echo this login's state; a bare code belongs to the same
/// PKCE session and inherits it (reference `#waitForCallback` manual input).
fn pasted_callback(pasted: &str, expected_state: &str) -> Result<Callback, String> {
    let (code, state) = split_code_fragment(pasted.trim());
    if code.is_empty() {
        return Err("the pasted input contains no authorization code".to_string());
    }
    match state {
        Some(state) if state != expected_state => {
            Err("state mismatch - possible CSRF attack".to_string())
        }
        Some(state) => Ok(Callback { code, state }),
        None => Ok(Callback {
            code,
            state: expected_state.to_string(),
        }),
    }
}

/// Runs the localhost callback listeners (IPv4 and, where supported, IPv6 —
/// the advertised redirect is `localhost`, which browsers may resolve to
/// either, reference `loopbackBindHostnames`) and returns the browser
/// redirect carrying a code.
pub(crate) async fn await_callback(
    port: u16,
    path: &str,
    shutdown: Duration,
) -> Result<Callback, String> {
    let path = path.to_string();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let v4 = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("cannot listen on port {port}: {e}"))?;
    let mut listeners = vec![v4];
    if let Ok(v6) = tokio::net::TcpListener::bind(("::1", port)).await {
        listeners.push(v6);
    }
    // Dropping the set when this function returns stops every listener and
    // every connection still open, so nothing keeps the ports after the flow.
    let mut tasks = tokio::task::JoinSet::new();
    for listener in listeners {
        let sender = sender.clone();
        let path = path.clone();
        tasks.spawn(async move {
            // Each connection is served on its own, so a socket that never
            // sends a request (a browser preconnect) cannot hold up the
            // redirect arriving on another one.
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let sender = sender.clone();
                let path = path.clone();
                connections.spawn(async move {
                    if let Some(result) = handle_callback_connection(&mut socket, &path).await {
                        let _ = sender.send(result);
                    }
                });
            }
        });
    }
    drop(sender);
    let deadline = tokio::time::Instant::now() + shutdown;
    match tokio::time::timeout_at(deadline, receiver.recv()).await {
        Ok(Some(result)) => result,
        _ => Err("the browser approval did not arrive in time".to_string()),
    }
}

/// Handles one callback connection. `None` keeps waiting (wrong path, a
/// missing code, or a foreign error whose state does not match — the
/// reference only consumes the flow when the state matches).
async fn handle_callback_connection(
    socket: &mut tokio::net::TcpStream,
    path: &str,
) -> Option<Result<Callback, String>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // The request line and headers fit in a small fixed read for a browser GET.
    let mut buffer = vec![0u8; 4096];
    let read: usize = socket.read(&mut buffer).await.ok()?;
    let request_line = String::from_utf8_lossy(&buffer[..read]);
    let target = request_line
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_string();
    let denied = target.contains("error=");
    let body = if denied {
        "<html><body><h1>login denied</h1></body></html>"
    } else {
        "<html><body><h1>login complete</h1>You can close this window.</body></html>"
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
    if !target.contains(path) {
        return None; // favicon or probe: keep waiting
    }
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("code", value)) => code = Some(urldecode(value)),
            Some(("state", value)) => state = Some(urldecode(value)),
            _ => {}
        }
    }
    if let Some(code) = code {
        return Some(Ok(Callback {
            code,
            state: state.unwrap_or_default(),
        }));
    }
    if denied {
        return Some(Err("the login was denied in the browser".to_string()));
    }
    None
}

fn urldecode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    index += 3;
                    continue;
                }
                out.push(bytes[index]);
                index += 1;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One-shot client for the token endpoints: no redirect following (the
/// reference treats a redirect after token rotation as an unknown outcome)
/// and a bounded 30-second request window.
fn token_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("cannot build the token HTTP client: {e}"))
}

/// Largest successful body accepted from the login endpoints (reference
/// `CHATGPT_SUCCESS_BODY_MAX_BYTES`); token and key payloads are far smaller.
pub(crate) const SUCCESS_BODY_MAX_BYTES: usize = 64 * 1024;

pub(crate) async fn read_success_body(
    mut response: reqwest::Response,
    what: &str,
) -> Result<Vec<u8>, String> {
    crate::llm::read_capped(&mut response, SUCCESS_BODY_MAX_BYTES)
        .await
        .map_err(|e| format!("reading the {what} failed: {e}"))?
        .ok_or_else(|| format!("the {what} exceeds {SUCCESS_BODY_MAX_BYTES} bytes"))
}

async fn post_token_json(
    client: &reqwest::Client,
    url: &str,
    body: Value,
) -> Result<Value, String> {
    send_token_request(
        client
            .post(url)
            .header("content-type", "application/json")
            .json(&body),
    )
    .await
}

async fn post_token_form(
    client: &reqwest::Client,
    url: &str,
    body: &[(&str, &str)],
) -> Result<Value, String> {
    let mut encoded = String::new();
    for (name, value) in body {
        if !encoded.is_empty() {
            encoded.push('&');
        }
        encoded.push_str(name);
        encoded.push('=');
        encoded.push_str(&urlencode(value));
    }
    send_token_request(
        client
            .post(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(encoded),
    )
    .await
}

/// How a token endpoint's refusal begins; [`refresh_refused`] reads the
/// status after it.
const TOKEN_STATUS_PREFIX: &str = "token request returned HTTP ";

async fn send_token_request(request: reqwest::RequestBuilder) -> Result<Value, String> {
    let response = request
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("token request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        // Never echo the body: it can carry request material.
        return Err(format!("{TOKEN_STATUS_PREFIX}{}", status.as_u16()));
    }
    let bytes = read_success_body(response, "token response").await?;
    serde_json::from_slice(&bytes).map_err(|_| "token response was not valid JSON".to_string())
}

pub(crate) fn urlencode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Milliseconds since the Unix epoch; a clock set before the epoch reads 0.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn expiry_from(expires_in: Option<u64>) -> Option<u64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    // Five-minute skew so a request started near expiry refreshes first. A
    // missing expires_in defaults to one hour so refresh semantics still
    // apply (reference `credsFrom`).
    let seconds = expires_in.unwrap_or(3600);
    Some(now.saturating_add(seconds.saturating_sub(300).saturating_mul(1000)))
}

fn credential_from_token(
    token: &Value,
    fallback_refresh: Option<&str>,
) -> Result<Credential, String> {
    let access = token
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("token response is missing the access token")?
        .to_string();
    Ok(Credential {
        access,
        refresh: token
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| fallback_refresh.map(str::to_string))
            .unwrap_or_default(),
        expires_at_ms: expiry_from(token.get("expires_in").and_then(Value::as_u64)),
        account_id: None,
        email: None,
        api_key: None,
        oauth_access_token: None,
    })
}

/// Everything the browser login needs before the user approves.
pub struct ClaudeLoginStart {
    /// The URL to open in a browser.
    pub url: String,
    /// The `redirect_uri` the token exchange must repeat.
    pub redirect: String,
    pub verifier: String,
    pub state: String,
    /// The port the callback listener must bind (may have fallen back from
    /// the preferred one).
    pub callback_port: u16,
}

/// Builds the authorize URL with a fresh PKCE pair. The `ClaudeLoginStart`
/// returned here must be passed to [`claude_login_finish`].
pub fn claude_login_start(endpoints: &FlowEndpoints, code_mode: bool) -> ClaudeLoginStart {
    let pkce = generate_pkce();
    let state = format!("inocode-{}", generate_state());
    let callback_port = reserve_callback_port(endpoints.callback_port);
    let redirect = match (code_mode, &endpoints.code_redirect_uri) {
        (true, Some(uri)) => uri.clone(),
        _ => {
            // The OAuth apps registered `localhost` as the redirect host; it
            // must be advertised verbatim (reference callback-server).
            format!(
                "http://localhost:{}{}",
                callback_port, endpoints.callback_path
            )
        }
    };
    let url = format!(
        "{}?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        endpoints.authorize_url,
        urlencode(ANTHROPIC_CLIENT_ID),
        urlencode(&redirect),
        urlencode(ANTHROPIC_SCOPES),
        urlencode(&pkce.challenge),
        urlencode(&state),
    );
    ClaudeLoginStart {
        url,
        redirect,
        verifier: pkce.verifier,
        state,
        callback_port,
    }
}

/// Reserves the callback port: the preferred one is tried first and only a
/// busy port falls back to an ephemeral one (reference port fallback, which
/// is possible whenever no fixed redirect URI forces the port). The probe
/// listener is dropped so the flow can bind it again; the window between the
/// probe and the bind mirrors the reference's own fallback race.
pub(crate) fn reserve_callback_port(preferred: u16) -> u16 {
    match std::net::TcpListener::bind(("127.0.0.1", preferred)) {
        Ok(listener) => {
            let port = listener
                .local_addr()
                .map(|addr| addr.port())
                .unwrap_or(preferred);
            drop(listener);
            port
        }
        Err(_) => match std::net::TcpListener::bind(("127.0.0.1", 0)) {
            Ok(listener) => listener
                .local_addr()
                .map(|addr| addr.port())
                .unwrap_or(preferred),
            Err(_) => preferred,
        },
    }
}

/// Exchanges the approval for tokens. In code-display mode the user pasted the
/// `code#state` Anthropic showed, passed as `pasted`; in browser mode
/// (`pasted: None`) a local callback listener receives the redirect.
pub async fn claude_login_finish(
    endpoints: &FlowEndpoints,
    start: ClaudeLoginStart,
    pasted: Option<String>,
) -> Result<Credential, String> {
    let callback = match pasted {
        Some(pasted) => pasted_callback(&pasted, &start.state)?,
        None => {
            await_callback(
                start.callback_port,
                &endpoints.callback_path,
                Duration::from_secs(300),
            )
            .await?
        }
    };
    // CSRF check: the callback must echo the state this login generated
    // (reference `stateMatches`).
    if callback.state != start.state {
        return Err("state mismatch - possible CSRF attack".to_string());
    }
    let client = token_client()?;
    let token = post_token_json(
        &client,
        &endpoints.token_url,
        json!({
            "grant_type": "authorization_code",
            "client_id": ANTHROPIC_CLIENT_ID,
            "code": callback.code,
            "state": callback.state,
            "redirect_uri": start.redirect,
            "code_verifier": start.verifier,
        }),
    )
    .await?;
    let mut credential = credential_from_token(&token, None)?;
    credential.email = token
        .get("account")
        .and_then(|account| account.get("email_address"))
        .and_then(Value::as_str)
        .map(str::to_string);
    credential.account_id = token
        .get("account")
        .and_then(|account| account.get("uuid"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(credential)
}

/// Refreshes a Claude OAuth credential.
pub async fn refresh_claude(
    endpoints: &FlowEndpoints,
    refresh: &str,
) -> Result<Credential, String> {
    let client = token_client()?;
    let scoped = post_token_json(
        &client,
        &endpoints.token_url,
        json!({
            "grant_type": "refresh_token",
            "client_id": ANTHROPIC_CLIENT_ID,
            "refresh_token": refresh,
            "scope": ANTHROPIC_REFRESH_SCOPES,
        }),
    )
    .await;
    // A grant the scopes cannot widen is refused; the refresh token is not
    // spent then, so it is refreshed with the scopes it already has.
    let token = match scoped {
        Ok(token) => token,
        Err(_) => {
            post_token_json(
                &client,
                &endpoints.token_url,
                json!({
                    "grant_type": "refresh_token",
                    "client_id": ANTHROPIC_CLIENT_ID,
                    "refresh_token": refresh,
                }),
            )
            .await?
        }
    };
    credential_from_token(&token, Some(refresh))
}

/// Everything the ChatGPT/Codex browser login needs before the user approves.
pub struct GptLoginStart {
    pub url: String,
    pub redirect: String,
    pub verifier: String,
    pub state: String,
    /// The port the callback listener must bind (may have fallen back).
    pub callback_port: u16,
}

/// Builds the ChatGPT authorize URL with a fresh PKCE pair; pass the result
/// to [`gpt_login_finish`].
pub fn gpt_login_start(endpoints: &FlowEndpoints) -> GptLoginStart {
    let pkce = generate_pkce();
    let state = generate_state();
    let callback_port = reserve_callback_port(endpoints.callback_port);
    let redirect = format!(
        "http://localhost:{}{}",
        callback_port, endpoints.callback_path
    );
    let url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&codex_cli_simplified_flow=true&originator=codex_cli_rs&id_token_add_organizations=true",
        endpoints.authorize_url,
        urlencode(GPT_CLIENT_ID),
        urlencode(&redirect),
        urlencode("openid profile email offline_access api.connectors.read api.connectors.invoke"),
        urlencode(&pkce.challenge),
        urlencode(&state),
    );
    GptLoginStart {
        url,
        redirect,
        verifier: pkce.verifier,
        state,
        callback_port,
    }
}

/// Exchanges the browser approval for tokens. The token exchange itself
/// carries no state (reference `exchangeToken`), but the callback must echo
/// this login's state.
pub async fn gpt_login_finish(
    endpoints: &FlowEndpoints,
    start: GptLoginStart,
    pasted: Option<String>,
) -> Result<Credential, String> {
    let callback = match pasted {
        Some(pasted) => pasted_callback(&pasted, &start.state)?,
        None => {
            await_callback(
                start.callback_port,
                &endpoints.callback_path,
                Duration::from_secs(300),
            )
            .await?
        }
    };
    if callback.state != start.state {
        return Err("state mismatch - possible CSRF attack".to_string());
    }
    let token = post_token_form(
        &token_client()?,
        &endpoints.token_url,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", GPT_CLIENT_ID),
            ("code", &callback.code),
            ("redirect_uri", &start.redirect),
            ("code_verifier", &start.verifier),
        ],
    )
    .await?;
    let mut credential = credential_from_token(&token, None)?;
    let id_token = token.get("id_token").and_then(Value::as_str).unwrap_or("");
    credential.account_id =
        chatgpt_account_id(id_token).or_else(|| chatgpt_account_id(&credential.access));
    credential.email = token
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            jwt_payload(id_token)?
                .get("email")?
                .as_str()
                .map(str::to_string)
        });
    Ok(credential)
}

/// Refreshes a ChatGPT OAuth credential.
pub async fn refresh_gpt(endpoints: &FlowEndpoints, refresh: &str) -> Result<Credential, String> {
    let client = token_client()?;
    let token = post_token_form(
        &client,
        &endpoints.token_url,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", GPT_CLIENT_ID),
            ("refresh_token", refresh),
        ],
    )
    .await?;
    credential_from_token(&token, Some(refresh))
}

fn generate_state() -> String {
    // The CSRF state must be unguessable: 16 bytes from the OS CSPRNG,
    // hex-encoded (reference `generateState`).
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| e.to_string())
        .expect("OS CSPRNG");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Muse Code device-approval state shown to the user.
pub struct MuseDevice {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: Duration,
    pub expires_at_ms: u64,
}

pub struct MuseEndpoints {
    pub device_authorization_url: String,
    pub device_token_url: String,
    pub key_url: String,
}

impl Default for MuseEndpoints {
    fn default() -> Self {
        Self {
            device_authorization_url: MUSE_DEVICE_AUTH_URL.to_string(),
            device_token_url: MUSE_DEVICE_TOKEN_URL.to_string(),
            key_url: MUSE_KEY_URL.to_string(),
        }
    }
}

/// Step 1: request a device/user code pair.
pub async fn muse_device_authorization(
    client: &reqwest::Client,
    endpoints: &MuseEndpoints,
) -> Result<MuseDevice, String> {
    let response = client
        .post(&endpoints.device_authorization_url)
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("x-api-version", MUSE_API_VERSION)
        .header("user-agent", MUSE_USER_AGENT)
        .body(format!("client_id={}", urlencode(MUSE_CLIENT_ID)))
        .send()
        .await
        .map_err(|e| format!("device authorization request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "device authorization returned HTTP {}",
            status.as_u16()
        ));
    }
    let bytes = read_success_body(response, "device authorization response").await?;
    let payload: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "device authorization response was not valid JSON".to_string())?;
    let device_code = payload
        .get("device_code")
        .and_then(Value::as_str)
        .ok_or("device authorization response is missing device_code")?
        .to_string();
    let user_code = payload
        .get("user_code")
        .and_then(Value::as_str)
        .ok_or("device authorization response is missing user_code")?
        .to_string();
    let verification_uri = payload
        .get("verification_uri")
        .and_then(Value::as_str)
        .unwrap_or("https://auth.meta.com/oauth/device/")
        .to_string();
    let interval = Duration::from_secs(
        payload
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .max(1),
    );
    let expires = payload
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(900);
    let now = now_ms();
    Ok(MuseDevice {
        device_code,
        user_code,
        verification_uri,
        interval,
        expires_at_ms: now.saturating_add(expires.saturating_mul(1000)),
    })
}

/// Step 2: poll until approval; returns the Meta account access token.
pub async fn muse_device_token(
    client: &reqwest::Client,
    endpoints: &MuseEndpoints,
    device: &MuseDevice,
) -> Result<String, String> {
    let mut interval = device.interval;
    loop {
        let response = client
            .post(&endpoints.device_token_url)
            .header("accept", "application/json")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("x-api-version", MUSE_API_VERSION)
            .header("user-agent", MUSE_USER_AGENT)
            .body(format!(
                "client_id={}&device_code={}&grant_type={}",
                urlencode(MUSE_CLIENT_ID),
                urlencode(&device.device_code),
                urlencode(MUSE_DEVICE_GRANT),
            ))
            .send()
            .await
            .map_err(|e| format!("device token poll failed: {e}"))?;
        let status = response.status();
        let max = if status.is_success() {
            SUCCESS_BODY_MAX_BYTES
        } else {
            crate::llm::ERROR_BODY_MAX_BYTES
        };
        let mut response = response;
        let bytes = crate::llm::read_capped(&mut response, max)
            .await
            .map_err(|e| format!("device token poll failed: {e}"))?
            .unwrap_or_default();
        let payload: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status.is_success() {
            return payload
                .get("access_token")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string())
                .ok_or_else(|| "device token response is missing the access token".to_string());
        }
        let code = payload
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match code.as_str() {
            "access_denied" => return Err("the login was denied in the browser".to_string()),
            "expired_token" => {
                return Err("the device code expired; start the login again".to_string())
            }
            "slow_down" => interval += Duration::from_secs(5),
            "authorization_pending" => {}
            other => {
                return Err(format!(
                    "device token poll failed: HTTP {} ({other})",
                    status.as_u16()
                ))
            }
        }
        if now_ms() >= device.expires_at_ms {
            return Err("the device authorization expired before approval".to_string());
        }
        tokio::time::sleep(interval.min(Duration::from_secs(60))).await;
    }
}

/// Step 3: mint the subscription key from the account token.
pub async fn muse_mint_key(
    client: &reqwest::Client,
    endpoints: &MuseEndpoints,
    account_token: &str,
) -> Result<Credential, String> {
    let response = client
        .post(&endpoints.key_url)
        .header("accept", "application/json")
        .header("user-agent", MUSE_USER_AGENT)
        .header("authorization", format!("Bearer {account_token}"))
        .header("content-type", "application/json")
        .header("x-api-version", MUSE_API_VERSION)
        .json(&json!({ "onboard": true }))
        .send()
        .await
        .map_err(|e| format!("the Muse key request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        // The body can contain the key itself; report the status only.
        return Err(format!(
            "the Muse key request returned HTTP {}",
            status.as_u16()
        ));
    }
    let bytes = read_success_body(response, "Muse key response").await?;
    let payload: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "the Muse key response was not valid JSON".to_string())?;
    let api_key = payload
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|value| value.starts_with("LLM|"))
        .ok_or("the Muse key response did not contain a usable api_key")?
        .to_string();
    Ok(Credential {
        access: String::new(),
        refresh: String::new(),
        expires_at_ms: None,
        account_id: payload
            .get("user_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        email: payload
            .get("user_email")
            .and_then(Value::as_str)
            .map(str::to_string),
        api_key: Some(api_key),
        oauth_access_token: Some(account_token.to_string()),
    })
}

/// Full Muse Code login: device approval, then the subscription key mint.
pub async fn login_muse(endpoints: &MuseEndpoints) -> Result<Credential, String> {
    // Every Muse login request is bounded (reference `REQUEST_TIMEOUT_MS`),
    // so a stalled endpoint cannot park the login indefinitely.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| format!("cannot build the Muse HTTP client: {e}"))?;
    let device = muse_device_authorization(&client, endpoints).await?;
    println!(
        "Open {} and approve the code: {}",
        device.verification_uri, device.user_code
    );
    let account_token = muse_device_token(&client, endpoints, &device).await?;
    muse_mint_key(&client, endpoints, &account_token).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_the_sha256_of_the_verifier() {
        let pkce = generate_pkce();
        assert_eq!(pkce.verifier.len(), 43); // 32 bytes, base64url unpadded
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()));
        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn code_fragment_splitting_matches_the_anthropic_code_mode() {
        let (code, state) = split_code_fragment("abc123#state456");
        assert_eq!(code, "abc123");
        assert_eq!(state.as_deref(), Some("state456"));
        let (code, state) = split_code_fragment("plain");
        assert_eq!(code, "plain");
        assert!(state.is_none());
    }

    #[test]
    fn pasted_code_is_trimmed_and_must_echo_the_login_state() {
        let callback = pasted_callback(
            "abc#expected
",
            "expected",
        )
        .expect("matching state");
        assert_eq!(callback.code, "abc");
        assert_eq!(callback.state, "expected");
        let callback = pasted_callback(
            "  bare-code
",
            "expected",
        )
        .expect("bare code");
        assert_eq!(callback.code, "bare-code");
        assert_eq!(callback.state, "expected");
        assert!(pasted_callback("abc#forged", "expected").is_err());
        assert!(pasted_callback(
            "
", "expected"
        )
        .is_err());
    }

    #[test]
    fn jwt_claims_yield_the_chatgpt_account_id() {
        // payload: {"https://api.openai.com/auth":{"chatgpt_account_id":"acc-1"}}
        let payload = r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc-1"}}"#;
        let encoded = URL_SAFE_NO_PAD.encode(payload.as_bytes());
        let token = format!("h.{encoded}.s");
        assert_eq!(chatgpt_account_id(&token).as_deref(), Some("acc-1"));
        assert_eq!(chatgpt_account_id("not-a-jwt"), None);
    }

    #[test]
    fn store_redacts_credentials_in_debug_output() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "inocode-store-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let mut store = Store::load(path.clone()).expect("load the test store");
        store
            .save(
                "claude-oauth",
                Credential {
                    access: "super-secret-access".to_string(),
                    refresh: "super-secret-refresh".to_string(),
                    expires_at_ms: Some(123),
                    account_id: Some("acc".to_string()),
                    email: Some("user@example.test".to_string()),
                    api_key: Some("LLM|secret".to_string()),
                    oauth_access_token: Some("meta-token".to_string()),
                },
            )
            .expect("save");
        let rendered = format!("{store:?}");
        assert!(!rendered.contains("super-secret"));
        assert!(!rendered.contains("LLM|secret"));
        assert!(rendered.contains("claude-oauth"));

        // A reload round-trips the values without leaking them in Debug.
        let reloaded = Store::load(path.clone()).expect("load the test store");
        let credential = reloaded.credential("claude-oauth").expect("entry");
        assert_eq!(credential.access, "super-secret-access");
        assert_eq!(credential.api_key.as_deref(), Some("LLM|secret"));
        std::fs::remove_file(&path).expect("remove the test store");
    }

    fn temp_store_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "inocode-store-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    fn sample_credential(access: &str) -> Credential {
        Credential {
            access: access.to_string(),
            refresh: String::new(),
            expires_at_ms: None,
            account_id: None,
            email: None,
            api_key: None,
            oauth_access_token: None,
        }
    }

    #[test]
    fn unreadable_store_is_an_error_and_is_never_overwritten() {
        let path = temp_store_path("corrupt");
        std::fs::write(&path, b"{not json").expect("write");
        assert!(Store::load(path.clone()).is_err());
        let mut store = Store {
            path: path.clone(),
            entries: BTreeMap::new(),
            info: BTreeMap::new(),
            vault: crate::secrets::Vault::new(path.clone(), crate::secrets::mode(), false),
        };
        assert!(store.save("gpt-oauth", sample_credential("a")).is_err());
        assert_eq!(std::fs::read(&path).expect("read"), b"{not json");
        std::fs::remove_file(&path).expect("remove");
    }

    #[test]
    fn saving_keeps_entries_written_after_this_store_was_loaded() {
        let path = temp_store_path("merge");
        let mut early = Store::load(path.clone()).expect("load");
        let mut other = Store::load(path.clone()).expect("load");
        other
            .save("claude-oauth", sample_credential("refreshed"))
            .expect("save");
        early
            .save("gpt-oauth", sample_credential("logged-in"))
            .expect("save");
        let reloaded = Store::load(path.clone()).expect("load");
        assert_eq!(
            reloaded
                .credential("claude-oauth")
                .map(|c| c.access.as_str()),
            Some("refreshed")
        );
        assert_eq!(
            reloaded.credential("gpt-oauth").map(|c| c.access.as_str()),
            Some("logged-in")
        );
        let leftovers = std::fs::read_dir(std::env::temp_dir())
            .expect("list")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_name().to_string_lossy().starts_with(&format!(
                    "{}.tmp-",
                    path.file_name().unwrap().to_string_lossy()
                ))
            })
            .count();
        assert_eq!(leftovers, 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_file(&path).expect("remove");
    }

    #[test]
    fn concurrent_saves_from_separate_stores_keep_every_entry() {
        let path = temp_store_path("concurrent");
        let writers: Vec<_> = (0..8)
            .map(|writer| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for round in 0..25 {
                        let mut store = Store::load(path.clone()).expect("load");
                        store
                            .save(
                                &format!("provider-{writer}"),
                                sample_credential(&format!("round-{round}")),
                            )
                            .expect("save");
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("writer thread");
        }
        let store = Store::load(path.clone()).expect("load");
        for writer in 0..8 {
            assert_eq!(
                store
                    .credential(&format!("provider-{writer}"))
                    .map(|c| c.access.as_str()),
                Some("round-24"),
                "provider-{writer}"
            );
        }
        std::fs::remove_file(&path).expect("remove");
        let _ = std::fs::remove_file(crate::secrets::sibling(&path, ".lock"));
    }

    #[test]
    fn concurrent_refreshes_spend_the_refresh_token_once() {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_time()
            .build()
            .expect("runtime")
            .block_on(concurrent_refreshes());
    }

    async fn concurrent_refreshes() {
        let path = temp_store_path("refresh");
        let mut store = Store::load(path.clone()).expect("load");
        store
            .save(
                "gpt-oauth",
                Credential {
                    access: "expired-access".to_string(),
                    refresh: "rt-1".to_string(),
                    expires_at_ms: Some(1),
                    account_id: Some("acct-1".to_string()),
                    email: None,
                    api_key: None,
                    oauth_access_token: None,
                },
            )
            .expect("save");
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                let calls = std::sync::Arc::clone(&calls);
                tokio::spawn(async move {
                    refresh_stored(path, "gpt-oauth", "default", None, |refresh| async move {
                        assert_eq!(refresh, "rt-1", "only the stored token is spent");
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        Ok(Credential {
                            access: "fresh-access".to_string(),
                            refresh: "rt-2".to_string(),
                            expires_at_ms: Some(u64::MAX),
                            account_id: None,
                            email: None,
                            api_key: None,
                            oauth_access_token: None,
                        })
                    })
                    .await
                    .expect("refresh")
                })
            })
            .collect();
        for task in tasks {
            let credential = task.await.expect("task");
            assert_eq!(credential.access, "fresh-access");
            // The token response carries no account id; the stored one stays.
            assert_eq!(credential.account_id.as_deref(), Some("acct-1"));
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let stored = Store::load(path.clone()).expect("load");
        assert_eq!(
            stored.credential("gpt-oauth").map(|c| c.refresh.as_str()),
            Some("rt-2")
        );
        std::fs::remove_file(&path).expect("remove");
        let _ = std::fs::remove_file(crate::secrets::sibling(&path, ".lock"));
        let _ = std::fs::remove_file(crate::secrets::sibling(&path, ".refresh.gpt-oauth.lock"));
    }

    fn person(email: Option<&str>, id: Option<&str>) -> Credential {
        Credential {
            email: email.map(str::to_string),
            account_id: id.map(str::to_string),
            ..sample_credential("a")
        }
    }

    #[test]
    fn identity_needs_one_shared_field_and_no_disagreeing_one() {
        let same = |a: Credential, b: Credential| same_identity(&a, &b);
        assert!(same(
            person(Some("A@Example.test"), None),
            person(Some(" a@example.test"), Some("x"))
        ));
        assert!(same(person(None, Some("acct")), person(None, Some("acct"))));
        // One email in two workspaces is two accounts.
        assert!(!same(
            person(Some("a@example.test"), Some("team")),
            person(Some("a@example.test"), Some("personal"))
        ));
        // Two people in one workspace are two accounts.
        assert!(!same(
            person(Some("a@example.test"), Some("team")),
            person(Some("b@example.test"), Some("team"))
        ));
        assert!(!same(person(None, None), person(None, None)));
        assert!(!same(
            person(Some("a@example.test"), None),
            person(None, Some("x"))
        ));
    }

    fn listed(id: &str, paused: bool, needs_reauth: bool) -> Account {
        Account {
            id: id.to_string(),
            info: crate::secrets::AccountInfo {
                paused,
                needs_reauth,
                ..Default::default()
            },
            email: None,
            account_id: None,
        }
    }

    #[test]
    fn the_active_account_or_the_only_usable_one_is_chosen() {
        let choose = |accounts: &[Account], active: Option<&str>| {
            choose_account("claude-oauth", accounts, active).map_err(|error| error.code)
        };
        assert_eq!(choose(&[], None), Err("provider_config"));
        let one = [listed("default", false, false)];
        assert_eq!(choose(&one, None), Ok("default".to_string()));
        assert_eq!(choose(&one, Some("other")), Err("account_unknown"));
        let two = [listed("default", false, false), listed("b", false, false)];
        assert_eq!(choose(&two, None), Err("account_ambiguous"));
        assert_eq!(choose(&two, Some("b")), Ok("b".to_string()));
        let one_usable = [listed("default", true, false), listed("b", false, false)];
        assert_eq!(choose(&one_usable, None), Ok("b".to_string()));
        assert_eq!(choose(&one_usable, Some("default")), Err("account_paused"));
        let refused = [listed("default", false, true)];
        assert_eq!(choose(&refused, None), Err("account_needs_reauth"));
        let none_usable = [listed("default", true, false), listed("b", false, true)];
        assert_eq!(choose(&none_usable, None), Err("account_unavailable"));
    }

    #[test]
    fn sign_ins_add_or_update_accounts_and_their_info_persists() {
        let path = temp_store_path("accounts");
        let mut store = Store::load(path.clone()).expect("load");
        let first = store
            .add_signed_in("claude-oauth", person(Some("a@example.test"), Some("u-a")))
            .expect("first");
        assert_eq!(
            first,
            SignedIn {
                account: "default".into(),
                added: true
            }
        );
        let other = store
            .add_signed_in("claude-oauth", person(Some("b@example.test"), Some("u-b")))
            .expect("second");
        assert!(other.added);
        assert_ne!(other.account, "default");
        store
            .edit_info("claude-oauth", "default", |info| info.needs_reauth = true)
            .expect("mark");
        let again = store
            .add_signed_in(
                "claude-oauth",
                Credential {
                    access: "renewed".into(),
                    ..person(Some("A@example.test"), None)
                },
            )
            .expect("again");
        assert_eq!(
            again,
            SignedIn {
                account: "default".into(),
                added: false
            }
        );
        store
            .edit_info("claude-oauth", &other.account, |info| {
                info.alias = Some("work".into())
            })
            .expect("alias");
        let reloaded = Store::load(path.clone()).expect("reload");
        let accounts = reloaded.accounts("claude-oauth");
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].id, "default", "the earliest first");
        assert!(!accounts[0].info.needs_reauth, "a new sign-in clears it");
        assert_eq!(accounts[0].account_id.as_deref(), Some("u-a"), "kept");
        assert!(accounts[0].info.added_ms.is_some());
        assert_eq!(accounts[1].info.alias.as_deref(), Some("work"));
        assert_eq!(
            reloaded
                .account_credential("claude-oauth", "default")
                .map(|c| c.access.as_str()),
            Some("renewed")
        );
        let mut store = reloaded;
        store
            .remove_account("claude-oauth", &other.account)
            .expect("remove");
        assert!(store
            .remove_account("claude-oauth", &other.account)
            .is_err());
        assert!(store.edit_info("claude-oauth", "nobody", |_| {}).is_err());
        let reloaded = Store::load(path.clone()).expect("reload");
        assert_eq!(reloaded.accounts("claude-oauth").len(), 1);
        let file = std::fs::read_to_string(&path).expect("file");
        assert!(!file.contains("work"), "the removed account's info is gone");
        std::fs::remove_file(&path).expect("remove");
        let _ = std::fs::remove_file(crate::secrets::sibling(&path, ".lock"));
    }

    #[test]
    fn only_a_refusal_of_the_refresh_token_asks_for_a_new_sign_in() {
        assert!(refresh_refused("token request returned HTTP 400"));
        assert!(refresh_refused("token request returned HTTP 401"));
        assert!(!refresh_refused("token request returned HTTP 429"));
        assert!(!refresh_refused("token request returned HTTP 503"));
        assert!(!refresh_refused("token request failed: connection refused"));
    }

    #[test]
    fn urldecode_and_urlencode_round_trip() {
        assert_eq!(urldecode("a%20b%2Fc+d"), "a b/c d");
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
    }
}
