//! Durable conversations and project memory in one SQLite database: message
//! history, large payloads stored once by hash, and notes (lessons,
//! procedures, claims, knowledge) joined by tags and links. Every query uses
//! an index and runs off the async runtime under a time budget. Design and
//! sources: docs/evidence/project-memory.md.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, ErrorCode, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::conversation::{AssistantPart, Body, Message};
pub use crate::store_pool::{PoolBinding, PoolCooldown};
pub use crate::store_threads::{SessionUsage, ThreadSummary};
use crate::ErrorInfo;

const SCHEMA_VERSION: i64 = 12;
/// Tool results above this size are stored once in `blobs` and referenced.
const BLOB_THRESHOLD: usize = 16 * 1024;
/// Text indexed per message; the rest stays readable but unsearchable.
const INDEXED_TEXT_MAX: usize = 64 * 1024;
/// Idle reader connections kept for reuse.
const READER_POOL: usize = 4;
/// SQLite virtual-machine steps between deadline checks.
const PROGRESS_STEPS: i32 = 1000;
/// Personalized PageRank teleport probability and push tolerance.
const PPR_ALPHA: f64 = 0.15;
const PPR_EPSILON: f64 = 1e-4;
const PPR_MAX_PUSHES: usize = 20_000;

const SCHEMA: &str = r#"
CREATE TABLE projects(
    id INTEGER PRIMARY KEY,
    key TEXT NOT NULL UNIQUE,
    created INTEGER NOT NULL
);
CREATE TABLE sessions(
    id INTEGER PRIMARY KEY,
    project_id INTEGER REFERENCES projects(id),
    title TEXT NOT NULL,
    created INTEGER NOT NULL
);
CREATE INDEX sessions_project ON sessions(project_id);
CREATE TABLE messages(
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions(id),
    seq INTEGER NOT NULL,
    role TEXT NOT NULL,
    body TEXT NOT NULL,
    text TEXT NOT NULL,
    created INTEGER NOT NULL,
    UNIQUE(session_id, seq)
);
CREATE INDEX messages_created ON messages(created);
CREATE VIRTUAL TABLE messages_fts USING fts5(
    text, content='messages', content_rowid='id', tokenize='trigram'
);
CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TABLE blobs(
    hash TEXT PRIMARY KEY,
    data BLOB NOT NULL
) WITHOUT ROWID;
CREATE TABLE notes(
    id INTEGER PRIMARY KEY,
    project_id INTEGER REFERENCES projects(id),
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    summary TEXT NOT NULL,
    keywords TEXT NOT NULL,
    condition TEXT NOT NULL,
    body TEXT NOT NULL,
    tags TEXT NOT NULL,
    status TEXT NOT NULL,
    source TEXT NOT NULL,
    up INTEGER NOT NULL DEFAULT 0,
    down INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL,
    last_used INTEGER,
    superseded_by INTEGER REFERENCES notes(id),
    valid_to INTEGER
);
CREATE INDEX notes_project ON notes(project_id, kind);
CREATE VIRTUAL TABLE notes_fts USING fts5(
    title, summary, keywords, condition, body, tags,
    content='notes', content_rowid='id', tokenize='trigram'
);
CREATE TRIGGER notes_ai AFTER INSERT ON notes BEGIN
    INSERT INTO notes_fts(rowid, title, summary, keywords, condition, body, tags)
    VALUES (new.id, new.title, new.summary, new.keywords, new.condition, new.body, new.tags);
END;
CREATE TRIGGER notes_au AFTER UPDATE OF title, summary, keywords, condition, body, tags ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, title, summary, keywords, condition, body, tags)
    VALUES ('delete', old.id, old.title, old.summary, old.keywords, old.condition, old.body, old.tags);
    INSERT INTO notes_fts(rowid, title, summary, keywords, condition, body, tags)
    VALUES (new.id, new.title, new.summary, new.keywords, new.condition, new.body, new.tags);
END;
CREATE TABLE tags(
    tag TEXT NOT NULL,
    note_id INTEGER NOT NULL REFERENCES notes(id),
    PRIMARY KEY(tag, note_id)
) WITHOUT ROWID;
CREATE INDEX tags_note ON tags(note_id, tag);
CREATE TABLE tag_aliases(
    alias TEXT PRIMARY KEY,
    tag TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE links(
    src INTEGER NOT NULL REFERENCES notes(id),
    dst INTEGER NOT NULL REFERENCES notes(id),
    kind TEXT NOT NULL,
    PRIMARY KEY(src, dst, kind)
) WITHOUT ROWID;
CREATE INDEX links_dst ON links(dst, src);
"#;

/// Rules (their origin, strength and enforcement), issue priorities, and
/// the event log of rule applications and issue changes.
const SCHEMA_V2: &str = r#"
CREATE TABLE rules(
    note_id INTEGER PRIMARY KEY REFERENCES notes(id),
    strength TEXT NOT NULL,
    origin_path TEXT,
    origin_start INTEGER,
    origin_end INTEGER,
    section TEXT NOT NULL DEFAULT '',
    ordinal INTEGER,
    trigger_tool TEXT,
    patterns TEXT,
    action TEXT,
    enforcement TEXT
);
CREATE INDEX rules_origin ON rules(origin_path, section, ordinal);
CREATE INDEX rules_enforcement ON rules(enforcement, trigger_tool);
CREATE TABLE issues(
    note_id INTEGER PRIMARY KEY REFERENCES notes(id),
    priority INTEGER NOT NULL
);
CREATE TABLE note_events(
    id INTEGER PRIMARY KEY,
    note_id INTEGER NOT NULL REFERENCES notes(id),
    at INTEGER NOT NULL,
    event TEXT NOT NULL,
    actor TEXT NOT NULL,
    session_id INTEGER,
    seq INTEGER,
    detail TEXT NOT NULL DEFAULT ''
);
CREATE INDEX note_events_note ON note_events(note_id, at);
"#;

/// How each rule was cut from its document, and the hash of each imported
/// section so an unchanged section is not sent to the model again.
const SCHEMA_V3: &str = r#"
ALTER TABLE rules ADD COLUMN segmented_by TEXT NOT NULL DEFAULT 'mechanical';
CREATE TABLE rule_sections(
    scope INTEGER NOT NULL,
    path TEXT NOT NULL,
    section TEXT NOT NULL,
    hash TEXT NOT NULL,
    PRIMARY KEY(scope, path, section)
) WITHOUT ROWID;
"#;

/// The model's fields of an extracted rule, as JSON.
const SCHEMA_V4: &str = "ALTER TABLE rules ADD COLUMN details TEXT NOT NULL DEFAULT '{}';";

/// The work list of a thread, kept apart from its history so it survives
/// compaction as written.
const SCHEMA_V5: &str = r#"
CREATE TABLE plans(
    session_id INTEGER PRIMARY KEY REFERENCES sessions(id),
    items TEXT NOT NULL,
    updated INTEGER NOT NULL
);
"#;

/// The log of requests to models (docs/evidence/usage-log.md).
const SCHEMA_V6: &str = r#"
CREATE TABLE requests(
    id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    session_id INTEGER,
    purpose TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    input INTEGER,
    output INTEGER,
    cached INTEGER,
    first_output_ms INTEGER,
    duration_ms INTEGER NOT NULL,
    outcome TEXT NOT NULL
);
CREATE INDEX requests_at ON requests(at);
"#;

/// The goal of a thread (docs/evidence/goal-command.md), as JSON written by
/// the harness, kept apart from the history like the plan.
const SCHEMA_V7: &str = r#"
CREATE TABLE goals(
    session_id INTEGER PRIMARY KEY REFERENCES sessions(id),
    goal TEXT NOT NULL,
    updated INTEGER NOT NULL
);
"#;

/// The person's standing instructions in a thread
/// (docs/evidence/promise-list.md), as JSON written by the harness, and the
/// position of the message that last replaced them.
const SCHEMA_V8: &str = r#"
CREATE TABLE promises(
    session_id INTEGER PRIMARY KEY REFERENCES sessions(id),
    items TEXT NOT NULL,
    seq INTEGER NOT NULL,
    updated INTEGER NOT NULL
);
"#;

/// The provider, model, reasoning and approval mode a thread chose
/// (docs/evidence/account-pool.md), as JSON written by the harness.
const SCHEMA_V9: &str = r#"
CREATE TABLE thread_settings(
    session_id INTEGER PRIMARY KEY REFERENCES sessions(id),
    settings TEXT NOT NULL,
    updated INTEGER NOT NULL
);
"#;

/// Thread ids that are never given twice, even after the latest thread was
/// deleted: `sessions` predates `AUTOINCREMENT`, so its ids come from this
/// table's sequence, started above every id in use. What the latest turn of
/// a thread left for the person (its held calls and questions), and the
/// questions of MCP servers, kept so a restart does not lose them.
const SCHEMA_V10: &str = r#"
CREATE TABLE session_ids(id INTEGER PRIMARY KEY AUTOINCREMENT);
INSERT INTO session_ids(id) SELECT MAX(id) FROM sessions HAVING MAX(id) IS NOT NULL;
DELETE FROM session_ids;
CREATE TABLE waiting(
    session_id INTEGER PRIMARY KEY REFERENCES sessions(id),
    held TEXT NOT NULL,
    questions TEXT NOT NULL,
    updated INTEGER NOT NULL
);
CREATE TABLE elicitations(
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL,
    question TEXT NOT NULL,
    expired INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX elicitations_session ON elicitations(session_id);
"#;

/// The account pool (docs/evidence/account-pool.md): which account of a
/// provider each thread stays on, and the quota cooldowns of accounts, so a
/// restart neither loses a thread's prompt cache nor sends to an account
/// that refused on quota until a later time. `kind` is the credential
/// store's provider name and `account` the account id there.
const SCHEMA_V11: &str = r#"
CREATE TABLE pool_affinity(
    session_id INTEGER NOT NULL,
    provider TEXT NOT NULL,
    kind TEXT NOT NULL,
    account TEXT NOT NULL,
    bound INTEGER NOT NULL,
    used INTEGER NOT NULL,
    PRIMARY KEY(session_id, provider)
);
CREATE INDEX pool_affinity_account ON pool_affinity(kind, account);
CREATE TABLE pool_cooldowns(
    kind TEXT NOT NULL,
    account TEXT NOT NULL,
    until INTEGER NOT NULL,
    avoid_until INTEGER NOT NULL,
    since INTEGER NOT NULL,
    source TEXT NOT NULL,
    status INTEGER NOT NULL,
    PRIMARY KEY(kind, account)
);
"#;

/// The account pool's cooldowns of one quota group of an account (a
/// ChatGPT model group, an Anthropic model family), apart from the whole
/// account's, and which thread each forked thread came from, so a fork
/// starts on its parent's account (docs/evidence/account-pool.md).
const SCHEMA_V12: &str = r#"
CREATE TABLE pool_scoped_cooldowns(
    kind TEXT NOT NULL,
    account TEXT NOT NULL,
    scope TEXT NOT NULL,
    until INTEGER NOT NULL,
    avoid_until INTEGER NOT NULL,
    since INTEGER NOT NULL,
    source TEXT NOT NULL,
    status INTEGER NOT NULL,
    PRIMARY KEY(kind, account, scope)
);
CREATE TABLE thread_lineage(
    session_id INTEGER PRIMARY KEY,
    parent INTEGER NOT NULL
);
CREATE INDEX thread_lineage_parent ON thread_lineage(parent);
"#;

/// Schema steps; step `i` brings the database to version `i + 1`.
const MIGRATIONS: [&str; 12] = [
    SCHEMA, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7, SCHEMA_V8, SCHEMA_V9,
    SCHEMA_V10, SCHEMA_V11, SCHEMA_V12,
];

/// A thread id never given before (`SCHEMA_V10`).
pub(crate) fn new_session_id(db: &Connection) -> Result<i64, ErrorInfo> {
    db.execute("INSERT INTO session_ids DEFAULT VALUES", [])
        .map_err(store_error)?;
    let id = db.last_insert_rowid();
    db.execute("DELETE FROM session_ids WHERE id = ?1", [id])
        .map_err(store_error)?;
    Ok(id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProjectId(pub i64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub i64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NoteId(pub i64);

/// A history message with its position and time.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredMessage {
    pub seq: i64,
    pub created: i64,
    pub message: Message,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    /// A condition-scoped lesson: what to do or avoid, and why.
    Lesson,
    /// A reusable way of doing something.
    Procedure,
    /// A claim and the evidence it rests on (the evidence ledger).
    Claim,
    /// Reference knowledge about a named subject.
    Knowledge,
    /// Something to keep to (`crate::rules`).
    Rule,
    /// Work that outlives a conversation (`crate::rules`).
    Issue,
}

impl NoteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lesson => "lesson",
            Self::Procedure => "procedure",
            Self::Claim => "claim",
            Self::Knowledge => "knowledge",
            Self::Rule => "rule",
            Self::Issue => "issue",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "lesson" => Self::Lesson,
            "procedure" => Self::Procedure,
            "claim" => Self::Claim,
            "knowledge" => Self::Knowledge,
            "rule" => Self::Rule,
            "issue" => Self::Issue,
            _ => return None,
        })
    }

    /// Statuses the kind may take, the usable ones first.
    pub(crate) fn statuses(self) -> &'static [&'static str] {
        match self {
            Self::Lesson | Self::Procedure | Self::Rule => &["active", "candidate", "retired"],
            Self::Issue => &["open", "in_progress", "blocked", "done", "wontfix"],
            Self::Claim => &["verified", "approved", "assumed", "unverified", "retired"],
            Self::Knowledge => &["active", "retired"],
        }
    }
}

/// Who a note comes from. Only a person or an external check makes a
/// lesson usable at once; a model's proposal waits as a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Person,
    Verified,
    Model,
}

impl Source {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Verified => "verified",
            Self::Model => "model",
        }
    }
}

fn initial_status(kind: NoteKind, source: Source) -> &'static str {
    match (kind, source) {
        (NoteKind::Lesson | NoteKind::Procedure | NoteKind::Rule, Source::Model) => "candidate",
        (NoteKind::Lesson | NoteKind::Procedure | NoteKind::Rule | NoteKind::Knowledge, _) => {
            "active"
        }
        (NoteKind::Issue, _) => "open",
        (NoteKind::Claim, Source::Verified) => "verified",
        (NoteKind::Claim, Source::Person) => "approved",
        (NoteKind::Claim, Source::Model) => "unverified",
    }
}

/// Statuses a note may be injected or relied on with.
const USABLE: &str = "('active', 'verified', 'approved')";

/// A note to add. `project: None` makes it shared across projects.
#[derive(Debug, Clone, PartialEq)]
pub struct NewNote {
    pub project: Option<ProjectId>,
    pub kind: NoteKind,
    pub title: String,
    pub summary: String,
    pub keywords: String,
    /// When the note applies (lessons and procedures).
    pub condition: String,
    pub body: String,
    pub source: Source,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub id: NoteId,
    pub project: Option<ProjectId>,
    pub kind: NoteKind,
    pub title: String,
    pub summary: String,
    pub keywords: String,
    pub condition: String,
    pub body: String,
    pub tags: Vec<String>,
    pub status: String,
    pub source: String,
    pub up: i64,
    pub down: i64,
    pub created: i64,
    pub last_used: Option<i64>,
    pub superseded_by: Option<NoteId>,
}

/// One search hit; `snippet` marks matches with `[` `]`.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteHit {
    pub id: NoteId,
    pub kind: NoteKind,
    pub title: String,
    pub summary: String,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MessageHit {
    pub session: SessionId,
    pub seq: i64,
    pub role: String,
    pub created: i64,
    pub snippet: String,
}

/// Results cut short by the time budget say so.
#[derive(Debug, Clone, PartialEq)]
pub struct Found<T> {
    pub items: Vec<T>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NoteQuery {
    /// Whitespace-separated terms of at least three characters each.
    pub text: String,
    pub project: Option<ProjectId>,
    pub include_shared: bool,
    /// Empty means every kind.
    pub kinds: Vec<NoteKind>,
    /// Only notes that may be relied on (no candidates or unverified claims).
    pub usable_only: bool,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub limit: usize,
    pub budget: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryQuery {
    pub text: String,
    /// `None` searches every project's sessions.
    pub project: Option<ProjectId>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub limit: usize,
    pub budget: Duration,
}

/// The database. Cloning shares it; writes are serialized on one
/// connection, reads use their own so they never wait for a write (WAL).
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

/// A question of an MCP server waiting for the person.
struct Waiting {
    session: SessionId,
    question: serde_json::Value,
    answer: tokio::sync::oneshot::Sender<serde_json::Value>,
}

struct Inner {
    path: PathBuf,
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
    /// The MCP servers named beside the database, started at first use and
    /// stopped when they are forgotten or the last copy of the store is
    /// dropped.
    mcp: tokio::sync::Mutex<Option<Arc<crate::mcp::Tools>>>,
    /// Questions of MCP servers waiting for the person's answer, by id, with
    /// the thread whose turn asked and the question as it was reported.
    elicitations: Mutex<HashMap<u64, Waiting>>,
    next_elicitation: std::sync::atomic::AtomicU64,
}

impl Store {
    /// The folder for files that belong with this database's threads, such
    /// as command logs: beside the database, named after it.
    pub(crate) fn files_folder(&self) -> PathBuf {
        self.inner.path.with_extension("files")
    }

    pub(crate) fn mcp(&self) -> &tokio::sync::Mutex<Option<Arc<crate::mcp::Tools>>> {
        &self.inner.mcp
    }

    /// Lets the MCP servers start anew at their next use, as after a plugin
    /// is installed.
    pub(crate) async fn forget_mcp(&self) {
        *self.inner.mcp.lock().await = None;
    }

    /// Registers a question of an MCP server for the person: its id, and
    /// where the answer arrives. [`Self::describe_elicitation`] keeps the
    /// question as reported, for listing.
    pub(crate) fn wait_elicitation(
        &self,
        session: SessionId,
    ) -> (u64, tokio::sync::oneshot::Receiver<serde_json::Value>) {
        let id = self
            .inner
            .next_elicitation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.inner
            .elicitations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                id,
                Waiting {
                    session,
                    question: serde_json::Value::Null,
                    answer: sender,
                },
            );
        (id, receiver)
    }

    pub(crate) fn describe_elicitation(&self, id: u64, question: serde_json::Value) {
        let session = match self
            .inner
            .elicitations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_mut(&id)
        {
            Some(waiting) => {
                waiting.question = question.clone();
                waiting.session
            }
            None => return,
        };
        let question = question.to_string();
        self.in_background(move |db| {
            db.execute(
                "INSERT OR REPLACE INTO elicitations(id, session_id, question, expired)
                 VALUES (?1, ?2, ?3, 0)",
                params![id as i64, session.0, question],
            )
        });
    }

    /// Runs a small write of the questions of MCP servers without waiting
    /// for it: their callers are not async. Its failure only costs the
    /// question's listing after a restart, so it is reported, not returned.
    fn in_background(
        &self,
        work: impl FnOnce(&Connection) -> rusqlite::Result<usize> + Send + 'static,
    ) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let store = self.clone();
        runtime.spawn(async move {
            if let Err(error) = store.write(move |db| work(db).map_err(store_error)).await {
                eprintln!(
                    "inocode: a question of an MCP server was not saved: {}",
                    error.message
                );
            }
        });
    }

    /// Marks the questions of MCP servers saved by an earlier run as
    /// expired: the turns that waited for them ended with that run.
    pub async fn expire_elicitations(&self) -> Result<(), ErrorInfo> {
        self.write(|db| {
            db.execute("UPDATE elicitations SET expired = 1 WHERE expired = 0", [])
                .map(|_| ())
                .map_err(store_error)
        })
        .await
    }

    /// The questions of MCP servers of `session` that expired with an
    /// earlier run of the server, oldest first.
    pub(crate) async fn expired_elicitations(
        &self,
        session: SessionId,
    ) -> Result<Vec<(u64, serde_json::Value)>, ErrorInfo> {
        self.read(Duration::from_secs(5), move |db| {
            let mut statement = db
                .prepare_cached(
                    "SELECT id, question FROM elicitations
                     WHERE session_id = ?1 AND expired = 1 ORDER BY id",
                )
                .map_err(store_error)?;
            let rows = statement
                .query_map([session.0], |row| {
                    Ok((row.get::<_, i64>(0)? as u64, row.get::<_, String>(1)?))
                })
                .map_err(store_error)?;
            let mut found = Vec::new();
            for row in rows {
                let (id, question) = row.map_err(store_error)?;
                found.push((id, serde_json::from_str(&question).unwrap_or_default()));
            }
            Ok(found)
        })
        .await
    }

    /// Removes the expired question `id` of `session`; whether there was one.
    pub(crate) async fn drop_expired_elicitation(
        &self,
        session: SessionId,
        id: u64,
    ) -> Result<bool, ErrorInfo> {
        self.write(move |db| {
            db.execute(
                "DELETE FROM elicitations WHERE id = ?1 AND session_id = ?2 AND expired = 1",
                params![id as i64, session.0],
            )
            .map(|deleted| deleted == 1)
            .map_err(store_error)
        })
        .await
    }

    /// What the latest turn of `session` left for the person, as JSON: the
    /// held calls and the questions.
    pub(crate) async fn set_waiting(
        &self,
        session: SessionId,
        held: String,
        questions: String,
    ) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            db.execute(
                "INSERT INTO waiting(session_id, held, questions, updated)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(session_id) DO UPDATE SET
                     held = excluded.held, questions = excluded.questions,
                     updated = excluded.updated",
                params![session.0, held, questions, now()],
            )
            .map(|_| ())
            .map_err(store_error)
        })
        .await
    }

    pub(crate) async fn waiting(
        &self,
        session: SessionId,
    ) -> Result<Option<(String, String)>, ErrorInfo> {
        self.read(Duration::from_secs(5), move |db| {
            db.query_row(
                "SELECT held, questions FROM waiting WHERE session_id = ?1",
                [session.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(store_error)
        })
        .await
    }

    /// The questions of MCP servers that `session`'s turn waits on, oldest
    /// first.
    pub(crate) fn elicitations(&self, session: SessionId) -> Vec<(u64, serde_json::Value)> {
        let waiting = self
            .inner
            .elicitations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut found: Vec<(u64, serde_json::Value)> = waiting
            .iter()
            .filter(|(_, waiting)| waiting.session == session)
            .map(|(id, waiting)| (*id, waiting.question.clone()))
            .collect();
        found.sort_by_key(|(id, _)| *id);
        found
    }

    /// Drops a question that is no longer waited for.
    pub(crate) fn forget_elicitation(&self, id: u64) {
        self.inner
            .elicitations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&id);
        self.forget_saved_elicitation(id);
    }

    fn forget_saved_elicitation(&self, id: u64) {
        self.in_background(move |db| {
            db.execute("DELETE FROM elicitations WHERE id = ?1", [id as i64])
        });
    }

    /// Hands the person's answer to the question `id` of `session`; whether
    /// such a question was waiting.
    pub fn answer_elicitation(
        &self,
        session: SessionId,
        id: u64,
        answer: serde_json::Value,
    ) -> bool {
        let mut waiting = self
            .inner
            .elicitations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match waiting.get(&id) {
            Some(asked) if asked.session == session => {}
            _ => return false,
        }
        let asked = waiting.remove(&id).expect("checked above");
        drop(waiting);
        self.forget_saved_elicitation(id);
        asked.answer.send(answer).is_ok()
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, ErrorInfo> {
        let path = path.as_ref().to_path_buf();
        let opened = path.clone();
        let (writer, last_elicitation) = blocking(move || {
            let mut connection = connect(&opened)?;
            migrate(&mut connection)?;
            // Ids go on after the saved questions, so an expired question is
            // never mistaken for a new one.
            let last: i64 = connection
                .query_row("SELECT COALESCE(MAX(id), 0) FROM elicitations", [], |row| {
                    row.get(0)
                })
                .map_err(store_error)?;
            Ok((connection, last.max(0) as u64))
        })
        .await?;
        Ok(Self {
            inner: Arc::new(Inner {
                path,
                writer: Mutex::new(writer),
                readers: Mutex::new(Vec::new()),
                mcp: tokio::sync::Mutex::new(None),
                elicitations: Mutex::new(HashMap::new()),
                next_elicitation: std::sync::atomic::AtomicU64::new(last_elicitation),
            }),
        })
    }

    pub(crate) async fn write<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Connection) -> Result<T, ErrorInfo> + Send + 'static,
    ) -> Result<T, ErrorInfo> {
        let inner = Arc::clone(&self.inner);
        blocking(move || {
            let mut connection = inner.writer.lock().map_err(|_| poisoned())?;
            work(&mut connection)
        })
        .await
    }

    /// Runs `work` on a reader that SQLite interrupts once `budget` passes.
    pub(crate) async fn read<T: Send + 'static>(
        &self,
        budget: Duration,
        work: impl FnOnce(&Connection) -> Result<T, ErrorInfo> + Send + 'static,
    ) -> Result<T, ErrorInfo> {
        let inner = Arc::clone(&self.inner);
        blocking(move || {
            let pooled = inner.readers.lock().map_err(|_| poisoned())?.pop();
            let mut connection = match pooled {
                Some(connection) => connection,
                None => connect(&inner.path)?,
            };
            let deadline = Instant::now() + budget;
            connection
                .progress_handler(PROGRESS_STEPS, Some(move || Instant::now() >= deadline))
                .map_err(store_error)?;
            let result = work(&connection);
            connection
                .progress_handler(0, None::<fn() -> bool>)
                .map_err(store_error)?;
            let mut readers = inner.readers.lock().map_err(|_| poisoned())?;
            if readers.len() < READER_POOL {
                readers.push(connection);
            }
            result
        })
        .await
    }

    /// The project with this key (a repository remote or folder path).
    pub async fn project(&self, key: &str) -> Result<ProjectId, ErrorInfo> {
        let key = key.to_string();
        self.write(move |db| {
            db.execute(
                "INSERT OR IGNORE INTO projects(key, created) VALUES (?1, ?2)",
                params![key, now()],
            )
            .map_err(store_error)?;
            db.query_row("SELECT id FROM projects WHERE key = ?1", [&key], |row| {
                row.get(0)
            })
            .map(ProjectId)
            .map_err(store_error)
        })
        .await
    }

    pub async fn start_session(
        &self,
        project: Option<ProjectId>,
        title: &str,
    ) -> Result<SessionId, ErrorInfo> {
        let title = title.to_string();
        self.write(move |db| {
            let tx = db.transaction().map_err(store_error)?;
            let id = new_session_id(&tx)?;
            tx.execute(
                "INSERT INTO sessions(id, project_id, title, created) VALUES (?1, ?2, ?3, ?4)",
                params![id, project.map(|p| p.0), title, now()],
            )
            .map_err(store_error)?;
            tx.commit().map_err(store_error)?;
            Ok(SessionId(id))
        })
        .await
    }

    pub async fn session_exists(&self, session: SessionId) -> Result<bool, ErrorInfo> {
        self.read(Duration::from_secs(5), move |db| {
            db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
                [session.0],
                |row| row.get(0),
            )
            .map_err(store_error)
        })
        .await
    }

    /// The project of `session` and its key, when it has one.
    pub async fn session_project(
        &self,
        session: SessionId,
    ) -> Result<Option<(ProjectId, String)>, ErrorInfo> {
        self.read(Duration::from_secs(5), move |db| {
            db.query_row(
                "SELECT p.id, p.key FROM sessions s JOIN projects p ON p.id = s.project_id
                 WHERE s.id = ?1",
                [session.0],
                |row| Ok((ProjectId(row.get(0)?), row.get(1)?)),
            )
            .optional()
            .map_err(store_error)
        })
        .await
    }

    /// Replaces the work list of `session` (JSON written by the harness).
    pub async fn set_plan(&self, session: SessionId, items: String) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            db.execute(
                "INSERT INTO plans(session_id, items, updated) VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET items = ?2, updated = ?3",
                params![session.0, items, now()],
            )
            .map(drop)
            .map_err(store_error)
        })
        .await
    }

    pub async fn plan(&self, session: SessionId) -> Result<Option<String>, ErrorInfo> {
        self.read(Duration::from_secs(5), move |db| {
            db.query_row(
                "SELECT items FROM plans WHERE session_id = ?1",
                [session.0],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_error)
        })
        .await
    }

    /// Replaces the goal of `session` (JSON written by the harness).
    pub async fn set_goal(&self, session: SessionId, goal: String) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            db.execute(
                "INSERT INTO goals(session_id, goal, updated) VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET goal = ?2, updated = ?3",
                params![session.0, goal, now()],
            )
            .map(drop)
            .map_err(store_error)
        })
        .await
    }

    pub async fn goal(&self, session: SessionId) -> Result<Option<String>, ErrorInfo> {
        self.read(Duration::from_secs(5), move |db| {
            db.query_row(
                "SELECT goal FROM goals WHERE session_id = ?1",
                [session.0],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_error)
        })
        .await
    }

    /// Replaces what `session` chose to run on (JSON written by the
    /// harness).
    pub async fn set_thread_settings(
        &self,
        session: SessionId,
        settings: String,
    ) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            db.execute(
                "INSERT INTO thread_settings(session_id, settings, updated) VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET settings = ?2, updated = ?3",
                params![session.0, settings, now()],
            )
            .map(drop)
            .map_err(store_error)
        })
        .await
    }

    pub async fn thread_settings(&self, session: SessionId) -> Result<Option<String>, ErrorInfo> {
        self.read(Duration::from_secs(5), move |db| {
            db.query_row(
                "SELECT settings FROM thread_settings WHERE session_id = ?1",
                [session.0],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_error)
        })
        .await
    }

    /// Replaces the person's standing instructions of `session` (JSON
    /// written by the harness), as of the message at `seq`.
    pub async fn set_promises(
        &self,
        session: SessionId,
        items: String,
        seq: i64,
    ) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            db.execute(
                "INSERT INTO promises(session_id, items, seq, updated) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(session_id) DO UPDATE SET items = ?2, seq = ?3, updated = ?4",
                params![session.0, items, seq, now()],
            )
            .map(drop)
            .map_err(store_error)
        })
        .await
    }

    /// The standing instructions and the position they were last replaced at.
    pub async fn promises(&self, session: SessionId) -> Result<Option<(String, i64)>, ErrorInfo> {
        self.read(Duration::from_secs(5), move |db| {
            db.query_row(
                "SELECT items, seq FROM promises WHERE session_id = ?1",
                [session.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(store_error)
        })
        .await
    }

    /// Appends messages in one transaction and returns their positions.
    pub async fn append(
        &self,
        session: SessionId,
        messages: Vec<Message>,
    ) -> Result<Vec<i64>, ErrorInfo> {
        self.write(move |db| {
            let tx = db.transaction().map_err(store_error)?;
            let mut seq: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(seq), 0) FROM messages WHERE session_id = ?1",
                    [session.0],
                    |row| row.get(0),
                )
                .map_err(store_error)?;
            let created = now();
            let mut positions = Vec::with_capacity(messages.len());
            for message in &messages {
                seq += 1;
                let (role, body) = encode(&tx, message)?;
                tx.execute(
                    "INSERT INTO messages(session_id, seq, role, body, text, created)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![session.0, seq, role, body, indexed_text(message), created],
                )
                .map_err(store_error)?;
                positions.push(seq);
            }
            tx.commit().map_err(store_error)?;
            Ok(positions)
        })
        .await
    }

    /// Up to `limit` messages after `after` (exclusive), oldest first.
    pub async fn since(
        &self,
        session: SessionId,
        after: i64,
        limit: usize,
    ) -> Result<Vec<StoredMessage>, ErrorInfo> {
        self.read(Duration::from_secs(30), move |db| {
            let mut statement = db
                .prepare_cached(
                    "SELECT seq, created, body FROM messages
                     WHERE session_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
                )
                .map_err(store_error)?;
            let rows = statement
                .query_map(params![session.0, after, limit as i64], raw_message)
                .map_err(store_error)?;
            rows.map(|row| decode(db, row.map_err(store_error)?))
                .collect()
        })
        .await
    }

    /// The page of up to `limit` messages before `before` (the latest when
    /// `None`), oldest first: how history is scrolled back.
    pub async fn page(
        &self,
        session: SessionId,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<StoredMessage>, ErrorInfo> {
        self.read(Duration::from_secs(30), move |db| {
            let mut statement = db
                .prepare_cached(
                    "SELECT seq, created, body FROM messages
                     WHERE session_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3",
                )
                .map_err(store_error)?;
            let rows = statement
                .query_map(
                    params![session.0, before.unwrap_or(i64::MAX), limit as i64],
                    raw_message,
                )
                .map_err(store_error)?;
            let mut page = rows
                .map(|row| decode(db, row.map_err(store_error)?))
                .collect::<Result<Vec<_>, _>>()?;
            page.reverse();
            Ok(page)
        })
        .await
    }

    /// Stores `data` once and returns its SHA-256 hex digest.
    pub async fn put_blob(&self, data: Vec<u8>) -> Result<String, ErrorInfo> {
        self.write(move |db| put_blob(db, &data)).await
    }

    pub async fn blob(&self, hash: &str) -> Result<Option<Vec<u8>>, ErrorInfo> {
        let hash = hash.to_string();
        self.read(Duration::from_secs(30), move |db| get_blob(db, &hash))
            .await
    }

    pub async fn add_note(&self, note: NewNote) -> Result<NoteId, ErrorInfo> {
        self.write(move |db| {
            let tags = normalize_tags(db, &note.tags)?;
            let tx = db.transaction().map_err(store_error)?;
            let time = now();
            tx.execute(
                "INSERT INTO notes(project_id, kind, title, summary, keywords, condition, body,
                                   tags, status, source, created, updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
                params![
                    note.project.map(|p| p.0),
                    note.kind.as_str(),
                    note.title,
                    note.summary,
                    note.keywords,
                    note.condition,
                    note.body,
                    tags.join(" "),
                    initial_status(note.kind, note.source),
                    note.source.as_str(),
                    time,
                ],
            )
            .map_err(store_error)?;
            let id = tx.last_insert_rowid();
            for tag in &tags {
                tx.execute(
                    "INSERT OR IGNORE INTO tags(tag, note_id) VALUES (?1, ?2)",
                    params![tag, id],
                )
                .map_err(store_error)?;
            }
            tx.commit().map_err(store_error)?;
            Ok(NoteId(id))
        })
        .await
    }

    pub async fn note(&self, id: NoteId) -> Result<Option<Note>, ErrorInfo> {
        self.read(Duration::from_secs(30), move |db| load_note(db, id))
            .await
    }

    /// Adds tags (normalized, aliases resolved) to a note.
    pub async fn tag(&self, id: NoteId, tags: Vec<String>) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            let added = normalize_tags(db, &tags)?;
            let tx = db.transaction().map_err(store_error)?;
            for tag in &added {
                tx.execute(
                    "INSERT OR IGNORE INTO tags(tag, note_id) VALUES (?1, ?2)",
                    params![tag, id.0],
                )
                .map_err(store_error)?;
            }
            let all: Vec<String> = {
                let mut statement = tx
                    .prepare_cached("SELECT tag FROM tags WHERE note_id = ?1 ORDER BY tag")
                    .map_err(store_error)?;
                let rows = statement
                    .query_map([id.0], |row| row.get(0))
                    .map_err(store_error)?;
                rows.collect::<Result<_, _>>().map_err(store_error)?
            };
            tx.execute(
                "UPDATE notes SET tags = ?1, updated = ?2 WHERE id = ?3",
                params![all.join(" "), now(), id.0],
            )
            .map_err(store_error)?;
            tx.commit().map_err(store_error)
        })
        .await
    }

    /// Every tag in use, with the number of notes carrying it, most used
    /// first.
    pub async fn tag_counts(&self) -> Result<Vec<(String, u64)>, ErrorInfo> {
        self.read(Duration::from_secs(30), move |db| {
            let mut statement = db
                .prepare_cached(
                    "SELECT tag, COUNT(*) FROM tags GROUP BY tag ORDER BY COUNT(*) DESC, tag",
                )
                .map_err(store_error)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
                })
                .map_err(store_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(store_error)
        })
        .await
    }

    /// Makes `alias` resolve to `tag` for future tagging and searches.
    pub async fn alias_tag(&self, alias: &str, tag: &str) -> Result<(), ErrorInfo> {
        let (alias, tag) = (normalize_tag(alias), normalize_tag(tag));
        self.write(move |db| {
            db.execute(
                "INSERT OR REPLACE INTO tag_aliases(alias, tag) VALUES (?1, ?2)",
                params![alias, tag],
            )
            .map(|_| ())
            .map_err(store_error)
        })
        .await
    }

    pub async fn link(&self, from: NoteId, to: NoteId, kind: &str) -> Result<(), ErrorInfo> {
        let kind = kind.to_string();
        self.write(move |db| {
            db.execute(
                "INSERT OR IGNORE INTO links(src, dst, kind) VALUES (?1, ?2, ?3)",
                params![from.0, to.0, kind],
            )
            .map(|_| ())
            .map_err(store_error)
        })
        .await
    }

    /// Changes a note's status; the status must belong to its kind.
    pub async fn set_status(&self, id: NoteId, status: &str) -> Result<(), ErrorInfo> {
        let status = status.to_string();
        self.write(move |db| {
            let kind = note_kind(db, id)?;
            if !kind.statuses().contains(&status.as_str()) {
                return Err(ErrorInfo::new(
                    "invalid_params",
                    format!(
                        "a {} note's status is one of {}",
                        kind.as_str(),
                        kind.statuses().join(", ")
                    ),
                ));
            }
            db.execute(
                "UPDATE notes SET status = ?1, updated = ?2 WHERE id = ?3",
                params![status, now(), id.0],
            )
            .map(|_| ())
            .map_err(store_error)
        })
        .await
    }

    /// Counts whether a note helped (`true`) or misled.
    pub async fn vote(&self, id: NoteId, helped: bool) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            let column = if helped { "up" } else { "down" };
            db.execute(
                &format!("UPDATE notes SET {column} = {column} + 1 WHERE id = ?1"),
                [id.0],
            )
            .map(|_| ())
            .map_err(store_error)
        })
        .await
    }

    /// Replaces `old` with `new` while keeping `old` as history.
    pub async fn supersede(&self, old: NoteId, new: NoteId) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            let tx = db.transaction().map_err(store_error)?;
            tx.execute(
                "UPDATE notes SET superseded_by = ?1, valid_to = ?2, updated = ?2 WHERE id = ?3",
                params![new.0, now(), old.0],
            )
            .map_err(store_error)?;
            tx.execute(
                "INSERT OR IGNORE INTO links(src, dst, kind) VALUES (?1, ?2, 'supersedes')",
                params![new.0, old.0],
            )
            .map_err(store_error)?;
            tx.commit().map_err(store_error)
        })
        .await
    }

    /// Moves a project note into the shared scope.
    pub async fn share(&self, id: NoteId) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            db.execute(
                "UPDATE notes SET project_id = NULL, updated = ?1 WHERE id = ?2",
                params![now(), id.0],
            )
            .map(|_| ())
            .map_err(store_error)
        })
        .await
    }

    /// Notes ranked by BM25 (title, keywords, condition and tags weigh most).
    pub async fn search_notes(&self, query: NoteQuery) -> Result<Found<NoteHit>, ErrorInfo> {
        let Some(matched) = fts_query(&query.text) else {
            return Ok(Found {
                items: Vec::new(),
                truncated: false,
            });
        };
        self.read(query.budget, move |db| {
            let mut sql = String::from(
                "SELECT n.id, n.kind, n.title, n.summary,
                        snippet(notes_fts, -1, '[', ']', '…', 16)
                 FROM notes_fts JOIN notes n ON n.id = notes_fts.rowid
                 WHERE notes_fts MATCH ?1
                   AND (n.project_id IS ?2 OR (?3 AND n.project_id IS NULL))
                   AND n.superseded_by IS NULL AND n.status != 'retired'
                   AND n.created >= ?4 AND n.created <= ?5",
            );
            if query.usable_only {
                sql.push_str(&format!(" AND n.status IN {USABLE}"));
            }
            if !query.kinds.is_empty() {
                let kinds: Vec<String> = query
                    .kinds
                    .iter()
                    .map(|kind| format!("'{}'", kind.as_str()))
                    .collect();
                sql.push_str(&format!(" AND n.kind IN ({})", kinds.join(", ")));
            }
            sql.push_str(" ORDER BY bm25(notes_fts, 4.0, 2.0, 3.0, 3.0, 1.0, 3.0) LIMIT ?6");
            let mut statement = db.prepare_cached(&sql).map_err(store_error)?;
            let rows = statement.query_map(
                params![
                    matched,
                    query.project.map(|p| p.0),
                    query.include_shared,
                    query.since.unwrap_or(i64::MIN),
                    query.until.unwrap_or(i64::MAX),
                    query.limit as i64,
                ],
                |row| {
                    Ok(NoteHit {
                        id: NoteId(row.get(0)?),
                        kind: NoteKind::parse(&row.get::<_, String>(1)?)
                            .unwrap_or(NoteKind::Knowledge),
                        title: row.get(2)?,
                        summary: row.get(3)?,
                        snippet: row.get(4)?,
                    })
                },
            );
            collect(rows)
        })
        .await
    }

    /// The usable lessons and procedures whose text matches the work at
    /// hand, best first; their use is recorded.
    pub async fn lessons_for(
        &self,
        project: Option<ProjectId>,
        context: &str,
        limit: usize,
        budget: Duration,
    ) -> Result<Found<NoteHit>, ErrorInfo> {
        let found = self
            .search_notes(NoteQuery {
                text: context.to_string(),
                project,
                include_shared: true,
                kinds: vec![NoteKind::Lesson, NoteKind::Procedure],
                usable_only: true,
                since: None,
                until: None,
                limit,
                budget,
            })
            .await?;
        let ids: Vec<i64> = found.items.iter().map(|hit| hit.id.0).collect();
        if !ids.is_empty() {
            self.write(move |db| {
                let tx = db.transaction().map_err(store_error)?;
                let time = now();
                for id in &ids {
                    tx.execute(
                        "UPDATE notes SET last_used = ?1 WHERE id = ?2",
                        params![time, id],
                    )
                    .map_err(store_error)?;
                }
                tx.commit().map_err(store_error)
            })
            .await?;
        }
        Ok(found)
    }

    /// History messages matching the terms, best first.
    pub async fn search_history(
        &self,
        query: HistoryQuery,
    ) -> Result<Found<MessageHit>, ErrorInfo> {
        let Some(matched) = fts_query(&query.text) else {
            return Ok(Found {
                items: Vec::new(),
                truncated: false,
            });
        };
        self.read(query.budget, move |db| {
            let mut statement = db
                .prepare_cached(
                    "SELECT m.session_id, m.seq, m.role, m.created,
                            snippet(messages_fts, 0, '[', ']', '…', 16)
                     FROM messages_fts
                     JOIN messages m ON m.id = messages_fts.rowid
                     JOIN sessions s ON s.id = m.session_id
                     WHERE messages_fts MATCH ?1
                       AND (?2 IS NULL OR s.project_id IS ?2)
                       AND m.created >= ?3 AND m.created <= ?4
                     ORDER BY bm25(messages_fts) LIMIT ?5",
                )
                .map_err(store_error)?;
            let rows = statement.query_map(
                params![
                    matched,
                    query.project.map(|p| p.0),
                    query.since.unwrap_or(i64::MIN),
                    query.until.unwrap_or(i64::MAX),
                    query.limit as i64,
                ],
                |row| {
                    Ok(MessageHit {
                        session: SessionId(row.get(0)?),
                        seq: row.get(1)?,
                        role: row.get(2)?,
                        created: row.get(3)?,
                        snippet: row.get(4)?,
                    })
                },
            );
            collect(rows)
        })
        .await
    }

    /// Notes associated with `seeds` through links and shared tags, ranked
    /// by Personalized PageRank approximated with the local push method, so
    /// the work depends on the tolerance rather than the size of the graph.
    /// Seeds, retired and superseded notes, and notes outside the scope are
    /// not returned.
    pub async fn associate(
        &self,
        seeds: Vec<NoteId>,
        project: Option<ProjectId>,
        limit: usize,
        budget: Duration,
    ) -> Result<Found<NoteHit>, ErrorInfo> {
        if seeds.is_empty() {
            return Ok(Found {
                items: Vec::new(),
                truncated: false,
            });
        }
        self.read(budget, move |db| {
            let deadline = Instant::now() + budget;
            let graph = Graph { db, project };
            let (scores, truncated) = match personalized_pagerank(&graph, &seeds, deadline) {
                Ok(result) => result,
                Err(error) if interrupted(&error) => (HashMap::new(), true),
                Err(error) => return Err(store_error(error)),
            };
            let seed_set: HashSet<NoteId> = seeds.iter().copied().collect();
            let mut ranked: Vec<(NoteId, f64)> = scores
                .into_iter()
                .filter_map(|(node, score)| match node {
                    Node::Note(id) if !seed_set.contains(&id) => Some((id, score)),
                    _ => None,
                })
                .collect();
            ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            ranked.truncate(limit);
            let mut items = Vec::with_capacity(ranked.len());
            for (id, _) in ranked {
                if let Some(note) = load_note(db, id)? {
                    items.push(NoteHit {
                        id,
                        kind: note.kind,
                        title: note.title,
                        summary: note.summary,
                        snippet: String::new(),
                    });
                }
            }
            Ok(Found { items, truncated })
        })
        .await
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Node {
    Note(NoteId),
    Tag(String),
}

/// The note graph seen from one project: notes are joined by links (either
/// direction) and through the tags they share.
struct Graph<'a> {
    db: &'a Connection,
    project: Option<ProjectId>,
}

impl Graph<'_> {
    fn neighbors(&self, node: &Node) -> rusqlite::Result<Vec<Node>> {
        const VISIBLE: &str = "n.superseded_by IS NULL AND n.status != 'retired'
             AND (n.project_id IS ?2 OR n.project_id IS NULL)";
        let project = self.project.map(|p| p.0);
        match node {
            Node::Note(id) => {
                let mut out = Vec::new();
                let mut linked = self.db.prepare_cached(&format!(
                    "SELECT n.id FROM (
                         SELECT dst AS other FROM links WHERE src = ?1
                         UNION SELECT src FROM links WHERE dst = ?1
                     ) JOIN notes n ON n.id = other WHERE {VISIBLE}"
                ))?;
                for id in linked.query_map(params![id.0, project], |row| row.get(0))? {
                    out.push(Node::Note(NoteId(id?)));
                }
                let mut tagged = self
                    .db
                    .prepare_cached("SELECT tag FROM tags WHERE note_id = ?1")?;
                for tag in tagged.query_map([id.0], |row| row.get(0))? {
                    out.push(Node::Tag(tag?));
                }
                Ok(out)
            }
            Node::Tag(tag) => {
                let mut tagged = self.db.prepare_cached(&format!(
                    "SELECT n.id FROM tags t JOIN notes n ON n.id = t.note_id
                     WHERE t.tag = ?1 AND {VISIBLE}"
                ))?;
                let rows = tagged.query_map(params![tag, project], |row| row.get(0))?;
                rows.map(|id| id.map(|id| Node::Note(NoteId(id)))).collect()
            }
        }
    }
}

/// Andersen–Chung–Lang push: each node keeps an estimate `p` and residual
/// `r`; a node whose residual reaches `ε·degree` keeps `α` of it and spreads
/// the rest evenly to its neighbours. A node is queued once its residual
/// reaches `ε`, and checked against its degree only when popped, so a hub's
/// neighbours are listed only if mass actually arrives there.
fn personalized_pagerank(
    graph: &Graph,
    seeds: &[NoteId],
    deadline: Instant,
) -> rusqlite::Result<(HashMap<Node, f64>, bool)> {
    let mut estimate: HashMap<Node, f64> = HashMap::new();
    let mut residual: HashMap<Node, f64> = HashMap::new();
    let mut adjacency: HashMap<Node, Vec<Node>> = HashMap::new();
    let mut queue: VecDeque<Node> = VecDeque::new();
    let mut queued: HashSet<Node> = HashSet::new();
    let share = 1.0 / seeds.len() as f64;
    for seed in seeds {
        let node = Node::Note(*seed);
        *residual.entry(node.clone()).or_default() += share;
        if queued.insert(node.clone()) {
            queue.push_back(node);
        }
    }
    let mut pushes = 0;
    while let Some(node) = queue.pop_front() {
        queued.remove(&node);
        if pushes >= PPR_MAX_PUSHES || Instant::now() >= deadline {
            return Ok((estimate, true));
        }
        let mass = residual.get(&node).copied().unwrap_or(0.0);
        if !adjacency.contains_key(&node) {
            let neighbors = graph.neighbors(&node)?;
            adjacency.insert(node.clone(), neighbors);
        }
        let neighbors = &adjacency[&node];
        if neighbors.is_empty() {
            *estimate.entry(node.clone()).or_default() += mass;
            residual.insert(node, 0.0);
            continue;
        }
        if mass < PPR_EPSILON * neighbors.len() as f64 {
            continue;
        }
        pushes += 1;
        *estimate.entry(node.clone()).or_default() += PPR_ALPHA * mass;
        residual.insert(node.clone(), 0.0);
        let spread = (1.0 - PPR_ALPHA) * mass / neighbors.len() as f64;
        for neighbor in neighbors.clone() {
            let value = residual.entry(neighbor.clone()).or_default();
            *value += spread;
            if *value >= PPR_EPSILON && queued.insert(neighbor.clone()) {
                queue.push_back(neighbor);
            }
        }
    }
    Ok((estimate, false))
}

fn connect(path: &Path) -> Result<Connection, ErrorInfo> {
    let connection = Connection::open(path).map_err(store_error)?;
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )
        .map_err(store_error)?;
    Ok(connection)
}

fn migrate(connection: &mut Connection) -> Result<(), ErrorInfo> {
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(store_error)?;
    if version > SCHEMA_VERSION {
        return Err(ErrorInfo::new(
            "store",
            format!(
                "the database schema version {version} is newer than this build ({SCHEMA_VERSION})"
            ),
        ));
    }
    for (step, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let tx = connection.transaction().map_err(store_error)?;
        tx.execute_batch(sql).map_err(store_error)?;
        tx.pragma_update(None, "user_version", step as i64 + 1)
            .map_err(store_error)?;
        tx.commit().map_err(store_error)?;
    }
    Ok(())
}

/// The JSON stored for a message, with large tool results moved to `blobs`.
fn encode(db: &Connection, message: &Message) -> Result<(&'static str, String), ErrorInfo> {
    let role = match &message.body {
        Body::User { .. } => "user",
        Body::Assistant { .. } => "assistant",
        Body::ToolResult { .. } => "tool_result",
    };
    let mut value = serde_json::to_value(message).map_err(json_error)?;
    if let Body::ToolResult { content, .. } = &message.body {
        if content.len() > BLOB_THRESHOLD {
            let hash = put_blob(db, content.as_bytes())?;
            value["body"]["content"] = Value::Null;
            value["body"]["content_blob"] = Value::String(hash);
        }
    }
    Ok((role, value.to_string()))
}

fn raw_message(row: &rusqlite::Row) -> rusqlite::Result<(i64, i64, String)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
}

fn decode(
    db: &Connection,
    (seq, created, body): (i64, i64, String),
) -> Result<StoredMessage, ErrorInfo> {
    let mut value: Value = serde_json::from_str(&body).map_err(json_error)?;
    if let Some(hash) = value["body"]["content_blob"].as_str().map(str::to_string) {
        let data = get_blob(db, &hash)?
            .ok_or_else(|| ErrorInfo::new("store", format!("blob {hash} is missing")))?;
        value["body"]["content"] = Value::String(String::from_utf8_lossy(&data).into_owned());
        if let Some(body) = value["body"].as_object_mut() {
            body.remove("content_blob");
        }
    }
    Ok(StoredMessage {
        seq,
        created,
        message: serde_json::from_value(value).map_err(json_error)?,
    })
}

/// The searchable text of a message: what was said, called, and answered.
/// Reasoning is not indexed.
fn indexed_text(message: &Message) -> String {
    let mut text = String::new();
    match &message.body {
        Body::User { parts, .. } => text = parts.join("\n"),
        Body::Assistant { parts, .. } => {
            for part in parts {
                match part {
                    AssistantPart::Text { text: part } => text.push_str(part),
                    AssistantPart::ToolCall {
                        name, arguments, ..
                    } => {
                        text.push_str(name);
                        text.push(' ');
                        text.push_str(arguments);
                    }
                    AssistantPart::Reasoning { .. } => continue,
                }
                text.push('\n');
            }
        }
        Body::ToolResult { name, content, .. } => {
            text.push_str(name);
            text.push('\n');
            text.push_str(content);
        }
    }
    if text.len() > INDEXED_TEXT_MAX {
        let mut end = INDEXED_TEXT_MAX;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn put_blob(db: &Connection, data: &[u8]) -> Result<String, ErrorInfo> {
    let hash: String = Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    db.execute(
        "INSERT OR IGNORE INTO blobs(hash, data) VALUES (?1, ?2)",
        params![hash, data],
    )
    .map_err(store_error)?;
    Ok(hash)
}

fn get_blob(db: &Connection, hash: &str) -> Result<Option<Vec<u8>>, ErrorInfo> {
    db.query_row("SELECT data FROM blobs WHERE hash = ?1", [hash], |row| {
        row.get(0)
    })
    .optional()
    .map_err(store_error)
}

fn note_kind(db: &Connection, id: NoteId) -> Result<NoteKind, ErrorInfo> {
    let kind: String = db
        .query_row("SELECT kind FROM notes WHERE id = ?1", [id.0], |row| {
            row.get(0)
        })
        .optional()
        .map_err(store_error)?
        .ok_or_else(|| ErrorInfo::new("not_found", format!("no note {}", id.0)))?;
    NoteKind::parse(&kind)
        .ok_or_else(|| ErrorInfo::new("store", format!("unknown note kind {kind}")))
}

pub(crate) fn load_note(db: &Connection, id: NoteId) -> Result<Option<Note>, ErrorInfo> {
    db.query_row(
        "SELECT id, project_id, kind, title, summary, keywords, condition, body, tags, status,
                source, up, down, created, last_used, superseded_by
         FROM notes WHERE id = ?1",
        [id.0],
        |row| {
            let tags: String = row.get(8)?;
            Ok(Note {
                id: NoteId(row.get(0)?),
                project: row.get::<_, Option<i64>>(1)?.map(ProjectId),
                kind: NoteKind::parse(&row.get::<_, String>(2)?).unwrap_or(NoteKind::Knowledge),
                title: row.get(3)?,
                summary: row.get(4)?,
                keywords: row.get(5)?,
                condition: row.get(6)?,
                body: row.get(7)?,
                tags: tags.split_whitespace().map(str::to_string).collect(),
                status: row.get(9)?,
                source: row.get(10)?,
                up: row.get(11)?,
                down: row.get(12)?,
                created: row.get(13)?,
                last_used: row.get(14)?,
                superseded_by: row.get::<_, Option<i64>>(15)?.map(NoteId),
            })
        },
    )
    .optional()
    .map_err(store_error)
}

/// `#Rust`, ` rust ` and `RUST` are one tag.
pub(crate) fn normalize_tag(tag: &str) -> String {
    tag.trim()
        .trim_start_matches('#')
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
}

pub(crate) fn normalize_tags(db: &Connection, tags: &[String]) -> Result<Vec<String>, ErrorInfo> {
    let mut out: Vec<String> = Vec::with_capacity(tags.len());
    for tag in tags {
        let tag = normalize_tag(tag);
        if tag.is_empty() {
            continue;
        }
        let resolved: Option<String> = db
            .query_row(
                "SELECT tag FROM tag_aliases WHERE alias = ?1",
                [&tag],
                |row| row.get(0),
            )
            .optional()
            .map_err(store_error)?;
        let tag = resolved.unwrap_or(tag);
        if !out.contains(&tag) {
            out.push(tag);
        }
    }
    Ok(out)
}

/// An FTS5 query matching any of the terms. The trigram index needs three
/// characters, so shorter terms are dropped; `None` when none remain.
pub(crate) fn fts_query(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split_whitespace()
        .filter(|term| term.chars().count() >= 3)
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

/// Rows read before an interrupt are kept and flagged as truncated.
pub(crate) fn collect<T>(
    rows: rusqlite::Result<
        rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row) -> rusqlite::Result<T>>,
    >,
) -> Result<Found<T>, ErrorInfo> {
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) if interrupted(&error) => {
            return Ok(Found {
                items: Vec::new(),
                truncated: true,
            })
        }
        Err(error) => return Err(store_error(error)),
    };
    let mut items = Vec::new();
    for row in rows {
        match row {
            Ok(item) => items.push(item),
            Err(error) if interrupted(&error) => {
                return Ok(Found {
                    items,
                    truncated: true,
                })
            }
            Err(error) => return Err(store_error(error)),
        }
    }
    Ok(Found {
        items,
        truncated: false,
    })
}

fn interrupted(error: &rusqlite::Error) -> bool {
    error.sqlite_error_code() == Some(ErrorCode::OperationInterrupted)
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ErrorInfo> + Send + 'static,
) -> Result<T, ErrorInfo> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ErrorInfo::new("store", format!("the store task failed: {error}")))?
}

pub(crate) fn now() -> i64 {
    crate::oauth::now_ms() as i64
}

pub(crate) fn store_error(error: rusqlite::Error) -> ErrorInfo {
    ErrorInfo::new("store", error.to_string())
}

fn json_error(error: serde_json::Error) -> ErrorInfo {
    ErrorInfo::new("store", format!("stored message is not valid: {error}"))
}

fn poisoned() -> ErrorInfo {
    ErrorInfo::new("store", "a store connection was poisoned by a panic")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{Origin, Speaker, StopReason};
    use crate::ProviderKind;

    fn run<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    async fn store(dir: &tempfile::TempDir) -> Store {
        Store::open(dir.path().join("inocode.db"))
            .await
            .expect("open")
    }

    #[test]
    fn thread_ids_are_never_given_twice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inocode.db");
        // A database from before the sequence, whose latest thread is 5.
        {
            let mut db = connect(&path).unwrap();
            for (step, sql) in MIGRATIONS.iter().take(9).enumerate() {
                let tx = db.transaction().unwrap();
                tx.execute_batch(sql).unwrap();
                tx.pragma_update(None, "user_version", step as i64 + 1)
                    .unwrap();
                tx.commit().unwrap();
            }
            for id in [1, 2, 5] {
                db.execute(
                    "INSERT INTO sessions(id, title, created) VALUES (?1, '', 0)",
                    [id],
                )
                .unwrap();
            }
        }
        run(async {
            let store = Store::open(&path).await.unwrap();
            let next = store.start_session(None, "").await.unwrap();
            assert_eq!(next, SessionId(6));
            assert!(store.delete_session(next).await.unwrap());
            let after = store.start_session(None, "").await.unwrap();
            assert_eq!(after, SessionId(7), "the deleted latest id is not reused");
            let fork = store.fork_session(after).await.unwrap();
            assert_eq!(fork, SessionId(8));

            let fresh = tempfile::tempdir().unwrap();
            let new = Store::open(fresh.path().join("inocode.db")).await.unwrap();
            let one = new.start_session(None, "").await.unwrap();
            new.delete_session(one).await.unwrap();
            assert_eq!(new.start_session(None, "").await.unwrap(), SessionId(2));
        });
    }

    #[test]
    fn what_a_turn_left_waiting_and_the_questions_of_mcp_servers_outlive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        run(async {
            let store = store(&dir).await;
            let thread = store.start_session(None, "").await.unwrap();
            store
                .set_waiting(thread, "[1]".into(), "[2]".into())
                .await
                .unwrap();
            let (id, _answer) = store.wait_elicitation(thread);
            store.describe_elicitation(id, serde_json::json!({"message": "Which?"}));
            // The saving runs beside the caller.
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(store);

            let store = Store::open(dir.path().join("inocode.db")).await.unwrap();
            assert_eq!(
                store.waiting(thread).await.unwrap(),
                Some(("[1]".to_string(), "[2]".to_string()))
            );
            assert!(store.expired_elicitations(thread).await.unwrap().is_empty());
            store.expire_elicitations().await.unwrap();
            let expired = store.expired_elicitations(thread).await.unwrap();
            assert_eq!(expired, [(id, serde_json::json!({"message": "Which?"}))]);
            let (next, _) = store.wait_elicitation(thread);
            assert!(next > id, "a new question never takes an expired one's id");
            assert!(store.drop_expired_elicitation(thread, id).await.unwrap());
            assert!(store.expired_elicitations(thread).await.unwrap().is_empty());

            assert!(store.delete_session(thread).await.unwrap());
            assert_eq!(store.waiting(thread).await.unwrap(), None);
        });
    }

    fn user(text: &str) -> Message {
        Message {
            id: text.to_string(),
            body: Body::User {
                from: Speaker::Person,
                parts: vec![text.to_string()],
            },
        }
    }

    fn note(project: Option<ProjectId>, kind: NoteKind, title: &str, source: Source) -> NewNote {
        NewNote {
            project,
            kind,
            title: title.to_string(),
            summary: String::new(),
            keywords: String::new(),
            condition: String::new(),
            body: String::new(),
            source,
            tags: Vec::new(),
        }
    }

    fn ids(found: Found<NoteHit>) -> Vec<NoteId> {
        found.items.iter().map(|hit| hit.id).collect()
    }

    const BUDGET: Duration = Duration::from_secs(5);

    #[test]
    fn history_pages_back_and_keeps_large_results_apart() {
        let dir = tempfile::tempdir().expect("dir");
        run(async {
            let store = store(&dir).await;
            let project = store
                .project("github.com/mouseos/inocode")
                .await
                .expect("project");
            assert_eq!(
                store
                    .project("github.com/mouseos/inocode")
                    .await
                    .expect("again"),
                project
            );
            let session = store
                .start_session(Some(project), "t")
                .await
                .expect("session");
            let log = "検索ログ line\n".repeat(2000);
            let messages = vec![
                user("日本語の検索を試す"),
                Message {
                    id: "a".to_string(),
                    body: Body::Assistant {
                        origin: Origin {
                            format: ProviderKind::Anthropic,
                            model: "m".to_string(),
                        },
                        stop: StopReason::ToolUse,
                        parts: vec![AssistantPart::ToolCall {
                            call_id: "c1".to_string(),
                            name: "read_log".to_string(),
                            arguments: "{}".to_string(),
                            replay: None,
                        }],
                    },
                },
                Message {
                    id: "r".to_string(),
                    body: Body::ToolResult {
                        call_id: "c1".to_string(),
                        name: "read_log".to_string(),
                        content: log,
                        is_error: false,
                        images: Vec::new(),
                    },
                },
                user("続きをお願いします"),
            ];
            let positions = store
                .append(session, messages.clone())
                .await
                .expect("append");
            assert_eq!(positions, [1, 2, 3, 4]);

            let last = store.page(session, None, 2).await.expect("page");
            assert_eq!(last.iter().map(|m| m.seq).collect::<Vec<_>>(), [3, 4]);
            let earlier = store.page(session, Some(3), 10).await.expect("page");
            assert_eq!(earlier.iter().map(|m| m.seq).collect::<Vec<_>>(), [1, 2]);
            let all = store.since(session, 0, 10).await.expect("since");
            let restored: Vec<Message> = all.into_iter().map(|m| m.message).collect();
            assert_eq!(restored, messages, "the large result comes back whole");

            let stored_body: String = connect(&dir.path().join("inocode.db"))
                .expect("db")
                .query_row("SELECT body FROM messages WHERE seq = 3", [], |row| {
                    row.get(0)
                })
                .expect("row");
            assert!(stored_body.len() < 1024, "the row keeps only a reference");

            let search = |text: &str, project| HistoryQuery {
                text: text.to_string(),
                project,
                since: None,
                until: None,
                limit: 5,
                budget: BUDGET,
            };
            let found = store
                .search_history(search("日本語", Some(project)))
                .await
                .expect("search");
            assert_eq!(found.items.len(), 1);
            assert_eq!(found.items[0].seq, 1);
            assert!(found.items[0].snippet.contains("[日本語]"));
            let other = store.project("elsewhere").await.expect("project");
            let none = store
                .search_history(search("検索ログ", Some(other)))
                .await
                .expect("search");
            assert!(none.items.is_empty(), "history stays in its project");
        });
    }

    #[test]
    fn lessons_respect_scope_status_and_versions() {
        let dir = tempfile::tempdir().expect("dir");
        run(async {
            let store = store(&dir).await;
            let a = store.project("a").await.expect("a");
            let b = store.project("b").await.expect("b");
            let mut proposed = note(
                Some(a),
                NoteKind::Lesson,
                "rebuild the index before search",
                Source::Model,
            );
            proposed.condition = "when the search returns stale results".to_string();
            let proposed = store.add_note(proposed).await.expect("note");
            let mut told = note(
                Some(a),
                NoteKind::Lesson,
                "never push without running clippy",
                Source::Person,
            );
            told.condition = "before pushing rust changes".to_string();
            let told = store.add_note(told).await.expect("note");
            let mut shared = note(
                None,
                NoteKind::Procedure,
                "run rust tests in release",
                Source::Person,
            );
            shared.condition = "when testing rust".to_string();
            let shared = store.add_note(shared).await.expect("note");

            let for_a = ids(store
                .lessons_for(Some(a), "pushing rust stale search", 10, BUDGET)
                .await
                .expect("lessons"));
            assert!(for_a.contains(&told) && for_a.contains(&shared));
            assert!(
                !for_a.contains(&proposed),
                "a proposed lesson waits as a candidate"
            );
            let for_b = ids(store
                .lessons_for(Some(b), "pushing rust", 10, BUDGET)
                .await
                .expect("lessons"));
            assert_eq!(for_b, [shared], "project lessons stay in their project");
            let used = store.note(told).await.expect("note").expect("exists");
            assert!(used.last_used.is_some());

            store.set_status(proposed, "active").await.expect("checked");
            assert!(
                store.set_status(proposed, "verified").await.is_err(),
                "verified is a claim status"
            );
            let for_a = ids(store
                .lessons_for(Some(a), "stale search", 10, BUDGET)
                .await
                .expect("lessons"));
            assert_eq!(for_a, [proposed]);

            let replacement = store
                .add_note(note(
                    Some(a),
                    NoteKind::Lesson,
                    "rebuild the index after every import",
                    Source::Person,
                ))
                .await
                .expect("note");
            store
                .supersede(proposed, replacement)
                .await
                .expect("supersede");
            let found = ids(store
                .lessons_for(Some(a), "rebuild index", 10, BUDGET)
                .await
                .expect("lessons"));
            assert_eq!(
                found,
                [replacement],
                "the old version is kept but not offered"
            );
            let old = store.note(proposed).await.expect("note").expect("kept");
            assert_eq!(old.superseded_by, Some(replacement));

            store.vote(told, false).await.expect("vote");
            assert_eq!(
                store.note(told).await.expect("note").expect("exists").down,
                1
            );
            store.share(told).await.expect("share");
            let for_b = ids(store
                .lessons_for(Some(b), "pushing rust", 10, BUDGET)
                .await
                .expect("lessons"));
            assert!(
                for_b.contains(&told),
                "a shared lesson reaches every project"
            );
        });
    }

    #[test]
    fn tags_normalize_and_resolve_aliases() {
        let dir = tempfile::tempdir().expect("dir");
        run(async {
            let store = store(&dir).await;
            store.alias_tag("#RustLang", "rust").await.expect("alias");
            let mut first = note(None, NoteKind::Knowledge, "first", Source::Person);
            first.tags = vec![
                "#Rust".to_string(),
                " rustlang ".to_string(),
                "Error Handling".to_string(),
            ];
            let id = store.add_note(first).await.expect("note");
            assert_eq!(
                store.note(id).await.expect("note").expect("exists").tags,
                ["rust", "error-handling"]
            );
            store
                .tag(id, vec!["SQLite".to_string()])
                .await
                .expect("tag");
            let found = store
                .search_notes(NoteQuery {
                    text: "sqlite".to_string(),
                    project: None,
                    include_shared: true,
                    kinds: Vec::new(),
                    usable_only: false,
                    since: None,
                    until: None,
                    limit: 5,
                    budget: BUDGET,
                })
                .await
                .expect("search");
            assert_eq!(ids(found), [id], "added tags are searchable");
        });
    }

    #[test]
    fn association_follows_links_then_shared_tags() {
        let dir = tempfile::tempdir().expect("dir");
        run(async {
            let store = store(&dir).await;
            let p = store.project("p").await.expect("p");
            let mut added = Vec::new();
            for (title, tags) in [
                ("seed", vec![]),
                ("linked", vec!["sqlite"]),
                ("via tag", vec!["sqlite"]),
                ("unrelated", vec!["other"]),
                ("retired", vec!["sqlite"]),
            ] {
                let mut new = note(Some(p), NoteKind::Knowledge, title, Source::Person);
                new.tags = tags.into_iter().map(str::to_string).collect();
                added.push(store.add_note(new).await.expect("note"));
            }
            let [seed, linked, via_tag, unrelated, retired] = added[..] else {
                unreachable!()
            };
            store.set_status(retired, "retired").await.expect("retire");
            store.link(seed, linked, "related").await.expect("link");

            let found = store
                .associate(vec![seed], Some(p), 10, BUDGET)
                .await
                .expect("associate");
            assert!(!found.truncated);
            let found = ids(found);
            assert_eq!(found, [linked, via_tag], "one hop outranks two");
            assert!(!found.contains(&unrelated) && !found.contains(&retired));

            let other = store.project("q").await.expect("q");
            let elsewhere = store
                .associate(vec![seed], Some(other), 10, BUDGET)
                .await
                .expect("associate");
            assert!(
                elsewhere.items.is_empty(),
                "another project's notes are out of reach"
            );

            let late = store
                .associate(vec![seed], Some(p), 10, Duration::ZERO)
                .await
                .expect("associate");
            assert!(late.truncated, "an exhausted budget says so");
        });
    }

    #[test]
    fn short_terms_are_dropped_from_queries() {
        assert_eq!(fts_query("ab 日本 rust"), Some("\"rust\"".to_string()));
        assert_eq!(
            fts_query("a \"quoted\""),
            Some("\"\"\"quoted\"\"\"".to_string())
        );
        assert_eq!(fts_query("ab"), None);
    }
}

#[cfg(test)]
mod measure {
    use super::*;
    use crate::conversation::Speaker;

    /// Release-mode timings at a realistic size. They print numbers instead
    /// of asserting; run with
    /// `cargo test --release -p inocode-core store::measure -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement"]
    fn measure_store_at_scale() {
        let dir = tempfile::tempdir().expect("dir");
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let store = Store::open(dir.path().join("m.db")).await.expect("open");
                let project = store.project("p").await.expect("project");
                let session = store.start_session(Some(project), "m").await.expect("session");
                let started = Instant::now();
                for turn in 0..1000 {
                    let batch = (0..50)
                        .map(|i| Message {
                            id: format!("{turn}-{i}"),
                            body: Body::User {
                                from: Speaker::Person,
                                parts: vec![format!(
                                    "turn {turn} message {i} 会話の履歴 sample text compile error E{:04}",
                                    (turn * 50 + i) % 9973
                                )],
                            },
                        })
                        .collect();
                    store.append(session, batch).await.expect("append");
                }
                println!("append 50000 messages in 1000 turns: {:?}", started.elapsed());

                let started = Instant::now();
                let found = store
                    .search_history(HistoryQuery {
                        text: "E0042".to_string(),
                        project: Some(project),
                        since: None,
                        until: None,
                        limit: 20,
                        budget: Duration::from_secs(5),
                    })
                    .await
                    .expect("search");
                println!(
                    "history search over 50000: {:?} ({} hits, truncated {})",
                    started.elapsed(),
                    found.items.len(),
                    found.truncated
                );
                let started = Instant::now();
                let page = store.page(session, None, 50).await.expect("page");
                println!("latest page of 50: {:?} ({})", started.elapsed(), page.len());

                let started = Instant::now();
                let mut notes = Vec::new();
                for i in 0..5000 {
                    let id = store
                        .add_note(NewNote {
                            project: Some(project),
                            kind: NoteKind::Knowledge,
                            title: format!("note {i} about module {}", i % 97),
                            summary: String::new(),
                            keywords: String::new(),
                            condition: String::new(),
                            body: String::new(),
                            source: Source::Person,
                            tags: vec![format!("module-{}", i % 97), "hub".to_string()],
                        })
                        .await
                        .expect("note");
                    if i > 0 {
                        store.link(id, notes[(i * 7) % i], "related").await.expect("link");
                    }
                    notes.push(id);
                }
                println!("add 5000 notes with tags and links: {:?}", started.elapsed());

                let started = Instant::now();
                let found = store
                    .associate(vec![notes[10], notes[20]], Some(project), 10, Duration::from_secs(5))
                    .await
                    .expect("associate");
                println!(
                    "associate over 5000 notes (one hub tag on all): {:?} ({} hits, truncated {})",
                    started.elapsed(),
                    found.items.len(),
                    found.truncated
                );
            });
    }
}

#[cfg(test)]
mod migration {
    use super::*;

    #[test]
    fn a_version_one_database_is_upgraded_in_place() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("old.db");
        {
            let connection = connect(&path).expect("db");
            connection.execute_batch(MIGRATIONS[0]).expect("v1");
            connection
                .pragma_update(None, "user_version", 1)
                .expect("version");
            connection
                .execute("INSERT INTO projects(key, created) VALUES ('kept', 1)", [])
                .expect("row");
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let store = Store::open(&path).await.expect("open");
                assert_eq!(store.project("kept").await.expect("project"), ProjectId(1));
            });
        let connection = connect(&path).expect("db");
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
        connection
            .query_row("SELECT COUNT(*) FROM note_events", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("the version 2 tables exist");
        connection
            .query_row("SELECT COUNT(*) FROM thread_settings", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("the version 9 table exists");
        connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM pool_affinity) + (SELECT COUNT(*) FROM pool_cooldowns)",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("the version 11 tables exist");
    }

    #[test]
    fn a_thread_keeps_its_settings() {
        let dir = tempfile::tempdir().expect("dir");
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let store = Store::open(dir.path().join("t.db")).await.expect("open");
                let session = store.start_session(None, "").await.expect("session");
                assert_eq!(store.thread_settings(session).await.expect("read"), None);
                store
                    .set_thread_settings(session, "{\"model\":\"a\"}".into())
                    .await
                    .expect("set");
                store
                    .set_thread_settings(session, "{\"model\":\"b\"}".into())
                    .await
                    .expect("replace");
                assert_eq!(
                    store
                        .thread_settings(session)
                        .await
                        .expect("read")
                        .as_deref(),
                    Some("{\"model\":\"b\"}")
                );
            });
    }
}
