#![cfg(windows)]

use std::path::Path;
use std::process::Command;

#[test]
fn a_confined_command_writes_only_in_the_project_and_temp_folders() {
    let base = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("inocode-sandbox-{}", std::process::id()));
    let workspace = base.join("work");
    let temp = base.join("temp");
    let outside = base.join("outside");
    for folder in [&workspace, &temp, &outside] {
        std::fs::create_dir_all(folder).expect("folder");
    }
    let script = format!(
        "Set-Content -Path '{}' -Value ok -ErrorAction Stop; \
         Set-Content -Path (Join-Path $env:TEMP 'temp.txt') -Value ok -ErrorAction Stop; \
         try {{ Set-Content -Path '{}' -Value bad -ErrorAction Stop; exit 3 }} catch {{ exit 7 }}",
        workspace.join("work.txt").display(),
        outside.join("outside.txt").display(),
    );

    let status = Command::new(env!("CARGO_BIN_EXE_inocode-server"))
        .args(["sandbox-run", "--workspace"])
        .arg(&workspace)
        .arg("--temp")
        .arg(&temp)
        .args([
            "--",
            "powershell.exe",
            "-NoLogo",
            "-NoProfile",
            "-Command",
            &script,
        ])
        .current_dir(&workspace)
        .status()
        .expect("runner");

    assert_eq!(status.code(), Some(7));
    assert!(workspace.join("work.txt").is_file());
    assert!(temp.join("temp.txt").is_file());
    assert!(!outside.join("outside.txt").exists());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_confined_command_cannot_read_the_hidden_folder_but_its_roots_inside_it() {
    let base = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("inocode-hidden-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("work");
    let home = base.join("home");
    let temp = home.join("files").join("logs").join("thread-1").join("tmp");
    for folder in [&workspace, &temp] {
        std::fs::create_dir_all(folder).expect("folder");
    }
    let secret = home.join("server-token");
    std::fs::write(&secret, "secret").expect("secret");
    inocode_core::sandbox::hide(&home).expect("hide");
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "secret");

    let script = format!(
        "Set-Content -Path (Join-Path $env:TEMP 'temp.txt') -Value ok -ErrorAction Stop; \
         if ((Get-Content -Path (Join-Path $env:TEMP 'temp.txt') -ErrorAction Stop) -ne 'ok') {{ exit 4 }}; \
         try {{ Get-Content -Path '{}' -ErrorAction Stop | Out-Null; exit 3 }} catch {{ exit 7 }}",
        secret.display(),
    );
    let status = Command::new(env!("CARGO_BIN_EXE_inocode-server"))
        .args(["sandbox-run", "--workspace"])
        .arg(&workspace)
        .arg("--temp")
        .arg(&temp)
        .args([
            "--",
            "powershell.exe",
            "-NoLogo",
            "-NoProfile",
            "-Command",
            &script,
        ])
        .current_dir(&workspace)
        .status()
        .expect("runner");

    assert_eq!(status.code(), Some(7), "the token was readable");
    assert!(temp.join("temp.txt").is_file());
    let _ = std::fs::remove_dir_all(&base);
}
