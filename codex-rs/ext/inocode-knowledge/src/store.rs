use crate::record::Kind;
use crate::record::Record;
use crate::record::Scope;
use crate::record::Severity;
use crate::record::Status;
use std::io;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Store {
    project_root: Option<PathBuf>,
    home: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// None なら project と global の両方。
    pub scope: Option<Scope>,
    pub kind: Option<Kind>,
    pub status: Option<Status>,
    pub query: Option<String>,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Clone)]
pub struct SummaryRow {
    pub record: Record,
    pub summary: String,
}

impl Store {
    pub fn new(project_root: Option<PathBuf>, home: PathBuf) -> Self {
        Self { project_root, home }
    }

    fn root(&self, scope: Scope) -> Option<PathBuf> {
        match scope {
            Scope::Project => self
                .project_root
                .as_ref()
                .map(|p| p.join(".inocode").join("knowledge")),
            Scope::Global => Some(self.home.join("knowledge")),
        }
    }

    fn file_path(&self, scope: Scope, kind: Kind, id: &str) -> Option<PathBuf> {
        Some(self.root(scope)?.join(kind.as_str()).join(format!("{id}.md")))
    }

    fn load_dir(dir: &Path, scope: Scope, out: &mut Vec<Record>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            if let Some(mut r) = Record::from_file_text(&text) {
                if r.id.is_empty() {
                    r.id = p
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string();
                }
                r.scope = scope;
                out.push(r);
            }
        }
    }

    /// 指定範囲の全レコード(ID 順)。
    pub fn all(&self, scope: Option<Scope>) -> Vec<Record> {
        let scopes = match scope {
            Some(s) => vec![s],
            None => vec![Scope::Project, Scope::Global],
        };
        let mut out = Vec::new();
        for s in scopes {
            let Some(root) = self.root(s) else { continue };
            for k in Kind::ALL {
                Self::load_dir(&root.join(k.as_str()), s, &mut out);
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    fn next_id(&self, scope: Scope, kind: Kind) -> String {
        let prefix = kind.as_str();
        let max = self
            .all(Some(scope))
            .iter()
            .filter(|r| r.kind == kind)
            .filter_map(|r| r.id.strip_prefix(&format!("{prefix}-"))?.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        format!("{prefix}-{:04}", max + 1)
    }

    pub fn read(&self, scope: Scope, id: &str) -> Option<Record> {
        self.all(Some(scope)).into_iter().find(|r| r.id == id)
    }

    fn save(&self, rec: &Record) -> io::Result<()> {
        let path = self
            .file_path(rec.scope, rec.kind, &rec.id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no project directory"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, rec.to_file_text())
    }

    fn now() -> String {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    /// 書き込み。
    /// - `id` が空: 新しい ID を採番して作成。
    /// - `id` が既存: 新しい ID で作成し、`supersedes` に古い ID を入れ、古い版は
    ///   `superseded` にして残す。
    /// - `id` が未存在: その ID で作成。
    /// 書いた rule はすぐ有効(承認の手順は無い)。
    pub fn write(&self, mut rec: Record) -> io::Result<String> {
        let now = Self::now();
        let existing = if rec.id.is_empty() {
            None
        } else {
            self.read(rec.scope, &rec.id)
        };
        if let Some(old) = existing {
            let mut old = old;
            old.status = Status::Superseded;
            old.updated_at = now.clone();
            self.save(&old)?;
            rec.supersedes = Some(old.id.clone());
            if rec.created_at.is_empty() {
                rec.created_at = old.created_at.clone();
            }
            rec.id = self.next_id(rec.scope, rec.kind);
        } else if rec.id.is_empty() {
            rec.id = self.next_id(rec.scope, rec.kind);
        }
        if rec.created_at.is_empty() {
            rec.created_at = now.clone();
        }
        rec.updated_at = now;
        // 内部の値は利用者に決めさせない: 署名は症状から、適用方法は対象の有無から決める。
        if rec.created_by.is_none() {
            rec.created_by = Some("user".to_string());
        }
        if rec.kind == Kind::FailureCase && rec.signature.as_deref().is_none_or(str::is_empty) {
            let src = rec
                .symptom
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(&rec.title);
            let line = src.lines().next().unwrap_or("").trim();
            rec.signature = Some(line.chars().take(80).collect());
        }
        if rec.kind == Kind::Rule && rec.enforced_by.as_deref().is_none_or(str::is_empty) {
            let has_match = rec
                .match_spec
                .as_ref()
                .is_some_and(|m| !m.paths.is_empty() || !m.commands.is_empty());
            rec.enforced_by = Some(if has_match { "hook" } else { "prompt" }.to_string());
        }
        // supersedes 指定で新規作成した場合も、古い方に印を付ける。
        if let Some(prev) = rec.supersedes.clone()
            && let Some(mut p) = self.read(rec.scope, &prev)
            && p.status != Status::Superseded
        {
            p.status = Status::Superseded;
            p.updated_at = rec.updated_at.clone();
            self.save(&p)?;
        }
        self.save(&rec)?;
        Ok(rec.id)
    }

    /// レコードのファイルを消す。
    pub fn delete(&self, scope: Scope, id: &str) -> io::Result<()> {
        let rec = self
            .read(scope, id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("not found: {id}")))?;
        let path = self
            .file_path(scope, rec.kind, &rec.id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no project directory"))?;
        std::fs::remove_file(path)
    }

    /// 種類ごと・状態ごとの件数(`kind` と `status` の名前をそのままキーにする)。
    pub fn counts(&self, scope: Option<Scope>) -> std::collections::BTreeMap<String, u32> {
        let mut m = std::collections::BTreeMap::new();
        for r in self.all(scope) {
            *m.entry(r.kind.as_str().to_string()).or_insert(0) += 1;
            let st = serde_json::to_value(r.status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            *m.entry(st).or_insert(0) += 1;
        }
        m
    }

    pub fn set_status(&self, scope: Scope, id: &str, status: Status) -> io::Result<()> {
        let mut rec = self
            .read(scope, id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("not found: {id}")))?;
        rec.status = status;
        rec.updated_at = Self::now();
        self.save(&rec)
    }

    /// 一覧と全文検索(部分一致。空白で区切った語は AND、タイトルは重み 3)。
    pub fn list(&self, q: &ListQuery) -> (Vec<SummaryRow>, bool) {
        let tokens: Vec<String> = q
            .query
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let mut scored: Vec<(u32, Record)> = Vec::new();
        for r in self.all(q.scope) {
            if q.kind.is_some_and(|k| k != r.kind) || q.status.is_some_and(|s| s != r.status) {
                continue;
            }
            let score = if tokens.is_empty() {
                1
            } else {
                let title = r.title.to_lowercase();
                let hay = haystack(&r);
                let mut total = 0;
                let mut all = true;
                for t in &tokens {
                    let mut s = 0;
                    if title.contains(t) {
                        s += 3;
                    }
                    if hay.contains(t) {
                        s += 1;
                    }
                    if s == 0 {
                        all = false;
                        break;
                    }
                    total += s;
                }
                if all { total } else { 0 }
            };
            if score > 0 {
                scored.push((score, r));
            }
        }
        if !tokens.is_empty() {
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.updated_at.cmp(&a.1.updated_at)));
        } else {
            scored.sort_by(|a, b| b.1.updated_at.cmp(&a.1.updated_at));
        }
        let limit = if q.limit == 0 { usize::MAX } else { q.limit };
        let total = scored.len();
        let rows: Vec<SummaryRow> = scored
            .into_iter()
            .skip(q.offset)
            .take(limit)
            .map(|(_, r)| SummaryRow {
                summary: r.summary(),
                record: r,
            })
            .collect();
        let has_more = q.offset.saturating_add(rows.len()) < total;
        (rows, has_more)
    }

    pub fn live_rules(&self) -> Vec<Record> {
        self.all(None)
            .into_iter()
            .filter(|r| r.kind == Kind::Rule && r.status.is_live())
            .collect()
    }

    /// 再注入の対象(有効で重大度が `block` の rule)。
    pub fn pinned_rules(&self) -> Vec<Record> {
        self.live_rules()
            .into_iter()
            .filter(|r| r.severity == Some(Severity::Block))
            .collect()
    }

    pub fn pinned_rules_text(&self) -> String {
        let mut lines = Vec::new();
        for r in self.live_rules() {
            if r.severity != Some(Severity::Block) {
                continue;
            }
            let reason = r.reason.as_deref().filter(|s| !s.is_empty()).unwrap_or("");
            if reason.is_empty() {
                lines.push(format!("- [{}] {}", r.id, r.title));
            } else {
                lines.push(format!("- [{}] {} (理由: {})", r.id, r.title, reason));
            }
        }
        if lines.is_empty() {
            return String::new();
        }
        format!("禁止事項(常に守る):\n{}", lines.join("\n"))
    }

    pub fn record_failure_case(
        &self,
        signature: &str,
        symptom: &str,
        root_cause: Option<&str>,
        do_not: Option<&str>,
        what_worked: Option<&str>,
        created_by: &str,
    ) -> io::Result<String> {
        if let Some(r) = self.all(Some(Scope::Project)).into_iter().find(|r| {
            r.kind == Kind::FailureCase
                && r.status.is_live()
                && r.signature.as_deref() == Some(signature)
        }) {
            return Ok(r.id);
        }
        let mut rec = Record::new(Kind::FailureCase, &format!("失敗: {signature}"));
        rec.signature = Some(signature.to_string());
        rec.symptom = Some(symptom.to_string());
        rec.root_cause = root_cause.map(str::to_string);
        rec.do_not = do_not.map(str::to_string);
        rec.what_worked = what_worked.map(str::to_string);
        rec.created_by = Some(created_by.to_string());
        self.write(rec)
    }

    pub fn build_handoff_pack(&self, task: &str) -> String {
        let mut out = String::from("# 引き継ぎパック\n");
        let pinned = self.pinned_rules_text();
        if !pinned.is_empty() {
            out.push_str(&format!("\n## 禁止事項\n{pinned}\n"));
        }
        let task_tokens: Vec<String> = task
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let related = |r: &Record| {
            task_tokens.is_empty() || {
                let h = haystack(r);
                task_tokens.iter().any(|t| h.contains(t))
            }
        };
        let all: Vec<Record> = self
            .all(None)
            .into_iter()
            .filter(|r| r.status.is_live())
            .collect();
        let section = |out: &mut String, title: &str, items: Vec<&Record>| {
            if items.is_empty() {
                return;
            }
            out.push_str(&format!("\n## {title}\n"));
            for r in items {
                out.push_str(&format!("- [{}] {} — {}\n", r.id, r.title, r.summary()));
            }
        };
        let mut issues: Vec<&Record> = all
            .iter()
            .filter(|r| r.kind == Kind::Issue && related(r))
            .collect();
        issues.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        section(&mut out, "関連する issue / 未解決の点", issues);
        let mut decisions: Vec<&Record> =
            all.iter().filter(|r| r.kind == Kind::Decision).collect();
        decisions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        decisions.truncate(5);
        section(&mut out, "直近の決定", decisions);
        let mut fails: Vec<&Record> = all
            .iter()
            .filter(|r| r.kind == Kind::FailureCase && related(r))
            .collect();
        fails.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        fails.truncate(5);
        if !fails.is_empty() {
            out.push_str("\n## 関連する失敗例\n");
            for r in fails {
                out.push_str(&format!("- [{}] {}", r.id, r.title));
                if let Some(d) = &r.do_not {
                    out.push_str(&format!(" / やってはいけない: {d}"));
                }
                if let Some(w) = &r.what_worked {
                    out.push_str(&format!(" / 効いた対処: {w}"));
                }
                out.push('\n');
            }
        }
        if !task.is_empty() {
            out.push_str(&format!("\n## 現在のタスク\n{task}\n"));
        }
        out
    }
}

fn haystack(r: &Record) -> String {
    let mut s = String::new();
    for p in [
        Some(r.id.as_str()),
        Some(r.title.as_str()),
        Some(r.body.as_str()),
        r.reason.as_deref(),
        r.symptom.as_deref(),
        r.root_cause.as_deref(),
        r.do_not.as_deref(),
        r.what_worked.as_deref(),
        r.signature.as_deref(),
        r.source.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        s.push_str(p);
        s.push('\n');
    }
    s.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::GuardAction;
    use crate::guard::check_action;
    use crate::guard::guard_message;
    use crate::record::MatchSpec;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::new(Some(d.path().join("proj")), d.path().join("home"));
        (d, s)
    }

    fn rule(title: &str, sev: Severity, commands: &[&str], paths: &[&str]) -> Record {
        let mut r = Record::new(Kind::Rule, title);
        r.severity = Some(sev);
        r.reason = Some("理由".into());
        r.match_spec = Some(MatchSpec {
            paths: paths.iter().map(|s| s.to_string()).collect(),
            commands: commands.iter().map(|s| s.to_string()).collect(),
        });
        r
    }

    #[test]
    fn write_read_roundtrip_and_supersede() {
        let (_d, s) = store();
        let id = s.write(Record::new(Kind::Fact, "事実A")).unwrap();
        assert_eq!(id, "fact-0001");
        let mut again = Record::new(Kind::Fact, "事実A改");
        again.id = id.clone();
        let id2 = s.write(again).unwrap();
        assert_eq!(id2, "fact-0002");
        assert_eq!(s.read(Scope::Project, &id).unwrap().status, Status::Superseded);
        assert_eq!(
            s.read(Scope::Project, &id2).unwrap().supersedes.as_deref(),
            Some("fact-0001")
        );
        s.set_status(Scope::Project, &id2, Status::Verified).unwrap();
        assert_eq!(s.read(Scope::Project, &id2).unwrap().status, Status::Verified);
    }

    #[test]
    fn search_and_filter() {
        let (_d, s) = store();
        let mut a = Record::new(Kind::Decision, "データベースはSQLite");
        a.body = "理由は軽量".into();
        s.write(a).unwrap();
        s.write(Record::new(Kind::Issue, "別の件")).unwrap();
        let (rows, _) = s.list(&ListQuery {
            query: Some("SQLite".into()),
            limit: 10,
            ..Default::default()
        });
        assert_eq!(rows.len(), 1);
        let (rows, _) = s.list(&ListQuery {
            kind: Some(Kind::Issue),
            limit: 10,
            ..Default::default()
        });
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn pinned_and_guard() {
        let (_d, s) = store();
        s.write(rule("force push 禁止", Severity::Block, &["git push --force"], &[]))
            .unwrap();
        s.write(rule("lock 注意", Severity::Warn, &[], &["**/Cargo.lock"]))
            .unwrap();
        let pinned = s.pinned_rules_text();
        assert!(pinned.contains("force push 禁止") && !pinned.contains("lock 注意"));
        let v = check_action(
            &s,
            &GuardAction {
                command: Some("cd x && git push --force origin main".into()),
                paths: vec![],
            },
        );
        assert_eq!(v.blocked.len(), 1);
        assert!(guard_message(&v).unwrap().contains("理由"));
        let v = check_action(
            &s,
            &GuardAction {
                command: None,
                paths: vec!["D:\\work\\codex-rs\\Cargo.lock".into()],
            },
        );
        assert_eq!((v.blocked.len(), v.warnings.len()), (0, 1));
        // 撤回した rule は効かない
        s.set_status(Scope::Project, "rule-0001", Status::Retracted)
            .unwrap();
        assert!(s.pinned_rules_text().is_empty());
    }

    #[test]
    fn regex_command_and_failure_case_and_handoff() {
        let (_d, s) = store();
        s.write(rule("rm 禁止", Severity::Block, &["re:^rm\\s+-rf\\s+/"], &[]))
            .unwrap();
        let v = check_action(
            &s,
            &GuardAction {
                command: Some("rm -rf /tmp".into()),
                paths: vec![],
            },
        );
        assert_eq!(v.blocked.len(), 1);
        let a = s
            .record_failure_case("E0308", "型不一致", None, Some("as で握りつぶさない"), None, "m")
            .unwrap();
        let b = s
            .record_failure_case("E0308", "型不一致", None, None, None, "m")
            .unwrap();
        assert_eq!(a, b);
        let pack = s.build_handoff_pack("E0308 を直す");
        assert!(pack.contains("禁止事項") && pack.contains("as で握りつぶさない"));
    }

    #[test]
    fn action_extraction() {
        let a = crate::guard::action_from_tool_input(
            "apply_patch",
            &serde_json::json!({"command": "*** Begin Patch
*** Update File: src/a.rs
*** End Patch"}),
        );
        assert_eq!(a.paths, vec!["src/a.rs"]);
        assert!(a.command.is_none());
        let b = crate::guard::action_from_tool_input(
            "Bash",
            &serde_json::json!({"command": "git push --force", "workdir": "x"}),
        );
        assert_eq!(b.command.as_deref(), Some("git push --force"));
    }

    #[test]
    fn guard_disabled_by_flag() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), "[inocode.knowledge]
guard = false
").unwrap();
        let s = Store::new(Some(d.path().join("proj")), home.clone());
        s.write(rule("x", Severity::Block, &["rm -rf"], &[])).unwrap();
        let flags = codex_inocode_flags::InocodeFlags::load_from(&home);
        assert!(!flags.knowledge_guard);
        let act = GuardAction { command: Some("rm -rf /".into()), paths: vec![] };
        let v = crate::guard_check_with(&flags, &s, &act);
        assert!(v.blocked.is_empty() && v.warnings.is_empty());
        let on = codex_inocode_flags::InocodeFlags::default();
        assert_eq!(crate::guard_check_with(&on, &s, &act).blocked.len(), 1);
    }
}
