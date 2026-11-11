//! On-disk layout of the pool, `INOCODE_HOME/accounts/`:
//!
//! ```text
//! accounts/policy.json            switching policy
//! accounts/_default.json          alias/priority/... of the legacy `auth.json` login
//! accounts/<id>/auth.json         credentials, same shape as the legacy `auth.json`
//! accounts/<id>/meta.json         alias, priority, cooldown, cached usage (no secrets)
//! ```
//!
//! Credentials go through the existing auth storage (`save_auth`/`AuthManager`), so refresh,
//! locking and the keyring modes behave exactly as for the default login.

use super::types::AccountKind;
use super::types::AccountMeta;
use super::types::AccountPolicy;
use crate::AuthCredentialsStoreMode;
use crate::AuthKeyringBackendKind;
use crate::AuthDotJson;
use crate::CodexAuth;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::Digest;
use sha2::Sha256;
use std::io;
use std::path::Path;
use std::path::PathBuf;

pub const DEFAULT_ID: &str = "default";
const POLICY_FILE: &str = "policy.json";
const DEFAULT_META_FILE: &str = "_default.json";
const META_FILE: &str = "meta.json";
const STATE_FILE: &str = "state.json";
/// Where a login that is still in progress writes, before it is renamed to its real id.
pub const PENDING_PREFIX: &str = "_pending-";

#[derive(Clone, Debug)]
pub struct AccountStore {
    root: PathBuf,
    mode: AuthCredentialsStoreMode,
    keyring: AuthKeyringBackendKind,
}

/// Ids become directory names, so only plain characters are allowed.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('_')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn short_hash(parts: &[&str], len: usize) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    let hex = format!("{:x}", hasher.finalize());
    hex.chars().take(len).collect()
}

/// `chatgpt-` + a hash of the ChatGPT user id and the workspace id (the pair
/// `WorkspaceLogin` compares). `None` when the auth carries neither.
pub fn chatgpt_account_id(auth: &CodexAuth) -> Option<String> {
    let user = auth.get_chatgpt_user_id()?;
    let workspace = auth.get_account_id().unwrap_or_default();
    Some(format!("chatgpt-{}", short_hash(&[&user, &workspace], 16)))
}

pub fn api_key_account_id(api_key: &str) -> String {
    format!("apikey-{}", short_hash(&[api_key], 6))
}

impl AccountStore {
    pub fn new(
        codex_home: &Path,
        mode: AuthCredentialsStoreMode,
        keyring: AuthKeyringBackendKind,
    ) -> Self {
        Self::in_dir(codex_home, "accounts", mode, keyring)
    }

    /// A store under another directory name (one per provider group).
    pub fn in_dir(
        codex_home: &Path,
        dir_name: &str,
        mode: AuthCredentialsStoreMode,
        keyring: AuthKeyringBackendKind,
    ) -> Self {
        Self {
            root: codex_home.join(dir_name),
            mode,
            keyring,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn account_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    pub fn pending_dir(&self, login_id: &str) -> PathBuf {
        self.root.join(format!("{PENDING_PREFIX}{login_id}"))
    }

    pub fn mode(&self) -> AuthCredentialsStoreMode {
        self.mode
    }

    pub fn keyring(&self) -> AuthKeyringBackendKind {
        self.keyring
    }

    pub fn ids(&self) -> Vec<String> {
        let Ok(read) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = read
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| valid_id(name))
            .collect();
        ids.sort();
        ids
    }

    pub fn load_meta(&self, id: &str) -> Option<AccountMeta> {
        if id == DEFAULT_ID {
            return read_json(&self.root.join(DEFAULT_META_FILE));
        }
        if !valid_id(id) {
            return None;
        }
        read_json(&self.account_dir(id).join(META_FILE))
    }

    pub fn save_meta(&self, meta: &AccountMeta) -> io::Result<()> {
        if meta.id == DEFAULT_ID {
            return write_json(&self.root.join(DEFAULT_META_FILE), meta);
        }
        if !valid_id(&meta.id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid account id",
            ));
        }
        write_json(&self.account_dir(&meta.id).join(META_FILE), meta)
    }

    pub fn load_policy(&self) -> AccountPolicy {
        read_json(&self.root.join(POLICY_FILE)).unwrap_or_default()
    }

    pub fn save_policy(&self, policy: &AccountPolicy) -> io::Result<()> {
        write_json(&self.root.join(POLICY_FILE), policy)
    }

    /// The account chosen by hand (`use`), which survives a restart.
    pub fn load_pinned(&self) -> Option<String> {
        let state: PersistedState = read_json(&self.root.join(STATE_FILE))?;
        state.pinned
    }

    pub fn save_pinned(&self, pinned: Option<&str>) -> io::Result<()> {
        write_json(
            &self.root.join(STATE_FILE),
            &PersistedState {
                pinned: pinned.map(str::to_string),
            },
        )
    }

    /// Stores an API key as its own entry (same shape as the legacy `auth.json`).
    pub fn save_api_key(&self, id: &str, api_key: &str) -> io::Result<()> {
        if !valid_id(id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid account id",
            ));
        }
        crate::login_with_api_key(&self.account_dir(id), api_key, self.mode, self.keyring)
    }

    /// Moves a finished pending login to its real id. Returns the new directory.
    pub fn adopt_pending(&self, login_id: &str, id: &str) -> io::Result<PathBuf> {
        if !valid_id(id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid account id",
            ));
        }
        let from = self.pending_dir(login_id);
        let to = self.account_dir(id);
        if to.exists() {
            // Signing in again to a known account refreshes its credentials.
            std::fs::remove_dir_all(&to)?;
        }
        std::fs::rename(&from, &to)?;
        Ok(to)
    }

    pub fn load_auth(&self, id: &str) -> io::Result<Option<AuthDotJson>> {
        crate::load_auth_dot_json(&self.account_dir(id), self.mode, self.keyring)
    }

    /// Deletes one entry's credentials and metadata. The legacy login is only forgotten here;
    /// its `auth.json` is the caller's business.
    pub fn remove(&self, id: &str) -> io::Result<()> {
        if id == DEFAULT_ID {
            match std::fs::remove_file(self.root.join(DEFAULT_META_FILE)) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
                _ => return Ok(()),
            }
        }
        if !valid_id(id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid account id",
            ));
        }
        match std::fs::remove_dir_all(self.account_dir(id)) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        }
    }

    pub fn kind_of(&self, id: &str) -> Option<AccountKind> {
        self.load_meta(id).map(|meta| meta.kind)
    }
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct PersistedState {
    #[serde(default)]
    pinned: Option<String>,
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let json = serde_json::to_string_pretty(value)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    io::Write::write_all(&mut tmp, json.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|err| err.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_pool::types::AccountKind;
    use pretty_assertions::assert_eq;

    fn store(dir: &Path) -> AccountStore {
        AccountStore::new(
            dir,
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )
    }

    #[test]
    fn ids_are_filename_safe() {
        assert!(valid_id("chatgpt-3a9f0c12d4e5b678"));
        assert!(valid_id("apikey-3f9c1a"));
        for bad in ["", "..", "a/b", "a\\b", "_pending-x", "a:b", "a b"] {
            assert!(!valid_id(bad), "{bad}");
        }
        assert!(api_key_account_id("sk-test-FAKE").starts_with("apikey-"));
    }

    #[test]
    fn meta_policy_and_api_key_round_trip_without_leaking_the_key_into_meta() {
        let home = tempfile::tempdir().expect("tempdir");
        let store = store(home.path());
        let id = api_key_account_id("sk-test-FAKE-0001");
        store.save_api_key(&id, "sk-test-FAKE-0001").expect("save key");
        let meta = AccountMeta {
            id: id.clone(),
            kind: AccountKind::ApiKey,
            alias: "予備".to_string(),
            plan: None,
            priority: 3,
            enabled: true,
            cooldown_until: None,
            last_used_at: None,
            usage: None,
            added_at: 5,
            provider: "openai".to_string(),
        };
        store.save_meta(&meta).expect("save meta");
        assert_eq!(store.ids(), vec![id.clone()]);
        assert_eq!(store.load_meta(&id), Some(meta));
        let auth = store.load_auth(&id).expect("load").expect("present");
        assert_eq!(auth.openai_api_key.as_deref(), Some("sk-test-FAKE-0001"));
        let meta_text =
            std::fs::read_to_string(store.account_dir(&id).join("meta.json")).expect("meta file");
        assert!(!meta_text.contains("sk-test-FAKE"));

        let mut policy = AccountPolicy::default();
        policy.threshold_percent = 75.0;
        store.save_policy(&policy).expect("save policy");
        assert_eq!(store.load_policy(), policy);

        store.remove(&id).expect("remove");
        assert!(store.ids().is_empty());
        assert_eq!(store.load_policy(), policy);
    }

    #[test]
    fn default_meta_lives_beside_the_directories() {
        let home = tempfile::tempdir().expect("tempdir");
        let store = store(home.path());
        let meta = AccountMeta {
            id: DEFAULT_ID.to_string(),
            kind: AccountKind::Chatgpt,
            alias: "メイン".to_string(),
            plan: Some("pro".to_string()),
            priority: 10,
            enabled: true,
            cooldown_until: None,
            last_used_at: None,
            usage: None,
            added_at: 0,
            provider: "openai".to_string(),
        };
        store.save_meta(&meta).expect("save");
        assert_eq!(store.load_meta(DEFAULT_ID), Some(meta));
        assert!(store.ids().is_empty());
        store.remove(DEFAULT_ID).expect("forget");
        assert_eq!(store.load_meta(DEFAULT_ID), None);
    }
}
