//! InoCode 知識データベース。
//!
//! 1 レコード 1 ファイル(Markdown + YAML front-matter)が正本。
//! プロジェクト: `<cwd>/.inocode/knowledge/<kind>/<id>.md`
//! 共通: `<INOCODE_HOME>/knowledge/<kind>/<id>.md`
//! 検索はファイルから都度組み立てる(索引は常にファイルから再生成できる状態で、
//! 日本語の部分一致を扱うため、FTS5 ではなく部分一致 + 重み付けで実装している)。

mod events;
mod guard;
mod record;
mod store;

pub use guard::{GuardAction, action_from_tool_input, GuardHit, GuardVerdict, check_action, guard_message};
pub use events::{
    GuardEvent, GuardOutcome, GuardTarget, KnowledgeRecorded, guard_events_path, list_guard_events,
    record_guard_hits, subscribe_guard, subscribe_recorded,
};
pub use record::{Confidence, Kind, MatchSpec, Record, Scope, Severity, Status};
pub use store::{ListQuery, Store, SummaryRow};

use std::path::{Path, PathBuf};

/// `INOCODE_HOME`(未設定なら `~/.inocode`)。
pub fn inocode_home() -> PathBuf {
    codex_inocode_flags::inocode_home()
}

/// 現在の cwd と共通領域に対する Store。
pub fn store_for(cwd: &Path) -> Store {
    Store::new(Some(cwd.to_path_buf()), inocode_home())
}

/// 重大度 `block` の有効な rule を、1 行 1 件(理由付き)のリストにして返す。
/// 圧縮のあと・モデル切り替えのあとの再注入に使う。該当が無ければ空文字列。
/// `knowledge.enabled` / `knowledge.pinned_rules` が false なら空文字列。
pub fn pinned_rules_text(cwd: &Path) -> String {
    let f = codex_inocode_flags::load();
    if !(f.knowledge_enabled && f.knowledge_pinned_rules) {
        return String::new();
    }
    store_for(cwd).pinned_rules_text()
}

/// 再注入の対象の規則そのもの(フラグに関係なく返す。GUI の表示用)。
pub fn pinned_rules(cwd: &Path) -> Vec<Record> {
    store_for(cwd).pinned_rules()
}

/// 引き継ぎパック: 禁止事項、現在のタスクに関連する issue、直近の decision、
/// 関連する failure_case、未解決の点。
/// `knowledge.enabled` が false なら空文字列。
pub fn build_handoff_pack(cwd: &Path, task: &str) -> String {
    if !codex_inocode_flags::load().knowledge_enabled {
        return String::new();
    }
    store_for(cwd).build_handoff_pack(task)
}

/// 失敗例を記録する。同じ署名の未撤回の失敗例があればその ID を返す(重複作成しない)。
/// `knowledge.enabled` が false なら何もせず None。
pub fn record_failure_case(
    cwd: &Path,
    signature: &str,
    symptom: &str,
    root_cause: Option<&str>,
    do_not: Option<&str>,
    what_worked: Option<&str>,
    created_by: &str,
) -> std::io::Result<Option<String>> {
    if !codex_inocode_flags::load().knowledge_enabled {
        return Ok(None);
    }
    let store = store_for(cwd);
    let id = store.record_failure_case(
        signature,
        symptom,
        root_cause,
        do_not,
        what_worked,
        created_by,
    )?;
    events::notify_recorded(&store, &id);
    Ok(Some(id))
}

/// PreToolUse ガード用: cwd のプロジェクトと共通領域の rule に照合する。
/// `knowledge.enabled` / `knowledge.guard` が false なら何も合わない(空の判定)。
pub fn guard_check(cwd: &Path, action: &GuardAction) -> GuardVerdict {
    guard_check_with(&codex_inocode_flags::load(), &store_for(cwd), action)
}

/// フラグと Store を明示するガード(テスト用に分離)。
pub fn guard_check_with(
    flags: &codex_inocode_flags::InocodeFlags,
    store: &Store,
    action: &GuardAction,
) -> GuardVerdict {
    if !(flags.knowledge_enabled && flags.knowledge_guard) {
        return GuardVerdict::default();
    }
    check_action(store, action)
}

#[cfg(test)]
mod event_tests {
    use super::*;

    /// 全テストで同じ INOCODE_HOME を使う(環境変数はプロセス共通)。
    fn shared_home() {
        static H: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        let d = H.get_or_init(|| tempfile::tempdir().unwrap());
        // SAFETY: 値は常に同じ。
        unsafe { std::env::set_var("INOCODE_HOME", d.path()) };
    }

    #[test]
    fn guard_hits_are_logged_and_broadcast() {
        shared_home();
        let mut rx = subscribe_guard();
        let verdict = GuardVerdict {
            blocked: vec![GuardHit {
                id: "rule-0001".into(),
                title: "force push".into(),
                severity: Severity::Block,
                reason: "履歴が消える".into(),
                scope: Scope::Project,
            }],
            warnings: vec![],
        };
        let t = GuardTarget {
            thread_id: "th1".into(),
            tool: "shell".into(),
            target: "git push --force".into(),
            cwd: "/w".into(),
            ..Default::default()
        };
        let evs = record_guard_hits(&verdict, &t);
        assert_eq!(evs.len(), 1);
        assert_eq!(rx.try_recv().unwrap().rule_id, "rule-0001");
        let (list, next) = list_guard_events(Some("/w"), Some("th1"), 10, 0);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].result, GuardOutcome::Blocked);
        assert!(next.is_none());
        assert!(list_guard_events(Some("/other"), None, 10, 0).0.is_empty());
    }

    #[test]
    fn failure_case_is_recorded_once_notified_and_in_handoff() {
        shared_home();
        let cwd = tempfile::tempdir().unwrap();
        let mut rx = subscribe_recorded();
        let id = record_failure_case(cwd.path(), "same_failure:cargo test", "E0432", None, None, None, "advisor")
            .unwrap()
            .expect("recorded");
        let again = record_failure_case(cwd.path(), "same_failure:cargo test", "E0432", None, None, None, "advisor")
            .unwrap();
        assert_eq!(again.as_deref(), Some(id.as_str()));
        let note = rx.try_recv().unwrap();
        assert_eq!(note.id, id);
        assert_eq!(note.kind, Kind::FailureCase);
        let pack = build_handoff_pack(cwd.path(), "cargo test");
        assert!(pack.contains("引き継ぎパック"));
        assert!(pack.contains("same_failure:cargo test") || pack.contains(&id));
    }

    #[test]
    fn write_fills_internal_values_counts_and_delete() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let store = Store::new(Some(cwd.path().to_path_buf()), home.path().to_path_buf());
        let mut f = Record::new(Kind::FailureCase, "ロック失敗");
        f.symptom = Some("database is locked\n詳細".into());
        let fid = store.write(f).unwrap();
        let f = store.read(Scope::Project, &fid).unwrap();
        assert_eq!(f.signature.as_deref(), Some("database is locked"));
        assert_eq!(f.created_by.as_deref(), Some("user"));
        let mut r = Record::new(Kind::Rule, "rm 禁止");
        r.match_spec = Some(MatchSpec {
            commands: vec!["rm -rf".into()],
            ..Default::default()
        });
        let rid = store.write(r).unwrap();
        assert_eq!(
            store.read(Scope::Project, &rid).unwrap().enforced_by.as_deref(),
            Some("hook")
        );
        let rid2 = store.write(Record::new(Kind::Rule, "注意")).unwrap();
        assert_eq!(
            store.read(Scope::Project, &rid2).unwrap().enforced_by.as_deref(),
            Some("prompt")
        );
        let counts = store.counts(Some(Scope::Project));
        assert_eq!(counts["rule"], 2);
        assert_eq!(counts["failure_case"], 1);
        assert_eq!(counts["unverified"], 3);
        store.delete(Scope::Project, &rid2).unwrap();
        assert!(store.read(Scope::Project, &rid2).is_none());
        assert!(store.delete(Scope::Project, &rid2).is_err());
    }
}
