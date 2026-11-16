//! Account usage of the subscription providers: rate-limit windows for the
//! OAuth kinds and the Theos allowance for DeepAI. Endpoints, headers and
//! fields follow the reference implementation; see
//! docs/evidence/account-usage.md.

use serde::Serialize;
use serde_json::Value;

use crate::deepai::TheosAllowance;
use crate::ErrorInfo;

/// Account usage as one provider reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Usage {
    /// Plan or tier name when the provider reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Rate-limit windows, in the order the provider lists them.
    pub windows: Vec<UsageWindow>,
    /// DeepAI's daily Theos allowance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theos: Option<TheosAllowance>,
}

/// One rate-limit window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageWindow {
    /// The provider's name for the window (`primary`, `session`, `five_hour`, …).
    pub name: String,
    /// Share of the window used, clamped to 0–100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    /// When the window resets, as the provider writes it (epoch seconds or
    /// milliseconds, or a date string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    /// Window length in seconds when the provider states it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_seconds: Option<u64>,
}

/// A percentage as a number or numeric string, clamped to 0–100 (reference
/// `normalizeUsagePercent`).
fn percent(value: Option<&Value>) -> Option<f64> {
    let number = match value? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    number.is_finite().then(|| number.clamp(0.0, 100.0))
}

/// A reset time kept as written: numbers are rendered, strings kept.
fn reset(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
        _ => None,
    }
}

/// `GET {chatgpt}/backend-api/wham/usage` → `rate_limit.{primary,secondary,tertiary}_window`
/// with `used_percent`, `reset_at`, `limit_window_seconds` (reference
/// `parseUsageQuota`, `WhamUsageResponse`).
pub(crate) fn from_wham(value: &Value) -> Result<Usage, ErrorInfo> {
    let rate_limit = value.get("rate_limit").filter(|v| v.is_object());
    let windows: Vec<UsageWindow> = ["primary", "secondary", "tertiary"]
        .into_iter()
        .filter_map(|name| {
            let window = rate_limit?.get(format!("{name}_window"))?;
            window.is_object().then(|| UsageWindow {
                name: name.to_string(),
                used_percent: percent(window.get("used_percent")),
                resets_at: reset(window.get("reset_at")),
                window_seconds: window.get("limit_window_seconds").and_then(Value::as_u64),
            })
        })
        .collect();
    if windows.is_empty() {
        return Err(no_usage("ChatGPT"));
    }
    Ok(Usage {
        plan: value
            .get("plan_type")
            .and_then(Value::as_str)
            .map(str::to_string),
        windows,
        theos: None,
    })
}

/// `GET https://api.anthropic.com/api/oauth/usage`: the `limits[]` rows
/// (`kind`, `percent`, `resets_at`, model scope) when present, else the older
/// `five_hour` / `seven_day*` objects with `utilization` (reference
/// `anthropicUsageBuckets`).
pub(crate) fn from_anthropic(value: &Value) -> Result<Usage, ErrorInfo> {
    let mut windows: Vec<UsageWindow> = value
        .get("limits")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let kind = row.get("kind")?.as_str()?;
                    let scope = row
                        .get("scope")
                        .and_then(|scope| scope.get("model"))
                        .and_then(|model| model.get("display_name"))
                        .and_then(Value::as_str);
                    Some(UsageWindow {
                        name: match scope {
                            Some(model) => format!("{kind}:{model}"),
                            None => kind.to_string(),
                        },
                        used_percent: percent(row.get("percent")),
                        resets_at: reset(row.get("resets_at")),
                        window_seconds: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if windows.is_empty() {
        windows = [
            "five_hour",
            "seven_day",
            "seven_day_fable",
            "seven_day_opus",
            "seven_day_sonnet",
        ]
        .into_iter()
        .filter_map(|name| {
            let bucket = value.get(name).filter(|bucket| bucket.is_object())?;
            Some(UsageWindow {
                name: name.to_string(),
                used_percent: percent(bucket.get("utilization")),
                resets_at: reset(bucket.get("resets_at")),
                window_seconds: None,
            })
        })
        .collect();
    }
    if windows.is_empty() {
        return Err(no_usage("Anthropic"));
    }
    Ok(Usage {
        plan: None,
        windows,
        theos: None,
    })
}

/// The Muse key-mint answer: `subs_usage.window` (`used_percent`,
/// `resets_at`, `window_duration_mins`) and `subs_usage.weekly`, with the tier
/// in `subs_tier_name` (reference `museUsageWindowsToQuota`). The payload
/// also carries the API key, which is never read here.
pub(crate) fn from_muse(value: &Value) -> Result<Usage, ErrorInfo> {
    let plan = value
        .get("subs_tier_name")
        .and_then(Value::as_str)
        .map(str::to_string);
    if value.get("is_subs_active").and_then(Value::as_bool) == Some(false) {
        return Ok(Usage {
            plan,
            windows: Vec::new(),
            theos: None,
        });
    }
    let usage = value.get("subs_usage").filter(|usage| usage.is_object());
    let mut windows = Vec::new();
    if let Some(window) = usage
        .and_then(|usage| usage.get("window"))
        .filter(|w| w.is_object())
    {
        windows.push(UsageWindow {
            name: "window".to_string(),
            used_percent: percent(window.get("used_percent")),
            resets_at: reset(window.get("resets_at")),
            window_seconds: window
                .get("window_duration_mins")
                .and_then(Value::as_u64)
                .map(|minutes| minutes * 60),
        });
    }
    if let Some(weekly) = usage
        .and_then(|usage| usage.get("weekly"))
        .filter(|w| w.is_object())
    {
        windows.push(UsageWindow {
            name: "weekly".to_string(),
            used_percent: percent(weekly.get("used_percent")),
            resets_at: reset(weekly.get("resets_at")),
            window_seconds: None,
        });
    }
    if windows.is_empty() {
        return Err(no_usage("Muse"));
    }
    Ok(Usage {
        plan,
        windows,
        theos: None,
    })
}

fn no_usage(provider: &str) -> ErrorInfo {
    ErrorInfo::new(
        "provider_protocol",
        format!("{provider} returned no usage windows"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wham_windows_keep_their_order_and_units() {
        let usage = from_wham(&json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "primary_window": {"used_percent": 12.5, "reset_at": 1794894516, "limit_window_seconds": 18000},
                "secondary_window": {"used_percent": "140", "reset_at": "1795000000", "limit_window_seconds": 604800},
                "tertiary_window": null
            }
        }))
        .expect("usage");
        assert_eq!(usage.plan.as_deref(), Some("plus"));
        assert_eq!(usage.windows.len(), 2);
        assert_eq!(usage.windows[0].used_percent, Some(12.5));
        assert_eq!(usage.windows[0].resets_at.as_deref(), Some("1794894516"));
        assert_eq!(usage.windows[0].window_seconds, Some(18000));
        assert_eq!(usage.windows[1].used_percent, Some(100.0), "clamped");
        assert!(from_wham(&json!({"rate_limit": {}})).is_err());
    }

    #[test]
    fn anthropic_prefers_limit_rows_and_falls_back_to_buckets() {
        let rows = from_anthropic(&json!({
            "limits": [
                {"kind": "session", "percent": 30, "resets_at": "2026-11-16T15:00:00Z", "is_active": true},
                {"kind": "weekly_scoped", "percent": 5, "scope": {"model": {"display_name": "Opus"}}}
            ],
            "five_hour": {"utilization": 99}
        }))
        .expect("usage");
        let names: Vec<&str> = rows.windows.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["session", "weekly_scoped:Opus"]);
        let buckets = from_anthropic(&json!({
            "five_hour": {"utilization": 41.0, "resets_at": 1794894516},
            "seven_day": {"utilization": 7}
        }))
        .expect("usage");
        let names: Vec<&str> = buckets.windows.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["five_hour", "seven_day"]);
        assert!(from_anthropic(&json!({})).is_err());
    }

    #[test]
    fn muse_reads_subscription_windows_and_never_the_key() {
        let usage = from_muse(&json!({
            "api_key": "LLM|secret",
            "is_subs_active": true,
            "subs_tier_name": "Plus",
            "subs_usage": {
                "window": {"used_percent": 20, "resets_at": 1794894516, "window_duration_mins": 300},
                "weekly": {"used_percent": 3, "resets_at": 1795000000}
            }
        }))
        .expect("usage");
        assert_eq!(usage.plan.as_deref(), Some("Plus"));
        assert_eq!(usage.windows[0].window_seconds, Some(18000));
        assert_eq!(usage.windows[1].name, "weekly");
        assert!(!format!("{usage:?}").contains("secret"));
        let inactive = from_muse(&json!({"is_subs_active": false})).expect("inactive");
        assert!(inactive.windows.is_empty());
    }
}
