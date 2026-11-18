//! The `read` tool: a window of at most 100 numbered lines of a file, or of
//! one definition found by name. Design and sources:
//! docs/evidence/harness-tools.md, docs/evidence/read-function.md.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;

use crate::conversation::ToolSpec;
use crate::outline;

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
            answer says how many lines the file has; read further windows as needed. \
            With `name`, read just the definition (function, type, method, heading) of \
            that name in the file, in 100-line windows when it is longer; `from` then \
            picks which same-named definition and where in it to continue."
            .into(),
        parameters: json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {"type": "string"},
                "name": {"type": "string"},
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
    name: Option<String>,
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
    let name = arguments.name;
    tokio::task::spawn_blocking(move || match name {
        Some(name) => definition(&path, &name, arguments.from, count),
        None => window(&path, from, count),
    })
    .await
    .map_err(|error| format!("read failed: {error}"))?
}

pub(crate) fn resolve(folder: Option<&Path>, path: &str) -> Result<PathBuf, String> {
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

/// A window of the definition named `name`: the one containing line `from`
/// when there is one, else the first; the window starts at `from` inside it.
fn definition(
    path: &Path,
    name: &str,
    from: Option<usize>,
    count: usize,
) -> Result<String, String> {
    let definitions = outline::file_definitions(path)?.ok_or_else(|| {
        format!(
            "{} is not in a language whose definitions can be found; read it by lines",
            path.display()
        )
    })?;
    let named: Vec<&outline::Definition> = definitions
        .iter()
        .filter(|definition| definition.name == name)
        .collect();
    let inside = |definition: &&&outline::Definition| {
        from.is_some_and(|line| definition.line <= line && line <= definition.end)
    };
    let chosen = *named
        .iter()
        .find(inside)
        .or(named.first())
        .ok_or_else(|| format!("{} has no definition named {name}", path.display()))?;
    let start = from
        .filter(|line| chosen.line <= *line && *line <= chosen.end)
        .unwrap_or(chosen.line);
    let count = count.min(chosen.end - start + 1);
    let mut answer = format!(
        "{name}: lines {}-{} of {}",
        chosen.line,
        chosen.end,
        path.display()
    );
    let others: Vec<String> = named
        .iter()
        .filter(|other| other.line != chosen.line)
        .map(|other| other.line.to_string())
        .collect();
    if !others.is_empty() {
        answer.push_str(&format!(
            ". Also defined at lines {}; pass one as `from` to read it",
            others.join(", ")
        ));
    }
    answer.push('\n');
    answer.push_str(&window(path, start, count)?);
    let last = start + count - 1;
    if last < chosen.end {
        answer.push_str(&format!("\n… continue with from={}", last + 1));
    }
    Ok(answer)
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

    #[test]
    fn a_definition_is_read_by_name_in_windows() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("a.rs");
        let body: String = (1..=150)
            .map(|n| format!("    let x{n} = {n};\n"))
            .collect();
        std::fs::write(
            &path,
            format!("fn small() {{}}\n\nfn big() {{\n{body}}}\n\nfn after() {{}}\n"),
        )
        .expect("write");

        let first = definition(&path, "big", None, WINDOW).expect("read");
        assert!(first.starts_with(&format!("big: lines 3-154 of {}\n", path.display())));
        assert!(first.contains("lines 3-102 of 156:\n3: fn big() {"));
        assert!(first.ends_with("… continue with from=103"));

        let rest = definition(&path, "big", Some(103), WINDOW).expect("read");
        assert!(rest.contains("lines 103-154 of 156"));
        assert!(rest.ends_with("154: }"));

        assert!(definition(&path, "small", None, WINDOW)
            .expect("read")
            .ends_with("lines 1-1 of 156:\n1: fn small() {}"));
        assert!(definition(&path, "missing", None, WINDOW)
            .unwrap_err()
            .ends_with("has no definition named missing"));
    }

    #[test]
    fn same_named_definitions_are_chosen_by_line() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("a.py");
        std::fs::write(
            &path,
            "class A:\n    def run(self):\n        return 1\n\nclass B:\n    def run(self):\n        return 2\n",
        )
        .expect("write");
        let first = definition(&path, "run", None, WINDOW).expect("read");
        assert!(first.starts_with("run: lines 2-3 of "));
        assert!(first.contains("Also defined at lines 6"));
        let second = definition(&path, "run", Some(6), WINDOW).expect("read");
        assert!(second.ends_with("7:         return 2"));
    }

    #[test]
    fn a_c_function_includes_its_body() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("a.c");
        std::fs::write(
            &path,
            "static int\nadd(int a,\n    int b)\n{\n\treturn a + b;\n}\n\nint main(void) { return add(1, 2); }\n",
        )
        .expect("write");
        let answer = definition(&path, "add", None, WINDOW).expect("read");
        assert!(answer.starts_with("add: lines 2-6 of "), "{answer}");
        assert!(answer.ends_with("6: }"), "{answer}");
    }

    #[test]
    fn other_languages_end_by_indentation() {
        let dir = tempfile::tempdir().expect("dir");
        let ruby = dir.path().join("shape.rb");
        std::fs::write(
            &ruby,
            "class Shape\n  def area\n    0\n  end\n\n  def name\n    'shape'\n  end\nend\n",
        )
        .expect("write");
        assert!(definition(&ruby, "area", None, WINDOW)
            .expect("read")
            .ends_with("2:   def area\n3:     0\n4:   end"));
        assert!(definition(&ruby, "Shape", None, WINDOW)
            .expect("read")
            .starts_with("Shape: lines 1-9 of "));

        let markdown = dir.path().join("guide.md");
        std::fs::write(&markdown, "# Guide\n\nText.\n\n## Install\n\nSteps.\n").expect("write");
        assert!(definition(&markdown, "Guide", None, WINDOW)
            .expect("read")
            .starts_with("Guide: lines 1-3 of "));
    }
}
