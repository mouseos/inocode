//! `youtubei/v1/next`: watch-next data — related videos and the
//! entrance to the comments section (its continuation token), plus
//! comment pages fetched by feeding that token back into `next`.

use serde_json::{json, Value};

use crate::client::YoutubeClient;
use crate::errors::{YoutubeError, KIND_NOT_FOUND};
use crate::parse::{collect_renderers, normalize_worded_count, text_of, thumbnail_url, VideoItem};

/// Watch-next data for one video.
pub struct WatchNext {
    pub video_id: String,
    pub related: Vec<VideoItem>,
    /// Continuation token that opens the comments section; feed it to
    /// [`YoutubeClient::get_comments`]. None when comments are off.
    pub comments_continuation: Option<String>,
    /// Continuation token for the next page of related videos; feed
    /// it to [`YoutubeClient::get_related_continuation`].
    pub related_continuation: Option<String>,
    /// Exact like count of the watched video, from the response's
    /// `likeCountEntity` (the count a guest sees, i.e. the
    /// "indifferent" figure). The ANDROID player response has no
    /// microformat, so this is where the app itself gets the number.
    pub like_count: Option<u64>,
    /// Compact like-count text ("86K"), when present.
    pub like_count_text: Option<String>,
}

impl WatchNext {
    pub fn to_value(&self) -> Value {
        json!({
            "provider": "youtube",
            "ok": true,
            "video_id": self.video_id,
            "related": self.related.iter().map(|i| i.to_value()).collect::<Vec<_>>(),
            "related_count": self.related.len(),
            "related_continuation": self.related_continuation,
            "comments_continuation": self.comments_continuation,
            "like_count": self.like_count,
            "like_count_text": self.like_count_text,
        })
    }
}

/// One page of related videos fetched via a continuation token.
pub struct RelatedPage {
    pub items: Vec<VideoItem>,
    pub next_continuation: Option<String>,
}

impl RelatedPage {
    pub fn to_value(&self) -> Value {
        json!({
            "provider": "youtube",
            "ok": true,
            "count": self.items.len(),
            "items": self.items.iter().map(|i| i.to_value()).collect::<Vec<_>>(),
            "next_continuation": self.next_continuation,
        })
    }
}

/// One comment (top-level thread entry).
pub struct CommentItem {
    pub comment_id: String,
    pub author: String,
    pub author_channel_id: Option<String>,
    pub text: String,
    pub like_count_text: Option<String>,
    pub published_time: Option<String>,
    pub reply_count: Option<u64>,
    pub author_thumbnail: Option<String>,
    pub is_pinned: bool,
}

impl CommentItem {
    pub fn to_value(&self) -> Value {
        json!({
            "comment_id": self.comment_id,
            "author": self.author,
            "author_channel_id": self.author_channel_id,
            "text": self.text,
            "like_count_text": self.like_count_text,
            "published_time": self.published_time,
            "reply_count": self.reply_count,
            "author_thumbnail": self.author_thumbnail,
            "is_pinned": self.is_pinned,
        })
    }
}

/// One page of comments.
pub struct CommentsPage {
    pub comments: Vec<CommentItem>,
    pub next_continuation: Option<String>,
    /// Header count text, e.g. "1.2K Comments", when present.
    pub header_count_text: Option<String>,
}

impl CommentsPage {
    pub fn to_value(&self) -> Value {
        json!({
            "provider": "youtube",
            "ok": true,
            "count": self.comments.len(),
            "comments": self.comments.iter().map(|c| c.to_value()).collect::<Vec<_>>(),
            "next_continuation": self.next_continuation,
            "header_count_text": self.header_count_text,
        })
    }
}

fn parse_compact_video(r: &Value) -> Option<VideoItem> {
    // compactVideoRenderer has the same field names as videoRenderer
    // for the subset we expose.
    crate::parse::video_from_renderer(r)
}

/// Extract a VideoItem from an Elements-framework
/// `videoWithContextModel` — the Android client's current related-
/// video representation in watch-next responses.
fn video_from_lockup_model(model: &Value) -> Option<VideoItem> {
    let data = model.get("videoWithContextData")?;
    let vd = data.get("videoData")?;
    // Video id: the drag-and-drop URL is the canonical watch URL.
    let video_id = vd
        .get("dragAndDropUrl")
        .and_then(|u| u.as_str())
        .and_then(|u| u.split("v=").nth(1))
        .map(|s| s.split('&').next().unwrap_or(s).to_string())
        .filter(|s| !s.is_empty())?;
    let lockup = vd
        .pointer("/lockupMetadata/lockupMetadataViewModel")
        .cloned()
        .unwrap_or(Value::Null);
    let title = lockup
        .pointer("/title/content")
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    // Channel / views / age ride in one metadata row's
    // accessibilityLabel: "<channel>, <views> views, <age>".
    let mut channel_name = String::new();
    let mut view_count_text: Option<String> = None;
    let mut published_time: Option<String> = None;
    if let Some(rows) = lockup
        .pointer("/metadata/contentMetadataViewModel/metadataRows")
        .and_then(|r| r.as_array())
    {
        for row in rows {
            if let Some(label) = row
                .pointer("/metadataParts/0/accessibilityLabel")
                .and_then(|l| l.as_str())
                .or_else(|| {
                    row.pointer("/metadataParts/0/text/accessibilityLabel")
                        .and_then(|l| l.as_str())
                })
            {
                let parts: Vec<&str> = label.split(", ").collect();
                if parts.len() >= 3 {
                    channel_name = parts[0].to_string();
                    view_count_text = Some(parts[1].to_string());
                    published_time = Some(parts[2..].join(", "));
                } else if parts.len() == 1 {
                    channel_name = parts[0].to_string();
                }
            }
        }
    }
    let duration_text = vd
        .pointer("/thumbnail/timestampText")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string());
    let thumbnail = vd
        .pointer("/thumbnail/image/sources")
        .and_then(|s| s.as_array())
        .and_then(|a| a.last())
        .and_then(|s| s.get("url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string());
    let view_count = view_count_text
        .as_deref()
        .and_then(|t| crate::parse::parse_count_text(&normalize_worded_count(t)));
    Some(VideoItem {
        url: format!("https://www.youtube.com/watch?v={video_id}"),
        video_id,
        title,
        channel_name,
        channel_id: None,
        view_count,
        view_count_text,
        duration_secs: duration_text
            .as_deref()
            .and_then(crate::parse::parse_duration_secs),
        duration_text,
        published_time,
        thumbnail,
        is_live: false,
    })
}

/// Find the comments-section continuation token inside a watch-next
/// response. The token rides in the engagement panel whose content is
/// the comment section; fall back to any continuation located under a
/// subtree mentioning "comment-item-section".
fn extract_comments_continuation(resp: &Value) -> Option<String> {
    fn walk(v: &Value, in_comment_panel: bool) -> Option<String> {
        match v {
            Value::Object(map) => {
                let this_panel = in_comment_panel
                    || map
                        .get("panelIdentifier")
                        .and_then(|p| p.as_str())
                        .map(|p| p.contains("comment"))
                        .unwrap_or(false)
                    || map
                        .get("targetId")
                        .and_then(|p| p.as_str())
                        .map(|p| p.contains("comment"))
                        .unwrap_or(false);
                if this_panel {
                    if let Some(tok) = map
                        .get("continuationItemRenderer")
                        .and_then(|c| c.get("continuationEndpoint"))
                        .and_then(|c| c.get("continuationCommand"))
                        .and_then(|c| c.get("token"))
                        .and_then(|t| t.as_str())
                    {
                        return Some(tok.to_string());
                    }
                    if let Some(tok) = map
                        .get("continuationCommand")
                        .and_then(|c| c.get("token"))
                        .and_then(|t| t.as_str())
                    {
                        return Some(tok.to_string());
                    }
                    // The Android comment panel carries only a
                    // reload continuation for its section list.
                    for key in ["reloadContinuationData", "nextContinuationData"] {
                        if let Some(tok) = map
                            .get(key)
                            .and_then(|c| c.get("continuation"))
                            .and_then(|t| t.as_str())
                        {
                            return Some(tok.to_string());
                        }
                    }
                }
                for child in map.values() {
                    if let Some(t) = walk(child, this_panel) {
                        return Some(t);
                    }
                }
                None
            }
            Value::Array(arr) => arr.iter().find_map(|c| walk(c, in_comment_panel)),
            _ => None,
        }
    }
    walk(resp, false)
}

/// Extract the watched video's like count from a watch-next
/// response. The count rides in a `likeCountEntity` — both inside
/// the action bar's segmented like/dislike model and as an entity
/// mutation in `frameworkUpdates`. A guest has not liked the video,
/// so the displayed count is `likeCountIfIndifferentNumber`.
fn extract_like_count(resp: &Value) -> (Option<u64>, Option<String>) {
    let mut entities = Vec::new();
    collect_renderers(resp, "likeCountEntity", &mut entities);
    for e in &entities {
        let count = e
            .get("likeCountIfIndifferentNumber")
            .and_then(|n| n.as_str())
            .and_then(|s| s.parse::<u64>().ok());
        if count.is_some() {
            let text = e
                .pointer("/likeCountIfIndifferent/content")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string());
            return (count, text);
        }
    }
    (None, None)
}

/// Extract a related-videos continuation: the `nextContinuationData`
/// in a section's `continuations` array — in the watch-next response
/// at the results level (the one labelled "Show more"), in a
/// continuation response on the section continuation itself.
fn extract_related_continuation(resp: &Value) -> Option<String> {
    for base in [
        "/contents/singleColumnWatchNextResults/results/results/continuations",
        "/continuationContents/sectionListContinuation/continuations",
        "/continuationContents/itemSectionContinuation/continuations",
    ] {
        if let Some(arr) = resp.pointer(base).and_then(|c| c.as_array()) {
            for c in arr {
                if let Some(tok) = c
                    .pointer("/nextContinuationData/continuation")
                    .and_then(|t| t.as_str())
                {
                    return Some(tok.to_string());
                }
            }
        }
    }
    None
}

/// Parse a watch-next response.
pub fn parse_next_response(resp: &Value, video_id: &str) -> WatchNext {
    let mut compacts = Vec::new();
    collect_renderers(resp, "compactVideoRenderer", &mut compacts);
    let mut related: Vec<VideoItem> = compacts.iter().filter_map(parse_compact_video).collect();
    // The Android client's related list is Elements-framework
    // lockups (videoWithContextModel), not classic renderers.
    let mut models = Vec::new();
    collect_renderers(resp, "videoWithContextModel", &mut models);
    related.extend(models.iter().filter_map(video_from_lockup_model));
    // De-duplicate by video id, keeping first occurrence order.
    let mut seen = std::collections::HashSet::new();
    related.retain(|i| seen.insert(i.video_id.clone()));
    let (like_count, like_count_text) = extract_like_count(resp);
    WatchNext {
        video_id: video_id.to_string(),
        related,
        comments_continuation: extract_comments_continuation(resp),
        related_continuation: extract_related_continuation(resp),
        like_count,
        like_count_text,
    }
}

/// Parse a related-videos continuation response (`next` fed with a
/// related token). Continuation responses append items under
/// `continuationContents` or `onResponseReceivedCommands`, and carry
/// the following page's token in a trailing
/// `continuationItemRenderer` (or a further `nextContinuationData`).
pub fn parse_related_continuation_response(resp: &Value) -> RelatedPage {
    let mut compacts = Vec::new();
    collect_renderers(resp, "compactVideoRenderer", &mut compacts);
    let mut items: Vec<VideoItem> = compacts.iter().filter_map(parse_compact_video).collect();
    let mut models = Vec::new();
    collect_renderers(resp, "videoWithContextModel", &mut models);
    items.extend(models.iter().filter_map(video_from_lockup_model));
    let mut seen = std::collections::HashSet::new();
    items.retain(|i| seen.insert(i.video_id.clone()));

    // The next token: prefer a continuationItemRenderer token that is
    // not the comments one (this response is the related section's),
    // else any remaining nextContinuationData.
    let mut next_continuation = None;
    let mut conts = Vec::new();
    collect_renderers(resp, "continuationItemRenderer", &mut conts);
    for c in conts.iter().rev() {
        if let Some(tok) = c
            .pointer("/continuationEndpoint/continuationCommand/token")
            .and_then(|t| t.as_str())
        {
            next_continuation = Some(tok.to_string());
            break;
        }
    }
    if next_continuation.is_none() {
        next_continuation = extract_related_continuation(resp);
    }
    RelatedPage {
        items,
        next_continuation,
    }
}

/// Decode an Elements entity key (base64, possibly URL-escaped).
/// Entity keys embed the comment id as a plain string inside the
/// decoded bytes, which is how models and payloads are linked.
fn decode_entity_key(key: &str) -> Vec<u8> {
    use base64::Engine;
    let cleaned: String = key
        .replace("%3D", "=")
        .replace("%3d", "=")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(&cleaned)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(&cleaned))
        .unwrap_or_default()
}

/// Which of the known comment ids is embedded in this entity key?
fn key_comment_id(key: &str, known_ids: &[String]) -> Option<String> {
    let bytes = decode_entity_key(key);
    if bytes.is_empty() {
        return None;
    }
    let hay = String::from_utf8_lossy(&bytes);
    known_ids
        .iter()
        .find(|id| hay.contains(id.as_str()))
        .cloned()
}

/// Parse one `commentEntityPayload` mutation into a CommentItem.
fn comment_from_entity(p: &Value) -> Option<CommentItem> {
    let props = p.get("properties")?;
    let comment_id = props.get("commentId")?.as_str()?.to_string();
    let author = p.get("author").cloned().unwrap_or(Value::Null);
    let toolbar = p.get("toolbar").cloned().unwrap_or(Value::Null);
    Some(CommentItem {
        comment_id,
        author: author
            .get("displayName")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        author_channel_id: author
            .get("channelId")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        text: props
            .pointer("/content/content")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        like_count_text: toolbar
            .get("likeCountNotliked")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        published_time: props
            .get("publishedTime")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        reply_count: toolbar
            .get("replyCount")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse::<u64>().ok()),
        author_thumbnail: author
            .get("avatarThumbnailUrl")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        is_pinned: false,
    })
}

/// Collect, in document order, the comment surface keys of all
/// comment thread models (including the pinned-thread model flag).
fn collect_thread_surface_keys(resp: &Value) -> Vec<(String, bool)> {
    fn walk(v: &Value, out: &mut Vec<(String, bool)>) {
        match v {
            Value::Object(map) => {
                if let Some(m) = map.get("commentThreadModel") {
                    if let Some(k) = m
                        .pointer("/surfaceKeys/commentSurfaceKey")
                        .and_then(|x| x.as_str())
                    {
                        out.push((k.to_string(), false));
                    }
                }
                if let Some(m) = map.get("pinnedCommentThreadModel") {
                    if let Some(k) = m.get("pinnedKey").and_then(|x| x.as_str()) {
                        out.push((k.to_string(), true));
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

/// Parse a comments continuation response (`next` with a comments
/// token). The Android client delivers comments as Elements
/// entities: `frameworkUpdates.entityBatchUpdate.mutations[]`
/// carries one `commentEntityPayload` per comment, ordered/linked
/// by the thread models' surface keys. Classic
/// `commentThreadRenderer` layouts are handled as a fallback.
pub fn parse_comments_response(resp: &Value) -> CommentsPage {
    // 1) Entity payloads (current Android layout).
    let mut by_id: std::collections::HashMap<String, CommentItem> =
        std::collections::HashMap::new();
    let mut mutation_order: Vec<String> = Vec::new();
    if let Some(muts) = resp
        .pointer("/frameworkUpdates/entityBatchUpdate/mutations")
        .and_then(|m| m.as_array())
    {
        for m in muts {
            if let Some(p) = m.pointer("/payload/commentEntityPayload") {
                if let Some(item) = comment_from_entity(p) {
                    mutation_order.push(item.comment_id.clone());
                    by_id.insert(item.comment_id.clone(), item);
                }
            }
        }
    }
    let known_ids: Vec<String> = mutation_order.clone();
    let mut comments: Vec<CommentItem> = Vec::new();
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (surface_key, is_pinned_thread) in collect_thread_surface_keys(resp) {
        if let Some(id) = key_comment_id(&surface_key, &known_ids) {
            if let Some(mut item) = by_id.remove(&id) {
                if is_pinned_thread {
                    item.is_pinned = true;
                }
                used.insert(id);
                comments.push(item);
            }
        }
    }
    for id in &mutation_order {
        if !used.contains(id) {
            if let Some(item) = by_id.remove(id) {
                comments.push(item);
            }
        }
    }
    // 2) Classic renderer layout fallback.
    if comments.is_empty() {
        let mut threads = Vec::new();
        collect_renderers(resp, "commentThreadRenderer", &mut threads);
        for t in &threads {
            let Some(c) = t
                .get("comment")
                .and_then(|c| c.get("commentRenderer"))
                .or_else(|| t.get("commentRenderer"))
            else {
                continue;
            };
            let comment_id = c
                .get("commentId")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            if comment_id.is_empty() {
                continue;
            }
            comments.push(CommentItem {
                comment_id,
                author: c.get("authorText").map(text_of).unwrap_or_default(),
                author_channel_id: c
                    .pointer("/authorEndpoint/browseEndpoint/browseId")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string()),
                text: c.get("contentText").map(text_of).unwrap_or_default(),
                like_count_text: c.get("voteCount").map(text_of).filter(|s| !s.is_empty()),
                published_time: c
                    .get("publishedTimeText")
                    .map(text_of)
                    .filter(|s| !s.is_empty()),
                reply_count: c.get("replyCount").and_then(|x| x.as_u64()),
                author_thumbnail: thumbnail_url(c),
                is_pinned: c.get("pinnedCommentBadge").is_some(),
            });
        }
    }
    let mut conts = Vec::new();
    collect_renderers(resp, "continuationItemRenderer", &mut conts);
    let mut next_continuation = conts.iter().rev().find_map(|c| {
        c.pointer("/continuationEndpoint/continuationCommand/token")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
    });
    if next_continuation.is_none() {
        let mut datas = Vec::new();
        collect_renderers(resp, "nextContinuationData", &mut datas);
        next_continuation = datas.iter().rev().find_map(|c| {
            c.get("continuation")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        });
    }
    CommentsPage {
        comments,
        next_continuation,
        header_count_text: None,
    }
}

impl YoutubeClient {
    /// Fetch watch-next data: related videos and the comments entrance.
    pub async fn get_watch_next(&self, video_id: &str) -> Result<WatchNext, YoutubeError> {
        let resp = self.post("next", json!({ "videoId": video_id })).await?;
        let wn = parse_next_response(&resp, video_id);
        if wn.related.is_empty() && wn.comments_continuation.is_none() {
            return Err(YoutubeError::new(
                KIND_NOT_FOUND,
                format!("no watch-next data for video {video_id}"),
            ));
        }
        Ok(wn)
    }

    /// Fetch the next page of related videos using a continuation
    /// token from [`WatchNext::related_continuation`] or from a
    /// previous related page.
    pub async fn get_related_continuation(
        &self,
        continuation: &str,
    ) -> Result<RelatedPage, YoutubeError> {
        let resp = self
            .post("next", json!({ "continuation": continuation }))
            .await?;
        Ok(parse_related_continuation_response(&resp))
    }

    /// Fetch one page of comments using a continuation token obtained
    /// from [`YoutubeClient::get_watch_next`] (or from a previous
    /// comments page).
    pub async fn get_comments(&self, continuation: &str) -> Result<CommentsPage, YoutubeError> {
        let resp = self
            .post("next", json!({ "continuation": continuation }))
            .await?;
        Ok(parse_comments_response(&resp))
    }
}
