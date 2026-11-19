//! Confines shell commands so they can write only in the project folder and
//! a private temporary folder. Reads, the network, and other processes are
//! not restricted. Where no confinement is available the command is not run.
//! The mechanisms are ported from DeepSeek Harness (MIT, see
//! THIRD_PARTY_NOTICES.md). Design and approval: docs/evidence/sandbox.md.

use std::path::PathBuf;
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

#[derive(Debug, Clone)]
struct Settings {
    enabled: bool,
    runner: Option<PathBuf>,
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Sets, once per process, whether commands are confined and which
/// executable acts as the runner (one that calls [`run`] when given
/// [`RUNNER_ARGUMENT`]). Without it, commands are confined and the current
/// executable is the runner.
pub fn configure(enabled: bool, runner: Option<PathBuf>) {
    let _ = SETTINGS.set(Settings { enabled, runner });
}

fn settings() -> &'static Settings {
    SETTINGS.get_or_init(|| Settings {
        enabled: true,
        runner: None,
    })
}

/// Whether shell commands are confined.
pub(crate) fn enabled() -> bool {
    settings().enabled
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
