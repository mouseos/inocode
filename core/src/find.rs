//! The `find` tool: search file contents (text or a regular expression),
//! file names (globs), or definitions by name, over the project with no
//! index, so every answer reflects the files as they are now. Answers are
//! matching lines as `path:line: text`, capped, with the total; a content
//! match also names the definition it lies in. Design, sources and the
//! benchmarks: docs/evidence/harness-tools.md,
//! docs/evidence/repo-understanding.md and docs/evidence/find-enclosing.md.

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
            the search when there are more. A matching line inside a function, method or \
            type shows its name as path:line (in name): text. Respects .gitignore; hidden \
            files and folders are searched, the .git folder is not."
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
    /// The innermost definition containing a content match.
    within: Option<String>,
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
        (_, Some(pattern)) if !pattern.is_empty() => {
            let (mut found, total) = by_content(
                root,
                files.as_ref(),
                pattern,
                arguments.regex,
                arguments.ignore_case,
            )?;
            found.sort();
            found.truncate(SHOWN);
            name_enclosing(&mut found);
            (found, total)
        }
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
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != ".git")
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
                    within: None,
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
                        within: None,
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
                within: None,
            });
        }
    }
    Ok((collected.found, collected.total))
}

/// Sets `within` on each shown match from its file's definitions; files in
/// a language with no known definitions are left as they are.
fn name_enclosing(found: &mut [Found]) {
    let mut start = 0;
    while start < found.len() {
        let path = found[start].path.clone();
        let end = start + found[start..].iter().take_while(|f| f.path == path).count();
        if let Ok(Some(definitions)) = outline::file_definitions(&path) {
            for found in &mut found[start..end] {
                let Some(line) = found.line.map(|line| line as usize) else {
                    continue;
                };
                found.within = definitions
                    .iter()
                    .filter(|d| d.line <= line && line <= d.end)
                    .max_by_key(|d| d.line)
                    .map(|d| d.name.clone());
            }
        }
        start = end;
    }
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
            match (found.line, &found.within) {
                (Some(line), Some(within)) => {
                    format!("{}:{line} (in {within}): {}", path.display(), found.text)
                }
                (Some(line), None) => format!("{}:{line}: {}", path.display(), found.text),
                (None, _) => path.display().to_string(),
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

/// Up to `limit` files under `root` whose relative path (with `/`) holds
/// the letters of `query` in order, ignoring case and spaces, best first,
/// with their scores; every file, shortest path first, for an empty query.
/// Files `.gitignore` leaves out are not listed, as [`walk`] lists them.
pub(crate) fn fuzzy_paths(root: &Path, query: &str, limit: usize) -> Vec<(String, i64)> {
    let query: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    let scored = Mutex::new(Vec::new());
    walk(root, None, || {
        let (scored, query) = (&scored, &query);
        Box::new(move |path| {
            let relative = path.strip_prefix(root).unwrap_or(path);
            let relative = relative.to_string_lossy().replace('\\', "/");
            if let Some(score) = fuzzy_score(&relative, query) {
                if let Ok(mut scored) = scored.lock() {
                    scored.push((relative, score));
                }
            }
        })
    });
    let mut scored = scored.into_inner().unwrap_or_else(|e| e.into_inner());
    scored.sort_by(|(a, a_score), (b, b_score)| {
        b_score
            .cmp(a_score)
            .then(a.len().cmp(&b.len()))
            .then(a.cmp(b))
    });
    scored.truncate(limit);
    scored
}

/// How well `query` (lower case) matches `path`, or `None` when its
/// letters are not all in `path` in order. Each matched letter counts, more
/// when it follows the previous match, starts a word (after `/`, `_`, `-`,
/// `.`, a space or a lower-case letter before an upper-case one) or lies in
/// the file's name; letters skipped between the first and last match cost
/// a little. The best of the matches starting at each place is taken.
fn fuzzy_score(path: &str, query: &[char]) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let chars: Vec<char> = path.chars().collect();
    let name_start = chars.iter().rposition(|&c| c == '/').map_or(0, |i| i + 1);
    let lower: Vec<char> = chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let starts_word = |i: usize| {
        i == 0
            || matches!(chars[i - 1], '/' | '_' | '-' | '.' | ' ')
            || (chars[i - 1].is_lowercase() && chars[i].is_uppercase())
    };
    let mut best = None;
    for start in (0..lower.len()).filter(|&i| lower[i] == query[0]) {
        let mut score = 0i64;
        let mut previous: Option<usize> = None;
        let mut at = start;
        let mut matched = 0;
        while matched < query.len() && at < lower.len() {
            if lower[at] == query[matched] {
                score += 16;
                if previous == Some(at.wrapping_sub(1)) {
                    score += 8;
                }
                if starts_word(at) {
                    score += 8;
                }
                if at >= name_start {
                    score += 4;
                }
                if let Some(previous) = previous {
                    score -= (at - previous - 1) as i64;
                }
                previous = Some(at);
                matched += 1;
            }
            at += 1;
        }
        if matched == query.len() {
            best = Some(best.map_or(score, |best: i64| best.max(score)));
        }
    }
    best
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
    fn paths_match_fuzzily_best_first_without_ignored_files() {
        let dir = project();
        let found = fuzzy_paths(dir.path(), "src MAIN", 20);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, "src/main.rs");
        // Both have the letters; the file name that starts with them wins.
        let found = fuzzy_paths(dir.path(), "rs", 20);
        let paths: Vec<&str> = found.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, ["src/lib.rs", "src/main.rs"]);
        assert!(found[0].1 >= found[1].1);
        assert!(fuzzy_paths(dir.path(), "out", 20).is_empty());
        assert_eq!(fuzzy_paths(dir.path(), "", 2).len(), 2);
    }

    #[test]
    fn content_search_lists_matching_lines_sorted_and_skips_ignored_files() {
        let dir = project();
        let text = find(dir.path(), json!({"pattern": "parse"}));
        let separator = std::path::MAIN_SEPARATOR;
        assert_eq!(
            text,
            format!(
                "README.md:3 (in Parser): Call parse to read.\nsrc{separator}lib.rs:1 (in parse): pub fn parse(text: &str) -> u32 {{\nsrc{separator}main.rs:2 (in main):     parse(\"x\");\n(3 results)"
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
                "README.md:1 (in Parser): # Parser
README.md:3 (in Parser):"
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
    fn hidden_files_are_searched_and_the_git_folder_is_not() {
        let dir = project();
        std::fs::create_dir_all(dir.path().join(".github/workflows")).expect("hidden");
        std::fs::write(
            dir.path().join(".github/workflows/ci.yml"),
            "run: hidden-needle\n",
        )
        .expect("workflow");
        std::fs::create_dir_all(dir.path().join(".git")).expect("git");
        std::fs::write(dir.path().join(".git/config"), "hidden-needle\n").expect("config");
        let text = find(dir.path(), json!({"pattern": "hidden-needle"}));
        assert!(
            text.contains("ci.yml:1") && text.ends_with("(1 results)"),
            "{text}"
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
