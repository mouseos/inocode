use super::*;
use pretty_assertions::assert_eq;

fn vec_str(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

/// A project directory below a temp dir, plus a scope for it.
fn project() -> (tempfile::TempDir, std::path::PathBuf, DeleteScope) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(cwd.join("sub")).expect("create project");
    let scope = DeleteScope::new(
        &cwd,
        /*user_home*/ None,
        &[],
        /*case_insensitive*/ cfg!(windows),
        /*check_filesystem*/ true,
    )
    .expect("scope");
    (dir, cwd, scope)
}

fn posix(command: &[&str], scope: Option<&DeleteScope>) -> Option<DangerousCommandMatch> {
    dangerous_command_match_for_platform_scoped(
        &vec_str(command),
        DangerousCommandPlatform::Posix,
        scope,
    )
}

fn windows(command: &[&str], scope: Option<&DeleteScope>) -> Option<DangerousCommandMatch> {
    dangerous_command_match_for_platform_scoped(
        &vec_str(command),
        DangerousCommandPlatform::Windows,
        scope,
    )
}

#[test]
fn rm_inside_the_scope_is_routine() {
    let (_dir, cwd, scope) = project();
    let absolute = cwd.join("sub").join("out.bin");
    for command in [
        vec!["rm", "-f", "recording.mp4"],
        vec!["rm", "-rf", "build"],
        vec!["rm", "-r", "-f", "sub/out"],
        vec!["rm", "--force", "--recursive", "--", "-weird-name"],
        vec!["rm", "-f", absolute.to_str().unwrap()],
        vec!["env", "TARGET=x", "rm", "-rf", "build"],
    ] {
        assert_eq!(posix(&command, Some(&scope)), None, "{command:?}");
        // Without a scope the same commands stay dangerous.
        assert_eq!(
            posix(&command, None),
            Some(DangerousCommandMatch::ForcedRm),
            "{command:?}"
        );
    }
}

#[test]
fn rm_that_is_not_provably_contained_stays_dangerous() {
    let (dir, _cwd, scope) = project();
    let outside = dir.path().join("elsewhere");
    for command in [
        vec!["rm", "-rf", "/"],
        vec!["rm", "-rf", "."],
        vec!["rm", "-rf", ".."],
        vec!["rm", "-rf", "../elsewhere"],
        vec!["rm", "-rf", outside.to_str().unwrap()],
        vec!["rm", "-rf", "build", "../elsewhere"],
        vec!["rm", "-rf", "*"],
        vec!["rm", "-rf", "$TARGET"],
        vec!["rm", "-rf", "~/work"],
        vec!["rm", "-rf", ".git"],
        vec!["rm", "-rf"],
        vec!["rm", "-rf", "--no-preserve-root", "build"],
        vec!["rm", "--unknown-option", "-f", "build"],
        vec!["sudo", "rm", "-rf", "build"],
        vec!["env", "-C", "/", "rm", "-rf", "build"],
    ] {
        assert_eq!(
            posix(&command, Some(&scope)),
            Some(DangerousCommandMatch::ForcedRm),
            "{command:?}"
        );
    }
}

#[test]
fn shell_scripts_that_change_directory_do_not_use_the_scope() {
    let (_dir, _cwd, scope) = project();
    assert_eq!(posix(&["bash", "-lc", "rm -rf build"], Some(&scope)), None);
    assert_eq!(
        posix(&["bash", "-lc", "cd / && rm -rf build"], Some(&scope)),
        Some(DangerousCommandMatch::ForcedRm)
    );
    assert_eq!(
        posix(&["bash", "-lc", "pushd /etc; rm -rf build"], Some(&scope)),
        Some(DangerousCommandMatch::ForcedRm)
    );
}

#[test]
fn powershell_forced_delete_inside_the_scope_is_routine() {
    let (_dir, cwd, scope) = project();
    let absolute = cwd.join("sub").join("test.bin");
    let absolute_script = format!(
        "Remove-Item -LiteralPath '{}' -Force",
        absolute.to_str().unwrap()
    );
    for script in [
        "Remove-Item -LiteralPath 'artifacts/test.bin' -Force",
        "Remove-Item test.bin -Force",
        "Remove-Item -Path sub/out -Recurse -Force",
        "ri -Force sub/out",
        "Remove-Item -Force -ErrorAction SilentlyContinue sub/out",
        "Remove-Item sub/a.txt, sub/b.txt -Force",
        "Remove-Item -Path sub -Include '*.tmp' -Recurse -Force",
        absolute_script.as_str(),
    ] {
        let command = ["pwsh", "-NoProfile", "-Command", script];
        assert_eq!(windows(&command, Some(&scope)), None, "{script}");
        assert_eq!(
            windows(&command, None),
            Some(DangerousCommandMatch::ForcedDelete),
            "{script}"
        );
    }
}

#[test]
fn powershell_forced_delete_that_is_not_provably_contained_stays_dangerous() {
    let (dir, _cwd, scope) = project();
    let outside = dir.path().join("elsewhere.bin");
    let outside_script = format!(
        "Remove-Item -LiteralPath '{}' -Force",
        outside.to_str().unwrap()
    );
    for script in [
        "Remove-Item -Recurse -Force .",
        "Remove-Item -Recurse -Force ..",
        "Remove-Item ../elsewhere.bin -Force",
        "Remove-Item * -Force",
        "Remove-Item sub/*.tmp -Force",
        "Remove-Item $target -Force",
        "Remove-Item $env:TEMP/x -Force",
        "Remove-Item (Get-Item x) -Force",
        "rm -Force:$true sub/out",
        "Remove-Item sub/out -Force @args",
        "Remove-Item -Force",
        "Remove-Item -Force -Bogus sub/out",
        "Remove-Item -Fi sub/out -Force",
        "Remove-Item .git -Recurse -Force",
        "Remove-Item sub/out -Force; Remove-Item ../x -Force",
        "Remove-Item sub/out -Force; Remove-Item -Force",
        "Get-ChildItem sub | Remove-Item -Force",
        "Write-Host hi; Remove-Item ~/x -Force",
        outside_script.as_str(),
    ] {
        let command = ["pwsh", "-NoProfile", "-Command", script];
        assert_eq!(
            windows(&command, Some(&scope)),
            Some(DangerousCommandMatch::ForcedDelete),
            "{script}"
        );
    }
}

#[test]
fn scope_does_not_excuse_other_dangerous_powershell_commands() {
    let (_dir, _cwd, scope) = project();
    let command = ["pwsh", "-Command", "Start-Process https://example.com"];
    assert_eq!(
        windows(&command, Some(&scope)),
        Some(DangerousCommandMatch::UrlLaunch)
    );
}
