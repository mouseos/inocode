//! Walking helpers for InnerTube renderer trees and the shared
//! extraction of video items from `videoRenderer` (search) and
//! `compactVideoRenderer` (watch-next related) objects.

use serde_json::{json, Value};

/// Concatenate a `simpleText` or `runs[]` text object into a String.
pub fn text_of(v: &Value) -> String {
    if let Some(s) = v.get("simpleText").and_then(|t| t.as_str()) {
        return s.to_string();
    }
    if let Some(runs) = v.get("runs").and_then(|r| r.as_array()) {
        let mut out = String::new();
        for run in runs {
            if let Some(t) = run.get("text").and_then(|t| t.as_str()) {
                out.push_str(t);
            }
        }
        return out;
    }
    String::new()
}

/// First URL of a thumbnail container (`thumbnail.thumbnails[]`),
/// preferring the largest (last) entry.
pub fn thumbnail_url(v: &Value) -> Option<String> {
    let thumbs = v
        .get("thumbnail")
        .and_then(|t| t.get("thumbnails"))
        .and_then(|t| t.as_array())?;
    thumbs
        .last()
        .and_then(|t| t.get("url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
}

/// Parse "12:34" / "1:02:03" into seconds.
pub fn parse_duration_secs(text: &str) -> Option<u64> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut secs: u64 = 0;
    for p in parts {
        let n: u64 = p.parse().ok()?;
        secs = secs * 60 + n;
    }
    Some(secs)
}

/// Convert worded counts ("525 thousand") to the compact form the
/// shared parser understands ("525K"). Elements-framework models use
/// the worded form in their accessibility labels.
pub fn normalize_worded_count(text: &str) -> String {
    text.replace(" thousand", "K")
        .replace(" million", "M")
        .replace(" billion", "B")
}

/// Parse a view-count text like "1.2M views" / "345K views" / "No views"
/// into an approximate count. Returns None when unparsable.
pub fn parse_count_text(text: &str) -> Option<u64> {
    let t = text.trim().to_lowercase();
    if t.starts_with("no views") {
        return Some(0);
    }
    let num_part: String = t
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
        .collect();
    if num_part.is_empty() {
        return None;
    }
    let mult = if t[num_part.len()..].starts_with('k') {
        1_000f64
    } else if t[num_part.len()..].starts_with('m') {
        1_000_000f64
    } else if t[num_part.len()..].starts_with('b') {
        1_000_000_000f64
    } else {
        1f64
    };
    let cleaned: String = num_part.chars().filter(|c| *c != ',').collect();
    cleaned.parse::<f64>().ok().map(|n| (n * mult) as u64)
}

/// A video item as returned by search and by watch-next related lists.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VideoItem {
    pub video_id: String,
    pub title: String,
    pub url: String,
    pub channel_name: String,
    pub channel_id: Option<String>,
    pub view_count_text: Option<String>,
    pub view_count: Option<u64>,
    pub duration_text: Option<String>,
    pub duration_secs: Option<u64>,
    pub published_time: Option<String>,
    pub thumbnail: Option<String>,
    pub is_live: bool,
}

impl VideoItem {
    pub fn to_value(&self) -> Value {
        json!({
            "video_id": self.video_id,
            "title": self.title,
            "url": self.url,
            "channel_name": self.channel_name,
            "channel_id": self.channel_id,
            "view_count_text": self.view_count_text,
            "view_count": self.view_count,
            "duration_text": self.duration_text,
            "duration_secs": self.duration_secs,
            "published_time": self.published_time,
            "thumbnail": self.thumbnail,
            "is_live": self.is_live,
        })
    }
}

/// Extract a VideoItem from a `videoRenderer` object.
pub fn video_from_renderer(r: &Value) -> Option<VideoItem> {
    let video_id = r.get("videoId")?.as_str()?.to_string();
    let title = text_of(r.get("title")?);
    let byline = r.get("longBylineText").or_else(|| r.get("ownerText"));
    let (channel_name, channel_id) = match byline {
        Some(b) => {
            let name = text_of(b);
            let cid = b
                .get("runs")
                .and_then(|x| x.as_array())
                .and_then(|a| a.first())
                .and_then(|run| run.get("navigationEndpoint"))
                .and_then(|ne| ne.get("browseEndpoint"))
                .and_then(|be| be.get("browseId"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            (name, cid)
        }
        None => (String::new(), None),
    };
    let view_count_text = r
        .get("viewCountText")
        .map(text_of)
        .filter(|s| !s.is_empty());
    let duration_text = r.get("lengthText").map(text_of).filter(|s| !s.is_empty());
    let is_live = duration_text.is_none()
        && r.get("badges")
            .and_then(|b| b.as_array())
            .map(|badges| {
                badges.iter().any(|b| {
                    b.pointer("/metadataBadgeRenderer/label")
                        .and_then(|l| l.as_str())
                        == Some("LIVE")
                })
            })
            .unwrap_or(false);
    Some(VideoItem {
        url: format!("https://www.youtube.com/watch?v={video_id}"),
        video_id,
        title,
        channel_name,
        channel_id,
        view_count: view_count_text.as_deref().and_then(parse_count_text),
        view_count_text,
        duration_secs: duration_text.as_deref().and_then(parse_duration_secs),
        duration_text,
        published_time: r
            .get("publishedTimeText")
            .map(text_of)
            .filter(|s| !s.is_empty()),
        thumbnail: thumbnail_url(r),
        is_live,
    })
}

/// A channel item as returned by search (`compactChannelRenderer`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChannelItem {
    pub channel_id: String,
    pub title: String,
    pub url: String,
    /// Canonical handle URL path ("/@Fireship"), when provided.
    pub handle: Option<String>,
    pub subscriber_count_text: Option<String>,
    pub video_count_text: Option<String>,
    pub description_snippet: Option<String>,
    pub thumbnail: Option<String>,
}

impl ChannelItem {
    pub fn to_value(&self) -> Value {
        json!({
            "kind": "channel",
            "channel_id": self.channel_id,
            "title": self.title,
            "url": self.url,
            "handle": self.handle,
            "subscriber_count_text": self.subscriber_count_text,
            "video_count_text": self.video_count_text,
            "description_snippet": self.description_snippet,
            "thumbnail": self.thumbnail,
        })
    }
}

/// Extract a ChannelItem from a `compactChannelRenderer` object.
pub fn channel_from_renderer(r: &Value) -> Option<ChannelItem> {
    let channel_id = r.get("channelId")?.as_str()?.to_string();
    let title = text_of(r.get("title")?);
    let handle = r
        .pointer("/navigationEndpoint/browseEndpoint/canonicalBaseUrl")
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            r.get("subscriberCountText")
                .map(text_of)
                .filter(|s| s.starts_with('@'))
        });
    let url = match &handle {
        Some(h) if h.starts_with('/') => format!("https://www.youtube.com{h}"),
        _ => format!("https://www.youtube.com/channel/{channel_id}"),
    };
    // On some layouts the handle itself rides in subscriberCountText's
    // place; only keep it as a count when it is not the handle.
    let subscriber_count_text = r
        .get("subscriberCountText")
        .map(text_of)
        .filter(|s| !s.is_empty() && !s.starts_with('@'));
    Some(ChannelItem {
        channel_id,
        title,
        url,
        handle,
        subscriber_count_text,
        video_count_text: r
            .get("videoCountText")
            .map(text_of)
            .filter(|s| !s.is_empty()),
        description_snippet: r
            .get("descriptionSnippet")
            .map(text_of)
            .filter(|s| !s.is_empty()),
        thumbnail: thumbnail_url(r),
    })
}

/// Extract a ChannelItem from an Elements-framework
/// `compactChannelModel` — the Android client's current channel
/// representation in search responses (a plain-string title, a
/// handle, and count strings; the channel id rides in the avatar /
/// tap browse endpoints).
pub fn channel_from_model(model: &Value) -> Option<ChannelItem> {
    let data = model.get("compactChannelData")?;
    let browse = data
        .pointer("/onTap/innertubeCommand/browseEndpoint")
        .or_else(|| data.pointer("/avatar/endpoint/innertubeCommand/browseEndpoint"))
        .cloned()
        .unwrap_or(Value::Null);
    let channel_id = browse
        .get("browseId")
        .and_then(|b| b.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            data.pointer("/subscribeButtonViewModel/subscribeButtonViewModel/channelId")
                .and_then(|c| c.as_str())
                .map(|s| s.to_string())
        })?;
    let title = data.get("title")?.as_str()?.to_string();
    let handle = data
        .get("handle")
        .and_then(|h| h.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            browse
                .get("canonicalBaseUrl")
                .and_then(|u| u.as_str())
                .and_then(|u| u.strip_prefix('/'))
                .map(|s| s.to_string())
        });
    let url = match browse.get("canonicalBaseUrl").and_then(|u| u.as_str()) {
        Some(path) if path.starts_with('/') => format!("https://www.youtube.com{path}"),
        _ => format!("https://www.youtube.com/channel/{channel_id}"),
    };
    let thumbnail = data
        .pointer("/avatar/image/sources")
        .and_then(|s| s.as_array())
        .and_then(|a| a.last())
        .and_then(|s| s.get("url"))
        .and_then(|u| u.as_str())
        .map(|s| {
            if let Some(rest) = s.strip_prefix("//") {
                format!("https://{rest}")
            } else {
                s.to_string()
            }
        });
    Some(ChannelItem {
        channel_id,
        title,
        url,
        handle,
        subscriber_count_text: data
            .get("subscriberCount")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
        video_count_text: data
            .get("videoCount")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
        description_snippet: None,
        thumbnail,
    })
}

/// A playlist item as returned by search (`compactPlaylistRenderer`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PlaylistItem {
    pub playlist_id: String,
    pub title: String,
    pub url: String,
    pub channel_name: String,
    pub channel_id: Option<String>,
    pub video_count: Option<u64>,
    pub video_count_text: Option<String>,
    pub thumbnail: Option<String>,
}

impl PlaylistItem {
    pub fn to_value(&self) -> Value {
        json!({
            "kind": "playlist",
            "playlist_id": self.playlist_id,
            "title": self.title,
            "url": self.url,
            "channel_name": self.channel_name,
            "channel_id": self.channel_id,
            "video_count": self.video_count,
            "video_count_text": self.video_count_text,
            "thumbnail": self.thumbnail,
        })
    }
}

/// Extract a PlaylistItem from a `compactPlaylistRenderer` object.
pub fn playlist_from_renderer(r: &Value) -> Option<PlaylistItem> {
    let playlist_id = r.get("playlistId")?.as_str()?.to_string();
    let title = text_of(r.get("title")?);
    let byline = r.get("shortBylineText");
    let (channel_name, channel_id) = match byline {
        Some(b) => {
            let name = text_of(b);
            let cid = b
                .get("runs")
                .and_then(|x| x.as_array())
                .and_then(|a| a.first())
                .and_then(|run| run.get("navigationEndpoint"))
                .and_then(|ne| ne.get("browseEndpoint"))
                .and_then(|be| be.get("browseId"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            (name, cid)
        }
        None => (String::new(), None),
    };
    let video_count = r
        .get("videoCountShortText")
        .map(text_of)
        .and_then(|s| s.trim().parse::<u64>().ok())
        .or_else(|| {
            r.get("videoCountText")
                .map(text_of)
                .and_then(|s| parse_count_text(&s.replace(" videos", " views")))
        });
    Some(PlaylistItem {
        url: format!("https://www.youtube.com/playlist?list={playlist_id}"),
        playlist_id,
        title,
        channel_name,
        channel_id,
        video_count,
        video_count_text: r
            .get("videoCountText")
            .map(text_of)
            .filter(|s| !s.is_empty()),
        thumbnail: thumbnail_url(r),
    })
}

/// Extract a Shorts item from one entry of a `shortsShelfModel`'s
/// `shelfItems`. Shorts items are Elements-framework models: the id
/// rides in the reel watch endpoint (and in the `entityId` suffix),
/// the title and view count ride in the accessibility text
/// ("<title>, 1.1 million views - play Short").
pub fn short_from_shelf_item(item: &Value) -> Option<VideoItem> {
    let endpoint = item
        .pointer("/onTap/innertubeCommand/reelWatchEndpoint")
        .cloned()
        .unwrap_or(Value::Null);
    let video_id = endpoint
        .get("videoId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            item.get("entityId")
                .and_then(|e| e.as_str())
                .and_then(|e| e.rsplit('-').next())
                .map(|s| s.to_string())
        })
        .filter(|s| !s.is_empty())?;
    let a11y = item
        .get("accessibilityText")
        .and_then(|a| a.as_str())
        .unwrap_or_default();
    // "<title>, <views> views - play Short" — the title itself may
    // contain commas, so split from the right on the views segment.
    let (title, view_count_text) = match a11y.rsplit_once(", ") {
        Some((head, tail)) if tail.contains("views") || tail.contains("view") => {
            let views = tail.trim_end_matches(" - play Short").to_string();
            (head.to_string(), Some(views))
        }
        _ => (a11y.trim_end_matches(" - play Short").to_string(), None),
    };
    let view_count = view_count_text
        .as_deref()
        .and_then(|t| parse_count_text(&normalize_worded_count(t)));
    let thumbnail = item
        .pointer("/thumbnailViewModel/image/sources")
        .and_then(|s| s.as_array())
        .and_then(|a| a.last())
        .and_then(|s| s.get("url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            endpoint
                .pointer("/thumbnail/thumbnails")
                .and_then(|t| t.as_array())
                .and_then(|a| a.last())
                .and_then(|t| t.get("url"))
                .and_then(|u| u.as_str())
                .map(|s| s.to_string())
        });
    Some(VideoItem {
        url: format!("https://www.youtube.com/shorts/{video_id}"),
        video_id,
        title,
        channel_name: String::new(),
        channel_id: None,
        view_count,
        view_count_text,
        duration_secs: None,
        duration_text: None,
        published_time: None,
        thumbnail,
        is_live: false,
    })
}

/// One search result of any kind, in the order YouTube returned it.
/// The canonical JSON shape is [`SearchItem::to_value`], which tags
/// every entry with a `kind` field.
#[derive(Debug, Clone)]
pub enum SearchItem {
    Video(VideoItem),
    Short(VideoItem),
    Channel(ChannelItem),
    Playlist(PlaylistItem),
}

impl SearchItem {
    pub fn to_value(&self) -> Value {
        match self {
            SearchItem::Video(v) => {
                let mut val = v.to_value();
                val["kind"] = json!("video");
                val
            }
            SearchItem::Short(v) => {
                let mut val = v.to_value();
                val["kind"] = json!("short");
                val
            }
            SearchItem::Channel(c) => c.to_value(),
            SearchItem::Playlist(p) => p.to_value(),
        }
    }
}

/// Depth-first search for the first string value stored under `key`
/// anywhere in the tree (used for continuation tokens that move between
/// renderer layouts).
pub fn find_string<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    match v {
        Value::Object(map) => {
            if let Some(s) = map.get(key).and_then(|x| x.as_str()) {
                return Some(s);
            }
            for child in map.values() {
                if let Some(s) = find_string(child, key) {
                    return Some(s);
                }
            }
            None
        }
        Value::Array(arr) => arr.iter().find_map(|child| find_string(child, key)),
        _ => None,
    }
}

/// Collect every object in the tree that has the given renderer key,
/// in document order.
pub fn collect_renderers(v: &Value, renderer_key: &str, out: &mut Vec<Value>) {
    match v {
        Value::Object(map) => {
            if let Some(inner) = map.get(renderer_key) {
                out.push(inner.clone());
            }
            for child in map.values() {
                collect_renderers(child, renderer_key, out);
            }
        }
        Value::Array(arr) => {
            for child in arr {
                collect_renderers(child, renderer_key, out);
            }
        }
        _ => {}
    }
}
