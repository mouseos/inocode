//! Plain data types of the account pool. No credentials and no e-mail addresses live here.

use serde::Deserialize;
use serde::Serialize;

/// Which kind of credential an entry holds. A new provider adds a variant (and an
/// [`super::AccountProvider`]); nothing else in the pool is provider specific.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AccountKind {
    Chatgpt,
    ApiKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AccountState {
    Active,
    Cooldown,
    Disabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WindowKind {
    FiveHour,
    Weekly,
    Both,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SelectionStrategy {
    Quota,
    RoundRobin,
    FillFirst,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApiKeyOrder {
    AfterChatgpt,
    BeforeChatgpt,
    Mixed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SwitchReason {
    Threshold,
    LimitReached,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    pub used_percent: f64,
    pub window_minutes: Option<i64>,
    /// Unix seconds.
    pub resets_at: Option<i64>,
}

impl UsageWindow {
    /// A window that has already reset counts as unused.
    fn effective_percent(&self, now: i64) -> f64 {
        match self.resets_at {
            Some(reset) if reset <= now => 0.0,
            _ => self.used_percent,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountUsage {
    pub five_hour: Option<UsageWindow>,
    pub weekly: Option<UsageWindow>,
    /// Unix seconds of the last refresh.
    pub updated_at: Option<i64>,
}

impl AccountUsage {
    /// Builds a usage view from the backend's two windows. A window of at most six hours is the
    /// short window; anything longer is the weekly one. When the lengths are unknown the first
    /// window is the short one.
    pub fn from_windows(
        primary: Option<UsageWindow>,
        secondary: Option<UsageWindow>,
        now: i64,
    ) -> Self {
        let mut usage = Self {
            updated_at: Some(now),
            ..Self::default()
        };
        for (index, window) in [primary, secondary].into_iter().enumerate() {
            let Some(window) = window else { continue };
            let short = match window.window_minutes {
                Some(minutes) => minutes <= 360,
                None => index == 0,
            };
            if short && usage.five_hour.is_none() {
                usage.five_hour = Some(window);
            } else if usage.weekly.is_none() {
                usage.weekly = Some(window);
            } else {
                usage.five_hour = Some(window);
            }
        }
        usage
    }

    /// From the backend's rate-limit snapshot (response headers or the usage endpoint).
    pub fn from_snapshot(snapshot: &codex_protocol::protocol::RateLimitSnapshot, now: i64) -> Self {
        let window = |w: &codex_protocol::protocol::RateLimitWindow| UsageWindow {
            used_percent: w.used_percent,
            window_minutes: w.window_minutes,
            resets_at: w.resets_at,
        };
        Self::from_windows(
            snapshot.primary.as_ref().map(window),
            snapshot.secondary.as_ref().map(window),
            now,
        )
    }

    /// The percentage the policy cares about, or `None` when nothing is known.
    pub fn effective_percent(&self, window: WindowKind, now: i64) -> Option<f64> {
        let five = self.five_hour.as_ref().map(|w| w.effective_percent(now));
        let week = self.weekly.as_ref().map(|w| w.effective_percent(now));
        match window {
            WindowKind::FiveHour => five,
            WindowKind::Weekly => week,
            WindowKind::Both => match (five, week) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AccountPolicy {
    /// Switch before a window would exceed this percentage. 0-100.
    pub threshold_percent: f64,
    pub window: WindowKind,
    pub selection: SelectionStrategy,
    pub api_key_order: ApiKeyOrder,
    /// Cooldown when a limit error carries no reset time.
    pub default_cooldown_seconds: i64,
    /// Switch on threshold and on limit errors. Off means "use the selected one until told otherwise".
    pub auto_switch: bool,
}

impl Default for AccountPolicy {
    fn default() -> Self {
        Self {
            threshold_percent: 90.0,
            window: WindowKind::Both,
            selection: SelectionStrategy::Quota,
            api_key_order: ApiKeyOrder::AfterChatgpt,
            default_cooldown_seconds: 60,
            auto_switch: true,
        }
    }
}

/// Non-secret facts about one entry; stored next to its credentials.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountMeta {
    pub id: String,
    pub kind: AccountKind,
    pub alias: String,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub cooldown_until: Option<i64>,
    #[serde(default)]
    pub last_used_at: Option<i64>,
    #[serde(default)]
    pub usage: Option<AccountUsage>,
    #[serde(default)]
    pub added_at: i64,
}

fn default_true() -> bool {
    true
}

/// What a client sees of one entry.
#[derive(Clone, Debug, PartialEq)]
pub struct AccountView {
    pub id: String,
    pub kind: AccountKind,
    pub alias: String,
    pub plan: Option<String>,
    pub usage: Option<AccountUsage>,
    pub state: AccountState,
    pub cooldown_until: Option<i64>,
    pub priority: i32,
    pub last_used_at: Option<i64>,
    pub in_use: bool,
    pub is_default: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchEvent {
    pub from_alias: Option<String>,
    pub to_alias: String,
    pub reason: SwitchReason,
    pub thread_id: Option<String>,
}

impl SwitchEvent {
    /// The one conversation line that says where the work went and why.
    pub fn summary(&self) -> String {
        let why = match self.reason {
            SwitchReason::Threshold => "使用量がしきい値に近いため",
            SwitchReason::LimitReached => "上限に達したため",
            SwitchReason::Manual => "手動",
        };
        match &self.from_alias {
            Some(from) => format!("アカウントを「{from}」から「{}」に切り替えました({why})", self.to_alias),
            None => format!("アカウント「{}」を使います({why})", self.to_alias),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PoolEvent {
    /// The list changed; re-read it with `list`.
    Updated,
    Switched(SwitchEvent),
    LoginCompleted {
        login_id: String,
        result: Result<String, String>,
    },
}
