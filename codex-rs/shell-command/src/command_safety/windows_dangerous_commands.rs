use super::DangerousCommandMatch;
use super::DeleteScope;
use crate::command_safety::powershell_tree_sitter::try_parse_powershell_commands;
use once_cell::sync::Lazy;
use regex::Regex;
use shlex::split as shlex_split;
use url::Url;

#[cfg(test)]
pub fn is_dangerous_command_windows(command: &[String]) -> bool {
    dangerous_command_windows_match(command, /*scope*/ None).is_some()
}

pub(super) fn dangerous_command_windows_match(
    command: &[String],
    scope: Option<&DeleteScope>,
) -> Option<DangerousCommandMatch> {
    // Prefer structured parsing for PowerShell/CMD so we can spot URL-bearing
    // invocations of ShellExecute-style entry points before falling back to
    // simple argv heuristics.
    if let Some(dangerous_match) = dangerous_powershell_match(command, scope) {
        return Some(dangerous_match);
    }

    if let Some(dangerous_match) = dangerous_cmd_match(command) {
        return Some(dangerous_match);
    }

    if is_direct_gui_launch(command) {
        return Some(DangerousCommandMatch::UrlLaunch);
    }

    None
}

fn dangerous_powershell_match(
    command: &[String],
    scope: Option<&DeleteScope>,
) -> Option<DangerousCommandMatch> {
    let (exe, rest) = command.split_first()?;
    if !is_powershell_executable(exe) {
        return None;
    }
    let parsed = parse_powershell_invocation(rest)?;

    if let Some(script) = parsed.script
        && let Some(commands) = try_parse_powershell_commands(script)
        && powershell_commands_are_independent(script, &commands)
    {
        return commands
            .iter()
            .find_map(|words| powershell_literal_words_match(words, scope));
    }
    powershell_words_match(&parsed.tokens, scope)
}

/// Whether strictly lowered commands can be classified independently of the rest of the script.
pub fn powershell_commands_are_independent(script: &str, commands: &[Vec<String>]) -> bool {
    // Deliberately keep even quoted pipes and `||` chains on the existing path. Do not
    // model pipeline data, command rebinding, or strings that may execute using script state.
    !script.contains('|')
        && commands.iter().all(|words| {
            !matches!(
                words
                    .first()
                    .and_then(|word| executable_basename(word))
                    .as_deref(),
                Some(
                    "set-alias"
                        | "sal"
                        | "new-alias"
                        | "nal"
                        | "set-item"
                        | "si"
                        | "new-item"
                        | "ni"
                )
            ) && !words.iter().skip(1).any(|word| {
                let word = word.to_ascii_lowercase();
                [
                    "start-process",
                    "invoke-item",
                    "shellexecute",
                    "shell.application",
                ]
                .iter()
                .any(|marker| word.contains(marker))
            })
        })
}

enum PowerShellCommandKind {
    Opaque,
    IndependentLiteral,
}

pub(super) fn powershell_words_match(
    words: &[String],
    scope: Option<&DeleteScope>,
) -> Option<DangerousCommandMatch> {
    classify_powershell_words(words, PowerShellCommandKind::Opaque, scope)
}

pub(super) fn powershell_literal_words_match(
    words: &[String],
    scope: Option<&DeleteScope>,
) -> Option<DangerousCommandMatch> {
    // Shell and terminal wrappers can interpret later words as commands, including `start`.
    let kind = if words.first().is_some_and(|word| {
        is_powershell_executable(word)
            || matches!(
                executable_basename(word).as_deref(),
                Some("cmd" | "cmd.exe" | "wt" | "wt.exe")
            )
    }) {
        PowerShellCommandKind::Opaque
    } else {
        PowerShellCommandKind::IndependentLiteral
    };
    classify_powershell_words(words, kind, scope)
}

fn classify_powershell_words(
    words: &[String],
    kind: PowerShellCommandKind,
    scope: Option<&DeleteScope>,
) -> Option<DangerousCommandMatch> {
    let tokens_lc: Vec<String> = words
        .iter()
        .map(|t| t.trim_matches('\'').trim_matches('"').to_ascii_lowercase())
        .collect();
    let has_url = args_have_url(words);

    if has_url && start_process_opens_url(words, &kind) {
        return Some(DangerousCommandMatch::UrlLaunch);
    }

    if has_url
        && tokens_lc
            .iter()
            .any(|t| t.contains("shellexecute") || t.contains("shell.application"))
    {
        return Some(DangerousCommandMatch::UrlLaunch);
    }

    if let Some(first) = tokens_lc.first() {
        // Legacy ShellExecute path via url.dll
        if first == "rundll32"
            && tokens_lc
                .iter()
                .any(|t| t.contains("url.dll,fileprotocolhandler"))
            && has_url
        {
            return Some(DangerousCommandMatch::UrlLaunch);
        }
        if first == "mshta" && has_url {
            return Some(DangerousCommandMatch::UrlLaunch);
        }
        if is_browser_executable(first) && has_url {
            return Some(DangerousCommandMatch::UrlLaunch);
        }
        if matches!(first.as_str(), "explorer" | "explorer.exe") && has_url {
            return Some(DangerousCommandMatch::UrlLaunch);
        }
    }

    // Check for force delete operations (e.g., Remove-Item -Force)
    let trimmed: Vec<String> = words
        .iter()
        .map(|t| t.trim_matches('\'').trim_matches('"').to_string())
        .collect();
    has_force_delete_cmdlet(&trimmed, scope).then_some(DangerousCommandMatch::ForcedDelete)
}

fn dangerous_cmd_match(command: &[String]) -> Option<DangerousCommandMatch> {
    let (exe, rest) = command.split_first()?;
    let base = executable_basename(exe)?;
    if base != "cmd" && base != "cmd.exe" {
        return None;
    }

    let mut iter = rest.iter();
    for arg in iter.by_ref() {
        let lower = arg.to_ascii_lowercase();
        match lower.as_str() {
            "/c" | "/r" | "-c" => break,
            _ if lower.starts_with('/') => continue,
            // Unknown tokens before the command body => bail.
            _ => return None,
        }
    }

    let remaining: Vec<String> = iter.cloned().collect();
    if remaining.is_empty() {
        return None;
    }

    let cmd_tokens: Vec<String> = match remaining.as_slice() {
        [only] => shlex_split(only).unwrap_or_else(|| vec![only.clone()]),
        _ => remaining,
    };

    // Refine tokens by splitting concatenated CMD operators (e.g. "echo hi&del")
    let tokens: Vec<String> = cmd_tokens
        .into_iter()
        .flat_map(|t| split_embedded_cmd_operators(&t))
        .collect();

    const CMD_SEPARATORS: &[&str] = &["&", "&&", "|", "||"];
    tokens
        .split(|t| CMD_SEPARATORS.contains(&t.as_str()))
        .find_map(|segment| {
            let cmd = segment.first()?;

            // Classic `cmd /c ... start https://...` ShellExecute path.
            if cmd.eq_ignore_ascii_case("start") && args_have_url(segment) {
                return Some(DangerousCommandMatch::UrlLaunch);
            }
            // Force delete: del /f, erase /f
            if (cmd.eq_ignore_ascii_case("del") || cmd.eq_ignore_ascii_case("erase"))
                && has_force_flag_cmd(segment)
            {
                return Some(DangerousCommandMatch::Other);
            }
            // Recursive directory removal: rd /s /q, rmdir /s /q
            if (cmd.eq_ignore_ascii_case("rd") || cmd.eq_ignore_ascii_case("rmdir"))
                && has_recursive_flag_cmd(segment)
                && has_quiet_flag_cmd(segment)
            {
                return Some(DangerousCommandMatch::Other);
            }
            None
        })
}

/// Whether a `Start-Process` / `Invoke-Item` style launch in `words` opens a URL.
///
/// A URL that is only an argument for the launched program, as in
/// `Start-Process node -ArgumentList 'http://127.0.0.1:18800/v1'`, opens nothing by itself, so only
/// the launch target is inspected. A target that cannot be read literally keeps the command
/// flagged whenever a URL appears anywhere in it.
fn start_process_opens_url(words: &[String], kind: &PowerShellCommandKind) -> bool {
    let tokens_lc: Vec<String> = words
        .iter()
        .map(|t| t.trim_matches('\'').trim_matches('"').to_ascii_lowercase())
        .collect();
    tokens_lc.iter().enumerate().any(|(index, token)| {
        let exact = matches!(
            token.as_str(),
            "start-process" | "start" | "saps" | "invoke-item" | "ii"
        ) && (index == 0 || matches!(kind, PowerShellCommandKind::Opaque));
        let embedded = token.contains("start-process") || token.contains("invoke-item");
        if !exact && !embedded {
            return false;
        }
        if !exact {
            // A whole script can arrive as one token: look inside it word by word.
            if let Some(inner) = shlex_split(&words[index]).filter(|inner| inner.len() > 1) {
                return start_process_opens_url(&inner, &PowerShellCommandKind::Opaque);
            }
            // `Start-Process('https://...')` arrives as one token: the URL is the target.
            if looks_like_url(&words[index]) {
                return true;
            }
        }
        match launch_target(&words[index + 1..]) {
            Some(target) => launch_target_opens_url(&target, args_have_url(words)),
            None => false,
        }
    })
}

/// The program or document a `Start-Process` / `Invoke-Item` call launches: the `-FilePath` or
/// `-Path` value, or else the first positional word. Values of other parameters are skipped.
fn launch_target(rest: &[String]) -> Option<String> {
    let mut index = 0;
    while index < rest.len() {
        let token = rest[index].trim();
        let bare = token.trim_matches(['\'', '"']);
        index += 1;
        if matches!(bare, ";" | "|" | "||" | "&" | "&&") {
            return None;
        }
        if let Some(flag) = bare
            .strip_prefix('-')
            .filter(|flag| flag.starts_with(|c: char| c.is_ascii_alphabetic()))
        {
            let (name, inline_value) = match flag.split_once(':') {
                Some((name, value)) => (name, Some(value)),
                None => (flag, None),
            };
            let name = name.to_ascii_lowercase();
            if is_launch_target_flag(&name) {
                return match inline_value {
                    Some(value) => Some(value.to_string()),
                    None => rest.get(index).cloned(),
                };
            }
            if inline_value.is_none() && is_launch_value_flag(&name) {
                let skipped = rest.get(index)?;
                index += 1;
                if skipped.trim_end().ends_with(';') {
                    return None;
                }
            }
            continue;
        }
        // `cmd /c start "" <url>` passes an empty window title first.
        if bare.is_empty() {
            continue;
        }
        return Some(token.to_string());
    }
    None
}

fn is_launch_target_flag(name: &str) -> bool {
    matches!(name, "path" | "literalpath" | "lp") || (name.len() >= 2 && "filepath".starts_with(name))
}

fn is_launch_value_flag(name: &str) -> bool {
    const VALUE_FLAGS: &[&str] = &[
        "argumentlist",
        "workingdirectory",
        "redirectstandardoutput",
        "redirectstandarderror",
        "redirectstandardinput",
        "verb",
        "windowstyle",
        "credential",
    ];
    name == "args"
        || (name.len() >= 2 && VALUE_FLAGS.iter().any(|flag| flag.starts_with(name)))
}

fn launch_target_opens_url(target: &str, args_have_url: bool) -> bool {
    if looks_like_url(target) {
        return true;
    }
    if !args_have_url {
        return false;
    }
    // A variable, sub-expression or escape may hide a URL, and browsers and shells given a URL
    // argument open it.
    if target.contains(['$', '(', '`', '@']) {
        return true;
    }
    let name = executable_basename(target.trim_matches(['\'', '"'])).unwrap_or_default();
    is_browser_executable(&name)
        || matches!(
            name.as_str(),
            "explorer"
                | "explorer.exe"
                | "mshta"
                | "mshta.exe"
                | "rundll32"
                | "rundll32.exe"
                | "cmd"
                | "cmd.exe"
                | "wt"
                | "wt.exe"
                | "powershell"
                | "powershell.exe"
                | "pwsh"
                | "pwsh.exe"
        )
}

fn is_direct_gui_launch(command: &[String]) -> bool {
    let Some((exe, rest)) = command.split_first() else {
        return false;
    };
    let Some(base) = executable_basename(exe) else {
        return false;
    };

    // Explorer/rundll32/mshta or direct browser exe with a URL anywhere in args.
    if matches!(base.as_str(), "explorer" | "explorer.exe") && args_have_url(rest) {
        return true;
    }
    if matches!(base.as_str(), "mshta" | "mshta.exe") && args_have_url(rest) {
        return true;
    }
    if (base == "rundll32" || base == "rundll32.exe")
        && rest.iter().any(|t| {
            t.to_ascii_lowercase()
                .contains("url.dll,fileprotocolhandler")
        })
        && args_have_url(rest)
    {
        return true;
    }
    if is_browser_executable(&base) && args_have_url(rest) {
        return true;
    }

    false
}

fn split_embedded_cmd_operators(token: &str) -> Vec<String> {
    // Split concatenated CMD operators so `echo hi&del` becomes `["echo hi", "&", "del"]`.
    // Handles `&`, `&&`, `|`, `||`. Best-effort (CMD escaping is weird by nature).
    let mut parts = Vec::new();
    let mut start = 0;
    let mut it = token.char_indices().peekable();

    while let Some((i, ch)) = it.next() {
        if ch == '&' || ch == '|' {
            if i > start {
                parts.push(token[start..i].to_string());
            }

            // Detect doubled operator: && or ||
            let op_len = match it.peek() {
                Some(&(j, next)) if next == ch => {
                    it.next(); // consume second char
                    (j + next.len_utf8()) - i
                }
                _ => ch.len_utf8(),
            };

            parts.push(token[i..i + op_len].to_string());
            start = i + op_len;
        }
    }

    if start < token.len() {
        parts.push(token[start..].to_string());
    }

    parts.retain(|s| !s.trim().is_empty());
    parts
}

const DELETE_CMDLETS: &[&str] = &["remove-item", "ri", "rm", "del", "erase", "rd", "rmdir"];

fn is_delete_cmdlet(atom: &str) -> bool {
    DELETE_CMDLETS
        .iter()
        .any(|cmd| atom.eq_ignore_ascii_case(cmd))
}

/// Whether the tokens contain a delete cmdlet together with `-Force` in the same command segment.
/// The delete is excused only when `scope` proves every target is a contained literal path.
fn has_force_delete_cmdlet(tokens: &[String], scope: Option<&DeleteScope>) -> bool {
    // Hard separators that end a command segment (so -Force must be in same segment)
    const SEG_SEPS: &[char] = &[';', '|', '&', '\n', '\r', '\t'];

    // Soft separators: punctuation that can stick to tokens (blocks, parens, brackets, commas, etc.)
    const SOFT_SEPS: &[char] = &['{', '}', '(', ')', '[', ']', ',', ';'];

    // Build rough command segments first
    let mut segments: Vec<Vec<String>> = vec![Vec::new()];
    for tok in tokens {
        // If token itself contains segment separators, split it (best-effort)
        let mut cur = String::new();
        for ch in tok.chars() {
            if SEG_SEPS.contains(&ch) {
                let s = cur.trim();
                if let Some(msg) = segments.last_mut()
                    && !s.is_empty()
                {
                    msg.push(s.to_string());
                }
                cur.clear();
                if let Some(last) = segments.last()
                    && !last.is_empty()
                {
                    segments.push(Vec::new());
                }
            } else {
                cur.push(ch);
            }
        }
        let s = cur.trim();
        if let Some(segment) = segments.last_mut()
            && !s.is_empty()
        {
            segment.push(s.to_string());
        }
    }

    // Now, inside each segment, normalize tokens by splitting on soft punctuation
    segments.into_iter().any(|seg| {
        let atoms: Vec<String> = seg
            .iter()
            .flat_map(|t| t.split(|c| SOFT_SEPS.contains(&c)))
            .map(|atom| atom.trim().trim_matches(['\'', '"']).to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let has_delete = atoms.iter().any(|atom| is_delete_cmdlet(atom));
        let has_force = atoms.iter().any(|a| {
            a.eq_ignore_ascii_case("-force")
                || a.get(..7)
                    .is_some_and(|p| p.eq_ignore_ascii_case("-force:"))
        });

        has_delete && has_force && !forced_delete_is_contained(&seg, &atoms, scope)
    })
}

enum DeleteFlag {
    /// A switch such as `-Force` or `-Recurse`.
    Switch,
    /// `-Path`, `-LiteralPath` or `-PSPath`: the value is a delete target.
    Target,
    /// A parameter whose value is not a target (`-Include`, `-ErrorAction` ...).
    IgnoredValue,
    Unknown,
}

fn classify_delete_flag(name: &str) -> DeleteFlag {
    // Value-taking parameters are matched by exact name only, so an abbreviation can never make
    // the following token (a real target) look like a parameter value.
    const IGNORED_VALUE_FLAGS: &[&str] = &[
        "include",
        "exclude",
        "filter",
        "stream",
        "credential",
        "erroraction",
        "ea",
        "warningaction",
        "wa",
        "informationaction",
        "ia",
        "errorvariable",
        "ev",
        "warningvariable",
        "wv",
        "informationvariable",
        "iv",
        "outvariable",
        "ov",
        "outbuffer",
        "ob",
        "pipelinevariable",
        "pv",
    ];
    const SWITCHES: &[&str] = &[
        "force",
        "recurse",
        "whatif",
        "confirm",
        "verbose",
        "debug",
        "usetransaction",
    ];
    if name.is_empty() {
        return DeleteFlag::Unknown;
    }
    if IGNORED_VALUE_FLAGS.contains(&name) {
        return DeleteFlag::IgnoredValue;
    }
    if name == "lp"
        || ("literalpath".starts_with(name))
        || (name.len() >= 2 && ("path".starts_with(name) || "pspath".starts_with(name)))
    {
        return DeleteFlag::Target;
    }
    if SWITCHES.iter().any(|switch| switch.starts_with(name)) && name != "f" {
        return DeleteFlag::Switch;
    }
    DeleteFlag::Unknown
}

/// True when the segment is a single delete cmdlet whose every target is a literal path that
/// `scope` proves to be contained. Anything unexpected keeps the command dangerous.
fn forced_delete_is_contained(
    segment: &[String],
    atoms: &[String],
    scope: Option<&DeleteScope>,
) -> bool {
    let Some(scope) = scope else {
        return false;
    };
    // Sub-expressions, script blocks, variables, splatting and escapes are not plain literals. The
    // atoms below are split on this punctuation, so they would hide such constructs.
    if segment
        .iter()
        .any(|token| token.contains(['(', ')', '{', '}', '[', ']', '$', '@', '\x60']))
    {
        return false;
    }
    let Some((first, rest)) = atoms.split_first() else {
        return false;
    };
    if !is_delete_cmdlet(first) || rest.iter().any(|atom| is_delete_cmdlet(atom)) {
        return false;
    }

    let mut targets = Vec::new();
    let mut index = 0;
    while index < rest.len() {
        let atom = &rest[index];
        index += 1;
        let Some(flag) = atom.strip_prefix('-') else {
            targets.push(atom.clone());
            continue;
        };
        let (name, inline_value) = match flag.split_once(':') {
            Some((name, value)) => (name, Some(value)),
            None => (flag, None),
        };
        match classify_delete_flag(&name.to_ascii_lowercase()) {
            DeleteFlag::Switch => {}
            DeleteFlag::Target => {
                let value = match inline_value {
                    Some(value) => value.to_string(),
                    None => {
                        let Some(value) = rest.get(index) else {
                            return false;
                        };
                        index += 1;
                        value.clone()
                    }
                };
                targets.push(value);
            }
            DeleteFlag::IgnoredValue => {
                if inline_value.is_none() {
                    if index >= rest.len() {
                        return false;
                    }
                    index += 1;
                }
            }
            DeleteFlag::Unknown => return false,
        }
    }
    scope.contains_all(&targets)
}

/// Check for /f or /F flag in CMD del/erase arguments.
fn has_force_flag_cmd(args: &[String]) -> bool {
    args.iter().any(|a| a.eq_ignore_ascii_case("/f"))
}

/// Check for /s or /S flag in CMD rd/rmdir arguments.
fn has_recursive_flag_cmd(args: &[String]) -> bool {
    args.iter().any(|a| a.eq_ignore_ascii_case("/s"))
}

/// Check for /q or /Q flag in CMD rd/rmdir arguments.
fn has_quiet_flag_cmd(args: &[String]) -> bool {
    args.iter().any(|a| a.eq_ignore_ascii_case("/q"))
}

fn args_have_url(args: &[String]) -> bool {
    args.iter().any(|arg| looks_like_url(arg))
}

fn looks_like_url(token: &str) -> bool {
    // Strip common PowerShell punctuation around inline URLs (quotes, parens, trailing semicolons).
    // Capture the middle token after trimming leading quotes/parens/whitespace and trailing semicolons/closing parens.
    static RE: Lazy<Option<Regex>> =
        Lazy::new(|| Regex::new(r#"^[ "'\(\s]*([^\s"'\);]+)[\s;\)]*$"#).ok());
    // If the token embeds a URL alongside other text (e.g., Start-Process('https://...'))
    // as a single shlex token, grab the substring starting at the first URL prefix.
    let lowercase_token = token.to_ascii_lowercase();
    let urlish = lowercase_token
        .find("https://")
        .or_else(|| lowercase_token.find("http://"))
        .map(|idx| &token[idx..])
        .unwrap_or(token);

    let candidate = RE
        .as_ref()
        .and_then(|re| re.captures(urlish))
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str())
        .unwrap_or(urlish);
    let Ok(url) = Url::parse(candidate) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https")
}

fn executable_basename(exe: &str) -> Option<String> {
    let name = exe
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())?;
    let name = match name.as_bytes() {
        [drive, b':', ..] if drive.is_ascii_alphabetic() => &name[2..],
        _ => name,
    };
    (!name.is_empty()).then(|| name.to_ascii_lowercase())
}

fn is_powershell_executable(exe: &str) -> bool {
    matches!(
        executable_basename(exe).as_deref(),
        Some("powershell") | Some("powershell.exe") | Some("pwsh") | Some("pwsh.exe")
    )
}

fn is_browser_executable(name: &str) -> bool {
    matches!(
        name,
        "chrome"
            | "chrome.exe"
            | "msedge"
            | "msedge.exe"
            | "firefox"
            | "firefox.exe"
            | "iexplore"
            | "iexplore.exe"
    )
}

struct ParsedPowershell<'a> {
    tokens: Vec<String>,
    script: Option<&'a str>,
}

fn parse_powershell_invocation(args: &[String]) -> Option<ParsedPowershell<'_>> {
    if args.is_empty() {
        return None;
    }

    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        let lower = arg.to_ascii_lowercase();
        match lower.as_str() {
            "-command" | "/command" | "-c" => {
                let script = args.get(idx + 1)?;
                if idx + 2 != args.len() {
                    return None;
                }
                let tokens = shlex_split(script)?;
                return Some(ParsedPowershell {
                    tokens,
                    script: Some(script),
                });
            }
            _ if lower.starts_with("-command:") || lower.starts_with("/command:") => {
                if idx + 1 != args.len() {
                    return None;
                }
                let (_, script) = arg.split_once(':')?;
                let tokens = shlex_split(script)?;
                return Some(ParsedPowershell {
                    tokens,
                    script: Some(script),
                });
            }
            "-nologo" | "-noprofile" | "-noninteractive" | "-mta" | "-sta" => {
                idx += 1;
            }
            _ if lower.starts_with('-') => {
                idx += 1;
            }
            _ => {
                let rest = args[idx..].to_vec();
                return Some(ParsedPowershell {
                    tokens: rest,
                    script: None,
                });
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::is_dangerous_command_windows;

    fn vec_str(items: &[&str]) -> Vec<String> {
        items.iter().map(std::string::ToString::to_string).collect()
    }

    #[test]
    fn powershell_start_process_url_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "powershell",
            "-NoLogo",
            "-Command",
            "Start-Process 'https://example.com'"
        ])));
    }

    #[test]
    fn powershell_start_process_url_with_trailing_semicolon_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "powershell",
            "-Command",
            "Start-Process('https://example.com');"
        ])));
    }

    #[test]
    fn powershell_start_process_mixed_case_urls_are_dangerous() {
        for script in [
            "Start-Process('HTTP://example.com');",
            "Start-Process('hTtPs://example.com');",
        ] {
            assert!(
                is_dangerous_command_windows(&vec_str(&["powershell", "-Command", script])),
                "{script}"
            );
        }
    }

    #[test]
    fn powershell_start_process_local_is_not_flagged() {
        assert!(!is_dangerous_command_windows(&vec_str(&[
            "powershell",
            "-Command",
            "Start-Process notepad.exe"
        ])));
    }

    #[test]
    fn cmd_start_with_url_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd",
            "/c",
            "start",
            "https://example.com"
        ])));
    }

    #[test]
    fn msedge_with_url_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "msedge.exe",
            "https://example.com"
        ])));
    }

    #[test]
    fn explorer_with_directory_is_not_flagged() {
        assert!(!is_dangerous_command_windows(&vec_str(&[
            "explorer.exe",
            "."
        ])));
    }

    // Force delete tests for PowerShell

    #[test]
    fn powershell_remove_item_force_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "powershell",
            "-Command",
            "Remove-Item test -Force"
        ])));
    }

    #[test]
    fn powershell_remove_item_recurse_force_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "powershell",
            "-Command",
            "Remove-Item test -Recurse -Force"
        ])));
    }

    #[test]
    fn powershell_ri_alias_force_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "pwsh",
            "-Command",
            "ri test -Force"
        ])));
    }

    #[test]
    fn powershell_remove_item_without_force_is_not_flagged() {
        assert!(!is_dangerous_command_windows(&vec_str(&[
            "powershell",
            "-Command",
            "Remove-Item test"
        ])));
    }

    // Force delete tests for CMD
    #[test]
    fn cmd_del_force_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd", "/c", "del", "/f", "test.txt"
        ])));
    }

    #[test]
    fn cmd_erase_force_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd", "/c", "erase", "/f", "test.txt"
        ])));
    }

    #[test]
    fn cmd_del_without_force_is_not_flagged() {
        assert!(!is_dangerous_command_windows(&vec_str(&[
            "cmd", "/c", "del", "test.txt"
        ])));
    }

    #[test]
    fn cmd_rd_recursive_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd", "/c", "rd", "/s", "/q", "test"
        ])));
    }

    #[test]
    fn cmd_rd_without_quiet_is_not_flagged() {
        assert!(!is_dangerous_command_windows(&vec_str(&[
            "cmd", "/c", "rd", "/s", "test"
        ])));
    }

    #[test]
    fn cmd_rmdir_recursive_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd", "/c", "rmdir", "/s", "/q", "test"
        ])));
    }

    // Test exact scenario from issue #8567
    #[test]
    fn powershell_remove_item_path_recurse_force_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "powershell",
            "-Command",
            "Remove-Item -Path 'test' -Recurse -Force"
        ])));
    }

    #[test]
    fn powershell_remove_item_force_with_semicolon_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "powershell",
            "-Command",
            "Remove-Item test -Force; Write-Host done"
        ])));
    }

    #[test]
    fn powershell_remove_item_force_inside_block_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "powershell",
            "-Command",
            "if ($true) { Remove-Item test -Force}"
        ])));
    }

    #[test]
    fn powershell_remove_item_force_inside_brackets_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "powershell",
            "-Command",
            "[void]( Remove-Item test -Force)]"
        ])));
    }

    #[test]
    fn cmd_del_path_containing_f_is_not_flagged() {
        assert!(!is_dangerous_command_windows(&vec_str(&[
            "cmd",
            "/c",
            "del",
            "C:/foo/bar.txt"
        ])));
    }

    #[test]
    fn cmd_rd_path_containing_s_is_not_flagged() {
        assert!(!is_dangerous_command_windows(&vec_str(&[
            "cmd",
            "/c",
            "rd",
            "C:/source"
        ])));
    }

    #[test]
    fn cmd_bypass_chained_del_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd", "/c", "echo", "hello", "&", "del", "/f", "file.txt"
        ])));
    }

    #[test]
    fn powershell_chained_no_space_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "powershell",
            "-Command",
            "Write-Host hi;Remove-Item -Force C:\\tmp"
        ])));
    }

    #[test]
    fn powershell_comma_separated_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "powershell",
            "-Command",
            "del,-Force,C:\\foo"
        ])));
    }

    #[test]
    fn cmd_echo_del_is_not_dangerous() {
        assert!(!is_dangerous_command_windows(&vec_str(&[
            "cmd", "/c", "echo", "del", "/f"
        ])));
    }

    #[test]
    fn cmd_del_single_string_argument_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd",
            "/c",
            "del /f file.txt"
        ])));
    }

    #[test]
    fn cmd_del_chained_single_string_argument_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd",
            "/c",
            "echo hello & del /f file.txt"
        ])));
    }

    #[test]
    fn cmd_chained_no_space_del_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd",
            "/c",
            "echo hi&del /f file.txt"
        ])));
    }

    #[test]
    fn cmd_chained_andand_no_space_del_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd",
            "/c",
            "echo hi&&del /f file.txt"
        ])));
    }

    #[test]
    fn cmd_chained_oror_no_space_del_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd",
            "/c",
            "echo hi||del /f file.txt"
        ])));
    }

    #[test]
    fn cmd_start_url_single_string_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd",
            "/c",
            "start https://example.com"
        ])));
    }

    #[test]
    fn cmd_chained_no_space_rmdir_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd",
            "/c",
            "echo hi&rmdir /s /q testdir"
        ])));
    }

    #[test]
    fn cmd_del_force_uppercase_flag_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd", "/c", "DEL", "/F", "file.txt"
        ])));
    }

    #[test]
    fn cmdexe_r_del_force_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd.exe", "/r", "del", "/f", "file.txt"
        ])));
    }

    #[test]
    fn cmd_start_quoted_url_single_string_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd",
            "/c",
            r#"start "https://example.com""#
        ])));
    }

    #[test]
    fn cmd_start_title_then_url_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "cmd",
            "/c",
            r#"start "" https://example.com"#
        ])));
    }

    #[test]
    fn powershell_rm_alias_force_is_dangerous() {
        assert!(is_dangerous_command_windows(&vec_str(&[
            "powershell",
            "-Command",
            "rm test -Force"
        ])));
    }

    #[test]
    fn start_process_with_url_only_in_arguments_is_not_flagged() {
        for script in [
            "$s = Start-Process 'C:\\bin\\inocode.exe' -ArgumentList 'app-server -c base_url=http://127.0.0.1:18800/v1' -WindowStyle Hidden -PassThru",
            "Start-Process node -ArgumentList 'server.js','http://127.0.0.1:18800'",
            "Start-Process -FilePath node -ArgumentList 'http://127.0.0.1:18800'",
            "Start-Process node -Args 'http://127.0.0.1:18800' -PassThru",
            "Start-Process node -RedirectStandardError 'http://x.invalid/err' -PassThru",
        ] {
            assert!(
                !is_dangerous_command_windows(&vec_str(&["powershell", "-Command", script])),
                "{script}"
            );
        }
    }

    #[test]
    fn start_process_launching_a_url_target_is_flagged() {
        for script in [
            "Start-Process chrome 'https://example.com'",
            "Start-Process -FilePath https://example.com",
            "Start-Process -FilePath:https://example.com",
            "Start-Process -WindowStyle Hidden https://example.com",
            "saps https://example.com",
            "Invoke-Item https://example.com",
            "ii https://example.com",
            "$target = 'https://example.com/'; Start-Process $target",
            "Start-Process cmd -ArgumentList '/c start https://example.com'",
            "Start-Process powershell -ArgumentList '-c ii https://example.com'",
        ] {
            assert!(
                is_dangerous_command_windows(&vec_str(&["powershell", "-Command", script])),
                "{script}"
            );
        }
    }

    #[test]
    fn url_launches_are_reported_as_url_launch_and_deletes_are_not() {
        use super::DangerousCommandMatch;
        use super::dangerous_command_windows_match;

        for command in [
            vec_str(&["powershell", "-Command", "Start-Process 'https://example.com'"]),
            vec_str(&["cmd", "/c", "start", "https://example.com"]),
            vec_str(&["msedge.exe", "https://example.com"]),
            vec_str(&["explorer.exe", "https://example.com"]),
        ] {
            assert_eq!(
                dangerous_command_windows_match(&command, /*scope*/ None),
                Some(DangerousCommandMatch::UrlLaunch),
                "{command:?}"
            );
        }
        assert_eq!(
            dangerous_command_windows_match(
                &vec_str(&["cmd", "/c", "del", "/f", "x.txt"]),
                /*scope*/ None
            ),
            Some(DangerousCommandMatch::Other)
        );
        assert_eq!(
            dangerous_command_windows_match(
                &vec_str(&["powershell", "-Command", "Remove-Item x -Force"]),
                /*scope*/ None
            ),
            Some(DangerousCommandMatch::ForcedDelete)
        );
    }

    #[test]
    fn powershell_benign_force_separate_command_is_not_dangerous() {
        assert!(!is_dangerous_command_windows(&vec_str(&[
            "powershell",
            "-Command",
            "Get-ChildItem -Force; Remove-Item test"
        ])));
    }
}
