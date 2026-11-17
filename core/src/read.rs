//! The `read` tool: a window of at most 100 numbered lines of a file.
//! Design and sources: docs/evidence/harness-tools.md.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;

use crate::conversation::ToolSpec;

pub(crate) const READ: &str = "read";
/// Lines one call shows at most; SWE-agent found a 100-line window better
/// than a whole file or 30 lines.
const WINDOW: usize = 100;
const LINE_CHARS: usize = 2_000;

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: READ.into(),
        description: "Read up to 100 numbered lines of a text file, starting at line \
            `from` (1 by default). The path may be relative to the project folder. The \
            answer says how many lines the file has; read further windows as needed."
            .into(),
        parameters: json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {"type": "string"},
                "from": {"type": "integer", "minimum": 1},
                "lines": {"type": "integer", "minimum": 1, "maximum": WINDOW}
            }
        }),
    }
}

#[derive(Deserialize)]
struct Arguments {
    path: String,
    #[serde(default)]
    from: Option<usize>,
    #[serde(default)]
    lines: Option<usize>,
}

pub(crate) async fn run(folder: Option<&Path>, arguments: &str) -> Result<String, String> {
    let arguments: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let path = resolve(folder, &arguments.path)?;
    let from = arguments.from.unwrap_or(1).max(1);
    let count = arguments.lines.unwrap_or(WINDOW).clamp(1, WINDOW);
    tokio::task::spawn_blocking(move || window(&path, from, count))
        .await
        .map_err(|error| format!("read failed: {error}"))?
}

fn resolve(folder: Option<&Path>, path: &str) -> Result<PathBuf, String> {
    let given = Path::new(path);
    if given.is_absolute() {
        return Ok(given.to_path_buf());
    }
    folder
        .map(|folder| folder.join(given))
        .ok_or_else(|| format!("{path} is relative, and this thread has no project folder"))
}

fn window(path: &Path, from: usize, count: usize) -> Result<String, String> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut shown = Vec::new();
    let mut total = 0usize;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        if line.contains(&0) {
            return Err(format!("{} is not a text file", path.display()));
        }
        total += 1;
        if total >= from && shown.len() < count {
            let text = String::from_utf8_lossy(&line);
            let text = text.trim_end_matches(['\n', '\r']);
            shown.push(format!("{total}: {}", cut(text)));
        }
    }
    if total == 0 {
        return Ok(format!("{} is empty.", path.display()));
    }
    if shown.is_empty() {
        return Err(format!(
            "{} has {total} lines; line {from} is past the end",
            path.display()
        ));
    }
    let last = from + shown.len() - 1;
    Ok(format!(
        "{} lines {from}-{last} of {total}:\n{}",
        path.display(),
        shown.join("\n")
    ))
}

fn cut(text: &str) -> String {
    match text.char_indices().nth(LINE_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_is_numbered_and_says_how_long_the_file_is() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("a.txt");
        let text: String = (1..=250).map(|n| format!("line {n}\r\n")).collect();
        std::fs::write(&path, text).expect("write");

        let first = window(&path, 1, WINDOW).expect("read");
        assert!(first.starts_with(&format!(
            "{} lines 1-100 of 250:\n1: line 1\n",
            path.display()
        )));
        assert!(first.ends_with("100: line 100"));

        let last = window(&path, 240, WINDOW).expect("read");
        assert!(last.contains("lines 240-250 of 250"));
        assert!(window(&path, 251, WINDOW)
            .unwrap_err()
            .contains("past the end"));

        std::fs::write(&path, b"a\0b").expect("write");
        assert!(window(&path, 1, WINDOW)
            .unwrap_err()
            .ends_with("is not a text file"));
    }
}
