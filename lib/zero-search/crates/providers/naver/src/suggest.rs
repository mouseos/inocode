//! Naver search suggestions.
//!
//! The public autocomplete endpoint `ac.search.naver.com/nx/ac` answers
//! JSON anonymously (verified live during the APK analysis); candidates
//! live in `items[0]` as `[keyword, kind]` pairs. No key or signature is
//! involved.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::*;
use crate::urlbuild::{build_search_url, suggest_url};
use crate::NaverSearchClient;

/// One autocomplete suggestion, with the integrated-search URL for it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub keyword: String,
    pub search_url: String,
    /// The candidate kind the API pairs with the keyword (the second
    /// element of each `[keyword, kind]` pair, e.g. `"0"` / `"1"`).
    /// Kept verbatim: the audit records the pair shape but the kind
    /// vocabulary is the server's (audit W-25).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// The full parsed suggest response: the candidates plus the
/// response's `answer` / `intend` groups.
#[derive(Debug, Clone, Default)]
pub struct ParsedSuggest {
    pub suggestions: Vec<Suggestion>,
    /// The response's `answer` group, preserved verbatim. It was
    /// empty in every capture taken during the analysis, so its
    /// element structure is not modelled — but it is no longer
    /// silently dropped (audit W-25).
    pub answer: Vec<Value>,
    /// The response's `intend` group, preserved verbatim (same
    /// reasoning as `answer`).
    pub intend: Vec<Value>,
}

/// Response envelope for [`NaverSearchClient::suggest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestResult {
    pub provider: String,
    pub vertical: String,
    pub ok: bool,
    pub query: String,
    pub count: usize,
    pub suggestions: Vec<Suggestion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answer: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intend: Vec<Value>,
}

/// Parse the suggestions API JSON (offline-testable): the
/// candidates from `items[0]` plus the `answer` / `intend` groups.
/// Anything malformed yields empty lists.
pub fn parse_suggest_response(value: &Value) -> ParsedSuggest {
    let mut parsed = ParsedSuggest {
        answer: value
            .get("answer")
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default(),
        intend: value
            .get("intend")
            .and_then(|a| a.as_array())
            .cloned()
            .unwrap_or_default(),
        ..Default::default()
    };
    let Some(first) = value
        .get("items")
        .and_then(|i| i.as_array())
        .and_then(|groups| groups.first())
        .and_then(|g| g.as_array())
    else {
        return parsed;
    };
    for entry in first {
        let Some(pair) = entry.as_array() else {
            continue;
        };
        let Some(keyword) = pair
            .first()
            .and_then(|k| k.as_str())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        parsed.suggestions.push(Suggestion {
            keyword: keyword.to_string(),
            search_url: build_search_url(keyword, "integrated").unwrap_or_default(),
            kind: pair.get(1).and_then(|k| match k {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            }),
        });
    }
    parsed
}

/// Parse only the candidate list (kept for existing callers).
pub fn parse_suggest_json(value: &Value) -> Vec<Suggestion> {
    parse_suggest_response(value).suggestions
}

impl NaverSearchClient {
    /// Fetch autocomplete suggestions for a (partial) query.
    pub async fn suggest(&self, query: &str) -> Result<SuggestResult> {
        let query = query.trim();
        if query.is_empty() {
            return Err(anyhow::anyhow!("query must not be empty"));
        }
        let url = suggest_url(query);
        let json = self.get_json(&url, "https://search.naver.com/").await?;
        if json.get("items").is_none() {
            return Err(layout_error("suggest"));
        }
        let parsed = parse_suggest_response(&json);
        Ok(SuggestResult {
            provider: crate::PROVIDER.to_string(),
            vertical: "suggest".to_string(),
            ok: true,
            query: query.to_string(),
            count: parsed.suggestions.len(),
            suggestions: parsed.suggestions,
            answer: parsed.answer,
            intend: parsed.intend,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_first_items_group() {
        let value: Value = serde_json::from_str(
            r#"{"query":["네이"],"items":[[["네이버웹툰","0"],["네이트","1"]]]}"#,
        )
        .unwrap();
        let out = parse_suggest_json(&value);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].keyword, "네이버웹툰");
        assert!(out[0].search_url.contains("m.search.naver.com"));
    }

    #[test]
    fn parse_keeps_kind_and_answer_intend_groups() {
        let value: Value = serde_json::from_str(
            r#"{"query":["네이"],"answer":[{"x":1}],"intend":["y"],"items":[[["네이버웹툰","0"],["네이트","1"]]]}"#,
        )
        .unwrap();
        let parsed = parse_suggest_response(&value);
        assert_eq!(parsed.suggestions[0].kind.as_deref(), Some("0"));
        assert_eq!(parsed.suggestions[1].kind.as_deref(), Some("1"));
        assert_eq!(parsed.answer.len(), 1);
        assert_eq!(parsed.intend.len(), 1);
    }

    #[test]
    fn parse_tolerates_missing_items() {
        assert!(parse_suggest_json(&serde_json::json!({})).is_empty());
    }
}
