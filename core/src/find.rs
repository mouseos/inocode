//! The `find` tool: search file contents (text or a regular expression),
//! file names (globs), or definitions by name, over the project with no
//! index, so every answer reflects the files as they are now. Answers are
//! matching lines as `path:line: text`, capped, with the total. Design,
//! sources and the benchmark: docs/evidence/harness-tools.md and
//! docs/evidence/repo-understanding.md.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::Lossy;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::{WalkBuilder, WalkState};
use serde::Deserialize;
use serde_json::json;

use crate::conversation::ToolSpec;
use crate::outline;

pub(crate) const FIND: &str = "find";
/// Results shown at once; SWE-agent capped its search at 50.
const SHOWN: usize = 50;
/// Results kept for sorting; beyond this only the count grows.
const KEPT: usize = 10_000;
const LINE_CHARS: usize = 300;

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: FIND.into(),
        description: "Search the project. With pattern: lines containing the text (or the \
            regular expression when regex is true). With name: where a function, type, \
            class or method of exactly that name is defined. With only files: file paths \
            matching the glob. files also limits the other searches (e.g. \"*.rs\", \
            \"src/**\"). Shows up to 50 results as path:line: text, with the total; narrow \
            the search when there are more. Respects .gitignore."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "regex": {"type": "boolean"},
                "ignore_case": {"type": "boolean"},
                "name": {"type": "string"},
                "files": {"type": "string", "description": "A glob over paths relative to the searched folder."},
                "path": {"type": "string", "description": "A folder to search instead of the whole project."}
            }
        }),
    }
}

#[derive(Deserialize, Default)]
struct Arguments {
    #[serde(default)]
    pattern: Option<String>,
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    files: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

/// One result: a file, and a line in it unless only the file matched.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Found {
    path: PathBuf,
    line: Option<u64>,
    text: String,
}

pub(crate) async fn run(folder: Option<&Path>, arguments: &str) -> Result<String, String> {
    let arguments: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let root = match (&arguments.path, folder) {
        (Some(path), _) if Path::new(path).is_absolute() => PathBuf::from(path),
        (Some(path), Some(folder)) => folder.join(path),
        (None, Some(folder)) => folder.to_path_buf(),
        (_, None) => return Err("this thread has no project folder to search".into()),
    };
    if !root.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    tokio::task::spawn_blocking(move || search(&root, &arguments))
        .await
        .map_err(|error| format!("find failed: {error}"))?
}

fn search(root: &Path, arguments: &Arguments) -> Result<String, String> {
    let files = match &arguments.files {
        Some(glob) => Some(
            globset::GlobBuilder::new(glob)
                .build()
                .map_err(|error| format!("invalid files glob: {error}"))?
                .compile_matcher(),
        ),
        None => None,
    };
    let (found, total) = match (&arguments.name, &arguments.pattern) {
        (Some(name), _) if !name.trim().is_empty() => by_name(root, files.as_ref(), name.trim())?,
        (_, Some(pattern)) if !pattern.is_empty() => by_content(
            root,
            files.as_ref(),
            pattern,
            arguments.regex,
            arguments.ignore_case,
        )?,
        _ => match &files {
            Some(files) => by_path(root, files),
            None => return Err("give pattern, name, or files".into()),
        },
    };
    Ok(render(root, found, total))
}

/// Files under `root` that `.gitignore` keeps and the glob (if any) accepts.
fn walk<'s>(
    root: &'s Path,
    files: Option<&'s globset::GlobMatcher>,
    mut visit: impl FnMut() -> Box<dyn FnMut(&Path) + Send + 's>,
) {
    WalkBuilder::new(root)
        .require_git(false)
        .build_parallel()
        .run(|| {
            let mut each = visit();
            Box::new(move |entry| {
                let Ok(entry) = entry else {
                    return WalkState::Continue;
                };
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    return WalkState::Continue;
                }
                let path = entry.path();
                let relative = path.strip_prefix(root).unwrap_or(path);
                if files.is_some_and(|glob| !glob.is_match(relative)) {
                    return WalkState::Continue;
                }
                each(path);
                WalkState::Continue
            })
        });
}

struct Collected {
    found: Vec<Found>,
    total: usize,
}

impl Collected {
    fn add(&mut self, found: Found) {
        self.total += 1;
        if self.found.len() < KEPT {
            self.found.push(found);
        }
    }
}

fn by_path(root: &Path, files: &globset::GlobMatcher) -> (Vec<Found>, usize) {
    let collected = Mutex::new(Collected {
        found: Vec::new(),
        total: 0,
    });
    walk(root, Some(files), || {
        let collected = &collected;
        Box::new(move |path| {
            if let Ok(mut collected) = collected.lock() {
                collected.add(Found {
                    path: path.to_path_buf(),
                    line: None,
                    text: String::new(),
                });
            }
        })
    });
    let collected = collected.into_inner().unwrap_or_else(|e| e.into_inner());
    (collected.found, collected.total)
}

fn by_content(
    root: &Path,
    files: Option<&globset::GlobMatcher>,
    pattern: &str,
    regex: bool,
    ignore_case: bool,
) -> Result<(Vec<Found>, usize), String> {
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(ignore_case)
        .fixed_strings(!regex)
        .build(pattern)
        .map_err(|error| format!("invalid pattern: {error}"))?;
    let collected = Mutex::new(Collected {
        found: Vec::new(),
        total: 0,
    });
    walk(root, files, || {
        let matcher = matcher.clone();
        let collected = &collected;
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(0))
            .line_number(true)
            .build();
        Box::new(move |path| {
            let mut here = Vec::new();
            let _ = searcher.search_path(
                &matcher,
                path,
                Lossy(|line, text| {
                    here.push(Found {
                        path: path.to_path_buf(),
                        line: Some(line),
                        text: cut(text.trim_end()),
                    });
                    Ok(true)
                }),
            );
            if !here.is_empty() {
                if let Ok(mut collected) = collected.lock() {
                    for found in here {
                        collected.add(found);
                    }
                }
            }
        })
    });
    let collected = collected.into_inner().unwrap_or_else(|e| e.into_inner());
    Ok((collected.found, collected.total))
}

/// Definitions named exactly `name`. Only files that contain the name are
/// parsed.
fn by_name(
    root: &Path,
    files: Option<&globset::GlobMatcher>,
    name: &str,
) -> Result<(Vec<Found>, usize), String> {
    let (candidates, _) = by_content(root, files, name, false, false)?;
    let mut paths: Vec<PathBuf> = candidates.into_iter().map(|found| found.path).collect();
    paths.sort();
    paths.dedup();
    let mut collected = Collected {
        found: Vec::new(),
        total: 0,
    };
    for path in paths {
        let Ok(Some(definitions)) = outline::file_definitions(&path) else {
            continue;
        };
        for definition in definitions.into_iter().filter(|d| d.name == name) {
            collected.add(Found {
                path: path.clone(),
                line: Some(definition.line as u64),
                text: definition.text,
            });
        }
    }
    Ok((collected.found, collected.total))
}

fn render(root: &Path, mut found: Vec<Found>, total: usize) -> String {
    if total == 0 {
        return "No results.".into();
    }
    found.sort();
    let lines: Vec<String> = found
        .iter()
        .take(SHOWN)
        .map(|found| {
            let path = found.path.strip_prefix(root).unwrap_or(&found.path);
            match found.line {
                Some(line) => format!("{}:{line}: {}", path.display(), found.text),
                None => path.display().to_string(),
            }
        })
        .collect();
    let mut answer = lines.join("\n");
    if total > SHOWN {
        answer.push_str(&format!(
            "\n… {SHOWN} of {total} results shown; narrow the search (pattern, files, or path) to see the rest …"
        ));
    } else {
        answer.push_str(&format!("\n({total} results)"));
    }
    answer
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

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn parse(text: &str) -> u32 {\n    helper(text)\n}\n\nfn helper(text: &str) -> u32 {\n    text.len() as u32\n}\n",
        )
        .expect("lib");
        std::fs::write(
            dir.path().join("src/main.rs"),
            "fn main() {\n    parse(\"x\");\n}\n",
        )
        .expect("main");
        std::fs::write(
            dir.path().join("README.md"),
            "# Parser\n\nCall parse to read.\n",
        )
        .expect("readme");
        std::fs::write(dir.path().join(".gitignore"), "target\n").expect("gitignore");
        std::fs::create_dir_all(dir.path().join("target")).expect("target");
        std::fs::write(dir.path().join("target/out.rs"), "fn parse() {}\n").expect("ignored");
        dir
    }

    fn find(dir: &Path, arguments: serde_json::Value) -> String {
        let arguments: Arguments = serde_json::from_value(arguments).expect("arguments");
        search(dir, &arguments).expect("search")
    }

    #[test]
    fn content_search_lists_matching_lines_sorted_and_skips_ignored_files() {
        let dir = project();
        let text = find(dir.path(), json!({"pattern": "parse"}));
        let separator = std::path::MAIN_SEPARATOR;
        assert_eq!(
            text,
            format!(
                "README.md:3: Call parse to read.\nsrc{separator}lib.rs:1: pub fn parse(text: &str) -> u32 {{\nsrc{separator}main.rs:2:     parse(\"x\");\n(3 results)"
            )
        );
        let text = find(
            dir.path(),
            json!({"pattern": "fn \\w+\\(text", "regex": true, "files": "*.rs"}),
        );
        assert!(text.ends_with("(2 results)"), "{text}");
        let text = find(
            dir.path(),
            json!({"pattern": "PARSE", "ignore_case": true, "files": "*.md"}),
        );
        assert!(
            text.starts_with(
                "README.md:1: # Parser
README.md:3:"
            ),
            "{text}"
        );
    }

    #[test]
    fn name_search_finds_only_definitions() {
        let dir = project();
        let text = find(dir.path(), json!({"name": "parse"}));
        let separator = std::path::MAIN_SEPARATOR;
        assert_eq!(
            text,
            format!("src{separator}lib.rs:1: pub fn parse(text: &str) -> u32 {{\n(1 results)")
        );
    }

    #[test]
    fn file_search_and_the_cap() {
        let dir = project();
        let text = find(dir.path(), json!({"files": "**/*.rs"}));
        assert!(text.ends_with("(2 results)"), "{text}");
        for n in 0..60 {
            std::fs::write(dir.path().join(format!("f{n}.txt")), "needle\n").expect("file");
        }
        let text = find(dir.path(), json!({"pattern": "needle"}));
        assert!(text.ends_with("… 50 of 60 results shown; narrow the search (pattern, files, or path) to see the rest …"), "{text}");
        assert_eq!(
            find(dir.path(), json!({"pattern": "absent"})),
            "No results."
        );
    }
}
