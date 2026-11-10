//! `youtubei/v1/updated_metadata`: the watch page's metadata
//! refresh — the response the app uses to fill the engagement
//! entities (view count, like count, upload time) of the video
//! being watched.
//!
//! The data rides as entity payloads in
//! `frameworkUpdates.entityBatchUpdate.mutations`: a
//! `viewCountEntity` (with the exact count as a number string), a
//! `likeCountEntity`, and an `uploadTimeEntity`. Note what the
//! upload-time entity contains: relative expressions only
//! ("5y ago" / "5 years ago"). No response in the Android app's own
//! flow carries an absolute calendar date — the app itself works
//! with the relative form.

use serde_json::{json, Value};

use crate::client::YoutubeClient;
use crate::errors::YoutubeError;
use crate::parse::collect_renderers;

/// Engagement metadata of one video, from `updated_metadata`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UpdatedMetadata {
    pub video_id: String,
    /// Exact view count (`viewCountEntity.viewCountNumber`).
    pub view_count: Option<u64>,
    /// Full view-count text ("2,651,215 views").
    pub view_count_text: Option<String>,
    /// Exact like count a guest sees
    /// (`likeCountEntity.likeCountIfIndifferentNumber`).
    pub like_count: Option<u64>,
    /// Compact like-count text ("86K").
    pub like_count_text: Option<String>,
    /// Relative upload time as the app displays it ("5y ago").
    pub upload_time_text: Option<String>,
    /// Accessible form of the upload time ("5 years ago").
    pub upload_time_a11y_text: Option<String>,
}

impl UpdatedMetadata {
    pub fn to_value(&self) -> Value {
        json!({
            "provider": "youtube",
            "ok": true,
            "video_id": self.video_id,
            "view_count": self.view_count,
            "view_count_text": self.view_count_text,
            "like_count": self.like_count,
            "like_count_text": self.like_count_text,
            "upload_time_text": self.upload_time_text,
            "upload_time_a11y_text": self.upload_time_a11y_text,
        })
    }
}

fn entity_text(entity: &Value, path: &str) -> Option<String> {
    entity
        .pointer(path)
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
}

/// Parse an `updated_metadata` response.
pub fn parse_updated_metadata_response(resp: &Value, video_id: &str) -> UpdatedMetadata {
    let mut views = Vec::new();
    collect_renderers(resp, "viewCountEntity", &mut views);
    let view = views.first().cloned().unwrap_or(Value::Null);
    let mut likes = Vec::new();
    collect_renderers(resp, "likeCountEntity", &mut likes);
    let like = likes.first().cloned().unwrap_or(Value::Null);
    let mut uploads = Vec::new();
    collect_renderers(resp, "uploadTimeEntity", &mut uploads);
    let upload = uploads.first().cloned().unwrap_or(Value::Null);
    UpdatedMetadata {
        video_id: video_id.to_string(),
        view_count: view
            .get("viewCountNumber")
            .and_then(|n| n.as_str())
            .and_then(|s| s.parse::<u64>().ok()),
        view_count_text: entity_text(&view, "/viewCount/simpleText"),
        like_count: like
            .get("likeCountIfIndifferentNumber")
            .and_then(|n| n.as_str())
            .and_then(|s| s.parse::<u64>().ok()),
        like_count_text: entity_text(&like, "/likeCountIfIndifferent/content"),
        upload_time_text: entity_text(&upload, "/compactTimeAgo/content"),
        upload_time_a11y_text: entity_text(&upload, "/compactTimeAgoA11yText/content"),
    }
}

impl YoutubeClient {
    /// Fetch the engagement metadata (exact view count, like count,
    /// relative upload time) the app shows for a video, via the
    /// `updated_metadata` method.
    pub async fn get_updated_metadata(
        &self,
        video_id: &str,
    ) -> Result<UpdatedMetadata, YoutubeError> {
        let resp = self
            .post("updated_metadata", json!({ "videoId": video_id }))
            .await?;
        Ok(parse_updated_metadata_response(&resp, video_id))
    }
}
