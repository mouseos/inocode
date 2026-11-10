use crate::bash::parse_shell_lc_literal_commands;
#[path = "windows_dangerous_commands.rs"]
mod windows_dangerous_commands;
pub use windows_dangerous_commands::powershell_commands_are_independent;

pub use super::delete_scope::DeleteScope;

/// The platform whose command semantics should be used for safety checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DangerousCommandPlatform {
    /// POSIX command and path semantics.
    Posix,
    /// Windows command and path semantics.
    Windows,
}

impl DangerousCommandPlatform {
    /// Returns the platform where the classifier is running.
    pub fn host() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        }
    }
}

/// Identifies the dangerous-command rule matched by a command invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DangerousCommandMatch {
    /// An `rm` invocation includes the force option.
    ForcedRm,
    /// A forced PowerShell delete (for example `Remove-Item -Force`) that is not provably limited to
    /// literal paths inside the working directory or a temporary directory.
    ForcedDelete,
    /// The command hands a URL to the shell or a browser (for example `Start-Process <url>`,
    /// `explorer <url>`, `cmd /c start <url>`). It opens a program on the user's desktop but does
    /// not change files, so unrestricted sessions that disable approvals may still run it.
    UrlLaunch,
    /// Another dangerous-command rule matched.
    Other,
}

const MAX_DANGEROUS_COMMAND_WRAPPER_DEPTH: usize = 8;

/// Returns the dangerous-command rule matched by an already-tokenized command.
pub fn dangerous_command_match(command: &[String]) -> Option<DangerousCommandMatch> {
    dangerous_command_match_for_platform(command, DangerousCommandPlatform::host())
}

/// Returns the dangerous-command rule matched using the target platform's semantics.
pub fn dangerous_command_match_for_platform(
    command: &[String],
    platform: DangerousCommandPlatform,
) -> Option<DangerousCommandMatch> {
    dangerous_command_match_for_platform_scoped(command, platform, /*scope*/ None)
}

/// Like [`dangerous_command_match_for_platform`], but a forced delete whose targets are all
/// literal paths contained in `scope` is not reported as dangerous.
pub fn dangerous_command_match_for_platform_scoped(
    command: &[String],
    platform: DangerousCommandPlatform,
    scope: Option<&DeleteScope>,
) -> Option<DangerousCommandMatch> {
    dangerous_command_match_with_depth(command, /*wrapper_depth*/ 0, platform, scope)
}

fn dangerous_command_match_with_depth(
    command: &[String],
    wrapper_depth: usize,
    platform: DangerousCommandPlatform,
    scope: Option<&DeleteScope>,
) -> Option<DangerousCommandMatch> {
    if wrapper_depth > MAX_DANGEROUS_COMMAND_WRAPPER_DEPTH {
        return Some(DangerousCommandMatch::Other);
    }

    if let Some(dangerous_match) =
        dangerous_command_match_for_exec(command, wrapper_depth, platform, scope)
    {
        return Some(dangerous_match);
    }

    // Support shell scripts where any literal command might be dangerous,
    // including commands nested in control flow or substitutions.
    if let Some(commands) = parse_shell_lc_literal_commands(command) {
        // Relative targets are resolved against the working directory, so a script that can change
        // directory cannot rely on the scope.
        let scope = if commands
            .iter()
            .any(|command| changes_working_directory(command, platform))
        {
            None
        } else {
            scope
        };
        if let Some(dangerous_match) = commands.iter().find_map(|command| {
            dangerous_command_match_with_depth(command, wrapper_depth + 1, platform, scope)
        }) {
            return Some(dangerous_match);
        }
    }

    if platform == DangerousCommandPlatform::Windows
        && let Some(dangerous_match) =
            windows_dangerous_commands::dangerous_command_windows_match(command, scope)
    {
        return Some(dangerous_match);
    }

    None
}

/// Returns the PowerShell-specific rule matched using the target platform's semantics.
pub fn dangerous_powershell_words_match(
    command: &[String],
    platform: DangerousCommandPlatform,
) -> Option<DangerousCommandMatch> {
    dangerous_powershell_words_match_scoped(command, platform, /*scope*/ None)
}

/// Like [`dangerous_powershell_words_match`] with a [`DeleteScope`].
pub fn dangerous_powershell_words_match_scoped(
    command: &[String],
    platform: DangerousCommandPlatform,
    scope: Option<&DeleteScope>,
) -> Option<DangerousCommandMatch> {
    if platform == DangerousCommandPlatform::Windows {
        windows_dangerous_commands::powershell_words_match(command, scope)
    } else {
        None
    }
}

/// Classifies a strictly lowered command from an independent literal PowerShell script.
pub fn dangerous_powershell_literal_words_match(
    command: &[String],
) -> Option<DangerousCommandMatch> {
    dangerous_powershell_literal_words_match_scoped(command, /*scope*/ None)
}

/// Like [`dangerous_powershell_literal_words_match`] with a [`DeleteScope`].
pub fn dangerous_powershell_literal_words_match_scoped(
    command: &[String],
    scope: Option<&DeleteScope>,
) -> Option<DangerousCommandMatch> {
    windows_dangerous_commands::powershell_literal_words_match(command, scope)
}

fn executable_name_lookup_key(raw: &str, platform: DangerousCommandPlatform) -> Option<String> {
    match platform {
        DangerousCommandPlatform::Posix => raw
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        DangerousCommandPlatform::Windows => {
            let name = raw
                .rsplit(['/', '\\'])
                .next()
                .filter(|name| !name.is_empty())?;
            let name = match name.as_bytes() {
                [drive, b':', ..] if drive.is_ascii_alphabetic() => &name[2..],
                _ => name,
            };
            let name = name.to_ascii_lowercase();
            for suffix in [".exe", ".cmd", ".bat", ".com"] {
                if let Some(stripped) = name.strip_suffix(suffix) {
                    return Some(stripped.to_string());
                }
            }
            (!name.is_empty()).then_some(name)
        }
    }
}

fn dangerous_command_match_for_exec(
    command: &[String],
    wrapper_depth: usize,
    platform: DangerousCommandPlatform,
    scope: Option<&DeleteScope>,
) -> Option<DangerousCommandMatch> {
    let cmd0 = command
        .first()
        .and_then(|command| executable_name_lookup_key(command, platform));

    match cmd0.as_deref() {
        Some("rm") if rm_args_include_force_option(&command[1..]) => {
            let contained = scope.is_some_and(|scope| {
                rm_targets(&command[1..]).is_some_and(|targets| scope.contains_all(&targets))
            });
            (!contained).then_some(DangerousCommandMatch::ForcedRm)
        }

        // For sudo <cmd>, simply check <cmd>. Running as another user is never routine cleanup,
        // so the scope does not apply.
        Some("sudo") => dangerous_command_match_with_depth(
            &command[1..],
            wrapper_depth + 1,
            platform,
            /*scope*/ None,
        ),

        // Skip environment assignments before checking the command run by env.
        Some("env") => dangerous_command_match_for_env(command, wrapper_depth, platform, scope),

        // A trap action is shell source stored in the first operand.
        Some("trap") => dangerous_command_match_for_trap(command, wrapper_depth, platform, scope),

        // ── anything else ─────────────────────────────────────────────────
        _ => None,
    }
}

fn dangerous_command_match_for_env(
    command: &[String],
    wrapper_depth: usize,
    platform: DangerousCommandPlatform,
    scope: Option<&DeleteScope>,
) -> Option<DangerousCommandMatch> {
    let mut command_index = 1;
    let mut scope = scope;
    while let Some(argument) = command.get(command_index) {
        if argument == "--" {
            command_index += 1;
            break;
        }
        if matches!(argument.as_str(), "-i" | "--ignore-environment")
            || argument
                .split_once('=')
                .is_some_and(|(name, _)| !name.is_empty() && !name.starts_with('-'))
        {
            command_index += 1;
            continue;
        }
        // env -C DIR / --chdir changes directory before running the command, so relative
        // targets no longer resolve against the working directory.
        if argument == "-C" || argument == "--chdir" {
            scope = None;
            command_index += 2;
            continue;
        }
        if argument.starts_with("--chdir=") || (argument.starts_with("-C") && argument.len() > 2) {
            scope = None;
            command_index += 1;
            continue;
        }
        if argument == "-u" || argument == "--unset" {
            command_index += 2;
            continue;
        }
        if argument.starts_with("--unset=") {
            command_index += 1;
            continue;
        }
        break;
    }
    dangerous_command_match_with_depth(
        &command[command_index..],
        wrapper_depth + 1,
        platform,
        scope,
    )
}

fn dangerous_command_match_for_trap(
    command: &[String],
    wrapper_depth: usize,
    platform: DangerousCommandPlatform,
    scope: Option<&DeleteScope>,
) -> Option<DangerousCommandMatch> {
    let mut action_index = 1;
    if command
        .get(action_index)
        .is_some_and(|argument| argument == "--")
    {
        action_index += 1;
    }
    let action = command
        .get(action_index)
        .filter(|action| !action.starts_with('-'))?;
    let shell_command = vec!["sh".to_string(), "-c".to_string(), action.clone()];
    dangerous_command_match_with_depth(&shell_command, wrapper_depth + 1, platform, scope)
}

/// True for shell builtins that change the directory later relative paths are resolved against.
fn changes_working_directory(command: &[String], platform: DangerousCommandPlatform) -> bool {
    command
        .first()
        .and_then(|word| executable_name_lookup_key(word, platform))
        .is_some_and(|name| matches!(name.as_str(), "cd" | "pushd" | "popd" | "chdir" | "builtin"))
}

/// The operands of an `rm` invocation. Returns `None` when an option is not understood or the
/// command asks to disable a safeguard, so callers keep treating the command as dangerous.
fn rm_targets(args: &[String]) -> Option<Vec<String>> {
    const SHORT_FLAGS: &str = "fFrRdvIiPxW";
    const LONG_FLAGS: &[&str] = &[
        "--force",
        "--recursive",
        "--dir",
        "--verbose",
        "--interactive",
        "--one-file-system",
        "--preserve-root",
    ];
    let mut targets = Vec::new();
    let mut after_double_dash = false;
    for arg in args {
        if after_double_dash {
            targets.push(arg.clone());
        } else if arg == "--" {
            after_double_dash = true;
        } else if let Some(long) = arg.strip_prefix("--") {
            let name = format!("--{}", long.split('=').next().unwrap_or(long));
            if arg.starts_with("--no-preserve-root") || !LONG_FLAGS.contains(&name.as_str()) {
                return None;
            }
        } else if arg.len() > 1 && arg.starts_with('-') {
            if !arg[1..].chars().all(|flag| SHORT_FLAGS.contains(flag)) {
                return None;
            }
        } else {
            targets.push(arg.clone());
        }
    }
    (!targets.is_empty()).then_some(targets)
}

fn rm_args_include_force_option(args: &[String]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| {
            arg == "--force"
                || arg
                    .strip_prefix('-')
                    .is_some_and(|flags| !flags.starts_with('-') && flags.contains('f'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn vec_str(items: &[&str]) -> Vec<String> {
        items.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn rm_rf_is_dangerous() {
        assert_eq!(
            dangerous_command_match(&vec_str(&["rm", "-rf", "/"])),
            Some(DangerousCommandMatch::ForcedRm)
        );
    }

    #[test]
    fn rm_f_is_dangerous() {
        assert_eq!(
            dangerous_command_match(&vec_str(&["rm", "-f", "/"])),
            Some(DangerousCommandMatch::ForcedRm)
        );
    }

    #[test]
    fn forced_rm_variants_are_dangerous() {
        for command in [
            vec_str(&["/bin/rm", "-fr", "/tmp/example"]),
            vec_str(&["rm", "-r", "-f", "/tmp/example"]),
            vec_str(&["rm", "--force", "/tmp/example"]),
            vec_str(&["rm", "/tmp/example", "-f"]),
            vec_str(&["sudo", "rm", "-rf", "/tmp/example"]),
            vec_str(&["env", "TARGET=/tmp/example", "rm", "-rf", "/tmp/example"]),
        ] {
            assert_eq!(
                dangerous_command_match(&command),
                Some(DangerousCommandMatch::ForcedRm),
                "{command:?}"
            );
        }
    }

    #[test]
    fn deeply_nested_command_wrappers_fail_closed() {
        for (depth, expected) in [
            (
                MAX_DANGEROUS_COMMAND_WRAPPER_DEPTH,
                DangerousCommandMatch::ForcedRm,
            ),
            (
                MAX_DANGEROUS_COMMAND_WRAPPER_DEPTH + 1,
                DangerousCommandMatch::Other,
            ),
        ] {
            let command = std::iter::repeat_n("env", depth)
                .chain(["rm", "-rf", "/tmp/example"])
                .map(str::to_owned)
                .collect::<Vec<_>>();

            assert_eq!(dangerous_command_match(&command), Some(expected));
        }
    }

    #[test]
    fn forced_rm_in_complex_shell_syntax_is_dangerous() {
        for script in [
            "printf x | rm -rf /tmp/example",
            "if test -d /tmp/example; then rm --force /tmp/example; fi",
            "rm -rf \"$TARGET\" >/dev/null",
            "for target in /tmp/a /tmp/b; do rm -r -f \"$target\"; done",
            "echo \"$(rm -rf /tmp/example)\"",
            "bash -c 'rm -rf /tmp/example'",
            "trap 'rm -rf /tmp/example' EXIT",
            "for a in '-C5a25KeRr' '--' '--json' '--bogus'; do HOME=$(mktemp -d) MDE_URL=http://127.0.0.1:1 MDE_TOKEN=x node cli/mde.cjs ls \"$a\" >/tmp/mde-review-out 2>/tmp/mde-review-err; code=$?; printf '%s\\t%s\\t%s\\n' \"$a\" \"$code\" \"$(tr '\\n' ' ' </tmp/mde-review-err)\"; rm -rf \"$HOME\"; done",
        ] {
            let command = vec_str(&["bash", "-lc", script]);
            assert_eq!(
                dangerous_command_match(&command),
                Some(DangerousCommandMatch::ForcedRm),
                "{script}"
            );
        }
    }

    #[test]
    fn non_forced_or_non_literal_rm_is_not_dangerous() {
        for command in [
            vec_str(&["rm", "-r", "/tmp/example"]),
            vec_str(&["rm", "--", "-f"]),
            vec_str(&["bash", "-lc", "echo 'rm -rf /tmp/example'"]),
            vec_str(&["bash", "-lc", "cmd=rm; $cmd -rf /tmp/example"]),
            vec_str(&["bash", "-lc", "if then rm -rf /tmp/example"]),
            vec_str(&["env", "TARGET=/tmp/example", "rm", "-r", "/tmp/example"]),
            vec_str(&["bash", "-lc", "trap 'echo rm -rf /tmp/example' EXIT"]),
        ] {
            assert_eq!(dangerous_command_match(&command), None, "{command:?}");
        }
    }

    #[test]
    fn direct_powershell_words_return_forced_delete_match_on_windows() {
        let command = vec_str(&["Remove-Item", "test", "-Force"]);

        if cfg!(windows) {
            assert_eq!(
                dangerous_powershell_words_match(&command, DangerousCommandPlatform::host()),
                Some(DangerousCommandMatch::ForcedDelete)
            );
        } else {
            assert_eq!(
                dangerous_powershell_words_match(&command, DangerousCommandPlatform::host()),
                None
            );
        }
    }
}

#[cfg(test)]
#[path = "is_dangerous_command_scoped_tests.rs"]
mod scoped_tests;
