//! The `outline` tool: the definitions of a file or folder (functions,
//! types, methods, document headings) as their first lines with line
//! numbers, without bodies. Built with no model call: from syntax trees for
//! the languages tree-sitter covers here, and otherwise from the scopes of
//! syntax-highlighting grammars, which more grammars can extend without a
//! rebuild. Design, sources and the comparison of the two:
//! docs/evidence/repo-understanding.md.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::json;
use syntect::parsing::syntax_definition::Pattern;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};
use tree_sitter_tags::{TagsConfiguration, TagsContext};

use crate::conversation::ToolSpec;

pub(crate) const OUTLINE: &str = "outline";
/// Lines one answer shows at most.
const MAX_LINES: usize = 300;
const LINE_CHARS: usize = 200;
/// Files larger than this are skipped: generated or bundled code.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Highlighting scopes `entity.name.<kind>` that mark a definition;
/// `section` is a document heading.
const KINDS: [&str; 11] = [
    "function",
    "class",
    "type",
    "struct",
    "enum",
    "trait",
    "interface",
    "namespace",
    "impl",
    "module",
    "section",
];
/// Scopes under which a marked name is a use, or code quoted in a document,
/// rather than a definition.
const NOT_DEFINING: [&str; 6] = [
    "meta.function-call",
    "meta.type.annotation",
    "meta.return.type",
    "meta.type.parameters",
    "meta.type.tuple",
    "markup.raw",
];

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: OUTLINE.into(),
        description: "Show the definitions (functions, types, classes, methods) of a file, \
            or of every source file under a folder, as their first lines with line numbers \
            and without bodies. Use it to see what a file or folder contains before \
            reading; for documents, their headings. Works for about a hundred languages."
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

/// `grammars` is a folder of extra `.sublime-syntax` grammars, read once
/// per process.
pub(crate) async fn run(
    folder: Option<&Path>,
    grammars: PathBuf,
    arguments: &str,
) -> Result<String, String> {
    let arguments: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let path = resolve(folder, &arguments.path)?;
    tokio::task::spawn_blocking(move || {
        let _ = GRAMMARS.set(grammars);
        outline(&path)
    })
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
        .filter(|file| supported(file))
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

fn supported(path: &Path) -> bool {
    language(path).is_some() || grammar(path).is_some()
}

/// The definitions of a file; `None` when its language is not supported.
pub(crate) fn file_definitions(path: &Path) -> Result<Option<Vec<Definition>>, String> {
    if !supported(path) {
        return Ok(None);
    }
    let size = std::fs::metadata(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?
        .len();
    if size > MAX_FILE_BYTES {
        return Ok(Some(Vec::new()));
    }
    let source =
        std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if let Some(configuration) = language(path) {
        return Ok(Some(definitions(configuration, &source)));
    }
    Ok(grammar(path).map(|(set, syntax)| {
        highlighted_definitions(set, syntax, &String::from_utf8_lossy(&source))
    }))
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

/// A folder of extra `.sublime-syntax` grammars read with the bundled ones.
static GRAMMARS: OnceLock<PathBuf> = OnceLock::new();

struct Grammars {
    set: SyntaxSet,
    /// Grammars that mark at least one kind of definition; the others (data
    /// formats, plain text) are skipped rather than parsed for nothing.
    defining: HashSet<String>,
}

fn grammars() -> &'static Grammars {
    static LOADED: OnceLock<Grammars> = OnceLock::new();
    LOADED.get_or_init(|| {
        let mut builder = two_face::syntax::extra_newlines().into_builder();
        if let Some(folder) = GRAMMARS.get().filter(|folder| folder.is_dir()) {
            let _ = builder.add_from_folder(folder, true);
        }
        let defining = builder
            .syntaxes()
            .iter()
            .filter(|syntax| {
                syntax.contexts.values().any(|context| {
                    context
                        .meta_scope
                        .iter()
                        .chain(&context.meta_content_scope)
                        .any(|scope| defining_kind(*scope))
                        || context.patterns.iter().any(|pattern| match pattern {
                            Pattern::Match(found) => {
                                found.scope.iter().any(|scope| defining_kind(*scope))
                                    || found.captures.iter().flatten().any(|(_, scopes)| {
                                        scopes.iter().any(|scope| defining_kind(*scope))
                                    })
                            }
                            Pattern::Include(_) => false,
                        })
                })
            })
            .map(|syntax| syntax.name.clone())
            .collect();
        Grammars {
            set: builder.build(),
            defining,
        }
    })
}

fn defining_kind(scope: Scope) -> bool {
    kind(&scope.build_string()).is_some()
}

fn kind(scope: &str) -> Option<&'static str> {
    let first = scope.strip_prefix("entity.name.")?.split('.').next()?;
    KINDS.iter().find(|kind| **kind == first).copied()
}

/// The highlighting grammar for a file, when it marks definitions.
fn grammar(path: &Path) -> Option<(&'static SyntaxSet, &'static SyntaxReference)> {
    let extension = path.extension()?.to_str()?;
    let loaded = grammars();
    let syntax = loaded.set.find_syntax_by_extension(extension).or_else(|| {
        loaded
            .set
            .find_syntax_by_extension(&extension.to_ascii_lowercase())
    })?;
    loaded
        .defining
        .contains(&syntax.name)
        .then_some((&loaded.set, syntax))
}

/// Definitions from a highlighting grammar: the lines holding text scoped
/// `entity.name.<kind>`, outside the scopes where such a name is only used.
fn highlighted_definitions(
    set: &SyntaxSet,
    syntax: &SyntaxReference,
    source: &str,
) -> Vec<Definition> {
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut kinds: HashMap<Scope, bool> = HashMap::new();
    let mut ignored: HashMap<Scope, bool> = HashMap::new();
    let mut found = Vec::new();
    for (index, line) in syntect::util::LinesWithEndings::from(source).enumerate() {
        let Ok(operations) = state.parse_line(line, set) else {
            break;
        };
        let mut name = String::new();
        let mut position = 0;
        let mut operations = operations.into_iter().peekable();
        loop {
            let end = operations.peek().map_or(line.len(), |(offset, _)| *offset);
            if end > position {
                let scopes = stack.as_slice();
                let marked = scopes
                    .iter()
                    .any(|scope| *kinds.entry(*scope).or_insert_with(|| defining_kind(*scope)));
                let used = scopes.iter().any(|scope| {
                    *ignored.entry(*scope).or_insert_with(|| {
                        let text = scope.build_string();
                        NOT_DEFINING.iter().any(|prefix| text.starts_with(prefix))
                    })
                });
                if marked && !used {
                    name.push_str(&line[position..end]);
                }
                position = end;
            }
            match operations.next() {
                Some((_, operation)) => {
                    let _ = stack.apply(&operation);
                }
                None => break,
            }
        }
        let name = name.trim();
        if !name.is_empty() {
            found.push(Definition {
                name: name.to_string(),
                line: index + 1,
                text: cut(line.trim_end()),
            });
        }
    }
    found
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
    fn other_languages_and_documents_use_highlighting_grammars() {
        let dir = tempfile::tempdir().expect("dir");
        let ruby = dir.path().join("shape.rb");
        std::fs::write(
            &ruby,
            "class Shape\n  def area\n    0\n  end\nend\n\nputs Shape.new.area\n",
        )
        .expect("ruby");
        let shown: Vec<String> = file_definitions(&ruby)
            .expect("read")
            .expect("supported")
            .iter()
            .map(render)
            .collect();
        assert_eq!(shown, ["  1: class Shape", "  2:   def area"]);

        let markdown = dir.path().join("guide.md");
        std::fs::write(
            &markdown,
            "# Guide\n\nText.\n\n## Install\n\n```rust\nfn hidden() {}\n```\n",
        )
        .expect("markdown");
        let shown: Vec<String> = file_definitions(&markdown)
            .expect("read")
            .expect("supported")
            .iter()
            .map(render)
            .collect();
        assert_eq!(shown, ["  1: # Guide", "  5: ## Install"]);

        let data = dir.path().join("data.json");
        std::fs::write(&data, "{\"a\": 1}").expect("json");
        assert_eq!(file_definitions(&data).expect("read"), None);
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
