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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rules {
    pub strategy: Strategy,
    pub sticky_limit: u32,
    pub auto_switch_threshold: u32,
    pub failover_threshold: u32,
    pub cache_affinity: bool,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            strategy: Strategy::Quota,
            sticky_limit: DEFAULT_STICKY_LIMIT,
            auto_switch_threshold: DEFAULT_AUTO_SWITCH_THRESHOLD,
            failover_threshold: DEFAULT_FAILOVER_THRESHOLD,
            cache_affinity: true,
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
        }
    }

    pub fn view(&self) -> Value {
        json!({
            "strategy": self.strategy.name(),
            "sticky_limit": self.sticky_limit,
            "auto_switch_threshold": self.auto_switch_threshold,
            "failover_threshold": self.failover_threshold,
            "cache_affinity": self.cache_affinity,
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
    pub updated_ms: u64,
}

impl Quota {
    fn is_empty(&self) -> bool {
        self.short.is_none() && self.weekly.is_none() && self.monthly.is_none()
    }

    /// Takes the windows `newer` reports; the others keep what was known.
    fn merge(&mut self, newer: &Quota) {
        if newer.short.is_some() {
            self.short = newer.short;
            self.short_reset_ms = newer.short_reset_ms;
            self.short_observed_ms = newer.short_observed_ms;
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

    fn view(&self) -> Value {
        json!({
            "short_percent": self.short,
            "short_resets_at": self.short_reset_ms,
            "weekly_percent": self.weekly,
            "weekly_resets_at": self.weekly_reset_ms,
            "monthly_percent": self.monthly,
            "monthly_resets_at": self.monthly_reset_ms,
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
    (!quota.is_empty()).then_some(quota)
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
    let quota = Quota {
        short,
        short_reset_ms: short.and(reset("5h")),
        short_observed_ms: short.map(|_| now),
        weekly,
        weekly_reset_ms: weekly.and(reset("7d")),
        updated_ms: now,
        ..Quota::default()
    };
    (!quota.is_empty()).then_some(quota)
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
    (!quota.is_empty()).then_some(quota)
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

    fn clear_cooldown(&mut self) {
        self.cooldown_until = None;
        self.cooldown_since = None;
        self.cooldown_source = None;
        self.quota_avoid_until = None;
        self.quota_refused = false;
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
        message: String,
    },
    /// No answer: the connection failed, timed out, or the stream broke.
    Broken(String),
}

impl Outcome {
    /// A successful answer.
    pub(crate) fn answered() -> Self {
        Self::Status {
            status: 200,
            retry_after: None,
            resets: Vec::new(),
            denial: false,
            message: String::new(),
        }
    }

    /// A refusal on quota (HTTP 429 or 402).
    pub(crate) fn is_quota(&self) -> bool {
        classify(self) == Class::Quota
    }
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
}

fn classify(outcome: &Outcome) -> Class {
    match outcome {
        Outcome::Broken(_) => Class::Transient,
        Outcome::Status { status, denial, .. } => match status {
            200..=299 => Class::Success,
            300..=399 => Class::Neutral,
            403 if *denial => Class::Workspace,
            401 | 403 => Class::Credential,
            402 | 429 => Class::Quota,
            400..=499 => Class::Caller,
            500..=599 => Class::Transient,
            _ => Class::Neutral,
        },
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
        }
    }

    fn usable(&self) -> bool {
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
    pub now: u64,
}

impl Ctx<'_> {
    fn member(&self, id: &str) -> Option<&Member> {
        self.members.iter().find(|member| member.id == id)
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
    Clear(Option<String>, Option<SessionId>),
}

type Key = (String, String);

fn key(kind: &str, account: &str) -> Key {
    (kind.to_string(), account.to_string())
}

#[derive(Debug, Clone, Default)]
struct Inner {
    health: HashMap<Key, Health>,
    quota: HashMap<Key, Quota>,
    providers: HashMap<String, ProviderState>,
    bindings: HashMap<(i64, String), Binding>,
    polling: HashSet<Key>,
    polled: HashMap<Key, u64>,
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
            .map_or(UNKNOWN_SCORE, |quota| quota.score(ctx.now))
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

    fn cooling(&self, ctx: &Ctx, id: &str) -> bool {
        self.health(ctx, id).is_some_and(|h| h.cooling(ctx.now))
    }

    fn quota_avoided(&self, ctx: &Ctx, id: &str) -> bool {
        self.health(ctx, id)
            .is_some_and(|h| h.quota_avoided(ctx.now))
    }

    fn soft_avoided(&self, ctx: &Ctx, id: &str) -> bool {
        self.health(ctx, id)
            .is_some_and(|h| h.soft_avoided(ctx.now))
    }

    fn degraded(&self, ctx: &Ctx, id: &str) -> bool {
        self.health(ctx, id).is_some_and(|h| h.window.degraded)
    }

    fn quota_refused(&self, ctx: &Ctx, id: &str) -> bool {
        self.health(ctx, id).is_some_and(|h| h.quota_refused)
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
    }

    /// Why the account is not selectable, if it is not.
    fn block_reason(&self, ctx: &Ctx, id: &str) -> Option<&'static str> {
        let member = ctx.member(id)?;
        if member.paused {
            Some("paused")
        } else if member.needs_reauth {
            Some("needs_reauth")
        } else if self.cooling(ctx, id) {
            Some("cooldown")
        } else if self.quota_avoided(ctx, id) {
            Some("quota_avoided")
        } else if self.soft_avoided(ctx, id) {
            Some("transient")
        } else if self.degraded(ctx, id) {
            Some("degraded")
        } else {
            None
        }
    }

    /// The accounts new placements and failovers may use, narrowed to the
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
                    && !(self.degraded(ctx, id) && ctx.pinned != Some(id))
                    && !self.quota_avoided(ctx, id)
            })
            .map(|member| member.id.clone())
            .collect();
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

    fn set_cursor(&mut self, ctx: &Ctx, id: &str) {
        self.state(ctx).cursor = Some(id.to_string());
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

    /// Blocked only by transient trouble: worth holding the thread's
    /// binding while one request goes elsewhere.
    fn transient_only(&self, ctx: &Ctx, id: &str) -> bool {
        ctx.member(id).is_some_and(Member::usable)
            && !self.cooling(ctx, id)
            && !self.quota_avoided(ctx, id)
            && !self.quota_refused(ctx, id)
            && (self.soft_avoided(ctx, id)
                || self.should_failover(ctx, id)
                || self.degraded(ctx, id))
    }

    /// A strictly less used account with headroom for a bound thread under
    /// the quota strategy, once its own account may be left: spent (or
    /// over its threshold when `cache_affinity` is off). Checked at most
    /// once a minute while it may not be left.
    fn reevaluate(&mut self, ctx: &Ctx, slot: &(i64, String)) -> Option<String> {
        if ctx.rules.strategy != Strategy::Quota {
            return None;
        }
        let (account, reeval) = {
            let binding = self.bindings.get(slot)?;
            (binding.account.clone(), binding.reeval_ms)
        };
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
    /// `None` when no account is usable at all.
    fn resolve(
        &mut self,
        ctx: &Ctx,
        thread: Option<i64>,
        writes: &mut Vec<Write>,
    ) -> Option<String> {
        self.sync_pin(ctx, writes);
        if let Some(thread) = thread {
            if let Some(account) = self.resolve_bound(ctx, thread, writes) {
                return Some(account);
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
            return Some(account);
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
        if matches!(ctx.rules.strategy, Strategy::Quota | Strategy::FillFirst) {
            if let Some(pinned) = self.usable_pin(ctx) {
                self.set_cursor(ctx, &pinned);
            }
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
    fn stay(&self, ctx: &Ctx, active: Option<&str>) -> Option<String> {
        if let Some(active) = active.filter(|id| ctx.member(id).is_some_and(Member::usable)) {
            return Some(active.to_string());
        }
        ctx.members
            .iter()
            .filter(|member| member.usable())
            .min_by_key(|member| {
                self.health(ctx, &member.id)
                    .and_then(|h| h.cooldown_until.max(h.soft_avoid_until))
                    .unwrap_or(0)
            })
            .map(|member| member.id.clone())
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
        let class = classify(outcome);
        let failover_on = ctx.rules.failover_threshold > 0;
        match class {
            Class::Neutral | Class::Caller => {}
            Class::Success => {
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
            Class::Quota => {
                let Outcome::Status {
                    status,
                    retry_after,
                    resets,
                    ..
                } = outcome
                else {
                    return;
                };
                let (until, source) = quota_cooldown(retry_after.as_deref(), resets, now);
                let avoid = quota_avoid_until(resets, now, until);
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
                    until_ms: until,
                    avoid_until_ms: avoid,
                    since_ms: now,
                    source: source.name().to_string(),
                    status: *status,
                }));
                self.unbind_account(ctx.kind, account, writes);
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
        if usable.len() < 2 || !by_usage {
            return Vec::new();
        }
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
                .is_none_or(|quota| ctx.now.saturating_sub(quota.updated_ms) >= USAGE_REFRESH)
                && self
                    .polled
                    .get(&slot)
                    .is_none_or(|at| ctx.now.saturating_sub(*at) >= USAGE_REFRESH);
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
        Outcome::Broken(message) => {
            health.last_status = None;
            health.last_error = Some(message.clone());
        }
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
                let mut inner = self.lock();
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
                Write::Uncool(kind, account) => store.delete_pool_cooldown(kind, account).await,
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

    /// Ends a cooldown and quota avoidance by hand; whether one was running.
    pub(crate) async fn clear_cooldown(&self, kind: &str, account: &str) -> bool {
        self.ready().await;
        let now = now_ms();
        let cleared = {
            let mut inner = self.lock();
            match inner.health.get_mut(&key(kind, account)) {
                Some(health) if health.cooling(now) || health.quota_avoided(now) => {
                    health.clear_cooldown();
                    health.last_probe_ms = Some(now);
                    true
                }
                _ => false,
            }
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

    /// `pool/status` of one provider.
    pub(crate) async fn status(&self, ctx: &Ctx<'_>, accounts: &[Account]) -> Value {
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
                json!({
                    "id": id,
                    "alias": account.info.alias,
                    "paused": account.info.paused,
                    "needs_reauth": account.info.needs_reauth,
                    "priority": account.info.priority.unwrap_or(0),
                    "auto_switch_threshold": account.info.auto_switch_threshold,
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
        json!({
            "provider": ctx.provider,
            "kind": ctx.kind,
            "pool": ctx.rules.view(),
            "active_account": ctx.pinned,
            "current": current,
            "accounts": rows,
        })
    }
}

/// What a client needs to choose its provider's account per request.
#[derive(Clone)]
pub(crate) struct Route {
    pub pool: Arc<Pool>,
    pub provider: String,
    pub kind: &'static str,
    pub rules: Rules,
    pub pinned: Option<String>,
}

impl Route {
    pub(crate) fn ctx<'a>(&'a self, members: &'a [Member], now: u64) -> Ctx<'a> {
        Ctx {
            provider: &self.provider,
            kind: self.kind,
            rules: self.rules,
            pinned: self.pinned.as_deref(),
            members,
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
            self.pool.observe(self.kind, account, quota);
        }
    }

    /// Records what `account` did, with the accounts as the credential
    /// store at `store` holds them now.
    pub(crate) async fn record(
        &self,
        store: &std::path::Path,
        account: &str,
        thread: Option<SessionId>,
        outcome: &Outcome,
    ) {
        let Ok(stored) = crate::oauth::Store::load(store.to_path_buf()) else {
            return;
        };
        let members: Vec<Member> = stored.accounts(self.kind).iter().map(Member::of).collect();
        let ctx = self.ctx(&members, now_ms());
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
        }
    }

    fn ctx<'a>(members: &'a [Member], rules: Rules, pinned: Option<&'a str>, now: u64) -> Ctx<'a> {
        Ctx {
            provider: "p",
            kind: "claude-oauth",
            rules,
            pinned,
            members,
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
            message: "provider returned HTTP 429".into(),
        }
    }

    fn ok() -> Outcome {
        Outcome::Status {
            status: 200,
            retry_after: None,
            resets: Vec::new(),
            denial: false,
            message: String::new(),
        }
    }

    fn server_error() -> Outcome {
        Outcome::Status {
            status: 503,
            retry_after: None,
            resets: Vec::new(),
            denial: false,
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
            message: "denied".into(),
        };
        inner.record(&c, "a", Some(1), &denied, &mut writes);
        assert!(inner.bindings.contains_key(&(1, "p".to_string())), "kept");
        let refused = Outcome::Status {
            status: 401,
            retry_after: None,
            resets: Vec::new(),
            denial: false,
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
        };
        let quota = usage_quota(&usage, NOW).unwrap();
        assert_eq!((quota.short, quota.weekly), (Some(3.0), Some(8.0)));
        assert_eq!(quota.weekly_reset_ms, Some(1_800_000_900_000));
    }

    impl Inner {
        fn peek_place(&self, ctx: &Ctx) -> Option<String> {
            self.clone().place(ctx, false)
        }
    }
}
