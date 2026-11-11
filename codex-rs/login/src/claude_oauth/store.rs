use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::io;
use std::path::Path;
use std::path::PathBuf;

const FILE: &str = "claude.json";

/// What a Claude sign-in leaves on disk. No e-mail address is stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeCredentials {
    pub access: String,
    pub refresh: String,
    /// Unix milliseconds, already reduced by the refresh margin.
    pub expires_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

impl ClaudeCredentials {
    pub fn is_expired(&self, now_ms: i64) -> bool {
        now_ms >= self.expires_at_ms
    }
}

/// `claude-` + a hash of the account uuid.
pub fn claude_account_id(account_uuid: &str) -> String {
    let digest = Sha256::digest(account_uuid.as_bytes());
    let hex = format!("{digest:x}");
    format!("claude-{}", &hex[..16])
}

pub(crate) fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

pub(crate) fn load(dir: &Path) -> io::Result<Option<ClaudeCredentials>> {
    match std::fs::read_to_string(path(dir)) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|_| io::Error::other("claude.json is not valid")),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

pub(crate) fn save(dir: &Path, credentials: &ClaudeCredentials) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(credentials)?;
    // A private temporary file in the same directory, then an atomic rename.
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    io::Write::write_all(&mut tmp, json.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path(dir)).map_err(|err| err.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn credentials_round_trip_and_the_id_is_stable_and_private() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(load(dir.path()).expect("load"), None);
        let credentials = ClaudeCredentials {
            access: "access-FAKE".into(),
            refresh: "refresh-FAKE".into(),
            expires_at_ms: 1_000,
            account_uuid: Some("uuid-1".into()),
            plan: Some("max".into()),
        };
        save(dir.path(), &credentials).expect("save");
        assert_eq!(load(dir.path()).expect("load"), Some(credentials.clone()));
        assert!(credentials.is_expired(1_000) && !credentials.is_expired(999));
        let id = claude_account_id("uuid-1");
        assert_eq!(id, claude_account_id("uuid-1"));
        assert!(id.starts_with("claude-") && !id.contains("uuid-1"));
    }
}
