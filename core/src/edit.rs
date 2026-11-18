//! The `edit` and `write` tools: replace an exact piece of a text file, or
//! write a whole file. Design and sources: docs/evidence/edit-write.md.

use std::path::Path;

use serde::Deserialize;
use serde_json::json;

use crate::conversation::ToolSpec;
use crate::read::resolve;

pub(crate) const EDIT: &str = "edit";
pub(crate) const WRITE: &str = "write";

pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: EDIT.into(),
            description: "Replace text in an existing text file. `old` must be copied \
                exactly from the file (without the line numbers `read` shows) and must \
                occur exactly once, unless `all` is true, which replaces every \
                occurrence; include enough surrounding lines to make it unique. The path \
                may be relative to the project folder."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["path", "old", "new"],
                "properties": {
                    "path": {"type": "string"},
                    "old": {"type": "string"},
                    "new": {"type": "string"},
                    "all": {"type": "boolean"}
                }
            }),
        },
        ToolSpec {
            name: WRITE.into(),
            description: "Write `content` as the whole text of a file, creating it and \
                its missing folders, or replacing what it held. Use `edit` to change part \
                of an existing file. The path may be relative to the project folder."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["path", "content"],
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                }
            }),
        },
    ]
}

#[derive(Deserialize)]
struct EditArguments {
    path: String,
    old: String,
    new: String,
    #[serde(default)]
    all: bool,
}

#[derive(Deserialize)]
struct WriteArguments {
    path: String,
    content: String,
}

pub(crate) async fn edit(folder: Option<&Path>, arguments: &str) -> Result<String, String> {
    let arguments: EditArguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let path = resolve(folder, &arguments.path)?;
    tokio::task::spawn_blocking(move || {
        let text = read_text(&path)?;
        let done = replace(&text, &arguments.old, &arguments.new, arguments.all)
            .map_err(|problem| format!("{} {problem}", path.display()))?;
        std::fs::write(&path, &done.text)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        let line = done.line;
        Ok(match done.count {
            _ if done.loose => format!(
                "Replaced 1 occurrence in {} at line {line}, matched with whitespace \
                 ignored; read the lines to check their indentation.",
                path.display()
            ),
            1 => format!(
                "Replaced 1 occurrence in {} at line {line}.",
                path.display()
            ),
            count => format!(
                "Replaced {count} occurrences in {}, the first at line {line}.",
                path.display()
            ),
        })
    })
    .await
    .map_err(|error| format!("edit failed: {error}"))?
}

pub(crate) async fn write(folder: Option<&Path>, arguments: &str) -> Result<String, String> {
    let arguments: WriteArguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let path = resolve(folder, &arguments.path)?;
    tokio::task::spawn_blocking(move || {
        let existed = path.exists();
        if path.is_dir() {
            return Err(format!("{} is a folder", path.display()));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        std::fs::write(&path, &arguments.content)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        let lines = arguments.content.lines().count();
        Ok(format!(
            "{} {} ({lines} lines).",
            if existed { "Replaced" } else { "Created" },
            path.display()
        ))
    })
    .await
    .map_err(|error| format!("write failed: {error}"))?
}

fn read_text(path: &Path) -> Result<String, String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    if bytes.contains(&0) {
        return Err(format!("{} is not a text file", path.display()));
    }
    String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8 text", path.display()))
}

/// A file's text after an edit.
#[derive(Debug, PartialEq)]
struct Replaced {
    text: String,
    count: usize,
    /// Line of the first replacement.
    line: usize,
    /// Matched only with whitespace ignored.
    loose: bool,
}

/// `text` with `old` replaced by `new`. `read` shows lines without their
/// `\r`, so in a file with CRLF line ends `old` and `new` written with `\n`
/// are matched and written with `\r\n`. When `old` does not occur exactly,
/// whole lines equal to its lines with whitespace ignored are replaced, but
/// only when they occur once (docs/evidence/edit-write.md).
fn replace(text: &str, old: &str, new: &str, all: bool) -> Result<Replaced, String> {
    if old.is_empty() {
        return Err("was not changed: `old` is empty; use `write` for a new file".into());
    }
    if old == new {
        return Err("was not changed: `old` and `new` are the same".into());
    }
    let crlf = |value: &str| value.replace("\r\n", "\n").replace('\n', "\r\n");
    let (exact_old, exact_new) =
        if !text.contains(old) && text.contains("\r\n") && text.contains(&crlf(old)) {
            (crlf(old), crlf(new))
        } else {
            (old.to_string(), new.to_string())
        };
    let (old, new) = (exact_old.as_str(), exact_new.as_str());
    let count = text.matches(old).count();
    match count {
        0 => {
            return loose_replace(text, old, new)?.ok_or_else(|| {
                "was not changed: `old` does not occur in it, even with whitespace ignored; \
                 read the lines again and copy them exactly"
                    .to_string()
            })
        }
        1 => {}
        _ if all => {}
        _ => {
            return Err(format!(
                "was not changed: `old` occurs {count} times; add surrounding lines to make it unique, or set `all`"
            ))
        }
    }
    let first = text.find(old).expect("counted at least once");
    Ok(Replaced {
        text: text.replace(old, new),
        count,
        line: text[..first].matches('\n').count() + 1,
        loose: false,
    })
}

/// A line without its surrounding whitespace, inner runs of whitespace made
/// one space.
fn normalized(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn indentation(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// Replaces the one run of whole lines of `text` equal to the lines of `old`
/// with whitespace ignored: `None` when there is none, an error when there
/// are several. When every non-blank line of the run is indented by the same
/// extra prefix compared with `old`, `new` gets that prefix too.
fn loose_replace(text: &str, old: &str, new: &str) -> Result<Option<Replaced>, String> {
    let given: Vec<&str> = old.lines().collect();
    let wanted: Vec<String> = given.iter().map(|line| normalized(line)).collect();
    if wanted.iter().all(String::is_empty) {
        return Ok(None);
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let bare = |line: &str| line.trim_end_matches(['\n', '\r']).to_string();
    let starts: Vec<usize> = (0..(lines.len() + 1).saturating_sub(wanted.len()))
        .filter(|&start| {
            wanted
                .iter()
                .enumerate()
                .all(|(offset, want)| normalized(&bare(lines[start + offset])) == *want)
        })
        .collect();
    let start = match starts.as_slice() {
        [] => return Ok(None),
        [start] => *start,
        _ => {
            return Err(format!(
                "was not changed: `old` does not occur exactly, and with whitespace ignored \
                 it occurs {} times; copy the lines exactly",
                starts.len()
            ))
        }
    };
    let end = start + wanted.len();
    let mut prefixes = lines[start..end]
        .iter()
        .zip(&given)
        .filter(|(line, _)| !line.trim().is_empty())
        .map(|(line, old_line)| {
            indentation(&bare(line))
                .strip_suffix(indentation(old_line))
                .map(str::to_string)
        });
    let prefix = match prefixes.next().flatten() {
        Some(prefix) if prefixes.all(|other| other.as_deref() == Some(prefix.as_str())) => prefix,
        _ => String::new(),
    };
    let ending = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut replacement = new
        .lines()
        .map(|line| match line.trim().is_empty() {
            true => line.to_string(),
            false => format!("{prefix}{line}"),
        })
        .collect::<Vec<_>>()
        .join(ending);
    if !new.is_empty() && lines[end - 1].ends_with('\n') {
        replacement.push_str(ending);
    }
    Ok(Some(Replaced {
        text: [lines[..start].concat(), replacement, lines[end..].concat()].concat(),
        count: 1,
        line: start + 1,
        loose: true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_needs_one_exact_occurrence_unless_all_is_set() {
        let text = "fn a() {\n    one();\n}\nfn b() {\n    one();\n}\n";
        let error = replace(text, "one();", "two();", false).unwrap_err();
        assert!(error.contains("occurs 2 times"), "{error}");
        let all = replace(text, "one();", "two();", true).expect("all");
        assert_eq!((all.count, all.line, all.loose), (2, 2, false));
        assert!(!all.text.contains("one();"));
        assert_eq!(
            replace(text, "fn b() {\n    one();", "fn b() {\n    two();", false),
            Ok(done(
                "fn a() {\n    one();\n}\nfn b() {\n    two();\n}\n",
                1,
                4,
                false
            ))
        );
        assert!(replace(text, "three();", "x", false)
            .unwrap_err()
            .contains("does not occur"));
        assert!(replace(text, "", "x", false).unwrap_err().contains("empty"));
        assert!(replace(text, "one();", "one();", false)
            .unwrap_err()
            .contains("the same"));
    }

    fn done(text: &str, count: usize, line: usize, loose: bool) -> Replaced {
        Replaced {
            text: text.to_string(),
            count,
            line,
            loose,
        }
    }

    #[test]
    fn crlf_files_take_edits_written_with_newlines() {
        assert_eq!(
            replace("a\r\nb\r\nc\r\n", "b\nc", "B\nC\nD", false),
            Ok(done("a\r\nB\r\nC\r\nD\r\n", 1, 2, false))
        );
    }

    #[test]
    fn whole_lines_match_with_whitespace_ignored_only_when_unique() {
        let text = "class A:\n    def run(self):\n        return  1\n\nx = 2\n";
        // Indented less than in the file: the new lines get the file's
        // extra indentation too.
        assert_eq!(
            replace(
                text,
                "def run(self):\n    return 1",
                "def run(self):\n    return 3",
                false
            ),
            Ok(done(
                "class A:\n    def run(self):\n        return 3\n\nx = 2\n",
                1,
                2,
                true
            ))
        );
        // Inner and trailing spaces, CRLF ends, no newline at the end.
        assert_eq!(
            replace("a  =  1 \r\nb = 2", "a = 1\nb = 2", "a = 1\nb = 3", false),
            Ok(done("a = 1\r\nb = 3", 1, 1, true))
        );
        // Deleting the lines.
        assert_eq!(
            replace("x\n  y()\nz\n", "\ty()", "", false),
            Ok(done("x\nz\n", 1, 2, true))
        );
        // Twice with whitespace ignored: refused, even with `all`.
        let twice = "if a:\n    go()\nif b:\n  go()\n";
        let error = replace(twice, "\tgo()", "stop()", true).unwrap_err();
        assert!(error.contains("occurs 2 times"), "{error}");
        // A piece of a line is not matched loosely.
        assert!(replace(text, "def  run", "def go", false)
            .unwrap_err()
            .contains("even with whitespace ignored"));
    }

    #[tokio::test]
    async fn edit_and_write_change_files_in_the_project_folder() {
        let dir = tempfile::tempdir().expect("dir");
        let folder = Some(dir.path());
        let created = write(folder, r#"{"path": "src/new.txt", "content": "x\ny\n"}"#)
            .await
            .expect("write");
        assert!(created.starts_with("Created "), "{created}");
        assert!(created.ends_with("(2 lines)."), "{created}");
        let path = dir.path().join("src").join("new.txt");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "x\ny\n");

        let edited = edit(folder, r#"{"path": "src/new.txt", "old": "y", "new": "z"}"#)
            .await
            .expect("edit");
        assert!(edited.ends_with("at line 2."), "{edited}");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "x\nz\n");

        let replaced = write(folder, r#"{"path": "src/new.txt", "content": "w"}"#)
            .await
            .expect("write");
        assert!(replaced.starts_with("Replaced "), "{replaced}");

        let missing = edit(folder, r#"{"path": "none.txt", "old": "a", "new": "b"}"#).await;
        assert!(missing.unwrap_err().starts_with("cannot open"));
        std::fs::write(dir.path().join("bin"), b"a\0b").expect("write");
        let binary = edit(folder, r#"{"path": "bin", "old": "a", "new": "b"}"#).await;
        assert!(binary.unwrap_err().ends_with("is not a text file"));
        assert!(write(None, r#"{"path": "rel.txt", "content": ""}"#)
            .await
            .unwrap_err()
            .contains("no project folder"));
    }
}
