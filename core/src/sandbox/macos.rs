//! Confinement on macOS with Seatbelt, ported from DeepSeek Harness
//! `packages/sandbox/sandbox-local/src/profiles.ts` (`seatbeltProfileArgs`)
//! and `packages/sandbox/sandbox-local/src/index.ts` (the Seatbelt runner).

use std::path::Path;

use super::Roots;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// `sandbox-exec` with a profile that allows everything except writing
/// outside `/dev/null` and the roots, followed by `argv`.
pub(super) fn confine(argv: &[String], roots: &Roots) -> Result<Vec<String>, String> {
    if !Path::new(SANDBOX_EXEC).is_file() {
        return Err(format!(
            "{SANDBOX_EXEC} was not found, so the command was not run"
        ));
    }
    let folder = canonical(&roots.folder)?;
    let temp = canonical(&roots.temp)?;
    let hidden = match super::hidden() {
        Some(hidden) => Some(canonical(hidden)?),
        None => None,
    };
    let mut confined = vec![
        SANDBOX_EXEC.to_string(),
        "-p".to_string(),
        profile(&[folder, temp], hidden.as_deref()),
        "--".to_string(),
    ];
    confined.extend_from_slice(argv);
    Ok(confined)
}

/// Seatbelt compares resolved paths (`/var` is `/private/var`), so a root
/// is granted by its canonical spelling.
fn canonical(root: &Path) -> Result<String, String> {
    let resolved = std::fs::canonicalize(root)
        .map_err(|error| format!("could not resolve {}: {error}", root.display()))?;
    resolved
        .into_os_string()
        .into_string()
        .map_err(|path| format!("{} is not valid UTF-8", Path::new(&path).display()))
}

/// The later of two matching rules wins, so the roots are readable again
/// after the hidden folder that holds them is denied.
fn profile(roots: &[String], hidden: Option<&str>) -> String {
    let mut forms = vec![
        "(version 1)".to_string(),
        "(allow default)".to_string(),
        "(deny file-write*)".to_string(),
        format!("(allow file-write* (literal {}))", sbpl_string("/dev/null")),
    ];
    let mut subpaths: Vec<String> = Vec::new();
    for root in roots {
        let subpath = format!("(subpath {})", sbpl_string(root));
        if !subpaths.contains(&subpath) {
            subpaths.push(subpath);
        }
    }
    if !subpaths.is_empty() {
        forms.push(format!("(allow file-write* {})", subpaths.join(" ")));
    }
    if let Some(hidden) = hidden {
        forms.push(format!(
            "(deny file-read* (subpath {}))",
            sbpl_string(hidden)
        ));
        if !subpaths.is_empty() {
            forms.push(format!("(allow file-read* {})", subpaths.join(" ")));
        }
    }
    forms.join(" ")
}

fn sbpl_string(path: &str) -> String {
    format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn profile_denies_writes_except_dev_null_and_quoted_roots() {
        let roots = [
            r#"/a "b"\c"#.to_string(),
            "/tmp/t".to_string(),
            "/tmp/t".to_string(),
        ];
        assert_eq!(
            profile(&roots, None),
            r#"(version 1) (allow default) (deny file-write*) (allow file-write* (literal "/dev/null")) (allow file-write* (subpath "/a \"b\"\\c") (subpath "/tmp/t"))"#
        );
    }

    #[test]
    fn the_hidden_folder_is_unreadable_except_the_roots_in_it() {
        let roots = ["/home/a/.inocode/t".to_string()];
        assert_eq!(
            profile(&roots, Some("/home/a/.inocode")),
            r#"(version 1) (allow default) (deny file-write*) (allow file-write* (literal "/dev/null")) (allow file-write* (subpath "/home/a/.inocode/t")) (deny file-read* (subpath "/home/a/.inocode")) (allow file-read* (subpath "/home/a/.inocode/t"))"#
        );
    }

    #[test]
    fn argv_runs_the_command_under_sandbox_exec() {
        let folder = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let roots = Roots {
            folder: folder.path().to_path_buf(),
            temp: temp.path().to_path_buf(),
        };
        let argv = confine(&["true".to_string()], &roots).unwrap();
        assert_eq!(argv[0], SANDBOX_EXEC);
        assert_eq!(argv[1], "-p");
        assert!(argv[2].contains(&format!(
            "(subpath {})",
            sbpl_string(&canonical(folder.path()).unwrap())
        )));
        assert!(argv[2].contains("/private/"));
        assert_eq!(&argv[3..], ["--", "true"]);
    }

    fn run_confined(roots: &Roots, script: &str, target: &Path) -> std::process::ExitStatus {
        let argv = confine(
            &[
                "sh".to_string(),
                "-c".to_string(),
                script.to_string(),
                "sh".to_string(),
                target.to_str().unwrap().to_string(),
            ],
            roots,
        )
        .unwrap();
        Command::new(&argv[0]).args(&argv[1..]).status().unwrap()
    }

    #[test]
    fn confined_command_writes_only_in_the_roots() {
        let folder = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let roots = Roots {
            folder: folder.path().to_path_buf(),
            temp: temp.path().to_path_buf(),
        };
        let write = "echo hi > \"$1\"";

        let inside = folder.path().join("a.txt");
        assert!(run_confined(&roots, write, &inside).success());
        assert_eq!(std::fs::read_to_string(&inside).unwrap(), "hi\n");

        let in_temp = temp.path().join("b.txt");
        assert!(run_confined(&roots, write, &in_temp).success());
        assert!(in_temp.exists());

        let blocked = outside.path().join("c.txt");
        assert!(!run_confined(&roots, write, &blocked).success());
        assert!(!blocked.exists());

        let discard = "echo hi > /dev/null && exit 7";
        let status = run_confined(&roots, discard, &inside);
        assert_eq!(status.code(), Some(7));
    }
}
