//! Copies of a project folder taken before each turn, kept in a git
//! repository of the store's own, so that the files can be put back as
//! they were (docs/evidence/file-snapshots.md).

use std::path::Path;
use std::time::Duration;

use crate::ErrorInfo;

/// How long one git step may take.
const WAIT: Duration = Duration::from_secs(120);

/// The file that reads as empty, given where git would read a config file
/// or a hooks folder, so that none is used.
const NOTHING: &str = if cfg!(windows) { "nul" } else { "/dev/null" };

/// One copy or restore at a time: the copies of a project share one index.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `git` on the repository `store` with `folder` as its files. As for the
/// project's own state (docs/evidence/git-context.md), the hooks, the
/// file-system monitor and the system and global settings are not used, and
/// line endings are kept as they are.
fn git(store: &Path, folder: &Path, arguments: &[&str]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    command
        .arg(format!("--git-dir={}", store.display()))
        .arg(format!("--work-tree={}", folder.display()))
        .args([
            "--no-pager",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.safecrlf=false",
            "-c",
            "core.longpaths=true",
        ])
        .arg("-c")
        .arg(format!("core.hooksPath={NOTHING}"))
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", NOTHING)
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(folder)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    command
}

async fn run(mut command: tokio::process::Command, step: &str) -> Result<Vec<u8>, ErrorInfo> {
    let failed = |reason: String| ErrorInfo::new("snapshot", format!("git {step}: {reason}"));
    let output = tokio::time::timeout(WAIT, command.output())
        .await
        .map_err(|_| failed(format!("took more than {} s", WAIT.as_secs())))?
        .map_err(|error| failed(error.to_string()))?;
    if !output.status.success() {
        return Err(failed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(output.stdout)
}

/// Copies the files of `folder` that its `.gitignore` files do not leave
/// out into `store` and returns the copy's tree.
pub(crate) async fn take(store: &Path, folder: &Path) -> Result<String, ErrorInfo> {
    let _one = ONE_AT_A_TIME.lock().await;
    take_locked(store, folder).await
}

async fn take_locked(store: &Path, folder: &Path) -> Result<String, ErrorInfo> {
    if !store.join("HEAD").exists() {
        std::fs::create_dir_all(store)
            .map_err(|error| ErrorInfo::new("snapshot", error.to_string()))?;
        run(git(store, folder, &["init", "-q"]), "init").await?;
    }
    run(git(store, folder, &["add", "-A", "--", "."]), "add").await?;
    let tree = run(git(store, folder, &["write-tree"]), "write-tree").await?;
    Ok(String::from_utf8_lossy(&tree).trim().to_string())
}

/// The most of a diff's text kept, in bytes; the rest is cut at a line's
/// end (docs/evidence/turn-diff.md).
pub(crate) const DIFF_TEXT_MAX: usize = 256 * 1024;

/// What changed between two copies of a project's files.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Diff {
    pub files: Vec<ChangedFile>,
    /// The unified diff, at most [`DIFF_TEXT_MAX`] bytes.
    pub text: String,
    /// Whether `text` was cut.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ChangedFile {
    pub path: String,
    /// `added`, `modified` or `deleted`.
    pub status: &'static str,
    /// Lines added and removed; `None` for a binary file.
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

/// What changed in the files of `folder` since the copy `tree`, taken now
/// as [`take`] does.
pub(crate) async fn diff(store: &Path, folder: &Path, tree: &str) -> Result<Diff, ErrorInfo> {
    let now = {
        let _one = ONE_AT_A_TIME.lock().await;
        take_locked(store, folder).await?
    };
    diff_trees(store, folder, tree, &now).await
}

/// What changed from the copy `from` to the copy `to`.
pub(crate) async fn diff_trees(
    store: &Path,
    folder: &Path,
    from: &str,
    to: &str,
) -> Result<Diff, ErrorInfo> {
    let diff_tree = |extra: &[&str]| {
        let mut arguments = vec!["diff-tree", "-r", "--no-renames", "--no-ext-diff"];
        arguments.extend_from_slice(extra);
        arguments.extend_from_slice(&[from, to]);
        run(git(store, folder, &arguments), "diff-tree")
    };
    let statuses = diff_tree(&["-z", "--name-status"]).await?;
    let mut files = Vec::new();
    let mut fields = statuses
        .split(|&byte| byte == 0)
        .filter(|field| !field.is_empty());
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        files.push(ChangedFile {
            path: String::from_utf8_lossy(path).into_owned(),
            status: match status.first() {
                Some(b'A') => "added",
                Some(b'D') => "deleted",
                _ => "modified",
            },
            added: None,
            removed: None,
        });
    }
    if files.is_empty() {
        return Ok(Diff {
            files,
            text: String::new(),
            truncated: false,
        });
    }
    let counts = diff_tree(&["-z", "--numstat"]).await?;
    for entry in counts
        .split(|&byte| byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let entry = String::from_utf8_lossy(entry);
        let mut parts = entry.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if let Some(file) = files.iter_mut().find(|file| file.path == path) {
            file.added = added.parse().ok();
            file.removed = removed.parse().ok();
        }
    }
    let mut text = diff_tree(&["-p", "--no-color", "--no-textconv"]).await?;
    let truncated = text.len() > DIFF_TEXT_MAX;
    if truncated {
        let end = text[..DIFF_TEXT_MAX]
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |line_end| line_end + 1);
        text.truncate(end);
    }
    Ok(Diff {
        files,
        text: String::from_utf8_lossy(&text).into_owned(),
        truncated,
    })
}

/// Puts the files of `folder` back as they were in the copy `tree`: those
/// changed or deleted since are written again, and those added since are
/// deleted. Files the `.gitignore` files leave out are not touched.
/// Returns how many files were added since and deleted.
pub(crate) async fn restore(store: &Path, folder: &Path, tree: &str) -> Result<usize, ErrorInfo> {
    let _one = ONE_AT_A_TIME.lock().await;
    let now = take_locked(store, folder).await?;
    let added = run(
        git(
            store,
            folder,
            &[
                "diff-tree",
                "-r",
                "-z",
                "--name-only",
                "--no-renames",
                "--diff-filter=A",
                tree,
                &now,
            ],
        ),
        "diff-tree",
    )
    .await?;
    run(git(store, folder, &["read-tree", tree]), "read-tree").await?;
    run(
        git(store, folder, &["checkout-index", "-a", "-f"]),
        "checkout-index",
    )
    .await?;
    let mut removed = 0;
    for path in added
        .split(|&byte| byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = folder.join(String::from_utf8_lossy(path).as_ref());
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ErrorInfo::new(
                    "snapshot",
                    format!("{} was not deleted: {error}", path.display()),
                ))
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let folder = std::env::temp_dir().join(format!(
            "inocode-snapshot-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&folder).expect("folder");
        folder
    }

    #[tokio::test]
    async fn a_copy_puts_back_changed_deleted_and_added_files() {
        let (project, store) = (folder("project"), folder("store").join("copies"));
        std::fs::create_dir_all(project.join("src")).expect("src");
        std::fs::write(project.join("src/kept.txt"), "before\r\n").expect("write");
        std::fs::write(project.join("gone.txt"), "here").expect("write");
        std::fs::write(project.join(".gitignore"), "ignored/\n").expect("write");
        std::fs::create_dir_all(project.join("ignored")).expect("ignored");
        std::fs::write(project.join("ignored/cache"), "1").expect("write");
        let tree = take(&store, &project).await.expect("copy");

        std::fs::write(project.join("src/kept.txt"), "after").expect("write");
        std::fs::remove_file(project.join("gone.txt")).expect("remove");
        std::fs::write(project.join("src/new.txt"), "new").expect("write");
        std::fs::write(project.join("ignored/cache"), "2").expect("write");
        let removed = restore(&store, &project, &tree).await.expect("restore");

        assert_eq!(removed, 1);
        assert_eq!(
            std::fs::read(project.join("src/kept.txt")).expect("read"),
            b"before\r\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.join("gone.txt")).expect("read"),
            "here"
        );
        assert!(!project.join("src/new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(project.join("ignored/cache")).expect("read"),
            "2",
            "what .gitignore leaves out is not touched"
        );
        assert_eq!(take(&store, &project).await.expect("copy"), tree);
    }

    #[tokio::test]
    async fn a_diff_lists_each_changed_file_and_cuts_a_long_text() {
        let (project, store) = (folder("project"), folder("store").join("copies"));
        std::fs::write(project.join("kept.txt"), "one\ntwo\n").expect("write");
        std::fs::write(project.join("gone.txt"), "here\n").expect("write");
        let tree = take(&store, &project).await.expect("copy");
        assert_eq!(diff(&store, &project, &tree).await.expect("diff").files, []);

        std::fs::write(project.join("kept.txt"), "one\nthree\n").expect("write");
        std::fs::remove_file(project.join("gone.txt")).expect("remove");
        std::fs::write(project.join("new.bin"), [0u8, 1, 2]).expect("write");
        let changed = diff(&store, &project, &tree).await.expect("diff");
        let summary: Vec<_> = changed
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.status, file.added, file.removed))
            .collect();
        assert_eq!(
            summary,
            [
                ("gone.txt", "deleted", Some(0), Some(1)),
                ("kept.txt", "modified", Some(1), Some(1)),
                ("new.bin", "added", None, None),
            ]
        );
        assert!(changed.text.contains("-two\n+three\n"), "{}", changed.text);
        assert!(!changed.truncated);

        std::fs::write(project.join("long.txt"), "line\n".repeat(DIFF_TEXT_MAX / 4))
            .expect("write");
        let long = diff(&store, &project, &tree).await.expect("diff");
        assert!(long.truncated);
        assert!(long.text.len() <= DIFF_TEXT_MAX && long.text.ends_with('\n'));
    }
}
