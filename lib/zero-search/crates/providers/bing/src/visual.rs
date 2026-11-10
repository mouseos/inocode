//! Bing visual search (search by image), via the app's camera
//! endpoint `POST https://www.bingapis.com/api/v7/images/visualsearch`
//! (APK `com/microsoft/unifiedcamera/functions/imageknowledge/a.java`,
//! native-protocol audit §5; audit W-17).
//!
//! The app POSTs a multipart form with two text parts:
//! `knowledgeRequest` — a Gson JSON document describing the
//! recognition skills to run — and `imageBase64` — the image,
//! base64-encoded. It also sends `Authorization: Bearer <token>`
//! where the token comes from a separate auth flow; before that
//! flow completes the header carries an *empty* bearer, which is
//! the state reproduced here (no token is fabricated).
//!
//! Live status (2026-11-08): in that pre-auth state the endpoint
//! answered **HTTP 401 Unauthorized** — the auth token is
//! effectively required. Completing the app's token flow
//! (`bingapiauth.sapphire.microsoftapp.net` + `Sec-MS-GEC`) is a
//! separate piece of work; the request construction and response
//! parser here are implemented and offline-tested.
//!
//! The response is the ImageKnowledge document: `tags[]`, each
//! with a `displayName` and `actions[]` (SimilarImages,
//! PagesIncluding, VisualSearchResults, Entities, …). Actions are
//! surfaced as (type, item count, first items' titles/URLs)
//! summaries rather than dropped.

use anyhow::{anyhow, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::*;
use crate::BingSearchClient;

/// App-embedded public identifier for the visual-search API
/// (APK `imageknowledge/a.java`). Ships inside the app; not a
/// user credential.
pub const VISUAL_APPID: &str = "AFABE67E806C5160B119CA98774705698A6CFE7C";
/// Visual-search API base (the app's own endpoint).
pub const VISUAL_BASE: &str = "https://www.bingapis.com/api/v7/images/visualsearch";

/// Skills the app's general identification mode requests
/// (`KnowledgeRequestData.KnowledgeRequest`, recognition type with
/// object detection and entity linking).
pub const IDENTIFY_SKILLS: &[&str] = &[
    "ObjectDetection",
    "EntityLinkingFace",
    "EntityLinkingDog",
    "EntityLinkingAnimal",
    "EntityLinkingPlant",
    "EntityLinkingLandmark",
    "EntityLinkingFood",
    "EntityLinkingBook",
];

/// Build the `knowledgeRequest` JSON document the app sends
/// (Gson form of `KnowledgeRequestData`).
pub fn knowledge_request_json(skills: &[&str]) -> String {
    serde_json::json!({
        "imageInfo": {"source": "BackCamera"},
        "knowledgeRequest": {
            "index": 1,
            "invokedSkills": skills,
            "invokedSkillsRequestData": {},
            "subscriptionId": "Bing.App.LiveCamera.Capture",
        },
    })
    .to_string()
}

/// Build the visual-search URL. Empty market/language fall back
/// to Japan / Japanese, like the rest of the provider.
pub fn visual_search_url(mkt: &str, setlang: &str) -> String {
    let mkt = if mkt.trim().is_empty() {
        "ja-JP"
    } else {
        mkt.trim()
    };
    let cc = match mkt.rsplit('-').next() {
        Some(cc) if cc.len() == 2 => cc.to_uppercase(),
        _ => "JP".to_string(),
    };
    let lang = if setlang.trim().is_empty() {
        "ja"
    } else {
        setlang.trim()
    };
    format!(
        "{VISUAL_BASE}?appid={VISUAL_APPID}&cc={cc}&setlang={}",
        urlencode(lang)
    )
}

/// Summary of one action inside a result tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualAction {
    /// Action type verbatim (e.g. `SimilarImages`).
    pub action_type: String,
    /// Number of items in the action's `data.value` list.
    pub item_count: usize,
    /// Titles / names of the first few items, when they carry any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sample_titles: Vec<String>,
    /// Destination URLs of the first few items (`hostPageUrl`,
    /// `contentUrl` or `webSearchUrl`, whichever the item carries).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sample_urls: Vec<String>,
    /// The action's `displayName`, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// One tag of the ImageKnowledge response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualTag {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub actions: Vec<VisualAction>,
}

/// Parse an ImageKnowledge response (offline-testable).
pub fn parse_visual_response(value: &Value) -> Vec<VisualTag> {
    let Some(tags) = value.get("tags").and_then(|t| t.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for tag in tags {
        let actions = tag
            .get("actions")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|action| {
                        let action_type = action
                            .get("actionType")
                            .and_then(|t| t.as_str())?
                            .to_string();
                        let items = action
                            .get("data")
                            .and_then(|d| d.get("value"))
                            .and_then(|v| v.as_array());
                        let (count, titles, urls) = match items {
                            Some(list) => {
                                let titles = list
                                    .iter()
                                    .filter_map(|i| {
                                        i.get("name")
                                            .or_else(|| i.get("title"))
                                            .and_then(|n| n.as_str())
                                            .map(clean_text)
                                            .filter(|s| !s.is_empty())
                                    })
                                    .take(5)
                                    .collect();
                                let urls = list
                                    .iter()
                                    .filter_map(|i| {
                                        ["hostPageUrl", "contentUrl", "webSearchUrl"]
                                            .iter()
                                            .find_map(|k| {
                                                i.get(*k)
                                                    .and_then(|u| u.as_str())
                                                    .filter(|s| !s.is_empty())
                                            })
                                            .map(str::to_string)
                                    })
                                    .take(5)
                                    .collect();
                                (list.len(), titles, urls)
                            }
                            None => (0, Vec::new(), Vec::new()),
                        };
                        Some(VisualAction {
                            action_type,
                            item_count: count,
                            sample_titles: titles,
                            sample_urls: urls,
                            display_name: action
                                .get("displayName")
                                .and_then(|d| d.as_str())
                                .map(clean_text)
                                .filter(|s| !s.is_empty()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(VisualTag {
            display_name: tag
                .get("displayName")
                .and_then(|d| d.as_str())
                .map(clean_text)
                .filter(|s| !s.is_empty()),
            actions,
        });
    }
    out
}

impl BingSearchClient {
    /// Run visual search over raw image bytes, in the app's
    /// general identification mode. The image is sent
    /// base64-encoded, exactly as the app sends it.
    pub async fn visual_search(
        &self,
        image: &[u8],
        mkt: &str,
        setlang: &str,
    ) -> Result<Vec<VisualTag>> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(image);
        let form = reqwest::multipart::Form::new()
            .text("knowledgeRequest", knowledge_request_json(IDENTIFY_SKILLS))
            .text("imageBase64", b64);
        let resp = self
            .http_post_multipart(&visual_search_url(mkt, setlang), form)
            .await?;
        let value: Value = serde_json::from_str(&resp)
            .map_err(|e| anyhow!("parse_error: visual search response is not JSON: {e}"))?;
        Ok(parse_visual_response(&value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn knowledge_request_matches_app_shape() {
        let v: Value = serde_json::from_str(&knowledge_request_json(IDENTIFY_SKILLS)).unwrap();
        assert_eq!(v["imageInfo"]["source"], "BackCamera");
        assert_eq!(
            v["knowledgeRequest"]["subscriptionId"],
            "Bing.App.LiveCamera.Capture"
        );
        assert_eq!(v["knowledgeRequest"]["index"], 1);
        assert_eq!(v["knowledgeRequest"]["invokedSkills"][0], "ObjectDetection");
        assert!(visual_search_url("", "").contains("&cc=JP&setlang=ja"));
    }

    #[test]
    fn parse_visual_tags_and_actions() {
        let value = json!({
            "tags": [{
                "displayName": "Cat",
                "actions": [
                    {"actionType": "VisualSearchResults", "data": {"value": [
                        {"name": "a cat", "hostPageUrl": "https://example.com/cat"}
                    ]}},
                    {"actionType": "PagesIncluding", "data": {"value": []}}
                ]
            }]
        });
        let tags = parse_visual_response(&value);
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].display_name.as_deref(), Some("Cat"));
        assert_eq!(tags[0].actions.len(), 2);
        assert_eq!(tags[0].actions[0].item_count, 1);
        assert_eq!(tags[0].actions[0].sample_titles, vec!["a cat"]);
        assert_eq!(
            tags[0].actions[0].sample_urls,
            vec!["https://example.com/cat"]
        );
        assert!(parse_visual_response(&json!({})).is_empty());
    }
}
