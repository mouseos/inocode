//! Plugins of the Claude and Codex catalogs: the model finds them with
//! `plugin_search` and installs one with `plugin_install` once the person
//! approves; an installed plugin brings its skills and MCP servers. Design
//! and sources: docs/evidence/plugin-mechanism.md.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::conversation::ToolSpec;
use crate::store::Store;

pub(crate) const SEARCH: &str = "plugin_search";
pub(crate) const INSTALL: &str = "plugin_install";
/// How many plugins a search returns unless the model asks for another
/// number.
const SEARCH_LIMIT: usize = 8;
/// How long one git command may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(300);
/// Characters of a skill's description shown to the model.
const DESCRIPTION_CHARS: usize = 500;

/// A catalog of plugins: a git repository, the ref to take, and where its
/// list of plugins is in it.
pub(crate) struct Catalog {
    pub folder: &'static str,
    pub repository: String,
    pub reference: String,
    pub file: &'static str,
}

/// The official catalogs of Claude Code and Codex.
fn official() -> Vec<Catalog> {
    vec![
        Catalog {
            folder: "claude",
            repository: "https://github.com/anthropics/claude-plugins-official.git".into(),
            reference: "HEAD".into(),
            file: ".claude-plugin/marketplace.json",
        },
        Catalog {
            folder: "codex",
            repository: "https://github.com/openai/plugins.git".into(),
            reference: "HEAD".into(),
            file: ".agents/plugins/marketplace.json",
        },
    ]
}

/// Where plugins live: beside the thread store.
pub(crate) fn folder(store: &Store) -> PathBuf {
    store.files_folder().with_file_name("plugins")
}

pub(crate) fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: SEARCH.into(),
            description: "Search the plugin catalogs of Claude Code and Codex for a plugin \
                that adds what the task needs (skills, MCP servers). Returns plugins as \
                name@catalog with their description and whether they are installed."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["query"],
                "additionalProperties": false,
                "properties": {
                    "query": {"type": "string", "description": "Words describing the plugin."},
                    "limit": {"type": "integer", "minimum": 1}
                }
            }),
        },
        ToolSpec {
            name: INSTALL.into(),
            description: "Install a plugin found with plugin_search. The person is asked \
                first. Its skills are listed with your next request and its MCP servers' \
                tools are available from then on. Install only a plugin the task needs."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["plugin"],
                "additionalProperties": false,
                "properties": {
                    "plugin": {"type": "string", "description": "name@catalog, as plugin_search returned it."}
                }
            }),
        },
    ]
}

pub(crate) async fn search(store: &Store, arguments: &str) -> Result<String, String> {
    search_in(&folder(store), &official(), arguments).await
}

pub(crate) async fn install(store: &Store, arguments: &str) -> Result<String, String> {
    let answer = install_from(&folder(store), &official(), arguments).await?;
    store.forget_mcp().await;
    Ok(answer)
}

/// One plugin a catalog lists.
struct Entry {
    name: String,
    catalog: String,
    description: String,
    category: String,
    source: Value,
    /// The catalog's checkout, which relative sources are in.
    checkout: PathBuf,
}

async fn search_in(root: &Path, catalogs: &[Catalog], arguments: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Arguments {
        query: String,
        limit: Option<usize>,
    }
    let arguments: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let query = crate::mcp::words(&arguments.query);
    if query.is_empty() {
        return Err("query must not be empty".into());
    }
    let limit = arguments.limit.unwrap_or(SEARCH_LIMIT);
    if limit == 0 {
        return Err("limit must be greater than zero".into());
    }
    let (entries, unavailable) = entries(root, catalogs).await;
    let documents: Vec<Vec<String>> = entries
        .iter()
        .map(|entry| {
            crate::mcp::words(&format!(
                "{} {} {}",
                entry.name, entry.description, entry.category
            ))
        })
        .collect();
    let plugins: Vec<Value> = crate::mcp::bm25(&documents, &query)
        .into_iter()
        .take(limit)
        .map(|index| {
            let entry = &entries[index];
            json!({
                "plugin": format!("{}@{}", entry.name, entry.catalog),
                "description": entry.description,
                "category": entry.category,
                "installed": installed_at(root, &entry.catalog, &entry.name).is_dir(),
            })
        })
        .collect();
    let mut answer = json!({"plugins": plugins});
    if !unavailable.is_empty() {
        answer["unavailable_catalogs"] = json!(unavailable);
    }
    Ok(answer.to_string())
}

async fn install_from(
    root: &Path,
    catalogs: &[Catalog],
    arguments: &str,
) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Arguments {
        plugin: String,
    }
    let arguments: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let (name, catalog) = arguments
        .plugin
        .rsplit_once('@')
        .ok_or("give the plugin as name@catalog")?;
    let (entries, unavailable) = entries(root, catalogs).await;
    let entry = entries
        .iter()
        .find(|entry| entry.name == name && entry.catalog == catalog)
        .ok_or_else(|| match unavailable.is_empty() {
            true => format!("no plugin {name} in catalog {catalog}"),
            false => format!(
                "no plugin {name} in catalog {catalog}; unavailable: {}",
                unavailable.join("; ")
            ),
        })?;
    let target = installed_at(root, &entry.catalog, &entry.name);
    let staging = target.with_file_name(format!(".{}.partial", entry.name));
    let previewed = preview_at(root, entry);
    let _ = std::fs::remove_dir_all(&staging);
    if let Some(parent) = staging.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot make {}: {error}", parent.display()))?;
    }
    // What the person was shown when they approved is what is installed.
    let copied = match previewed.is_dir() {
        true => std::fs::rename(&previewed, &staging)
            .map_err(|error| format!("cannot take the shown plugin: {error}")),
        false => stage(entry, root, &staging).await,
    };
    if let Err(error) = copied {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    replace(&staging, &target)?;

    let plugin = Installed::at(entry.name.clone(), entry.catalog.clone(), target.clone());
    let skills: Vec<String> = plugin
        .skills()
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    let servers: Vec<String> = plugin
        .mcp_servers(&[], false)
        .into_iter()
        .map(|server| server.name)
        .collect();
    let mut answer = format!(
        "Installed {}@{} in {}.",
        entry.name,
        entry.catalog,
        target.display()
    );
    answer.push_str(&match skills.is_empty() {
        true => " It has no skills.".to_string(),
        false => format!(
            " Skills, listed with your next request: {}.",
            skills.join(", ")
        ),
    });
    answer.push_str(&match servers.is_empty() {
        true => " It has no MCP servers.".to_string(),
        false => format!(
            " MCP servers, whose tools are available from your next request: {}.",
            servers.join(", ")
        ),
    });
    let unused: Vec<&str> = ["agents", "hooks"]
        .into_iter()
        .filter(|part| target.join(part).exists())
        .collect();
    if !unused.is_empty() {
        answer.push_str(&format!(" Not used here: {}.", unused.join(", ")));
    }
    Ok(answer)
}

/// Copies the plugin's files into `staging`.
async fn stage(entry: &Entry, root: &Path, staging: &Path) -> Result<(), String> {
    let download = root
        .join("downloads")
        .join(format!("{}-{}", entry.catalog, entry.name));
    let copied = fetch_source(entry, &download).await.and_then(|source| {
        copy_folder(&source, staging).map_err(|error| format!("cannot copy the plugin: {error}"))
    });
    let _ = std::fs::remove_dir_all(&download);
    copied
}

/// Puts `staging` in place of `target`; the plugin installed before stays
/// when that fails.
fn replace(staging: &Path, target: &Path) -> Result<(), String> {
    let old = target.with_file_name(format!(
        ".{}.old",
        target.file_name().unwrap_or_default().to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&old);
    let had = target.exists();
    if had {
        std::fs::rename(target, &old)
            .map_err(|error| format!("cannot move the installed plugin aside: {error}"))?;
    }
    if let Err(error) = std::fs::rename(staging, target) {
        if had {
            let _ = std::fs::rename(&old, target);
        }
        let _ = std::fs::remove_dir_all(staging);
        return Err(format!("cannot put the plugin in place: {error}"));
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// Where a plugin shown for approval waits until it is installed.
fn preview_at(root: &Path, entry: &Entry) -> PathBuf {
    root.join("previews")
        .join(format!("{}-{}", entry.catalog, entry.name))
}

/// What the person is shown before approving `plugin_install`: the
/// commands its MCP servers run, the addresses they reach, and the
/// variables of the environment they ask for. The plugin is fetched for
/// this and kept, so that the approved files are the ones installed.
pub(crate) async fn preview(store: &Store, arguments: &str) -> String {
    preview_in(&folder(store), &official(), arguments)
        .await
        .unwrap_or_else(|error| format!(". Its contents could not be shown: {error}"))
}

async fn preview_in(root: &Path, catalogs: &[Catalog], arguments: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Arguments {
        plugin: String,
    }
    let arguments: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let (name, catalog) = arguments
        .plugin
        .rsplit_once('@')
        .ok_or("give the plugin as name@catalog")?;
    let (entries, _) = entries(root, catalogs).await;
    let entry = entries
        .iter()
        .find(|entry| entry.name == name && entry.catalog == catalog)
        .ok_or_else(|| format!("no plugin {name} in catalog {catalog}"))?;
    let previewed = preview_at(root, entry);
    let _ = std::fs::remove_dir_all(&previewed);
    if let Err(error) = stage(entry, root, &previewed).await {
        let _ = std::fs::remove_dir_all(&previewed);
        return Err(error);
    }
    let plugin = Installed::at(entry.name.clone(), entry.catalog.clone(), previewed);
    Ok(describe(&plugin))
}

/// The servers of `plugin` as the person reads them before approving.
fn describe(plugin: &Installed) -> String {
    let mut lines = Vec::new();
    for (name, settings) in plugin.listed() {
        let text = |key: &str| settings.get(key).and_then(Value::as_str);
        let mut line = match (text("command"), text("url")) {
            (Some(command), _) => {
                let args: Vec<&str> = settings
                    .get("args")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                format!(
                    "MCP server {name} runs `{}` outside the sandbox",
                    std::iter::once(command)
                        .chain(args)
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            }
            (None, Some(url)) => format!(
                "MCP server {name} connects to {}",
                reqwest::Url::parse(url)
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_string))
                    .unwrap_or_else(|| url.to_string())
            ),
            (None, None) => format!("MCP server {name} names neither a command nor an address"),
        };
        let mut variables = Vec::new();
        variables_in(&settings, &mut variables);
        variables.retain(|variable| !ROOT_VARIABLES.contains(&variable.as_str()));
        variables.sort();
        variables.dedup();
        if !variables.is_empty() {
            line.push_str(&format!(
                " and asks for the variables {} (filled only when listed in pluginVariables of mcp.json)",
                variables.join(", ")
            ));
        }
        lines.push(line);
    }
    let unused: Vec<&str> = ["agents", "hooks"]
        .into_iter()
        .filter(|part| plugin.root.join(part).exists())
        .collect();
    if !unused.is_empty() {
        lines.push(format!("not used here: {}", unused.join(", ")));
    }
    match lines.is_empty() {
        true => ". It has no MCP servers".to_string(),
        false => format!(". It brings: {}", lines.join("; ")),
    }
}

/// The names of the `${NAME}`s in `value`.
fn variables_in(value: &Value, names: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            let mut rest = text.as_str();
            while let Some(start) = rest.find("${") {
                let Some(length) = rest[start..].find('}') else {
                    break;
                };
                names.push(rest[start + 2..start + length].to_string());
                rest = &rest[start + length + 1..];
            }
        }
        Value::Array(items) => items.iter().for_each(|item| variables_in(item, names)),
        Value::Object(fields) => fields.values().for_each(|item| variables_in(item, names)),
        _ => {}
    }
}

/// The folder of the plugin's own files: from the catalog's checkout, or
/// fetched with git into `download`.
async fn fetch_source(entry: &Entry, download: &Path) -> Result<PathBuf, String> {
    let field = |key: &str| entry.source.get(key).and_then(Value::as_str);
    let reference = || field("sha").or(field("ref")).unwrap_or("HEAD").to_string();
    let repository = |url: &str| match url.contains("://") || url.starts_with("git@") {
        true => url.to_string(),
        false => format!("https://github.com/{url}.git"),
    };
    match (&entry.source, field("source")) {
        (Value::String(path), _) => inside(&entry.checkout, path),
        (_, Some("local")) => inside(
            &entry.checkout,
            field("path").ok_or("the source has no path")?,
        ),
        (_, Some("url")) => {
            let url = field("url").ok_or("the source has no url")?;
            fetch(&repository(url), &reference(), download).await?;
            Ok(download.to_path_buf())
        }
        (_, Some("github")) => {
            let repo = field("repo").ok_or("the source has no repo")?;
            fetch(&repository(repo), &reference(), download).await?;
            Ok(download.to_path_buf())
        }
        (_, Some("git-subdir")) => {
            let url = field("url").ok_or("the source has no url")?;
            fetch(&repository(url), &reference(), download).await?;
            inside(download, field("path").ok_or("the source has no path")?)
        }
        (_, kind) => Err(format!(
            "plugins from a {} source are not installed here",
            kind.unwrap_or("unknown")
        )),
    }
}

/// The plugins of every catalog, each catalog fetched once per process, and
/// the catalogs that could not be read.
async fn entries(root: &Path, catalogs: &[Catalog]) -> (Vec<Entry>, Vec<String>) {
    static FETCHED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    static FETCHING: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _fetching = FETCHING
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let mut entries = Vec::new();
    let mut unavailable = Vec::new();
    for catalog in catalogs {
        let checkout = root.join("catalogs").join(catalog.folder);
        let fetched = FETCHED.lock().expect("fetched lock").contains(&checkout);
        if !fetched {
            match fetch(&catalog.repository, &catalog.reference, &checkout).await {
                Ok(()) => FETCHED.lock().expect("fetched lock").push(checkout.clone()),
                Err(error) => unavailable.push(format!("{}: {error}", catalog.repository)),
            }
        }
        match read_catalog(&checkout, catalog.file) {
            Ok(listed) => entries.extend(listed),
            Err(error) if fetched || unavailable.is_empty() => {
                unavailable.push(format!("{}: {error}", catalog.repository));
            }
            Err(_) => {}
        }
    }
    (entries, unavailable)
}

fn read_catalog(checkout: &Path, file: &str) -> Result<Vec<Entry>, String> {
    let text = std::fs::read_to_string(checkout.join(file))
        .map_err(|error| format!("cannot read {file}: {error}"))?;
    let listed: Value =
        serde_json::from_str(&text).map_err(|error| format!("{file} is not valid: {error}"))?;
    let catalog = listed
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| plain(name))
        .ok_or_else(|| format!("{file} has no usable name"))?
        .to_string();
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    Ok(listed
        .get("plugins")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|plugin| {
            let name = plugin
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| plain(name))?;
            let source = plugin.get("source")?.clone();
            let mut description = text(plugin, "description");
            if description.is_empty() {
                // Codex lists the description in the plugin's own manifest.
                let path = match &source {
                    Value::String(path) => Some(path.as_str()),
                    source => source.get("path").and_then(Value::as_str),
                };
                if let Some(folder) = path.and_then(|path| inside(checkout, path).ok()) {
                    let manifest = manifest(&folder);
                    description = text(&manifest, "description");
                    if description.is_empty() {
                        description = manifest
                            .pointer("/interface/shortDescription")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                    }
                }
            }
            Some(Entry {
                name: name.to_string(),
                catalog: catalog.clone(),
                description,
                category: text(plugin, "category"),
                source,
                checkout: checkout.to_path_buf(),
            })
        })
        .collect())
}

/// Takes `reference` of `repository` into `folder` with git.
async fn fetch(repository: &str, reference: &str, folder: &Path) -> Result<(), String> {
    std::fs::create_dir_all(folder)
        .map_err(|error| format!("cannot make {}: {error}", folder.display()))?;
    if !folder.join(".git").exists() {
        git(folder, &["init", "-q"]).await?;
    }
    git(
        folder,
        &["fetch", "-q", "--depth", "1", repository, reference],
    )
    .await?;
    git(folder, &["checkout", "-q", "--force", "FETCH_HEAD"]).await
}

async fn git(folder: &Path, arguments: &[&str]) -> Result<(), String> {
    let run = tokio::process::Command::new("git")
        .arg("-C")
        .arg(folder)
        .args(arguments)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(GIT_TIMEOUT, run)
        .await
        .map_err(|_| format!("git {} did not finish within {GIT_TIMEOUT:?}", arguments[0]))?
        .map_err(|error| format!("cannot run git: {error}"))?;
    match output.status.success() {
        true => Ok(()),
        false => Err(format!(
            "git {} failed: {}",
            arguments[0],
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Copies a folder without its `.git` and without links.
fn copy_folder(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for item in std::fs::read_dir(from)? {
        let item = item?;
        let kind = item.file_type()?;
        let target = to.join(item.file_name());
        if kind.is_dir() && item.file_name() != ".git" {
            copy_folder(&item.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(item.path(), target)?;
        }
    }
    Ok(())
}

/// `path` under `base`, refused when it would lead out of it.
fn inside(base: &Path, path: &str) -> Result<PathBuf, String> {
    let mut joined = base.to_path_buf();
    for part in Path::new(path).components() {
        match part {
            Component::Normal(part) => joined.push(part),
            Component::CurDir => {}
            _ => {
                return Err(format!(
                    "the plugin path {path} leads outside its repository"
                ))
            }
        }
    }
    Ok(joined)
}

/// Whether a name from a catalog can be a folder name.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn installed_at(root: &Path, catalog: &str, name: &str) -> PathBuf {
    root.join("installed").join(catalog).join(name)
}

/// The plugins turned off, as `name@catalog`; they stay installed
/// (docs/evidence/plugin-management.md).
const DISABLED_FILE: &str = "disabled.json";

fn disabled(root: &Path) -> std::collections::BTreeSet<String> {
    std::fs::read_to_string(root.join(DISABLED_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_disabled(root: &Path, ids: &std::collections::BTreeSet<String>) -> Result<(), String> {
    std::fs::create_dir_all(root)
        .map_err(|error| format!("cannot make {}: {error}", root.display()))?;
    let path = root.join(DISABLED_FILE);
    let partial = root.join(format!(".{DISABLED_FILE}.partial"));
    let text = serde_json::to_string_pretty(ids).map_err(|error| error.to_string())?;
    std::fs::write(&partial, text)
        .and_then(|()| std::fs::rename(&partial, &path))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// `name@catalog`, split, for an installed plugin.
fn installed_id<'a>(root: &Path, plugin: &'a str) -> Result<(&'a str, &'a str), String> {
    let (name, catalog) = plugin
        .rsplit_once('@')
        .filter(|(name, catalog)| plain(name) && plain(catalog))
        .ok_or("give the plugin as name@catalog")?;
    if !installed_at(root, catalog, name).is_dir() {
        return Err(format!("{plugin} is not installed"));
    }
    Ok((name, catalog))
}

/// Deletes an installed plugin's files; it is no longer listed or turned off.
pub(crate) async fn remove(store: &Store, plugin: &str) -> Result<(), String> {
    let root = folder(store);
    let (name, catalog) = installed_id(&root, plugin)?;
    let target = installed_at(&root, catalog, name);
    let id = plugin.to_string();
    tokio::task::spawn_blocking(move || {
        std::fs::remove_dir_all(&target)
            .map_err(|error| format!("cannot delete {}: {error}", target.display()))?;
        let mut off = disabled(&root);
        if off.remove(&id) {
            write_disabled(&root, &off)?;
        }
        Ok::<(), String>(())
    })
    .await
    .map_err(|error| error.to_string())??;
    store.forget_mcp().await;
    Ok(())
}

/// Turns an installed plugin on or off; `false` when it already was.
pub(crate) async fn set_enabled(
    store: &Store,
    plugin: &str,
    enabled: bool,
) -> Result<bool, String> {
    let root = folder(store);
    installed_id(&root, plugin)?;
    let id = plugin.to_string();
    let changed = tokio::task::spawn_blocking(move || {
        let mut off = disabled(&root);
        let changed = match enabled {
            true => off.remove(&id),
            false => off.insert(id),
        };
        if changed {
            write_disabled(&root, &off)?;
        }
        Ok::<bool, String>(changed)
    })
    .await
    .map_err(|error| error.to_string())??;
    if changed {
        store.forget_mcp().await;
    }
    Ok(changed)
}

/// Every installed plugin, on or off, as the screens list them.
pub(crate) fn listing(root: &Path) -> Vec<Value> {
    let off = disabled(root);
    Installed::every(root)
        .iter()
        .map(|plugin| {
            json!({
                "id": plugin.id(),
                "name": plugin.name(),
                "catalog": plugin.catalog,
                "enabled": !off.contains(&plugin.id()),
                "path": plugin.root(),
                "skills": plugin.skills().iter().map(|skill| &skill.name).collect::<Vec<_>>(),
                "servers": plugin.servers().iter().map(|(name, _)| name).collect::<Vec<_>>(),
                "commands": plugin.commands().iter()
                    .map(|(name, _)| format!("/{}:{name}", plugin.name()))
                    .collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// A plugin's manifest: Codex's, else Claude's; empty when it has none.
fn manifest(folder: &Path) -> Value {
    [".codex-plugin/plugin.json", ".claude-plugin/plugin.json"]
        .into_iter()
        .find_map(|file| {
            let text = std::fs::read_to_string(folder.join(file)).ok()?;
            serde_json::from_str(&text).ok()
        })
        .unwrap_or(Value::Null)
}

/// An installed plugin.
pub(crate) struct Installed {
    name: String,
    catalog: String,
    root: PathBuf,
    manifest: Value,
}

/// An MCP server of an installed plugin, as `mcp.json` would list it.
pub(crate) struct Server {
    pub name: String,
    pub settings: Value,
    /// The `${NAME}`s left empty: not listed in `pluginVariables`, or unset.
    pub unfilled: Vec<String>,
}

/// The variables every plugin's settings may use: its own folder.
const ROOT_VARIABLES: [&str; 2] = ["CLAUDE_PLUGIN_ROOT", "PLUGIN_ROOT"];

pub(crate) struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

impl Installed {
    fn at(name: String, catalog: String, root: PathBuf) -> Self {
        let manifest = manifest(&root);
        Self {
            name,
            catalog,
            root,
            manifest,
        }
    }

    /// The installed plugins under `root` that are turned on.
    pub(crate) fn all(root: &Path) -> Vec<Self> {
        let off = disabled(root);
        Self::every(root)
            .into_iter()
            .filter(|plugin| !off.contains(&plugin.id()))
            .collect()
    }

    /// `name@catalog`.
    pub(crate) fn id(&self) -> String {
        format!("{}@{}", self.name, self.catalog)
    }

    /// The installed plugins under `root`, on or off.
    fn every(root: &Path) -> Vec<Self> {
        let mut plugins = Vec::new();
        let Ok(catalogs) = std::fs::read_dir(root.join("installed")) else {
            return plugins;
        };
        let mut catalogs: Vec<_> = catalogs.flatten().map(|item| item.path()).collect();
        catalogs.sort();
        for catalog in catalogs {
            let Ok(found) = std::fs::read_dir(&catalog) else {
                continue;
            };
            let catalog_name = catalog
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut found: Vec<_> = found
                .flatten()
                .filter(|item| item.path().is_dir())
                .filter_map(|item| Some((item.file_name().into_string().ok()?, item.path())))
                .filter(|(name, _)| plain(name))
                .collect();
            found.sort();
            for (name, path) in found {
                plugins.push(Self::at(name, catalog_name.clone(), path));
            }
        }
        plugins
    }

    /// Its skills: `skills/<name>/SKILL.md`, and those under the folders the
    /// manifest names.
    pub(crate) fn skills(&self) -> Vec<Skill> {
        let mut folders = vec![self.root.join("skills")];
        let named: Vec<&str> = match self.manifest.get("skills") {
            Some(Value::String(path)) => vec![path.as_str()],
            Some(Value::Array(paths)) => paths.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        folders.extend(
            named
                .into_iter()
                .filter_map(|path| inside(&self.root, path).ok()),
        );
        folders.sort();
        folders.dedup();
        let mut skills = Vec::new();
        for folder in folders {
            let Ok(found) = std::fs::read_dir(&folder) else {
                continue;
            };
            let mut found: Vec<PathBuf> = found
                .flatten()
                .map(|item| item.path().join("SKILL.md"))
                .filter(|path| path.is_file())
                .collect();
            found.sort();
            for path in found {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let folder_name = path
                    .parent()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let (name, description) = front_matter(&text);
                skills.push(Skill {
                    name: format!("{}:{}", self.name, name.unwrap_or(folder_name)),
                    description: description.unwrap_or_default(),
                    path,
                });
            }
        }
        skills
    }

    /// Its MCP servers as its files list them, by their own names.
    fn listed(&self) -> Vec<(String, Value)> {
        let listed = match self.manifest.get("mcpServers") {
            Some(Value::Object(servers)) => Value::Object(servers.clone()),
            Some(Value::String(path)) => read_json(inside(&self.root, path).ok()),
            _ => read_json(Some(self.root.join(".mcp.json"))),
        };
        let listed = match listed.get("mcpServers") {
            Some(servers) => servers.clone(),
            None => listed,
        };
        let Value::Object(servers) = listed else {
            return Vec::new();
        };
        servers
            .into_iter()
            .filter(|(_, settings)| settings.is_object())
            .collect()
    }

    /// Its MCP servers by name and settings, no variable of the
    /// environment filled in.
    pub(crate) fn servers(&self) -> Vec<(String, Value)> {
        self.mcp_servers(&[], false)
            .into_iter()
            .map(|server| (server.name, server.settings))
            .collect()
    }

    /// Its MCP servers as `mcp.json` would list them, named
    /// `plugin_<plugin>_<server>` (with the catalog before the plugin's
    /// name when `qualified`, for a name installed from two catalogs),
    /// with `${CLAUDE_PLUGIN_ROOT}` and the `variables` the person listed
    /// filled in. Any other `${NAME}` is left empty: a plugin chosen by the
    /// model must not send the person's environment where it likes.
    pub(crate) fn mcp_servers(&self, variables: &[String], qualified: bool) -> Vec<Server> {
        self.listed()
            .into_iter()
            .map(|(server, settings)| {
                let mut unfilled = Vec::new();
                let settings = fill(settings, &self.root, variables, &mut unfilled);
                unfilled.sort();
                unfilled.dedup();
                Server {
                    name: match qualified {
                        true => format!("plugin_{}_{}_{server}", self.catalog, self.name),
                        false => format!("plugin_{}_{server}", self.name),
                    },
                    settings,
                    unfilled,
                }
            })
            .collect()
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Its commands for the person: each `commands/<name>.md`, by name.
    pub(crate) fn commands(&self) -> Vec<(String, PathBuf)> {
        let Ok(found) = std::fs::read_dir(self.root.join("commands")) else {
            return Vec::new();
        };
        let mut commands: Vec<(String, PathBuf)> = found
            .flatten()
            .map(|item| item.path())
            .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "md"))
            .filter_map(|path| Some((path.file_stem()?.to_str()?.to_string(), path)))
            .collect();
        commands.sort();
        commands
    }
}

fn read_json(path: Option<PathBuf>) -> Value {
    path.and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

/// Replaces `${CLAUDE_PLUGIN_ROOT}` with the plugin's folder and the
/// `${NAME}`s of `variables` with the environment's values; the others,
/// and listed ones that are unset, become empty and go to `unfilled`.
fn fill(value: Value, root: &Path, variables: &[String], unfilled: &mut Vec<String>) -> Value {
    match value {
        Value::String(text) => Value::String(fill_text(&text, root, variables, unfilled)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| fill(item, root, variables, unfilled))
                .collect(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, item)| (key, fill(item, root, variables, unfilled)))
                .collect(),
        ),
        other => other,
    }
}

fn fill_text(text: &str, root: &Path, variables: &[String], unfilled: &mut Vec<String>) -> String {
    let mut filled = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let Some(length) = rest[start..].find('}') else {
            break;
        };
        filled.push_str(&rest[..start]);
        let name = &rest[start + 2..start + length];
        if ROOT_VARIABLES.contains(&name) {
            filled.push_str(&root.to_string_lossy());
        } else {
            let value = variables
                .iter()
                .any(|listed| listed == name)
                .then(|| std::env::var(name).ok())
                .flatten();
            match value {
                Some(value) => filled.push_str(&value),
                None => unfilled.push(name.to_string()),
            }
        }
        rest = &rest[start + length + 1..];
    }
    filled.push_str(rest);
    filled
}

/// The `name` and `description` of a SKILL.md's front matter.
fn front_matter(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let block: Vec<&str> = lines.take_while(|line| line.trim() != "---").collect();
    let field = |key: &str| -> Option<String> {
        let at = block.iter().position(|line| {
            line.strip_prefix(key)
                .is_some_and(|rest| rest.starts_with(':'))
        })?;
        let value = block[at][key.len() + 1..].trim();
        let value = match value {
            ">" | "|" | ">-" | "|-" | "" => block[at + 1..]
                .iter()
                .take_while(|line| line.starts_with(' ') || line.starts_with('\t'))
                .map(|line| line.trim())
                .collect::<Vec<_>>()
                .join(" "),
            value => value.trim_matches(|c| c == '"' || c == '\'').to_string(),
        };
        (!value.is_empty()).then_some(value)
    };
    (field("name"), field("description"))
}

/// The skills of the installed plugins, for the model; `None` when there
/// are none.
pub(crate) fn render_skills(root: &Path) -> Option<String> {
    let lines: Vec<String> = Installed::all(root)
        .iter()
        .flat_map(Installed::skills)
        .map(|skill| {
            let description: String = skill.description.chars().take(DESCRIPTION_CHARS).collect();
            format!(
                "- {}: {} ({})",
                skill.name,
                description,
                skill.path.display()
            )
        })
        .collect();
    (!lines.is_empty()).then(|| {
        format!(
            "Skills of installed plugins. When one fits the task, read its SKILL.md with \
             the read tool before you start, and follow it:\n{}",
            lines.join("\n")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(folder: &Path, arguments: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(folder)
            .args(arguments)
            .stdout(Stdio::null())
            .status()
            .expect("git");
        assert!(status.success(), "git {arguments:?}");
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("folder");
        std::fs::write(path, text).expect("write");
    }

    /// A git repository with one commit of `files`.
    fn repository(folder: &Path, files: &[(&str, &str)]) -> String {
        for (path, text) in files {
            write(&folder.join(path), text);
        }
        run_git(folder, &["init", "-q"]);
        run_git(folder, &["add", "."]);
        run_git(
            folder,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-q",
                "-m",
                "plugins",
            ],
        );
        format!(
            "file:///{}",
            folder
                .to_string_lossy()
                .replace('\\', "/")
                .trim_start_matches('/')
        )
    }

    /// Reads the real catalogs from GitHub; run by hand.
    #[tokio::test]
    #[ignore = "needs the network"]
    async fn the_official_catalogs_are_read() {
        let temp = tempfile::tempdir().expect("temp");
        let (entries, unavailable) = entries(temp.path(), &official()).await;
        assert!(unavailable.is_empty(), "{unavailable:?}");
        for catalog in ["claude-plugins-official", "openai-curated"] {
            let count = entries
                .iter()
                .filter(|entry| entry.catalog == catalog)
                .count();
            let described = entries
                .iter()
                .filter(|entry| entry.catalog == catalog && !entry.description.is_empty())
                .count();
            println!("{catalog}: {count} plugins, {described} with a description");
            assert!(count > 0, "{catalog}");
        }
    }

    #[test]
    fn only_listed_variables_are_filled() {
        std::env::set_var("INOCODE_TEST_PLUGIN_LISTED", "listed-value");
        std::env::set_var("INOCODE_TEST_PLUGIN_SECRET", "secret-value");
        let mut unfilled = Vec::new();
        let filled = fill(
            json!({
                "url": "https://example.invalid/${INOCODE_TEST_PLUGIN_SECRET}",
                "headers": {"x": "${INOCODE_TEST_PLUGIN_LISTED} ${INOCODE_TEST_PLUGIN_UNSET}"},
                "args": ["${CLAUDE_PLUGIN_ROOT}/run"]
            }),
            Path::new("/plugin"),
            &[
                "INOCODE_TEST_PLUGIN_LISTED".to_string(),
                "INOCODE_TEST_PLUGIN_UNSET".to_string(),
            ],
            &mut unfilled,
        );
        assert_eq!(filled["url"], "https://example.invalid/");
        assert_eq!(filled["headers"]["x"], "listed-value ");
        assert_eq!(filled["args"][0], "/plugin/run");
        unfilled.sort();
        assert_eq!(
            unfilled,
            ["INOCODE_TEST_PLUGIN_SECRET", "INOCODE_TEST_PLUGIN_UNSET"]
        );
    }

    #[test]
    fn a_failed_replacement_keeps_the_installed_plugin() {
        let temp = tempfile::tempdir().expect("temp");
        let target = temp.path().join("installed").join("notes");
        write(&target.join("old.txt"), "old");
        let missing = temp.path().join("no-staging");
        assert!(replace(&missing, &target).is_err());
        assert_eq!(
            std::fs::read_to_string(target.join("old.txt")).expect("kept"),
            "old"
        );
        let staging = temp.path().join("staging");
        write(&staging.join("new.txt"), "new");
        replace(&staging, &target).expect("replace");
        assert!(target.join("new.txt").exists() && !target.join("old.txt").exists());
        assert!(!temp.path().join("installed").join(".notes.old").exists());
    }

    #[tokio::test]
    async fn plugins_are_found_installed_and_used() {
        let temp = tempfile::tempdir().expect("temp");
        let outside = repository(
            &temp.path().join("outside"),
            &[(
                "skills/lint/SKILL.md",
                "---\nname: lint\ndescription: >\n  Lints Rust code\n  with clippy.\n---\nRun clippy.",
            )],
        );
        let marketplace = json!({
            "name": "test-catalog",
            "plugins": [
                {"name": "notes", "source": "./plugins/notes", "category": "productivity"},
                {"name": "linter", "description": "Rust linting", "source": {"source": "url", "url": outside}},
                {"name": "escape", "description": "Rust escape", "source": "../outside"},
                {"name": "../bad", "description": "Rust bad name", "source": "./x"}
            ]
        });
        let catalog = repository(
            &temp.path().join("catalog"),
            &[
                (".claude-plugin/marketplace.json", &marketplace.to_string()),
                (
                    "plugins/notes/.codex-plugin/plugin.json",
                    r#"{"name":"notes","interface":{"shortDescription":"Take notes in Markdown"}}"#,
                ),
                (
                    "plugins/notes/skills/write/SKILL.md",
                    "---\nname: write-note\ndescription: \"Writes a note\"\n---\nWrite it.",
                ),
                (
                    "plugins/notes/.mcp.json",
                    r#"{"mcpServers":{"store":{"command":"${CLAUDE_PLUGIN_ROOT}/server","args":["--root","${CLAUDE_PLUGIN_ROOT}"],"env":{"TOKEN":"${NOTES_TOKEN}"}}}}"#,
                ),
                ("plugins/notes/commands/new.md", "Make a note."),
            ],
        );
        let catalogs = [Catalog {
            folder: "test",
            repository: catalog,
            reference: "HEAD".into(),
            file: ".claude-plugin/marketplace.json",
        }];
        let root = temp.path().join("plugins");

        let found: Value = serde_json::from_str(
            &search_in(&root, &catalogs, r#"{"query": "markdown notes"}"#)
                .await
                .expect("search"),
        )
        .expect("json");
        assert_eq!(found["plugins"][0]["plugin"], "notes@test-catalog");
        assert_eq!(found["plugins"][0]["description"], "Take notes in Markdown");
        assert_eq!(found["plugins"][0]["installed"], false);
        let rust: Value = serde_json::from_str(
            &search_in(&root, &catalogs, r#"{"query": "rust"}"#)
                .await
                .expect("search"),
        )
        .expect("json");
        let names: Vec<&str> = rust["plugins"]
            .as_array()
            .expect("plugins")
            .iter()
            .map(|plugin| plugin["plugin"].as_str().expect("name"))
            .collect();
        assert_eq!(
            names.len(),
            2,
            "a name that is not a folder name is left out: {names:?}"
        );
        assert!(search_in(&root, &catalogs, r#"{"query": " "}"#)
            .await
            .is_err());

        let shown = preview_in(&root, &catalogs, r#"{"plugin": "notes@test-catalog"}"#)
            .await
            .expect("preview");
        assert!(
            shown.contains("MCP server store runs `${CLAUDE_PLUGIN_ROOT}/server --root ${CLAUDE_PLUGIN_ROOT}` outside the sandbox")
                && shown.contains("asks for the variables NOTES_TOKEN"),
            "{shown}"
        );
        let installed = install_from(&root, &catalogs, r#"{"plugin": "notes@test-catalog"}"#)
            .await
            .expect("install");
        assert!(!root.join("previews").join("test-catalog-notes").exists());
        assert!(installed.contains("notes:write-note"), "{installed}");
        assert!(installed.contains("plugin_notes_store"), "{installed}");
        assert!(!installed.contains("Not used here"), "{installed}");
        install_from(&root, &catalogs, r#"{"plugin": "linter@test-catalog"}"#)
            .await
            .expect("install from a url");
        let escape = install_from(&root, &catalogs, r#"{"plugin": "escape@test-catalog"}"#).await;
        assert!(escape.expect_err("escape").contains("outside"));
        assert!(
            install_from(&root, &catalogs, r#"{"plugin": "none@test-catalog"}"#)
                .await
                .is_err()
        );

        let skills = render_skills(&root).expect("skills");
        assert!(
            skills.contains("- linter:lint: Lints Rust code with clippy."),
            "{skills}"
        );
        assert!(
            skills.contains("- notes:write-note: Writes a note"),
            "{skills}"
        );
        assert!(!root.join("installed/test-catalog/linter/.git").exists());

        let plugins = Installed::all(&root);
        let notes = plugins
            .iter()
            .find(|plugin| plugin.name == "notes")
            .expect("notes");
        let servers = notes.mcp_servers(&[], false);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "plugin_notes_store");
        let place = notes.root().to_string_lossy().into_owned();
        assert_eq!(servers[0].settings["command"], format!("{place}/server"));
        assert_eq!(servers[0].settings["args"][1], place);
        assert_eq!(
            notes.mcp_servers(&[], true)[0].name,
            "plugin_test-catalog_notes_store"
        );

        let again: Value = serde_json::from_str(
            &search_in(&root, &catalogs, r#"{"query": "notes"}"#)
                .await
                .expect("search"),
        )
        .expect("json");
        assert_eq!(again["plugins"][0]["installed"], true);
    }
}
