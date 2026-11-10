//! Large tool output persistence.
//!
//! Instead of middle-truncating an oversized tool output (which loses the
//! omitted part for good), the full text is written to
//! `INOCODE_HOME/tool-results/<id>.txt` and the model only sees a preview, the
//! size and the path. The model can read the file back with its file tools.

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

/// If `text` exceeds the threshold, persist it and return the replacement
/// text shown to the model. Returns `None` when nothing should change (disabled,
/// small enough, already persisted, or the file could not be written, in which
/// case the caller falls back to its normal truncation).
pub fn persist_if_large(text: &str, config: &PersistConfig) -> Option<String> {
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
    let path = config.dir.join(format!("{}.txt", next_id()));
    std::fs::write(&path, text).ok()?;
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
