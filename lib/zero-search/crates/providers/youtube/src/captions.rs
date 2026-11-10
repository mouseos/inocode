//! Caption track bodies: the timedtext documents the app downloads
//! from the server-issued `baseUrl` in the player response's
//! `playerCaptionsTracklistRenderer`.
//!
//! The app fetches the track URL as issued and renders the cues.
//! We ask the same URL for `fmt=json3`; the endpoint answers with
//! either the JSON form or the srv3 XML form of the same document
//! (the ASR track of the round-2 verification video answered with
//! srv3), and both forms are parsed.

use serde_json::{json, Value};

use crate::client::YoutubeClient;
use crate::errors::{YoutubeError, KIND_NOT_FOUND, KIND_PARSE_ERROR};
use crate::player::CaptionTrack;

/// One caption cue.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CaptionCue {
    pub start_ms: u64,
    pub duration_ms: u64,
    pub text: String,
}

impl CaptionCue {
    pub fn to_value(&self) -> Value {
        json!({
            "start_ms": self.start_ms,
            "duration_ms": self.duration_ms,
            "text": self.text,
        })
    }
}

/// The fetched body of one caption track.
pub struct CaptionTranscript {
    pub video_id: String,
    pub language_code: String,
    pub kind: Option<String>,
    pub cues: Vec<CaptionCue>,
}

impl CaptionTranscript {
    pub fn to_value(&self) -> Value {
        json!({
            "provider": "youtube",
            "ok": true,
            "video_id": self.video_id,
            "language_code": self.language_code,
            "kind": self.kind,
            "cue_count": self.cues.len(),
            "cues": self.cues.iter().map(|c| c.to_value()).collect::<Vec<_>>(),
        })
    }

    /// All cue texts joined with newlines.
    pub fn full_text(&self) -> String {
        self.cues
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Parse a timedtext `fmt=json3` document into cues.
pub fn parse_timedtext_json3(body: &str) -> Result<Vec<CaptionCue>, YoutubeError> {
    let v: Value = serde_json::from_str(body).map_err(|e| {
        YoutubeError::new(
            KIND_PARSE_ERROR,
            format!("timedtext json3: invalid JSON: {e}"),
        )
    })?;
    let events = v
        .get("events")
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_default();
    let mut cues = Vec::new();
    for ev in &events {
        let Some(segs) = ev.get("segs").and_then(|s| s.as_array()) else {
            continue;
        };
        let mut text = String::new();
        for seg in segs {
            if let Some(t) = seg.get("utf8").and_then(|t| t.as_str()) {
                text.push_str(t);
            }
        }
        let text = text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        cues.push(CaptionCue {
            start_ms: ev.get("tStartMs").and_then(|t| t.as_u64()).unwrap_or(0),
            duration_ms: ev.get("dDurationMs").and_then(|t| t.as_u64()).unwrap_or(0),
            text,
        });
    }
    Ok(cues)
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Minimal reader for the srv3 XML form of a timedtext document
/// (`<p t=".." d="..">text</p>` entries), used when a track does not
/// answer with the JSON form.
pub fn parse_timedtext_srv3(body: &str) -> Result<Vec<CaptionCue>, YoutubeError> {
    let mut cues = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("<p ") {
        let after = &rest[start..];
        let Some(tag_end) = after.find('>') else {
            break;
        };
        let tag = &after[..tag_end];
        let attr = |name: &str| -> Option<u64> {
            let pat = format!("{name}=\"");
            let i = tag.find(&pat)? + pat.len();
            let j = tag[i..].find('"')? + i;
            tag[i..j].parse::<u64>().ok()
        };
        let Some(close) = after.find("</p>") else {
            break;
        };
        let inner = &after[tag_end + 1..close];
        let text = xml_unescape(&strip_tags(inner)).trim().to_string();
        if !text.is_empty() {
            cues.push(CaptionCue {
                start_ms: attr("t").unwrap_or(0),
                duration_ms: attr("d").unwrap_or(0),
                text,
            });
        }
        rest = &after[close + 4..];
    }
    if cues.is_empty() {
        return Err(YoutubeError::new(
            KIND_PARSE_ERROR,
            "timedtext srv3: no cues found in document",
        ));
    }
    Ok(cues)
}

/// Pick the track for `language` (exact `languageCode` match), else
/// the first manual track, else the first track.
fn pick_track<'a>(tracks: &'a [CaptionTrack], language: Option<&str>) -> Option<&'a CaptionTrack> {
    if let Some(lang) = language {
        if let Some(t) = tracks.iter().find(|t| t.language_code == lang) {
            return Some(t);
        }
    }
    tracks
        .iter()
        .find(|t| t.kind.as_deref() != Some("asr"))
        .or_else(|| tracks.first())
}

impl YoutubeClient {
    /// Fetch the caption transcript of one track of a video.
    /// `language` selects by `languageCode`; without it, a manual
    /// track is preferred over the automatic (ASR) one.
    ///
    /// Two requests are made: the player request that issues the
    /// track URL, then the timedtext document itself — the same two
    /// steps the app performs when captions are shown.
    pub async fn get_captions(
        &self,
        video_id: &str,
        language: Option<&str>,
    ) -> Result<CaptionTranscript, YoutubeError> {
        let details = self.get_video(video_id).await?;
        let track = pick_track(&details.caption_tracks, language).ok_or_else(|| {
            YoutubeError::new(
                KIND_NOT_FOUND,
                format!("no caption tracks for video {video_id}"),
            )
        })?;
        let url = format!("{}&fmt=json3", track.base_url);
        let body = self.fetch_text(&url).await?;
        let cues = if body.trim_start().starts_with('{') {
            parse_timedtext_json3(&body)?
        } else {
            parse_timedtext_srv3(&body)?
        };
        Ok(CaptionTranscript {
            video_id: video_id.to_string(),
            language_code: track.language_code.clone(),
            kind: track.kind.clone(),
            cues,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json3_events_become_cues() {
        let body = r#"{"wireMagic":"pb3","events":[
            {"tStartMs":0,"dDurationMs":1200,"id":1,"segs":[{"utf8":"Hello "},{"utf8":"world"}]},
            {"tStartMs":1200,"dDurationMs":800,"id":2},
            {"tStartMs":2000,"dDurationMs":900,"id":3,"segs":[{"utf8":"\n"}]},
            {"tStartMs":2900,"dDurationMs":1000,"id":4,"segs":[{"utf8":"Second line"}]}
        ]}"#;
        let cues = parse_timedtext_json3(body).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "Hello world");
        assert_eq!(cues[0].start_ms, 0);
        assert_eq!(cues[1].text, "Second line");
        assert_eq!(cues[1].duration_ms, 1000);
    }

    #[test]
    fn srv3_paragraphs_become_cues() {
        let body = r#"<?xml version="1.0" encoding="utf-8"?><timedtext format="3"><body>
            <p t="0" d="1200">Hello &amp; welcome</p>
            <p t="1200" d="800"><s>Second</s> <s>cue</s></p>
            </body></timedtext>"#;
        let cues = parse_timedtext_srv3(body).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "Hello & welcome");
        assert_eq!(cues[1].text, "Second cue");
        assert_eq!(cues[1].start_ms, 1200);
    }
}
