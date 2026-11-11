//! Extracts the files a sub-agent was assigned from its spawn instruction.
//!
//! The instruction names its files on a line that starts with a label such as
//! `owned paths:`, `owned_paths:`, `owns:`, `files:` or `担当パス:` / `担当ファイル:`;
//! paths are separated by commas, semicolons or whitespace and may be wrapped in backticks or
//! quotes. A list may continue on following `- path` bullet lines.

const LABELS: [&str; 9] = [
    "owned paths",
    "owned_paths",
    "owned files",
    "owns",
    "files",
    "write paths",
    "担当パス",
    "担当ファイル",
    "担当",
];

fn split_paths(rest: &str, out: &mut Vec<String>) {
    for token in rest.split(|c: char| c == ',' || c == ';' || c == '、' || c.is_whitespace()) {
        let path = token
            .trim_matches(|c: char| matches!(c, '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '。'))
            .trim();
        if path.is_empty() || path.eq_ignore_ascii_case("none") || path == "-" {
            continue;
        }
        if !out.iter().any(|p| p == path) {
            out.push(path.to_string());
        }
    }
}

/// Paths named on `owned paths:`-style lines (and their bullet continuations), in order.
pub fn owned_paths_from_instruction(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_list = false;
    for line in text.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_lowercase();
        let labelled = LABELS.iter().find_map(|label| {
            let rest = lower.strip_prefix(label)?;
            let rest = rest.trim_start();
            let rest = rest.strip_prefix(':').or_else(|| rest.strip_prefix('：'))?;
            // Index into the original line, which has the same prefix length in bytes
            // only when lowercasing preserved it; fall back to the lowered text otherwise.
            let consumed = lower.len() - rest.len();
            Some(trimmed.get(consumed..).unwrap_or(rest).to_string())
        });
        if let Some(rest) = labelled {
            split_paths(&rest, &mut out);
            in_list = true;
        } else if in_list && (trimmed.starts_with("- ") || trimmed.starts_with("* ")) {
            split_paths(&trimmed[2..], &mut out);
        } else {
            in_list = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn reads_inline_and_bulleted_lists() {
        let text = "Port wait.rs.\nowned paths: `core/src/a.rs`, core/src/b.rs\n- core/src/c.rs\n- \"core/src/d.rs\"\nDo not touch anything else.";
        assert_eq!(
            owned_paths_from_instruction(text),
            vec!["core/src/a.rs", "core/src/b.rs", "core/src/c.rs", "core/src/d.rs"]
        );
    }

    #[test]
    fn accepts_japanese_labels_and_ignores_prose() {
        assert_eq!(
            owned_paths_from_instruction("担当ファイル: app-server/src/x.rs、app-server/src/y.rs"),
            vec!["app-server/src/x.rs", "app-server/src/y.rs"]
        );
        assert!(owned_paths_from_instruction("files are everywhere\nno list here").is_empty());
        assert!(owned_paths_from_instruction("Files: none").is_empty());
    }
}
