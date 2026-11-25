//! The account pool: which stored account of a sign-in provider each
//! request uses when the provider has several, how it moves away from an
//! account that refuses on quota or keeps failing, and how a thread stays
//! on its account for the provider's prompt cache.
//!
//! The rules and numbers follow opencodex's Codex pool; where each comes
//! from, and what was left out, is in docs/evidence/account-pool.md.
//!
//! Health, usage and the round-robin cursor live in memory. Thread
//! bindings and quota cooldowns are also written to the database
//! (`store_pool`), so a restart keeps both.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::oauth::Account;
use crate::store::{PoolBinding, PoolCooldown, SessionId, Store};

pub const MAX_STICKY_LIMIT: u32 = 100;
pub const MAX_FAILOVER_THRESHOLD: u32 = 20;
pub const MAX_HOST_CIRCUIT_THRESHOLD: u32 = 20;
pub const MIN_PRIORITY: i32 = -100;
pub const MAX_PRIORITY: i32 = 100;

const DEFAULT_STICKY_LIMIT: u32 = 1;
const DEFAULT_AUTO_SWITCH_THRESHOLD: u32 = 80;
const DEFAULT_FAILOVER_THRESHOLD: u32 = 3;

const SECOND: u64 = 1_000;
const MINUTE: u64 = 60 * SECOND;
const HOUR: u64 = 60 * MINUTE;

const DEFAULT_QUOTA_COOLDOWN: u64 = MINUTE;
const MAX_QUOTA_COOLDOWN: u64 = 24 * HOUR;
/// A reset time announces when a window refreshes, not when to come back;
/// quota often frees up earlier, so a cooldown taken from it is short.
const MAX_RESET_COOLDOWN: u64 = 15 * MINUTE;
const MAX_QUOTA_AVOID: u64 = 6 * HOUR;
const PROBE_INTERVAL: u64 = 5 * MINUTE;
/// Failures further apart than this do not add up to a streak.
const STREAK_GAP: u64 = 5 * MINUTE;
const SOFT_AVOID_STEPS: [u64; 4] = [30 * SECOND, 2 * MINUTE, 10 * MINUTE, 30 * MINUTE];

const FAILURE_WINDOW: u64 = MINUTE;
const FAILURE_WINDOW_MIN_SAMPLES: usize = 20;
const FAILURE_WINDOW_MAX_SAMPLES: usize = 512;
const FAILURE_WINDOW_RECOVER_SUSTAIN: u64 = 30 * SECOND;

const AFFINITY_IDLE: u64 = 24 * HOUR;
const AFFINITY_REEVAL: u64 = MINUTE;
const TRANSIENT_HOLD: u64 = 10 * MINUTE;
const MAX_BINDINGS: usize = 2048;
/// Touching a binding is written at most this often.
const TOUCH_WRITE: u64 = MINUTE;

const USAGE_REFRESH: u64 = 10 * MINUTE;
/// How fresh usage must be for a bound thread to move back to a higher
/// tier, and how often it is read while that may happen.
const FAILBACK_REFRESH: u64 = 5 * MINUTE;

/// A static key's cooldown is at most this, and a refused key (401) waits
/// this long: an undated refusal of a key is a guess.
const MAX_KEY_COOLDOWN: u64 = 10 * MINUTE;
/// The cooldown of an account the provider refused for its subscription
/// or billing (an Anthropic 403 that names it).
const ACCOUNT_REFUSAL_COOLDOWN: u64 = 10 * MINUTE;
/// An Anthropic 429 that names no window is retried on the same account
/// after this.
const SHORT_RETRY: u64 = 100;
/// A rate pause up to this long is waited out on the same account.
const MAX_INLINE_THROTTLE: u64 = SECOND;
/// A model family's refusal keeps the account from that family at most
/// this long without a newer answer.
const FAMILY_REFUSAL_MAX_AGE: u64 = 30 * MINUTE;
/// How long an account's refusal of a model keeps it last for that model.
const MODEL_DENIAL: u64 = 6 * HOUR;

const IDLE_WINDOW_SECONDS: u64 = 5 * 60 * 60;
const IDLE_WINDOW_TOLERANCE: u64 = MINUTE;
const IDLE_WINDOW_FRESHNESS: u64 = 5 * MINUTE;

const MAX_SIBLINGS: usize = 8;

const HOST_FAILURE_WINDOW: u64 = 10 * MINUTE;
const HOST_CIRCUIT_COOLDOWN: u64 = 30 * SECOND;
const MAX_HOSTS: usize = 128;
/// ChatGPT credits read longer ago than this are not counted on.
const CREDITS_FRESHNESS: u64 = 5 * MINUTE;

/// The score of an account whose usage has not been seen: above every
/// measured one, so it never looks emptier than a measured account, and
/// never counts as over a threshold.
const UNKNOWN_SCORE: f64 = 101.0;
const EXHAUSTED: f64 = 100.0;
const TERMINAL_SHORT_FRESHNESS: u64 = 5 * MINUTE;
const WEEKLY_MIN_SECONDS: u64 = 24 * 60 * 60;
const MONTHLY_MIN_SECONDS: u64 = 28 * 24 * 60 * 60;
/// Below this a stored reset instant is in seconds, else milliseconds.
const SECONDS_LIMIT: u64 = 1_000_000_000_000;

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

tokio::task_local! {
    static THREAD: i64;
}

/// Runs `work` as requests of `thread`: the pool keeps a thread on its
/// account, so every model request made inside (the turn's and its helper
/// roles') is attributed to it. A request outside any thread has no
/// binding.
pub(crate) async fn in_thread<F: std::future::Future>(thread: SessionId, work: F) -> F::Output {
    THREAD.scope(thread.0, work).await
}

pub(crate) fn current_thread() -> Option<SessionId> {
    THREAD.try_with(|thread| SessionId(*thread)).ok()
}

/// How new conversations are placed on accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// The least used account, staying on it until it crosses its threshold.
    Quota,
    /// Each account in turn.
    RoundRobin,
    /// One account until it crosses its threshold, then the next.
    FillFirst,
    /// The account whose usage window resets first.
    ResetFirst,
}

impl Strategy {
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim() {
            "quota" => Some(Self::Quota),
            "round-robin" => Some(Self::RoundRobin),
            "fill-first" => Some(Self::FillFirst),
            "reset-first" => Some(Self::ResetFirst),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Quota => "quota",
            Self::RoundRobin => "round-robin",
            Self::FillFirst => "fill-first",
            Self::ResetFirst => "reset-first",
        }
    }
}

/// A provider's pool settings with the defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rules {
    pub strategy: Strategy,
    pub sticky_limit: u32,
    pub auto_switch_threshold: u32,
    pub failover_threshold: u32,
    pub cache_affinity: bool,
    /// Lowercase plan names the pool does not choose by itself.
    pub excluded_plans: Vec<String>,
    pub start_idle_windows: bool,
    pub priority_failback: bool,
    pub host_circuit_threshold: u32,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            strategy: Strategy::Quota,
            sticky_limit: DEFAULT_STICKY_LIMIT,
            auto_switch_threshold: DEFAULT_AUTO_SWITCH_THRESHOLD,
            failover_threshold: DEFAULT_FAILOVER_THRESHOLD,
            cache_affinity: true,
            excluded_plans: Vec::new(),
            start_idle_windows: false,
            priority_failback: false,
            host_circuit_threshold: 0,
        }
    }
}

impl Rules {
    pub fn of(config: Option<&crate::settings::PoolConfig>) -> Self {
        let defaults = Self::default();
        let Some(config) = config else {
            return defaults;
        };
        Self {
            strategy: config
                .strategy
                .as_deref()
                .and_then(Strategy::parse)
                .unwrap_or(defaults.strategy),
            sticky_limit: config.sticky_limit.unwrap_or(defaults.sticky_limit),
            auto_switch_threshold: config
                .auto_switch_threshold
                .unwrap_or(defaults.auto_switch_threshold),
            failover_threshold: config
                .failover_threshold
                .unwrap_or(defaults.failover_threshold),
            cache_affinity: config.cache_affinity.unwrap_or(defaults.cache_affinity),
            excluded_plans: config
                .excluded_plans
                .iter()
                .flatten()
                .map(|plan| plan.trim().to_lowercase())
                .collect(),
            start_idle_windows: config
                .start_idle_windows
                .unwrap_or(defaults.start_idle_windows),
            priority_failback: config
                .priority_failback
                .unwrap_or(defaults.priority_failback),
            host_circuit_threshold: config
                .host_circuit_threshold
                .unwrap_or(defaults.host_circuit_threshold),
        }
    }

    pub fn view(&self) -> Value {
        json!({
            "strategy": self.strategy.name(),
            "sticky_limit": self.sticky_limit,
            "auto_switch_threshold": self.auto_switch_threshold,
            "failover_threshold": self.failover_threshold,
            "cache_affinity": self.cache_affinity,
            "excluded_plans": self.excluded_plans,
            "start_idle_windows": self.start_idle_windows,
            "priority_failback": self.priority_failback,
            "host_circuit_threshold": self.host_circuit_threshold,
        })
    }
}

/// What is known of an account's usage windows, in percent used.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Quota {
    pub short: Option<f64>,
    pub short_reset_ms: Option<u64>,
    pub short_observed_ms: Option<u64>,
    pub weekly: Option<f64>,
    pub weekly_reset_ms: Option<u64>,
    pub monthly: Option<f64>,
    pub monthly_reset_ms: Option<u64>,
    /// The short window's stated length, when known.
    pub short_seconds: Option<u64>,
    /// Windows of one model family (Anthropic `7d_oi`: Fable), percent used.
    pub families: std::collections::BTreeMap<String, f64>,
    /// The plan the usage read names.
    pub plan: Option<String>,
    /// The ChatGPT credits and when they were read.
    pub credits: Option<(crate::usage::Credits, u64)>,
    pub updated_ms: u64,
}

impl Quota {
    fn is_empty(&self) -> bool {
        self.short.is_none()
            && self.weekly.is_none()
            && self.monthly.is_none()
            && self.families.is_empty()
            && self.credits.is_none()
    }

    /// Takes the windows `newer` reports; the others keep what was known.
    fn merge(&mut self, newer: &Quota) {
        if newer.short.is_some() {
            self.short = newer.short;
            self.short_reset_ms = newer.short_reset_ms;
            self.short_observed_ms = newer.short_observed_ms;
            self.short_seconds = newer.short_seconds.or(self.short_seconds);
        }
        for (family, percent) in &newer.families {
            self.families.insert(family.clone(), *percent);
        }
        if newer.plan.is_some() {
            self.plan = newer.plan.clone();
        }
        if newer.credits.is_some() {
            self.credits = newer.credits.clone();
        }
        if newer.weekly.is_some() {
            self.weekly = newer.weekly;
            self.weekly_reset_ms = newer.weekly_reset_ms;
        }
        if newer.monthly.is_some() {
            self.monthly = newer.monthly;
            self.monthly_reset_ms = newer.monthly_reset_ms;
        }
        self.updated_ms = self.updated_ms.max(newer.updated_ms);
    }

    fn short_elapsed(&self, now: u64) -> bool {
        self.short_reset_ms.is_some_and(|reset| reset <= now)
    }

    /// A full short window that is known to be current: the account cannot
    /// serve now, whatever its long windows say.
    fn short_spent(&self, now: u64) -> bool {
        if self.short.is_none_or(|short| short < EXHAUSTED) {
            return false;
        }
        match self.short_reset_ms {
            Some(reset) => reset > now,
            None => self
                .short_observed_ms
                .is_some_and(|seen| now >= seen && now - seen <= TERMINAL_SHORT_FRESHNESS),
        }
    }

    /// The highest of the long windows and, while it runs, the short one.
    /// The short window alone is no measure of the account; with no long
    /// window known the score is unknown, unless the short one is spent.
    pub(crate) fn score(&self, now: u64) -> f64 {
        let long: Vec<f64> = [self.weekly, self.monthly].into_iter().flatten().collect();
        if long.is_empty() {
            return if self.short_spent(now) {
                EXHAUSTED
            } else {
                UNKNOWN_SCORE
            };
        }
        let short = self.short.filter(|_| !self.short_elapsed(now));
        long.into_iter().chain(short).fold(0.0, f64::max)
    }

    /// The soonest future reset of the short and weekly windows.
    fn next_reset(&self, now: u64) -> Option<u64> {
        [self.short_reset_ms, self.weekly_reset_ms]
            .into_iter()
            .flatten()
            .filter(|reset| *reset > now)
            .min()
    }

    /// [`Self::score`] for a request of the model family `family`: its own
    /// window counts too (reference `anthropicModelPercents`).
    fn score_for(&self, now: u64, family: Option<&str>) -> f64 {
        let score = self.score(now);
        match family.and_then(|family| self.families.get(family)) {
            Some(percent) if score >= UNKNOWN_SCORE => {
                if *percent >= EXHAUSTED {
                    EXHAUSTED
                } else {
                    score
                }
            }
            Some(percent) => score.max(*percent),
            None => score,
        }
    }

    /// A window that is full now: the short one while it runs, or a long
    /// one whose reset is still ahead. A long window without a reset is not
    /// trusted to say when it ends (reference `codexUsageLimitResetAt`).
    fn limit_reached(&self, now: u64) -> bool {
        self.short_spent(now)
            || [
                (self.weekly, self.weekly_reset_ms),
                (self.monthly, self.monthly_reset_ms),
            ]
            .into_iter()
            .any(|(percent, reset)| {
                percent.is_some_and(|p| p >= EXHAUSTED) && reset.is_some_and(|r| r > now)
            })
    }

    /// Fresh credits the provider spends once a window is full (reference
    /// `hasSpendableCodexCredits`).
    fn spendable_credits(&self, now: u64) -> bool {
        let Some((credits, read)) = &self.credits else {
            return false;
        };
        if now < *read || now - read > CREDITS_FRESHNESS {
            return false;
        }
        if credits.overage_limit_reached == Some(true) || credits.has_credits == Some(false) {
            return false;
        }
        credits.unlimited == Some(true)
            || (credits.has_credits == Some(true) && credits.balance.is_some_and(|b| b > 0.0))
    }

    fn view(&self) -> Value {
        json!({
            "short_percent": self.short,
            "short_resets_at": self.short_reset_ms,
            "weekly_percent": self.weekly,
            "weekly_resets_at": self.weekly_reset_ms,
            "monthly_percent": self.monthly,
            "monthly_resets_at": self.monthly_reset_ms,
            "families": self.families,
            "plan": self.plan,
            "updated": self.updated_ms,
        })
    }
}

fn number(text: &str) -> Option<f64> {
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

fn percent(text: Option<&str>) -> Option<f64> {
    number(text?).map(|value| value.clamp(0.0, 100.0))
}

/// A reset instant in milliseconds: epoch seconds, epoch milliseconds, or
/// an RFC 3339 date.
pub(crate) fn reset_ms(text: &str) -> Option<u64> {
    if let Some(value) = number(text) {
        if value <= 0.0 {
            return None;
        }
        let value = value as u64;
        return Some(if value < SECONDS_LIMIT {
            value * 1_000
        } else {
            value
        });
    }
    chrono::DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .and_then(|date| u64::try_from(date.timestamp_millis()).ok())
}

/// One response header's value as text.
pub(crate) type Header<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The Codex `x-codex-{primary,secondary,tertiary}-*` headers. A primary
/// window that declares a duration under a day is the short window and the
/// secondary is the week; one of 28 days or more is the month.
pub(crate) fn codex_headers(header: Header<'_>, now: u64) -> Option<Quota> {
    let field = |window: &str, name: &str| header(&format!("x-codex-{window}-{name}"));
    let primary = percent(field("primary", "used-percent").as_deref());
    let secondary = percent(field("secondary", "used-percent").as_deref());
    let tertiary = percent(field("tertiary", "used-percent").as_deref());
    let reset = |window: &str| field(window, "reset-at").as_deref().and_then(reset_ms);
    let minutes = field("primary", "window-minutes")
        .as_deref()
        .and_then(number)
        .filter(|minutes| *minutes > 0.0);
    let mut quota = Quota {
        updated_ms: now,
        ..Quota::default()
    };
    let seconds = minutes.map(|minutes| (minutes * 60.0) as u64);
    match seconds {
        Some(seconds) if seconds >= MONTHLY_MIN_SECONDS => {
            quota.monthly = primary;
            quota.monthly_reset_ms = primary.and(reset("primary"));
            quota.weekly = secondary;
            quota.weekly_reset_ms = secondary.and(reset("secondary"));
        }
        Some(seconds) if seconds < WEEKLY_MIN_SECONDS => {
            quota.short = primary;
            quota.short_reset_ms = primary.and(reset("primary"));
            quota.short_observed_ms = primary.map(|_| now);
            quota.short_seconds = primary.map(|_| seconds);
            quota.weekly = secondary;
            quota.weekly_reset_ms = secondary.and(reset("secondary"));
        }
        _ => {
            let (weekly, window) = match primary {
                Some(primary) => (Some(primary), "primary"),
                None => (secondary, "secondary"),
            };
            quota.weekly = weekly;
            quota.weekly_reset_ms = weekly.and(reset(window));
        }
    }
    if quota.monthly.is_none() && tertiary.is_some() {
        quota.monthly = tertiary;
        quota.monthly_reset_ms = reset("tertiary");
    }
    let flag = |name: &str| {
        header(&format!("x-codex-credits-{name}")).and_then(|value| value.trim().parse().ok())
    };
    let credits = crate::usage::Credits {
        has_credits: flag("has-credits"),
        unlimited: flag("unlimited"),
        overage_limit_reached: None,
        balance: header("x-codex-credits-balance")
            .as_deref()
            .and_then(number)
            .filter(|balance| *balance >= 0.0),
    };
    if credits != crate::usage::Credits::default() {
        quota.credits = Some((credits, now));
    }
    (!quota.is_empty()).then_some(quota)
}

/// The Anthropic model family a model belongs to, for the families with a
/// window of their own: `Fable` (reference `anthropicModelFamily`).
pub(crate) fn anthropic_family(model: &str) -> Option<&'static str> {
    let name = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    (name == "claude-fable-5" || name.starts_with("claude-fable-5-")).then_some("Fable")
}

/// The Anthropic `anthropic-ratelimit-unified-{5h,7d}-*` headers:
/// utilization as a fraction of 1, resets in epoch seconds.
pub(crate) fn anthropic_headers(header: Header<'_>, now: u64) -> Option<Quota> {
    let utilization = |window: &str| {
        header(&format!("anthropic-ratelimit-unified-{window}-utilization"))
            .as_deref()
            .and_then(number)
            .filter(|fraction| (0.0..=1.0).contains(fraction))
            .map(|fraction| (fraction * 10_000.0).round() / 100.0)
    };
    let reset = |window: &str| {
        header(&format!("anthropic-ratelimit-unified-{window}-reset"))
            .as_deref()
            .and_then(reset_ms)
    };
    let short = utilization("5h");
    let weekly = utilization("7d");
    let mut quota = Quota {
        short,
        short_reset_ms: short.and(reset("5h")),
        short_observed_ms: short.map(|_| now),
        weekly,
        weekly_reset_ms: weekly.and(reset("7d")),
        updated_ms: now,
        ..Quota::default()
    };
    // The Fable window: its observed overage reads 1.01, shown as 100
    // (reference `parseAnthropicFamilyHeaders`).
    let family = header("anthropic-ratelimit-unified-7d_oi-utilization")
        .as_deref()
        .and_then(number)
        .filter(|fraction| (0.0..=1.01).contains(fraction))
        .map(|fraction| ((fraction * 10_000.0).round() / 100.0).min(EXHAUSTED))
        .or_else(|| family_rejected(header).then_some(EXHAUSTED));
    if let Some(percent) = family {
        quota.families.insert("Fable".into(), percent);
    }
    (!quota.is_empty()).then_some(quota)
}

fn family_rejected(header: Header<'_>) -> bool {
    header("anthropic-ratelimit-unified-7d_oi-status").is_some_and(|s| s.trim() == "rejected")
}

/// How an Anthropic 429 limits the account (reference `classifyAnthropic429`):
/// a rejected 5-hour, weekly or unified window is the shared quota, a
/// rejected `7d_oi` window the Fable family's, a `Retry-After` or an
/// allowed unified status a short pause of the account, else unknown.
pub(crate) fn anthropic_429(header: Header<'_>, now: u64) -> Refusal {
    let rejected = |window: &str| {
        header(&format!("anthropic-ratelimit-unified-{window}-status"))
            .is_some_and(|status| status.trim() == "rejected")
    };
    let unified = header("anthropic-ratelimit-unified-status");
    if rejected("5h") || rejected("7d") {
        Refusal::Shared
    } else if family_rejected(header) {
        Refusal::Family
    } else if unified.as_deref().map(str::trim) == Some("rejected") {
        Refusal::Shared
    } else if retry_after_ms(header("retry-after").as_deref(), now).is_some()
        || unified.as_deref().map(str::trim) == Some("allowed")
    {
        Refusal::Rate
    } else {
        Refusal::Unknown
    }
}

/// When the Fable window a refusal rejected resets, if it says.
pub(crate) fn family_resets(header: Header<'_>) -> Vec<u64> {
    header("anthropic-ratelimit-unified-7d_oi-reset")
        .as_deref()
        .and_then(reset_ms)
        .into_iter()
        .collect()
}

/// Whether an Anthropic 403 body refuses the account itself for its
/// subscription, organization or credit balance, rather than the request
/// (reference `isAccountRefusal`).
pub(crate) fn anthropic_account_refusal(body: &str) -> bool {
    let Ok(payload) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    let Some(error) = payload.get("error").filter(|error| error.is_object()) else {
        return false;
    };
    let kind = error.get("type").and_then(Value::as_str).unwrap_or("");
    let Some(message) = error.get("message").and_then(Value::as_str) else {
        return false;
    };
    let code = error.get("code").filter(|code| !code.is_null());
    if let Some(code) = code {
        if !matches!(
            code.as_str(),
            Some(
                "subscription_required"
                    | "insufficient_quota"
                    | "permission_denied"
                    | "oauth_org_not_allowed"
            )
        ) {
            return false;
        }
    }
    if kind == "oauth_org_not_allowed"
        || code.and_then(Value::as_str) == Some("oauth_org_not_allowed")
    {
        return true;
    }
    let message = message.trim().trim_end_matches(['.', '!']).to_lowercase();
    let subscription = [
        "your account does not have access to claude code",
        "your subscription has expired",
        "your claude subscription has expired",
        "your subscription is expired",
        "your claude subscription is expired",
        "your subscription is inactive",
        "your claude subscription is inactive",
        "your account does not have an active subscription",
        "your organization has disabled claude subscription access for claude code",
        "this organization has disabled claude subscription access for claude code",
        "your organization has disabled claude subscription access for claude code. use an \
         anthropic api key instead, or ask your admin to enable access",
        "this organization has disabled claude subscription access for claude code. use an \
         anthropic api key instead, or ask your admin to enable access",
    ];
    let credit = [
        "your credit balance is too low to access the anthropic api",
        "your credit balance is too low to access the anthropic api. please go to plans & \
         billing to upgrade or purchase credits",
    ];
    (kind == "permission_error" && subscription.contains(&message.as_str()))
        || (matches!(kind, "billing_error" | "permission_error")
            && credit.contains(&message.as_str()))
}

/// The model a ChatGPT 400 says the account may not use (reference
/// `CODEX_ACCOUNT_MODEL_REFUSAL`): `detail`, or `error.message`, is exactly
/// "The '<model>' model is not supported when using Codex with a ChatGPT
/// account."
pub(crate) fn codex_model_refusal(body: &str) -> Option<String> {
    let payload: Value = serde_json::from_str(body.trim()).ok()?;
    if payload.get("detail").is_some() && payload.get("error").is_some() {
        return None;
    }
    let text = payload
        .get("detail")
        .and_then(Value::as_str)
        .or_else(|| payload.get("error")?.get("message")?.as_str())?;
    let model = text
        .strip_prefix("The '")?
        .strip_suffix("' model is not supported when using Codex with a ChatGPT account.")?;
    (!model.is_empty() && model.len() <= 256 && !model.contains('\'')).then(|| model.to_string())
}

/// The usage endpoint's windows ([`crate::Usage`]): sorted into short,
/// weekly and monthly by their stated length, else by their name. Windows
/// of one model only (`seven_day_opus`, `kind:model`) are left out.
pub(crate) fn usage_quota(usage: &crate::Usage, now: u64) -> Option<Quota> {
    let mut quota = Quota {
        updated_ms: now,
        ..Quota::default()
    };
    for window in &usage.windows {
        let name = window.name.to_ascii_lowercase();
        if name.contains(':') || name.starts_with("seven_day_") {
            continue;
        }
        let Some(used) = window.used_percent else {
            continue;
        };
        let reset = window.resets_at.as_deref().and_then(reset_ms);
        let slot = match window.window_seconds {
            Some(seconds) if seconds < WEEKLY_MIN_SECONDS => 0,
            Some(seconds) if seconds >= MONTHLY_MIN_SECONDS => 2,
            Some(_) => 1,
            None if ["five", "5h", "session", "hour", "window"]
                .iter()
                .any(|word| name.contains(word)) =>
            {
                0
            }
            None if name.contains("month") || name == "tertiary" => 2,
            None => 1,
        };
        match slot {
            0 => {
                quota.short = Some(used);
                quota.short_reset_ms = reset;
                quota.short_observed_ms = Some(now);
                quota.short_seconds = window.window_seconds;
            }
            1 if quota.weekly.is_none() => {
                quota.weekly = Some(used);
                quota.weekly_reset_ms = reset;
            }
            2 if quota.monthly.is_none() => {
                quota.monthly = Some(used);
                quota.monthly_reset_ms = reset;
            }
            _ => {}
        }
    }
    quota.plan = usage.plan.clone().filter(|plan| !plan.trim().is_empty());
    quota.credits = usage.credits.clone().map(|credits| (credits, now));
    (!quota.is_empty() || quota.plan.is_some()).then_some(quota)
}

/// The reset instants a quota refusal announces: every Codex window's, and
/// the Anthropic windows reported as rejected.
pub(crate) fn refusal_resets(header: Header<'_>) -> Vec<u64> {
    let mut resets: Vec<u64> = ["primary", "secondary", "tertiary"]
        .iter()
        .filter_map(|window| header(&format!("x-codex-{window}-reset-at")))
        .filter_map(|value| reset_ms(&value))
        .collect();
    let rejected = |window: &str| {
        header(&format!("anthropic-ratelimit-unified-{window}-status"))
            .is_some_and(|status| status.trim() == "rejected")
    };
    let mut any = false;
    for window in ["5h", "7d"] {
        if rejected(window) {
            any = true;
            resets.extend(
                header(&format!("anthropic-ratelimit-unified-{window}-reset"))
                    .as_deref()
                    .and_then(reset_ms),
            );
        }
    }
    if !any && header("anthropic-ratelimit-unified-status").is_some_and(|s| s.trim() == "rejected")
    {
        resets.extend(
            header("anthropic-ratelimit-unified-reset")
                .as_deref()
                .and_then(reset_ms),
        );
    }
    resets
}

/// `Retry-After` in milliseconds: seconds or an HTTP date, capped at a day.
pub(crate) fn retry_after_ms(value: Option<&str>, now: u64) -> Option<u64> {
    let text = value?.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(seconds) = number(text) {
        return (seconds > 0.0)
            .then(|| ((seconds * 1_000.0).ceil() as u64).clamp(1, MAX_QUOTA_COOLDOWN));
    }
    let at = chrono::DateTime::parse_from_rfc2822(text).ok()?;
    let at = u64::try_from(at.timestamp_millis()).ok()?;
    (at > now).then(|| (at - now).clamp(1, MAX_QUOTA_COOLDOWN))
}

/// What set a quota cooldown's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CooldownSource {
    RetryAfter,
    Reset,
    Default,
}

impl CooldownSource {
    fn name(self) -> &'static str {
        match self {
            Self::RetryAfter => "retry-after",
            Self::Reset => "reset",
            Self::Default => "default",
        }
    }

    fn parse(name: &str) -> Self {
        match name {
            "retry-after" => Self::RetryAfter,
            "reset" => Self::Reset,
            _ => Self::Default,
        }
    }
}

/// When a quota refusal's cooldown ends: `Retry-After`, else the soonest
/// announced reset (at most [`MAX_RESET_COOLDOWN`] away), else a minute.
pub(crate) fn quota_cooldown(
    retry_after: Option<&str>,
    resets: &[u64],
    now: u64,
) -> (u64, CooldownSource) {
    if let Some(delay) = retry_after_ms(retry_after, now) {
        return (now + delay, CooldownSource::RetryAfter);
    }
    let soonest = resets
        .iter()
        .filter(|reset| **reset > now)
        .map(|reset| {
            (reset - now)
                .clamp(1, MAX_QUOTA_COOLDOWN)
                .min(MAX_RESET_COOLDOWN)
        })
        .min();
    match soonest {
        Some(delay) => (now + delay, CooldownSource::Reset),
        None => (now + DEFAULT_QUOTA_COOLDOWN, CooldownSource::Default),
    }
}

/// Until when the pool prefers other accounts after a quota refusal: the
/// soonest announced reset (at most [`MAX_QUOTA_AVOID`] away), and never
/// before the cooldown ends.
pub(crate) fn quota_avoid_until(resets: &[u64], now: u64, cooldown_until: u64) -> u64 {
    let announced = resets
        .iter()
        .filter(|reset| **reset > now)
        .map(|reset| now + (reset - now).min(MAX_QUOTA_AVOID))
        .min()
        .unwrap_or(0);
    cooldown_until.max(announced)
}

/// Success and transient-failure samples of the last minute.
#[derive(Debug, Clone, Default)]
struct FailureWindow {
    samples: VecDeque<(u64, bool)>,
    degraded: bool,
    healthy_since: Option<u64>,
}

impl FailureWindow {
    fn push(&mut self, now: u64, failed: bool) {
        self.samples.push_back((now, failed));
        while self.samples.len() > FAILURE_WINDOW_MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.recompute(now);
    }

    /// Degraded at a quarter of 20 or more samples failed; it recovers once
    /// at most a tenth failed for 30 seconds.
    fn recompute(&mut self, now: u64) {
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| *at + FAILURE_WINDOW <= now)
        {
            self.samples.pop_front();
        }
        let samples = self.samples.len();
        let failed = self.samples.iter().filter(|(_, failed)| *failed).count();
        let enough = samples >= FAILURE_WINDOW_MIN_SAMPLES;
        if enough && failed * 4 >= samples {
            self.degraded = true;
            self.healthy_since = None;
            return;
        }
        if !self.degraded || !(enough && failed * 10 <= samples) {
            self.healthy_since = None;
            return;
        }
        let since = *self.healthy_since.get_or_insert(now);
        if now - since >= FAILURE_WINDOW_RECOVER_SUSTAIN {
            self.degraded = false;
            self.healthy_since = None;
        }
    }
}

/// What the pool knows of one account's health.
#[derive(Debug, Clone, Default)]
struct Health {
    consecutive_failures: u32,
    consecutive_successes: u32,
    last_failure_ms: Option<u64>,
    last_status: Option<u16>,
    last_error: Option<String>,
    cooldown_until: Option<u64>,
    cooldown_since: Option<u64>,
    cooldown_source: Option<CooldownSource>,
    quota_avoid_until: Option<u64>,
    last_probe_ms: Option<u64>,
    soft_avoid_until: Option<u64>,
    /// An Anthropic rate pause: the account takes no new request until
    /// then, and keeps its threads.
    rate_pause_until: Option<u64>,
    /// It refused on quota and has not served since: a thread bound to it
    /// leaves, whatever its cooldown says.
    quota_refused: bool,
    window: FailureWindow,
}

impl Health {
    fn cooling(&self, now: u64) -> bool {
        self.cooldown_until.is_some_and(|until| until > now)
    }

    fn quota_avoided(&self, now: u64) -> bool {
        self.quota_avoid_until.is_some_and(|until| until > now)
    }

    fn soft_avoided(&self, now: u64) -> bool {
        self.soft_avoid_until.is_some_and(|until| until > now)
    }

    fn rate_paused(&self, now: u64) -> bool {
        self.rate_pause_until.is_some_and(|until| until > now)
    }

    fn clear_cooldown(&mut self) {
        self.cooldown_until = None;
        self.cooldown_since = None;
        self.cooldown_source = None;
        self.quota_avoid_until = None;
        self.quota_refused = false;
    }
}

/// What an error answer says beyond its status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Its status alone.
    Plain,
    /// Anthropic 429: the account's shared quota.
    Shared,
    /// Anthropic 429: the Fable family's quota only.
    Family,
    /// Anthropic 429: a short pause of the account, not its quota.
    Rate,
    /// Anthropic 429 that names no window.
    Unknown,
    /// Anthropic 403 refusing the account's subscription or billing.
    Account,
    /// ChatGPT 400: the account may not use the model.
    ModelDenied,
}

/// What an error answer of the provider of the store name `kind` is.
pub(crate) fn refusal(
    kind: &str,
    status: u16,
    header: Header<'_>,
    body: &str,
    now: u64,
) -> Refusal {
    match (kind, status) {
        ("claude-oauth", 429) => anthropic_429(header, now),
        ("claude-oauth", 403) if anthropic_account_refusal(body) => Refusal::Account,
        ("gpt-oauth", 400) if codex_model_refusal(body).is_some() => Refusal::ModelDenied,
        _ => Refusal::Plain,
    }
}

/// How a response or a failure to get one counts for an account.
#[derive(Debug, Clone)]
pub(crate) enum Outcome {
    /// The provider answered with this status.
    Status {
        status: u16,
        retry_after: Option<String>,
        resets: Vec<u64>,
        /// A 403 whose body names a workspace or entitlement denial: the
        /// credential is fine, the account lacks access.
        denial: bool,
        refusal: Refusal,
        message: String,
    },
    /// No answer: the connection failed, timed out, or the stream broke.
    Broken(String),
    /// The host was never reached (its name did not resolve, or it refused
    /// the connection): nothing about the account.
    Unreachable { message: String, code: String },
}

/// Whether, and how, a refused request is sent again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Retry {
    No,
    /// Once, on the account the pool chooses next.
    Other,
    /// Once, on the same account after this many milliseconds.
    Same(u64),
    /// On the same account after this many milliseconds, then once on
    /// another.
    SameThenOther(u64),
}

impl Outcome {
    /// A successful answer.
    pub(crate) fn answered() -> Self {
        Self::Status {
            status: 200,
            retry_after: None,
            resets: Vec::new(),
            denial: false,
            refusal: Refusal::Plain,
            message: String::new(),
        }
    }

    /// How a refusal on an account of the store name `kind` is retried:
    /// quota and account refusals elsewhere; an Anthropic pause of up to a
    /// second on the same account first; an unnamed Anthropic 429 once on
    /// the same account (reference `rotateAnthropicAccountOnResponse`); a
    /// refused static key elsewhere (reference `rotateKeyAfterFailure`).
    pub(crate) fn retry(&self, kind: &str, now: u64) -> Retry {
        let Self::Status {
            status,
            retry_after,
            refusal,
            ..
        } = self
        else {
            return Retry::No;
        };
        match refusal {
            Refusal::Shared | Refusal::Family | Refusal::Account | Refusal::ModelDenied => {
                Retry::Other
            }
            Refusal::Rate => {
                let wait = retry_after_ms(retry_after.as_deref(), now).unwrap_or(SHORT_RETRY);
                if wait <= MAX_INLINE_THROTTLE {
                    Retry::SameThenOther(wait)
                } else {
                    Retry::Other
                }
            }
            Refusal::Unknown => Retry::Same(SHORT_RETRY),
            Refusal::Plain => match status {
                402 | 429 => Retry::Other,
                401 if is_key(kind) => Retry::Other,
                _ => Retry::No,
            },
        }
    }
}

/// Whether the store name `kind` holds static API keys.
fn is_key(kind: &str) -> bool {
    kind.starts_with("api-key/")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Success,
    Neutral,
    Caller,
    Workspace,
    Credential,
    Quota,
    Transient,
    /// A model family's quota is spent; the account serves other models.
    Family,
    /// The account is paused for a moment; its quota is fine.
    Rate,
    /// The account may not use the model.
    ModelDenied,
    /// A static key was refused.
    KeyRefused,
    /// The host was never reached.
    Unreachable,
}

fn classify(kind: &str, outcome: &Outcome) -> Class {
    let (status, denial, refusal) = match outcome {
        Outcome::Broken(_) => return Class::Transient,
        Outcome::Unreachable { .. } => return Class::Unreachable,
        Outcome::Status {
            status,
            denial,
            refusal,
            ..
        } => (*status, *denial, *refusal),
    };
    match refusal {
        Refusal::Shared | Refusal::Account => return Class::Quota,
        Refusal::Family => return Class::Family,
        Refusal::Rate => return Class::Rate,
        Refusal::Unknown => return Class::Neutral,
        Refusal::ModelDenied => return Class::ModelDenied,
        Refusal::Plain => {}
    }
    match status {
        200..=299 => Class::Success,
        300..=399 => Class::Neutral,
        401 if is_key(kind) => Class::KeyRefused,
        403 if is_key(kind) || kind == "claude-oauth" => Class::Caller,
        403 if denial => Class::Workspace,
        401 | 403 => Class::Credential,
        402 | 429 => Class::Quota,
        400..=499 => Class::Caller,
        500..=599 => Class::Transient,
        _ => Class::Neutral,
    }
}

/// Whether a 403 body names a workspace or entitlement denial rather than a
/// refused credential (reference `WORKSPACE_DENIAL_CODES`,
/// `ENTITLEMENT_DENIAL_CODES`).
pub(crate) fn names_denial(body: &str) -> bool {
    let Ok(payload) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    let code = payload
        .get("code")
        .or_else(|| payload.get("error").and_then(|error| error.get("code")))
        .or_else(|| payload.get("detail").and_then(|detail| detail.get("code")))
        .and_then(Value::as_str);
    matches!(
        code,
        Some(
            "codex_workspace_access_denied"
                | "workspace_access_denied"
                | "codex_entitlement_missing"
                | "entitlement_missing"
        )
    )
}

/// One account of the pool, as the credential store describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    pub id: String,
    pub priority: i32,
    pub threshold: Option<u32>,
    pub paused: bool,
    pub needs_reauth: bool,
    /// The plan its credential names, if any.
    pub plan: Option<String>,
    /// It may serve from credits once a window is full.
    pub credits: bool,
}

impl Member {
    pub(crate) fn of(account: &Account) -> Self {
        Self {
            id: account.id.clone(),
            priority: account
                .info
                .priority
                .unwrap_or(0)
                .clamp(MIN_PRIORITY, MAX_PRIORITY),
            threshold: account.info.auto_switch_threshold.map(|t| t.min(100)),
            paused: account.info.paused,
            needs_reauth: account.info.needs_reauth,
            plan: account.plan.clone(),
            credits: account.info.credits,
        }
    }

    pub(crate) fn usable(&self) -> bool {
        !self.paused && !self.needs_reauth
    }
}

/// One provider's pool as a request sees it.
pub(crate) struct Ctx<'a> {
    pub provider: &'a str,
    /// The credential store's provider name of the accounts.
    pub kind: &'a str,
    pub rules: Rules,
    /// The settings' `active_account`.
    pub pinned: Option<&'a str>,
    pub members: &'a [Member],
    /// The model of the request.
    pub model: &'a str,
    pub now: u64,
}

/// The quota group of a model whose refusals may spare the account's other
/// models: ChatGPT's `gpt-reserve` (`reserve`) apart from the rest
/// (`shared`), reference `NATIVE_MODEL_QUOTA_SCOPES`; Anthropic's Fable
/// family, reference `anthropicModelFamily`. Other kinds have none.
pub(crate) fn quota_scope(kind: &str, model: &str) -> Option<&'static str> {
    match kind {
        "gpt-oauth" if model.trim().eq_ignore_ascii_case("gpt-reserve") => Some("reserve"),
        "gpt-oauth" if !model.trim().is_empty() => Some("shared"),
        "claude-oauth" => anthropic_family(model),
        _ => None,
    }
}

impl Ctx<'_> {
    fn member(&self, id: &str) -> Option<&Member> {
        self.members.iter().find(|member| member.id == id)
    }

    fn scope(&self) -> Option<&'static str> {
        quota_scope(self.kind, self.model)
    }

    /// A quota group apart from the account's shared quota: refusals in it
    /// do not move the provider's placement.
    fn independent_scope(&self) -> bool {
        self.scope().is_some_and(|scope| scope != "shared")
    }

    fn family(&self) -> Option<&'static str> {
        (self.kind == "claude-oauth")
            .then(|| anthropic_family(self.model))
            .flatten()
    }

    fn threshold(&self, id: &str) -> u32 {
        self.member(id)
            .and_then(|member| member.threshold)
            .unwrap_or(self.rules.auto_switch_threshold)
    }

    fn priority(&self, id: &str) -> i32 {
        self.member(id).map_or(0, |member| member.priority)
    }
}

/// Per provider: where automatic placement is, and the round-robin state.
#[derive(Debug, Clone, Default)]
struct ProviderState {
    cursor: Option<String>,
    /// The `active_account` the cursor was last set from.
    pinned_seen: Option<Option<String>>,
    weights: HashMap<String, i64>,
    sticky: Option<String>,
    sticky_count: u32,
}

#[derive(Debug, Clone)]
struct Binding {
    kind: String,
    account: String,
    bound_ms: u64,
    used_ms: u64,
    written_ms: u64,
    reeval_ms: u64,
    /// When transient trouble first sent this thread elsewhere while it
    /// stayed bound.
    hold_since: Option<u64>,
    detour: Option<String>,
}

/// What the pool remembers to write to the database.
enum Write {
    Bind(PoolBinding),
    Unbind(SessionId, String),
    UnbindAccount(String, String),
    Cooldown(PoolCooldown),
    Uncool(String, String),
    UncoolScope(String, String, String),
    Clear(Option<String>, Option<SessionId>),
}

type Key = (String, String);

fn key(kind: &str, account: &str) -> Key {
    (kind.to_string(), account.to_string())
}

/// A quota refusal of one quota group of an account ([`quota_scope`]).
#[derive(Debug, Clone)]
struct Scoped {
    until: u64,
    avoid_until: u64,
    since: u64,
    source: CooldownSource,
    status: u16,
    /// Refused and not served in the group since.
    refused: bool,
}

/// What is known of one provider host's reachability (reference
/// `upstream-host-health.ts`).
#[derive(Debug, Clone, Default)]
struct Host {
    failures: u32,
    last_failure_ms: u64,
    last_code: Option<String>,
    cooldown_until: Option<u64>,
    /// When a request after the cooldown started; others wait for it,
    /// at most another cooldown, since an unsent trial reports nothing.
    trial: Option<u64>,
}

#[derive(Debug, Clone, Default)]
struct Inner {
    health: HashMap<Key, Health>,
    quota: HashMap<Key, Quota>,
    providers: HashMap<String, ProviderState>,
    bindings: HashMap<(i64, String), Binding>,
    polling: HashSet<Key>,
    polled: HashMap<Key, u64>,
    scoped: HashMap<(Key, String), Scoped>,
    /// Until when an account's refusal of a model keeps it last for it.
    denials: HashMap<(Key, String), u64>,
    /// Accounts a new conversation was sent to for their idle 5-hour
    /// window, and until when that window is taken as started.
    steered: HashMap<Key, u64>,
    /// The thread each forked thread was made from.
    parents: HashMap<i64, i64>,
    hosts: HashMap<String, Host>,
}

/// Pick the highest priority tier that still has an account with headroom;
/// a pinned account with headroom caps the tier at its own. Order inside
/// the tier is kept. With one tier, or none with headroom, all of `ids`.
fn priority_tier(
    ids: Vec<String>,
    priority: impl Fn(&str) -> i32,
    headroom: impl Fn(&str) -> bool,
    pinned: Option<&str>,
) -> Vec<String> {
    if ids.len() <= 1 {
        return ids;
    }
    let priorities: Vec<i32> = ids.iter().map(|id| priority(id)).collect();
    if priorities.iter().all(|p| *p == priorities[0]) {
        return ids;
    }
    let ceiling = pinned
        .and_then(|pinned| ids.iter().position(|id| id == pinned))
        .filter(|index| headroom(&ids[*index]))
        .map_or(i32::MAX, |index| priorities[index]);
    let mut tiers = priorities.clone();
    tiers.sort_unstable_by(|a, b| b.cmp(a));
    tiers.dedup();
    for tier in tiers.into_iter().filter(|tier| *tier <= ceiling) {
        let members: Vec<String> = ids
            .iter()
            .zip(&priorities)
            .filter(|(_, p)| **p == tier)
            .map(|(id, _)| id.clone())
            .collect();
        if members.iter().any(|id| headroom(id)) {
            return members;
        }
    }
    ids
}

impl Inner {
    fn health(&self, ctx: &Ctx, id: &str) -> Option<&Health> {
        self.health.get(&key(ctx.kind, id))
    }

    fn health_mut(&mut self, kind: &str, id: &str) -> &mut Health {
        self.health.entry(key(kind, id)).or_default()
    }

    fn score(&self, ctx: &Ctx, id: &str) -> f64 {
        self.quota
            .get(&key(ctx.kind, id))
            .map_or(UNKNOWN_SCORE, |quota| {
                quota.score_for(ctx.now, ctx.family())
            })
    }

    /// Whether the account is under its threshold. Unknown usage and a
    /// threshold of 0 count as headroom: a guess never drains an account.
    fn headroom(&self, ctx: &Ctx, id: &str) -> bool {
        let threshold = ctx.threshold(id);
        if threshold == 0 {
            return true;
        }
        let score = self.score(ctx, id);
        score >= UNKNOWN_SCORE || score < f64::from(threshold)
    }

    /// The account's refusal in the request's quota group, if one runs.
    fn scoped(&self, ctx: &Ctx, id: &str) -> Option<&Scoped> {
        let scope = ctx.scope()?;
        self.scoped.get(&(key(ctx.kind, id), scope.to_string()))
    }

    fn account_cooling(&self, ctx: &Ctx, id: &str) -> bool {
        self.health(ctx, id).is_some_and(|h| h.cooling(ctx.now))
    }

    fn scope_cooling(&self, ctx: &Ctx, id: &str) -> bool {
        self.scoped(ctx, id).is_some_and(|s| s.until > ctx.now)
    }

    fn cooling(&self, ctx: &Ctx, id: &str) -> bool {
        self.account_cooling(ctx, id) || self.scope_cooling(ctx, id)
    }

    fn account_quota_avoided(&self, ctx: &Ctx, id: &str) -> bool {
        self.health(ctx, id)
            .is_some_and(|h| h.quota_avoided(ctx.now))
    }

    fn quota_avoided(&self, ctx: &Ctx, id: &str) -> bool {
        self.account_quota_avoided(ctx, id)
            || self
                .scoped(ctx, id)
                .is_some_and(|s| s.avoid_until > ctx.now)
    }

    fn soft_avoided(&self, ctx: &Ctx, id: &str) -> bool {
        self.health(ctx, id)
            .is_some_and(|h| h.soft_avoided(ctx.now))
    }

    fn rate_paused(&self, ctx: &Ctx, id: &str) -> bool {
        self.health(ctx, id).is_some_and(|h| h.rate_paused(ctx.now))
    }

    fn degraded(&self, ctx: &Ctx, id: &str) -> bool {
        self.health(ctx, id).is_some_and(|h| h.window.degraded)
    }

    fn account_quota_refused(&self, ctx: &Ctx, id: &str) -> bool {
        self.health(ctx, id).is_some_and(|h| h.quota_refused)
    }

    fn quota_refused(&self, ctx: &Ctx, id: &str) -> bool {
        self.account_quota_refused(ctx, id) || self.scoped(ctx, id).is_some_and(|s| s.refused)
    }

    /// The account's own family refusal keeps it from this request only:
    /// another model of the thread may still use it.
    fn family_blocked(&self, ctx: &Ctx, id: &str) -> bool {
        ctx.family().is_some() && self.scope_cooling(ctx, id)
    }

    fn plan(&self, ctx: &Ctx, id: &str) -> Option<String> {
        self.quota
            .get(&key(ctx.kind, id))
            .and_then(|quota| quota.plan.clone())
            .or_else(|| ctx.member(id).and_then(|member| member.plan.clone()))
    }

    /// Its plan is one the pool does not choose by itself; the account the
    /// person chose is still used (reference `isCodexAccountPlanExcluded`).
    fn plan_excluded(&self, ctx: &Ctx, id: &str) -> bool {
        !ctx.rules.excluded_plans.is_empty()
            && ctx.pinned != Some(id)
            && self.plan(ctx, id).is_some_and(|plan| {
                ctx.rules
                    .excluded_plans
                    .contains(&plan.trim().to_lowercase())
            })
    }

    /// A ChatGPT account with a full window is kept from spending credits
    /// unless it may and holds them (reference `isCodexAccountHeldForCredits`).
    fn credits_held(&self, ctx: &Ctx, id: &str) -> bool {
        if ctx.kind != "gpt-oauth" {
            return false;
        }
        let Some(quota) = self.quota.get(&key(ctx.kind, id)) else {
            return false;
        };
        let allowed = ctx.member(id).is_some_and(|member| member.credits);
        quota.limit_reached(ctx.now) && !(allowed && quota.spendable_credits(ctx.now))
    }

    /// The account refused the request's model lately, or its family's
    /// window is full: other accounts go first for this model.
    fn model_unfit(&self, ctx: &Ctx, id: &str) -> bool {
        let slot = key(ctx.kind, id);
        let denied = self
            .denials
            .get(&(slot.clone(), ctx.model.to_string()))
            .is_some_and(|until| *until > ctx.now);
        let spent = ctx.family().is_some_and(|family| {
            self.quota
                .get(&slot)
                .and_then(|quota| quota.families.get(family))
                .is_some_and(|percent| *percent >= EXHAUSTED)
        });
        denied || spent
    }

    fn should_failover(&self, ctx: &Ctx, id: &str) -> bool {
        let threshold = ctx.rules.failover_threshold;
        let Some(health) = self.health(ctx, id) else {
            return false;
        };
        if threshold == 0 {
            return false;
        }
        if health
            .last_failure_ms
            .is_some_and(|at| ctx.now.saturating_sub(at) > STREAK_GAP)
        {
            return false;
        }
        health.consecutive_failures >= threshold
    }

    fn selectable(&self, ctx: &Ctx, id: &str) -> bool {
        ctx.member(id).is_some_and(Member::usable)
            && !self.cooling(ctx, id)
            && !self.quota_avoided(ctx, id)
            && !self.soft_avoided(ctx, id)
            && !self.rate_paused(ctx, id)
            && !self.plan_excluded(ctx, id)
            && !self.credits_held(ctx, id)
    }

    /// Why the account is not selectable, if it is not.
    fn block_reason(&self, ctx: &Ctx, id: &str) -> Option<&'static str> {
        let member = ctx.member(id)?;
        if member.paused {
            Some("paused")
        } else if member.needs_reauth {
            Some("needs_reauth")
        } else if self.plan_excluded(ctx, id) {
            Some("plan_excluded")
        } else if self.credits_held(ctx, id) {
            Some("usage_limit")
        } else if self.cooling(ctx, id) {
            Some("cooldown")
        } else if self.quota_avoided(ctx, id) {
            Some("quota_avoided")
        } else if self.rate_paused(ctx, id) {
            Some("rate_paused")
        } else if self.soft_avoided(ctx, id) {
            Some("transient")
        } else if self.degraded(ctx, id) {
            Some("degraded")
        } else {
            None
        }
    }

    /// The accounts new placements and failovers may use, those fit for the
    /// request's model first ([`Self::model_unfit`]), narrowed to the
    /// priority tier ([`priority_tier`]).
    fn eligible(&self, ctx: &Ctx, exclude: Option<&str>, skip_failing: bool) -> Vec<String> {
        let ids: Vec<String> = ctx
            .members
            .iter()
            .filter(|member| {
                let id = member.id.as_str();
                Some(id) != exclude
                    && member.usable()
                    && !(skip_failing && self.should_failover(ctx, id))
                    && !self.cooling(ctx, id)
                    && !self.soft_avoided(ctx, id)
                    && !self.rate_paused(ctx, id)
                    && !(self.degraded(ctx, id) && ctx.pinned != Some(id))
                    && !self.quota_avoided(ctx, id)
                    && !self.plan_excluded(ctx, id)
                    && !self.credits_held(ctx, id)
            })
            .map(|member| member.id.clone())
            .collect();
        let fit: Vec<String> = ids
            .iter()
            .filter(|id| !self.model_unfit(ctx, id))
            .cloned()
            .collect();
        let ids = if fit.is_empty() { ids } else { fit };
        priority_tier(
            ids,
            |id| ctx.priority(id),
            |id| self.headroom(ctx, id),
            ctx.pinned,
        )
    }
    /// The least used of `ids`; the first wins a tie.
    fn lowest(&self, ctx: &Ctx, ids: &[String]) -> Option<String> {
        let mut best: Option<(&String, f64)> = None;
        for id in ids {
            let score = self.score(ctx, id);
            if best.is_none_or(|(_, lowest)| score < lowest) {
                best = Some((id, score));
            }
        }
        best.map(|(id, _)| id.clone())
    }

    fn state(&mut self, ctx: &Ctx) -> &mut ProviderState {
        let state = self.providers.entry(ctx.provider.to_string()).or_default();
        if state.pinned_seen.is_none() {
            let pinned = ctx.pinned.map(str::to_string);
            state.cursor = pinned.clone();
            state.sticky = pinned.clone();
            state.pinned_seen = Some(pinned);
        }
        state
    }

    /// Follows a change of the provider's `active_account`. Choosing an
    /// account by hand starts placement and rotation from it, moves the
    /// provider's threads to it, and overrules the preferences that kept the
    /// pool away from it (failure streak, soft and quota avoidance); a
    /// running quota cooldown stays. Clearing the choice only stops steering
    /// placement towards it.
    fn sync_pin(&mut self, ctx: &Ctx, writes: &mut Vec<Write>) {
        let pinned = ctx.pinned.map(str::to_string);
        let state = self.providers.entry(ctx.provider.to_string()).or_default();
        let first = state.pinned_seen.is_none();
        if state.pinned_seen.as_ref() == Some(&pinned) {
            return;
        }
        state.pinned_seen = Some(pinned.clone());
        state.cursor = pinned.clone();
        state.weights.clear();
        state.sticky = pinned.clone();
        state.sticky_count = 0;
        let Some(pinned) = pinned.filter(|_| !first) else {
            return;
        };
        let before = self.bindings.len();
        self.bindings
            .retain(|(_, provider), _| provider != ctx.provider);
        if self.bindings.len() != before {
            writes.push(Write::Clear(Some(ctx.provider.to_string()), None));
        }
        if let Some(health) = self.health.get_mut(&key(ctx.kind, &pinned)) {
            health.consecutive_failures = 0;
            health.soft_avoid_until = None;
            health.quota_avoid_until = health.quota_avoid_until.min(health.cooldown_until);
        }
    }

    /// The pinned account while it can serve with headroom.
    fn usable_pin(&self, ctx: &Ctx) -> Option<String> {
        ctx.pinned
            .filter(|pinned| self.selectable(ctx, pinned) && self.headroom(ctx, pinned))
            .map(str::to_string)
    }

    fn cursor(&mut self, ctx: &Ctx) -> Option<String> {
        self.state(ctx)
            .cursor
            .clone()
            .filter(|id| ctx.member(id).is_some())
    }

    /// Placement in a quota group of its own (`gpt-reserve`) leaves the
    /// provider's current account alone (reference
    /// `isIndependentCodexQuotaScope`).
    fn set_cursor(&mut self, ctx: &Ctx, id: &str) {
        if !ctx.independent_scope() {
            self.state(ctx).cursor = Some(id.to_string());
        }
    }

    /// Smooth weighted round robin with equal weights; with a sticky limit
    /// above 1 the pick stays for that many new conversations.
    fn round_robin(&mut self, ctx: &Ctx, ids: &[String], commit: bool) -> Option<String> {
        if ids.is_empty() {
            return None;
        }
        let limit = ctx.rules.sticky_limit.max(1);
        let state = self.state(ctx);
        if let Some(sticky) = state.sticky.clone().filter(|id| ids.contains(id)) {
            if commit {
                Self::note_round(state, &sticky, limit);
            }
            return Some(sticky);
        }
        let mut scratch;
        let state = if commit {
            state.sticky = None;
            state.sticky_count = 0;
            state
        } else {
            scratch = state.clone();
            &mut scratch
        };
        let total = ids.len() as i64;
        let mut best = 0;
        let mut best_weight = i64::MIN;
        for (index, id) in ids.iter().enumerate() {
            let weight = state.weights.entry(id.clone()).or_insert(0);
            *weight += 1;
            if *weight > best_weight {
                best = index;
                best_weight = *weight;
            }
        }
        *state.weights.get_mut(&ids[best]).expect("weighed") -= total;
        let picked = ids[best].clone();
        if commit {
            if limit > 1 {
                state.sticky = Some(picked.clone());
                state.sticky_count = 0;
            }
            Self::note_round(state, &picked, limit);
        }
        Some(picked)
    }

    fn note_round(state: &mut ProviderState, id: &str, limit: u32) {
        if state.sticky.as_deref() != Some(id) {
            if limit <= 1 {
                return;
            }
            state.sticky = Some(id.to_string());
            state.sticky_count = 0;
        }
        state.sticky_count += 1;
        if state.sticky_count >= limit {
            state.sticky = None;
            state.sticky_count = 0;
        }
    }

    /// The next account after `after` in id order that has headroom, else
    /// the next one at all.
    fn next_fill(&self, ctx: &Ctx, after: Option<&str>, eligible: &[String]) -> Option<String> {
        let mut ordered = eligible.to_vec();
        ordered.sort();
        let first_with_room = || {
            ordered
                .iter()
                .find(|id| self.headroom(ctx, id))
                .or(ordered.first())
                .cloned()
        };
        let Some(after) = after else {
            return first_with_room();
        };
        let mut all: Vec<&str> = ctx.members.iter().map(|m| m.id.as_str()).collect();
        all.sort_unstable();
        let Some(start) = all.iter().position(|id| *id == after) else {
            return first_with_room();
        };
        let mut fallback = None;
        for step in 1..=all.len() {
            let candidate = all[(start + step) % all.len()];
            if !eligible.iter().any(|id| id == candidate) {
                continue;
            }
            fallback.get_or_insert(candidate);
            if self.headroom(ctx, candidate) {
                return Some(candidate.to_string());
            }
        }
        fallback
            .map(str::to_string)
            .or_else(|| ordered.first().cloned())
    }

    /// Among the accounts with headroom, the one whose short or weekly
    /// window resets first; ties and missing resets go by usage.
    fn reset_first(&self, ctx: &Ctx, ids: &[String]) -> Option<String> {
        let available: Vec<String> = ids
            .iter()
            .filter(|id| self.headroom(ctx, id))
            .cloned()
            .collect();
        if available.is_empty() {
            return self.lowest(ctx, ids);
        }
        let next = |id: &str| {
            self.quota
                .get(&key(ctx.kind, id))
                .and_then(|quota| quota.next_reset(ctx.now))
                .unwrap_or(u64::MAX)
        };
        let earliest = available
            .iter()
            .map(|id| next(id))
            .min()
            .unwrap_or(u64::MAX);
        let candidates: Vec<String> = available
            .into_iter()
            .filter(|id| next(id) == earliest)
            .collect();
        self.lowest(ctx, &candidates)
    }

    /// Another account than `exclude`, by the strategy: after a quota
    /// refusal or a failure streak.
    fn alternate(&mut self, ctx: &Ctx, exclude: &str) -> Option<String> {
        let eligible = self.eligible(ctx, Some(exclude), false);
        match ctx.rules.strategy {
            Strategy::RoundRobin => self.round_robin(ctx, &eligible, true),
            Strategy::FillFirst => self.next_fill(ctx, Some(exclude), &eligible),
            Strategy::ResetFirst => self.reset_first(ctx, &eligible),
            Strategy::Quota => self.lowest(ctx, &eligible),
        }
    }

    /// The account a new conversation goes to under round-robin,
    /// fill-first or reset-first.
    fn strategy_pick(&mut self, ctx: &Ctx, commit: bool) -> Option<String> {
        let eligible = self.eligible(ctx, None, false);
        let picked = match ctx.rules.strategy {
            Strategy::Quota => return None,
            Strategy::RoundRobin => {
                let with_room: Vec<String> = eligible
                    .iter()
                    .filter(|id| self.headroom(ctx, id))
                    .cloned()
                    .collect();
                let placed = if with_room.is_empty() {
                    eligible
                } else {
                    with_room
                };
                return self.round_robin(ctx, &placed, commit);
            }
            Strategy::FillFirst => {
                let active = self.cursor(ctx);
                match active {
                    Some(active) if eligible.contains(&active) && self.headroom(ctx, &active) => {
                        Some(active)
                    }
                    active => self.next_fill(ctx, active.as_deref(), &eligible),
                }
            }
            Strategy::ResetFirst => self.reset_first(ctx, &eligible),
        }?;
        if commit {
            self.set_cursor(ctx, &picked);
        }
        Some(picked)
    }

    /// A higher tier that regained headroom takes new conversations back,
    /// unless a pinned account with headroom holds the ceiling.
    fn preemption(&self, ctx: &Ctx, active: &str) -> Option<String> {
        let eligible = self.eligible(ctx, None, false);
        if eligible.is_empty() || eligible.iter().any(|id| id == active) {
            return None;
        }
        if let Some(pinned) = ctx.pinned {
            if eligible.iter().any(|id| id == pinned) && self.headroom(ctx, pinned) {
                return None;
            }
        }
        if ctx.priority(&eligible[0]) <= ctx.priority(active) {
            return None;
        }
        let with_room: Vec<String> = eligible
            .into_iter()
            .filter(|id| self.headroom(ctx, id))
            .collect();
        self.lowest(ctx, &with_room)
    }

    /// Moves off `active` once its usage reaches its threshold, to a less
    /// used account.
    fn auto_switch(&mut self, ctx: &Ctx, active: String) -> String {
        let threshold = ctx.threshold(&active);
        if threshold == 0 {
            return active;
        }
        let usage = self.score(ctx, &active);
        if usage >= UNKNOWN_SCORE || usage < f64::from(threshold) {
            return active;
        }
        let mut best = (active.clone(), usage);
        for id in self.eligible(ctx, Some(&active), false) {
            let score = self.score(ctx, &id);
            if score < best.1 {
                best = (id, score);
            }
        }
        if best.0 != active {
            self.set_cursor(ctx, &best.0);
        }
        best.0
    }

    fn failure_failover(&mut self, ctx: &Ctx, active: String) -> String {
        let steered = self.degraded(ctx, &active) && ctx.pinned != Some(active.as_str());
        if !self.should_failover(ctx, &active) && !steered {
            return active;
        }
        match self.alternate(ctx, &active) {
            Some(best) => {
                self.set_cursor(ctx, &best);
                best
            }
            None => active,
        }
    }

    /// Blocked only by trouble that passes: transient failures, a rate
    /// pause, or the refusal of the request's model family. Worth holding
    /// the thread's binding while one request goes elsewhere.
    fn transient_only(&self, ctx: &Ctx, id: &str) -> bool {
        ctx.member(id).is_some_and(Member::usable)
            && !self.account_cooling(ctx, id)
            && !self.account_quota_avoided(ctx, id)
            && !self.account_quota_refused(ctx, id)
            && !self.plan_excluded(ctx, id)
            && !self.credits_held(ctx, id)
            && (!self.cooling(ctx, id) || self.family_blocked(ctx, id))
            && (self.soft_avoided(ctx, id)
                || self.should_failover(ctx, id)
                || self.degraded(ctx, id)
                || self.rate_paused(ctx, id)
                || self.family_blocked(ctx, id))
    }

    /// A higher tier's account for a thread bound to `account`, under the
    /// quota strategy with `priority_failback` on and a threshold on the
    /// account: one with fresh usage under 100% and under its threshold,
    /// that has not refused and is not failing (reference
    /// `pickAffinityPriorityFailback`).
    fn priority_failback(&self, ctx: &Ctx, account: &str) -> Option<String> {
        if !ctx.rules.priority_failback
            || ctx.rules.strategy != Strategy::Quota
            || ctx.threshold(account) == 0
            || self.preemption(ctx, account).is_none()
        {
            return None;
        }
        let fresh = |id: &str| {
            self.quota.get(&key(ctx.kind, id)).is_some_and(|quota| {
                ctx.now.saturating_sub(quota.updated_ms) < FAILBACK_REFRESH
                    && quota
                        .short_observed_ms
                        .is_none_or(|seen| ctx.now.saturating_sub(seen) < FAILBACK_REFRESH)
            })
        };
        let candidates: Vec<String> = self
            .eligible(ctx, None, false)
            .into_iter()
            .filter(|id| {
                let threshold = ctx.threshold(id);
                let usage = self.score(ctx, id);
                self.headroom(ctx, id)
                    && !self.quota_refused(ctx, id)
                    && !self.should_failover(ctx, id)
                    && fresh(id)
                    && usage < EXHAUSTED
                    && (threshold == 0 || usage < f64::from(threshold))
            })
            .collect();
        self.lowest(ctx, &candidates)
    }

    /// A strictly less used account with headroom for a bound thread under
    /// the quota strategy, once its own account may be left: spent (or
    /// over its threshold when `cache_affinity` is off). Checked at most
    /// once a minute while it may not be left. A higher tier that has room
    /// again is checked on every request ([`Self::priority_failback`]).
    fn reevaluate(&mut self, ctx: &Ctx, slot: &(i64, String)) -> Option<String> {
        if ctx.rules.strategy != Strategy::Quota {
            return None;
        }
        let (account, reeval) = {
            let binding = self.bindings.get(slot)?;
            (binding.account.clone(), binding.reeval_ms)
        };
        if let Some(higher) = self.priority_failback(ctx, &account) {
            if let Some(binding) = self.bindings.get_mut(slot) {
                binding.reeval_ms = ctx.now;
            }
            return Some(higher);
        }
        let threshold = ctx.threshold(&account);
        let usage = if threshold > 0 {
            self.score(ctx, &account)
        } else {
            0.0
        };
        let known = usage < UNKNOWN_SCORE;
        let may_rebind = if ctx.rules.cache_affinity {
            !self.selectable(ctx, &account) || (known && usage >= EXHAUSTED)
        } else {
            threshold > 0 && known && usage >= f64::from(threshold)
        };
        if !may_rebind && ctx.now.saturating_sub(reeval) < AFFINITY_REEVAL {
            return None;
        }
        if let Some(binding) = self.bindings.get_mut(slot) {
            binding.reeval_ms = ctx.now;
        }
        if !may_rebind {
            return None;
        }
        let candidates: Vec<String> = self
            .eligible(ctx, Some(&account), true)
            .into_iter()
            .filter(|id| self.headroom(ctx, id))
            .collect();
        let best = self.lowest(ctx, &candidates)?;
        (self.score(ctx, &best) < usage).then_some(best)
    }

    fn bind(&mut self, ctx: &Ctx, thread: i64, account: &str, writes: &mut Vec<Write>) {
        let slot = (thread, ctx.provider.to_string());
        self.bindings.insert(
            slot,
            Binding {
                kind: ctx.kind.to_string(),
                account: account.to_string(),
                bound_ms: ctx.now,
                used_ms: ctx.now,
                written_ms: ctx.now,
                reeval_ms: ctx.now,
                hold_since: None,
                detour: None,
            },
        );
        writes.push(Write::Bind(PoolBinding {
            thread: SessionId(thread),
            provider: ctx.provider.to_string(),
            kind: ctx.kind.to_string(),
            account: account.to_string(),
            bound_ms: ctx.now,
            used_ms: ctx.now,
        }));
        if self.bindings.len() > MAX_BINDINGS {
            let oldest = self
                .bindings
                .iter()
                .min_by_key(|(_, binding)| binding.used_ms)
                .map(|(slot, _)| slot.clone());
            if let Some(oldest) = oldest {
                self.bindings.remove(&oldest);
                writes.push(Write::Unbind(SessionId(oldest.0), oldest.1));
            }
        }
    }

    fn touch(&mut self, ctx: &Ctx, slot: &(i64, String), writes: &mut Vec<Write>) {
        let Some(binding) = self.bindings.get_mut(slot) else {
            return;
        };
        binding.used_ms = ctx.now;
        binding.hold_since = None;
        binding.detour = None;
        if ctx.now.saturating_sub(binding.written_ms) >= TOUCH_WRITE {
            binding.written_ms = ctx.now;
            writes.push(Write::Bind(PoolBinding {
                thread: SessionId(slot.0),
                provider: slot.1.clone(),
                kind: binding.kind.clone(),
                account: binding.account.clone(),
                bound_ms: binding.bound_ms,
                used_ms: ctx.now,
            }));
        }
    }

    fn unbind(&mut self, slot: &(i64, String), writes: &mut Vec<Write>) {
        if self.bindings.remove(slot).is_some() {
            writes.push(Write::Unbind(SessionId(slot.0), slot.1.clone()));
        }
    }

    fn unbind_account(&mut self, kind: &str, account: &str, writes: &mut Vec<Write>) {
        self.bindings
            .retain(|_, binding| !(binding.kind == kind && binding.account == account));
        writes.push(Write::UnbindAccount(kind.to_string(), account.to_string()));
    }

    /// The account of the request of `thread` (none outside a thread);
    /// `None` when no account is usable at all. A thread that never had an
    /// account of the provider starts where its parent or a sibling is
    /// served ([`Self::lineage_pick`]).
    fn resolve(
        &mut self,
        ctx: &Ctx,
        thread: Option<i64>,
        writes: &mut Vec<Write>,
    ) -> Option<String> {
        self.sync_pin(ctx, writes);
        if let Some(thread) = thread {
            let had = self
                .bindings
                .contains_key(&(thread, ctx.provider.to_string()));
            if let Some(account) = self.resolve_bound(ctx, thread, writes) {
                return Some(account);
            }
            if !had {
                if let Some(account) = self.lineage_pick(ctx, thread) {
                    self.bind(ctx, thread, &account, writes);
                    return Some(account);
                }
            }
        }
        let placed = self.place(ctx, true)?;
        if let Some(thread) = thread {
            if self.selectable(ctx, &placed) {
                self.bind(ctx, thread, &placed, writes);
            }
        }
        Some(placed)
    }

    /// The account serving the thread `other` of the provider: its detour
    /// while a transient hold lasts, else its own, when that can take a
    /// request now (reference `lineageServingAccountId`).
    fn serving(&self, ctx: &Ctx, other: i64) -> Option<String> {
        let binding = self.bindings.get(&(other, ctx.provider.to_string()))?;
        if binding.kind != ctx.kind || ctx.now.saturating_sub(binding.used_ms) >= AFFINITY_IDLE {
            return None;
        }
        let held = binding
            .hold_since
            .is_some_and(|since| ctx.now.saturating_sub(since) < TRANSIENT_HOLD);
        let serving = match (&binding.detour, held) {
            (Some(detour), true) => detour.clone(),
            _ => binding.account.clone(),
        };
        (self.selectable(ctx, &serving)
            && !self.quota_refused(ctx, &serving)
            && !self.should_failover(ctx, &serving))
        .then_some(serving)
    }

    /// Where a forked thread starts: its parent's serving account, else one
    /// of up to [`MAX_SIBLINGS`] siblings' (newest first), for the prompt
    /// cache they share (reference `pickLineageServingAccount`).
    fn lineage_pick(&self, ctx: &Ctx, thread: i64) -> Option<String> {
        let parent = *self.parents.get(&thread)?;
        if let Some(account) = self.serving(ctx, parent) {
            return Some(account);
        }
        let mut siblings: Vec<i64> = self
            .parents
            .iter()
            .filter(|(child, of)| **of == parent && **child != thread)
            .map(|(child, _)| *child)
            .collect();
        siblings.sort_unstable_by(|a, b| b.cmp(a));
        siblings
            .into_iter()
            .take(MAX_SIBLINGS)
            .find_map(|sibling| self.serving(ctx, sibling))
    }

    /// Another account fit for the request's model when `account` is not
    /// ([`Self::model_unfit`]); the binding stays, since the thread's other
    /// models may still use `account`.
    fn fit_alternative(&self, ctx: &Ctx, account: &str) -> Option<String> {
        if !self.model_unfit(ctx, account) {
            return None;
        }
        let fit: Vec<String> = self
            .eligible(ctx, Some(account), true)
            .into_iter()
            .filter(|id| !self.model_unfit(ctx, id))
            .collect();
        self.lowest(ctx, &fit)
    }

    /// The bound account of `thread` while it can serve, a detour while
    /// it has transient trouble, else `None` after releasing the binding.
    fn resolve_bound(&mut self, ctx: &Ctx, thread: i64, writes: &mut Vec<Write>) -> Option<String> {
        let slot = (thread, ctx.provider.to_string());
        let binding = self.bindings.get(&slot)?.clone();
        if binding.kind != ctx.kind
            || ctx.member(&binding.account).is_none()
            || ctx.now.saturating_sub(binding.used_ms) >= AFFINITY_IDLE
        {
            self.unbind(&slot, writes);
            return None;
        }
        let account = binding.account.clone();
        if self.selectable(ctx, &account)
            && !self.quota_refused(ctx, &account)
            && !self.should_failover(ctx, &account)
        {
            self.touch(ctx, &slot, writes);
            if let Some(cooler) = self.reevaluate(ctx, &slot) {
                self.bind(ctx, thread, &cooler, writes);
                return Some(cooler);
            }
            return Some(self.fit_alternative(ctx, &account).unwrap_or(account));
        }
        let hold_expired = binding
            .hold_since
            .is_some_and(|since| ctx.now.saturating_sub(since) >= TRANSIENT_HOLD);
        if !hold_expired && self.transient_only(ctx, &account) {
            let remembered = binding.detour.clone().filter(|detour| {
                self.selectable(ctx, detour) && !self.should_failover(ctx, detour)
            });
            let detour = remembered.or_else(|| self.alternate(ctx, &account));
            if let Some(entry) = self.bindings.get_mut(&slot) {
                entry.hold_since.get_or_insert(ctx.now);
                entry.used_ms = ctx.now;
                entry.detour = detour.clone();
            }
            return Some(detour.unwrap_or(account));
        }
        if let Some(detour) = binding.detour.filter(|detour| {
            hold_expired
                && *detour != account
                && self.selectable(ctx, detour)
                && !self.quota_refused(ctx, detour)
                && !self.should_failover(ctx, detour)
        }) {
            self.bind(ctx, thread, &detour, writes);
            return Some(detour);
        }
        self.unbind(&slot, writes);
        None
    }

    /// Where a new conversation goes: the strategy's pick, or under the
    /// quota strategy the provider's current account while it serves and
    /// has headroom.
    fn place(&mut self, ctx: &Ctx, commit: bool) -> Option<String> {
        let placed = self.place_shared(ctx, commit)?;
        Some(self.fit_alternative(ctx, &placed).unwrap_or(placed))
    }

    /// An account whose 5-hour window has not started, for a new
    /// conversation to start it, when `start_idle_windows` is on and no
    /// account is chosen by hand: its short window reads 0% of a stated 5
    /// hours, read in the last 5 minutes, with its reset 5 hours after that
    /// read (within a minute), and it was not sent one for that window yet
    /// (reference `pickIdleWindowAccount`).
    fn idle_window(&mut self, ctx: &Ctx, commit: bool) -> Option<String> {
        if !ctx.rules.start_idle_windows || ctx.independent_scope() || ctx.pinned.is_some() {
            return None;
        }
        let window = IDLE_WINDOW_SECONDS * SECOND;
        let picked = self.eligible(ctx, None, false).into_iter().find(|id| {
            let slot = key(ctx.kind, id);
            let Some(quota) = self.quota.get(&slot) else {
                return false;
            };
            let (Some(seen), Some(reset)) = (quota.short_observed_ms, quota.short_reset_ms) else {
                return false;
            };
            let steered = self.steered.get(&slot);
            self.headroom(ctx, id)
                && !self.model_unfit(ctx, id)
                && quota.short == Some(0.0)
                && quota.short_seconds == Some(IDLE_WINDOW_SECONDS)
                && ctx.now >= seen
                && ctx.now - seen <= IDLE_WINDOW_FRESHNESS
                && reset.abs_diff(seen + window) <= IDLE_WINDOW_TOLERANCE
                && reset > ctx.now
                && steered.is_none_or(|until| ctx.now > *until && seen > *until)
        })?;
        if commit {
            let now = ctx.now;
            self.steered
                .retain(|_, until| *until + IDLE_WINDOW_FRESHNESS >= now);
            let reset = self
                .quota
                .get(&key(ctx.kind, &picked))
                .and_then(|quota| quota.short_reset_ms)
                .unwrap_or(now);
            self.steered.insert(
                key(ctx.kind, &picked),
                reset.max(now + window) + IDLE_WINDOW_TOLERANCE,
            );
        }
        Some(picked)
    }

    fn place_shared(&mut self, ctx: &Ctx, commit: bool) -> Option<String> {
        if matches!(ctx.rules.strategy, Strategy::Quota | Strategy::FillFirst) {
            if let Some(pinned) = self.usable_pin(ctx) {
                self.set_cursor(ctx, &pinned);
            }
        }
        if let Some(idle) = self.idle_window(ctx, commit) {
            return Some(idle);
        }
        if let Some(picked) = self.strategy_pick(ctx, commit) {
            return Some(picked);
        }
        let mut active = match self.cursor(ctx) {
            Some(active) => active,
            None => {
                let eligible = self.eligible(ctx, None, false);
                let chosen = self
                    .lowest(ctx, &eligible)
                    .or_else(|| self.stay(ctx, None))?;
                self.set_cursor(ctx, &chosen);
                chosen
            }
        };
        if !self.selectable(ctx, &active) {
            let eligible = self.eligible(ctx, Some(&active), false);
            match self.lowest(ctx, &eligible) {
                Some(fallback) => {
                    self.set_cursor(ctx, &fallback);
                    active = fallback;
                }
                None => return self.stay(ctx, Some(&active)),
            }
        }
        if let Some(preempted) = self.preemption(ctx, &active) {
            self.set_cursor(ctx, &preempted);
            active = preempted;
        }
        let active = self.auto_switch(ctx, active);
        Some(self.failure_failover(ctx, active))
    }

    /// With nowhere else to go: `active` when it is a usable account, else
    /// the usable account whose cooldown ends first. The provider then
    /// answers for itself, which is also how a lapsed refusal is noticed.
    /// An account kept out by its plan or from spending credits is never
    /// sent to (reference `isCodexAccountRotationExcluded`).
    fn stay(&self, ctx: &Ctx, active: Option<&str>) -> Option<String> {
        let open = |id: &str| {
            ctx.member(id).is_some_and(Member::usable)
                && !self.plan_excluded(ctx, id)
                && !self.credits_held(ctx, id)
        };
        if let Some(active) = active.filter(|id| open(id)) {
            return Some(active.to_string());
        }
        ctx.members
            .iter()
            .filter(|member| open(&member.id))
            .min_by_key(|member| {
                self.health(ctx, &member.id)
                    .and_then(|h| h.cooldown_until.max(h.soft_avoid_until))
                    .unwrap_or(0)
            })
            .map(|member| member.id.clone())
    }

    /// Records a refusal of `account` in the quota group `scope` only.
    fn cool_scope(
        &mut self,
        ctx: &Ctx,
        account: &str,
        scope: &str,
        refusal: Scoped,
        writes: &mut Vec<Write>,
    ) {
        writes.push(Write::Cooldown(PoolCooldown {
            kind: ctx.kind.to_string(),
            account: account.to_string(),
            scope: Some(scope.to_string()),
            until_ms: refusal.until,
            avoid_until_ms: refusal.avoid_until,
            since_ms: refusal.since,
            source: refusal.source.name().to_string(),
            status: refusal.status,
        }));
        self.scoped
            .insert((key(ctx.kind, account), scope.to_string()), refusal);
    }

    /// Moves the provider's placement off `account` after it refused.
    fn leave(&mut self, ctx: &Ctx, account: &str) {
        let state = self.state(ctx);
        if state.sticky.as_deref() == Some(account) {
            state.sticky = None;
            state.sticky_count = 0;
        }
        if self.cursor(ctx).as_deref() == Some(account) {
            if let Some(next) = self.alternate(ctx, account) {
                self.set_cursor(ctx, &next);
            }
        }
    }

    /// Records what `account` did, and moves the provider's placement away
    /// from it when it refused on quota or keeps failing.
    fn record(
        &mut self,
        ctx: &Ctx,
        account: &str,
        thread: Option<i64>,
        outcome: &Outcome,
        writes: &mut Vec<Write>,
    ) {
        self.sync_pin(ctx, writes);
        let now = ctx.now;
        let class = classify(ctx.kind, outcome);
        let failover_on = ctx.rules.failover_threshold > 0;
        let slot = key(ctx.kind, account);
        match class {
            Class::Neutral | Class::Caller | Class::Unreachable => {}
            Class::Success => {
                self.denials.remove(&(slot.clone(), ctx.model.to_string()));
                if let Some(scope) = ctx.scope() {
                    let scoped = (slot.clone(), scope.to_string());
                    if ctx.family().is_some() {
                        if self.scoped.remove(&scoped).is_some() {
                            writes.push(Write::UncoolScope(
                                ctx.kind.to_string(),
                                account.to_string(),
                                scope.to_string(),
                            ));
                        }
                    } else if let Some(entry) = self.scoped.get_mut(&scoped) {
                        entry.refused = false;
                    }
                }
                let health = self.health_mut(ctx.kind, account);
                health.window.push(now, false);
                health.quota_refused = false;
                if failover_on && health.consecutive_failures >= 2 {
                    health.consecutive_successes += 1;
                    if health.consecutive_successes < 2 {
                        return;
                    }
                }
                health.consecutive_failures = 0;
                health.consecutive_successes = 0;
                health.soft_avoid_until = None;
            }
            Class::Workspace => {
                let health = self.health_mut(ctx.kind, account);
                health.consecutive_failures += 1;
                health.last_failure_ms = Some(now);
                note(health, outcome);
            }
            Class::Credential => {
                let health = self.health_mut(ctx.kind, account);
                health.consecutive_failures = 1;
                health.consecutive_successes = 0;
                health.last_failure_ms = Some(now);
                let cooled = health.cooldown_until.is_some();
                health.clear_cooldown();
                note(health, outcome);
                if cooled {
                    writes.push(Write::Uncool(ctx.kind.to_string(), account.to_string()));
                }
                self.unbind_account(ctx.kind, account, writes);
            }
            Class::Rate => {
                let Outcome::Status { retry_after, .. } = outcome else {
                    return;
                };
                let wait = retry_after_ms(retry_after.as_deref(), now).unwrap_or(SHORT_RETRY);
                let health = self.health_mut(ctx.kind, account);
                health.rate_pause_until =
                    Some(health.rate_pause_until.unwrap_or(0).max(now + wait));
                note(health, outcome);
            }
            Class::ModelDenied => {
                self.denials
                    .insert((slot, ctx.model.to_string()), now + MODEL_DENIAL);
                note(self.health_mut(ctx.kind, account), outcome);
            }
            Class::Family => {
                let Some(scope) = ctx.family() else {
                    return;
                };
                let Outcome::Status { status, resets, .. } = outcome else {
                    return;
                };
                let until = resets
                    .iter()
                    .copied()
                    .filter(|reset| *reset > now)
                    .min()
                    .unwrap_or(u64::MAX)
                    .min(now + FAMILY_REFUSAL_MAX_AGE);
                let refusal = Scoped {
                    until,
                    avoid_until: until,
                    since: now,
                    source: CooldownSource::Reset,
                    status: *status,
                    refused: true,
                };
                self.cool_scope(ctx, account, scope, refusal, writes);
                self.quota
                    .entry(slot)
                    .or_default()
                    .families
                    .insert(scope.to_string(), EXHAUSTED);
                note(self.health_mut(ctx.kind, account), outcome);
            }
            Class::KeyRefused | Class::Quota => {
                let Outcome::Status {
                    status,
                    retry_after,
                    resets,
                    refusal,
                    ..
                } = outcome
                else {
                    return;
                };
                let (until, source, avoid) = if is_key(ctx.kind) {
                    // Reference `rotateKeyAfterFailure`: a refused key waits
                    // the cap, a 429 its Retry-After up to the cap.
                    let delay = match class {
                        Class::KeyRefused => Some(MAX_KEY_COOLDOWN),
                        _ => retry_after_ms(retry_after.as_deref(), now)
                            .map(|delay| delay.min(MAX_KEY_COOLDOWN)),
                    };
                    let (until, source) = match delay {
                        Some(delay) if class == Class::Quota => {
                            (now + delay, CooldownSource::RetryAfter)
                        }
                        Some(delay) => (now + delay, CooldownSource::Default),
                        None => (now + DEFAULT_QUOTA_COOLDOWN, CooldownSource::Default),
                    };
                    (until, source, until)
                } else if *refusal == Refusal::Account {
                    match retry_after_ms(retry_after.as_deref(), now) {
                        Some(delay) => (now + delay, CooldownSource::RetryAfter, now + delay),
                        None => {
                            let until = now + ACCOUNT_REFUSAL_COOLDOWN;
                            (until, CooldownSource::Default, until)
                        }
                    }
                } else {
                    let (until, source) = quota_cooldown(retry_after.as_deref(), resets, now);
                    (until, source, quota_avoid_until(resets, now, until))
                };
                // Reference `recordCodexUpstreamOutcome`: a reset-derived
                // ChatGPT refusal belongs to the model's quota group.
                if ctx.kind == "gpt-oauth" && source == CooldownSource::Reset {
                    if let Some(scope) = ctx.scope() {
                        let refusal = Scoped {
                            until,
                            avoid_until: avoid,
                            since: now,
                            source,
                            status: *status,
                            refused: true,
                        };
                        self.cool_scope(ctx, account, scope, refusal, writes);
                        note(self.health_mut(ctx.kind, account), outcome);
                        if scope == "shared" {
                            self.unbind_account(ctx.kind, account, writes);
                            self.leave(ctx, account);
                        }
                        return;
                    }
                }
                let health = self.health_mut(ctx.kind, account);
                health.consecutive_failures = 0;
                health.consecutive_successes = 0;
                health.last_failure_ms = Some(now);
                health.cooldown_until = Some(until);
                health.cooldown_since = Some(now);
                health.cooldown_source = Some(source);
                health.quota_avoid_until = Some(avoid);
                health.quota_refused = true;
                note(health, outcome);
                writes.push(Write::Cooldown(PoolCooldown {
                    kind: ctx.kind.to_string(),
                    account: account.to_string(),
                    scope: None,
                    until_ms: until,
                    avoid_until_ms: avoid,
                    since_ms: now,
                    source: source.name().to_string(),
                    status: *status,
                }));
                self.unbind_account(ctx.kind, account, writes);
                self.leave(ctx, account);
            }
            Class::Transient => {
                let threshold = ctx.rules.failover_threshold;
                let health = self.health_mut(ctx.kind, account);
                health.window.push(now, true);
                let stale = health
                    .last_failure_ms
                    .is_some_and(|at| now.saturating_sub(at) > STREAK_GAP);
                health.consecutive_failures = if stale {
                    1
                } else {
                    health.consecutive_failures + 1
                };
                health.consecutive_successes = 0;
                health.last_failure_ms = Some(now);
                note(health, outcome);
                let ready = threshold > 0 && health.consecutive_failures >= threshold;
                if ready {
                    let step = (health.consecutive_failures - threshold) as usize;
                    let avoid = now + SOFT_AVOID_STEPS[step.min(SOFT_AVOID_STEPS.len() - 1)];
                    health.soft_avoid_until = Some(health.soft_avoid_until.unwrap_or(0).max(avoid));
                }
                if ready {
                    if let Some(thread) = thread {
                        let slot = (thread, ctx.provider.to_string());
                        let spent = self.bindings.get(&slot).is_some_and(|binding| {
                            binding.account == account
                                && binding.hold_since.is_some_and(|since| {
                                    now.saturating_sub(since) >= TRANSIENT_HOLD
                                })
                        });
                        if spent {
                            self.unbind(&slot, writes);
                        }
                    }
                }
                if self.cursor(ctx).as_deref() == Some(account) {
                    self.failure_failover(ctx, account.to_string());
                }
            }
        }
    }

    /// Accounts whose usage the pool should read: only when it chooses
    /// among several usable accounts by usage, for each account not read in
    /// the last [`USAGE_REFRESH`], and for each account in a cooldown not
    /// set by `Retry-After` every [`PROBE_INTERVAL`], so a cooldown can end
    /// early when the usage shows room again.
    fn due_reads(&mut self, ctx: &Ctx) -> Vec<String> {
        let usable: Vec<&Member> = ctx.members.iter().filter(|m| m.usable()).collect();
        let by_usage = ctx.rules.strategy == Strategy::Quota
            || ctx.rules.strategy == Strategy::ResetFirst
            || usable.iter().any(|member| ctx.threshold(&member.id) > 0);
        if usable.len() < 2 || !by_usage || is_key(ctx.kind) {
            return Vec::new();
        }
        let refresh = if ctx.rules.priority_failback {
            FAILBACK_REFRESH
        } else {
            USAGE_REFRESH
        };
        let mut due = Vec::new();
        for member in usable {
            let slot = key(ctx.kind, &member.id);
            if self.polling.contains(&slot) {
                continue;
            }
            let health = self.health.get(&slot);
            let probe = health.is_some_and(|h| {
                h.cooling(ctx.now)
                    && h.cooldown_source != Some(CooldownSource::RetryAfter)
                    && ctx
                        .now
                        .saturating_sub(h.last_probe_ms.or(h.cooldown_since).unwrap_or(0))
                        >= PROBE_INTERVAL
            });
            let stale = self
                .quota
                .get(&slot)
                .is_none_or(|quota| ctx.now.saturating_sub(quota.updated_ms) >= refresh)
                && self
                    .polled
                    .get(&slot)
                    .is_none_or(|at| ctx.now.saturating_sub(*at) >= refresh);
            if probe || stale {
                if probe {
                    self.health_mut(ctx.kind, &member.id).last_probe_ms = Some(ctx.now);
                }
                self.polled.insert(slot.clone(), ctx.now);
                self.polling.insert(slot);
                due.push(member.id.clone());
            }
        }
        due
    }
}

fn note(health: &mut Health, outcome: &Outcome) {
    match outcome {
        Outcome::Status {
            status, message, ..
        } => {
            health.last_status = Some(*status);
            health.last_error = Some(message.clone());
        }
        Outcome::Broken(message) | Outcome::Unreachable { message, .. } => {
            health.last_status = None;
            health.last_error = Some(message.clone());
        }
    }
}

impl Inner {
    /// Whether a request to `host` may go: always with no circuit
    /// (`threshold` 0); else not while its cooldown runs, and after it
    /// only one trial at a time (reference `acquireUpstreamHostAdmission`).
    /// `Err` holds the seconds to wait.
    fn admit_host(&mut self, host: &str, threshold: u32, now: u64) -> Result<(), u64> {
        if threshold == 0 {
            return Ok(());
        }
        let Some(entry) = self.hosts.get_mut(host) else {
            return Ok(());
        };
        match entry.cooldown_until {
            Some(until) if until > now => Err((until - now).div_ceil(SECOND).max(1)),
            Some(_)
                if entry
                    .trial
                    .is_some_and(|since| now.saturating_sub(since) < HOST_CIRCUIT_COOLDOWN) =>
            {
                Err(1)
            }
            Some(_) => {
                entry.trial = Some(now);
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// A request to `host` that never reached it (reference
    /// `recordUpstreamHostFailure`): failures within 10 minutes of each
    /// other add up; with a circuit, the threshold, or a failed trial,
    /// starts a 30-second cooldown.
    fn host_failed(&mut self, host: &str, code: &str, threshold: u32, now: u64) {
        if !self.hosts.contains_key(host) && self.hosts.len() >= MAX_HOSTS {
            let stalest = self
                .hosts
                .iter()
                .min_by_key(|(_, entry)| entry.last_failure_ms)
                .map(|(name, _)| name.clone());
            if let Some(stalest) = stalest {
                self.hosts.remove(&stalest);
            }
        }
        let entry = self.hosts.entry(host.to_string()).or_default();
        let reopens = entry.trial.is_some() || entry.cooldown_until.is_some();
        let stale = entry.failures == 0
            || (!reopens && now.saturating_sub(entry.last_failure_ms) > HOST_FAILURE_WINDOW);
        entry.failures = if reopens && threshold > 0 {
            threshold.max(entry.failures + 1)
        } else if stale {
            1
        } else {
            entry.failures + 1
        };
        entry.last_failure_ms = now;
        if !code.is_empty() {
            entry.last_code = Some(code.to_string());
        }
        entry.trial = None;
        entry.cooldown_until = (threshold > 0 && (reopens || entry.failures >= threshold))
            .then_some(now + HOST_CIRCUIT_COOLDOWN);
    }

    /// Any answer from `host` proves it is reachable (reference
    /// `resetUpstreamHostHealth`).
    fn host_reached(&mut self, host: &str) {
        self.hosts.remove(host);
    }
}

/// The pool of every provider of one core.
pub struct Pool {
    inner: Mutex<Inner>,
    store: Option<Store>,
    loaded: tokio::sync::OnceCell<()>,
}

impl Pool {
    pub fn new(store: Option<Store>) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            store,
            loaded: tokio::sync::OnceCell::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Reads the bindings and cooldowns the database kept, once.
    async fn ready(&self) {
        let Some(store) = &self.store else {
            return;
        };
        self.loaded
            .get_or_init(|| async {
                let now = now_ms();
                let bindings = store
                    .pool_bindings(now.saturating_sub(AFFINITY_IDLE))
                    .await
                    .unwrap_or_default();
                let cooldowns = store.pool_cooldowns(now).await.unwrap_or_default();
                let lineage = store.thread_lineage().await.unwrap_or_default();
                let mut inner = self.lock();
                inner.parents.extend(lineage);
                for row in bindings {
                    inner
                        .bindings
                        .entry((row.thread.0, row.provider))
                        .or_insert(Binding {
                            kind: row.kind,
                            account: row.account,
                            bound_ms: row.bound_ms,
                            used_ms: row.used_ms,
                            written_ms: row.used_ms,
                            reeval_ms: 0,
                            hold_since: None,
                            detour: None,
                        });
                }
                for row in cooldowns {
                    if let Some(scope) = row.scope {
                        inner
                            .scoped
                            .entry((key(&row.kind, &row.account), scope))
                            .or_insert(Scoped {
                                until: row.until_ms,
                                avoid_until: row.avoid_until_ms,
                                since: row.since_ms,
                                source: CooldownSource::parse(&row.source),
                                status: row.status,
                                refused: true,
                            });
                        continue;
                    }
                    let health = inner.health_mut(&row.kind, &row.account);
                    if health.cooldown_until.is_none() {
                        health.cooldown_until = Some(row.until_ms);
                        health.cooldown_since = Some(row.since_ms);
                        health.cooldown_source = Some(CooldownSource::parse(&row.source));
                        health.quota_avoid_until = Some(row.avoid_until_ms);
                        health.last_status = Some(row.status);
                        health.last_failure_ms = Some(row.since_ms);
                        health.quota_refused = true;
                    }
                }
            })
            .await;
    }

    async fn persist(&self, writes: Vec<Write>) {
        let Some(store) = &self.store else {
            return;
        };
        for write in writes {
            let written = match write {
                Write::Bind(binding) => store.set_pool_binding(binding).await,
                Write::Unbind(thread, provider) => {
                    store.delete_pool_binding(thread, provider).await
                }
                Write::UnbindAccount(kind, account) => {
                    store.delete_pool_bindings_to(kind, account).await
                }
                Write::Cooldown(cooldown) => store.set_pool_cooldown(cooldown).await,
                Write::Uncool(kind, account) => {
                    store.delete_pool_cooldown(kind, account, None).await
                }
                Write::UncoolScope(kind, account, scope) => {
                    store.delete_pool_cooldown(kind, account, Some(scope)).await
                }
                Write::Clear(provider, thread) => store.clear_pool_bindings(provider, thread).await,
            };
            if let Err(error) = written {
                eprintln!("inocode: the account pool was not saved: {}", error.message);
            }
        }
    }

    /// The account for a request of `thread` and the accounts whose usage
    /// to read; `None` when no account is usable.
    pub(crate) async fn choose(
        &self,
        ctx: &Ctx<'_>,
        thread: Option<SessionId>,
    ) -> (Option<String>, Vec<String>) {
        self.ready().await;
        let mut writes = Vec::new();
        let (chosen, due) = {
            let mut inner = self.lock();
            for health in inner.health.values_mut() {
                health.window.recompute(ctx.now);
            }
            let chosen = inner.resolve(ctx, thread.map(|thread| thread.0), &mut writes);
            (chosen, inner.due_reads(ctx))
        };
        self.persist(writes).await;
        (chosen, due)
    }

    /// The account `thread` is bound to for `provider`, if any.
    pub(crate) async fn bound(&self, provider: &str, thread: SessionId) -> Option<String> {
        self.ready().await;
        self.lock()
            .bindings
            .get(&(thread.0, provider.to_string()))
            .map(|binding| binding.account.clone())
    }

    /// The account a new conversation would get now, changing nothing.
    pub(crate) async fn peek(&self, ctx: &Ctx<'_>) -> Option<String> {
        self.ready().await;
        let mut scratch = self.lock().clone();
        for health in scratch.health.values_mut() {
            health.window.recompute(ctx.now);
        }
        scratch.sync_pin(ctx, &mut Vec::new());
        scratch.place(ctx, false)
    }

    pub(crate) async fn record(
        &self,
        ctx: &Ctx<'_>,
        account: &str,
        thread: Option<SessionId>,
        outcome: &Outcome,
    ) {
        self.ready().await;
        let mut writes = Vec::new();
        self.lock().record(
            ctx,
            account,
            thread.map(|thread| thread.0),
            outcome,
            &mut writes,
        );
        self.persist(writes).await;
    }

    /// Takes in usage windows read from a response or the usage endpoint.
    pub(crate) fn observe(&self, kind: &str, account: &str, quota: Quota) {
        let mut inner = self.lock();
        inner
            .quota
            .entry(key(kind, account))
            .or_default()
            .merge(&quota);
    }

    /// Ends a usage read; a read that shows room ends a cooldown not set
    /// by `Retry-After`.
    pub(crate) async fn read_done(&self, kind: &str, account: &str, quota: Option<Quota>) {
        let mut writes = Vec::new();
        {
            let mut inner = self.lock();
            let slot = key(kind, account);
            inner.polling.remove(&slot);
            let Some(quota) = quota else {
                return;
            };
            let now = now_ms();
            let entry = inner.quota.entry(slot.clone()).or_default();
            entry.merge(&quota);
            let roomy = entry.score(now) < EXHAUSTED;
            if let Some(health) = inner.health.get_mut(&slot) {
                if roomy
                    && health.cooling(now)
                    && health.cooldown_source != Some(CooldownSource::RetryAfter)
                {
                    health.clear_cooldown();
                    writes.push(Write::Uncool(kind.to_string(), account.to_string()));
                }
            }
        }
        self.persist(writes).await;
    }

    /// Ends a cooldown and quota avoidance by hand, the account's and its
    /// quota groups'; whether one was running.
    pub(crate) async fn clear_cooldown(&self, kind: &str, account: &str) -> bool {
        self.ready().await;
        let now = now_ms();
        let cleared = {
            let mut inner = self.lock();
            let slot = key(kind, account);
            let before = inner.scoped.len();
            inner.scoped.retain(|(owner, _), _| *owner != slot);
            let scoped = inner.scoped.len() != before;
            let own = match inner.health.get_mut(&slot) {
                Some(health) if health.cooling(now) || health.quota_avoided(now) => {
                    health.clear_cooldown();
                    health.last_probe_ms = Some(now);
                    true
                }
                _ => false,
            };
            own || scoped
        };
        if cleared {
            self.persist(vec![Write::Uncool(kind.to_string(), account.to_string())])
                .await;
        }
        cleared
    }

    /// Forgets which account threads are on, for one provider, one thread,
    /// both, or all; how many bindings were removed.
    pub(crate) async fn reset_affinity(
        &self,
        provider: Option<&str>,
        thread: Option<SessionId>,
    ) -> usize {
        self.ready().await;
        let removed = {
            let mut inner = self.lock();
            let before = inner.bindings.len();
            inner.bindings.retain(|(bound, bound_provider), _| {
                !(provider.is_none_or(|p| p == bound_provider)
                    && thread.is_none_or(|t| t.0 == *bound))
            });
            before - inner.bindings.len()
        };
        self.persist(vec![Write::Clear(provider.map(str::to_string), thread)])
            .await;
        removed
    }

    /// Notes that `child` was forked from `parent`, so its first request
    /// starts on the parent's account.
    pub(crate) async fn note_lineage(&self, child: SessionId, parent: SessionId) {
        self.ready().await;
        self.lock().parents.insert(child.0, parent.0);
    }

    /// Whether a request to `host` may go now ([`Inner::admit_host`]).
    pub(crate) fn admit_host(&self, host: &str, threshold: u32) -> Result<(), u64> {
        self.lock().admit_host(host, threshold, now_ms())
    }

    /// Records whether a request reached `host` ([`Inner::host_failed`],
    /// [`Inner::host_reached`]); a request that broke after reaching it
    /// says neither.
    pub(crate) fn note_host(&self, host: &str, threshold: u32, outcome: &Outcome) {
        let mut inner = self.lock();
        match outcome {
            Outcome::Unreachable { code, .. } => inner.host_failed(host, code, threshold, now_ms()),
            Outcome::Status { .. } => inner.host_reached(host),
            Outcome::Broken(_) => {
                if let Some(entry) = inner.hosts.get_mut(host) {
                    entry.trial = None;
                }
            }
        }
    }

    /// Forgets everything about a removed account.
    pub(crate) async fn forget(&self, kind: &str, account: &str) {
        self.ready().await;
        let mut writes = vec![Write::Uncool(kind.to_string(), account.to_string())];
        {
            let mut inner = self.lock();
            let slot = key(kind, account);
            inner.health.remove(&slot);
            inner.quota.remove(&slot);
            inner.polled.remove(&slot);
            inner.steered.remove(&slot);
            inner.scoped.retain(|(owner, _), _| *owner != slot);
            inner.denials.retain(|(owner, _), _| *owner != slot);
            inner.unbind_account(kind, account, &mut writes);
            for state in inner.providers.values_mut() {
                if state.cursor.as_deref() == Some(account) {
                    state.cursor = None;
                }
                if state.sticky.as_deref() == Some(account) {
                    state.sticky = None;
                }
                state.weights.remove(account);
            }
        }
        self.persist(writes).await;
    }

    /// `pool/status` of one provider, its host's reachability included.
    pub(crate) async fn status(&self, ctx: &Ctx<'_>, accounts: &[Account], host: &str) -> Value {
        let current = self.peek(ctx).await;
        let mut inner = self.lock();
        for health in inner.health.values_mut() {
            health.window.recompute(ctx.now);
        }
        let inner = &*inner;
        let rows: Vec<Value> = accounts
            .iter()
            .map(|account| {
                let id = account.id.as_str();
                let health = inner.health(ctx, id);
                let quota = inner.quota.get(&key(ctx.kind, id));
                let score = inner.score(ctx, id);
                let active = |time: Option<u64>| time.filter(|time| *time > ctx.now);
                let scoped: serde_json::Map<String, Value> = inner
                    .scoped
                    .iter()
                    .filter(|((owner, _), refusal)| {
                        *owner == key(ctx.kind, id)
                            && refusal.until.max(refusal.avoid_until) > ctx.now
                    })
                    .map(|((_, scope), refusal)| {
                        (
                            scope.clone(),
                            json!({
                                "cooldown_until": active(Some(refusal.until)),
                                "cooldown_source": refusal.source.name(),
                                "quota_avoid_until": active(Some(refusal.avoid_until)),
                                "status": refusal.status,
                            }),
                        )
                    })
                    .collect();
                let denied: Vec<&String> = inner
                    .denials
                    .iter()
                    .filter(|((owner, _), until)| *owner == key(ctx.kind, id) && **until > ctx.now)
                    .map(|((_, model), _)| model)
                    .collect();
                json!({
                    "id": id,
                    "alias": account.info.alias,
                    "paused": account.info.paused,
                    "needs_reauth": account.info.needs_reauth,
                    "priority": account.info.priority.unwrap_or(0),
                    "auto_switch_threshold": account.info.auto_switch_threshold,
                    "credits": account.info.credits,
                    "plan": inner.plan(ctx, id),
                    "threshold": ctx.threshold(id),
                    "score": (score < UNKNOWN_SCORE).then_some(score),
                    "usage": quota.map(Quota::view),
                    "cooldown_until": active(health.and_then(|h| h.cooldown_until)),
                    "cooldown_source": health
                        .filter(|h| h.cooling(ctx.now))
                        .and_then(|h| h.cooldown_source)
                        .map(CooldownSource::name),
                    "quota_avoid_until": active(health.and_then(|h| h.quota_avoid_until)),
                    "soft_avoid_until": active(health.and_then(|h| h.soft_avoid_until)),
                    "rate_pause_until": active(health.and_then(|h| h.rate_pause_until)),
                    "scoped_cooldowns": scoped,
                    "denied_models": denied,
                    "reason": inner.block_reason(ctx, id),
                    "consecutive_failures": health.map_or(0, |h| h.consecutive_failures),
                    "degraded": inner.degraded(ctx, id),
                    "bound_threads": inner
                        .bindings
                        .iter()
                        .filter(|((_, provider), binding)| {
                            provider == ctx.provider && binding.account == id
                        })
                        .count(),
                    "last_status": health.and_then(|h| h.last_status),
                    "last_error": health.and_then(|h| h.last_error.clone()),
                    "current": current.as_deref() == Some(id),
                })
            })
            .collect();
        let host_view = inner.hosts.get(host).map(|entry| {
            json!({
                "host": host,
                "consecutive_failures": entry.failures,
                "last_failure": entry.last_failure_ms,
                "last_code": entry.last_code,
                "cooldown_until": entry.cooldown_until.filter(|until| *until > ctx.now),
            })
        });
        json!({
            "provider": ctx.provider,
            "kind": ctx.kind,
            "model": ctx.model,
            "pool": ctx.rules.view(),
            "active_account": ctx.pinned,
            "current": current,
            "host": host_view,
            "accounts": rows,
        })
    }
}

/// What a client needs to choose its provider's account per request.
#[derive(Clone)]
pub(crate) struct Route {
    pub pool: Arc<Pool>,
    pub provider: String,
    /// The credential store's provider name of the accounts.
    pub kind: String,
    pub rules: Rules,
    pub pinned: Option<String>,
    /// The provider's id and endpoint host, for the reachability ledger.
    pub host: String,
}

impl Route {
    pub(crate) fn ctx<'a>(&'a self, members: &'a [Member], model: &'a str, now: u64) -> Ctx<'a> {
        Ctx {
            provider: &self.provider,
            kind: &self.kind,
            rules: self.rules.clone(),
            pinned: self.pinned.as_deref(),
            members,
            model,
            now,
        }
    }

    /// Takes in the usage headers of a response on `account`.
    pub(crate) fn observe(&self, account: &str, headers: &reqwest::header::HeaderMap) {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        let now = now_ms();
        for quota in [codex_headers(&header, now), anthropic_headers(&header, now)]
            .into_iter()
            .flatten()
        {
            self.pool.observe(&self.kind, account, quota);
        }
    }

    /// Records what `account` did for a request of `model`, with the
    /// accounts as the credential store at `store` holds them now.
    pub(crate) async fn record(
        &self,
        store: &std::path::Path,
        account: &str,
        model: &str,
        thread: Option<SessionId>,
        outcome: &Outcome,
    ) {
        self.pool
            .note_host(&self.host, self.rules.host_circuit_threshold, outcome);
        let Ok(stored) = crate::oauth::Store::load(store.to_path_buf()) else {
            return;
        };
        let members: Vec<Member> = stored.accounts(&self.kind).iter().map(Member::of).collect();
        let ctx = self.ctx(&members, model, now_ms());
        self.pool.record(&ctx, account, thread, outcome).await;
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: &str) -> Member {
        Member {
            id: id.into(),
            priority: 0,
            threshold: None,
            paused: false,
            needs_reauth: false,
            plan: None,
            credits: false,
        }
    }

    fn ctx<'a>(members: &'a [Member], rules: Rules, pinned: Option<&'a str>, now: u64) -> Ctx<'a> {
        Ctx {
            provider: "p",
            kind: "claude-oauth",
            rules,
            pinned,
            members,
            model: "m",
            now,
        }
    }

    fn rules(strategy: Strategy) -> Rules {
        Rules {
            strategy,
            ..Rules::default()
        }
    }

    fn weekly(inner: &mut Inner, id: &str, percent: f64) {
        inner.quota.insert(
            key("claude-oauth", id),
            Quota {
                weekly: Some(percent),
                updated_ms: 1,
                ..Quota::default()
            },
        );
    }

    fn quota_refusal(retry_after: Option<&str>, resets: Vec<u64>) -> Outcome {
        Outcome::Status {
            status: 429,
            retry_after: retry_after.map(str::to_string),
            resets,
            denial: false,
            refusal: Refusal::Plain,
            message: "provider returned HTTP 429".into(),
        }
    }

    fn ok() -> Outcome {
        Outcome::Status {
            status: 200,
            retry_after: None,
            resets: Vec::new(),
            denial: false,
            refusal: Refusal::Plain,
            message: String::new(),
        }
    }

    fn server_error() -> Outcome {
        Outcome::Status {
            status: 503,
            retry_after: None,
            resets: Vec::new(),
            denial: false,
            refusal: Refusal::Plain,
            message: "provider returned HTTP 503".into(),
        }
    }

    const NOW: u64 = 1_800_000_000_000;

    #[test]
    fn quota_places_on_the_least_used_and_stays_until_the_threshold() {
        let members = [member("a"), member("b"), member("c")];
        let mut inner = Inner::default();
        weekly(&mut inner, "a", 50.0);
        weekly(&mut inner, "b", 10.0);
        weekly(&mut inner, "c", 30.0);
        let c = ctx(&members, rules(Strategy::Quota), None, NOW);
        assert_eq!(inner.place(&c, true).as_deref(), Some("b"));
        weekly(&mut inner, "b", 60.0);
        assert_eq!(inner.place(&c, true).as_deref(), Some("b"), "under 80%");
        weekly(&mut inner, "b", 85.0);
        assert_eq!(
            inner.place(&c, true).as_deref(),
            Some("c"),
            "less used than b"
        );
        assert_eq!(
            inner.place(&c, true).as_deref(),
            Some("c"),
            "the cursor moved"
        );
    }

    #[test]
    fn unknown_usage_never_forces_a_switch_and_a_zero_threshold_turns_it_off() {
        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let c = ctx(&members, rules(Strategy::Quota), None, NOW);
        assert_eq!(
            inner.place(&c, true).as_deref(),
            Some("a"),
            "first on a tie"
        );
        weekly(&mut inner, "b", 1.0);
        assert_eq!(inner.place(&c, true).as_deref(), Some("a"), "a is unknown");
        let off = Rules {
            auto_switch_threshold: 0,
            ..Rules::default()
        };
        weekly(&mut inner, "a", 99.0);
        let c = ctx(&members, off, None, NOW);
        assert_eq!(inner.place(&c, true).as_deref(), Some("a"));
        let mut own = members.clone();
        own[0].threshold = Some(95);
        let c = ctx(&own, rules(Strategy::Quota), None, NOW);
        assert_eq!(
            inner.place(&c, true).as_deref(),
            Some("b"),
            "own threshold 95"
        );
    }

    #[test]
    fn round_robin_rotates_and_holds_for_the_sticky_limit() {
        let members = [member("a"), member("b"), member("c")];
        let mut inner = Inner::default();
        let c = ctx(&members, rules(Strategy::RoundRobin), None, NOW);
        let picks: Vec<String> = (0..6).map(|_| inner.place(&c, true).unwrap()).collect();
        assert_eq!(picks, ["a", "b", "c", "a", "b", "c"]);
        let sticky = Rules {
            sticky_limit: 2,
            ..rules(Strategy::RoundRobin)
        };
        let mut inner = Inner::default();
        let c = ctx(&members, sticky, None, NOW);
        let picks: Vec<String> = (0..6).map(|_| inner.place(&c, true).unwrap()).collect();
        assert_eq!(picks, ["a", "a", "b", "b", "c", "c"]);
        assert_eq!(inner.peek_place(&c).as_deref(), Some("a"));
        assert_eq!(
            inner.place(&c, true).as_deref(),
            Some("a"),
            "peeking changed nothing"
        );
        let mut inner = Inner::default();
        weekly(&mut inner, "b", 90.0);
        let c = ctx(&members, rules(Strategy::RoundRobin), None, NOW);
        let picks: Vec<String> = (0..4).map(|_| inner.place(&c, true).unwrap()).collect();
        assert_eq!(picks, ["a", "c", "a", "c"], "b is over its threshold");
    }

    #[test]
    fn fill_first_stays_until_the_threshold_then_takes_the_next_by_id() {
        let members = [member("c"), member("a"), member("b")];
        let mut inner = Inner::default();
        let c = ctx(&members, rules(Strategy::FillFirst), None, NOW);
        assert_eq!(inner.place(&c, true).as_deref(), Some("a"));
        assert_eq!(inner.place(&c, true).as_deref(), Some("a"));
        weekly(&mut inner, "a", 80.0);
        assert_eq!(inner.place(&c, true).as_deref(), Some("b"));
        weekly(&mut inner, "c", 95.0);
        weekly(&mut inner, "b", 90.0);
        assert_eq!(
            inner.place(&c, true).as_deref(),
            Some("c"),
            "all full: the next"
        );
    }

    #[test]
    fn reset_first_takes_the_soonest_reset_with_room() {
        let members = [member("a"), member("b"), member("c")];
        let mut inner = Inner::default();
        let window = |percent: f64, reset: u64| Quota {
            weekly: Some(percent),
            weekly_reset_ms: Some(reset),
            updated_ms: 1,
            ..Quota::default()
        };
        inner
            .quota
            .insert(key("claude-oauth", "a"), window(10.0, NOW + 5 * HOUR));
        inner
            .quota
            .insert(key("claude-oauth", "b"), window(70.0, NOW + HOUR));
        inner
            .quota
            .insert(key("claude-oauth", "c"), window(90.0, NOW + 60_000));
        let c = ctx(&members, rules(Strategy::ResetFirst), None, NOW);
        assert_eq!(inner.place(&c, true).as_deref(), Some("b"), "c has no room");
    }

    #[test]
    fn the_highest_tier_with_room_is_used_and_a_pin_caps_it() {
        let mut members = [member("low"), member("high"), member("mid")];
        members[1].priority = 10;
        members[2].priority = 5;
        let mut inner = Inner::default();
        let c = ctx(&members, rules(Strategy::Quota), None, NOW);
        assert_eq!(inner.place(&c, true).as_deref(), Some("high"));
        weekly(&mut inner, "high", 90.0);
        let mut inner2 = inner.clone();
        inner2.providers.clear();
        assert_eq!(
            inner2.place(&c, true).as_deref(),
            Some("mid"),
            "high is full"
        );
        let c = ctx(&members, rules(Strategy::RoundRobin), Some("low"), NOW);
        let mut inner3 = Inner::default();
        assert_eq!(inner3.place(&c, true).as_deref(), Some("low"), "the pin");
        assert_eq!(
            priority_tier(
                vec!["low".into(), "high".into(), "mid".into()],
                |id| c.priority(id),
                |_| true,
                Some("mid"),
            ),
            ["mid"],
            "a pinned tier is the ceiling"
        );
        weekly(&mut inner, "mid", 85.0);
        weekly(&mut inner, "low", 85.0);
        let c = ctx(&members, rules(Strategy::Quota), None, NOW);
        inner.providers.clear();
        assert_eq!(
            inner.eligible(&c, None, false),
            ["low", "high", "mid"],
            "no tier has room: all of them"
        );
    }

    #[test]
    fn a_higher_tier_that_regains_room_takes_new_conversations_back() {
        let mut members = [member("a"), member("b")];
        members[0].priority = 1;
        let mut inner = Inner::default();
        weekly(&mut inner, "a", 95.0);
        let c = ctx(&members, rules(Strategy::Quota), None, NOW);
        assert_eq!(inner.place(&c, true).as_deref(), Some("b"));
        weekly(&mut inner, "a", 5.0);
        assert_eq!(inner.place(&c, true).as_deref(), Some("a"));
    }

    #[test]
    fn a_pinned_account_is_used_while_it_can_serve() {
        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        weekly(&mut inner, "a", 10.0);
        weekly(&mut inner, "b", 50.0);
        let c = ctx(&members, rules(Strategy::Quota), Some("b"), NOW);
        assert_eq!(inner.place(&c, true).as_deref(), Some("b"));
        let mut paused = members.clone();
        paused[1].paused = true;
        let c = ctx(&paused, rules(Strategy::Quota), Some("b"), NOW);
        assert_eq!(inner.place(&c, true).as_deref(), Some("a"));
        let mut none = paused.clone();
        none[0].needs_reauth = true;
        let c = ctx(&none, rules(Strategy::Quota), Some("b"), NOW);
        assert_eq!(inner.place(&c, true), None, "nothing is usable");
    }

    #[test]
    fn choosing_an_account_by_hand_moves_threads_and_placement_to_it() {
        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let mut writes = Vec::new();
        let c = ctx(&members, rules(Strategy::Quota), None, NOW);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("a")
        );
        for _ in 0..3 {
            inner.record(&c, "b", None, &server_error(), &mut writes);
        }
        let pinned = ctx(&members, rules(Strategy::Quota), Some("b"), NOW);
        assert_eq!(
            inner.resolve(&pinned, Some(1), &mut writes).as_deref(),
            Some("b")
        );
        assert_eq!(inner.health(&pinned, "b").unwrap().soft_avoid_until, None);
        assert!(writes
            .iter()
            .any(|w| matches!(w, Write::Clear(Some(p), None) if p == "p")));
        let mut paused = members.clone();
        paused[1].paused = true;
        let c = ctx(&paused, rules(Strategy::Quota), Some("b"), NOW);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("a")
        );
        let c = ctx(&members, rules(Strategy::Quota), Some("b"), NOW);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("a"),
            "kept"
        );
        assert_eq!(
            inner.resolve(&c, Some(2), &mut writes).as_deref(),
            Some("b"),
            "new"
        );
    }

    #[test]
    fn cooldowns_follow_retry_after_then_the_reset_then_a_minute() {
        let (until, source) = quota_cooldown(Some("120"), &[NOW + HOUR], NOW);
        assert_eq!((until, source), (NOW + 120_000, CooldownSource::RetryAfter));
        let (until, _) = quota_cooldown(Some("999999"), &[], NOW);
        assert_eq!(until, NOW + 24 * HOUR, "capped at a day");
        let (until, source) = quota_cooldown(None, &[NOW + 5 * MINUTE, NOW + HOUR], NOW);
        assert_eq!((until, source), (NOW + 5 * MINUTE, CooldownSource::Reset));
        let (until, _) = quota_cooldown(None, &[NOW + 3 * HOUR], NOW);
        assert_eq!(until, NOW + 15 * MINUTE, "a reset is capped at 15 minutes");
        let (until, source) = quota_cooldown(None, &[NOW - 1], NOW);
        assert_eq!((until, source), (NOW + MINUTE, CooldownSource::Default));
        assert_eq!(
            quota_avoid_until(&[NOW + 3 * HOUR], NOW, NOW + MINUTE),
            NOW + 3 * HOUR
        );
        assert_eq!(
            quota_avoid_until(&[NOW + 30 * HOUR], NOW, NOW + MINUTE),
            NOW + 6 * HOUR
        );
        assert_eq!(quota_avoid_until(&[], NOW, NOW + MINUTE), NOW + MINUTE);
        let date = chrono::DateTime::from_timestamp_millis((NOW + 90_000) as i64)
            .unwrap()
            .to_rfc2822();
        assert_eq!(retry_after_ms(Some(&date), NOW), Some(90_000));
        assert_eq!(retry_after_ms(Some("0"), NOW), None);
        assert_eq!(retry_after_ms(Some("soon"), NOW), None);
    }

    #[test]
    fn a_quota_refusal_cools_the_account_moves_the_cursor_and_frees_its_threads() {
        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let c = ctx(&members, rules(Strategy::Quota), None, NOW);
        let mut writes = Vec::new();
        assert_eq!(
            inner.resolve(&c, Some(7), &mut writes).as_deref(),
            Some("a")
        );
        inner.record(
            &c,
            "a",
            Some(7),
            &quota_refusal(Some("3600"), Vec::new()),
            &mut writes,
        );
        assert!(inner.bindings.is_empty());
        assert!(writes
            .iter()
            .any(|w| matches!(w, Write::Cooldown(c) if c.until_ms == NOW + HOUR)));
        assert_eq!(
            inner.resolve(&c, Some(7), &mut writes).as_deref(),
            Some("b")
        );
        let later = ctx(&members, rules(Strategy::Quota), None, NOW + HOUR + 1);
        assert_eq!(
            inner.resolve(&later, Some(7), &mut writes).as_deref(),
            Some("b"),
            "the thread stays on b after a's cooldown"
        );
        assert_eq!(
            inner.place(&later, true).as_deref(),
            Some("b"),
            "the cursor too"
        );
    }

    #[test]
    fn every_account_cooling_stays_on_the_one_that_ends_first() {
        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let c = ctx(&members, rules(Strategy::Quota), None, NOW);
        let mut writes = Vec::new();
        inner.record(
            &c,
            "a",
            None,
            &quota_refusal(Some("600"), Vec::new()),
            &mut writes,
        );
        inner.record(
            &c,
            "b",
            None,
            &quota_refusal(Some("60"), Vec::new()),
            &mut writes,
        );
        assert_eq!(inner.place(&c, true).as_deref(), Some("b"));
    }

    #[test]
    fn three_failures_in_a_row_fail_over_with_growing_avoidance() {
        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let c = ctx(&members, rules(Strategy::Quota), None, NOW);
        let mut writes = Vec::new();
        assert_eq!(inner.place(&c, true).as_deref(), Some("a"));
        for _ in 0..2 {
            inner.record(&c, "a", None, &server_error(), &mut writes);
        }
        assert_eq!(
            inner.place(&c, true).as_deref(),
            Some("a"),
            "two are not enough"
        );
        inner.record(&c, "a", None, &server_error(), &mut writes);
        let health = inner.health(&c, "a").unwrap().clone();
        assert_eq!(health.soft_avoid_until, Some(NOW + 30 * SECOND));
        assert_eq!(inner.place(&c, true).as_deref(), Some("b"));
        inner.record(&c, "a", None, &server_error(), &mut writes);
        assert_eq!(
            inner.health(&c, "a").unwrap().soft_avoid_until,
            Some(NOW + 2 * MINUTE)
        );
        for _ in 0..4 {
            inner.record(&c, "a", None, &server_error(), &mut writes);
        }
        assert_eq!(
            inner.health(&c, "a").unwrap().soft_avoid_until,
            Some(NOW + 30 * MINUTE)
        );
        inner.record(&c, "a", None, &ok(), &mut writes);
        assert_eq!(
            inner.health(&c, "a").unwrap().consecutive_failures,
            8,
            "one is not enough"
        );
        inner.record(&c, "a", None, &ok(), &mut writes);
        assert_eq!(inner.health(&c, "a").unwrap().consecutive_failures, 0);
        let late = ctx(&members, rules(Strategy::Quota), None, NOW + 6 * MINUTE);
        inner.record(&late, "b", None, &server_error(), &mut writes);
        inner.health_mut("claude-oauth", "b").consecutive_failures = 3;
        let later = ctx(&members, rules(Strategy::Quota), None, NOW + 12 * MINUTE);
        assert!(
            !inner.should_failover(&later, "b"),
            "a streak goes stale after 5 minutes"
        );
    }

    #[test]
    fn the_failure_window_degrades_at_a_quarter_of_twenty_and_recovers() {
        let mut window = FailureWindow::default();
        for i in 0..15 {
            window.push(NOW + i, false);
        }
        for i in 0..4 {
            window.push(NOW + 20 + i, true);
        }
        assert!(!window.degraded, "19 samples are too few");
        window.push(NOW + 30, true);
        assert!(window.degraded, "5 of 20 failed");
        window.recompute(NOW + 61_000);
        assert!(window.degraded, "an empty window does not recover");
        for i in 0..20 {
            window.push(NOW + 62_000 + i, false);
        }
        assert!(window.degraded, "recovery is sustained 30 seconds");
        for i in 0..20 {
            window.push(NOW + 95_000 + i, false);
        }
        assert!(!window.degraded);
    }

    #[test]
    fn a_thread_stays_on_its_account_and_rebinds_only_to_a_cooler_one_with_room() {
        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let mut writes = Vec::new();
        weekly(&mut inner, "a", 10.0);
        weekly(&mut inner, "b", 20.0);
        let c = ctx(&members, rules(Strategy::Quota), None, NOW);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("a")
        );
        weekly(&mut inner, "a", 90.0);
        let c = ctx(&members, rules(Strategy::Quota), None, NOW + 2 * MINUTE);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("a"),
            "over the threshold, the cache keeps it"
        );
        assert_eq!(inner.place(&c, true).as_deref(), Some("b"), "new ones move");
        weekly(&mut inner, "a", 100.0);
        weekly(&mut inner, "b", 100.0);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("a"),
            "nowhere cooler"
        );
        weekly(&mut inner, "b", 30.0);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("b"),
            "spent"
        );
        let no_cache = Rules {
            cache_affinity: false,
            ..rules(Strategy::Quota)
        };
        weekly(&mut inner, "b", 85.0);
        weekly(&mut inner, "a", 20.0);
        let c = ctx(&members, no_cache, None, NOW + 4 * MINUTE);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("a")
        );
        let rr = ctx(
            &members,
            rules(Strategy::RoundRobin),
            None,
            NOW + 6 * MINUTE,
        );
        weekly(&mut inner, "a", 100.0);
        assert_eq!(
            inner.resolve(&rr, Some(1), &mut writes).as_deref(),
            Some("a"),
            "other strategies do not re-evaluate"
        );
    }

    #[test]
    fn a_binding_idle_for_a_day_is_released() {
        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let mut writes = Vec::new();
        let c = ctx(&members, rules(Strategy::RoundRobin), None, NOW);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("a")
        );
        let c = ctx(&members, rules(Strategy::RoundRobin), None, NOW + 23 * HOUR);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("a")
        );
        let c = ctx(&members, rules(Strategy::RoundRobin), None, NOW + 47 * HOUR);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("b")
        );
    }

    #[test]
    fn transient_trouble_detours_one_request_and_keeps_the_binding() {
        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let mut writes = Vec::new();
        let c = ctx(&members, rules(Strategy::Quota), None, NOW);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("a")
        );
        for _ in 0..3 {
            inner.record(&c, "a", Some(1), &server_error(), &mut writes);
        }
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("b"),
            "detour"
        );
        assert_eq!(
            inner.bindings[&(1, "p".to_string())].account,
            "a",
            "still bound"
        );
        let back = ctx(&members, rules(Strategy::Quota), None, NOW + MINUTE);
        inner.health_mut("claude-oauth", "a").consecutive_failures = 0;
        assert_eq!(
            inner.resolve(&back, Some(1), &mut writes).as_deref(),
            Some("a"),
            "home"
        );
        for _ in 0..3 {
            inner.record(&back, "a", Some(1), &server_error(), &mut writes);
        }
        assert_eq!(
            inner.resolve(&back, Some(1), &mut writes).as_deref(),
            Some("b")
        );
        let expired = ctx(&members, rules(Strategy::Quota), None, NOW + 12 * MINUTE);
        inner.health_mut("claude-oauth", "a").last_failure_ms = Some(NOW + 12 * MINUTE);
        assert_eq!(
            inner.resolve(&expired, Some(1), &mut writes).as_deref(),
            Some("b"),
            "a hold that outlived 10 minutes moves the thread to the detour"
        );
        assert_eq!(inner.bindings[&(1, "p".to_string())].account, "b");
    }

    #[test]
    fn credential_and_entitlement_refusals_differ() {
        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let mut writes = Vec::new();
        let c = ctx(&members, rules(Strategy::Quota), None, NOW);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("a")
        );
        let denied = Outcome::Status {
            status: 403,
            retry_after: None,
            resets: Vec::new(),
            denial: true,
            refusal: Refusal::Plain,
            message: "denied".into(),
        };
        inner.record(&c, "a", Some(1), &denied, &mut writes);
        assert!(inner.bindings.contains_key(&(1, "p".to_string())), "kept");
        let refused = Outcome::Status {
            status: 401,
            retry_after: None,
            resets: Vec::new(),
            denial: false,
            refusal: Refusal::Plain,
            message: "refused".into(),
        };
        inner.record(&c, "a", Some(1), &refused, &mut writes);
        assert!(inner.bindings.is_empty(), "released");
        assert!(names_denial(
            r#"{"error":{"code":"codex_workspace_access_denied"}}"#
        ));
        assert!(names_denial(r#"{"code":"entitlement_missing"}"#));
        assert!(!names_denial(r#"{"error":{"type":"permission_error"}}"#));
    }

    #[test]
    fn usage_reads_are_asked_for_only_when_the_pool_chooses_by_usage() {
        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let c = ctx(&members, rules(Strategy::Quota), None, NOW);
        assert_eq!(inner.due_reads(&c), ["a", "b"]);
        assert!(inner.due_reads(&c).is_empty(), "already reading");
        inner.polling.clear();
        assert!(inner.due_reads(&c).is_empty(), "read recently");
        let one = [member("a")];
        let c = ctx(&one, rules(Strategy::Quota), None, NOW + HOUR);
        assert!(inner.due_reads(&c).is_empty(), "one account");
        let rr = Rules {
            auto_switch_threshold: 0,
            ..rules(Strategy::RoundRobin)
        };
        let c = ctx(&members, rr, None, NOW + HOUR);
        assert!(inner.due_reads(&c).is_empty(), "no usage decides");
        let c = ctx(&members, rules(Strategy::Quota), None, NOW);
        let mut writes = Vec::new();
        inner.record(
            &c,
            "a",
            None,
            &quota_refusal(None, vec![NOW + HOUR]),
            &mut writes,
        );
        let probe = ctx(&members, rules(Strategy::Quota), None, NOW + 5 * MINUTE);
        assert_eq!(
            inner.due_reads(&probe),
            ["a"],
            "a cooldown from a reset is probed"
        );
    }

    #[test]
    fn codex_and_anthropic_headers_become_windows() {
        let headers = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            }
        };
        let short = headers(&[
            ("x-codex-primary-used-percent", "42"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "1800000600"),
            ("x-codex-secondary-used-percent", "17.5"),
            ("x-codex-secondary-reset-at", "1800300000"),
        ]);
        let quota = codex_headers(&short, NOW).unwrap();
        assert_eq!(quota.short, Some(42.0));
        assert_eq!(quota.short_reset_ms, Some(1_800_000_600_000));
        assert_eq!(quota.weekly, Some(17.5));
        assert_eq!(quota.score(NOW), 42.0);
        let weekly_only = headers(&[("x-codex-primary-used-percent", "150")]);
        let quota = codex_headers(&weekly_only, NOW).unwrap();
        assert_eq!((quota.weekly, quota.short), (Some(100.0), None));
        let monthly = headers(&[
            ("x-codex-primary-used-percent", "5"),
            ("x-codex-primary-window-minutes", "43200"),
            ("x-codex-secondary-used-percent", "9"),
        ]);
        let quota = codex_headers(&monthly, NOW).unwrap();
        assert_eq!((quota.monthly, quota.weekly), (Some(5.0), Some(9.0)));
        assert!(codex_headers(&headers(&[]), NOW).is_none());

        let anthropic = headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.25"),
            ("anthropic-ratelimit-unified-5h-reset", "1800000900"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.6"),
            ("anthropic-ratelimit-unified-7d-status", "rejected"),
            ("anthropic-ratelimit-unified-7d-reset", "1800100000"),
            ("x-codex-primary-reset-at", "1800000500"),
        ]);
        let quota = anthropic_headers(&anthropic, NOW).unwrap();
        assert_eq!((quota.short, quota.weekly), (Some(25.0), Some(60.0)));
        assert_eq!(quota.score(NOW), 60.0);
        assert_eq!(
            refusal_resets(&anthropic),
            [1_800_000_500_000, 1_800_100_000_000],
            "the Codex resets and the rejected window's"
        );
    }

    #[test]
    fn the_short_window_alone_is_no_measure_unless_it_is_spent() {
        let short = |percent: f64, reset: Option<u64>| Quota {
            short: Some(percent),
            short_reset_ms: reset,
            short_observed_ms: Some(NOW),
            updated_ms: NOW,
            ..Quota::default()
        };
        assert_eq!(short(0.0, None).score(NOW), UNKNOWN_SCORE);
        assert_eq!(short(100.0, Some(NOW + HOUR)).score(NOW), EXHAUSTED);
        assert_eq!(short(100.0, Some(NOW - 1)).score(NOW), UNKNOWN_SCORE);
        assert_eq!(
            short(100.0, None).score(NOW + 10 * MINUTE),
            UNKNOWN_SCORE,
            "stale"
        );
        let mut both = short(90.0, Some(NOW - 1));
        both.weekly = Some(30.0);
        assert_eq!(both.score(NOW), 30.0, "an elapsed short window is left out");
    }

    #[test]
    fn usage_windows_are_sorted_by_length_then_name() {
        let window = |name: &str, used: f64, seconds: Option<u64>| crate::usage::UsageWindow {
            name: name.into(),
            used_percent: Some(used),
            resets_at: Some("1800000900".into()),
            window_seconds: seconds,
        };
        let usage = crate::Usage {
            plan: None,
            windows: vec![
                window("primary", 12.0, Some(18_000)),
                window("secondary", 40.0, Some(604_800)),
                window("seven_day_opus", 99.0, None),
                window("five_hour:Opus", 99.0, None),
            ],
            theos: None,
            credits: None,
        };
        let quota = usage_quota(&usage, NOW).unwrap();
        assert_eq!(
            (quota.short, quota.weekly, quota.monthly),
            (Some(12.0), Some(40.0), None)
        );
        let usage = crate::Usage {
            plan: None,
            windows: vec![
                window("five_hour", 3.0, None),
                window("seven_day", 8.0, None),
            ],
            theos: None,
            credits: None,
        };
        let quota = usage_quota(&usage, NOW).unwrap();
        assert_eq!((quota.short, quota.weekly), (Some(3.0), Some(8.0)));
        assert_eq!(quota.weekly_reset_ms, Some(1_800_000_900_000));
    }

    fn ctx_of<'a>(
        members: &'a [Member],
        kind: &'a str,
        model: &'a str,
        rules: Rules,
        now: u64,
    ) -> Ctx<'a> {
        Ctx {
            provider: "p",
            kind,
            rules,
            pinned: None,
            members,
            model,
            now,
        }
    }

    fn refusal_of(status: u16, refusal: Refusal, retry_after: Option<&str>) -> Outcome {
        Outcome::Status {
            status,
            retry_after: retry_after.map(str::to_string),
            resets: Vec::new(),
            denial: false,
            refusal,
            message: format!("provider returned HTTP {status}"),
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn anthropic_429s_are_told_apart_by_their_windows() {
        let shared = headers(&[("anthropic-ratelimit-unified-7d-status", "rejected")]);
        assert_eq!(anthropic_429(&shared, NOW), Refusal::Shared);
        let family = headers(&[
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ("anthropic-ratelimit-unified-7d_oi-reset", "1800003600"),
        ]);
        assert_eq!(anthropic_429(&family, NOW), Refusal::Family);
        assert_eq!(family_resets(&family), vec![1_800_003_600_000]);
        let unified = headers(&[("anthropic-ratelimit-unified-status", "rejected")]);
        assert_eq!(anthropic_429(&unified, NOW), Refusal::Shared);
        let paused = headers(&[("retry-after", "0.5")]);
        assert_eq!(anthropic_429(&paused, NOW), Refusal::Rate);
        let allowed = headers(&[("anthropic-ratelimit-unified-status", "allowed")]);
        assert_eq!(anthropic_429(&allowed, NOW), Refusal::Rate);
        assert_eq!(anthropic_429(&headers(&[]), NOW), Refusal::Unknown);
        assert_eq!(
            refusal("claude-oauth", 429, &shared, "", NOW),
            Refusal::Shared
        );
        assert_eq!(
            refusal("gpt-oauth", 429, &shared, "", NOW),
            Refusal::Plain,
            "only Anthropic sign-ins"
        );
    }

    #[test]
    fn anthropic_429s_and_403s_are_retried_and_recorded_by_their_class() {
        let now = NOW;
        assert_eq!(
            refusal_of(429, Refusal::Rate, Some("0.5")).retry("claude-oauth", now),
            Retry::SameThenOther(500)
        );
        assert_eq!(
            refusal_of(429, Refusal::Rate, Some("30")).retry("claude-oauth", now),
            Retry::Other
        );
        assert_eq!(
            refusal_of(429, Refusal::Unknown, None).retry("claude-oauth", now),
            Retry::Same(SHORT_RETRY)
        );
        assert_eq!(
            refusal_of(403, Refusal::Account, None).retry("claude-oauth", now),
            Retry::Other
        );
        assert_eq!(
            refusal_of(403, Refusal::Plain, None).retry("claude-oauth", now),
            Retry::No
        );

        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let mut writes = Vec::new();
        let c = ctx(&members, rules(Strategy::Quota), None, now);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("a")
        );
        // A rate pause keeps the thread on its account and sends this
        // request elsewhere; it is no quota cooldown.
        inner.record(
            &c,
            "a",
            Some(1),
            &refusal_of(429, Refusal::Rate, Some("30")),
            &mut writes,
        );
        assert!(!inner.cooling(&c, "a"));
        assert_eq!(inner.block_reason(&c, "a"), Some("rate_paused"));
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("b")
        );
        assert_eq!(inner.bindings[&(1, "p".to_string())].account, "a", "kept");
        let later = ctx(&members, rules(Strategy::Quota), None, now + 31 * SECOND);
        assert_eq!(
            inner.resolve(&later, Some(1), &mut writes).as_deref(),
            Some("a")
        );
        // An unnamed 429 changes nothing of the account.
        inner.record(
            &later,
            "a",
            Some(1),
            &refusal_of(429, Refusal::Unknown, None),
            &mut writes,
        );
        assert_eq!(inner.block_reason(&later, "a"), None);
        // A plain Anthropic 403 is no verdict; one refusing the account
        // cools it for ten minutes.
        inner.record(
            &later,
            "a",
            Some(1),
            &refusal_of(403, Refusal::Plain, None),
            &mut writes,
        );
        assert_eq!(inner.block_reason(&later, "a"), None);
        inner.record(
            &later,
            "a",
            Some(1),
            &refusal_of(403, Refusal::Account, None),
            &mut writes,
        );
        let until = inner.health[&key("claude-oauth", "a")].cooldown_until;
        assert_eq!(until, Some(now + 31 * SECOND + ACCOUNT_REFUSAL_COOLDOWN));
        assert!(inner.bindings.is_empty(), "its threads leave");
        assert!(anthropic_account_refusal(
            r#"{"type":"error","error":{"type":"permission_error","message":"Your subscription has expired."}}"#
        ));
        assert!(anthropic_account_refusal(
            r#"{"error":{"type":"billing_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."}}"#
        ));
        assert!(anthropic_account_refusal(
            r#"{"error":{"type":"oauth_org_not_allowed","message":"x"}}"#
        ));
        assert!(!anthropic_account_refusal(
            r#"{"error":{"type":"permission_error","message":"This model is not available."}}"#
        ));
        assert!(!anthropic_account_refusal(
            r#"{"error":{"type":"permission_error","code":"other","message":"Your subscription has expired."}}"#
        ));
    }

    #[test]
    fn a_family_refusal_keeps_the_account_for_other_models() {
        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let mut writes = Vec::new();
        let fable = ctx_of(
            &members,
            "claude-oauth",
            "claude-fable-5",
            rules(Strategy::Quota),
            NOW,
        );
        let sonnet = ctx_of(
            &members,
            "claude-oauth",
            "claude-sonnet-5",
            rules(Strategy::Quota),
            NOW,
        );
        assert_eq!(
            inner.resolve(&fable, Some(1), &mut writes).as_deref(),
            Some("a")
        );
        let refused = Outcome::Status {
            status: 429,
            retry_after: None,
            resets: vec![NOW + HOUR],
            denial: false,
            refusal: Refusal::Family,
            message: "family".into(),
        };
        inner.record(&fable, "a", Some(1), &refused, &mut writes);
        assert!(inner.cooling(&fable, "a"));
        assert!(!inner.cooling(&sonnet, "a"), "other models still use it");
        let until = inner.scoped[&(key("claude-oauth", "a"), "Fable".to_string())].until;
        assert_eq!(until, NOW + FAMILY_REFUSAL_MAX_AGE, "at most 30 minutes");
        assert_eq!(
            inner.resolve(&fable, Some(1), &mut writes).as_deref(),
            Some("b")
        );
        assert_eq!(inner.bindings[&(1, "p".to_string())].account, "a", "kept");
        assert_eq!(
            inner.resolve(&sonnet, Some(2), &mut writes).as_deref(),
            Some("a")
        );
        // A Fable answer from the account clears its family refusal.
        inner.record(&fable, "a", Some(1), &Outcome::answered(), &mut writes);
        assert!(!inner.cooling(&fable, "a"));
        assert_eq!(anthropic_family("claude-fable-5-20261101"), Some("Fable"));
        assert_eq!(anthropic_family("claude-sonnet-5"), None);
    }

    #[test]
    fn a_reset_dated_codex_refusal_cools_only_the_models_quota_group() {
        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let mut writes = Vec::new();
        let reserve = ctx_of(
            &members,
            "gpt-oauth",
            "gpt-reserve",
            rules(Strategy::Quota),
            NOW,
        );
        let shared = ctx_of(
            &members,
            "gpt-oauth",
            "gpt-5.2-codex",
            rules(Strategy::Quota),
            NOW,
        );
        assert_eq!(
            inner.resolve(&shared, Some(1), &mut writes).as_deref(),
            Some("a")
        );
        let dated = Outcome::Status {
            status: 429,
            retry_after: None,
            resets: vec![NOW + 10 * MINUTE],
            denial: false,
            refusal: Refusal::Plain,
            message: "limit".into(),
        };
        inner.record(&reserve, "a", Some(2), &dated, &mut writes);
        assert!(inner.cooling(&reserve, "a"));
        assert!(
            !inner.cooling(&shared, "a"),
            "the shared group still serves"
        );
        assert_eq!(
            inner.resolve(&shared, Some(1), &mut writes).as_deref(),
            Some("a")
        );
        assert_eq!(
            inner.resolve(&reserve, Some(3), &mut writes).as_deref(),
            Some("b")
        );
        assert!(writes.iter().any(|write| matches!(
            write,
            Write::Cooldown(PoolCooldown { scope: Some(scope), .. }) if scope == "reserve"
        )));
        // The shared group's dated refusal leaves the shared group, and its
        // threads leave the account; gpt-reserve still reaches it later.
        inner.record(&shared, "a", Some(1), &dated, &mut writes);
        assert!(inner.cooling(&shared, "a"));
        assert!(!inner.bindings.contains_key(&(1, "p".to_string())));
        // A refusal with Retry-After cools the whole account.
        inner.record(
            &shared,
            "b",
            None,
            &quota_refusal(Some("60"), Vec::new()),
            &mut writes,
        );
        assert!(inner.cooling(&reserve, "b") && inner.cooling(&shared, "b"));
        assert_eq!(quota_scope("gpt-oauth", "GPT-Reserve"), Some("reserve"));
        assert_eq!(quota_scope("claude-oauth", "claude-sonnet-5"), None);
    }

    #[test]
    fn an_excluded_plan_is_chosen_only_by_hand() {
        let mut members = [member("a"), member("b")];
        members[0].plan = Some("Free".into());
        let mut inner = Inner::default();
        let mut writes = Vec::new();
        let excluding = Rules {
            excluded_plans: vec!["free".into()],
            ..rules(Strategy::Quota)
        };
        let c = ctx_of(&members, "gpt-oauth", "m", excluding.clone(), NOW);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("b")
        );
        assert_eq!(inner.block_reason(&c, "a"), Some("plan_excluded"));
        let pinned = Ctx {
            pinned: Some("a"),
            ..ctx_of(&members, "gpt-oauth", "m", excluding.clone(), NOW)
        };
        assert_eq!(
            inner.peek_place(&pinned).as_deref(),
            Some("a"),
            "chosen by hand"
        );
        // A plan the usage read names wins over the token's.
        inner.quota.insert(
            key("gpt-oauth", "b"),
            Quota {
                plan: Some("free".into()),
                updated_ms: NOW,
                ..Quota::default()
            },
        );
        assert_eq!(inner.peek_place(&c), None, "both excluded");
    }

    #[test]
    fn a_full_chatgpt_account_waits_unless_it_may_spend_credits() {
        let mut members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let c = ctx_of(&members, "gpt-oauth", "m", rules(Strategy::Quota), NOW);
        let full = |credits: Option<crate::usage::Credits>| Quota {
            weekly: Some(100.0),
            weekly_reset_ms: Some(NOW + HOUR),
            credits: credits.map(|credits| (credits, NOW)),
            updated_ms: NOW,
            ..Quota::default()
        };
        let funded = crate::usage::Credits {
            has_credits: Some(true),
            balance: Some(5.0),
            ..crate::usage::Credits::default()
        };
        inner
            .quota
            .insert(key("gpt-oauth", "a"), full(Some(funded.clone())));
        assert_eq!(inner.block_reason(&c, "a"), Some("usage_limit"));
        assert_eq!(inner.peek_place(&c).as_deref(), Some("b"));
        members[0].credits = true;
        let c = ctx_of(&members, "gpt-oauth", "m", rules(Strategy::Quota), NOW);
        assert_eq!(inner.block_reason(&c, "a"), None, "it may spend them");
        let stale = ctx_of(
            &members,
            "gpt-oauth",
            "m",
            rules(Strategy::Quota),
            NOW + 6 * MINUTE,
        );
        assert_eq!(
            inner.block_reason(&stale, "a"),
            Some("usage_limit"),
            "old credits"
        );
        inner.quota.insert(key("gpt-oauth", "a"), full(None));
        assert_eq!(
            inner.block_reason(&c, "a"),
            Some("usage_limit"),
            "none known"
        );
        let alone = [members[0].clone()];
        let c = ctx_of(&alone, "gpt-oauth", "m", rules(Strategy::Quota), NOW);
        assert_eq!(inner.peek_place(&c), None, "never sent to spend credits");
        let claude = ctx_of(&alone, "claude-oauth", "m", rules(Strategy::Quota), NOW);
        assert!(!inner.credits_held(&claude, "a"), "ChatGPT only");
    }

    #[test]
    fn an_account_that_refused_a_model_goes_last_for_it() {
        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let mut writes = Vec::new();
        let astra = ctx_of(
            &members,
            "gpt-oauth",
            "gpt-6-astra",
            rules(Strategy::Quota),
            NOW,
        );
        let other = ctx_of(
            &members,
            "gpt-oauth",
            "gpt-5.2-codex",
            rules(Strategy::Quota),
            NOW,
        );
        assert_eq!(
            inner.resolve(&astra, Some(1), &mut writes).as_deref(),
            Some("a")
        );
        assert_eq!(
            refusal_of(400, Refusal::ModelDenied, None).retry("gpt-oauth", NOW),
            Retry::Other
        );
        inner.record(
            &astra,
            "a",
            Some(1),
            &refusal_of(400, Refusal::ModelDenied, None),
            &mut writes,
        );
        assert_eq!(
            inner.resolve(&astra, Some(1), &mut writes).as_deref(),
            Some("b")
        );
        assert_eq!(inner.bindings[&(1, "p".to_string())].account, "a", "kept");
        assert_eq!(
            inner.resolve(&other, Some(2), &mut writes).as_deref(),
            Some("a")
        );
        let alone = [member("a")];
        let c = ctx_of(
            &alone,
            "gpt-oauth",
            "gpt-6-astra",
            rules(Strategy::Quota),
            NOW,
        );
        assert_eq!(inner.peek_place(&c).as_deref(), Some("a"), "nothing better");
        inner.record(&astra, "a", None, &Outcome::answered(), &mut writes);
        assert!(!inner.model_unfit(&astra, "a"), "an answer clears it");
        let later = ctx_of(
            &members,
            "gpt-oauth",
            "gpt-6-astra",
            rules(Strategy::Quota),
            NOW + 7 * HOUR,
        );
        inner.record(
            &astra,
            "a",
            None,
            &refusal_of(400, Refusal::ModelDenied, None),
            &mut writes,
        );
        assert!(!inner.model_unfit(&later, "a"), "six hours");
        assert_eq!(
            codex_model_refusal(
                r#"{"detail":"The 'gpt-6-astra' model is not supported when using Codex with a ChatGPT account."}"#
            )
            .as_deref(),
            Some("gpt-6-astra")
        );
        assert_eq!(
            codex_model_refusal(r#"{"detail":"The 'x' model is not supported."}"#),
            None
        );
    }

    #[test]
    fn a_new_thread_starts_an_idle_five_hour_window_first() {
        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let on = Rules {
            start_idle_windows: true,
            ..rules(Strategy::Quota)
        };
        let idle = |seen: u64| Quota {
            short: Some(0.0),
            short_reset_ms: Some(seen + 5 * HOUR),
            short_observed_ms: Some(seen),
            short_seconds: Some(IDLE_WINDOW_SECONDS),
            weekly: Some(50.0),
            updated_ms: seen,
            ..Quota::default()
        };
        weekly(&mut inner, "a", 10.0);
        inner.quota.insert(key("claude-oauth", "b"), idle(NOW));
        let c = ctx(&members, on.clone(), None, NOW + MINUTE);
        assert_eq!(inner.place(&c, true).as_deref(), Some("b"), "the idle one");
        assert_eq!(
            inner.place(&c, true).as_deref(),
            Some("a"),
            "its window is taken as started"
        );
        let off = ctx(&members, rules(Strategy::Quota), None, NOW + MINUTE);
        let mut fresh = Inner::default();
        weekly(&mut fresh, "a", 10.0);
        fresh.quota.insert(key("claude-oauth", "b"), idle(NOW));
        assert_eq!(
            fresh.place(&off, true).as_deref(),
            Some("a"),
            "off by default"
        );
        let old = ctx(&members, on.clone(), None, NOW + 6 * MINUTE);
        assert_eq!(
            fresh.place(&old, true).as_deref(),
            Some("a"),
            "a stale reading"
        );
        let pinned = ctx(&members, on, Some("a"), NOW + MINUTE);
        assert_eq!(
            fresh.peek_place(&pinned).as_deref(),
            Some("a"),
            "chosen by hand"
        );
    }

    #[test]
    fn a_fork_starts_on_its_parents_account_then_a_siblings() {
        let members = [member("a"), member("b")];
        let mut inner = Inner::default();
        let mut writes = Vec::new();
        let c = ctx(&members, rules(Strategy::RoundRobin), None, NOW);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("a")
        );
        inner.parents.insert(2, 1);
        inner.parents.insert(3, 1);
        assert_eq!(
            inner.resolve(&c, Some(2), &mut writes).as_deref(),
            Some("a"),
            "the parent's, not the next in turn"
        );
        assert_eq!(
            inner.resolve(&c, Some(4), &mut writes).as_deref(),
            Some("b")
        );
        inner.unbind(&(1, "p".to_string()), &mut writes);
        assert_eq!(
            inner.resolve(&c, Some(3), &mut writes).as_deref(),
            Some("a"),
            "a sibling's"
        );
        let mut writes = Vec::new();
        inner.record(
            &c,
            "a",
            None,
            &quota_refusal(Some("60"), Vec::new()),
            &mut writes,
        );
        inner.parents.insert(5, 1);
        assert_eq!(
            inner.resolve(&c, Some(5), &mut writes).as_deref(),
            Some("b"),
            "not where the family cannot be served"
        );
    }

    #[test]
    fn a_bound_thread_moves_back_to_a_higher_tier_that_has_room() {
        let mut members = [member("high"), member("low")];
        members[0].priority = 10;
        let mut inner = Inner::default();
        let mut writes = Vec::new();
        let on = Rules {
            priority_failback: true,
            ..rules(Strategy::Quota)
        };
        let fresh = |percent: f64, at: u64| Quota {
            weekly: Some(percent),
            updated_ms: at,
            ..Quota::default()
        };
        inner
            .quota
            .insert(key("claude-oauth", "high"), fresh(95.0, NOW));
        inner
            .quota
            .insert(key("claude-oauth", "low"), fresh(10.0, NOW));
        let c = ctx(&members, on.clone(), None, NOW);
        assert_eq!(
            inner.resolve(&c, Some(1), &mut writes).as_deref(),
            Some("low")
        );
        inner
            .quota
            .insert(key("claude-oauth", "high"), fresh(20.0, NOW + MINUTE));
        let back = ctx(&members, on.clone(), None, NOW + MINUTE);
        let off = ctx(&members, rules(Strategy::Quota), None, NOW + MINUTE);
        assert_eq!(
            inner
                .clone()
                .resolve(&off, Some(1), &mut Vec::new())
                .as_deref(),
            Some("low"),
            "off by default: the thread keeps its cache"
        );
        let mut stale = inner.clone();
        let late = ctx(&members, on.clone(), None, NOW + MINUTE + FAILBACK_REFRESH);
        assert_eq!(
            stale.resolve(&late, Some(1), &mut Vec::new()).as_deref(),
            Some("low"),
            "only on fresh usage"
        );
        assert_eq!(
            inner.resolve(&back, Some(1), &mut writes).as_deref(),
            Some("high")
        );
        assert_eq!(inner.bindings[&(1, "p".to_string())].account, "high");
        let mut pinned = inner.clone();
        pinned.bindings.insert(
            (2, "p".to_string()),
            Binding {
                kind: "claude-oauth".into(),
                account: "low".into(),
                bound_ms: NOW,
                used_ms: NOW,
                written_ms: NOW,
                reeval_ms: NOW,
                hold_since: None,
                detour: None,
            },
        );
        let hand = ctx(&members, on, Some("low"), NOW + MINUTE);
        pinned.sync_pin(&hand, &mut Vec::new());
        assert!(
            pinned.priority_failback(&hand, "low").is_none(),
            "a hand choice holds"
        );
    }

    #[test]
    fn unreachable_hosts_are_counted_apart_and_may_open_a_circuit() {
        let mut inner = Inner::default();
        let host = "p|api.example.test";
        inner.host_failed(host, "dns", 0, NOW);
        inner.host_failed(host, "dns", 0, NOW + MINUTE);
        assert_eq!(inner.hosts[host].failures, 2);
        assert_eq!(
            inner.admit_host(host, 0, NOW + MINUTE),
            Ok(()),
            "only recorded"
        );
        inner.host_failed(host, "dns", 0, NOW + 12 * MINUTE);
        assert_eq!(
            inner.hosts[host].failures, 1,
            "ten minutes apart start again"
        );
        inner.host_reached(host);
        assert!(inner.hosts.is_empty());

        for step in 0..3 {
            inner.host_failed(host, "connection_refused", 3, NOW + step);
        }
        assert_eq!(inner.admit_host(host, 3, NOW + 3), Err(30));
        let after = NOW + 2 + HOST_CIRCUIT_COOLDOWN;
        assert_eq!(inner.admit_host(host, 3, after), Ok(()), "one trial");
        assert_eq!(
            inner.admit_host(host, 3, after + 1),
            Err(1),
            "the others wait"
        );
        inner.host_failed(host, "connection_refused", 3, after + 2);
        assert!(
            inner.admit_host(host, 3, after + 3).is_err(),
            "a failed trial reopens"
        );
        inner.host_reached(host);
        assert_eq!(inner.admit_host(host, 3, after + 4), Ok(()));

        let members = [member("a"), member("b")];
        let mut writes = Vec::new();
        let c = ctx(&members, rules(Strategy::Quota), None, NOW);
        let unreachable = Outcome::Unreachable {
            message: "dns".into(),
            code: "dns".into(),
        };
        for _ in 0..5 {
            inner.record(&c, "a", None, &unreachable, &mut writes);
        }
        assert_eq!(inner.block_reason(&c, "a"), None, "no account's fault");
    }

    #[test]
    fn a_static_key_cools_by_its_own_caps() {
        let members = [member("k1"), member("k2")];
        let mut inner = Inner::default();
        let mut writes = Vec::new();
        let c = ctx_of(
            &members,
            "api-key/logfare",
            "m",
            rules(Strategy::Quota),
            NOW,
        );
        inner.record(
            &c,
            "k1",
            None,
            &quota_refusal(Some("3600"), Vec::new()),
            &mut writes,
        );
        assert_eq!(
            inner.health[&key("api-key/logfare", "k1")].cooldown_until,
            Some(NOW + MAX_KEY_COOLDOWN),
            "Retry-After up to ten minutes"
        );
        assert_eq!(inner.peek_place(&c).as_deref(), Some("k2"));
        let refused = refusal_of(401, Refusal::Plain, None);
        assert_eq!(refused.retry("api-key/logfare", NOW), Retry::Other);
        assert_eq!(refused.retry("claude-oauth", NOW), Retry::No);
        inner.record(&c, "k2", None, &refused, &mut writes);
        assert_eq!(
            inner.health[&key("api-key/logfare", "k2")].cooldown_until,
            Some(NOW + MAX_KEY_COOLDOWN),
            "a refused key waits the cap"
        );
        inner.record(
            &c,
            "k1",
            None,
            &quota_refusal(None, vec![NOW + HOUR]),
            &mut writes,
        );
        assert_eq!(
            inner.health[&key("api-key/logfare", "k1")].cooldown_until,
            Some(NOW + DEFAULT_QUOTA_COOLDOWN),
            "an undated 429 waits a minute"
        );
        assert!(inner.due_reads(&c).is_empty(), "keys have no usage to read");
    }

    impl Inner {
        fn peek_place(&self, ctx: &Ctx) -> Option<String> {
            self.clone().place(ctx, false)
        }
    }
}
