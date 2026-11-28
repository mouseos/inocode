//! Groups: named boxes, nested to any depth, that hold projects and the
//! threads without a project (docs/evidence/project-groups.md).

use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::store::{now, store_error, SessionId, Store};
use crate::ErrorInfo;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Group {
    pub group_id: i64,
    pub name: String,
    pub parent_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GroupedProject {
    /// The project's key: its folder or repository.
    pub project: String,
    pub group_id: Option<i64>,
}

fn unknown_group(id: i64) -> ErrorInfo {
    ErrorInfo::new("unknown_group", format!("no group has the id {id}"))
}

fn exists(db: &rusqlite::Connection, id: i64) -> Result<bool, ErrorInfo> {
    db.query_row("SELECT 1 FROM groups WHERE id = ?1", [id], |_| Ok(()))
        .optional()
        .map(|found| found.is_some())
        .map_err(store_error)
}

fn check_name(name: &str) -> Result<String, ErrorInfo> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ErrorInfo::new("invalid_params", "a group needs a name"));
    }
    Ok(name.to_string())
}

impl Store {
    /// Every group and every project, with where each is; the screens
    /// build the tree.
    pub async fn groups(&self) -> Result<(Vec<Group>, Vec<GroupedProject>), ErrorInfo> {
        self.read(std::time::Duration::from_secs(5), |db| {
            let mut statement = db
                .prepare("SELECT id, name, parent_id FROM groups ORDER BY name, id")
                .map_err(store_error)?;
            let groups = statement
                .query_map([], |row| {
                    Ok(Group {
                        group_id: row.get(0)?,
                        name: row.get(1)?,
                        parent_id: row.get(2)?,
                    })
                })
                .map_err(store_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(store_error)?;
            let mut statement = db
                .prepare("SELECT key, group_id FROM projects ORDER BY key")
                .map_err(store_error)?;
            let projects = statement
                .query_map([], |row| {
                    Ok(GroupedProject {
                        project: row.get(0)?,
                        group_id: row.get(1)?,
                    })
                })
                .map_err(store_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(store_error)?;
            Ok((groups, projects))
        })
        .await
    }

    pub async fn create_group(&self, name: &str, parent: Option<i64>) -> Result<i64, ErrorInfo> {
        let name = check_name(name)?;
        self.write(move |db| {
            if let Some(parent) = parent {
                if !exists(db, parent)? {
                    return Err(unknown_group(parent));
                }
            }
            db.execute(
                "INSERT INTO groups(name, parent_id, created) VALUES (?1, ?2, ?3)",
                params![name, parent, now()],
            )
            .map_err(store_error)?;
            Ok(db.last_insert_rowid())
        })
        .await
    }

    pub async fn rename_group(&self, id: i64, name: &str) -> Result<(), ErrorInfo> {
        let name = check_name(name)?;
        self.write(move |db| {
            let changed = db
                .execute(
                    "UPDATE groups SET name = ?1 WHERE id = ?2",
                    params![name, id],
                )
                .map_err(store_error)?;
            match changed {
                0 => Err(unknown_group(id)),
                _ => Ok(()),
            }
        })
        .await
    }

    /// Puts group `id` in `parent` (at the top when `None`); a group cannot
    /// go into itself or a group under it.
    pub async fn move_group(&self, id: i64, parent: Option<i64>) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            if !exists(db, id)? {
                return Err(unknown_group(id));
            }
            let mut above = parent;
            while let Some(group) = above {
                if group == id {
                    return Err(ErrorInfo::new(
                        "invalid_params",
                        "a group cannot go into itself or a group under it",
                    ));
                }
                above = db
                    .query_row(
                        "SELECT parent_id FROM groups WHERE id = ?1",
                        [group],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(store_error)?
                    .ok_or_else(|| unknown_group(group))?;
            }
            db.execute(
                "UPDATE groups SET parent_id = ?1 WHERE id = ?2",
                params![parent, id],
            )
            .map(drop)
            .map_err(store_error)
        })
        .await
    }

    /// Deletes an empty group: one without groups, projects or threads in it.
    pub async fn delete_group(&self, id: i64) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            if !exists(db, id)? {
                return Err(unknown_group(id));
            }
            let count = |sql: &str| -> Result<i64, ErrorInfo> {
                db.query_row(sql, [id], |row| row.get(0))
                    .map_err(store_error)
            };
            let held = count("SELECT COUNT(*) FROM groups WHERE parent_id = ?1")?
                + count("SELECT COUNT(*) FROM projects WHERE group_id = ?1")?
                + count(
                    "SELECT COUNT(*) FROM sessions WHERE group_id = ?1 AND project_id IS NULL",
                )?;
            if held > 0 {
                return Err(ErrorInfo::new(
                    "group_not_empty",
                    "the group holds groups, projects or threads; move or delete them first",
                ));
            }
            // A thread with a project keeps no group of its own.
            db.execute(
                "UPDATE sessions SET group_id = NULL WHERE group_id = ?1",
                [id],
            )
            .map_err(store_error)?;
            db.execute("DELETE FROM groups WHERE id = ?1", [id])
                .map(drop)
                .map_err(store_error)
        })
        .await
    }

    /// Puts the project of `key` in `group` (at the top when `None`); the
    /// project is made when it is not yet known.
    pub async fn group_project(&self, key: &str, group: Option<i64>) -> Result<(), ErrorInfo> {
        let project = self.project(key).await?;
        self.write(move |db| {
            if let Some(group) = group {
                if !exists(db, group)? {
                    return Err(unknown_group(group));
                }
            }
            db.execute(
                "UPDATE projects SET group_id = ?1 WHERE id = ?2",
                params![group, project.0],
            )
            .map(drop)
            .map_err(store_error)
        })
        .await
    }

    /// Puts a thread without a project in `group` (at the top when `None`).
    pub async fn group_thread(
        &self,
        session: SessionId,
        group: Option<i64>,
    ) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            let project: Option<Option<i64>> = db
                .query_row(
                    "SELECT project_id FROM sessions WHERE id = ?1",
                    [session.0],
                    |row| row.get(0),
                )
                .optional()
                .map_err(store_error)?;
            match project {
                None => return Err(ErrorInfo::new("unknown_thread", "no thread has this id")),
                Some(Some(_)) => return Err(ErrorInfo::new(
                    "invalid_params",
                    "this thread is in a project; it is where its project is, so move the project",
                )),
                Some(None) => {}
            }
            if let Some(group) = group {
                if !exists(db, group)? {
                    return Err(unknown_group(group));
                }
            }
            db.execute(
                "UPDATE sessions SET group_id = ?1 WHERE id = ?2",
                params![group, session.0],
            )
            .map(drop)
            .map_err(store_error)
        })
        .await
    }
}
