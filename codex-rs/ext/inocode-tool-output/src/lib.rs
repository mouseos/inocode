//! Large tool output persistence.
//!
//! Instead of middle-truncating an oversized tool output (which loses the
//! omitted part for good), the full text is written to
//! `INOCODE_HOME/tool-results/<id>.txt` and the model only sees a preview, the
//! size and the path. The model can read the file back with its file tools.

use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_utils_string::approx_token_count;

pub const PERSISTED_OUTPUT_OPEN_TAG: &str = "<persisted-output>";
pub const PERSISTED_OUTPUT_CLOSE_TAG: &str = "</persisted-output>";
/// Preview size shown to the model.
pub const DEFAULT_PREVIEW_BYTES: usize = 2_000;

/// Resolve `INOCODE_HOME`, falling back to `~/.inocode`.
pub fn inocode_home() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("INOCODE_HOME")
        && !value.is_empty()
    {
        return Some(PathBuf::from(value));
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(PathBuf::from(home).join(".inocode"))
}

#[derive(Debug, Clone)]
pub struct PersistConfig {
    /// `compaction.persist_large_output`.
    pub enabled: bool,
    /// Outputs above this many (approximate) tokens are persisted.
    pub threshold_tokens: usize,
    pub preview_bytes: usize,
    /// Directory receiving `<id>.txt`.
    pub dir: PathBuf,
}

impl PersistConfig {
    pub fn new(enabled: bool, threshold_tokens: usize, inocode_home: &Path) -> Self {
        Self {
            enabled,
            threshold_tokens,
            preview_bytes: DEFAULT_PREVIEW_BYTES,
            dir: inocode_home.join("tool-results"),
        }
    }
}

/// Placeholder for an empty tool result.
pub fn empty_output_placeholder(tool_name: &str) -> String {
    format!("({tool_name} completed with no output)")
}

/// True if `text` was already replaced by a persisted-output marker.
pub fn is_persisted_output(text: &str) -> bool {
    text.starts_with(PERSISTED_OUTPUT_OPEN_TAG)
}

fn preview_of(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn next_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{n}-{}", std::process::id())
}

/// Where a persisted output came from; recorded in `index.jsonl` so a client can match a
/// stored output to its tool row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersistMeta {
    pub tool: Option<String>,
    pub thread_id: Option<String>,
    pub call_id: Option<String>,
}

/// One line of `tool-results/index.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexEntry {
    pub id: String,
    pub ts_ms: u64,
    pub bytes: u64,
    pub approx_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn append_index(dir: &Path, entry: &IndexEntry) {
    use std::io::Write;
    let Ok(mut line) = serde_json::to_string(entry) else {
        return;
    };
    line.push('\n');
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("index.jsonl"))
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Persisted outputs, newest first. Entries come from `index.jsonl`; stored files that have no
/// index line (older versions) are listed from their file metadata with no tool or call id.
pub fn list_outputs(dir: &Path) -> Vec<IndexEntry> {
    let mut entries: Vec<IndexEntry> = std::fs::read_to_string(dir.join("index.jsonl"))
        .map(|text| {
            text.lines()
                .filter_map(|l| serde_json::from_str::<IndexEntry>(l).ok())
                .collect()
        })
        .unwrap_or_default();
    entries.retain(|e| dir.join(format!("{}.txt", e.id)).is_file());
    let known: std::collections::HashSet<String> = entries.iter().map(|e| e.id.clone()).collect();
    if let Ok(read) = std::fs::read_dir(dir) {
        for item in read.flatten() {
            let path = item.path();
            if path.extension().and_then(|e| e.to_str()) != Some("txt") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
                continue;
            };
            if known.contains(&id) {
                continue;
            }
            let Ok(meta) = item.metadata() else { continue };
            let ts_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let bytes = meta.len();
            entries.push(IndexEntry {
                id,
                ts_ms,
                bytes,
                approx_tokens: bytes / 4,
                tool: None,
                thread_id: None,
                call_id: None,
            });
        }
    }
    entries.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms).then_with(|| b.id.cmp(&a.id)));
    entries
}

/// A slice of a stored output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSlice {
    pub bytes: u64,
    pub text: String,
    /// More text exists after the returned slice.
    pub truncated: bool,
}

/// True for ids this crate generates (`[0-9a-z-_]`), so an id can never name another path.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Reads `limit` bytes (all when `None`) of output `id` starting at byte `offset`, trimmed to
/// UTF-8 character boundaries.
pub fn read_output(
    dir: &Path,
    id: &str,
    offset: usize,
    limit: Option<usize>,
) -> std::io::Result<OutputSlice> {
    if !is_valid_id(id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid tool output id",
        ));
    }
    let data = std::fs::read(dir.join(format!("{id}.txt")))?;
    let total = data.len();
    let mut start = offset.min(total);
    let mut end = match limit {
        Some(l) => start.saturating_add(l).min(total),
        None => total,
    };
    // Move to character boundaries (continuation bytes are 0b10xxxxxx).
    while start < total && (data[start] & 0xC0) == 0x80 {
        start += 1;
    }
    while end < total && end > start && (data[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    Ok(OutputSlice {
        bytes: total as u64,
        text: String::from_utf8_lossy(&data[start..end]).into_owned(),
        truncated: end < total,
    })
}

/// If `text` exceeds the threshold, persist it and return the replacement
/// text shown to the model. Returns `None` when nothing should change (disabled,
/// small enough, already persisted, or the file could not be written, in which
/// case the caller falls back to its normal truncation).
pub fn persist_if_large(text: &str, config: &PersistConfig) -> Option<String> {
    persist_if_large_with(text, config, &PersistMeta::default())
}

/// Like [`persist_if_large`], and records `meta` in the index.
pub fn persist_if_large_with(
    text: &str,
    config: &PersistConfig,
    meta: &PersistMeta,
) -> Option<String> {
    if !config.enabled || is_persisted_output(text) {
        return None;
    }
    // Cheap byte pre-check before the token estimate.
    if text.len() <= config.threshold_tokens {
        return None;
    }
    if approx_token_count(text) <= config.threshold_tokens {
        return None;
    }
    std::fs::create_dir_all(&config.dir).ok()?;
    let id = next_id();
    let path = config.dir.join(format!("{id}.txt"));
    std::fs::write(&path, text).ok()?;
    append_index(
        &config.dir,
        &IndexEntry {
            id,
            ts_ms: now_ms(),
            bytes: text.len() as u64,
            approx_tokens: approx_token_count(text) as u64,
            tool: meta.tool.clone(),
            thread_id: meta.thread_id.clone(),
            call_id: meta.call_id.clone(),
        },
    );
    Some(render_persisted(text, &path, config.preview_bytes))
}

pub fn render_persisted(text: &str, path: &Path, preview_bytes: usize) -> String {
    let preview = preview_of(text, preview_bytes);
    let more = if preview.len() < text.len() { "\n..." } else { "" };
    format!(
        "{PERSISTED_OUTPUT_OPEN_TAG}\nOutput too large ({} bytes, ~{} tokens). Full output saved to: {}\nRead that file with the file read tool to retrieve the omitted part.\n\nPreview (first {} bytes):\n{preview}{more}\n{PERSISTED_OUTPUT_CLOSE_TAG}",
        text.len(),
        approx_token_count(text),
        path.display(),
        preview.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = PersistConfig::new(true, 1000, dir.path());
        assert_eq!(persist_if_large("hello", &cfg), None);
    }

    #[test]
    fn disabled_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = PersistConfig::new(false, 10, dir.path());
        assert_eq!(persist_if_large(&"x".repeat(10_000), &cfg), None);
    }

    #[test]
    fn large_output_is_persisted_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = PersistConfig::new(true, 100, dir.path());
        let text: String = (0..2000).map(|i| format!("line {i}\n")).collect();
        let out = persist_if_large(&text, &cfg).expect("persisted");
        assert!(out.starts_with(PERSISTED_OUTPUT_OPEN_TAG));
        assert!(out.contains("line 0"));
        assert!(!out.contains("line 1999"));
        let path = out
            .lines()
            .find_map(|l| l.split("Full output saved to: ").nth(1))
            .unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
        // Already-persisted markers are not persisted again.
        assert_eq!(persist_if_large(&out, &cfg), None);
    }

    #[test]
    fn index_lists_newest_first_with_call_ids_and_reads_slices() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = PersistConfig::new(true, 10, dir.path());
        let first = "a".repeat(500);
        let second = "あ".repeat(300);
        let meta = |call: &str| PersistMeta {
            tool: Some("shell".into()),
            thread_id: Some("thr".into()),
            call_id: Some(call.into()),
        };
        persist_if_large_with(&first, &cfg, &meta("c1")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        persist_if_large_with(&second, &cfg, &meta("c2")).unwrap();
        // A file with no index line is still listed.
        std::fs::write(cfg.dir.join("legacy.txt"), "old").unwrap();
        let items = list_outputs(&cfg.dir);
        assert_eq!(items.len(), 3);
        let by_call = |c: &str| items.iter().find(|e| e.call_id.as_deref() == Some(c)).unwrap();
        assert_eq!(by_call("c1").bytes, 500);
        assert_eq!(by_call("c2").bytes, 900);
        assert_eq!(by_call("c2").thread_id.as_deref(), Some("thr"));
        assert!(items.iter().any(|e| e.id == "legacy" && e.tool.is_none()));
        let id = &by_call("c2").id;
        let all = read_output(&cfg.dir, id, 0, None).unwrap();
        assert_eq!(all.text, second);
        assert!(!all.truncated);
        // Slices never split a character.
        let part = read_output(&cfg.dir, id, 1, Some(5)).unwrap();
        assert!(part.truncated);
        assert_eq!(part.text, "あ");
        assert!(read_output(&cfg.dir, "../x", 0, None).is_err());
    }

    #[test]
    fn preview_respects_char_boundary() {
        let s = "あ".repeat(1000);
        let p = preview_of(&s, 2000);
        assert!(p.len() <= 2000 && p.len() % 3 == 0);
    }

    #[test]
    fn empty_placeholder() {
        assert_eq!(
            empty_output_placeholder("shell"),
            "(shell completed with no output)"
        );
    }
}
