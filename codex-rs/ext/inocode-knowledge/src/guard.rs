use crate::record::Severity;
use crate::store::Store;
use globset::Glob;
use regex::Regex;

/// ツール実行前に調べる操作。
#[derive(Debug, Clone, Default)]
pub struct GuardAction {
    /// 実行するコマンド文字列。
    pub command: Option<String>,
    /// 読み書きするファイルのパス。
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardHit {
    pub id: String,
    pub title: String,
    pub severity: Severity,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GuardVerdict {
    /// `block` に合ったもの(1 件でもあれば拒否)。
    pub blocked: Vec<GuardHit>,
    /// `warn` に合ったもの(モデルへ警告として返す)。
    pub warnings: Vec<GuardHit>,
}

/// 操作に合う有効な rule を探す。
pub fn check_action(store: &Store, action: &GuardAction) -> GuardVerdict {
    let mut verdict = GuardVerdict::default();
    for rule in store.live_rules() {
        let Some(spec) = &rule.match_spec else {
            continue;
        };
        let mut hit = false;
        if let Some(cmd) = &action.command {
            hit |= spec.commands.iter().any(|p| command_matches(p, cmd));
        }
        for path in &action.paths {
            hit |= spec.paths.iter().any(|g| path_matches(g, path));
        }
        if !hit {
            continue;
        }
        let h = GuardHit {
            id: rule.id.clone(),
            title: rule.title.clone(),
            severity: rule.severity.unwrap_or(Severity::Warn),
            reason: rule
                .reason
                .clone()
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| rule.title.clone()),
        };
        match h.severity {
            Severity::Block => verdict.blocked.push(h),
            Severity::Warn => verdict.warnings.push(h),
        }
    }
    verdict
}

/// モデルへ返す文面。何も合わなければ None。
pub fn guard_message(verdict: &GuardVerdict) -> Option<String> {
    if verdict.blocked.is_empty() && verdict.warnings.is_empty() {
        return None;
    }
    let mut out = String::new();
    if !verdict.blocked.is_empty() {
        out.push_str("この操作は禁止事項に合うため実行しません:\n");
        for h in &verdict.blocked {
            out.push_str(&format!("- [{}] {}: {}\n", h.id, h.title, h.reason));
        }
    }
    if !verdict.warnings.is_empty() {
        out.push_str("警告(禁止事項の注意):\n");
        for h in &verdict.warnings {
            out.push_str(&format!("- [{}] {}: {}\n", h.id, h.title, h.reason));
        }
    }
    Some(out)
}

/// `re:` で始まれば正規表現。それ以外は、コマンド全体または `&&` `;` `|` で区切った
/// 各部分の前方一致。
fn command_matches(pattern: &str, command: &str) -> bool {
    if let Some(re) = pattern.strip_prefix("re:") {
        return Regex::new(re).is_ok_and(|r| r.is_match(command));
    }
    let pat = pattern.trim();
    if pat.is_empty() {
        return false;
    }
    if command.trim_start().starts_with(pat) {
        return true;
    }
    command
        .split(['\n', ';', '|', '&'])
        .any(|seg| seg.trim_start().starts_with(pat))
}

fn path_matches(glob: &str, path: &str) -> bool {
    let norm = path.replace('\\', "/");
    let Ok(g) = Glob::new(&glob.replace('\\', "/")) else {
        return false;
    };
    let m = g.compile_matcher();
    if m.is_match(&norm) {
        return true;
    }
    // 絶対パスでも、相対の glob に末尾側で合えばよい。
    let parts: Vec<&str> = norm.split('/').collect();
    (1..parts.len()).any(|i| m.is_match(parts[i..].join("/")))
}

/// ツール入力(JSON)から、照合する操作を取り出す。
/// `command`(文字列または配列)はコマンド、`path` / `file_path` / `paths` / `workdir` 系はパス。
/// apply_patch の本文からは `*** Add/Update/Delete File:` と `*** Move to:` のパスを取る。
pub fn action_from_tool_input(tool_name: &str, input: &serde_json::Value) -> GuardAction {
    let mut action = GuardAction::default();
    let Some(obj) = input.as_object() else {
        return action;
    };
    let is_patch = tool_name == "apply_patch";
    if let Some(cmd) = obj.get("command").or_else(|| obj.get("cmd")) {
        let text = match cmd {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Array(a) => Some(
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        };
        if let Some(t) = text {
            if is_patch || t.contains("*** Begin Patch") {
                for line in t.lines() {
                    for p in ["*** Add File: ", "*** Update File: ", "*** Delete File: ", "*** Move to: "] {
                        if let Some(path) = line.strip_prefix(p) {
                            action.paths.push(path.trim().to_string());
                        }
                    }
                }
            } else {
                action.command = Some(t);
            }
        }
    }
    for key in ["path", "file_path", "filePath", "file", "target_file"] {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
            action.paths.push(s.to_string());
        }
    }
    if let Some(a) = obj.get("paths").and_then(|v| v.as_array()) {
        action
            .paths
            .extend(a.iter().filter_map(|v| v.as_str()).map(str::to_string));
    }
    action
}
