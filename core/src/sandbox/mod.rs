//! Confines shell commands so they can write only in the project folder and
//! a private temporary folder, and, where the OS allows it, keeps them from
//! reading the folder of inocode's own secrets ([`hide`]). Other reads, the
//! network, and other processes are not restricted. Where no confinement is
//! available the command is not run. The mechanisms are ported from
//! DeepSeek Harness (MIT, see THIRD_PARTY_NOTICES.md). Design and approval:
//! docs/evidence/sandbox.md and docs/evidence/server-auth.md.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// The argument that makes an inocode executable act as the runner.
pub const RUNNER_ARGUMENT: &str = "sandbox-run";
/// Every failure of the runner itself starts its message with this and exits
/// with [`RUNNER_FAILURE`], so it is told apart from the command's own exit.
pub(crate) const RUNNER_PREFIX: &str = "inocode-sandbox:";
pub(crate) const RUNNER_FAILURE: i32 = 127;

/// The folders a confined command may write in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Roots {
    pub folder: PathBuf,
    pub temp: PathBuf,
}

#[derive(Debug)]
struct Settings {
    enabled: AtomicBool,
    runner: Option<PathBuf>,
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// The folder confined commands may not read, set once by [`hide`].
static HIDDEN: OnceLock<PathBuf> = OnceLock::new();

/// Sets, once per process, whether commands are confined and which
/// executable acts as the runner (one that calls [`run`] when given
/// [`RUNNER_ARGUMENT`]). Without it, commands are confined and the current
/// executable is the runner.
pub fn configure(enabled: bool, runner: Option<PathBuf>) {
    let _ = SETTINGS.set(Settings {
        enabled: AtomicBool::new(enabled),
        runner,
    });
}

fn settings() -> &'static Settings {
    SETTINGS.get_or_init(|| Settings {
        enabled: AtomicBool::new(true),
        runner: None,
    })
}

/// Whether shell commands are confined.
pub fn enabled() -> bool {
    settings().enabled.load(Ordering::SeqCst)
}

/// Turns the confinement of the commands started from now on on or off.
pub fn set_enabled(enabled: bool) {
    settings().enabled.store(enabled, Ordering::SeqCst);
}

/// Keeps confined commands from reading `folder` (inocode's own folder,
/// with the credentials and the server's token), except the roots inside
/// it that they write to. On Windows the folder is labelled at once, so a
/// failure is returned; elsewhere the rule is added to each command's
/// confinement. Where the OS gives no way to do it (Landlock without
/// bwrap), commands can still read the folder.
pub fn hide(folder: &Path) -> Result<(), String> {
    let folder = std::fs::canonicalize(folder)
        .map_err(|error| format!("could not resolve {}: {error}", folder.display()))?;
    #[cfg(windows)]
    windows::hide(&folder)?;
    let _ = HIDDEN.set(folder);
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn hidden() -> Option<&'static Path> {
    HIDDEN.get().map(PathBuf::as_path)
}

fn runner() -> Result<PathBuf, String> {
    match &settings().runner {
        Some(runner) => Ok(runner.clone()),
        None => std::env::current_exe()
            .map_err(|error| format!("could not find the sandbox runner: {error}")),
    }
}

/// The program and arguments that run `argv` confined to `roots`, or `argv`
/// itself when confinement is turned off.
pub(crate) fn confine(argv: &[String], roots: &Roots) -> Result<Vec<String>, String> {
    if !enabled() {
        return Ok(argv.to_vec());
    }
    for root in [&roots.folder, &roots.temp] {
        if !root.is_dir() {
            return Err(format!("{} is not a folder", root.display()));
        }
    }
    platform_confine(argv, roots)
}

#[cfg(target_os = "linux")]
fn platform_confine(argv: &[String], roots: &Roots) -> Result<Vec<String>, String> {
    linux::confine(argv, roots, &runner()?)
}

#[cfg(target_os = "macos")]
fn platform_confine(argv: &[String], roots: &Roots) -> Result<Vec<String>, String> {
    let _ = runner;
    macos::confine(argv, roots)
}

#[cfg(windows)]
fn platform_confine(argv: &[String], roots: &Roots) -> Result<Vec<String>, String> {
    windows::confine(argv, roots, &runner()?)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_confine(_argv: &[String], _roots: &Roots) -> Result<Vec<String>, String> {
    let _ = runner;
    Err("no sandbox is available on this operating system, so the command was not run".into())
}

/// The runner: `args` are the arguments after [`RUNNER_ARGUMENT`]. Returns
/// the exit code of the confined command, or [`RUNNER_FAILURE`] after
/// printing a message that starts with [`RUNNER_PREFIX`].
pub fn run(args: &[String]) -> i32 {
    let result = platform_run(args);
    match result {
        Ok(code) => code,
        Err(message) => {
            eprintln!("{RUNNER_PREFIX} {message}");
            RUNNER_FAILURE
        }
    }
}

#[cfg(target_os = "linux")]
fn platform_run(args: &[String]) -> Result<i32, String> {
    linux::run(args)
}

#[cfg(windows)]
fn platform_run(args: &[String]) -> Result<i32, String> {
    windows::run(args)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn platform_run(_args: &[String]) -> Result<i32, String> {
    Err("this operating system does not use the runner".into())
}
