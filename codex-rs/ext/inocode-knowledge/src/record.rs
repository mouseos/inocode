use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Rule,
    FailureCase,
    Issue,
    Decision,
    Fact,
    Contract,
    Procedure,
}

impl Kind {
    pub const ALL: [Kind; 7] = [
        Kind::Rule,
        Kind::FailureCase,
        Kind::Issue,
        Kind::Decision,
        Kind::Fact,
        Kind::Contract,
        Kind::Procedure,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Rule => "rule",
            Kind::FailureCase => "failure_case",
            Kind::Issue => "issue",
            Kind::Decision => "decision",
            Kind::Fact => "fact",
            Kind::Contract => "contract",
            Kind::Procedure => "procedure",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Project,
    Global,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "project" => Some(Scope::Project),
            "global" => Some(Scope::Global),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Unverified,
    Verified,
    Superseded,
    Retracted,
}

impl Status {
    pub fn parse(s: &str) -> Option<Status> {
        match s {
            "unverified" => Some(Status::Unverified),
            "verified" => Some(Status::Verified),
            "superseded" => Some(Status::Superseded),
            "retracted" => Some(Status::Retracted),
            _ => None,
        }
    }
    /// 置き換え・撤回されたものは、禁止事項や引き継ぎには使わない。
    pub fn is_live(self) -> bool {
        !matches!(self, Status::Superseded | Status::Retracted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Block,
    Warn,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchSpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
}

/// front-matter + 本文。`id` が空なら書き込み時に採番する。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    #[serde(default)]
    pub id: String,
    pub kind: Kind,
    pub title: String,
    #[serde(default = "default_scope")]
    pub scope: Scope,
    #[serde(default = "default_status")]
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created_at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    // rule
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub match_spec: Option<MatchSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforced_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_failure: Option<String>,
    // failure_case
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symptom: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_cause: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub do_not: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub what_worked: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// Markdown 本文(front-matter の外)。
    #[serde(default)]
    pub body: String,
}

fn default_scope() -> Scope {
    Scope::Project
}
fn default_status() -> Status {
    Status::Unverified
}

impl Record {
    pub fn new(kind: Kind, title: &str) -> Self {
        Record {
            id: String::new(),
            kind,
            title: title.to_string(),
            scope: Scope::Project,
            status: Status::Unverified,
            source: None,
            confidence: None,
            created_at: String::new(),
            updated_at: String::new(),
            created_by: None,
            supersedes: None,
            severity: None,
            match_spec: None,
            reason: None,
            enforced_by: None,
            from_failure: None,
            signature: None,
            symptom: None,
            root_cause: None,
            do_not: None,
            what_worked: None,
            evidence: None,
            body: String::new(),
        }
    }

    pub fn to_file_text(&self) -> String {
        let mut front = self.clone();
        front.body = String::new();
        // body は front-matter に入れない
        let mut value = serde_yaml::to_value(&front).unwrap_or(serde_yaml::Value::Null);
        if let serde_yaml::Value::Mapping(m) = &mut value {
            m.remove(serde_yaml::Value::String("body".into()));
        }
        let yaml = serde_yaml::to_string(&value).unwrap_or_default();
        format!("---\n{yaml}---\n{}", self.body)
    }

    pub fn from_file_text(text: &str) -> Option<Record> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let text = text.replace("\r\n", "\n");
        let rest = text.strip_prefix("---\n")?;
        let end = rest.find("\n---")?;
        let yaml = &rest[..end];
        let after = &rest[end + 4..];
        let body = after.strip_prefix('\n').unwrap_or(after);
        let mut rec: Record = serde_yaml::from_str(yaml).ok()?;
        rec.body = body.to_string();
        Some(rec)
    }

    /// 一覧用の 1 行要約。
    pub fn summary(&self) -> String {
        let first = self
            .reason
            .as_deref()
            .or(self.symptom.as_deref())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                self.body
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .to_string()
            });
        first.chars().take(160).collect()
    }
}
