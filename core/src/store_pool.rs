//! What the account pool keeps across restarts (`SCHEMA_V11`): each
//! thread's account of a provider and the quota cooldowns of accounts
//! (docs/evidence/account-pool.md).

use std::time::Duration;

use rusqlite::params;

use crate::store::{store_error, SessionId, Store};
use crate::ErrorInfo;

/// The account a thread stays on for one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolBinding {
    pub thread: SessionId,
    /// The settings' provider id.
    pub provider: String,
    /// The credential store's provider name.
    pub kind: String,
    pub account: String,
    pub bound_ms: u64,
    pub used_ms: u64,
}

/// An account that refused on quota, and until when it is not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolCooldown {
    pub kind: String,
    pub account: String,
    pub until_ms: u64,
    pub avoid_until_ms: u64,
    pub since_ms: u64,
    /// What set `until_ms`: `retry-after`, `reset` or `default`.
    pub source: String,
    pub status: u16,
}

const BUDGET: Duration = Duration::from_secs(5);

impl Store {
    /// The bindings used at or after `since_ms`.
    pub async fn pool_bindings(&self, since_ms: u64) -> Result<Vec<PoolBinding>, ErrorInfo> {
        self.read(BUDGET, move |db| {
            let mut statement = db
                .prepare(
                    "SELECT session_id, provider, kind, account, bound, used FROM pool_affinity
                     WHERE used >= ?1",
                )
                .map_err(store_error)?;
            let rows = statement
                .query_map([since_ms as i64], |row| {
                    Ok(PoolBinding {
                        thread: SessionId(row.get(0)?),
                        provider: row.get(1)?,
                        kind: row.get(2)?,
                        account: row.get(3)?,
                        bound_ms: row.get::<_, i64>(4)? as u64,
                        used_ms: row.get::<_, i64>(5)? as u64,
                    })
                })
                .map_err(store_error)?;
            rows.collect::<Result<_, _>>().map_err(store_error)
        })
        .await
    }

    pub async fn set_pool_binding(&self, binding: PoolBinding) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            db.execute(
                "INSERT INTO pool_affinity(session_id, provider, kind, account, bound, used)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(session_id, provider) DO UPDATE
                 SET kind = ?3, account = ?4, bound = ?5, used = ?6",
                params![
                    binding.thread.0,
                    binding.provider,
                    binding.kind,
                    binding.account,
                    binding.bound_ms as i64,
                    binding.used_ms as i64
                ],
            )
            .map(drop)
            .map_err(store_error)
        })
        .await
    }

    pub async fn delete_pool_binding(
        &self,
        thread: SessionId,
        provider: String,
    ) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            db.execute(
                "DELETE FROM pool_affinity WHERE session_id = ?1 AND provider = ?2",
                params![thread.0, provider],
            )
            .map(drop)
            .map_err(store_error)
        })
        .await
    }

    /// Removes every thread's binding to one account.
    pub async fn delete_pool_bindings_to(
        &self,
        kind: String,
        account: String,
    ) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            db.execute(
                "DELETE FROM pool_affinity WHERE kind = ?1 AND account = ?2",
                params![kind, account],
            )
            .map(drop)
            .map_err(store_error)
        })
        .await
    }

    /// Removes the bindings of one provider, one thread, both, or all.
    pub async fn clear_pool_bindings(
        &self,
        provider: Option<String>,
        thread: Option<SessionId>,
    ) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            db.execute(
                "DELETE FROM pool_affinity
                 WHERE (?1 IS NULL OR provider = ?1) AND (?2 IS NULL OR session_id = ?2)",
                params![provider, thread.map(|thread| thread.0)],
            )
            .map(drop)
            .map_err(store_error)
        })
        .await
    }

    /// The cooldowns or quota avoidances still running at `now_ms`.
    pub async fn pool_cooldowns(&self, now_ms: u64) -> Result<Vec<PoolCooldown>, ErrorInfo> {
        self.read(BUDGET, move |db| {
            let mut statement = db
                .prepare(
                    "SELECT kind, account, until, avoid_until, since, source, status
                     FROM pool_cooldowns WHERE MAX(until, avoid_until) > ?1",
                )
                .map_err(store_error)?;
            let rows = statement
                .query_map([now_ms as i64], |row| {
                    Ok(PoolCooldown {
                        kind: row.get(0)?,
                        account: row.get(1)?,
                        until_ms: row.get::<_, i64>(2)? as u64,
                        avoid_until_ms: row.get::<_, i64>(3)? as u64,
                        since_ms: row.get::<_, i64>(4)? as u64,
                        source: row.get(5)?,
                        status: row.get::<_, i64>(6)? as u16,
                    })
                })
                .map_err(store_error)?;
            rows.collect::<Result<_, _>>().map_err(store_error)
        })
        .await
    }

    pub async fn set_pool_cooldown(&self, cooldown: PoolCooldown) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            db.execute(
                "INSERT INTO pool_cooldowns(kind, account, until, avoid_until, since, source, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(kind, account) DO UPDATE
                 SET until = ?3, avoid_until = ?4, since = ?5, source = ?6, status = ?7",
                params![
                    cooldown.kind,
                    cooldown.account,
                    cooldown.until_ms as i64,
                    cooldown.avoid_until_ms as i64,
                    cooldown.since_ms as i64,
                    cooldown.source,
                    i64::from(cooldown.status)
                ],
            )
            .map(drop)
            .map_err(store_error)
        })
        .await
    }

    pub async fn delete_pool_cooldown(
        &self,
        kind: String,
        account: String,
    ) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            db.execute(
                "DELETE FROM pool_cooldowns WHERE kind = ?1 AND account = ?2",
                params![kind, account],
            )
            .map(drop)
            .map_err(store_error)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_and_cooldowns_are_kept_and_removed() {
        let dir = tempfile::tempdir().expect("dir");
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async {
                let store = Store::open(dir.path().join("p.db")).await.expect("open");
                let binding =
                    |thread: i64, provider: &str, account: &str, used_ms: u64| PoolBinding {
                        thread: SessionId(thread),
                        provider: provider.into(),
                        kind: "claude-oauth".into(),
                        account: account.into(),
                        bound_ms: 1,
                        used_ms,
                    };
                store
                    .set_pool_binding(binding(1, "a", "x", 100))
                    .await
                    .unwrap();
                store
                    .set_pool_binding(binding(2, "a", "y", 100))
                    .await
                    .unwrap();
                store
                    .set_pool_binding(binding(3, "b", "x", 10))
                    .await
                    .unwrap();
                store
                    .set_pool_binding(binding(1, "a", "y", 200))
                    .await
                    .unwrap();
                let mut kept = store.pool_bindings(50).await.unwrap();
                kept.sort_by_key(|binding| binding.thread.0);
                assert_eq!(
                    kept,
                    vec![binding(1, "a", "y", 200), binding(2, "a", "y", 100)]
                );
                store
                    .delete_pool_bindings_to("claude-oauth".into(), "y".into())
                    .await
                    .unwrap();
                assert_eq!(
                    store.pool_bindings(0).await.unwrap(),
                    vec![binding(3, "b", "x", 10)]
                );
                store
                    .clear_pool_bindings(Some("b".into()), None)
                    .await
                    .unwrap();
                assert!(store.pool_bindings(0).await.unwrap().is_empty());

                let cooldown = PoolCooldown {
                    kind: "claude-oauth".into(),
                    account: "x".into(),
                    until_ms: 500,
                    avoid_until_ms: 900,
                    since_ms: 100,
                    source: "reset".into(),
                    status: 429,
                };
                store.set_pool_cooldown(cooldown.clone()).await.unwrap();
                assert_eq!(store.pool_cooldowns(800).await.unwrap(), vec![cooldown]);
                assert!(store.pool_cooldowns(900).await.unwrap().is_empty());
                store
                    .delete_pool_cooldown("claude-oauth".into(), "x".into())
                    .await
                    .unwrap();
                assert!(store.pool_cooldowns(0).await.unwrap().is_empty());
            });
    }
}
