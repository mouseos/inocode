//! Non-boolean settings in the `[inocode]` table of `config.toml`
//! (advisor thresholds, compaction constants, team estimate, `fork_turns` default).
//!
//! Keys use the same `section.name` form as the flags and map to
//! `[inocode.<section>] <name> = ...`. A missing key means the default.

use crate::config_path;
use crate::ensure_table;
use crate::inocode_home;
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    Int,
    Float,
    String,
    Enum,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    Int(i64),
    Float(f64),
    Text(String),
}

#[derive(Debug, Clone, Copy)]
pub struct ParamSpec {
    pub key: &'static str,
    pub kind: ParamKind,
    /// Default for `Int` / `Float` kinds.
    pub default_number: f64,
    /// Default for `String` / `Enum` kinds (empty for `advisor.model`: the current turn's model).
    pub default_text: &'static str,
    pub options: &'static [&'static str],
}

const fn int(key: &'static str, default: i64) -> ParamSpec {
    ParamSpec {
        key,
        kind: ParamKind::Int,
        default_number: default as f64,
        default_text: "",
        options: &[],
    }
}

const fn float(key: &'static str, default: f64) -> ParamSpec {
    ParamSpec {
        key,
        kind: ParamKind::Float,
        default_number: default,
        default_text: "",
        options: &[],
    }
}

pub const PARAMS: &[ParamSpec] = &[
    ParamSpec {
        key: "advisor.model",
        kind: ParamKind::String,
        default_number: 0.0,
        default_text: "",
        options: &[],
    },
    int("advisor.same_failure_k", 3),
    int("advisor.repeat_n", 3),
    int("advisor.no_progress_turns", 8),
    int("advisor.alternation_len", 4),
    int("compaction.reattach_files", 5),
    int("compaction.reattach_tokens", 50_000),
    int("compaction.max_consecutive_failures", 3),
    int("compaction.preview_bytes", 2000),
    float("team.speedup_threshold", 1.3),
    float("team.split_overhead_secs", 60.0),
    float("team.integrate_overhead_secs", 60.0),
    float("team.task_secs", 600.0),
    ParamSpec {
        key: "team.fork_turns_default",
        kind: ParamKind::Enum,
        default_number: 0.0,
        default_text: "none",
        options: &["none", "all"],
    },
];

impl ParamSpec {
    pub fn default_value(&self) -> ParamValue {
        match self.kind {
            ParamKind::Int => ParamValue::Int(self.default_number as i64),
            ParamKind::Float => ParamValue::Float(self.default_number),
            ParamKind::String | ParamKind::Enum => ParamValue::Text(self.default_text.to_string()),
        }
    }
}

pub fn spec(key: &str) -> Option<&'static ParamSpec> {
    PARAMS.iter().find(|p| p.key == key)
}

fn read_item(home: &Path, key: &str) -> Option<toml::Value> {
    let (section, name) = key.split_once('.')?;
    let table = std::fs::read_to_string(config_path(home))
        .ok()?
        .parse::<toml::Table>()
        .ok()?;
    table
        .get("inocode")?
        .as_table()?
        .get(section)?
        .as_table()?
        .get(name)
        .cloned()
}

/// The configured value, or the default when unset or of the wrong type.
/// `None` only for an unknown key.
pub fn read_param_from(home: &Path, key: &str) -> Option<ParamValue> {
    let spec = spec(key)?;
    let value = read_item(home, key).and_then(|v| match (spec.kind, v) {
        (ParamKind::Int, toml::Value::Integer(i)) if i >= 0 => Some(ParamValue::Int(i)),
        (ParamKind::Float, toml::Value::Float(f)) => Some(ParamValue::Float(f)),
        (ParamKind::Float, toml::Value::Integer(i)) => Some(ParamValue::Float(i as f64)),
        (ParamKind::String, toml::Value::String(s)) => Some(ParamValue::Text(s)),
        (ParamKind::Enum, toml::Value::String(s)) if spec.options.contains(&s.as_str()) => {
            Some(ParamValue::Text(s))
        }
        _ => None,
    });
    Some(value.unwrap_or_else(|| spec.default_value()))
}

pub fn read_param(key: &str) -> Option<ParamValue> {
    read_param_from(&inocode_home(), key)
}

/// Integer parameter at the point of use (default when unset). Unknown key: 0.
pub fn int_param(key: &str) -> u64 {
    match read_param(key) {
        Some(ParamValue::Int(i)) => u64::try_from(i).unwrap_or(0),
        _ => 0,
    }
}

pub fn float_param(key: &str) -> f64 {
    match read_param(key) {
        Some(ParamValue::Float(f)) => f,
        Some(ParamValue::Int(i)) => i as f64,
        _ => 0.0,
    }
}

/// String / enum parameter; `None` when unset or empty (for `advisor.model`: use the turn's model).
pub fn string_param(key: &str) -> Option<String> {
    match read_param(key) {
        Some(ParamValue::Text(s)) if !s.is_empty() => Some(s),
        _ => None,
    }
}

/// All parameters with their current values.
pub fn read_all_from(home: &Path) -> Vec<(&'static ParamSpec, ParamValue)> {
    PARAMS
        .iter()
        .map(|p| {
            (
                p,
                read_param_from(home, p.key).unwrap_or_else(|| p.default_value()),
            )
        })
        .collect()
}

fn bad(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

pub fn write_param(key: &str, value: Option<ParamValue>) -> io::Result<()> {
    write_param_to(&inocode_home(), key, value)
}

/// Writes (or, with `None`, removes) one parameter, preserving other content and comments.
/// Unknown keys and values of the wrong type are rejected with `InvalidInput`.
pub fn write_param_to(home: &Path, key: &str, value: Option<ParamValue>) -> io::Result<()> {
    let spec = spec(key).ok_or_else(|| bad(format!("unknown inocode parameter: {key}")))?;
    let (section, name) = key.split_once('.').ok_or_else(|| bad(key.to_string()))?;
    let item = match (&value, spec.kind) {
        (None, _) => None,
        (Some(ParamValue::Int(i)), ParamKind::Int) if *i >= 0 => Some(toml_edit::value(*i)),
        (Some(ParamValue::Float(f)), ParamKind::Int) if f.fract() == 0.0 && *f >= 0.0 => {
            Some(toml_edit::value(*f as i64))
        }
        (Some(ParamValue::Float(f)), ParamKind::Float) => Some(toml_edit::value(*f)),
        (Some(ParamValue::Int(i)), ParamKind::Float) => Some(toml_edit::value(*i as f64)),
        (Some(ParamValue::Text(s)), ParamKind::String) => Some(toml_edit::value(s.as_str())),
        (Some(ParamValue::Text(s)), ParamKind::Enum) if spec.options.contains(&s.as_str()) => {
            Some(toml_edit::value(s.as_str()))
        }
        _ => return Err(bad(format!("invalid value for inocode parameter `{key}`"))),
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
    match item {
        Some(item) => {
            let inocode = ensure_table(doc.as_table_mut(), "inocode")?;
            let sect = ensure_table(inocode, section)?;
            sect.insert(name, item);
        }
        None => {
            let Some(sect) = doc
                .as_table_mut()
                .get_mut("inocode")
                .and_then(|i| i.as_table_mut())
                .and_then(|t| t.get_mut(section))
                .and_then(|i| i.as_table_mut())
            else {
                return Ok(());
            };
            sect.remove(name);
        }
    }
    std::fs::create_dir_all(home)?;
    std::fs::write(&path, doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_write_reset_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        let h = dir.path();
        assert_eq!(
            read_param_from(h, "advisor.same_failure_k"),
            Some(ParamValue::Int(3))
        );
        std::fs::write(
            h.join("config.toml"),
            "# keep\n[inocode.advisor]\nmodel = \"m\" # c\n",
        )
        .unwrap();
        write_param_to(h, "advisor.same_failure_k", Some(ParamValue::Int(5))).unwrap();
        write_param_to(h, "team.speedup_threshold", Some(ParamValue::Float(1.5))).unwrap();
        write_param_to(
            h,
            "team.fork_turns_default",
            Some(ParamValue::Text("all".into())),
        )
        .unwrap();
        assert_eq!(
            read_param_from(h, "advisor.same_failure_k"),
            Some(ParamValue::Int(5))
        );
        assert_eq!(
            read_param_from(h, "team.speedup_threshold"),
            Some(ParamValue::Float(1.5))
        );
        assert_eq!(
            read_param_from(h, "advisor.model"),
            Some(ParamValue::Text("m".into()))
        );
        let out = std::fs::read_to_string(h.join("config.toml")).unwrap();
        assert!(out.contains("# keep") && out.contains("model = \"m\" # c"));
        write_param_to(h, "advisor.same_failure_k", None).unwrap();
        assert_eq!(
            read_param_from(h, "advisor.same_failure_k"),
            Some(ParamValue::Int(3))
        );
        assert!(write_param_to(h, "bogus.x", Some(ParamValue::Int(1))).is_err());
        assert!(
            write_param_to(
                h,
                "team.fork_turns_default",
                Some(ParamValue::Text("x".into()))
            )
            .is_err()
        );
        assert!(write_param_to(h, "advisor.repeat_n", Some(ParamValue::Int(-1))).is_err());
        assert!(crate::InocodeFlags::load_from(h).metrics_enabled);
    }
}
