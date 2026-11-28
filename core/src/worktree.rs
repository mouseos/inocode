//! A thread's own git worktree, kept beside the database
//! (docs/evidence/thread-folders.md).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::ErrorInfo;

/// How long one git command may take.
const WAIT: Duration = Duration::from_secs(120);

/// Where `thread`'s worktree goes, under the store's files folder.
pub(crate) fn place(files: &Path, thread: i64) -> PathBuf {
    files.join("worktrees").join(format!("thread-{thread}"))
}

/// The branch a thread's worktree gets when none is named.
pub(crate) fn default_branch(thread: i64) -> String {
    format!("inocode/thread-{thread}")
}

/// Runs git in `folder` as [`crate::git`] does, so that nothing the
/// repository names runs; the error is git's own message.
async fn run(folder: &Path, arguments: &[&str]) -> Result<String, String> {
    let mut command = crate::git::git(folder, arguments);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = tokio::time::timeout(WAIT, command.output())
        .await
        .map_err(|_| format!("git {} did not end in {}s", arguments[0], WAIT.as_secs()))?
        .map_err(|error| format!("git could not run: {error}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(match said.is_empty() {
            true => format!("git {} failed", arguments[0]),
            false => said,
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Adds a worktree of the repository at `repo` in `target`, on a new
/// `branch` started from `base` (the repository's `HEAD` when `None`).
pub(crate) async fn add(
    repo: &Path,
    target: &Path,
    branch: &str,
    base: Option<&str>,
) -> Result<(), ErrorInfo> {
    let failed = |message: String| ErrorInfo::new("worktree_failed", message);
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| failed(format!("{}: {error}", parent.display())))?;
    }
    let target_text = target.to_string_lossy().into_owned();
    run(
        repo,
        &[
            "worktree",
            "add",
            "-b",
            branch,
            &target_text,
            base.unwrap_or("HEAD"),
        ],
    )
    .await
    .map(drop)
    .map_err(failed)
}

/// Removes the worktree at `target` when it has no changes and no files git
/// does not track; its branch is kept. `Err` says why it was kept.
pub(crate) async fn remove_if_clean(target: &Path) -> Result<(), String> {
    let changes = run(target, &["status", "--porcelain", "--untracked-files=all"]).await?;
    if !changes.is_empty() {
        return Err("it has changes that are not committed".into());
    }
    let common = PathBuf::from(
        run(
            target,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?,
    );
    // From the main working tree, whose `.git` folder is the common one.
    let main = match common.file_name().is_some_and(|name| name == ".git") {
        true => common.parent().unwrap_or(&common).to_path_buf(),
        false => common,
    };
    let target_text = target.to_string_lossy().into_owned();
    run(&main, &["worktree", "remove", &target_text])
        .await
        .map(drop)
}
