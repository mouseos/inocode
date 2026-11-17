//! The `outline` tool: the definitions of a file or folder (functions,
//! types, methods) as their first lines with line numbers, without bodies.
//! Built from syntax trees with no model call. Design and sources:
//! docs/evidence/repo-understanding.md.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::json;
use tree_sitter_tags::{TagsConfiguration, TagsContext};

use crate::conversation::ToolSpec;

pub(crate) const OUTLINE: &str = "outline";
/// Lines one answer shows at most.
const MAX_LINES: usize = 300;
const LINE_CHARS: usize = 200;
/// Files larger than this are skipped: generated or bundled code.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: OUTLINE.into(),
        description: "Show the definitions (functions, types, classes, methods) of a file, \
            or of every source file under a folder, as their first lines with line numbers \
            and without bodies. Use it to see what a file or folder contains before \
            reading. Languages: Rust, Python, JavaScript, TypeScript, Go, Java, C, C++."
            .into(),
        parameters: json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {"type": "string", "description": "A file or folder; relative paths start at the project folder."}
            }
        }),
    }
}

#[derive(Deserialize)]
struct Arguments {
    path: String,
}

/// One definition: where it starts and the text of that line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Definition {
    pub name: String,
    pub line: usize,
    pub text: String,
}

pub(crate) async fn run(folder: Option<&Path>, arguments: &str) -> Result<String, String> {
    let arguments: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let path = resolve(folder, &arguments.path)?;
    tokio::task::spawn_blocking(move || outline(&path))
        .await
        .map_err(|error| format!("outline failed: {error}"))?
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

fn outline(path: &Path) -> Result<String, String> {
    if path.is_file() {
        let definitions = file_definitions(path)?
            .ok_or_else(|| format!("{} is not in a supported language", path.display()))?;
        if definitions.is_empty() {
            return Ok(format!("{} has no definitions.", path.display()));
        }
        let mut lines = vec![path.display().to_string()];
        lines.extend(definitions.iter().map(render));
        return Ok(capped(lines, path));
    }
    if !path.is_dir() {
        return Err(format!("{} does not exist", path.display()));
    }
    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(path)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .map(|entry| entry.into_path())
        .filter(|file| language(file).is_some())
        .collect();
    files.sort();
    let mut lines = Vec::new();
    for file in files {
        let Ok(Some(definitions)) = file_definitions(&file) else {
            continue;
        };
        if definitions.is_empty() {
            continue;
        }
        let shown = file.strip_prefix(path).unwrap_or(&file);
        lines.push(shown.display().to_string());
        lines.extend(definitions.iter().map(render));
        if lines.len() > MAX_LINES {
            break;
        }
    }
    if lines.is_empty() {
        return Ok(format!(
            "No definitions in supported languages under {}.",
            path.display()
        ));
    }
    Ok(capped(lines, path))
}

fn render(definition: &Definition) -> String {
    format!("  {}: {}", definition.line, definition.text)
}

fn capped(lines: Vec<String>, path: &Path) -> String {
    if lines.len() <= MAX_LINES {
        return lines.join("\n");
    }
    format!(
        "{}\n… more definitions not shown; outline a file or a smaller folder under {} …",
        lines[..MAX_LINES].join("\n"),
        path.display()
    )
}

/// The definitions of a file; `None` when its language is not supported.
pub(crate) fn file_definitions(path: &Path) -> Result<Option<Vec<Definition>>, String> {
    let Some(configuration) = language(path) else {
        return Ok(None);
    };
    let size = std::fs::metadata(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?
        .len();
    if size > MAX_FILE_BYTES {
        return Ok(Some(Vec::new()));
    }
    let source =
        std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    Ok(Some(definitions(configuration, &source)))
}

fn definitions(configuration: &TagsConfiguration, source: &[u8]) -> Vec<Definition> {
    let mut context = TagsContext::new();
    let Ok((tags, _)) = context.generate_tags(configuration, source, None) else {
        return Vec::new();
    };
    let mut found: Vec<Definition> = tags
        .filter_map(Result::ok)
        .filter(|tag| tag.is_definition)
        .map(|tag| {
            // The whole line, so nesting shows as the source's indentation.
            let start = source[..tag.range.start]
                .iter()
                .rposition(|&byte| byte == b'\n')
                .map_or(0, |newline| newline + 1);
            let end = source[start..]
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or(source.len(), |newline| start + newline);
            let text = String::from_utf8_lossy(&source[start..end]);
            Definition {
                name: String::from_utf8_lossy(&source[tag.name_range.clone()]).into_owned(),
                line: tag.span.start.row + 1,
                text: cut(text.trim_end()),
            }
        })
        .collect();
    found.sort_by_key(|definition| definition.line);
    found.dedup_by(|a, b| a.line == b.line && a.name == b.name);
    found
}

fn cut(text: &str) -> String {
    match text.char_indices().nth(LINE_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// The tag configuration for a file's language, chosen by its extension.
fn language(path: &Path) -> Option<&'static TagsConfiguration> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let index = match extension.as_str() {
        "rs" => 0,
        "py" | "pyi" => 1,
        "js" | "mjs" | "cjs" | "jsx" => 2,
        "ts" | "mts" | "cts" => 3,
        "tsx" => 4,
        "go" => 5,
        "java" => 6,
        "c" | "h" => 7,
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => 8,
        _ => return None,
    };
    configurations()[index].as_ref()
}

fn configurations() -> &'static [Option<TagsConfiguration>; 9] {
    static CONFIGURATIONS: OnceLock<[Option<TagsConfiguration>; 9]> = OnceLock::new();
    CONFIGURATIONS.get_or_init(|| {
        let typescript_tags = format!(
            "{}\n{}",
            tree_sitter_javascript::TAGS_QUERY,
            tree_sitter_typescript::TAGS_QUERY
        );
        let make = |language: tree_sitter_language::LanguageFn, tags: &str| {
            TagsConfiguration::new(language.into(), tags, "").ok()
        };
        [
            make(tree_sitter_rust::LANGUAGE, tree_sitter_rust::TAGS_QUERY),
            make(tree_sitter_python::LANGUAGE, tree_sitter_python::TAGS_QUERY),
            make(
                tree_sitter_javascript::LANGUAGE,
                tree_sitter_javascript::TAGS_QUERY,
            ),
            make(
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
                &typescript_tags,
            )
            .or_else(|| {
                make(
                    tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
                    tree_sitter_typescript::TAGS_QUERY,
                )
            }),
            make(tree_sitter_typescript::LANGUAGE_TSX, &typescript_tags).or_else(|| {
                make(
                    tree_sitter_typescript::LANGUAGE_TSX,
                    tree_sitter_typescript::TAGS_QUERY,
                )
            }),
            make(tree_sitter_go::LANGUAGE, tree_sitter_go::TAGS_QUERY),
            make(tree_sitter_java::LANGUAGE, tree_sitter_java::TAGS_QUERY),
            make(tree_sitter_c::LANGUAGE, tree_sitter_c::TAGS_QUERY),
            make(tree_sitter_cpp::LANGUAGE, tree_sitter_cpp::TAGS_QUERY),
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(extension: &str, source: &str) -> Vec<String> {
        let path = PathBuf::from(format!("x.{extension}"));
        definitions(language(&path).expect("supported"), source.as_bytes())
            .iter()
            .map(render)
            .collect()
    }

    #[test]
    fn every_language_is_configured() {
        assert!(configurations().iter().all(Option::is_some));
    }

    #[test]
    fn definitions_are_first_lines_with_numbers() {
        let rust = "struct Point {\n    x: i32,\n}\n\nimpl Point {\n    fn norm(&self) -> i32 {\n        self.x\n    }\n}\n";
        assert_eq!(
            shown("rs", rust),
            ["  1: struct Point {", "  6:     fn norm(&self) -> i32 {"]
        );
        let python =
            "class Shape:\n    def area(self):\n        return 0\n\ndef main():\n    pass\n";
        assert_eq!(
            shown("py", python),
            [
                "  1: class Shape:",
                "  2:     def area(self):",
                "  5: def main():"
            ]
        );
        let typescript = "export function load(path: string): string {\n  return path;\n}\ninterface Shape { area(): number }\n";
        let lines = shown("ts", typescript);
        assert!(
            lines.contains(&"  1: export function load(path: string): string {".to_string()),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("  4: interface Shape")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_folder_outline_lists_files_and_skips_other_languages() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir(dir.path().join("src")).expect("src");
        std::fs::write(dir.path().join("src/a.rs"), "fn alpha() {}\n").expect("a");
        std::fs::write(dir.path().join("notes.txt"), "fn not_code() {}\n").expect("notes");
        let text = outline(dir.path()).expect("outline");
        let expected = format!("src{}a.rs\n  1: fn alpha() {{}}", std::path::MAIN_SEPARATOR);
        assert_eq!(text, expected);
    }
}
