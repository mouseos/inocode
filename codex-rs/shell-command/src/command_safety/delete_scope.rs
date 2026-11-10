//! Path-aware scoping for forced deletes (`rm -f`, `Remove-Item -Force`).
//!
//! The command-safety heuristics treat every forced delete as dangerous because the flag alone says
//! nothing about what is being removed. That makes routine cleanup of files the agent created
//! impossible when approvals are disabled ("Full Access"). A [`DeleteScope`] lets the
//! heuristics relax only the clearly bounded case: every target is a literal path that lies strictly
//! inside the working directory (or a temporary directory). Anything that cannot be proven to be
//! contained -- variables, globs, `..` escapes, links that leave the root, `.git`, home or
//! system directories -- stays dangerous.

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

/// Characters that make a target non-literal or hard to reason about. Seeing one means "unknown".
const NON_LITERAL_CHARS: &[char] = &[
    '$', '%', '`', '*', '?', '[', ']', '{', '}', '(', ')', ';', '|', '&', '<', '>', '!', '@', '#',
    '"', '\'',
];

const UNIX_SYSTEM_TREES: &[&str] = &[
    "usr", "etc", "bin", "sbin", "lib", "lib32", "lib64", "boot", "dev", "proc", "sys", "system",
];
const UNIX_CONTAINER_DIRS: &[&str] = &[
    "home",
    "users",
    "opt",
    "var",
    "tmp",
    "srv",
    "mnt",
    "media",
    "volumes",
    "library",
    "applications",
    "root",
];
const WINDOWS_SYSTEM_TREES: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
    "$recycle.bin",
    "system volume information",
];

/// Directories in which a forced delete of a literal path is treated as routine cleanup.
#[derive(Debug, Clone)]
pub struct DeleteScope {
    /// Normalized absolute roots. The first entry is the working directory.
    roots: Vec<PathBuf>,
    case_insensitive: bool,
    check_filesystem: bool,
}

impl DeleteScope {
    /// Builds a scope for `cwd`. Returns `None` when the working directory is too broad to be
    /// a safe boundary (a filesystem root, the home directory or one of its ancestors, or a system
    /// directory); callers then keep treating every forced delete as dangerous.
    ///
    /// `check_filesystem` should only be set when the paths live on this machine; it follows links
    /// so that a link that leaves the root is not mistaken for a contained path.
    pub fn new(
        cwd: &Path,
        user_home: Option<&Path>,
        temp_dirs: &[PathBuf],
        case_insensitive: bool,
        check_filesystem: bool,
    ) -> Option<Self> {
        let cwd = normalize_absolute(cwd)?;
        let home = user_home.and_then(normalize_absolute);
        if !is_project_like(&cwd, home.as_deref(), case_insensitive) {
            return None;
        }
        let mut roots = vec![cwd];
        for temp_dir in temp_dirs {
            if let Some(temp_dir) = normalize_absolute(temp_dir)
                && normal_components(&temp_dir, case_insensitive).len() >= 1
            {
                roots.push(temp_dir);
            }
        }
        Some(Self {
            roots,
            case_insensitive,
            check_filesystem,
        })
    }

    /// True when there is at least one target and every target is provably contained.
    pub fn contains_all(&self, targets: &[String]) -> bool {
        !targets.is_empty() && targets.iter().all(|target| self.contains(target))
    }

    /// The working directory used to resolve relative targets.
    pub fn cwd(&self) -> &Path {
        &self.roots[0]
    }

    fn contains(&self, raw: &str) -> bool {
        let raw = strip_matching_quotes(raw.trim());
        if raw.is_empty()
            || raw.starts_with('~')
            || raw
                .chars()
                .any(|c| c.is_control() || NON_LITERAL_CHARS.contains(&c))
        {
            return false;
        }
        if self.case_insensitive && raw.contains('~') {
            // 8.3 short names hide the real path.
            return false;
        }

        let path = Path::new(raw);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else if path.has_root() || path.components().next().is_some_and(is_prefix) {
            // Drive-relative or root-relative paths depend on state we cannot see.
            return false;
        } else {
            self.cwd().join(path)
        };
        let Some(normalized) = normalize_absolute(&absolute) else {
            return false;
        };

        let Some(root) = self
            .roots
            .iter()
            .find(|root| self.is_strictly_inside(&normalized, root))
        else {
            return false;
        };

        // Never treat repository metadata, or names Windows would silently rewrite, as routine.
        let relative: Vec<String> = normal_components(&normalized, self.case_insensitive)
            .into_iter()
            .skip(normal_components(root, self.case_insensitive).len())
            .collect();
        if relative.iter().any(|part| {
            part == ".git"
                || (self.case_insensitive && (part.ends_with('.') || part.ends_with(' ')))
        }) {
            return false;
        }
        if self.case_insensitive && relative.iter().any(|part| part.contains(':')) {
            // NTFS alternate data streams.
            return false;
        }

        !self.check_filesystem || self.is_contained_on_disk(&normalized, root)
    }

    fn is_strictly_inside(&self, path: &Path, root: &Path) -> bool {
        let path_parts = self.parts(path);
        let root_parts = self.parts(root);
        path_parts.len() > root_parts.len() && path_parts.starts_with(&root_parts)
    }

    fn parts(&self, path: &Path) -> Vec<String> {
        path.components()
            .map(|component| {
                let part = component.as_os_str().to_string_lossy().into_owned();
                if self.case_insensitive {
                    part.to_lowercase()
                } else {
                    part
                }
            })
            .collect()
    }

    /// Resolves links on the part of the path that exists and re-checks containment.
    fn is_contained_on_disk(&self, path: &Path, root: &Path) -> bool {
        let Ok(canonical_root) = std::fs::canonicalize(root) else {
            return false;
        };
        let canonical_root = strip_verbatim(&canonical_root);

        let mut existing = path.to_path_buf();
        while !existing.exists() && std::fs::symlink_metadata(&existing).is_err() {
            if !existing.pop() {
                return false;
            }
        }
        let Ok(canonical) = std::fs::canonicalize(&existing) else {
            return false;
        };
        let canonical = strip_verbatim(&canonical);

        if existing == path {
            self.is_strictly_inside(&canonical, &canonical_root)
        } else {
            // The target does not exist yet; its nearest existing ancestor must stay in the root.
            let parts = self.parts(&canonical);
            let root_parts = self.parts(&canonical_root);
            parts.len() >= root_parts.len() && parts.starts_with(&root_parts)
        }
    }
}

fn is_prefix(component: Component<'_>) -> bool {
    matches!(component, Component::Prefix(_))
}

fn strip_matching_quotes(value: &str) -> &str {
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// Lexically normalizes an absolute path. Returns `None` for relative paths and for paths that
/// try to climb above their root.
fn normalize_absolute(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return None;
                }
                normalized.pop();
                depth -= 1;
            }
            Component::Normal(part) => {
                normalized.push(part);
                depth += 1;
            }
        }
    }
    Some(normalized)
}

fn normal_components(path: &Path, case_insensitive: bool) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => {
                let part = part.to_string_lossy().into_owned();
                Some(if case_insensitive {
                    part.to_lowercase()
                } else {
                    part
                })
            }
            _ => None,
        })
        .collect()
}

fn is_project_like(cwd: &Path, home: Option<&Path>, case_insensitive: bool) -> bool {
    let normals = normal_components(cwd, case_insensitive);
    let Some(first) = normals.first() else {
        return false; // filesystem or drive root
    };
    let system_trees = if case_insensitive {
        WINDOWS_SYSTEM_TREES
    } else {
        UNIX_SYSTEM_TREES
    };
    if system_trees.contains(&first.as_str()) {
        return false;
    }
    let containers = if case_insensitive {
        &["users"][..]
    } else {
        UNIX_CONTAINER_DIRS
    };
    if containers.contains(&first.as_str()) {
        // "/home", "C:\Users", "/var" ... and, when the home directory is unknown, the per-user
        // directory directly below them.
        let user_dir_depth = if home.is_none() && matches!(first.as_str(), "home" | "users") {
            2
        } else {
            1
        };
        if normals.len() <= user_dir_depth {
            return false;
        }
    }
    if let Some(home) = home {
        let cwd_parts = normal_components(cwd, case_insensitive);
        let home_parts = normal_components(home, case_insensitive);
        // The home directory itself, or any directory that contains it, is not a project.
        if home_parts.starts_with(&cwd_parts) {
            return false;
        }
    }
    true
}

/// Removes the Windows verbatim prefix (`\\?\\`) that `canonicalize` adds.
fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn scope(root: &Path) -> DeleteScope {
        DeleteScope::new(
            root,
            /*user_home*/ None,
            &[],
            /*case_insensitive*/ cfg!(windows),
            /*check_filesystem*/ true,
        )
        .expect("project directory is a valid scope")
    }

    fn project() -> tempfile::TempDir {
        // A directory two levels below the temp dir looks like an ordinary project.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("proj").join("sub")).expect("create dirs");
        dir
    }

    fn targets(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    #[test]
    fn literal_paths_inside_the_working_directory_are_contained() {
        let dir = project();
        let cwd = dir.path().join("proj");
        let scope = scope(&cwd);
        assert!(scope.contains_all(&targets(&["file.txt"])));
        assert!(scope.contains_all(&targets(&["sub/file.txt", "./other/x.bin"])));
        assert!(scope.contains_all(&targets(&[cwd.join("sub").join("a.txt").to_str().unwrap()])));
        assert!(scope.contains_all(&targets(&["'sub/with space.txt'"])));
    }

    #[test]
    fn escapes_and_unknown_targets_are_not_contained() {
        let dir = project();
        let cwd = dir.path().join("proj");
        let outside = dir.path().join("elsewhere.txt");
        let scope = scope(&cwd);
        assert_eq!(
            scope.contains_all(&[]),
            false,
            "no targets is never contained"
        );
        for bad in [
            "../elsewhere.txt",
            "sub/../../elsewhere.txt",
            outside.to_str().unwrap(),
            ".",
            "*.tmp",
            "sub/*",
            "$target",
            "$env:TEMP/x",
            "%TEMP%/x",
            "~/x",
            "",
        ] {
            assert_eq!(scope.contains_all(&targets(&[bad])), false, "{bad:?}");
        }
        assert_eq!(
            scope.contains_all(&targets(&["ok.txt", "../elsewhere.txt"])),
            false,
            "one bad target taints the command"
        );
    }

    #[test]
    fn repository_metadata_is_never_routine() {
        let dir = project();
        let cwd = dir.path().join("proj");
        let scope = scope(&cwd);
        assert_eq!(scope.contains_all(&targets(&[".git"])), false);
        assert_eq!(
            scope.contains_all(&targets(&[".git/hooks/pre-commit"])),
            false
        );
        assert_eq!(scope.contains_all(&targets(&["sub/.git/config"])), false);
        assert!(scope.contains_all(&targets(&[".github/workflows/ci.yml"])));
    }

    #[test]
    fn temp_directories_are_additional_roots() {
        let dir = project();
        let cwd = dir.path().join("proj");
        let temp = dir.path().join("scratch");
        std::fs::create_dir_all(&temp).unwrap();
        let scope =
            DeleteScope::new(&cwd, None, std::slice::from_ref(&temp), cfg!(windows), true).unwrap();
        assert!(scope.contains_all(&targets(&[temp.join("run-1").to_str().unwrap()])));
        assert_eq!(
            scope.contains_all(&targets(&[temp.to_str().unwrap()])),
            false,
            "the temp directory itself is not routine"
        );
    }

    #[test]
    fn broad_working_directories_do_not_form_a_scope() {
        let dir = project();
        let home = dir.path().join("proj");
        // The home directory and its ancestors.
        assert!(
            DeleteScope::new(&home, Some(&home), &[], cfg!(windows), false).is_none(),
            "home itself"
        );
        assert!(
            DeleteScope::new(dir.path(), Some(&home), &[], cfg!(windows), false).is_none(),
            "ancestor of home"
        );
        // A directory below home is fine.
        assert!(
            DeleteScope::new(&home.join("sub"), Some(&home), &[], cfg!(windows), false).is_some()
        );
        if cfg!(windows) {
            for cwd in [
                r"C:\",
                r"C:\Windows",
                r"C:\Windows\Temp",
                r"C:\Users",
                r"C:\Program Files\x",
            ] {
                assert!(
                    DeleteScope::new(Path::new(cwd), None, &[], true, false).is_none(),
                    "{cwd}"
                );
            }
        } else {
            for cwd in [
                "/",
                "/usr",
                "/usr/local",
                "/etc/x",
                "/home",
                "/var",
                "/tmp",
                "/Users",
            ] {
                assert!(
                    DeleteScope::new(Path::new(cwd), None, &[], false, false).is_none(),
                    "{cwd}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn links_that_leave_the_root_are_not_contained() {
        let dir = project();
        let cwd = dir.path().join("proj");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, cwd.join("link")).unwrap();
        let scope = scope(&cwd);
        assert_eq!(scope.contains_all(&targets(&["link"])), false);
        assert_eq!(scope.contains_all(&targets(&["link/secret.txt"])), false);
        assert_eq!(
            scope.contains_all(&targets(&["link/not-yet-created.txt"])),
            false
        );
        assert!(scope.contains_all(&targets(&["sub/not-yet-created.txt"])));
    }
}
