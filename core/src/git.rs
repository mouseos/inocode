//! The project's git state as one line, read when a turn starts and sent
//! after the history: the branch, the commit and how many files are
//! changed, never names, addresses or file names. Design and sources:
//! docs/evidence/git-context.md.

use std::path::Path;
use std::time::Duration;

/// How long `git status` may take before the line is left out.
const WAIT: Duration = Duration::from_secs(3);

/// The line, or `None` when the folder is not in a git repository, git is
/// missing, or it does not answer in time.
pub(crate) async fn state(folder: &Path) -> Option<String> {
    let mut command = tokio::process::Command::new("git");
    command
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "--branch",
        ])
        .current_dir(folder)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
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
