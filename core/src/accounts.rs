//! The accounts of the sign-in providers: listing, naming, pausing,
//! removing and choosing them, and sign-ins started over the protocol.
//! Decisions: docs/evidence/account-pool.md.
//!
//! A sign-in in progress is kept only in this process's memory. It lives
//! as long as the person needs to approve it in a browser or type a code
//! (at most [`LOGIN_WAIT`]), and what it waits for (a callback listener on
//! a local port, a PKCE verifier, a device code) does not outlive the
//! process anyway: after a restart the person starts it again.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{oneshot, watch};

use crate::deepai::SignInEndpoints;
use crate::oauth::{self, Account, Credential, FlowEndpoints, MuseEndpoints, SignedIn, Store};
use crate::settings::{self, Kind};
use crate::{pool, Core, ErrorInfo};

/// The endpoints sign-ins use; the defaults are the production hosts.
pub struct LoginEndpoints {
    pub claude: FlowEndpoints,
    pub gpt: FlowEndpoints,
    pub muse: MuseEndpoints,
    pub deepai: SignInEndpoints,
}

impl Default for LoginEndpoints {
    fn default() -> Self {
        Self {
            claude: FlowEndpoints::anthropic(),
            gpt: FlowEndpoints::chatgpt(),
            muse: MuseEndpoints::default(),
            deepai: SignInEndpoints::default(),
        }
    }
}

/// How long a sign-in waits for the person: the default lifetime of a Muse
/// device code (`expires_in` 900 seconds). The browser callbacks give up
/// after 300 seconds on their own.
const LOGIN_WAIT: Duration = Duration::from_secs(15 * 60);
/// How long a sign-in is kept after it started, so its result can still
/// be read once it finished.
const LOGIN_KEPT: Duration = Duration::from_secs(25 * 60);
/// How long `accounts/login/code` waits for the code's exchange.
const CODE_WAIT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
enum LoginState {
    Waiting,
    Done(SignedIn),
    Failed(String),
    Cancelled,
}

struct Login {
    kind: Kind,
    provider: Option<String>,
    started: Instant,
    /// What the person needs to approve it: a URL, or a device code.
    shown: Value,
    state: Arc<watch::Sender<LoginState>>,
    /// Takes the pasted code, in the modes that need one.
    code: Option<oneshot::Sender<String>>,
    task: tokio::task::AbortHandle,
}

/// The sign-ins in progress, by id.
pub(crate) struct Logins {
    entries: Mutex<HashMap<String, Login>>,
    endpoints: Arc<LoginEndpoints>,
}

impl Logins {
    pub(crate) fn new(endpoints: LoginEndpoints) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            endpoints: Arc::new(endpoints),
        }
    }

    /// The token endpoint that refreshes sign-ins of `kind`: the one its
    /// sign-ins use.
    pub(crate) fn token_url(&self, kind: Kind) -> Option<String> {
        match kind {
            Kind::ClaudeOauth => Some(self.endpoints.claude.token_url.clone()),
            Kind::GptOauth => Some(self.endpoints.gpt.token_url.clone()),
            _ => None,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Login>> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries.retain(|_, login| login.started.elapsed() < LOGIN_KEPT);
        entries
    }
}

/// What storing a finished sign-in did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completed {
    pub signed_in: SignedIn,
    /// Whether the settings gained a provider of the kind.
    pub provider_added: bool,
}

/// Stores the credential of a finished sign-in of `kind` as an account
/// ([`Store::add_signed_in`]) and gives the settings at `settings` a
/// provider of the kind when they have none. The CLI `login` and the
/// protocol's sign-ins both end here.
pub async fn complete_login(
    credentials: &Path,
    settings: Option<&Path>,
    kind: Kind,
    credential: Credential,
) -> Result<Completed, String> {
    let mut store = Store::load(credentials.to_path_buf())?;
    let signed_in = store.add_signed_in(kind.auth().store_name(), credential)?;
    let provider_added = match settings {
        Some(path) => settings::add_signed_in(path, kind).await?,
        None => false,
    };
    Ok(Completed {
        signed_in,
        provider_added,
    })
}

/// What a `provider` parameter names: the kind, the settings' provider if
/// it names one, and the credential store's name of the accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    kind: Kind,
    provider: Option<String>,
    store: String,
}

/// The accounts `name` means: a provider id of the settings first (a
/// static-key provider's accounts are its keys), then a sign-in kind name
/// (`claude-oauth`, ...), then the names the CLI `login` takes (`claude`,
/// `gpt`, `muse`).
fn resolve(settings: &settings::Settings, name: &str) -> Result<Target, ErrorInfo> {
    if let Some(provider) = settings.provider(name) {
        return Ok(Target {
            kind: provider.kind,
            provider: Some(name.to_string()),
            store: settings::account_store(name, provider.kind),
        });
    }
    let kind = match name.trim().to_ascii_lowercase().as_str() {
        "claude" => Some(Kind::ClaudeOauth),
        "gpt" => Some(Kind::GptOauth),
        "muse" => Some(Kind::MuseCode),
        other => Kind::parse(other).filter(|kind| !kind.uses_api_key()),
    };
    kind.map(|kind| Target {
        kind,
        provider: None,
        store: kind.auth().store_name().to_string(),
    })
    .ok_or_else(|| {
        ErrorInfo::new(
            "invalid_params",
            format!(
                "{name} is neither a provider of the settings nor one of \
                 claude-oauth, gpt-oauth, muse-code and deepai"
            ),
        )
    })
}

/// The account of `accounts` named `name`: its id, else its alias.
fn find(accounts: &[Account], target: &Target, name: &str) -> Result<String, ErrorInfo> {
    let by_alias = || {
        accounts.iter().find(|account| {
            account
                .info
                .alias
                .as_deref()
                .is_some_and(|alias| alias.eq_ignore_ascii_case(name.trim()))
        })
    };
    accounts
        .iter()
        .find(|account| account.id == name.trim())
        .or_else(by_alias)
        .map(|account| account.id.clone())
        .ok_or_else(|| {
            ErrorInfo::new(
                "account_unknown",
                format!(
                    "{} has no account {name}; /accounts list shows them",
                    target.provider.as_deref().unwrap_or(target.kind.name())
                ),
            )
        })
}
/// A new alias: 1 to 64 characters without control characters, not the
/// alias or id of another account of the kind, so it names one account.
fn check_alias(accounts: &[Account], account: &str, alias: &str) -> Result<(), ErrorInfo> {
    let invalid = |message: String| Err(ErrorInfo::new("invalid_params", message));
    let length = alias.chars().count();
    if !(1..=64).contains(&length) || alias.chars().any(char::is_control) {
        return invalid("an alias has 1 to 64 characters and no control characters".into());
    }
    let taken = accounts.iter().any(|other| {
        other.id != account
            && (other.id.eq_ignore_ascii_case(alias)
                || other
                    .info
                    .alias
                    .as_deref()
                    .is_some_and(|other| other.eq_ignore_ascii_case(alias)))
    });
    if taken {
        return invalid(format!("another account is already called {alias}"));
    }
    Ok(())
}

fn account_view(account: &Account, target: &Target, active: Option<&str>) -> Value {
    json!({
        "id": account.id,
        "alias": account.info.alias,
        "provider": target.provider,
        "kind": target.kind.name(),
        "paused": account.info.paused,
        "needs_reauth": account.info.needs_reauth,
        "added": account.info.added_ms,
        "email": account.email,
        "account_id": account.account_id,
        "plan": account.plan,
        "priority": account.info.priority.unwrap_or(0),
        "auto_switch_threshold": account.info.auto_switch_threshold,
        "credits": account.info.credits,
        "active": active == Some(account.id.as_str()),
    })
}

/// An integer field of `params` within `range`: `Some(None)` for `null`,
/// `None` when absent.
fn bounded(
    params: &Value,
    name: &str,
    range: std::ops::RangeInclusive<i64>,
) -> Result<Option<Option<i64>>, ErrorInfo> {
    match params.get(name) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(value) => value
            .as_i64()
            .filter(|value| range.contains(value))
            .map(|value| Some(Some(value)))
            .ok_or_else(|| {
                ErrorInfo::new(
                    "invalid_params",
                    format!(
                        "\"{name}\" must be an integer from {} to {}, or null",
                        range.start(),
                        range.end()
                    ),
                )
            }),
    }
}

fn string<'a>(params: &'a Value, name: &str) -> Result<&'a str, ErrorInfo> {
    params
        .get(name)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| ErrorInfo::new("invalid_params", format!("requires a \"{name}\" string")))
}

fn store_error(error: String) -> ErrorInfo {
    ErrorInfo::new("provider_config", error)
}

impl Core {
    /// Uses `endpoints` for the sign-ins started over the protocol instead
    /// of the production hosts.
    pub fn with_login_endpoints(mut self, endpoints: LoginEndpoints) -> Self {
        self.logins = Logins::new(endpoints);
        self
    }

    fn credentials_store(&self) -> Result<Store, ErrorInfo> {
        Store::load(self.credentials_path()?).map_err(store_error)
    }

    /// `accounts/list {provider?}`: the accounts (the keys of a static-key
    /// provider) of every provider of the settings, and of every sign-in
    /// kind with stored accounts that no provider uses (`provider: null`),
    /// or of one provider or kind. `active` marks the account the
    /// provider's requests use now.
    pub async fn accounts_list(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let settings = self.settings().await?;
        let store = self.credentials_store()?;
        let targets: Vec<Target> = match params.get("provider") {
            Some(Value::String(name)) => vec![resolve(&settings, name)?],
            None | Some(Value::Null) => {
                let mut targets: Vec<Target> = settings
                    .providers
                    .iter()
                    .flatten()
                    .map(|(id, provider)| Target {
                        kind: provider.kind,
                        provider: Some(id.clone()),
                        store: settings::account_store(id, provider.kind),
                    })
                    .collect();
                for kind in Kind::ALL.into_iter().filter(|kind| !kind.uses_api_key()) {
                    let used = targets.iter().any(|used| used.kind == kind);
                    if !used && !store.accounts(kind.auth().store_name()).is_empty() {
                        targets.push(Target {
                            kind,
                            provider: None,
                            store: kind.auth().store_name().to_string(),
                        });
                    }
                }
                targets
            }
            Some(_) => {
                return Err(ErrorInfo::new(
                    "invalid_params",
                    "\"provider\" must be a string",
                ))
            }
        };
        let mut views = Vec::new();
        for target in targets {
            let accounts = store.accounts(&target.store);
            let active = self.current_account(&settings, &target, &accounts).await;
            views.extend(
                accounts
                    .iter()
                    .map(|account| account_view(account, &target, active.as_deref())),
            );
        }
        Ok(json!({ "accounts": views }))
    }

    /// The account a new conversation on the target's provider would use
    /// now (the pool's choice); without a provider, the kind's only usable
    /// account.
    async fn current_account(
        &self,
        settings: &settings::Settings,
        target: &Target,
        accounts: &[Account],
    ) -> Option<String> {
        let Some((id, configured)) = target
            .provider
            .as_deref()
            .and_then(|id| settings.provider(id).map(|configured| (id, configured)))
        else {
            return oauth::choose_account(&target.store, accounts, None).ok();
        };
        let route = self.route(id, configured);
        if let Some(pinned) = &route.pinned {
            if !accounts.iter().any(|account| &account.id == pinned) {
                return None;
            }
        }
        let members: Vec<pool::Member> = accounts
            .iter()
            .filter(|account| !configured.kind.uses_api_key() || account.has_key)
            .map(pool::Member::of)
            .collect();
        let model = configured.default_model();
        self.pool
            .peek(&route.ctx(&members, model, pool::now_ms()))
            .await
    }

    /// What `params.provider` names, the account `params.account` names
    /// among its accounts, and the credential store.
    async fn named_account(&self, params: &Value) -> Result<(Target, String, Store), ErrorInfo> {
        let settings = self.settings().await?;
        let target = resolve(&settings, string(params, "provider")?)?;
        let store = self.credentials_store()?;
        let accounts = store.accounts(&target.store);
        let account = find(&accounts, &target, string(params, "account")?)?;
        Ok((target, account, store))
    }

    /// The view of one account after a change.
    async fn account_result(
        &self,
        target: &Target,
        account: &str,
        store: &Store,
    ) -> Result<Value, ErrorInfo> {
        let accounts = store.accounts(&target.store);
        let settings = self.settings().await?;
        let active = self.current_account(&settings, target, &accounts).await;
        let found = accounts
            .iter()
            .find(|candidate| candidate.id == account)
            .ok_or_else(|| ErrorInfo::new("internal", "the account was not kept"))?;
        Ok(json!({ "account": account_view(found, target, active.as_deref()) }))
    }

    /// `accounts/rename {provider, account, alias}`; a `null` or empty
    /// alias removes it.
    pub async fn rename_account(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let (target, account, mut store) = self.named_account(params).await?;
        let alias = match params.get("alias") {
            Some(Value::String(alias)) if !alias.trim().is_empty() => {
                let alias = alias.trim().to_string();
                check_alias(&store.accounts(&target.store), &account, &alias)?;
                Some(alias)
            }
            Some(Value::String(_)) | Some(Value::Null) => None,
            _ => {
                return Err(ErrorInfo::new(
                    "invalid_params",
                    "requires an \"alias\" string or null",
                ))
            }
        };
        store
            .edit_info(&target.store, &account, |info| info.alias = alias)
            .map_err(store_error)?;
        self.account_result(&target, &account, &store).await
    }

    /// `accounts/pause {provider, account, paused}`.
    pub async fn pause_account(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let paused = params
            .get("paused")
            .and_then(Value::as_bool)
            .ok_or_else(|| ErrorInfo::new("invalid_params", "requires a \"paused\" boolean"))?;
        let (target, account, mut store) = self.named_account(params).await?;
        store
            .edit_info(&target.store, &account, |info| info.paused = paused)
            .map_err(store_error)?;
        self.account_result(&target, &account, &store).await
    }

    /// `accounts/remove {provider, account}`: the account (or key) and its
    /// secret, and any provider's choice of it.
    pub async fn remove_account(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let (target, account, mut store) = self.named_account(params).await?;
        store
            .remove_account(&target.store, &account)
            .map_err(store_error)?;
        self.pool.forget(&target.store, &account).await;
        if self.store.is_some() {
            settings::forget_account(&self.writable_settings().await?, &target.store, &account)
                .await
                .map_err(|error| ErrorInfo::new("settings_invalid", error))?;
        }
        Ok(json!({ "removed": account, "kind": target.kind.name() }))
    }

    /// `accounts/add_key {provider, key, alias?}`: stores another API key of
    /// the static-key provider `provider` as an account of its pool, or
    /// replaces the key of the account `alias` names. Without `alias` it is
    /// the provider's default key, as `inocode-server key set` stores it.
    pub async fn add_key(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let settings = self.settings().await?;
        let target = resolve(&settings, string(params, "provider")?)?;
        if !target.kind.uses_api_key() || target.provider.is_none() {
            return Err(ErrorInfo::new(
                "invalid_params",
                format!(
                    "{} signs in instead of using an API key; use accounts/login/start",
                    target.kind.name()
                ),
            ));
        }
        let key = string(params, "key")?.trim().to_string();
        let alias = match params.get("alias") {
            None | Some(Value::Null) => None,
            Some(Value::String(alias)) if !alias.trim().is_empty() => Some(alias.trim()),
            Some(_) => {
                return Err(ErrorInfo::new(
                    "invalid_params",
                    "\"alias\" must be a non-empty string or null",
                ))
            }
        };
        let mut store = self.credentials_store()?;
        let accounts = store.accounts(&target.store);
        if let Some(alias) = alias {
            if find(&accounts, &target, alias).is_err() {
                check_alias(&accounts, "", alias)?;
            }
        }
        let stored = store
            .set_key(&target.store, alias, &key)
            .map_err(store_error)?;
        let mut view = self
            .account_result(&target, &stored.account, &store)
            .await?;
        view["added"] = json!(stored.added);
        Ok(view)
    }

    /// `accounts/priority {provider, account, priority?, auto_switch_threshold?,
    /// credits?}`: the account's pool tier (-100 to 100), its own usage
    /// threshold (0 to 100), and whether it may serve from ChatGPT credits
    /// once a window is full; `null` returns any to the default.
    pub async fn set_account_priority(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let priority = bounded(
            params,
            "priority",
            i64::from(pool::MIN_PRIORITY)..=i64::from(pool::MAX_PRIORITY),
        )?;
        let threshold = bounded(params, "auto_switch_threshold", 0..=100)?;
        let credits = match params.get("credits") {
            None => None,
            Some(Value::Null) => Some(false),
            Some(Value::Bool(credits)) => Some(*credits),
            Some(_) => {
                return Err(ErrorInfo::new(
                    "invalid_params",
                    "\"credits\" must be a boolean or null",
                ))
            }
        };
        if priority.is_none() && threshold.is_none() && credits.is_none() {
            return Err(ErrorInfo::new(
                "invalid_params",
                "requires \"priority\", \"auto_switch_threshold\" or \"credits\"",
            ));
        }
        let (target, account, mut store) = self.named_account(params).await?;
        store
            .edit_info(&target.store, &account, |info| {
                if let Some(priority) = priority {
                    info.priority = priority.map(|p| p as i32).filter(|p| *p != 0);
                }
                if let Some(threshold) = threshold {
                    info.auto_switch_threshold = threshold.map(|t| t as u32);
                }
                if let Some(credits) = credits {
                    info.credits = credits;
                }
            })
            .map_err(store_error)?;
        self.account_result(&target, &account, &store).await
    }

    /// `accounts/clear_cooldown {provider, account}`: the pool may use the
    /// account again at once after a quota refusal.
    pub async fn clear_account_cooldown(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let (target, account, _) = self.named_account(params).await?;
        let cleared = self.pool.clear_cooldown(&target.store, &account).await;
        Ok(json!({ "account": account, "cleared": cleared }))
    }

    /// The provider of the settings `params.provider` names.
    async fn pool_provider(
        &self,
        params: &Value,
    ) -> Result<(settings::Settings, String), ErrorInfo> {
        let settings = self.settings().await?;
        let name = string(params, "provider")?;
        match resolve(&settings, name)?.provider {
            Some(id) => Ok((settings, id)),
            None => Err(ErrorInfo::new(
                "invalid_params",
                format!(
                    "{name} is not a provider of the settings; the pool belongs to a provider \
                     (providers/list shows them)"
                ),
            )),
        }
    }

    /// `pool/get {provider}`: the provider's `pool` as written and in
    /// effect.
    pub async fn pool_get(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let (settings, id) = self.pool_provider(params).await?;
        pool_view(&settings, &id)
    }

    /// `pool/set {provider, <pool field>?...}`; `null` returns a field to
    /// its default.
    pub async fn pool_set(&self, params: &Value) -> Result<Value, ErrorInfo> {
        const FIELDS: [&str; 9] = [
            "strategy",
            "sticky_limit",
            "auto_switch_threshold",
            "failover_threshold",
            "cache_affinity",
            "excluded_plans",
            "start_idle_windows",
            "priority_failback",
            "host_circuit_threshold",
        ];
        let (settings, id) = self.pool_provider(params).await?;
        let fields = params.as_object().into_iter().flatten();
        let mut written = settings
            .provider(&id)
            .and_then(|provider| serde_json::to_value(&provider.pool).ok())
            .and_then(|pool| pool.as_object().cloned())
            .unwrap_or_default();
        let mut changed = false;
        for (name, value) in fields.filter(|(name, _)| *name != "provider") {
            if !FIELDS.contains(&name.as_str()) {
                return Err(ErrorInfo::new(
                    "invalid_params",
                    format!(
                        "unknown pool field {name:?}; the fields are {}",
                        FIELDS.join(", ")
                    ),
                ));
            }
            changed = true;
            if value.is_null() {
                written.remove(name);
            } else {
                written.insert(name.clone(), value.clone());
            }
        }
        if !changed {
            return Err(ErrorInfo::new(
                "invalid_params",
                format!("requires one of {}", FIELDS.join(", ")),
            ));
        }
        let pool = match written.is_empty() {
            true => Value::Null,
            false => Value::Object(written),
        };
        settings::update_provider(
            &self.writable_settings().await?,
            &id,
            &json!({ "pool": pool }),
        )
        .await
        .map_err(|error| ErrorInfo::new("invalid_params", error))?;
        pool_view(&self.settings().await?, &id)
    }

    /// `pool/reset_affinity {provider?, thread_id?}`: threads choose their
    /// account again on their next request.
    pub async fn pool_reset_affinity(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let provider = match params.get("provider") {
            None | Some(Value::Null) => None,
            Some(_) => Some(self.pool_provider(params).await?.1),
        };
        let thread = match params.get("thread_id") {
            None | Some(Value::Null) => None,
            Some(value) => Some(crate::store::SessionId(value.as_i64().ok_or_else(
                || ErrorInfo::new("invalid_params", "\"thread_id\" must be an integer"),
            )?)),
        };
        let cleared = self.pool.reset_affinity(provider.as_deref(), thread).await;
        Ok(json!({ "cleared": cleared }))
    }

    /// `pool/status {provider?}`: per provider of the settings, the pool's
    /// settings, its host's reachability, and per account (or key) its
    /// usage score, cooldowns, why it is not used, the threads bound to it
    /// and its last error, seen from a request of the provider's default
    /// model.
    pub async fn pool_status(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let settings = self.settings().await?;
        let ids: Vec<String> = match params.get("provider") {
            None | Some(Value::Null) => settings
                .providers
                .iter()
                .flatten()
                .map(|(id, _)| id.clone())
                .collect(),
            Some(_) => vec![self.pool_provider(params).await?.1],
        };
        let store = self.credentials_store()?;
        let mut providers = Vec::new();
        for id in ids {
            let Some(configured) = settings.provider(&id) else {
                continue;
            };
            let route = self.route(&id, configured);
            let accounts: Vec<Account> = store
                .accounts(&route.kind)
                .into_iter()
                .filter(|account| !configured.kind.uses_api_key() || account.has_key)
                .collect();
            let members: Vec<pool::Member> = accounts.iter().map(pool::Member::of).collect();
            let ctx = route.ctx(&members, configured.default_model(), pool::now_ms());
            providers.push(self.pool.status(&ctx, &accounts, &route.host).await);
        }
        Ok(json!({ "providers": providers }))
    }

    /// `accounts/use {provider, account}`: the provider's requests prefer
    /// the account from the next one on, while it can serve them;
    /// `account: null` leaves the choice to the pool.
    pub async fn use_account(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let settings = self.settings().await?;
        let target = resolve(&settings, string(params, "provider")?)?;
        let provider = target.provider.clone().ok_or_else(|| {
            ErrorInfo::new(
                "invalid_params",
                "accounts/use needs the id of a provider of the settings; providers/list shows them",
            )
        })?;
        let account = match params.get("account") {
            Some(Value::Null) => None,
            _ => {
                let store = self.credentials_store()?;
                let accounts = store.accounts(&target.store);
                Some(find(&accounts, &target, string(params, "account")?)?)
            }
        };
        settings::update_provider(
            &self.writable_settings().await?,
            &provider,
            &json!({ "active_account": account }),
        )
        .await
        .map_err(|error| ErrorInfo::new("invalid_params", error))?;
        Ok(json!({ "provider": provider, "active_account": account }))
    }
    /// `accounts/login/start {provider, mode?}`: starts a sign-in and
    /// returns what the person must do. `mode` is `callback` (the browser
    /// returns to a local port; the default for Claude, ChatGPT and DeepAI),
    /// `code` (Claude only: the person pastes the code Anthropic shows,
    /// with `accounts/login/code`) or `device` (Muse Code, its only mode:
    /// the person enters a code at Meta's page).
    pub async fn login_start(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let settings = self.settings().await?;
        let Target { kind, provider, .. } = resolve(&settings, string(params, "provider")?)?;
        if kind.uses_api_key() {
            return Err(ErrorInfo::new(
                "invalid_params",
                format!(
                    "the provider {} uses API keys; store one with accounts/add_key \
                     or `inocode-server key set`",
                    provider.as_deref().unwrap_or(kind.name())
                ),
            ));
        }
        let mode = match params.get("mode") {
            None | Some(Value::Null) => None,
            Some(Value::String(mode)) => Some(mode.as_str()),
            Some(_) => {
                return Err(ErrorInfo::new(
                    "invalid_params",
                    "\"mode\" must be a string",
                ))
            }
        };
        let endpoints = Arc::clone(&self.logins.endpoints);
        let (code_sender, code) = oneshot::channel::<String>();
        let mut takes_code = false;
        let refuse = |modes: &str| {
            Err(ErrorInfo::new(
                "invalid_params",
                format!("{} signs in with the mode {modes}", kind.name()),
            ))
        };
        type Flow =
            std::pin::Pin<Box<dyn std::future::Future<Output = Result<Credential, String>> + Send>>;
        let (shown, flow): (Value, Flow) = match (kind, mode) {
            (Kind::ClaudeOauth, None | Some("callback" | "code")) => {
                takes_code = mode == Some("code");
                let start = oauth::claude_login_start(&endpoints.claude, takes_code);
                let shown = json!({
                    "mode": if takes_code { "code" } else { "callback" },
                    "url": start.url,
                });
                let flow = async move {
                    let pasted = match takes_code {
                        true => Some(code.await.map_err(|_| "no code was given".to_string())?),
                        false => None,
                    };
                    oauth::claude_login_finish(&endpoints.claude, start, pasted).await
                };
                (shown, Box::pin(flow))
            }
            (Kind::ClaudeOauth, _) => return refuse("callback or code"),
            (Kind::GptOauth, None | Some("callback")) => {
                let start = oauth::gpt_login_start(&endpoints.gpt);
                let shown = json!({ "mode": "callback", "url": start.url });
                let flow =
                    async move { oauth::gpt_login_finish(&endpoints.gpt, start, None).await };
                (shown, Box::pin(flow))
            }
            (Kind::GptOauth, _) => return refuse("callback"),
            (Kind::DeepAi, None | Some("callback")) => {
                let start = crate::deepai::sign_in_start(&endpoints.deepai);
                let shown = json!({ "mode": "callback", "url": start.url });
                let flow =
                    async move { crate::deepai::sign_in_finish(&endpoints.deepai, start).await };
                (shown, Box::pin(flow))
            }
            (Kind::DeepAi, _) => return refuse("callback"),
            (Kind::MuseCode, None | Some("device")) => {
                // Every Muse request is bounded, as in `oauth::login_muse`.
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(20))
                    .build()
                    .map_err(|error| ErrorInfo::new("internal", error.to_string()))?;
                let device = oauth::muse_device_authorization(&client, &endpoints.muse)
                    .await
                    .map_err(|error| ErrorInfo::new("provider_http", error))?;
                let shown = json!({
                    "mode": "device",
                    "url": device.verification_uri,
                    "user_code": device.user_code,
                });
                let flow = async move {
                    let token = oauth::muse_device_token(&client, &endpoints.muse, &device).await?;
                    oauth::muse_mint_key(&client, &endpoints.muse, &token).await
                };
                (shown, Box::pin(flow))
            }
            (Kind::MuseCode, _) => return refuse("device"),
            (_, _) => return refuse("of no sign-in"),
        };
        let credentials = self.credentials_path()?;
        let settings_path = match self.store {
            Some(_) => Some(self.settings_path()?),
            None => None,
        };
        let state = Arc::new(watch::channel(LoginState::Waiting).0);
        let reported = Arc::clone(&state);
        let task = tokio::spawn(async move {
            let credential = match tokio::time::timeout(LOGIN_WAIT, flow).await {
                Ok(result) => result,
                Err(_) => Err("the sign-in was not finished in time".to_string()),
            };
            let finished = match credential {
                Ok(credential) => {
                    complete_login(&credentials, settings_path.as_deref(), kind, credential)
                        .await
                        .map(|completed| completed.signed_in)
                }
                Err(error) => Err(error),
            };
            reported.send_replace(match finished {
                Ok(signed_in) => LoginState::Done(signed_in),
                Err(error) => LoginState::Failed(error),
            });
        });
        let id = login_id();
        let login = Login {
            kind,
            provider,
            started: Instant::now(),
            shown,
            state,
            code: takes_code.then_some(code_sender),
            task: task.abort_handle(),
        };
        let view = login_view(&id, &login);
        self.logins.lock().insert(id, login);
        Ok(view)
    }

    /// `accounts/login/code {login_id, code}`: gives a sign-in in the
    /// `code` mode the code the person pasted, and waits for its exchange.
    pub async fn login_code(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let id = string(params, "login_id")?;
        let code = string(params, "code")?.to_string();
        let mut state = {
            let mut logins = self.logins.lock();
            let login = logins.get_mut(id).ok_or_else(unknown_login)?;
            let sender = login.code.take().ok_or_else(|| {
                ErrorInfo::new(
                    "invalid_params",
                    "this sign-in takes no code, or already has one",
                )
            })?;
            let _ = sender.send(code);
            login.state.subscribe()
        };
        let _ = tokio::time::timeout(
            CODE_WAIT,
            state.wait_for(|state| !matches!(state, LoginState::Waiting)),
        )
        .await;
        self.login_status(params).await
    }

    /// `accounts/login/status {login_id}`.
    pub async fn login_status(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let id = string(params, "login_id")?;
        let logins = self.logins.lock();
        let login = logins.get(id).ok_or_else(unknown_login)?;
        Ok(login_view(id, login))
    }

    /// `accounts/login/cancel {login_id}`: stops a sign-in that has not
    /// finished, and its callback listener with it.
    pub async fn login_cancel(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let id = string(params, "login_id")?;
        let logins = self.logins.lock();
        let login = logins.get(id).ok_or_else(unknown_login)?;
        login.task.abort();
        login.state.send_if_modified(|state| {
            let waiting = matches!(state, LoginState::Waiting);
            if waiting {
                *state = LoginState::Cancelled;
            }
            waiting
        });
        Ok(login_view(id, login))
    }
}

/// The `pool` of the provider `id` as written and in effect.
fn pool_view(settings: &settings::Settings, id: &str) -> Result<Value, ErrorInfo> {
    let provider = settings
        .provider(id)
        .ok_or_else(|| ErrorInfo::new("invalid_params", format!("no provider has the id {id}")))?;
    Ok(json!({
        "provider": id,
        "kind": provider.kind.name(),
        "pool": provider.pool,
        "effective": pool::Rules::of(provider.pool.as_ref()).view(),
        "active_account": provider.active_account,
    }))
}

fn unknown_login() -> ErrorInfo {
    ErrorInfo::new(
        "unknown_login",
        "no sign-in has this id; it may have ended more than 25 minutes ago or before a restart",
    )
}

fn login_id() -> String {
    let mut bytes = [0u8; 12];
    getrandom::getrandom(&mut bytes).expect("OS CSPRNG");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn login_view(id: &str, login: &Login) -> Value {
    let mut view = json!({
        "login_id": id,
        "provider": login.provider,
        "kind": login.kind.name(),
        "expires_in": LOGIN_WAIT.saturating_sub(login.started.elapsed()).as_secs(),
    });
    if let (Some(view), Some(shown)) = (view.as_object_mut(), login.shown.as_object()) {
        view.extend(shown.clone());
        let (status, extra) = match &*login.state.borrow() {
            LoginState::Waiting => ("waiting", json!({})),
            LoginState::Done(signed_in) => (
                "done",
                json!({ "account": signed_in.account, "added": signed_in.added }),
            ),
            LoginState::Failed(error) => ("failed", json!({ "error": error })),
            LoginState::Cancelled => ("cancelled", json!({})),
        };
        view.insert("status".into(), json!(status));
        if let Some(extra) = extra.as_object() {
            view.extend(extra.clone());
        }
    }
    view
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::AccountInfo;

    fn account(id: &str, alias: Option<&str>) -> Account {
        Account {
            id: id.into(),
            info: AccountInfo {
                alias: alias.map(str::to_string),
                ..AccountInfo::default()
            },
            email: None,
            account_id: None,
            plan: None,
            has_key: false,
        }
    }

    fn claude() -> Target {
        Target {
            kind: Kind::ClaudeOauth,
            provider: None,
            store: "claude-oauth".into(),
        }
    }

    #[test]
    fn an_account_is_found_by_id_then_by_alias() {
        let accounts = [account("default", Some("work")), account("ab12cd34", None)];
        assert_eq!(find(&accounts, &claude(), "ab12cd34").unwrap(), "ab12cd34");
        assert_eq!(find(&accounts, &claude(), "Work").unwrap(), "default");
        assert_eq!(
            find(&accounts, &claude(), "home").unwrap_err().code,
            "account_unknown"
        );
    }

    #[test]
    fn an_alias_names_one_account() {
        let accounts = [account("default", Some("work")), account("ab12cd34", None)];
        assert!(check_alias(&accounts, "ab12cd34", "home").is_ok());
        assert!(check_alias(&accounts, "default", "work").is_ok(), "its own");
        assert!(check_alias(&accounts, "ab12cd34", "WORK").is_err());
        assert!(check_alias(&accounts, "ab12cd34", "default").is_err());
        assert!(check_alias(&accounts, "ab12cd34", &"x".repeat(65)).is_err());
        assert!(check_alias(&accounts, "ab12cd34", "a\nb").is_err());
    }

    #[test]
    fn names_resolve_to_a_provider_then_a_kind() {
        let settings = settings::Settings::from_value(&json!({
            "providers": {
                "work": {"kind": "claude-oauth"},
                "logfare": {"kind": "openai"}
            }
        }))
        .unwrap();
        assert_eq!(
            resolve(&settings, "work").unwrap(),
            Target {
                kind: Kind::ClaudeOauth,
                provider: Some("work".into()),
                store: "claude-oauth".into(),
            }
        );
        assert_eq!(resolve(&settings, "claude").unwrap(), claude());
        assert_eq!(resolve(&settings, "deepai").unwrap().kind, Kind::DeepAi);
        assert_eq!(
            resolve(&settings, "logfare").unwrap(),
            Target {
                kind: Kind::OpenAi,
                provider: Some("logfare".into()),
                store: "api-key/logfare".into(),
            },
            "a static-key provider's accounts are its keys"
        );
        assert!(resolve(&settings, "openai").is_err(), "no provider of it");
        assert!(resolve(&settings, "nothing").is_err());
    }
}
