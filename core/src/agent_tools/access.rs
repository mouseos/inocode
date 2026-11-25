//! Where a tool call reads and writes files, and whether that is outside
//! the places a thread works in: the project folder, the thread's temporary
//! folder, and (for reading) the thread's own files and the installed
//! plugins. Paths are compared as the file system resolves them: `..` and
//! links are followed before deciding. Design:
//! docs/evidence/approval-modes.md.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::store::{SessionId, Store};

/// The places a thread may use without asking, as real paths.
pub(crate) struct Places {
    folder: Option<PathBuf>,
    /// Written and read: the project folder and the thread's temporary folder.
    writable: Vec<PathBuf>,
    /// Only read: the thread's logs, fetched pages, images and recordings,
    /// and the installed plugins' files.
    readable: Vec<PathBuf>,
    home: Option<PathBuf>,
    /// The folder of the database, the sign-ins and the settings.
    data: Option<PathBuf>,
}

impl Places {
    pub(crate) fn of(store: &Store, session: SessionId, folder: Option<&Path>) -> Self {
        let files = store.files_folder();
        let thread = format!("thread-{}", session.0);
        let mut writable = vec![files.join("logs").join(&thread).join("tmp")];
        writable.extend(folder.map(Path::to_path_buf));
        let mut readable: Vec<PathBuf> = ["logs", "fetch", "images", "audio"]
            .iter()
            .map(|kind| files.join(kind).join(&thread))
            .collect();
        readable.push(crate::plugins::folder(store).join("installed"));
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .filter(|home| home.is_absolute());
        Self {
            folder: folder.map(Path::to_path_buf),
            writable: writable.iter().filter_map(|path| real(path)).collect(),
            readable: readable.iter().filter_map(|path| real(path)).collect(),
            home: home.as_deref().and_then(real),
            data: files.parent().and_then(real),
        }
    }
}

/// What a call does outside the thread's places.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Outside {
    /// It writes there.
    pub writes: bool,
    /// It reads a place known to hold secrets.
    pub secret: bool,
    /// The paths outside, as the call gave them.
    pub paths: Vec<String>,
}

impl Outside {
    pub(crate) fn describe(&self) -> String {
        let what = match (self.writes, self.secret) {
            (true, _) => "it writes outside the project folder",
            (false, true) => "it reads a place outside the project that holds secrets",
            (false, false) => "it reads outside the project folder",
        };
        format!("{what} ({})", self.paths.join(", "))
    }
}

/// The paths a call names and whether it writes them; empty for a call
/// that names no file of its own.
fn targets(name: &str, arguments: &str) -> Vec<(bool, String)> {
    let Ok(arguments) = serde_json::from_str::<Value>(arguments) else {
        return Vec::new();
    };
    let path = |value: &Value| {
        value
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    match name {
        crate::edit::EDIT | crate::edit::WRITE => {
            path(&arguments).map(|p| (true, p)).into_iter().collect()
        }
        crate::edit::EDITS => arguments
            .get("edits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(path)
            .map(|p| (true, p))
            .collect(),
        crate::audio::AUDIO if crate::audio::records(&arguments.to_string()) => {
            path(&arguments).map(|p| (true, p)).into_iter().collect()
        }
        crate::read::READ | crate::find::FIND | crate::transcribe::TRANSCRIBE => {
            path(&arguments).map(|p| (false, p)).into_iter().collect()
        }
        _ => Vec::new(),
    }
}

/// What `name` with `arguments` does outside `places`; `None` when it stays
/// inside them.
pub(crate) fn outside(name: &str, arguments: &str, places: &Places) -> Option<Outside> {
    let mut found = Outside {
        writes: false,
        secret: false,
        paths: Vec::new(),
    };
    for (writes, given) in targets(name, arguments) {
        let path = Path::new(&given);
        let path = match (&places.folder, path.is_absolute()) {
            (_, true) => path.to_path_buf(),
            (Some(folder), false) => folder.join(path),
            // The tool refuses a relative path without a project folder.
            (None, false) => continue,
        };
        let real = real(&path);
        let allowed = match &real {
            Some(real) if writes => places.writable.iter().any(|root| within(real, root)),
            Some(real) => places
                .writable
                .iter()
                .chain(&places.readable)
                .any(|root| within(real, root)),
            None => false,
        };
        if allowed {
            continue;
        }
        found.writes |= writes;
        found.secret |= real.as_deref().is_none_or(|real| secret(real, places));
        found.paths.push(given);
    }
    (!found.paths.is_empty()).then_some(found)
}

/// `path` (absolute) as the file system resolves it: the longest part that
/// exists is resolved with its links and `..`, the rest is added name by
/// name, following each link that appears. `None` when no part exists or
/// a link cannot be resolved.
pub(crate) fn real(path: &Path) -> Option<PathBuf> {
    let base = path
        .ancestors()
        .find(|ancestor| !ancestor.as_os_str().is_empty() && ancestor.symlink_metadata().is_ok())?;
    let mut real = std::fs::canonicalize(base).ok()?;
    for part in path.strip_prefix(base).ok()?.components() {
        match part {
            Component::ParentDir => {
                real.pop();
            }
            Component::CurDir => {}
            Component::Normal(name) => {
                real.push(name);
                if real.symlink_metadata().is_ok() {
                    real = std::fs::canonicalize(&real).ok()?;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(real)
}

/// Whether the real path `path` is `root` or inside it. Windows compares
/// names without regard to case, as its file systems do by default.
fn within(path: &Path, root: &Path) -> bool {
    let name = |part: Component| {
        let text = part.as_os_str().to_string_lossy();
        if cfg!(windows) {
            text.to_lowercase()
        } else {
            text.into_owned()
        }
    };
    let mut parts = path.components().map(name);
    root.components()
        .map(name)
        .all(|part| parts.next() == Some(part))
}

/// Folders under the home folder that hold keys, sign-ins or browser
/// profiles.
const SECRET_FOLDERS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".config/gcloud",
    ".config/gh",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
    ".mozilla",
    ".local/share/keyrings",
    ".password-store",
    ".claude",
    ".codex",
    ".inocode",
    "Library/Keychains",
    "Library/Cookies",
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Firefox",
    "Library/Application Support/Microsoft Edge",
    "AppData/Local/Google/Chrome/User Data",
    "AppData/Local/Microsoft/Edge/User Data",
    "AppData/Local/BraveSoftware",
    "AppData/Roaming/Mozilla/Firefox",
    "AppData/Roaming/Microsoft/Credentials",
    "AppData/Local/Microsoft/Credentials",
    "AppData/Roaming/Microsoft/Protect",
];

/// Files under the home folder that hold sign-ins.
const SECRET_FILES: &[&str] = &[
    ".netrc",
    "_netrc",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".pgpass",
];

/// Whether a path outside the thread's places is a known place of secrets:
/// inocode's own data folder, the folders and files above under the home
/// folder, or a file named like a key or an environment file.
fn secret(path: &Path, places: &Places) -> bool {
    if places
        .data
        .as_deref()
        .is_some_and(|data| within(path, data))
    {
        return true;
    }
    if let Some(home) = &places.home {
        let under = |relative: &str| within(path, &home.join(relative));
        if SECRET_FOLDERS
            .iter()
            .chain(SECRET_FILES)
            .any(|place| under(place))
        {
            return true;
        }
    }
    let Some(name) = path
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
    else {
        return false;
    };
    name == ".env"
        || name.starts_with(".env.")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.starts_with("id_ecdsa")
        || name.starts_with("credentials")
        || [".pem", ".key", ".p12", ".pfx", ".kdbx"]
            .iter()
            .any(|extension| name.ends_with(extension))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places(folder: &Path, temp: &Path, home: &Path, data: &Path) -> Places {
        Places {
            folder: Some(folder.to_path_buf()),
            writable: vec![real(folder).unwrap(), real(temp).unwrap()],
            readable: Vec::new(),
            home: real(home),
            data: real(data),
        }
    }

    #[test]
    fn paths_are_resolved_before_they_are_judged() {
        let dir = tempfile::tempdir().expect("dir");
        let root = real(dir.path()).expect("root");
        let (project, temp, home, data) = (
            root.join("project"),
            root.join("temp"),
            root.join("home"),
            root.join("home").join(".inocode"),
        );
        for folder in [&project, &temp, &home.join(".ssh"), &data] {
            std::fs::create_dir_all(folder).expect("folder");
        }
        let places = places(&project, &temp, &home, &data);
        let write = |path: &str| {
            outside(
                crate::edit::WRITE,
                &serde_json::json!({"path": path, "content": ""}).to_string(),
                &places,
            )
        };
        let read = |path: &str| {
            outside(
                crate::read::READ,
                &serde_json::json!({"path": path}).to_string(),
                &places,
            )
        };
        assert_eq!(write("src/new/main.rs"), None);
        assert_eq!(write(&temp.join("x").to_string_lossy()), None);
        let escaped = write("src/../../home/.bashrc").expect("outside");
        assert!(escaped.writes && !escaped.secret, "{escaped:?}");
        assert!(write("missing/../../home/x").is_some());
        let key = read(&home.join(".ssh").join("id_ed25519").to_string_lossy()).expect("outside");
        assert!(key.secret && !key.writes, "{key:?}");
        assert!(
            read("../home/.inocode/settings.json")
                .expect("outside")
                .secret
        );
        assert!(read("../elsewhere/.env.local").expect("outside").secret);
        let plain = read("../home/notes.txt").expect("outside");
        assert!(!plain.secret && !plain.writes);
        assert_eq!(read("README.md"), None);

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&home, project.join("link")).expect("link");
            assert!(write("link/.profile").expect("outside").writes);
        }
    }

    #[test]
    fn edits_and_recordings_name_their_files() {
        let edits = serde_json::json!({"edits": [
            {"path": "a.rs", "old": "x", "new": "y"},
            {"path": "/etc/b", "old": "x", "new": "y"}
        ]});
        assert_eq!(
            targets(crate::edit::EDITS, &edits.to_string()),
            [(true, "a.rs".to_string()), (true, "/etc/b".to_string())]
        );
        assert_eq!(
            targets(crate::audio::AUDIO, r#"{"action":"record","path":"x.wav"}"#),
            [(true, "x.wav".to_string())]
        );
        assert!(targets(crate::audio::AUDIO, r#"{"action":"list","path":"x"}"#).is_empty());
        assert!(targets(crate::fetch::FETCH, r#"{"url":"https://x","path":"y"}"#).is_empty());
    }
}
