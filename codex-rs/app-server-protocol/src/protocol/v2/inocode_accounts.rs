//! Experimental `inocode/accounts/*` extension APIs: the native account pool
//! (several ChatGPT accounts and OpenAI API keys, switched by policy).
//! Contract: ref/docs/16-account-pool-contract.md. Design background: ref/docs/15-account-pool-and-providers.md (option B).
//! Account e-mail addresses and credentials are never part of any of these types.

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

/// Which kind of credential an entry holds. Other providers plug in here later.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeAccountKind {
    Chatgpt,
    ApiKey,
    /// A Claude (Pro/Max) OAuth sign-in.
    ClaudeOauth,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeAccountState {
    Active,
    Cooldown,
    Disabled,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeAccountWindowKind {
    FiveHour,
    Weekly,
    /// The higher of the two.
    Both,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeAccountSelection {
    /// Lowest usage first.
    Quota,
    RoundRobin,
    /// Highest priority first; move on only when it is unusable.
    FillFirst,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeApiKeyOrder {
    /// API keys are used only when every ChatGPT account is over the threshold or cooling down.
    AfterChatgpt,
    /// API keys are used before ChatGPT accounts.
    BeforeChatgpt,
    /// API keys and ChatGPT accounts are ranked together.
    Mixed,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeAccountSwitchReason {
    Threshold,
    LimitReached,
    Manual,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountUsageWindow {
    /// 0-100.
    pub used_percent: f64,
    #[ts(type = "number | null")]
    pub window_minutes: Option<i64>,
    /// Unix seconds.
    #[ts(type = "number | null")]
    pub resets_at: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountUsage {
    pub five_hour: Option<InocodeAccountUsageWindow>,
    pub weekly: Option<InocodeAccountUsageWindow>,
    /// Unix seconds of the last refresh; `null` when never fetched.
    #[ts(type = "number | null")]
    pub updated_at: Option<i64>,
}

/// One pool entry. There is deliberately no e-mail address: identify by `alias` and `plan`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccount {
    pub id: String,
    pub kind: InocodeAccountKind,
    /// Provider group the entry belongs to: `openai` or `claude`.
    pub provider: String,
    /// User-chosen name.
    pub alias: String,
    /// Plan name for ChatGPT accounts (for example `plus`, `pro`); `null` for API keys.
    pub plan: Option<String>,
    /// `null` for API keys (no usage concept) or before the first fetch.
    pub usage: Option<InocodeAccountUsage>,
    pub state: InocodeAccountState,
    /// Unix seconds; set while `state` is `cooldown`.
    #[ts(type = "number | null")]
    pub cooldown_until: Option<i64>,
    /// Larger is used earlier.
    pub priority: i32,
    /// Unix seconds.
    #[ts(type = "number | null")]
    pub last_used_at: Option<i64>,
    /// True for the entry used by the next request.
    pub in_use: bool,
    /// True for the legacy `auth.json` login shown as the default account.
    pub is_default: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountPolicy {
    /// Switch before a window would exceed this percentage. 0-100, default 90.
    pub threshold_percent: f64,
    pub window: InocodeAccountWindowKind,
    pub selection: InocodeAccountSelection,
    pub api_key_order: InocodeApiKeyOrder,
    /// Cooldown in seconds when a limit error carries no reset time. Default 60.
    #[ts(type = "number")]
    pub default_cooldown_seconds: i64,
    /// Master switch for threshold/limit-based switching. Default true.
    pub auto_switch: bool,
}

// ---------------------------------------------------------------- methods

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsListParams {
    /// Refresh usage from the backend before answering. Default false.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub refresh: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsListResponse {
    pub accounts: Vec<InocodeAccount>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsAddApiKeyParams {
    pub alias: String,
    pub api_key: String,
    /// `openai` (default) or `claude`.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub provider: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub priority: Option<i32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsAddApiKeyResponse {
    pub account: InocodeAccount,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsRemoveParams {
    pub id: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsRemoveResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsRenameParams {
    pub id: String,
    pub alias: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsRenameResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsSetPriorityParams {
    pub id: String,
    pub priority: i32,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsSetPriorityResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsSetEnabledParams {
    pub id: String,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsSetEnabledResponse {}

/// Manual switch. Takes effect from the next request of every thread.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsUseParams {
    pub id: String,
    /// Thread shown in the resulting `inocode/accounts/switched` notification.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsUseResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeAccountLoginMethod {
    Browser,
    DeviceCode,
    /// Claude only: the provider shows `code#state`; hand it back with `login/complete`.
    OauthCode,
}

/// Adds a ChatGPT account (never replaces the current login). Same flow as `account/login/start`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsLoginStartParams {
    pub alias: String,
    /// `openai` (default) or `claude`.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub provider: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub method: Option<InocodeAccountLoginMethod>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsLoginStartResponse {
    pub login_id: String,
    /// Browser method: open this URL.
    pub auth_url: Option<String>,
    /// Device-code method.
    pub verification_url: Option<String>,
    pub user_code: Option<String>,
    /// True when the client must show a code box and call `login/complete`.
    pub code_entry: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsLoginCompleteParams {
    pub login_id: String,
    /// What the user pasted: `code#state`, the final redirect URL, or the bare code.
    pub code: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsLoginCompleteResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum InocodeProviderWire {
    Responses,
    Chat,
    Anthropic,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeProvider {
    pub id: String,
    pub name: String,
    pub wire: InocodeProviderWire,
    /// Which kinds of account can be added.
    pub auth_kinds: Vec<InocodeAccountKind>,
    pub account_count: u32,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeProvidersListParams {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeProvidersListResponse {
    pub providers: Vec<InocodeProvider>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsLoginCancelParams {
    pub login_id: String,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsLoginCancelResponse {}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsPolicyReadParams {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsPolicyReadResponse {
    pub policy: InocodeAccountPolicy,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsPolicyWriteParams {
    pub policy: InocodeAccountPolicy,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsPolicyWriteResponse {
    pub policy: InocodeAccountPolicy,
}

// ---------------------------------------------------------- notifications

/// The account list changed (added, removed, usage refreshed, state changed...).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsUpdatedNotification {
    pub accounts: Vec<InocodeAccount>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsSwitchedNotification {
    /// Alias of the account used before; `null` when none.
    pub from_alias: Option<String>,
    pub to_alias: String,
    pub reason: InocodeAccountSwitchReason,
    pub thread_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct InocodeAccountsLoginCompletedNotification {
    pub login_id: String,
    pub success: bool,
    /// The new entry's id on success.
    pub account_id: Option<String>,
    pub error: Option<String>,
}
