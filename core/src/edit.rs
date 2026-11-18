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
        let (changed, count, line) = replace(&text, &arguments.old, &arguments.new, arguments.all)
            .map_err(|problem| format!("{} {problem}", path.display()))?;
        std::fs::write(&path, changed)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        Ok(match count {
            1 => format!(
                "Replaced 1 occurrence in {} at line {line}.",
                path.display()
            ),
            _ => format!(
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

/// `text` with `old` replaced by `new`, the number of replacements, and the
/// line of the first. `read` shows lines without their `\r`, so in a file
/// with CRLF line ends `old` and `new` written with `\n` are matched and
/// written with `\r\n`.
fn replace(text: &str, old: &str, new: &str, all: bool) -> Result<(String, usize, usize), String> {
    if old.is_empty() {
        return Err("was not changed: `old` is empty; use `write` for a new file".into());
    }
    if old == new {
        return Err("was not changed: `old` and `new` are the same".into());
    }
    let crlf = |value: &str| value.replace("\r\n", "\n").replace('\n', "\r\n");
    let (old, new) = if !text.contains(old) && text.contains("\r\n") && text.contains(&crlf(old)) {
        (crlf(old), crlf(new))
    } else {
        (old.to_string(), new.to_string())
    };
    let count = text.matches(old.as_str()).count();
    match count {
        0 => return Err("was not changed: `old` does not occur in it; read the lines again and copy them exactly".into()),
        1 => {}
        _ if all => {}
        _ => {
            return Err(format!(
                "was not changed: `old` occurs {count} times; add surrounding lines to make it unique, or set `all`"
            ))
        }
    }
    let first = text.find(old.as_str()).expect("counted at least once");
    let line = text[..first].matches('\n').count() + 1;
    Ok((text.replace(old.as_str(), new.as_str()), count, line))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_needs_one_exact_occurrence_unless_all_is_set() {
        let text = "fn a() {\n    one();\n}\nfn b() {\n    one();\n}\n";
        let error = replace(text, "one();", "two();", false).unwrap_err();
        assert!(error.contains("occurs 2 times"), "{error}");
        let (changed, count, line) = replace(text, "one();", "two();", true).expect("all");
        assert_eq!((count, line), (2, 2));
        assert!(!changed.contains("one();"));
        let (changed, count, line) =
            replace(text, "fn b() {\n    one();", "fn b() {\n    two();", false).expect("one");
        assert_eq!((count, line), (1, 4));
        assert_eq!(
            changed,
            "fn a() {\n    one();\n}\nfn b() {\n    two();\n}\n"
        );
        assert!(replace(text, "three();", "x", false)
            .unwrap_err()
            .contains("does not occur"));
        assert!(replace(text, "", "x", false).unwrap_err().contains("empty"));
        assert!(replace(text, "one();", "one();", false)
            .unwrap_err()
            .contains("the same"));
    }

    #[test]
    fn crlf_files_take_edits_written_with_newlines() {
        let text = "a\r\nb\r\nc\r\n";
        let (changed, count, line) = replace(text, "b\nc", "B\nC\nD", false).expect("crlf");
        assert_eq!((count, line), (1, 2));
        assert_eq!(changed, "a\r\nB\r\nC\r\nD\r\n");
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
