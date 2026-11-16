//! Thinking and reasoning controls on the request, per wire format. The
//! rules follow opencodex; see docs/evidence/reasoning-controls.md.

use serde_json::{json, Map, Value};

use crate::oauth::AuthKind;
use crate::{ErrorInfo, ProviderKind, DEFAULT_MAX_TOKENS as BASE_MAX_TOKENS};

const EFFORT_ENV: &str = "INOCODE_LLM_REASONING";
const SUMMARY_ENV: &str = "INOCODE_LLM_REASONING_SUMMARY";
/// Room kept for the answer beyond the thinking budget.
const ANSWER_HEADROOM: u32 = 8192;
const LEGACY_CEILING: u32 = 32_000;
const ADAPTIVE_CEILING: u32 = 40_192;
const MIN_BUDGET: u32 = 1024;
const BUDGET_FLOOR_GAP: u32 = 4096;
/// Beta that unlocks `display: "updates"` (Opus 5.5 on the first-party API).
pub(crate) const DISPLAY_UPDATES_BETA: &str = "thinking-display-updates-2026-08-18";

/// Requested reasoning depth. `ultra` is accepted as `Max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Effort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "none" => Self::None,
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::Xhigh,
            "max" | "ultra" => Self::Max,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Anthropic `budget_tokens` for this effort.
    fn budget(self) -> u32 {
        match self {
            Self::None | Self::Minimal => 1024,
            Self::Low => 4096,
            Self::Medium => 8192,
            Self::High => 16_384,
            Self::Xhigh => 24_576,
            Self::Max => 32_000,
        }
    }
}

/// How much of the reasoning the provider shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Summary {
    Auto,
    Concise,
    Detailed,
    None,
}

impl Summary {
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "concise" => Self::Concise,
            "detailed" => Self::Detailed,
            "none" => Self::None,
            _ => return None,
        })
    }
}

/// Every effort, for models whose provider publishes no list.
pub const ALL_EFFORTS: [Effort; 7] = [
    Effort::None,
    Effort::Minimal,
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::Xhigh,
    Effort::Max,
];

/// The effort used when neither the user nor the model's catalog chooses
/// one (opencodex `codex/catalog/effort.ts` falls back to medium).
pub const FALLBACK_EFFORT: Effort = Effort::Medium;

/// Reasoning settings of one client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reasoning {
    /// `None` uses the model's catalog default, else [`FALLBACK_EFFORT`].
    pub effort: Option<Effort>,
    pub summary: Summary,
}

impl Default for Reasoning {
    fn default() -> Self {
        Self {
            effort: None,
            summary: Summary::Auto,
        }
    }
}

impl Reasoning {
    pub fn from_env() -> Result<Self, ErrorInfo> {
        let mut reasoning = Self::default();
        if let Ok(text) = std::env::var(EFFORT_ENV) {
            reasoning.effort = Some(Effort::parse(&text).ok_or_else(|| {
                ErrorInfo::new(
                    "provider_config",
                    format!("{EFFORT_ENV} must be none, minimal, low, medium, high, xhigh, max, or ultra"),
                )
            })?);
        }
        if let Ok(text) = std::env::var(SUMMARY_ENV) {
            reasoning.summary = Summary::parse(&text).ok_or_else(|| {
                ErrorInfo::new(
                    "provider_config",
                    format!("{SUMMARY_ENV} must be auto, concise, detailed, or none"),
                )
            })?;
        }
        Ok(reasoning)
    }
}

/// Where a request goes, as far as the reasoning rules care.
pub(crate) struct Destination<'a> {
    pub format: ProviderKind,
    pub auth: AuthKind,
    pub model: &'a str,
    /// The request reaches api.anthropic.com itself.
    pub first_party_anthropic: bool,
}

/// Adds the reasoning fields (and Anthropic's `max_tokens`) to `body`. The
/// effort is sent as chosen: a provider that does not accept it answers with
/// its own error.
pub(crate) fn apply(
    body: &mut Map<String, Value>,
    to: &Destination,
    effort: Effort,
    summary: Summary,
) {
    match to.format {
        ProviderKind::Anthropic => anthropic(body, to, effort, summary),
        ProviderKind::Responses => responses(body, to, effort, summary),
        ProviderKind::OpenAi => chat(body, effort),
    }
}

/// The extra `anthropic-beta` a request needs for its thinking display.
pub(crate) fn anthropic_beta(to: &Destination, summary: Summary) -> Option<&'static str> {
    (native_opus_display(to, summary) == Some("updates")).then_some(DISPLAY_UPDATES_BETA)
}

/// Family and version of a Claude model id (`claude-sonnet-4-5`,
/// `vendor/claude-opus-5.5`, `claude-sonnet-4-20250514` → 4.0).
fn claude_version(model: &str) -> Option<(&str, u32, u32)> {
    let start = model
        .match_indices("claude-")
        .map(|(index, _)| index)
        .find(|&index| index == 0 || model.as_bytes()[index - 1] == b'/')?;
    let rest = &model[start + "claude-".len()..];
    let family_end = rest.find(|c: char| !c.is_ascii_lowercase())?;
    let (family, rest) = rest.split_at(family_end);
    let rest = rest.strip_prefix('-')?;
    let major_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let major = rest[..major_end].parse().ok()?;
    let rest = &rest[major_end..];
    let minor = rest
        .strip_prefix(['.', '-'])
        .map(|rest| {
            let digits = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            if (1..=2).contains(&digits) {
                rest[..digits].parse().unwrap_or(0)
            } else {
                0
            }
        })
        .unwrap_or(0);
    (!family.is_empty()).then_some((family, major, minor))
}

fn at_least(model: &str, family: &str, version: (u32, u32)) -> bool {
    claude_version(model).is_some_and(|(f, major, minor)| f == family && (major, minor) >= version)
}

/// Models that take `thinking: {type: "adaptive"}` with `output_config.effort`.
fn adaptive(model: &str) -> bool {
    at_least(model, "sonnet", (5, 0))
        || at_least(model, "opus", (4, 7))
        || at_least(model, "fable", (0, 0))
        || at_least(model, "haiku", (5, 5))
}

/// Models that accept `thinking: {type: "disabled"}`.
fn explicit_disable(model: &str) -> bool {
    (at_least(model, "sonnet", (5, 0)) && !at_least(model, "sonnet", (5, 5)))
        || at_least(model, "haiku", (5, 5))
}

/// The `display` Opus 5.5 always gets on the first-party API.
fn native_opus_display(to: &Destination, summary: Summary) -> Option<&'static str> {
    (to.first_party_anthropic && at_least(to.model, "opus", (5, 5))).then_some(match summary {
        Summary::Concise | Summary::Detailed => "summarized",
        _ => "updates",
    })
}

fn anthropic(body: &mut Map<String, Value>, to: &Destination, effort: Effort, summary: Summary) {
    let adaptive_max = |effort: Effort| {
        ADAPTIVE_CEILING.min(BASE_MAX_TOKENS.max(effort.budget() + ANSWER_HEADROOM))
    };
    if let Some(display) = native_opus_display(to, summary) {
        body.insert(
            "thinking".into(),
            json!({ "type": "adaptive", "display": display }),
        );
        if effort != Effort::None {
            body.insert(
                "output_config".into(),
                json!({ "effort": output_effort(effort) }),
            );
        }
        body.insert(
            "max_tokens".into(),
            json!(adaptive_max(effort.max(Effort::Medium))),
        );
        return;
    }
    let max_tokens = match effort {
        Effort::None => {
            if explicit_disable(to.model) {
                body.insert("thinking".into(), json!({ "type": "disabled" }));
            } else if at_least(to.model, "sonnet", (5, 5)) {
                body.insert("thinking".into(), json!({ "type": "between_tools" }));
            }
            BASE_MAX_TOKENS
        }
        _ if adaptive(to.model) => {
            let thinking = match summary {
                Summary::None => json!({ "type": "adaptive" }),
                _ => json!({ "type": "adaptive", "display": "summarized" }),
            };
            body.insert("thinking".into(), thinking);
            body.insert(
                "output_config".into(),
                json!({ "effort": output_effort(effort) }),
            );
            adaptive_max(effort)
        }
        _ => {
            let max_tokens =
                LEGACY_CEILING.min(BASE_MAX_TOKENS.max(effort.budget() + ANSWER_HEADROOM));
            let budget = MIN_BUDGET.max(effort.budget().min(max_tokens - BUDGET_FLOOR_GAP));
            body.insert(
                "thinking".into(),
                json!({ "type": "enabled", "budget_tokens": budget }),
            );
            max_tokens
        }
    };
    body.insert("max_tokens".into(), json!(max_tokens));
}

/// `output_config.effort` has no `minimal`.
fn output_effort(effort: Effort) -> &'static str {
    match effort {
        Effort::Minimal => "low",
        other => other.as_str(),
    }
}

fn responses(body: &mut Map<String, Value>, to: &Destination, effort: Effort, summary: Summary) {
    // The ChatGPT backend takes `none` as a level; elsewhere it means
    // sending no reasoning at all.
    let effort = match (to.auth, effort) {
        (AuthKind::GptOauth, effort) => Some(effort),
        (_, Effort::None) => None,
        (_, effort) => Some(effort),
    };
    let summary = match summary {
        Summary::Auto => Some("auto"),
        Summary::Concise => Some("concise"),
        Summary::Detailed => Some("detailed"),
        Summary::None => None,
    };
    let mut fields = Map::new();
    if let Some(effort) = effort {
        fields.insert("effort".into(), json!(effort.as_str()));
    }
    if let (Some(summary), true) = (summary, effort.is_some_and(|e| e != Effort::None)) {
        fields.insert("summary".into(), json!(summary));
    }
    if !fields.is_empty() {
        body.insert("reasoning".into(), Value::Object(fields));
    }
}

fn chat(body: &mut Map<String, Value>, effort: Effort) {
    if effort != Effort::None {
        body.insert("reasoning_effort".into(), json!(effort.as_str()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to(format: ProviderKind, auth: AuthKind, model: &str) -> Destination<'_> {
        Destination {
            format,
            auth,
            model,
            first_party_anthropic: auth == AuthKind::ClaudeOauth,
        }
    }

    fn body(to: &Destination, effort: Effort, summary: Summary) -> Value {
        let mut body = Map::new();
        apply(&mut body, to, effort, summary);
        Value::Object(body)
    }

    #[test]
    fn claude_versions_follow_the_reference_pattern() {
        assert_eq!(claude_version("claude-haiku-5-5"), Some(("haiku", 5, 5)));
        assert_eq!(claude_version("claude-sonnet-5"), Some(("sonnet", 5, 0)));
        assert_eq!(
            claude_version("anthropic/claude-opus-4.7"),
            Some(("opus", 4, 7))
        );
        assert_eq!(
            claude_version("claude-sonnet-4-20250514"),
            Some(("sonnet", 4, 0))
        );
        assert_eq!(
            claude_version("claude-sonnet-4-5-20250929"),
            Some(("sonnet", 4, 5))
        );
        assert_eq!(claude_version("myclaude-opus-5"), None);
        assert_eq!(claude_version("logfare/auto"), None);
    }

    #[test]
    fn anthropic_shapes_by_model() {
        let oauth = AuthKind::ClaudeOauth;
        let adaptive = body(
            &to(ProviderKind::Anthropic, oauth, "claude-sonnet-5-5"),
            Effort::High,
            Summary::Auto,
        );
        assert_eq!(
            adaptive["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(adaptive["output_config"], json!({"effort": "high"}));
        assert_eq!(adaptive["max_tokens"], 24_576);

        let legacy = body(
            &to(ProviderKind::Anthropic, AuthKind::ApiKey, "logfare/auto"),
            Effort::Max,
            Summary::Auto,
        );
        assert_eq!(legacy["max_tokens"], 32_000);
        assert_eq!(
            legacy["thinking"],
            json!({"type": "enabled", "budget_tokens": 27_904})
        );

        let off = |model| {
            body(
                &to(ProviderKind::Anthropic, oauth, model),
                Effort::None,
                Summary::Auto,
            )
        };
        assert_eq!(
            off("claude-haiku-5-5")["thinking"],
            json!({"type": "disabled"})
        );
        assert_eq!(
            off("claude-sonnet-5-5")["thinking"],
            json!({"type": "between_tools"})
        );
        assert!(off("claude-fable-5-2").get("thinking").is_none());
        assert_eq!(off("claude-fable-5-2")["max_tokens"], 8192);

        let minimal = body(
            &to(ProviderKind::Anthropic, oauth, "claude-haiku-5-5"),
            Effort::Minimal,
            Summary::None,
        );
        assert_eq!(minimal["thinking"], json!({"type": "adaptive"}));
        assert_eq!(minimal["output_config"], json!({"effort": "low"}));
    }

    #[test]
    fn first_party_opus_5_5_shows_updates_with_its_beta() {
        let native = to(
            ProviderKind::Anthropic,
            AuthKind::ClaudeOauth,
            "claude-opus-5-5",
        );
        let mut fields = Map::new();
        apply(&mut fields, &native, FALLBACK_EFFORT, Summary::Auto);
        assert_eq!(
            fields["thinking"],
            json!({"type": "adaptive", "display": "updates"})
        );
        assert_eq!(
            anthropic_beta(&native, Summary::Auto),
            Some(DISPLAY_UPDATES_BETA)
        );
        assert_eq!(anthropic_beta(&native, Summary::Concise), None);
        let proxied = Destination {
            first_party_anthropic: false,
            ..native
        };
        assert_eq!(anthropic_beta(&proxied, Summary::Auto), None);
    }

    #[test]
    fn responses_and_chat_fields() {
        let codex = to(ProviderKind::Responses, AuthKind::GptOauth, "gpt-5.2-codex");
        assert_eq!(
            body(&codex, Effort::Max, Summary::Auto)["reasoning"],
            json!({"effort": "max", "summary": "auto"}),
            "sent as chosen; the provider rejects what it does not take"
        );
        assert_eq!(
            body(&codex, Effort::None, Summary::Auto)["reasoning"],
            json!({"effort": "none"})
        );
        let muse = to(
            ProviderKind::Responses,
            AuthKind::MuseSubscription,
            "muse-spark-1.3",
        );
        assert!(body(&muse, Effort::None, Summary::Auto)
            .get("reasoning")
            .is_none());
        assert_eq!(
            body(&muse, Effort::Low, Summary::None)["reasoning"],
            json!({"effort": "low"})
        );

        let chat = |model| {
            body(
                &to(ProviderKind::OpenAi, AuthKind::ApiKey, model),
                Effort::Medium,
                Summary::Auto,
            )
        };
        assert_eq!(chat("glm-5.3-flash")["reasoning_effort"], "medium");
        assert_eq!(chat("deepseek-v4-pro")["reasoning_effort"], "medium");
        assert!(body(
            &to(ProviderKind::OpenAi, AuthKind::ApiKey, "m"),
            Effort::None,
            Summary::Auto
        )
        .get("reasoning_effort")
        .is_none());
    }

    #[test]
    fn efforts_parse_with_ultra_as_max() {
        assert_eq!(Effort::parse("ULTRA"), Some(Effort::Max));
        assert_eq!(Effort::parse("xhigh"), Some(Effort::Xhigh));
        assert_eq!(Effort::parse("huge"), None);
        assert_eq!(Summary::parse("detailed"), Some(Summary::Detailed));
    }
}
