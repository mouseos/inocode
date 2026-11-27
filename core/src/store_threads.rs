//! Thread and note queries the slash commands need: listing, renaming,
//! forking and deleting threads, their logged usage, and listing notes.

use std::time::Duration;

use rusqlite::{params, OptionalExtension};

use crate::store::{
    load_note, new_session_id, now, store_error, Note, NoteId, NoteKind, ProjectId, SessionId,
    Store,
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
        let given = title.to_string();
        let renamed = self
            .write(move |db| {
                db.execute(
                    "UPDATE sessions SET title = ?1 WHERE id = ?2",
                    params![given, session.0],
                )
                .map(|changed| changed == 1)
                .map_err(store_error)
            })
            .await?;
        if renamed {
            self.titled(session, title);
        }
        Ok(renamed)
    }

    /// Gives `session` `title` only if it still has none; `false` when it
    /// has one, so a title given in the meantime is kept.
    pub(crate) async fn title_untitled(
        &self,
        session: SessionId,
        title: &str,
    ) -> Result<bool, ErrorInfo> {
        let given = title.to_string();
        let titled = self
            .write(move |db| {
                db.execute(
                    "UPDATE sessions SET title = ?1 WHERE id = ?2 AND title = ''",
                    params![given, session.0],
                )
                .map(|changed| changed == 1)
                .map_err(store_error)
            })
            .await?;
        if titled {
            self.titled(session, title);
        }
        Ok(titled)
    }

    /// The title of `session`; empty when it has none or there is no such
    /// thread.
    pub async fn session_title(&self, session: SessionId) -> Result<String, ErrorInfo> {
        self.read(Duration::from_secs(5), move |db| {
            db.query_row(
                "SELECT title FROM sessions WHERE id = ?1",
                [session.0],
                |row| row.get(0),
            )
            .optional()
            .map(Option::unwrap_or_default)
            .map_err(store_error)
        })
        .await
    }

    /// A new thread in `session`'s project holding a copy of its messages,
    /// with their times, and of its plan, standing instructions, choice of
    /// model and the calls allowed in it. Its goal is not copied.
    pub async fn fork_session(&self, session: SessionId) -> Result<SessionId, ErrorInfo> {
        self.fork_session_through(session, i64::MAX).await
    }

    /// [`Self::fork_session`] with only the messages up to the one numbered
    /// `through`, inclusive.
    pub async fn fork_session_through(
        &self,
        session: SessionId,
        through: i64,
    ) -> Result<SessionId, ErrorInfo> {
        self.write(move |db| {
            let tx = db.transaction().map_err(store_error)?;
            let time = now();
            let fork = new_session_id(&tx)?;
            let copied = tx
                .execute(
                    "INSERT INTO sessions(id, project_id, title, created)
                     SELECT ?3, project_id, title, ?2 FROM sessions WHERE id = ?1",
                    params![session.0, time, fork],
                )
                .map_err(store_error)?;
            if copied != 1 {
                return Err(ErrorInfo::new("unknown_thread", "no thread has this id"));
            }
            tx.execute(
                "INSERT INTO messages(session_id, seq, role, body, text, created)
                 SELECT ?2, seq, role, body, text, created FROM messages
                 WHERE session_id = ?1 AND seq <= ?3 ORDER BY seq",
                params![session.0, fork, through],
            )
            .map_err(store_error)?;
            tx.execute(
                "INSERT INTO snapshots(session_id, seq, tree, created)
                 SELECT ?2, seq, tree, created FROM snapshots
                 WHERE session_id = ?1 AND seq <= ?3",
                params![session.0, fork, through],
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
            tx.execute(
                "INSERT INTO allowances(session_id, tool, arguments, created)
                 SELECT ?2, tool, arguments, ?3 FROM allowances WHERE session_id = ?1",
                params![session.0, fork, time],
            )
            .map_err(store_error)?;
            tx.execute(
                "INSERT INTO thread_lineage(session_id, parent) VALUES (?1, ?2)",
                params![fork, session.0],
            )
            .map_err(store_error)?;
            tx.commit().map_err(store_error)?;
            Ok(SessionId(fork))
        })
        .await
    }

    /// Deletes the messages of `session` from the one numbered `from` on,
    /// the questions its latest turn left, and the standing
    /// instructions quoted from the messages deleted; returns how many
    /// messages went.
    pub async fn rewind_session(&self, session: SessionId, from: i64) -> Result<usize, ErrorInfo> {
        self.write(move |db| {
            let tx = db.transaction().map_err(store_error)?;
            tx.execute(
                "INSERT INTO messages_fts(messages_fts, rowid, text)
                 SELECT 'delete', id, text FROM messages WHERE session_id = ?1 AND seq >= ?2",
                params![session.0, from],
            )
            .map_err(store_error)?;
            let removed = tx
                .execute(
                    "DELETE FROM messages WHERE session_id = ?1 AND seq >= ?2",
                    params![session.0, from],
                )
                .map_err(store_error)?;
            tx.execute("DELETE FROM waiting WHERE session_id = ?1", [session.0])
                .map_err(store_error)?;
            tx.execute(
                "DELETE FROM snapshots WHERE session_id = ?1 AND seq >= ?2",
                params![session.0, from],
            )
            .map_err(store_error)?;
            let promises: Option<(String, i64)> = tx
                .query_row(
                    "SELECT items, seq FROM promises WHERE session_id = ?1",
                    [session.0],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(store_error)?;
            if let Some((items, seq)) = promises {
                let items: Vec<serde_json::Value> = serde_json::from_str(&items)
                    .map_err(|error| ErrorInfo::new("store", error.to_string()))?;
                let kept: Vec<&serde_json::Value> = items
                    .iter()
                    .filter(|item| {
                        item["message"]
                            .as_i64()
                            .is_some_and(|message| message < from)
                    })
                    .collect();
                let kept = serde_json::to_string(&kept)
                    .map_err(|error| ErrorInfo::new("store", error.to_string()))?;
                tx.execute(
                    "UPDATE promises SET items = ?2, seq = ?3, updated = ?4 WHERE session_id = ?1",
                    params![session.0, kept, seq.min(from.saturating_sub(1)), now()],
                )
                .map_err(store_error)?;
            }
            tx.commit().map_err(store_error)?;
            Ok(removed)
        })
        .await
    }

    /// Deletes `session` with its messages, plan, goal, standing
    /// instructions, choice of model, questions and the calls allowed in
    /// it; `false`
    /// when there is no such thread. The log of its requests to models, the
    /// records of its tasks and the events of notes are kept, no longer
    /// linked to the thread, and
    /// stored payloads, which other threads may share, are kept. The files
    /// of the thread are [`Self::thread_files`].
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
            for table in [
                "messages",
                "plans",
                "goals",
                "promises",
                "thread_settings",
                "waiting",
                "elicitations",
                "thread_lineage",
                "delegations",
                "snapshots",
                "allowances",
            ] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE session_id = ?1"),
                    [session.0],
                )
                .map_err(store_error)?;
            }
            for table in ["requests", "note_events", "tasks"] {
                tx.execute(
                    &format!("UPDATE {table} SET session_id = NULL WHERE session_id = ?1"),
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

    /// The threads whose chosen provider is `provider`.
    pub(crate) async fn threads_choosing(&self, provider: &str) -> Result<Vec<i64>, ErrorInfo> {
        let provider = provider.to_string();
        self.read(Duration::from_secs(30), move |db| {
            let mut statement = db
                .prepare_cached(
                    "SELECT session_id FROM thread_settings
                     WHERE json_extract(settings, '$.provider') = ?1 ORDER BY session_id",
                )
                .map_err(store_error)?;
            let rows = statement
                .query_map([provider], |row| row.get(0))
                .map_err(store_error)?;
            rows.collect::<Result<Vec<i64>, _>>().map_err(store_error)
        })
        .await
    }

    /// Removes the files kept for `session` beside the database: its command
    /// logs, pictures, sound, fetched pages and saved messages. What could
    /// not be removed is returned, each with why.
    pub(crate) async fn remove_thread_files(&self, session: SessionId) -> Vec<String> {
        let files = self.files_folder();
        tokio::task::spawn_blocking(move || {
            let mut failed = Vec::new();
            let folder = format!("thread-{}", session.0);
            for kind in ["logs", "images", "audio", "fetch"] {
                let path = files.join(kind).join(&folder);
                match std::fs::remove_dir_all(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => failed.push(format!("{}: {error}", path.display())),
                }
            }
            let prefix = format!("{}-", session.0);
            let messages = files.join("messages");
            for entry in std::fs::read_dir(&messages).into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(&prefix) && name.ends_with(".txt") {
                    if let Err(error) = std::fs::remove_file(entry.path()) {
                        failed.push(format!("{}: {error}", entry.path().display()));
                    }
                }
            }
            failed
        })
        .await
        .unwrap_or_else(|error| vec![error.to_string()])
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

    /// Links the thread `session`, made for a delegated task, to the
    /// thread `parent` that delegated it to `member`.
    pub(crate) async fn record_delegation(
        &self,
        session: SessionId,
        parent: SessionId,
        member: &str,
        task: &str,
    ) -> Result<(), ErrorInfo> {
        let (member, task) = (member.to_string(), task.to_string());
        self.write(move |db| {
            db.execute(
                "INSERT INTO delegations(session_id, parent, member, task, created)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session.0, parent.0, member, task, now()],
            )
            .map(drop)
            .map_err(store_error)
        })
        .await
    }

    /// The threads `parent` delegated tasks to, the latest first: the
    /// thread, the member, the task and when.
    pub async fn delegations(
        &self,
        parent: SessionId,
    ) -> Result<Vec<(SessionId, String, String, i64)>, ErrorInfo> {
        self.read(Duration::from_secs(5), move |db| {
            let mut statement = db
                .prepare_cached(
                    "SELECT session_id, member, task, created FROM delegations
                     WHERE parent = ?1 ORDER BY created DESC, session_id DESC",
                )
                .map_err(store_error)?;
            let rows = statement
                .query_map([parent.0], |row| {
                    Ok((
                        SessionId(row.get(0)?),
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                    ))
                })
                .map_err(store_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(store_error)
        })
        .await
    }
}
