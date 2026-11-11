use super::flow;
use super::store;
use super::store::ClaudeCredentials;
use crate::AuthRouteConfig;
use std::io;
use std::path::PathBuf;
use tokio::sync::Mutex;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Holds one Claude sign-in and keeps its access token fresh.
///
/// The disk copy is re-read before a refresh, so a refresh done by another process is picked up
/// instead of being repeated (a rotated refresh token would otherwise be spent twice).
pub struct ClaudeAuthManager {
    dir: PathBuf,
    route: AuthRouteConfig,
    state: Mutex<Option<ClaudeCredentials>>,
}

impl std::fmt::Debug for ClaudeAuthManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeAuthManager")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl ClaudeAuthManager {
    pub fn open(dir: PathBuf, route: AuthRouteConfig) -> Self {
        let loaded = store::load(&dir).ok().flatten();
        Self {
            dir,
            route,
            state: Mutex::new(loaded),
        }
    }

    /// Stores a fresh sign-in under `dir` and opens it.
    pub fn create(dir: PathBuf, route: AuthRouteConfig, credentials: ClaudeCredentials) -> io::Result<Self> {
        store::save(&dir, &credentials)?;
        Ok(Self {
            dir,
            route,
            state: Mutex::new(Some(credentials)),
        })
    }

    pub async fn account_uuid(&self) -> Option<String> {
        self.state.lock().await.as_ref().and_then(|c| c.account_uuid.clone())
    }

    pub async fn plan(&self) -> Option<String> {
        self.state.lock().await.as_ref().and_then(|c| c.plan.clone())
    }

    /// A usable access token, refreshing first when it is (about to be) expired.
    pub async fn access_token(&self) -> io::Result<String> {
        self.access_token_inner(false).await
    }

    /// After a 401: refresh regardless of the recorded expiry. `Ok(true)` when a new token exists.
    pub async fn refresh_now(&self) -> io::Result<bool> {
        self.access_token_inner(true).await.map(|_| true)
    }

    async fn access_token_inner(&self, force: bool) -> io::Result<String> {
        let mut state = self.state.lock().await;
        let now = now_ms();
        let Some(current) = state.clone() else {
            return Err(io::Error::other("no Claude sign-in; run the sign-in again"));
        };
        if !force && !current.is_expired(now) {
            return Ok(current.access);
        }
        // Another process may already have refreshed.
        if let Ok(Some(on_disk)) = store::load(&self.dir)
            && on_disk.access != current.access
            && !on_disk.is_expired(now)
        {
            let access = on_disk.access.clone();
            *state = Some(on_disk);
            return Ok(access);
        }
        let refreshed = flow::refresh_credentials(&current, &self.route, now).await?;
        store::save(&self.dir, &refreshed)?;
        let access = refreshed.access.clone();
        *state = Some(refreshed);
        Ok(access)
    }
}
