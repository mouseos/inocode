//! Rules to keep to and local issues, stored as notes. Rules are imported
//! verbatim from AGENTS.md-style files; a rule becomes enforceable only when
//! a person approves its trigger. The harness, not the model, records which
//! rule was presented, enforced or overridden. Design and sources:
//! docs/evidence/rules-and-local-issues.md.

use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};

use crate::store::{
    collect, fts_query, normalize_tags, now, store_error, Found, NoteHit, NoteId, NoteKind,
    ProjectId, SessionId, Source, Store,
};
use crate::ErrorInfo;

/// Title length taken from a rule's first line.
const TITLE_CHARS: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strength {
    /// Checked by the harness before a tool runs.
    Must,
    /// Offered where relevant; departing from it needs a recorded reason.
    Should,
}

impl Strength {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Must => "must",
            Self::Should => "should",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Block,
    /// Wait for a person's approval.
    Ask,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Ask => "ask",
        }
    }
}

/// When a must rule fires: a tool (`*` for any) whose arguments contain one
/// of the patterns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enforcement {
    pub tool: String,
    pub patterns: Vec<String>,
    pub action: Action,
}

/// What a tool call may do under the active must rules. Blocking wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask(Vec<NoteId>),
    Block(Vec<NoteId>),
}

/// The texts of a call's arguments that rule patterns are matched in: each
/// string value of the JSON arguments, decoded, or the whole text when it
/// is not JSON. Runs of spaces become one space and a backslash becomes `/`, in
/// the texts and in the patterns alike, so that neither the JSON escapes
/// nor spacing nor the way a path is written lets a call past a rule.
fn rule_texts(arguments: &str) -> Vec<String> {
    fn strings(value: &serde_json::Value, texts: &mut Vec<String>) {
        match value {
            serde_json::Value::String(text) => texts.push(normalized(text)),
            serde_json::Value::Array(items) => items.iter().for_each(|item| strings(item, texts)),
            serde_json::Value::Object(fields) => {
                fields.values().for_each(|item| strings(item, texts))
            }
            _ => {}
        }
    }
    match serde_json::from_str::<serde_json::Value>(arguments) {
        Ok(value) => {
            let mut texts = Vec::new();
            strings(&value, &mut texts);
            texts
        }
        Err(_) => vec![normalized(arguments)],
    }
}

fn normalized(text: &str) -> String {
    text.replace('\\', "/")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `pattern` is in one of `texts` (made by [`rule_texts`]).
fn fires(pattern: &str, texts: &[String]) -> bool {
    let pattern = normalized(pattern);
    !pattern.is_empty() && texts.iter().any(|text| text.contains(&pattern))
}

/// Where an event happened in a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct At {
    pub session: SessionId,
    pub seq: i64,
}

/// Who recorded an event. A model's own account of following a rule is kept
/// apart from what the harness observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    Harness,
    Person,
    Model,
}

impl Actor {
    fn as_str(self) -> &'static str {
        match self {
            Self::Harness => "harness",
            Self::Person => "person",
            Self::Model => "model",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub at: i64,
    pub event: String,
    pub actor: String,
    pub session: Option<SessionId>,
    pub seq: Option<i64>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportReport {
    pub added: usize,
    pub retired: usize,
    pub unchanged: usize,
    /// Why the document was stored whole instead of extracted, if it was.
    pub extraction_error: Option<String>,
    /// Sources the model gave that are not in the document.
    pub unverified_sources: usize,
    /// Document text no extracted rule rests on.
    pub uncovered: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueState {
    Open,
    InProgress,
    Blocked,
    Done,
    WontFix,
}

impl IssueState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::WontFix => "wontfix",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub id: NoteId,
    pub title: String,
    pub state: String,
    /// 0 is the most urgent.
    pub priority: i64,
    /// Unfinished issues that block this one.
    pub blocked_by: Vec<NoteId>,
}

pub(crate) fn title_of(text: &str) -> String {
    let first = text.lines().next().unwrap_or_default();
    let first =
        first.trim_start_matches(|c: char| c == '-' || c == '*' || c == '+' || c.is_whitespace());
    first.chars().take(TITLE_CHARS).collect()
}

impl Store {
    /// Adds a rule a person states directly.
    pub async fn add_rule(
        &self,
        project: Option<ProjectId>,
        text: &str,
    ) -> Result<NoteId, ErrorInfo> {
        let text = text.trim().to_string();
        self.write(move |db| {
            let tx = db.transaction().map_err(store_error)?;
            let id = insert_rule_row(
                &tx,
                project,
                "",
                &RuleRow {
                    section: "",
                    ordinal: 0,
                    start: 0,
                    end: 0,
                    body: &text,
                    title: &title_of(&text),
                    summary: "",
                    condition: "",
                    keywords: "",
                    segmented_by: "person",
                    details: "{}",
                },
                now(),
            )?;
            tx.execute(
                "UPDATE rules SET origin_path = NULL, origin_start = NULL, origin_end = NULL,
                                  ordinal = NULL WHERE note_id = ?1",
                [id.0],
            )
            .map_err(store_error)?;
            tx.commit().map_err(store_error)?;
            Ok(id)
        })
        .await
    }

    /// Proposes when a rule should be enforced. It takes effect only after
    /// [`Self::approve_enforcement`].
    pub async fn propose_enforcement(
        &self,
        rule: NoteId,
        enforcement: Enforcement,
    ) -> Result<(), ErrorInfo> {
        if enforcement
            .patterns
            .iter()
            .all(|pattern| pattern.is_empty())
        {
            return Err(ErrorInfo::new(
                "invalid_params",
                "an enforcement needs at least one non-empty pattern",
            ));
        }
        let patterns = serde_json::to_string(&enforcement.patterns)
            .map_err(|error| ErrorInfo::new("store", error.to_string()))?;
        self.write(move |db| {
            let changed = db
                .execute(
                    "UPDATE rules SET trigger_tool = ?1, patterns = ?2, action = ?3,
                                      enforcement = 'candidate' WHERE note_id = ?4",
                    params![
                        enforcement.tool,
                        patterns,
                        enforcement.action.as_str(),
                        rule.0
                    ],
                )
                .map_err(store_error)?;
            (changed == 1).then_some(()).ok_or_else(|| not_a_rule(rule))
        })
        .await
    }

    /// A person's approval: the rule becomes a must rule.
    pub async fn approve_enforcement(&self, rule: NoteId) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            let tx = db.transaction().map_err(store_error)?;
            let changed = tx
                .execute(
                    "UPDATE rules SET enforcement = 'active', strength = 'must'
                     WHERE note_id = ?1 AND enforcement IS NOT NULL",
                    [rule.0],
                )
                .map_err(store_error)?;
            if changed != 1 {
                return Err(ErrorInfo::new(
                    "invalid_params",
                    format!("rule {} has no proposed enforcement", rule.0),
                ));
            }
            record(&tx, rule, "enforcement_approved", Actor::Person, None, "")?;
            tx.commit().map_err(store_error)
        })
        .await
    }

    /// The active rules relevant to the work at hand, best first. Each one
    /// returned is logged as presented by the harness.
    pub async fn rules_for(
        &self,
        project: Option<ProjectId>,
        context: &str,
        limit: usize,
        budget: Duration,
        at: Option<At>,
    ) -> Result<Found<NoteHit>, ErrorInfo> {
        let Some(matched) = fts_query(context) else {
            return Ok(Found {
                items: Vec::new(),
                truncated: false,
            });
        };
        let found = self
            .read(budget, move |db| {
                let mut statement = db
                    .prepare_cached(
                        "SELECT n.id, n.title, n.summary,
                                snippet(notes_fts, -1, '[', ']', '…', 16)
                         FROM notes_fts JOIN notes n ON n.id = notes_fts.rowid
                         WHERE notes_fts MATCH ?1 AND n.kind = 'rule' AND n.status = 'active'
                           AND n.superseded_by IS NULL
                           AND (n.project_id IS ?2 OR n.project_id IS NULL)
                         ORDER BY bm25(notes_fts, 4.0, 2.0, 3.0, 3.0, 1.0, 3.0) LIMIT ?3",
                    )
                    .map_err(store_error)?;
                let rows = statement.query_map(
                    params![matched, project.map(|p| p.0), limit as i64],
                    |row| {
                        Ok(NoteHit {
                            id: NoteId(row.get(0)?),
                            kind: NoteKind::Rule,
                            title: row.get(1)?,
                            summary: row.get(2)?,
                            snippet: row.get(3)?,
                        })
                    },
                );
                collect(rows)
            })
            .await?;
        let ids: Vec<NoteId> = found.items.iter().map(|hit| hit.id).collect();
        if !ids.is_empty() {
            self.write(move |db| {
                let tx = db.transaction().map_err(store_error)?;
                for id in ids {
                    record(&tx, id, "presented", Actor::Harness, at, "")?;
                }
                tx.commit().map_err(store_error)
            })
            .await?;
        }
        Ok(found)
    }

    /// Checks a tool call against the active must rules of the project and
    /// the shared scope, logging each rule that fired.
    pub async fn check(
        &self,
        project: Option<ProjectId>,
        tool: &str,
        arguments: &str,
        at: Option<At>,
    ) -> Result<Decision, ErrorInfo> {
        let tool = tool.to_string();
        let texts = rule_texts(arguments);
        self.write(move |db| {
            let fired: Vec<(NoteId, String)> = {
                let mut statement = db
                    .prepare_cached(
                        "SELECT r.note_id, r.patterns, r.action FROM rules r
                         JOIN notes n ON n.id = r.note_id
                         WHERE r.enforcement = 'active' AND r.trigger_tool IN ('*', ?1)
                           AND n.status = 'active' AND n.superseded_by IS NULL
                           AND (n.project_id IS ?2 OR n.project_id IS NULL)",
                    )
                    .map_err(store_error)?;
                let rows = statement
                    .query_map(params![tool, project.map(|p| p.0)], |row| {
                        Ok((
                            NoteId(row.get(0)?),
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })
                    .map_err(store_error)?;
                let mut fired = Vec::new();
                for row in rows {
                    let (id, patterns, action) = row.map_err(store_error)?;
                    let patterns: Vec<String> = serde_json::from_str(&patterns)
                        .map_err(|error| ErrorInfo::new("store", error.to_string()))?;
                    if patterns.iter().any(|pattern| fires(pattern, &texts)) {
                        fired.push((id, action));
                    }
                }
                fired
            };
            if fired.is_empty() {
                return Ok(Decision::Allow);
            }
            let tx = db.transaction().map_err(store_error)?;
            let mut blocked = Vec::new();
            let mut asked = Vec::new();
            for (id, action) in fired {
                let event = if action == "block" {
                    blocked.push(id);
                    "blocked"
                } else {
                    asked.push(id);
                    "asked"
                };
                record(&tx, id, event, Actor::Harness, at, &tool)?;
            }
            tx.commit().map_err(store_error)?;
            Ok(if blocked.is_empty() {
                Decision::Ask(asked)
            } else {
                Decision::Block(blocked)
            })
        })
        .await
    }

    /// Logs an event on a rule or issue, such as a person allowing an asked
    /// call, the model departing from a should rule (with its reason), or
    /// the model saying which rule it followed.
    pub async fn record(
        &self,
        note: NoteId,
        event: &str,
        actor: Actor,
        at: Option<At>,
        detail: &str,
    ) -> Result<(), ErrorInfo> {
        let (event, detail) = (event.to_string(), detail.to_string());
        self.write(move |db| record(db, note, &event, actor, at, &detail))
            .await
    }

    /// A note's events, oldest first.
    pub async fn events(&self, note: NoteId) -> Result<Vec<Event>, ErrorInfo> {
        self.read(Duration::from_secs(30), move |db| {
            let mut statement = db
                .prepare_cached(
                    "SELECT at, event, actor, session_id, seq, detail FROM note_events
                     WHERE note_id = ?1 ORDER BY at, id",
                )
                .map_err(store_error)?;
            let rows = statement
                .query_map([note.0], |row| {
                    Ok(Event {
                        at: row.get(0)?,
                        event: row.get(1)?,
                        actor: row.get(2)?,
                        session: row.get::<_, Option<i64>>(3)?.map(SessionId),
                        seq: row.get(4)?,
                        detail: row.get(5)?,
                    })
                })
                .map_err(store_error)?;
            rows.collect::<Result<_, _>>().map_err(store_error)
        })
        .await
    }

    pub async fn open_issue(
        &self,
        project: Option<ProjectId>,
        title: &str,
        body: &str,
        priority: i64,
        tags: Vec<String>,
    ) -> Result<NoteId, ErrorInfo> {
        self.open_issue_as(project, title, body, priority, tags, Actor::Person, None)
            .await
    }

    /// Opens an issue, recording who opened it and where.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_issue_as(
        &self,
        project: Option<ProjectId>,
        title: &str,
        body: &str,
        priority: i64,
        tags: Vec<String>,
        actor: Actor,
        at: Option<At>,
    ) -> Result<NoteId, ErrorInfo> {
        let (title, body) = (title.to_string(), body.to_string());
        self.write(move |db| {
            let tags = normalize_tags(db, &tags)?;
            let tx = db.transaction().map_err(store_error)?;
            let time = now();
            tx.execute(
                "INSERT INTO notes(project_id, kind, title, summary, keywords, condition, body,
                                   tags, status, source, created, updated)
                 VALUES (?1, 'issue', ?2, '', '', '', ?3, ?4, 'open', ?5, ?6, ?6)",
                params![
                    project.map(|p| p.0),
                    title,
                    body,
                    tags.join(" "),
                    actor.as_str(),
                    time
                ],
            )
            .map_err(store_error)?;
            let id = NoteId(tx.last_insert_rowid());
            for tag in &tags {
                tx.execute(
                    "INSERT OR IGNORE INTO tags(tag, note_id) VALUES (?1, ?2)",
                    params![tag, id.0],
                )
                .map_err(store_error)?;
            }
            tx.execute(
                "INSERT INTO issues(note_id, priority) VALUES (?1, ?2)",
                params![id.0, priority],
            )
            .map_err(store_error)?;
            record(&tx, id, "opened", actor, at, "")?;
            tx.commit().map_err(store_error)?;
            Ok(id)
        })
        .await
    }

    /// Moves an issue to `state`, logging who did it and why.
    pub async fn set_issue_state(
        &self,
        issue: NoteId,
        state: IssueState,
        actor: Actor,
        at: Option<At>,
        reason: &str,
    ) -> Result<(), ErrorInfo> {
        let reason = reason.to_string();
        self.write(move |db| {
            let tx = db.transaction().map_err(store_error)?;
            let changed = tx
                .execute(
                    "UPDATE notes SET status = ?1, updated = ?2 WHERE id = ?3 AND kind = 'issue'",
                    params![state.as_str(), now(), issue.0],
                )
                .map_err(store_error)?;
            if changed != 1 {
                return Err(ErrorInfo::new("not_found", format!("no issue {}", issue.0)));
            }
            record(&tx, issue, state.as_str(), actor, at, &reason)?;
            tx.commit().map_err(store_error)
        })
        .await
    }

    /// One issue of the project, with its text; `None` when the project has
    /// no issue of that id.
    pub async fn issue(
        &self,
        project: Option<ProjectId>,
        issue: NoteId,
    ) -> Result<Option<(Issue, String)>, ErrorInfo> {
        self.read(Duration::from_secs(30), move |db| {
            db.query_row(
                "SELECT n.title, n.status, i.priority, n.body FROM notes n
                 JOIN issues i ON i.note_id = n.id
                 WHERE n.id = ?1 AND n.project_id IS ?2",
                params![issue.0, project.map(|p| p.0)],
                |row| {
                    Ok((
                        Issue {
                            id: issue,
                            title: row.get(0)?,
                            state: row.get(1)?,
                            priority: row.get(2)?,
                            blocked_by: Vec::new(),
                        },
                        row.get(3)?,
                    ))
                },
            )
            .optional()
            .map_err(store_error)
        })
        .await
    }

    /// The issue a thread last started that is still in progress.
    pub async fn current_issue(&self, session: SessionId) -> Result<Option<NoteId>, ErrorInfo> {
        self.read(Duration::from_secs(30), move |db| {
            db.query_row(
                "SELECT e.note_id FROM note_events e JOIN notes n ON n.id = e.note_id
                 WHERE e.session_id = ?1 AND e.event = 'in_progress'
                   AND n.kind = 'issue' AND n.status = 'in_progress'
                 ORDER BY e.at DESC, e.id DESC LIMIT 1",
                [session.0],
                |row| row.get(0).map(NoteId),
            )
            .optional()
            .map_err(store_error)
        })
        .await
    }

    /// Records that `blocker` must be finished before `blocked`.
    pub async fn block_issue(&self, blocker: NoteId, blocked: NoteId) -> Result<(), ErrorInfo> {
        self.link(blocker, blocked, "blocks").await
    }

    /// Issues of the project, most urgent first, then most recently changed.
    pub async fn issues(
        &self,
        project: Option<ProjectId>,
        include_finished: bool,
        limit: usize,
    ) -> Result<Vec<Issue>, ErrorInfo> {
        self.read(Duration::from_secs(30), move |db| {
            let mut statement = db
                .prepare_cached(
                    "SELECT n.id, n.title, n.status, i.priority FROM notes n
                     JOIN issues i ON i.note_id = n.id
                     WHERE n.project_id IS ?1
                       AND (?2 OR n.status NOT IN ('done', 'wontfix'))
                     ORDER BY i.priority, n.updated DESC, n.id LIMIT ?3",
                )
                .map_err(store_error)?;
            let rows = statement
                .query_map(
                    params![project.map(|p| p.0), include_finished, limit as i64],
                    |row| {
                        Ok(Issue {
                            id: NoteId(row.get(0)?),
                            title: row.get(1)?,
                            state: row.get(2)?,
                            priority: row.get(3)?,
                            blocked_by: Vec::new(),
                        })
                    },
                )
                .map_err(store_error)?;
            let mut issues: Vec<Issue> = rows.collect::<Result<_, _>>().map_err(store_error)?;
            let mut blockers = db
                .prepare_cached(
                    "SELECT l.src FROM links l JOIN notes n ON n.id = l.src
                     WHERE l.dst = ?1 AND l.kind = 'blocks'
                       AND n.status NOT IN ('done', 'wontfix')",
                )
                .map_err(store_error)?;
            for issue in &mut issues {
                let rows = blockers
                    .query_map([issue.id.0], |row| row.get(0).map(NoteId))
                    .map_err(store_error)?;
                issue.blocked_by = rows.collect::<Result<_, _>>().map_err(store_error)?;
            }
            Ok(issues)
        })
        .await
    }

    /// A rule's strength and enforcement state.
    pub async fn rule_strength(&self, rule: NoteId) -> Result<Strength, ErrorInfo> {
        self.read(Duration::from_secs(30), move |db| {
            let strength: Option<String> = db
                .query_row(
                    "SELECT strength FROM rules WHERE note_id = ?1",
                    [rule.0],
                    |row| row.get(0),
                )
                .optional()
                .map_err(store_error)?;
            match strength.as_deref() {
                Some("must") => Ok(Strength::Must),
                Some(_) => Ok(Strength::Should),
                None => Err(not_a_rule(rule)),
            }
        })
        .await
    }
}

/// A rule ready to store. `body` is verbatim source text; `title`,
/// `summary` (model-written rule sentences), `condition` and `keywords`
/// only widen search.
pub(crate) struct RuleRow<'a> {
    pub(crate) section: &'a str,
    pub(crate) ordinal: i64,
    pub(crate) start: i64,
    pub(crate) end: i64,
    pub(crate) body: &'a str,
    pub(crate) title: &'a str,
    pub(crate) summary: &'a str,
    pub(crate) condition: &'a str,
    pub(crate) keywords: &'a str,
    /// `whole` (the entire document), `model` (extracted, sources located),
    /// `model_unverified` (extracted, no source located) or `person`.
    pub(crate) segmented_by: &'a str,
    /// The model's fields as JSON (statement, condition, exceptions, reason,
    /// suggested strength, section, sources it could not locate).
    pub(crate) details: &'a str,
}

pub(crate) fn insert_rule_row(
    db: &Connection,
    project: Option<ProjectId>,
    path: &str,
    rule: &RuleRow,
    time: i64,
) -> Result<NoteId, ErrorInfo> {
    db.execute(
        "INSERT INTO notes(project_id, kind, title, summary, keywords, condition, body, tags,
                           status, source, created, updated)
         VALUES (?1, 'rule', ?2, ?3, ?4, ?5, ?6, '', 'active', ?7, ?8, ?8)",
        params![
            project.map(|p| p.0),
            rule.title,
            rule.summary,
            rule.keywords,
            rule.condition,
            rule.body,
            Source::Person.as_str(),
            time,
        ],
    )
    .map_err(store_error)?;
    let id = NoteId(db.last_insert_rowid());
    db.execute(
        "INSERT INTO rules(note_id, strength, origin_path, origin_start, origin_end, section,
                           ordinal, segmented_by, details)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id.0,
            Strength::Should.as_str(),
            path,
            rule.start,
            rule.end,
            rule.section,
            rule.ordinal,
            rule.segmented_by,
            rule.details,
        ],
    )
    .map_err(store_error)?;
    Ok(id)
}

fn record(
    db: &Connection,
    note: NoteId,
    event: &str,
    actor: Actor,
    at: Option<At>,
    detail: &str,
) -> Result<(), ErrorInfo> {
    db.execute(
        "INSERT INTO note_events(note_id, at, event, actor, session_id, seq, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            note.0,
            now(),
            event,
            actor.as_str(),
            at.map(|at| at.session.0),
            at.map(|at| at.seq),
            detail,
        ],
    )
    .map(|_| ())
    .map_err(store_error)
}

fn not_a_rule(id: NoteId) -> ErrorInfo {
    ErrorInfo::new("not_found", format!("no rule {}", id.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    const BUDGET: Duration = Duration::from_secs(5);

    const AGENTS: &str = "# Rules\n\n## Scope\n- Do only what was asked.\n  Keep changes small.\n- Never push without approval.\n  - Ask first, every time.\n\nParagraph rule about evidence.\n\n```\ngit config user.email\n```\n\n## Comments\n1. No change history in comments.\n";

    #[test]
    fn relevant_rules_are_logged_as_presented() {
        let dir = tempfile::tempdir().expect("dir");
        run(async {
            let store = Store::open(dir.path().join("r.db")).await.expect("open");
            let p = store.project("p").await.expect("p");
            store
                .import_rules(None, Some(p), "AGENTS.md", AGENTS)
                .await
                .expect("import");
            let shared = store
                .add_rule(None, "Write commit messages in English.")
                .await
                .expect("rule");
            let session = store.start_session(Some(p), "s").await.expect("session");
            let at = At { session, seq: 7 };
            let found = store
                .rules_for(Some(p), "commit messages", 5, BUDGET, Some(at))
                .await
                .expect("rules");
            assert_eq!(
                found.items[0].id, shared,
                "shared rules apply to every project"
            );
            let events = store.events(shared).await.expect("events");
            assert_eq!(events.len(), 1);
            assert_eq!(
                (events[0].event.as_str(), events[0].actor.as_str()),
                ("presented", "harness")
            );
            assert_eq!((events[0].session, events[0].seq), (Some(session), Some(7)));

            store
                .record(
                    shared,
                    "deviated",
                    Actor::Model,
                    Some(at),
                    "the user asked for Japanese",
                )
                .await
                .expect("record");
            let events = store.events(shared).await.expect("events");
            assert_eq!(events[1].actor, "model");
            assert_eq!(events[1].detail, "the user asked for Japanese");
        });
    }

    #[test]
    fn must_rules_fire_only_after_approval() {
        let dir = tempfile::tempdir().expect("dir");
        run(async {
            let store = Store::open(dir.path().join("r.db")).await.expect("open");
            let p = store.project("p").await.expect("p");
            let push = store
                .add_rule(Some(p), "Never push without approval.")
                .await
                .expect("rule");
            let secret = store
                .add_rule(None, "Never write the private address.")
                .await
                .expect("rule");
            store
                .propose_enforcement(
                    push,
                    Enforcement {
                        tool: "shell".to_string(),
                        patterns: vec!["git push".to_string()],
                        action: Action::Ask,
                    },
                )
                .await
                .expect("propose");
            store
                .propose_enforcement(
                    secret,
                    Enforcement {
                        tool: "*".to_string(),
                        patterns: vec!["private@example.com".to_string()],
                        action: Action::Block,
                    },
                )
                .await
                .expect("propose");
            let check = |tool: &'static str, arguments: &'static str| {
                let store = store.clone();
                async move {
                    store
                        .check(Some(p), tool, arguments, None)
                        .await
                        .expect("check")
                }
            };
            assert_eq!(
                check("shell", "git push origin main").await,
                Decision::Allow,
                "proposed only"
            );
            assert_eq!(
                store.rule_strength(push).await.expect("strength"),
                Strength::Should
            );

            store.approve_enforcement(push).await.expect("approve");
            store.approve_enforcement(secret).await.expect("approve");
            assert_eq!(
                store.rule_strength(push).await.expect("strength"),
                Strength::Must
            );
            assert_eq!(
                check("shell", "git push origin main").await,
                Decision::Ask(vec![push])
            );
            assert_eq!(
                check("write_file", "git push").await,
                Decision::Allow,
                "other tools"
            );
            assert_eq!(
                check("shell", "git push && echo private@example.com").await,
                Decision::Block(vec![secret]),
                "blocking wins"
            );
            let other = store.project("q").await.expect("q");
            assert_eq!(
                store
                    .check(Some(other), "shell", "git push", None)
                    .await
                    .expect("check"),
                Decision::Allow,
                "project rules stay in their project"
            );
            let events: Vec<String> = store
                .events(push)
                .await
                .expect("events")
                .into_iter()
                .map(|event| event.event)
                .collect();
            // Each rule logs its own action, also when another rule blocked the call.
            assert_eq!(events, ["enforcement_approved", "asked", "asked"]);
            assert!(store
                .approve_enforcement(store.add_rule(None, "x y z").await.expect("rule"))
                .await
                .is_err());
        });
    }

    #[test]
    fn patterns_are_matched_in_the_decoded_argument_values() {
        let shell =
            |command: &str| rule_texts(&serde_json::json!({ "command": command }).to_string());
        assert!(fires("rm -rf", &shell("rm   -rf /")));
        assert!(fires(
            "rm -rf",
            &rule_texts(r#"{"command": "rm\u0020-rf /"}"#)
        ));
        assert!(fires(
            "C:\\Users\\me\\.ssh",
            &rule_texts(r#"{"path": "C:\\Users\\me\\.ssh\\id_rsa"}"#)
        ));
        assert!(fires(
            ".ssh/id_rsa",
            &rule_texts(r#"{"path": "C:\\Users\\me\\.ssh\\id_rsa"}"#)
        ));
        assert!(fires(
            ".env",
            &rule_texts(r#"{"edits": [{"path": "a/.env", "old": "", "new": ""}]}"#)
        ));
        assert!(!fires("command", &shell("ls")), "keys are not matched");
        assert!(
            fires("git push", &rule_texts("git push origin main")),
            "plain text"
        );
        assert!(!fires("  ", &shell("anything")));
    }

    #[test]
    fn issues_order_by_priority_and_track_blockers() {
        let dir = tempfile::tempdir().expect("dir");
        run(async {
            let store = Store::open(dir.path().join("r.db")).await.expect("open");
            let p = store.project("p").await.expect("p");
            let schema = store
                .open_issue(Some(p), "design the schema", "", 1, vec![])
                .await
                .expect("issue");
            let api = store
                .open_issue(Some(p), "expose the API", "", 2, vec![])
                .await
                .expect("issue");
            let crash = store
                .open_issue(Some(p), "fix the crash", "", 0, vec!["bug".into()])
                .await
                .expect("issue");
            store.block_issue(schema, api).await.expect("block");

            let open = store.issues(Some(p), false, 10).await.expect("issues");
            let order: Vec<NoteId> = open.iter().map(|issue| issue.id).collect();
            assert_eq!(order, [crash, schema, api]);
            assert_eq!(open[2].blocked_by, [schema]);

            store
                .set_issue_state(
                    schema,
                    IssueState::Done,
                    Actor::Model,
                    None,
                    "migrations land",
                )
                .await
                .expect("done");
            let open = store.issues(Some(p), false, 10).await.expect("issues");
            assert_eq!(
                open.iter().map(|issue| issue.id).collect::<Vec<_>>(),
                [crash, api]
            );
            assert!(
                open[1].blocked_by.is_empty(),
                "a finished blocker no longer blocks"
            );
            assert_eq!(
                store.issues(Some(p), true, 10).await.expect("issues").len(),
                3
            );
            let events = store.events(schema).await.expect("events");
            assert_eq!(
                events.last().map(|event| event.detail.as_str()),
                Some("migrations land")
            );
            assert!(store
                .set_issue_state(NoteId(9999), IssueState::Done, Actor::Person, None, "")
                .await
                .is_err());
        });
    }
}
