//! The project's git state as one line, read when a turn starts and sent
//! after the history: the branch, the commit and how many files are
//! changed, never names, addresses or file names. Design and sources:
//! docs/evidence/git-context.md.

use std::path::Path;
use std::time::Duration;

/// How long `git status` may take before the line is left out.
const WAIT: Duration = Duration::from_secs(3);

/// The file that reads as empty, given where git would read a config file
/// or a hooks folder, so that none is used.
const NOTHING: &str = if cfg!(windows) { "nul" } else { "/dev/null" };

/// `git` set up so that reading the state runs nothing the repository
/// names. The repository's `.git/config` can be written by a command in the
/// sandbox, while this runs outside it: the file-system monitor
/// (`core.fsmonitor`) and the hooks are turned off, the system and global
/// settings are not read, and submodules, whose own settings are not
/// checked, are not looked into (docs/evidence/git-context.md).
fn git(folder: &Path, arguments: &[&str]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    command
        .args([
            "--no-pager",
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
        ])
        .arg("-c")
        .arg(format!("core.hooksPath={NOTHING}"))
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", NOTHING)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(folder)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    command
}

/// Whether the repository's settings name a clean filter, which `git
/// status` runs on a file whose times changed to see whether its content
/// did; `None` when git cannot say.
async fn has_filters(folder: &Path) -> Option<bool> {
    let mut command = git(
        folder,
        &["config", "--get-regexp", r"^filter\..*\.(clean|process)$"],
    );
    let output = tokio::time::timeout(WAIT, command.output())
        .await
        .ok()?
        .ok()?;
    match output.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// The line, or `None` when the folder is not in a git repository, git is
/// missing, it does not answer in time, or the repository names a filter
/// program that reading the state could run.
pub(crate) async fn state(folder: &Path) -> Option<String> {
    if has_filters(folder).await? {
        return None;
    }
    let mut command = git(
        folder,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--ignore-submodules=all",
        ],
    );
    let output = tokio::time::timeout(WAIT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(line(&String::from_utf8_lossy(&output.stdout)))
}

fn line(status: &str) -> String {
    let (mut branch, mut commit) = (None, None);
    let (mut staged, mut changed, mut untracked, mut conflicted) = (0, 0, 0, 0);
    for entry in status.lines() {
        if let Some(name) = entry.strip_prefix("# branch.head ") {
            branch = Some(name);
        } else if let Some(oid) = entry.strip_prefix("# branch.oid ") {
            commit = Some(oid);
        } else if let Some(rest) = entry.strip_prefix("1 ").or(entry.strip_prefix("2 ")) {
            let mut codes = rest.chars();
            if codes.next().is_some_and(|code| code != '.') {
                staged += 1;
            }
            if codes.next().is_some_and(|code| code != '.') {
                changed += 1;
            }
        } else if entry.starts_with("u ") {
            conflicted += 1;
        } else if entry.starts_with("? ") {
            untracked += 1;
        }
    }
    let at = match (branch, commit) {
        (Some("(detached)"), Some(oid)) => format!("a detached HEAD at {}", short(oid)),
        (Some(name), Some("(initial)")) => format!("branch {name}, with no commits yet"),
        (Some(name), Some(oid)) => format!("branch {name} at {}", short(oid)),
        _ => "an unknown branch".to_string(),
    };
    let mut counts = Vec::new();
    for (count, what) in [
        (staged, "staged"),
        (changed, "changed and not staged"),
        (conflicted, "in conflict"),
        (untracked, "untracked"),
    ] {
        if count > 0 {
            counts.push(format!("{count} {what}"));
        }
    }
    let files = if counts.is_empty() {
        "no changed files".to_string()
    } else {
        format!("files: {}", counts.join(", "))
    };
    format!(
        "Git, when this turn started: {at}; {files}. Run git yourself for the current state \
         and the file names."
    )
}

fn short(oid: &str) -> &str {
    oid.get(..7).unwrap_or(oid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_counts_files_and_names_no_file() {
        let status = "# branch.oid 2530003abcdef0123456789\n# branch.head main\n\
            # branch.upstream origin/main\n# branch.ab +0 -0\n\
            1 .M N... 100644 100644 100644 aa bb secret-plan.txt\n\
            1 M. N... 100644 100644 100644 aa bb staged.rs\n\
            1 MM N... 100644 100644 100644 aa bb both.rs\n\
            2 R. N... 100644 100644 100644 aa bb R100 new.rs\told.rs\n\
            u UU N... 100644 100644 100644 100644 aa bb cc conflict.rs\n\
            ? private notes.md\n? other.txt\n";
        let line = line(status);
        assert_eq!(
            line,
            "Git, when this turn started: branch main at 2530003; files: 3 staged, \
             2 changed and not staged, 1 in conflict, 2 untracked. Run git yourself for the \
             current state and the file names."
        );
        assert!(!line.contains("secret") && !line.contains("origin"));
    }

    #[test]
    fn a_new_repository_and_a_detached_head_are_named() {
        assert!(line("# branch.oid (initial)\n# branch.head main\n")
            .contains("branch main, with no commits yet; no changed files."));
        assert!(
            line("# branch.oid 0123456789abcdef\n# branch.head (detached)\n")
                .contains("a detached HEAD at 0123456")
        );
    }

    /// A repository with one committed file and the settings `config`.
    fn repository(folder: &Path, config: &[(&str, &str)]) -> bool {
        let git = |arguments: &[&str]| {
            std::process::Command::new("git")
                .args(arguments)
                .current_dir(folder)
                .output()
                .is_ok_and(|output| output.status.success())
        };
        std::fs::create_dir_all(folder).expect("dir");
        if !git(&["init", "-q", "-b", "work"]) {
            return false;
        }
        std::fs::write(folder.join("a.txt"), "a").expect("write");
        std::fs::write(folder.join(".gitattributes"), "*.txt filter=probe\n").expect("write");
        assert!(git(&["add", "."]));
        assert!(git(&[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-q",
            "-m",
            "a"
        ]));
        for (key, value) in config {
            assert!(git(&["config", key, value]));
        }
        // Newer times make git look at the content again.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(folder.join("a.txt"), "a").expect("write");
        true
    }

    #[tokio::test]
    async fn reading_the_state_runs_no_program_the_repository_names() {
        let dir = tempfile::tempdir().expect("dir");
        let monitored = dir.path().join("monitored");
        if !repository(
            &monitored,
            &[("core.fsmonitor", "echo ran > fsmonitor-ran")],
        ) {
            return;
        }
        let plain = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&monitored)
            .output()
            .expect("git");
        assert!(plain.status.success());
        assert!(
            monitored.join("fsmonitor-ran").exists(),
            "a plain git status runs the monitor, so the test means something"
        );
        std::fs::remove_file(monitored.join("fsmonitor-ran")).expect("remove");
        assert!(state(&monitored).await.is_some());
        assert!(!monitored.join("fsmonitor-ran").exists());

        let filtered = dir.path().join("filtered");
        assert!(repository(
            &filtered,
            &[("filter.probe.clean", "echo ran > ../filter-ran; cat")]
        ));
        assert_eq!(state(&filtered).await, None);
        assert!(!dir.path().join("filter-ran").exists());
    }

    #[tokio::test]
    async fn a_real_repository_is_read_and_a_plain_folder_is_not() {
        let dir = tempfile::tempdir().expect("dir");
        let plain = dir.path().join("plain");
        std::fs::create_dir(&plain).expect("dir");
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).expect("dir");
        let git = |arguments: &[&str]| {
            std::process::Command::new("git")
                .args(arguments)
                .current_dir(&repo)
                .output()
                .expect("git")
        };
        if !git(&["init", "-q", "-b", "work"]).status.success() {
            return;
        }
        std::fs::write(repo.join("a.txt"), "a").expect("write");
        // The plain folder is inside the temporary folder, which itself is
        // not in a repository on the machines the tests run on.
        assert_eq!(state(&plain).await, None);
        assert_eq!(
            state(&repo).await.as_deref(),
            Some(
                "Git, when this turn started: branch work, with no commits yet; files: \
                 1 untracked. Run git yourself for the current state and the file names."
            )
        );
    }
}
