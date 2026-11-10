//! Bing search suggestions (the app's own endpoint, APK `d4.java`):
//! `https://www.bingapis.com/api/v7/suggestions` with the app-embedded
//! public `appid`. No search-page scraping is involved; the API answers
//! JSON anonymously. `images` / `videos` path variants return candidates
//! for those verticals.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::*;
use crate::urlbuild::suggest_url;
use crate::BingSearchClient;

/// One suggestion entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub keyword: String,
    pub search_url: String,
}

/// Suggestion response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestResult {
    pub provider: String,
    pub query: String,
    pub vertical: String,
    pub suggestions: Vec<Suggestion>,
    pub count: usize,
    pub source_url: String,
}

/// Parse the suggestions API JSON into suggestion entries (offline-testable).
pub fn parse_suggest_json(value: &Value) -> Vec<Suggestion> {
    let mut out = Vec::new();
    let Some(groups) = value.get("suggestionGroups").and_then(|g| g.as_array()) else {
        return out;
    };
    for group in groups {
        let Some(list) = group.get("searchSuggestions").and_then(|s| s.as_array()) else {
            continue;
        };
        for entry in list {
            let keyword = entry
                .get("displayText")
                .and_then(|t| t.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| entry.get("query").and_then(|t| t.as_str()))
                .unwrap_or("")
                .to_string();
            if keyword.is_empty() {
                continue;
            }
            let search_url = entry
                .get("url")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            out.push(Suggestion {
                keyword,
                search_url,
            });
        }
    }
    out
}

impl BingSearchClient {
    /// Fetch suggestions for `query`. `vertical` is `web` (default),
    /// `images` or `videos`.
    pub async fn suggest(
        &self,
        query: &str,
        vertical: &str,
        mkt: &str,
        setlang: &str,
    ) -> Result<SuggestResult> {
        let url = suggest_url(query, vertical, mkt, setlang)?;
        let json = self.get_json(&url).await?;
        let suggestions = parse_suggest_json(&json);
        if json.get("suggestionGroups").is_none() {
            return Err(layout_error("suggest"));
        }
        Ok(SuggestResult {
            provider: crate::PROVIDER.to_string(),
            query: query.trim().to_string(),
            vertical: crate::urlbuild::normalize_vertical(vertical).to_string(),
            count: suggestions.len(),
            suggestions,
            source_url: url,
        })
    }
}
