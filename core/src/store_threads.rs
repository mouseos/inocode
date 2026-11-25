//! Thread and note queries the slash commands need: listing, renaming,
//! forking and deleting threads, their logged usage, and listing notes.

use std::time::Duration;

use rusqlite::{params, OptionalExtension};

use crate::store::{
    load_note, now, store_error, Note, NoteId, NoteKind, ProjectId, SessionId, Store,
};
use crate::ErrorInfo;

/// A thread as it is listed for choosing one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    pub id: SessionId,
    /// The project's key: its folder or repository.
    pub project: Option<String>,
    pub title: String,
    pub created: i64,
    /// When its latest message was stored; its creation when it has none.
    pub last_activity: i64,
    pub messages: i64,
}

/// The requests to models a thread made, added up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionUsage {
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cached: u64,
}

const SUMMARY_SELECT: &str = "SELECT s.id, p.key, s.title, s.created,
        COALESCE((SELECT MAX(m.created) FROM messages m WHERE m.session_id = s.id), s.created)
            AS last_activity,
        (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id)
     FROM sessions s LEFT JOIN projects p ON p.id = s.project_id";

fn summary_row(row: &rusqlite::Row) -> rusqlite::Result<ThreadSummary> {
    Ok(ThreadSummary {
        id: SessionId(row.get(0)?),
        project: row.get(1)?,
        title: row.get(2)?,
        created: row.get(3)?,
        last_activity: row.get(4)?,
        messages: row.get(5)?,
    })
}

impl Store {
    /// Threads, the most recently active first; only those of the project
    /// with the key `project` when given.
    pub async fn sessions(
        &self,
        project: Option<String>,
        limit: usize,
    ) -> Result<Vec<ThreadSummary>, ErrorInfo> {
        self.read(Duration::from_secs(30), move |db| {
            let mut statement = db
                .prepare_cached(&format!(
                    "{SUMMARY_SELECT} WHERE ?1 IS NULL OR p.key = ?1
                     ORDER BY last_activity DESC, s.id DESC LIMIT ?2"
                ))
                .map_err(store_error)?;
            let rows = statement
                .query_map(params![project, limit as i64], summary_row)
                .map_err(store_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(store_error)
        })
        .await
    }

    pub async fn session_summary(
        &self,
        session: SessionId,
    ) -> Result<Option<ThreadSummary>, ErrorInfo> {
        self.read(Duration::from_secs(5), move |db| {
            db.query_row(
                &format!("{SUMMARY_SELECT} WHERE s.id = ?1"),
                [session.0],
                summary_row,
            )
            .optional()
            .map_err(store_error)
        })
        .await
    }

    /// Gives `session` a new title; `false` when there is no such thread.
    pub async fn rename_session(&self, session: SessionId, title: &str) -> Result<bool, ErrorInfo> {
        let title = title.to_string();
        self.write(move |db| {
            db.execute(
                "UPDATE sessions SET title = ?1 WHERE id = ?2",
                params![title, session.0],
            )
            .map(|changed| changed == 1)
            .map_err(store_error)
        })
        .await
    }

    /// A new thread in `session`'s project holding a copy of its messages,
    /// with their times, and of its plan, standing instructions and choice
    /// of model. Its goal is not copied.
    pub async fn fork_session(&self, session: SessionId) -> Result<SessionId, ErrorInfo> {
        self.write(move |db| {
            let tx = db.transaction().map_err(store_error)?;
            let time = now();
            let copied = tx
                .execute(
                    "INSERT INTO sessions(project_id, title, created)
                     SELECT project_id, title, ?2 FROM sessions WHERE id = ?1",
                    params![session.0, time],
                )
                .map_err(store_error)?;
            if copied != 1 {
                return Err(ErrorInfo::new("unknown_thread", "no thread has this id"));
            }
            let fork = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO messages(session_id, seq, role, body, text, created)
                 SELECT ?2, seq, role, body, text, created FROM messages
                 WHERE session_id = ?1 ORDER BY seq",
                params![session.0, fork],
            )
            .map_err(store_error)?;
            tx.execute(
                "INSERT INTO plans(session_id, items, updated)
                 SELECT ?2, items, ?3 FROM plans WHERE session_id = ?1",
                params![session.0, fork, time],
            )
            .map_err(store_error)?;
            tx.execute(
                "INSERT INTO promises(session_id, items, seq, updated)
                 SELECT ?2, items, seq, ?3 FROM promises WHERE session_id = ?1",
                params![session.0, fork, time],
            )
            .map_err(store_error)?;
            tx.execute(
                "INSERT INTO thread_settings(session_id, settings, updated)
                 SELECT ?2, settings, ?3 FROM thread_settings WHERE session_id = ?1",
                params![session.0, fork, time],
            )
            .map_err(store_error)?;
            tx.commit().map_err(store_error)?;
            Ok(SessionId(fork))
        })
        .await
    }

    /// Deletes `session` with its messages, plan, goal, standing
    /// instructions and choice of model; `false` when there is no such
    /// thread. The log of its requests to models is kept for the
    /// statistics, and stored payloads, which other threads may share, are
    /// kept.
    pub async fn delete_session(&self, session: SessionId) -> Result<bool, ErrorInfo> {
        self.write(move |db| {
            let tx = db.transaction().map_err(store_error)?;
            // The search index keeps its own copy of each message's text;
            // an external-content index is told which rows go.
            tx.execute(
                "INSERT INTO messages_fts(messages_fts, rowid, text)
                 SELECT 'delete', id, text FROM messages WHERE session_id = ?1",
                [session.0],
            )
            .map_err(store_error)?;
            for table in ["messages", "plans", "goals", "promises", "thread_settings"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE session_id = ?1"),
                    [session.0],
                )
                .map_err(store_error)?;
            }
            let deleted = tx
                .execute("DELETE FROM sessions WHERE id = ?1", [session.0])
                .map_err(store_error)?;
            tx.commit().map_err(store_error)?;
            Ok(deleted == 1)
        })
        .await
    }

    /// The logged requests of `session`, added up.
    pub async fn session_usage(&self, session: SessionId) -> Result<SessionUsage, ErrorInfo> {
        self.read(Duration::from_secs(30), move |db| {
            db.query_row(
                "SELECT COUNT(*), COALESCE(SUM(input), 0), COALESCE(SUM(output), 0),
                        COALESCE(SUM(cached), 0)
                 FROM requests WHERE session_id = ?1",
                [session.0],
                |row| {
                    Ok(SessionUsage {
                        requests: row.get::<_, i64>(0)? as u64,
                        input: row.get::<_, i64>(1)? as u64,
                        output: row.get::<_, i64>(2)? as u64,
                        cached: row.get::<_, i64>(3)? as u64,
                    })
                },
            )
            .map_err(store_error)
        })
        .await
    }

    /// When the first and the latest message of `session` from `from_seq`
    /// on were stored; `None` when there is none.
    pub async fn span(
        &self,
        session: SessionId,
        from_seq: i64,
    ) -> Result<Option<(i64, i64)>, ErrorInfo> {
        self.read(Duration::from_secs(5), move |db| {
            db.query_row(
                "SELECT MIN(created), MAX(created) FROM messages
                 WHERE session_id = ?1 AND seq >= ?2",
                params![session.0, from_seq],
                |row| {
                    Ok(match (row.get(0)?, row.get(1)?) {
                        (Some(first), Some(last)) => Some((first, last)),
                        _ => None,
                    })
                },
            )
            .map_err(store_error)
        })
        .await
    }

    /// Notes of `kinds` in `project` and the shared scope, the latest
    /// first, the retired ones included; superseded notes are left out.
    pub async fn notes_of(
        &self,
        project: Option<ProjectId>,
        kinds: Vec<NoteKind>,
        limit: usize,
    ) -> Result<Vec<Note>, ErrorInfo> {
        self.read(Duration::from_secs(30), move |db| {
            let kinds: Vec<String> = kinds
                .iter()
                .map(|kind| format!("'{}'", kind.as_str()))
                .collect();
            let mut statement = db
                .prepare_cached(&format!(
                    "SELECT id FROM notes
                     WHERE kind IN ({}) AND superseded_by IS NULL
                       AND (project_id IS ?1 OR project_id IS NULL)
                     ORDER BY id DESC LIMIT ?2",
                    kinds.join(", ")
                ))
                .map_err(store_error)?;
            let ids = statement
                .query_map(params![project.map(|p| p.0), limit as i64], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(store_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(store_error)?;
            let mut notes = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(note) = load_note(db, NoteId(id))? {
                    notes.push(note);
                }
            }
            Ok(notes)
        })
        .await
    }

    /// The links of note `id`: the other note, the link's kind, and whether
    /// it goes from `id`.
    pub async fn links_of(&self, id: NoteId) -> Result<Vec<(NoteId, String, bool)>, ErrorInfo> {
        self.read(Duration::from_secs(5), move |db| {
            let mut statement = db
                .prepare_cached(
                    "SELECT dst, kind, 1 FROM links WHERE src = ?1
                     UNION ALL SELECT src, kind, 0 FROM links WHERE dst = ?1",
                )
                .map_err(store_error)?;
            let rows = statement
                .query_map([id.0], |row| {
                    Ok((NoteId(row.get(0)?), row.get(1)?, row.get::<_, i64>(2)? == 1))
                })
                .map_err(store_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(store_error)
        })
        .await
    }
}
