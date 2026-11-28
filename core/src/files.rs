//! Files for the screens: reading, writing, listing, copying and deleting
//! by absolute path, as a person at the computer would
//! (docs/evidence/file-requests.md).

use std::path::{Path, PathBuf};

use base64::Engine;
use serde::Serialize;
use serde_json::{json, Value};

use crate::ErrorInfo;

/// The bytes one `fs/readFile` gives at most.
pub const READ_MAX: u64 = 16 * 1024 * 1024;

/// An absolute path from a request.
pub fn absolute(path: &str) -> Result<PathBuf, ErrorInfo> {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(ErrorInfo::new(
            "invalid_params",
            format!("{} is not an absolute path", path.display()),
        ));
    }
    Ok(path)
}

fn failed(path: &Path, error: std::io::Error) -> ErrorInfo {
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => "not_found",
        std::io::ErrorKind::AlreadyExists => "already_exists",
        std::io::ErrorKind::PermissionDenied => "permission_denied",
        _ => "fs_failed",
    };
    ErrorInfo::new(code, format!("{}: {error}", path.display()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Metadata {
    /// `file`, `directory`, `symlink` or `other`; a link is not followed.
    pub kind: &'static str,
    pub size: u64,
    pub modified_ms: Option<u64>,
    pub readonly: bool,
}

fn metadata_of(meta: &std::fs::Metadata) -> Metadata {
    let kind = if meta.is_symlink() {
        "symlink"
    } else if meta.is_dir() {
        "directory"
    } else if meta.is_file() {
        "file"
    } else {
        "other"
    };
    Metadata {
        kind,
        size: meta.len(),
        modified_ms: meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| since.as_millis() as u64),
        readonly: meta.permissions().readonly(),
    }
}

pub fn metadata(path: &Path) -> Result<Metadata, ErrorInfo> {
    std::fs::symlink_metadata(path)
        .map(|meta| metadata_of(&meta))
        .map_err(|error| failed(path, error))
}

/// The entries of a folder by name, each with its metadata.
pub fn read_directory(path: &Path) -> Result<Value, ErrorInfo> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|error| failed(path, error))? {
        let entry = entry.map_err(|error| failed(path, error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = std::fs::symlink_metadata(entry.path())
            .map(|meta| metadata_of(&meta))
            .map_err(|error| failed(&entry.path(), error))?;
        entries.push((name, meta));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let entries: Vec<Value> = entries
        .into_iter()
        .map(|(name, meta)| {
            let mut entry = json!(meta);
            entry["name"] = json!(name);
            entry
        })
        .collect();
    Ok(json!({ "entries": entries }))
}

/// `limit` bytes of a file from `offset` (the whole file when neither is
/// given), as UTF-8 text, or as base64 when `base64` is set.
pub fn read_file(
    path: &Path,
    offset: u64,
    limit: Option<u64>,
    base64: bool,
) -> Result<Value, ErrorInfo> {
    use std::io::{Read, Seek};
    let mut file = std::fs::File::open(path).map_err(|error| failed(path, error))?;
    let size = file.metadata().map_err(|error| failed(path, error))?.len();
    let limit = limit.unwrap_or(size.saturating_sub(offset));
    if limit > READ_MAX {
        return Err(ErrorInfo::new(
            "too_large",
            format!(
                "{} is larger than {READ_MAX} bytes; read it in parts",
                path.display()
            ),
        ));
    }
    file.seek(std::io::SeekFrom::Start(offset.min(size)))
        .map_err(|error| failed(path, error))?;
    let mut bytes = Vec::new();
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| failed(path, error))?;
    let mut answer = json!({ "size": size, "offset": offset.min(size), "length": bytes.len() });
    if base64 {
        answer["data"] = json!(base64::engine::general_purpose::STANDARD.encode(&bytes));
        return Ok(answer);
    }
    let text = String::from_utf8(bytes).map_err(|_| {
        ErrorInfo::new(
            "not_text",
            format!(
                "{} is not UTF-8 text there; read it as base64",
                path.display()
            ),
        )
    })?;
    answer["text"] = json!(text);
    Ok(answer)
}

/// What `fs/writeFile` writes.
pub enum Contents {
    Text(String),
    Base64(String),
}

/// Writes a whole file, making its folders when `parents` is set.
pub fn write_file(path: &Path, contents: Contents, parents: bool) -> Result<Value, ErrorInfo> {
    let bytes = match contents {
        Contents::Text(text) => text.into_bytes(),
        Contents::Base64(data) => base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|error| ErrorInfo::new("invalid_params", format!("\"data\": {error}")))?,
    };
    if parents {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| failed(parent, error))?;
        }
    }
    std::fs::write(path, &bytes).map_err(|error| failed(path, error))?;
    Ok(json!({ "written": bytes.len() }))
}

pub fn create_directory(path: &Path, recursive: bool) -> Result<Value, ErrorInfo> {
    match recursive {
        true => std::fs::create_dir_all(path),
        false => std::fs::create_dir(path),
    }
    .map_err(|error| failed(path, error))?;
    Ok(json!({}))
}

/// Deletes a file or link, or a folder: an empty one, or with all it holds
/// when `recursive` is set. A link is deleted, not what it points to.
pub fn remove(path: &Path, recursive: bool) -> Result<Value, ErrorInfo> {
    let meta = std::fs::symlink_metadata(path).map_err(|error| failed(path, error))?;
    let removed = if meta.is_dir() {
        match recursive {
            true => std::fs::remove_dir_all(path),
            false => std::fs::remove_dir(path),
        }
    } else {
        std::fs::remove_file(path).or_else(|error| {
            // A link to a folder is removed as a folder on Windows.
            if meta.is_symlink() {
                std::fs::remove_dir(path)
            } else {
                Err(error)
            }
        })
    };
    removed.map_err(|error| failed(path, error))?;
    Ok(json!({}))
}

/// Copies a file, or a folder with all it holds when `recursive` is set.
/// An existing target is replaced only when `overwrite` is set; links are
/// copied as the files they point to.
pub fn copy(from: &Path, to: &Path, recursive: bool, overwrite: bool) -> Result<Value, ErrorInfo> {
    let meta = std::fs::metadata(from).map_err(|error| failed(from, error))?;
    if !overwrite && std::fs::symlink_metadata(to).is_ok() {
        return Err(ErrorInfo::new(
            "already_exists",
            format!("{} already exists", to.display()),
        ));
    }
    if meta.is_dir() {
        if !recursive {
            return Err(ErrorInfo::new(
                "invalid_params",
                format!("{} is a folder; copy it with \"recursive\"", from.display()),
            ));
        }
        if to.starts_with(from) {
            return Err(ErrorInfo::new(
                "invalid_params",
                format!("{} is inside {}", to.display(), from.display()),
            ));
        }
        copy_folder(from, to)?;
    } else {
        std::fs::copy(from, to).map_err(|error| failed(to, error))?;
    }
    Ok(json!({}))
}

fn copy_folder(from: &Path, to: &Path) -> Result<(), ErrorInfo> {
    std::fs::create_dir_all(to).map_err(|error| failed(to, error))?;
    for entry in std::fs::read_dir(from).map_err(|error| failed(from, error))? {
        let entry = entry.map_err(|error| failed(from, error))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        if std::fs::metadata(&source)
            .map_err(|error| failed(&source, error))?
            .is_dir()
        {
            copy_folder(&source, &target)?;
        } else {
            std::fs::copy(&source, &target).map_err(|error| failed(&target, error))?;
        }
    }
    Ok(())
}
