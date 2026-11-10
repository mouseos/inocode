//! Feature flags for the InoCode agent-improvement mechanisms.
//!
//! Flags live in the `[inocode]` table of `$INOCODE_HOME/config.toml`
//! (`~/.inocode/config.toml` when unset). A key such as `metrics.enabled`
//! maps to `[inocode.metrics] enabled = ...`. Every flag defaults to `true`.
//!
//! The file is read directly with `toml`; writes use `toml_edit` so other
//! content and comments are preserved.

use std::io;
use std::path::Path;
use std::path::PathBuf;

/// All flag keys, in contract order.
pub const FLAG_KEYS: [&str; 11] = [
    "metrics.enabled",
    "wait.event_driven",
    "knowledge.enabled",
    "knowledge.pinned_rules",
    "knowledge.guard",
    "protocol.enabled",
    "peer_messaging.enabled",
    "advisor.enabled",
    "compaction.persist_large_output",
    "team.auto_select",
    "evidence.enabled",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InocodeFlags {
    pub metrics_enabled: bool,
    pub wait_event_driven: bool,
    pub knowledge_enabled: bool,
    pub knowledge_pinned_rules: bool,
    pub knowledge_guard: bool,
    pub protocol_enabled: bool,
    pub peer_messaging_enabled: bool,
    pub advisor_enabled: bool,
    pub compaction_persist_large_output: bool,
    pub team_auto_select: bool,
    pub evidence_enabled: bool,
}

impl Default for InocodeFlags {
    fn default() -> Self {
        Self {
            metrics_enabled: true,
            wait_event_driven: true,
            knowledge_enabled: true,
            knowledge_pinned_rules: true,
            knowledge_guard: true,
            protocol_enabled: true,
            peer_messaging_enabled: true,
            advisor_enabled: true,
            compaction_persist_large_output: true,
            team_auto_select: true,
            evidence_enabled: true,
        }
    }
}

/// `$INOCODE_HOME`, or `~/.inocode` when unset or empty.
pub fn inocode_home() -> PathBuf {
    match std::env::var("INOCODE_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".inocode"),
    }
}

fn config_path(home: &Path) -> PathBuf {
    home.join("config.toml")
}

impl InocodeFlags {
    /// Loads flags from `$INOCODE_HOME/config.toml`. Missing file, parse
    /// errors, and missing or non-boolean keys fall back to `true`.
    pub fn load() -> InocodeFlags {
        Self::load_from(&inocode_home())
    }

    pub fn load_from(home: &Path) -> InocodeFlags {
        let table = std::fs::read_to_string(config_path(home))
            .ok()
            .and_then(|s| s.parse::<toml::Table>().ok());
        let inocode = table
            .as_ref()
            .and_then(|t| t.get("inocode"))
            .and_then(|v| v.as_table());
        let mut flags = InocodeFlags::default();
        if let Some(inocode) = inocode {
            for key in FLAG_KEYS {
                if let Some(value) = lookup(inocode, key) {
                    flags.set(key, value);
                }
            }
        }
        flags
    }

    pub fn get(&self, key: &str) -> Option<bool> {
        Some(match key {
            "metrics.enabled" => self.metrics_enabled,
            "wait.event_driven" => self.wait_event_driven,
            "knowledge.enabled" => self.knowledge_enabled,
            "knowledge.pinned_rules" => self.knowledge_pinned_rules,
            "knowledge.guard" => self.knowledge_guard,
            "protocol.enabled" => self.protocol_enabled,
            "peer_messaging.enabled" => self.peer_messaging_enabled,
            "advisor.enabled" => self.advisor_enabled,
            "compaction.persist_large_output" => self.compaction_persist_large_output,
            "team.auto_select" => self.team_auto_select,
            "evidence.enabled" => self.evidence_enabled,
            _ => return None,
        })
    }

    /// Returns false when `key` is not a known flag.
    pub fn set(&mut self, key: &str, value: bool) -> bool {
        let slot = match key {
            "metrics.enabled" => &mut self.metrics_enabled,
            "wait.event_driven" => &mut self.wait_event_driven,
            "knowledge.enabled" => &mut self.knowledge_enabled,
            "knowledge.pinned_rules" => &mut self.knowledge_pinned_rules,
            "knowledge.guard" => &mut self.knowledge_guard,
            "protocol.enabled" => &mut self.protocol_enabled,
            "peer_messaging.enabled" => &mut self.peer_messaging_enabled,
            "advisor.enabled" => &mut self.advisor_enabled,
            "compaction.persist_large_output" => &mut self.compaction_persist_large_output,
            "team.auto_select" => &mut self.team_auto_select,
            "evidence.enabled" => &mut self.evidence_enabled,
            _ => return false,
        };
        *slot = value;
        true
    }

    /// All flags as `(key, value)` pairs in contract order.
    pub fn entries(&self) -> Vec<(&'static str, bool)> {
        FLAG_KEYS
            .iter()
            .map(|k| (*k, self.get(k).unwrap_or(true)))
            .collect()
    }
}

fn lookup(inocode: &toml::Table, key: &str) -> Option<bool> {
    let (section, name) = key.split_once('.')?;
    inocode.get(section)?.as_table()?.get(name)?.as_bool()
}

/// Convenience for `InocodeFlags::load()`.
pub fn load() -> InocodeFlags {
    InocodeFlags::load()
}

/// Writes one flag into `$INOCODE_HOME/config.toml`.
pub fn write_flag(key: &str, value: bool) -> io::Result<()> {
    write_flag_to(&inocode_home(), key, value)
}

/// Writes one flag into `<home>/config.toml`, preserving other content and
/// comments. Unknown keys are rejected with `InvalidInput`.
pub fn write_flag_to(home: &Path, key: &str, value: bool) -> io::Result<()> {
    let Some((section, name)) = key.split_once('.').filter(|_| FLAG_KEYS.contains(&key)) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown inocode flag: {key}"),
        ));
    };
    let path = config_path(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let root = doc.as_table_mut();
    let inocode = ensure_table(root, "inocode")?;
    let sect = ensure_table(inocode, section)?;
    sect.insert(name, toml_edit::value(value));
    std::fs::create_dir_all(home)?;
    std::fs::write(&path, doc.to_string())
}

fn ensure_table<'a>(
    parent: &'a mut toml_edit::Table,
    key: &str,
) -> io::Result<&'a mut toml_edit::Table> {
    if !parent.contains_key(key) {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        parent.insert(key, toml_edit::Item::Table(t));
    }
    parent
        .get_mut(key)
        .and_then(|i| i.as_table_mut())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("config key `{key}` is not a table"),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn defaults_all_true() {
        let dir = tempfile::tempdir().unwrap();
        let f = InocodeFlags::load_from(dir.path());
        assert_eq!(f, InocodeFlags::default());
        assert!(f.entries().iter().all(|(_, v)| *v));
        assert_eq!(f.entries().len(), 11);
    }

    #[test]
    fn reads_false_and_ignores_garbage() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(dir.path()),
            "[inocode.metrics]\nenabled = false\n[inocode.wait]\nevent_driven = \"x\"\n[inocode.knowledge]\nguard = false\n",
        )
        .unwrap();
        let f = InocodeFlags::load_from(dir.path());
        assert!(!f.metrics_enabled);
        assert!(!f.knowledge_guard);
        assert!(f.wait_event_driven);
        assert!(f.knowledge_enabled);
    }

    #[test]
    fn write_preserves_other_content() {
        let dir = tempfile::tempdir().unwrap();
        let src = "# top comment\nmodel = \"x\" # trailing\n\n[other]\na = 1\n";
        std::fs::write(config_path(dir.path()), src).unwrap();
        write_flag_to(dir.path(), "advisor.enabled", false).unwrap();
        write_flag_to(dir.path(), "knowledge.guard", false).unwrap();
        write_flag_to(dir.path(), "advisor.enabled", true).unwrap();
        let out = std::fs::read_to_string(config_path(dir.path())).unwrap();
        assert!(out.contains("# top comment"));
        assert!(out.contains("model = \"x\" # trailing"));
        assert!(out.contains("[other]\na = 1"));
        let f = InocodeFlags::load_from(dir.path());
        assert!(f.advisor_enabled);
        assert!(!f.knowledge_guard);
    }

    #[test]
    fn write_creates_file_and_rejects_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("new");
        write_flag_to(&home, "team.auto_select", false).unwrap();
        assert!(!InocodeFlags::load_from(&home).team_auto_select);
        assert!(write_flag_to(&home, "bogus.key", true).is_err());
    }
}
