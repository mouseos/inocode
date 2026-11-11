//! The runtime pool: one `AuthManager` per account, a policy, and the switching rules.
//!
//! Callers (the model provider and the turn loop) only ever ask three things:
//! which `AuthManager` to use right now ([`AccountPool::active_auth_manager`]), whether to move
//! on before the next sampling ([`AccountPool::select_for_sampling`]), and what to do after a
//! usage limit ([`AccountPool::report_limit_reached`]).

use super::provider::AccountProvider;
use super::provider::OpenAiApiKeyProvider;
use super::provider::OpenAiChatgptProvider;
use super::select::Candidate;
use super::select::SelectInput;
use super::select::select;
use super::store::AccountStore;
use super::store::DEFAULT_ID;
use super::store::api_key_account_id;
use super::types::AccountKind;
use super::types::AccountMeta;
use super::types::AccountPolicy;
use super::types::AccountState;
use super::types::AccountUsage;
use super::types::AccountView;
use super::types::PoolEvent;
use super::types::SwitchEvent;
use super::types::SwitchReason;
use crate::AuthCredentialsStoreMode;
use crate::AuthKeyringBackendKind;
use crate::AuthManager;
use crate::AuthRouteConfig;
use crate::claude_oauth::ClaudeAuthManager;
use crate::claude_oauth::ClaudeCredentials;
use crate::claude_oauth::claude_account_id;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::RwLock;
use std::sync::Weak;
use std::time::Duration;
use tokio::sync::broadcast;

/// What a Claude request authenticates with.
#[derive(Clone)]
pub enum ClaudeCredential {
    Oauth(Arc<ClaudeAuthManager>),
    ApiKey(Arc<AuthManager>),
}

/// Where and how the pool builds its per-account `AuthManager`s.
#[derive(Clone, Debug)]
pub struct PoolEnv {
    pub codex_home: PathBuf,
    pub mode: AuthCredentialsStoreMode,
    pub keyring: AuthKeyringBackendKind,
    pub chatgpt_base_url: Option<String>,
    pub auth_route_config: AuthRouteConfig,
}

struct Entry {
    meta: AccountMeta,
    manager: Arc<AuthManager>,
    /// Set for Claude OAuth entries (`manager` is then an unused placeholder).
    claude: Option<Arc<ClaudeAuthManager>>,
    /// Percentage points one sampling typically adds.
    pace: f64,
    is_default: bool,
}

struct Inner {
    entries: Vec<Entry>,
    policy: AccountPolicy,
    current: Option<String>,
    pinned: Option<String>,
}

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub struct AccountPool {
    env: PoolEnv,
    group: String,
    store: AccountStore,
    primary: Option<Arc<AuthManager>>,
    inner: Mutex<Inner>,
    providers: RwLock<HashMap<AccountKind, Arc<dyn AccountProvider>>>,
    events: broadcast::Sender<PoolEvent>,
    clock: Clock,
}

fn system_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn make_manager(env: &PoolEnv, dir: PathBuf) -> Arc<AuthManager> {
    Arc::new(
        AuthManager::new(
            dir,
            /*enable_codex_api_key_env*/ false,
            env.mode,
            /*forced_chatgpt_workspace_id*/ None,
            env.chatgpt_base_url.clone(),
            env.keyring,
            env.auth_route_config.clone(),
        )
        .await,
    )
}

fn io_err(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}

impl Inner {
    fn position(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.meta.id == id)
    }

    fn candidates(&self) -> Vec<Candidate> {
        self.entries
            .iter()
            .map(|e| Candidate {
                id: e.meta.id.clone(),
                kind: e.meta.kind,
                enabled: e.meta.enabled,
                cooldown_until: e.meta.cooldown_until,
                priority: e.meta.priority,
                usage: e.meta.usage.clone(),
                pace: e.pace,
            })
            .collect()
    }

    fn alias_of(&self, id: &str) -> Option<String> {
        self.position(id).map(|i| self.entries[i].meta.alias.clone())
    }
}

impl AccountPool {
    /// Opens the pool stored under `env.codex_home`. `primary` is the already running manager of
    /// the legacy `auth.json` login; it stays the "default" entry.
    pub async fn open(env: PoolEnv, primary: Arc<AuthManager>) -> Arc<Self> {
        Self::open_with_clock(env, primary, Arc::new(system_now)).await
    }

    pub async fn open_with_clock(
        env: PoolEnv,
        primary: Arc<AuthManager>,
        clock: Clock,
    ) -> Arc<Self> {
        let store = AccountStore::new(&env.codex_home, env.mode, env.keyring);
        Self::open_inner(env, "openai", store, Some(primary), clock).await
    }

    /// Opens the pool of another provider group (`claude`) under `INOCODE_HOME/<dir_name>/`.
    /// There is no legacy default login in such a group.
    pub async fn open_group(env: PoolEnv, group: &str, dir_name: &str) -> Arc<Self> {
        let store = AccountStore::in_dir(&env.codex_home, dir_name, env.mode, env.keyring);
        Self::open_inner(env, group, store, None, Arc::new(system_now)).await
    }

    async fn open_inner(
        env: PoolEnv,
        group: &str,
        store: AccountStore,
        primary: Option<Arc<AuthManager>>,
        clock: Clock,
    ) -> Arc<Self> {
        let mut entries = Vec::new();
        for id in store.ids() {
            let Some(mut meta) = store.load_meta(&id) else {
                continue;
            };
            meta.provider = group.to_string();
            let manager = make_manager(&env, store.account_dir(&id)).await;
            let claude = (meta.kind == AccountKind::ClaudeOauth).then(|| {
                Arc::new(ClaudeAuthManager::open(
                    store.account_dir(&id),
                    env.auth_route_config.clone(),
                ))
            });
            entries.push(Entry {
                meta,
                manager,
                claude,
                pace: 0.0,
                is_default: false,
            });
        }
        let (events, _) = broadcast::channel(64);
        let mut providers: HashMap<AccountKind, Arc<dyn AccountProvider>> = HashMap::new();
        providers.insert(
            AccountKind::Chatgpt,
            Arc::new(OpenAiChatgptProvider::default()),
        );
        providers.insert(AccountKind::ApiKey, Arc::new(OpenAiApiKeyProvider));
        let policy = store.load_policy();
        let pinned = store
            .load_pinned()
            .filter(|id| entries.iter().any(|e| &e.meta.id == id) || id == DEFAULT_ID);
        let pool = Arc::new(Self {
            env,
            group: group.to_string(),
            store,
            primary,
            inner: Mutex::new(Inner {
                entries,
                policy,
                current: None,
                pinned,
            }),
            providers: RwLock::new(providers),
            events,
            clock,
        });
        pool.sync_default();
        pool
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PoolEvent> {
        self.events.subscribe()
    }

    fn emit(&self, event: PoolEvent) {
        // No receiver is fine: notifications are best effort.
        let _ = self.events.send(event);
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    pub fn env(&self) -> &PoolEnv {
        &self.env
    }

    pub fn store(&self) -> &AccountStore {
        &self.store
    }

    /// Replaces the provider for one kind (for example to give ChatGPT accounts a usage reader).
    pub fn register_provider(&self, provider: Arc<dyn AccountProvider>) {
        self.providers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(provider.kind(), provider);
    }

    fn provider(&self, kind: AccountKind) -> Option<Arc<dyn AccountProvider>> {
        self.providers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&kind)
            .cloned()
    }

    /// Keeps the "default" entry in step with the legacy login: present while `auth.json` has a
    /// login, gone after it was removed.
    fn sync_default(&self) {
        let Some(primary) = self.primary.as_ref() else {
            return;
        };
        let auth = primary.auth_cached();
        let mut inner = lock(&self.inner);
        let position = inner.position(DEFAULT_ID);
        match (auth, position) {
            (Some(auth), None) => {
                let kind = if auth.is_api_key_auth() {
                    AccountKind::ApiKey
                } else {
                    AccountKind::Chatgpt
                };
                let stored = self.store.load_meta(DEFAULT_ID);
                let meta = stored.unwrap_or(AccountMeta {
                    id: DEFAULT_ID.to_string(),
                    kind,
                    alias: "既定".to_string(),
                    plan: None,
                    priority: 0,
                    enabled: true,
                    cooldown_until: None,
                    last_used_at: None,
                    usage: None,
                    added_at: self.now(),
                    provider: self.group.clone(),
                });
                let plan = self
                    .provider(kind)
                    .and_then(|p| p.identify(&auth))
                    .and_then(|identity| identity.plan);
                let mut meta = meta;
                meta.kind = kind;
                if plan.is_some() {
                    meta.plan = plan;
                }
                inner.entries.insert(
                    0,
                    Entry {
                        meta,
                        manager: Arc::clone(primary),
                        claude: None,
                        pace: 0.0,
                        is_default: true,
                    },
                );
            }
            (None, Some(index)) => {
                let removed = inner.entries.remove(index);
                if inner.current.as_deref() == Some(removed.meta.id.as_str()) {
                    inner.current = None;
                }
            }
            _ => {}
        }
    }

    // ----------------------------------------------------------------- policy

    pub fn policy(&self) -> AccountPolicy {
        lock(&self.inner).policy.clone()
    }

    pub fn set_policy(&self, policy: AccountPolicy) -> std::io::Result<AccountPolicy> {
        if !(0.0..=100.0).contains(&policy.threshold_percent) {
            return Err(io_err("thresholdPercent must be between 0 and 100"));
        }
        if policy.default_cooldown_seconds < 0 {
            return Err(io_err("defaultCooldownSeconds must not be negative"));
        }
        self.store.save_policy(&policy)?;
        lock(&self.inner).policy = policy.clone();
        self.emit(PoolEvent::Updated);
        Ok(policy)
    }

    // -------------------------------------------------------------- selection

    /// The manager requests should use right now. Cheap; selects on first use.
    pub fn active_auth_manager(&self) -> Option<Arc<AuthManager>> {
        self.sync_default();
        let mut inner = lock(&self.inner);
        if inner.current.is_none() {
            let now = self.now();
            let chosen = select(&SelectInput {
                candidates: &inner.candidates(),
                policy: &inner.policy,
                now,
                current: None,
                pinned: inner.pinned.as_deref(),
                rr_after: None,
                advance: true,
            });
            inner.current = chosen;
        }
        let id = inner.current.clone()?;
        let index = inner.position(&id)?;
        Some(Arc::clone(&inner.entries[index].manager))
    }

    pub fn current_id(&self) -> Option<String> {
        lock(&self.inner).current.clone()
    }

    /// Runs before every sampling request. Moves to another account when the current one is
    /// about to cross the threshold (or is no longer usable). `turn_start` lets round robin advance.
    pub fn select_for_sampling(
        &self,
        thread_id: Option<&str>,
        turn_start: bool,
    ) -> Option<SwitchEvent> {
        self.sync_default();
        let now = self.now();
        let event = {
            let mut inner = lock(&self.inner);
            let candidates = inner.candidates();
            let chosen = select(&SelectInput {
                candidates: &candidates,
                policy: &inner.policy,
                now,
                current: inner.current.as_deref(),
                pinned: inner.pinned.as_deref(),
                rr_after: None,
                advance: turn_start,
            });
            let Some(chosen) = chosen else {
                return None;
            };
            let previous = inner.current.clone();
            if previous.as_deref() == Some(chosen.as_str()) {
                return None;
            }
            let reason = previous.as_deref().and_then(|prev| {
                let old = candidates.iter().find(|c| c.id == prev)?;
                if !old.enabled {
                    None
                } else if !old.usable(now) {
                    Some(SwitchReason::LimitReached)
                } else if old.over_threshold(&inner.policy, now) {
                    Some(SwitchReason::Threshold)
                } else {
                    None
                }
            });
            let from_alias = previous.as_deref().and_then(|prev| inner.alias_of(prev));
            let to_alias = inner.alias_of(&chosen)?;
            inner.current = Some(chosen.clone());
            if let Some(index) = inner.position(&chosen) {
                inner.entries[index].meta.last_used_at = Some(now);
            }
            reason.map(|reason| SwitchEvent {
                from_alias,
                to_alias,
                reason,
                thread_id: thread_id.map(str::to_string),
            })
        };
        if let Some(event) = event.clone() {
            self.emit(PoolEvent::Switched(event));
            self.emit(PoolEvent::Updated);
        }
        event
    }

    /// The current account hit a usage limit (`UsageLimitReached`, `QuotaExceeded`, 429).
    /// It cools down until `resets_at` (or the policy default) and another account is chosen.
    /// `Some` means "retry the same request with the new account".
    pub fn report_limit_reached(
        &self,
        thread_id: Option<&str>,
        resets_at: Option<i64>,
    ) -> Option<SwitchEvent> {
        let now = self.now();
        let event = {
            let mut inner = lock(&self.inner);
            let current = inner.current.clone()?;
            let index = inner.position(&current)?;
            let cooldown = match resets_at {
                Some(at) if at > now => at,
                _ => now + inner.policy.default_cooldown_seconds,
            };
            inner.entries[index].meta.cooldown_until = Some(cooldown);
            let meta = inner.entries[index].meta.clone();
            let _ = self.store.save_meta(&meta);
            if inner.pinned.as_deref() == Some(current.as_str()) {
                inner.pinned = None;
                let _ = self.store.save_pinned(None);
            }
            if !inner.policy.auto_switch {
                drop(inner);
                self.emit(PoolEvent::Updated);
                return None;
            }
            let candidates = inner.candidates();
            let chosen = select(&SelectInput {
                candidates: &candidates,
                policy: &inner.policy,
                now,
                current: None,
                pinned: None,
                rr_after: Some(current.as_str()),
                advance: false,
            });
            match chosen {
                Some(chosen) if chosen != current => {
                    let from_alias = inner.alias_of(&current);
                    let to_alias = inner.alias_of(&chosen)?;
                    inner.current = Some(chosen.clone());
                    if let Some(index) = inner.position(&chosen) {
                        inner.entries[index].meta.last_used_at = Some(now);
                    }
                    Some(SwitchEvent {
                        from_alias,
                        to_alias,
                        reason: SwitchReason::LimitReached,
                        thread_id: thread_id.map(str::to_string),
                    })
                }
                _ => None,
            }
        };
        if let Some(event) = event.clone() {
            self.emit(PoolEvent::Switched(event));
        }
        self.emit(PoolEvent::Updated);
        event
    }

    /// Manual switch; it holds until that account cools down or is disabled.
    pub fn use_account(
        &self,
        id: &str,
        thread_id: Option<&str>,
    ) -> Result<SwitchEvent, std::io::Error> {
        self.sync_default();
        let now = self.now();
        let event = {
            let mut inner = lock(&self.inner);
            let index = inner
                .position(id)
                .ok_or_else(|| io_err("unknown account"))?;
            if !inner.entries[index].meta.enabled {
                return Err(io_err("the account is disabled"));
            }
            // Choosing an account explicitly overrides its cooldown.
            inner.entries[index].meta.cooldown_until = None;
            inner.entries[index].meta.last_used_at = Some(now);
            let meta = inner.entries[index].meta.clone();
            let _ = self.store.save_meta(&meta);
            let from_alias = inner.current.as_deref().and_then(|c| inner.alias_of(c));
            inner.current = Some(id.to_string());
            inner.pinned = Some(id.to_string());
            let _ = self.store.save_pinned(Some(id));
            SwitchEvent {
                from_alias,
                to_alias: meta.alias,
                reason: SwitchReason::Manual,
                thread_id: thread_id.map(str::to_string),
            }
        };
        self.emit(PoolEvent::Switched(event.clone()));
        self.emit(PoolEvent::Updated);
        Ok(event)
    }

    // ------------------------------------------------------------------ usage

    /// Records a fresh usage reading. `observed_while_in_use` marks readings that follow a
    /// sampling on that account, which is what teaches the pool its pace.
    pub fn record_usage(&self, id: &str, usage: AccountUsage, observed_while_in_use: bool) {
        let now = self.now();
        {
            let mut inner = lock(&self.inner);
            let window = inner.policy.window;
            let Some(index) = inner.position(id) else {
                return;
            };
            let entry = &mut inner.entries[index];
            let before = entry
                .meta
                .usage
                .as_ref()
                .and_then(|u| u.effective_percent(window, now));
            let after = usage.effective_percent(window, now);
            if observed_while_in_use && let (Some(before), Some(after)) = (before, after) {
                let delta = after - before;
                if delta > 0.0 {
                    entry.pace = 0.5 * entry.pace + 0.5 * delta;
                } else if delta == 0.0 {
                    entry.pace *= 0.9;
                }
            }
            entry.meta.usage = Some(usage);
            let meta = entry.meta.clone();
            let _ = self.store.save_meta(&meta);
        }
        self.emit(PoolEvent::Updated);
    }

    /// Usage of whichever account is in use (rate-limit headers arrive for that one).
    pub fn record_usage_for_current(&self, usage: AccountUsage) {
        if let Some(id) = self.current_id() {
            self.record_usage(&id, usage, /*observed_while_in_use*/ true);
        }
    }

    /// Asks every enabled account's provider for its usage.
    pub async fn refresh_usage(&self) {
        self.sync_default();
        let targets: Vec<(String, AccountKind, Arc<AuthManager>)> = {
            let inner = lock(&self.inner);
            inner
                .entries
                .iter()
                .filter(|e| e.meta.enabled)
                .map(|e| (e.meta.id.clone(), e.meta.kind, Arc::clone(&e.manager)))
                .collect()
        };
        for (id, kind, manager) in targets {
            let Some(provider) = self.provider(kind) else {
                continue;
            };
            let Some(auth) = manager.auth().await else {
                continue;
            };
            let plan = provider.identify(&auth).and_then(|identity| identity.plan);
            match provider.fetch_usage(auth).await {
                Ok(Some(usage)) => {
                    self.record_usage(&id, usage, /*observed_while_in_use*/ false);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!("account usage refresh failed for {id}: {error}");
                }
            }
            if let Some(plan) = plan {
                let mut inner = lock(&self.inner);
                if let Some(index) = inner.position(&id)
                    && inner.entries[index].meta.plan.as_deref() != Some(plan.as_str())
                {
                    inner.entries[index].meta.plan = Some(plan);
                    let meta = inner.entries[index].meta.clone();
                    let _ = self.store.save_meta(&meta);
                }
            }
        }
    }

    /// Refreshes usage now and then, for as long as the pool is alive.
    pub fn spawn_refresh_loop(self: &Arc<Self>, every: Duration) -> tokio::task::JoinHandle<()> {
        let pool: Weak<Self> = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(strong) = pool.upgrade() else { return };
                strong.refresh_usage().await;
                drop(strong);
                tokio::time::sleep(every).await;
            }
        })
    }

    // ------------------------------------------------------------------ views

    pub fn list(&self) -> Vec<AccountView> {
        self.sync_default();
        let now = self.now();
        let inner = lock(&self.inner);
        inner
            .entries
            .iter()
            .map(|e| {
                let cooling = e.meta.cooldown_until.is_some_and(|until| until > now);
                AccountView {
                    id: e.meta.id.clone(),
                    kind: e.meta.kind,
                    alias: e.meta.alias.clone(),
                    plan: e.meta.plan.clone(),
                    usage: e.meta.usage.clone(),
                    state: if !e.meta.enabled {
                        AccountState::Disabled
                    } else if cooling {
                        AccountState::Cooldown
                    } else {
                        AccountState::Active
                    },
                    cooldown_until: e.meta.cooldown_until.filter(|until| *until > now),
                    priority: e.meta.priority,
                    last_used_at: e.meta.last_used_at,
                    in_use: inner.current.as_deref() == Some(e.meta.id.as_str()),
                    is_default: e.is_default,
                    provider: self.group.clone(),
                }
            })
            .collect()
    }

    // ------------------------------------------------------------- management

    pub(crate) fn mutate<R>(
        &self,
        id: &str,
        change: impl FnOnce(&mut AccountMeta) -> R,
    ) -> Result<R, std::io::Error> {
        self.sync_default();
        let (result, meta) = {
            let mut inner = lock(&self.inner);
            let index = inner
                .position(id)
                .ok_or_else(|| io_err("unknown account"))?;
            let result = change(&mut inner.entries[index].meta);
            (result, inner.entries[index].meta.clone())
        };
        self.store.save_meta(&meta)?;
        self.emit(PoolEvent::Updated);
        Ok(result)
    }

    pub fn rename(&self, id: &str, alias: &str) -> Result<(), std::io::Error> {
        let alias = alias.trim();
        if alias.is_empty() {
            return Err(io_err("alias must not be empty"));
        }
        self.mutate(id, |meta| meta.alias = alias.to_string())
    }

    pub fn set_priority(&self, id: &str, priority: i32) -> Result<(), std::io::Error> {
        self.mutate(id, |meta| meta.priority = priority)
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), std::io::Error> {
        self.mutate(id, |meta| meta.enabled = enabled)?;
        let mut inner = lock(&self.inner);
        if !enabled {
            if inner.current.as_deref() == Some(id) {
                inner.current = None;
            }
            if inner.pinned.as_deref() == Some(id) {
                inner.pinned = None;
                let _ = self.store.save_pinned(None);
            }
        }
        Ok(())
    }

    pub async fn add_api_key(
        &self,
        alias: &str,
        api_key: &str,
        priority: Option<i32>,
    ) -> Result<AccountView, std::io::Error> {
        let alias = alias.trim();
        if alias.is_empty() || api_key.trim().is_empty() {
            return Err(io_err("alias and apiKey must not be empty"));
        }
        let id = api_key_account_id(api_key.trim());
        if lock(&self.inner).position(&id).is_some() {
            return Err(io_err("this API key is already in the pool"));
        }
        self.store.save_api_key(&id, api_key.trim())?;
        let meta = AccountMeta {
            id: id.clone(),
            kind: AccountKind::ApiKey,
            alias: alias.to_string(),
            plan: None,
            priority: priority.unwrap_or(0),
            enabled: true,
            cooldown_until: None,
            last_used_at: None,
            usage: None,
            added_at: self.now(),
            provider: self.group.clone(),
        };
        self.store.save_meta(&meta)?;
        let manager = make_manager(&self.env, self.store.account_dir(&id)).await;
        lock(&self.inner).entries.push(Entry {
            meta,
            manager,
            claude: None,
            pace: 0.0,
            is_default: false,
        });
        self.emit(PoolEvent::Updated);
        self.view(&id)
    }

    /// Registers the ChatGPT login that finished in the pending directory of `login_id`.
    pub async fn finish_chatgpt_login(
        &self,
        login_id: &str,
        alias: &str,
    ) -> Result<AccountView, std::io::Error> {
        let alias = alias.trim();
        if alias.is_empty() {
            return Err(io_err("alias must not be empty"));
        }
        let pending = make_manager(&self.env, self.store.pending_dir(login_id)).await;
        let auth = pending
            .auth()
            .await
            .ok_or_else(|| io_err("the sign-in did not produce a login"))?;
        let provider = self
            .provider(AccountKind::Chatgpt)
            .ok_or_else(|| io_err("no provider for ChatGPT accounts"))?;
        let identity = provider
            .identify(&auth)
            .ok_or_else(|| io_err("the sign-in carries no account identity"))?;
        drop(pending);
        self.sync_default();
        let duplicate_of_default = self
            .primary
            .as_ref()
            .and_then(|primary| primary.auth_cached())
            .and_then(|auth| provider.identify(&auth))
            .is_some_and(|other| other.id == identity.id);
        if duplicate_of_default {
            let _ = std::fs::remove_dir_all(self.store.pending_dir(login_id));
            return Err(io_err("this account is already the default login"));
        }
        let existing = lock(&self.inner).position(&identity.id).is_some();
        let dir = self.store.adopt_pending(login_id, &identity.id)?;
        let manager = make_manager(&self.env, dir).await;
        if existing {
            let mut inner = lock(&self.inner);
            if let Some(index) = inner.position(&identity.id) {
                inner.entries[index].manager = manager;
                inner.entries[index].meta.plan = identity.plan.clone();
                inner.entries[index].meta.cooldown_until = None;
                let meta = inner.entries[index].meta.clone();
                drop(inner);
                self.store.save_meta(&meta)?;
            }
        } else {
            let meta = AccountMeta {
                id: identity.id.clone(),
                kind: AccountKind::Chatgpt,
                alias: alias.to_string(),
                plan: identity.plan.clone(),
                priority: 0,
                enabled: true,
                cooldown_until: None,
                last_used_at: None,
                usage: None,
                added_at: self.now(),
                provider: self.group.clone(),
            };
            self.store.save_meta(&meta)?;
            lock(&self.inner).entries.push(Entry {
                meta,
                manager,
                claude: None,
                pace: 0.0,
                is_default: false,
            });
        }
        self.emit(PoolEvent::Updated);
        self.view(&identity.id)
    }

    pub fn discard_pending_login(&self, login_id: &str) {
        let _ = std::fs::remove_dir_all(self.store.pending_dir(login_id));
    }

    /// Removes an entry and its stored credentials. Removing the default entry logs the legacy
    /// `auth.json` out, as `account/logout` does.
    pub async fn remove(&self, id: &str) -> Result<(), std::io::Error> {
        self.sync_default();
        let manager = {
            let inner = lock(&self.inner);
            let index = inner
                .position(id)
                .ok_or_else(|| io_err("unknown account"))?;
            Arc::clone(&inner.entries[index].manager)
        };
        if id == DEFAULT_ID {
            manager.logout().await?;
            let _ = self.store.remove(DEFAULT_ID);
        } else {
            self.store.remove(id)?;
        }
        {
            let mut inner = lock(&self.inner);
            if let Some(index) = inner.position(id) {
                inner.entries.remove(index);
            }
            if inner.current.as_deref() == Some(id) {
                inner.current = None;
            }
            if inner.pinned.as_deref() == Some(id) {
                inner.pinned = None;
                let _ = self.store.save_pinned(None);
            }
        }
        self.emit(PoolEvent::Updated);
        Ok(())
    }

    pub fn group(&self) -> &str {
        &self.group
    }

    /// Registers a finished Claude sign-in as a pool entry.
    pub async fn add_claude_oauth(
        &self,
        alias: &str,
        credentials: ClaudeCredentials,
    ) -> Result<AccountView, std::io::Error> {
        let alias = alias.trim();
        if alias.is_empty() {
            return Err(io_err("alias must not be empty"));
        }
        let seed = credentials
            .account_uuid
            .clone()
            .unwrap_or_else(|| credentials.refresh.clone());
        let id = claude_account_id(&seed);
        let dir = self.store.account_dir(&id);
        let existing = lock(&self.inner).position(&id).is_some();
        let claude = Arc::new(ClaudeAuthManager::create(
            dir.clone(),
            self.env.auth_route_config.clone(),
            credentials.clone(),
        )?);
        let manager = make_manager(&self.env, dir).await;
        if existing {
            let mut inner = lock(&self.inner);
            if let Some(index) = inner.position(&id) {
                inner.entries[index].claude = Some(claude);
                inner.entries[index].meta.plan.clone_from(&credentials.plan);
                inner.entries[index].meta.cooldown_until = None;
                let meta = inner.entries[index].meta.clone();
                drop(inner);
                self.store.save_meta(&meta)?;
            }
        } else {
            let meta = AccountMeta {
                id: id.clone(),
                kind: AccountKind::ClaudeOauth,
                alias: alias.to_string(),
                plan: credentials.plan.clone(),
                priority: 0,
                enabled: true,
                cooldown_until: None,
                last_used_at: None,
                usage: None,
                added_at: self.now(),
                provider: self.group.clone(),
            };
            self.store.save_meta(&meta)?;
            lock(&self.inner).entries.push(Entry {
                meta,
                manager,
                claude: Some(claude),
                pace: 0.0,
                is_default: false,
            });
        }
        self.emit(PoolEvent::Updated);
        self.view(&id)
    }

    /// The Claude sign-in requests should use right now (selects on first use).
    pub fn active_claude_manager(&self) -> Option<Arc<ClaudeAuthManager>> {
        match self.active_claude_credential()? {
            ClaudeCredential::Oauth(manager) => Some(manager),
            ClaudeCredential::ApiKey(_) => None,
        }
    }

    /// The credential of the Claude entry in use: an OAuth sign-in or an API key.
    pub fn active_claude_credential(&self) -> Option<ClaudeCredential> {
        let mut inner = lock(&self.inner);
        if inner.current.is_none() {
            let now = self.now();
            let chosen = select(&SelectInput {
                candidates: &inner.candidates(),
                policy: &inner.policy,
                now,
                current: None,
                pinned: inner.pinned.as_deref(),
                rr_after: None,
                advance: true,
            });
            inner.current = chosen;
        }
        let id = inner.current.clone()?;
        let index = inner.position(&id)?;
        let entry = &inner.entries[index];
        match (&entry.claude, entry.meta.kind) {
            (Some(claude), _) => Some(ClaudeCredential::Oauth(Arc::clone(claude))),
            (None, AccountKind::ApiKey) => Some(ClaudeCredential::ApiKey(Arc::clone(&entry.manager))),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn insert_for_test(&self, meta: AccountMeta, manager: Arc<AuthManager>) {
        lock(&self.inner).entries.push(Entry {
            meta,
            manager,
            claude: None,
            pace: 0.0,
            is_default: false,
        });
    }

    fn view(&self, id: &str) -> Result<AccountView, std::io::Error> {
        self.list()
            .into_iter()
            .find(|v| v.id == id)
            .ok_or_else(|| io_err("unknown account"))
    }
}
