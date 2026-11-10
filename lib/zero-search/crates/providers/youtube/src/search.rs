//! `youtubei/v1/search`: query search and continuation-based paging.

use anyhow::Result;
use serde_json::{json, Value};

use crate::client::YoutubeClient;
use crate::errors::YoutubeError;
use crate::filters::SearchFilters;
use crate::parse::{
    channel_from_model, channel_from_renderer, playlist_from_renderer, short_from_shelf_item,
    video_from_renderer, SearchItem, VideoItem,
};

/// One page of search results.
pub struct SearchPage {
    /// Video-kind results only (backwards-compatible view).
    pub items: Vec<VideoItem>,
    /// Every result in the order YouTube returned it, tagged by
    /// kind: videos, Shorts, channels and playlists.
    pub results: Vec<SearchItem>,
    /// Estimated total result count reported by YouTube
    /// (`estimatedResults`), when present.
    pub estimated_results: Option<u64>,
    /// Token for the next page; pass to
    /// [`YoutubeClient::search_continuation`].
    pub next_continuation: Option<String>,
    pub visitor_data: Option<String>,
}

impl SearchPage {
    pub fn to_value(&self, query: &str) -> Value {
        json!({
            "provider": "youtube",
            "ok": true,
            "query": query,
            "count": self.items.len(),
            "estimated_results": self.estimated_results,
            "items": self.items.iter().map(|i| i.to_value()).collect::<Vec<_>>(),
            "results": self.results.iter().map(|i| i.to_value()).collect::<Vec<_>>(),
            "next_continuation": self.next_continuation,
        })
    }
}

/// Collect every search result in document order, tagged by kind.
/// The Android client's video results are `compactVideoRenderer`s
/// (`videoRenderer` handled too for layout variants); channels and
/// playlists use their own compact renderers; Shorts arrive as
/// Elements `shortsShelfModel` shelves whose `shelfItems` are
/// individual Shorts.
fn collect_search_items(resp: &Value) -> Vec<SearchItem> {
    fn walk(v: &Value, out: &mut Vec<SearchItem>) {
        match v {
            Value::Object(map) => {
                for key in [
                    "compactVideoRenderer",
                    "videoRenderer",
                    "compactMovieRenderer",
                ] {
                    if let Some(r) = map.get(key) {
                        if let Some(item) = video_from_renderer(r) {
                            out.push(SearchItem::Video(item));
                        }
                    }
                }
                if let Some(r) = map.get("compactChannelRenderer") {
                    if let Some(item) = channel_from_renderer(r) {
                        out.push(SearchItem::Channel(item));
                    }
                }
                if let Some(m) = map.get("compactChannelModel") {
                    if let Some(item) = channel_from_model(m) {
                        out.push(SearchItem::Channel(item));
                    }
                }
                if let Some(r) = map.get("compactPlaylistRenderer") {
                    if let Some(item) = playlist_from_renderer(r) {
                        out.push(SearchItem::Playlist(item));
                    }
                }
                if let Some(shelf) = map.get("shortsShelfModel") {
                    if let Some(items) = shelf.get("shelfItems").and_then(|i| i.as_array()) {
                        for entry in items {
                            if let Some(item) = short_from_shelf_item(entry) {
                                out.push(SearchItem::Short(item));
                            }
                        }
                    }
                }
                for child in map.values() {
                    walk(child, out);
                }
            }
            Value::Array(arr) => {
                for child in arr {
                    walk(child, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(resp, &mut out);
    out
}

/// The page-level continuation token of a search response. The
/// Android layout carries it as
/// `sectionListRenderer.continuations[].nextContinuationData.continuation`;
/// continuation responses and other layouts also use
/// `continuationItemRenderer`. The last one in document order wins.
fn collect_next_continuation(resp: &Value) -> Option<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(map) => {
                if let Some(tok) = map
                    .get("nextContinuationData")
                    .and_then(|n| n.get("continuation"))
                    .and_then(|t| t.as_str())
                {
                    out.push(tok.to_string());
                }
                if let Some(tok) = map
                    .get("continuationItemRenderer")
                    .and_then(|c| c.get("continuationEndpoint"))
                    .and_then(|c| c.get("continuationCommand"))
                    .and_then(|c| c.get("token"))
                    .and_then(|t| t.as_str())
                {
                    out.push(tok.to_string());
                }
                for child in map.values() {
                    walk(child, out);
                }
            }
            Value::Array(arr) => {
                for child in arr {
                    walk(child, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(resp, &mut out);
    out.into_iter().last()
}

/// Extract items + continuation from a search-shaped response
/// (`contents` for a fresh search, `continuationContents` /
/// `onResponseReceivedCommands` for a continuation response).
pub fn parse_search_response(resp: &Value) -> SearchPage {
    let results = collect_search_items(resp);
    let items: Vec<VideoItem> = results
        .iter()
        .filter_map(|r| match r {
            SearchItem::Video(v) => Some(v.clone()),
            _ => None,
        })
        .collect();
    let next_continuation = collect_next_continuation(resp);

    SearchPage {
        items,
        results,
        estimated_results: resp
            .get("estimatedResults")
            .and_then(|e| e.as_str())
            .and_then(|s| s.parse::<u64>().ok()),
        next_continuation,
        visitor_data: resp
            .pointer("/responseContext/visitorData")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    }
}

impl YoutubeClient {
    /// Search videos. `filters` carries sort order and the
    /// upload-date / type / duration filters.
    pub async fn search_videos(
        &self,
        query: &str,
        filters: &SearchFilters,
    ) -> Result<SearchPage, YoutubeError> {
        let mut body = json!({ "query": query });
        if let Some(params) = filters.encode_params() {
            body["params"] = json!(params);
        }
        let resp = self.post("search", body).await?;
        Ok(parse_search_response(&resp))
    }

    /// Fetch the next page of a previous search using its continuation
    /// token.
    pub async fn search_continuation(
        &self,
        continuation: &str,
    ) -> Result<SearchPage, YoutubeError> {
        let resp = self
            .post("search", json!({ "continuation": continuation }))
            .await?;
        Ok(parse_search_response(&resp))
    }
}
