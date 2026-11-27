//! The company: members (a model copied with a role written by the
//! person), the lists of what may be called, and the two tools that let
//! the model hand a task to a member (`delegate`) or have work reviewed by
//! one (`review`). Design, decisions and sources: docs/evidence/company.md.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Digest;
use tokio::sync::{mpsc, watch};

use crate::agent_tools::{Answer, Context};
use crate::conversation::{AssistantPart, Body, ToolSpec};
use crate::store::{ProjectId, SessionId, Store};
use crate::turn::{Mode, Outcome, TurnUsage};
use crate::{settings, Core, ErrorInfo, Llm};

pub(crate) const DELEGATE: &str = "delegate";
pub(crate) const REVIEW: &str = "review";
/// The repository's own allow list, used only in the exact content the
/// person approved.
pub(crate) const REPOSITORY_FILE: &str = ".inocode/models.json";
/// The most tokens a delegated task may use when the person sets no
/// maximum (`company.max_budget_tokens`); an engineering choice.
pub(crate) const DEFAULT_MAX_BUDGET: u64 = 1_000_000;
const ROLE_CHARS: usize = 2_000;
/// The benchmark uses the model names as a task's kind.
pub(crate) const KINDS: [&str; 5] = ["coding", "reasoning", "math", "agentic", "general"];
const DIFFICULTIES: [&str; 3] = ["easy", "medium", "hard"];
/// Words of a role that mark a strict or adversarial reviewer.
const STRICT_WORDS: [&str; 14] = [
    "strict",
    "adversarial",
    "reviewer",
    "review",
    "doubt",
    "skeptic",
    "critic",
    "厳しい",
    "厳格",
    "レビュー",
    "疑う",
    "疑い",
    "審査",
    "批判",
];

/// A member as the person wrote it in `settings.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberConfig {
    pub name: String,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default)]
    pub role: String,
}

/// The allow lists of `settings.json`: entries are member names or
/// `provider/model`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowConfig {
    #[serde(default)]
    pub default: Vec<String>,
    /// By project folder, as the project's key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub projects: BTreeMap<String, Vec<String>>,
    /// The SHA-256 of the repository file the person approved, by project.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repository: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_budget_tokens: Option<u64>,
}

/// The sections of `settings.json` this module owns: `members`, `allow`
/// and `company`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub members: Vec<MemberConfig>,
    pub allow: AllowConfig,
    pub limits: Limits,
}

impl Config {
    pub fn from_value(file: &Value) -> Result<Self, String> {
        let section = |name: &str| file.get(name).filter(|value| !value.is_null()).cloned();
        let members: Vec<MemberConfig> = match section("members") {
            Some(value) => {
                serde_json::from_value(value).map_err(|error| format!("\"members\": {error}"))?
            }
            None => Vec::new(),
        };
        let allow = match section("allow") {
            Some(value) => {
                serde_json::from_value(value).map_err(|error| format!("\"allow\": {error}"))?
            }
            None => AllowConfig::default(),
        };
        let limits = match section("company") {
            Some(value) => {
                serde_json::from_value(value).map_err(|error| format!("\"company\": {error}"))?
            }
            None => Limits::default(),
        };
        let config = Self {
            members,
            allow,
            limits,
        };
        for (index, member) in config.members.iter().enumerate() {
            check_member(member)?;
            if config.members[..index]
                .iter()
                .any(|other| other.name == member.name)
            {
                return Err(format!(
                    "\"members\": the name {} is used twice",
                    member.name
                ));
            }
        }
        if config.limits.max_budget_tokens == Some(0) {
            return Err("\"company.max_budget_tokens\" must be above 0".into());
        }
        Ok(config)
    }

    pub async fn load(path: &Path) -> Result<Self, String> {
        Self::from_value(&settings::read(path).await?)
            .map_err(|error| format!("{}: {error}", path.display()))
    }

    pub fn max_budget(&self) -> u64 {
        self.limits.max_budget_tokens.unwrap_or(DEFAULT_MAX_BUDGET)
    }

    fn member(&self, name: &str) -> Option<&MemberConfig> {
        self.members.iter().find(|member| member.name == name)
    }

    /// What an allow-list entry names: a member, or a provider's model.
    pub fn resolve(&self, entry: &str) -> Option<MemberConfig> {
        if let Some(member) = self.member(entry) {
            return Some(member.clone());
        }
        let (provider, model) = entry.split_once('/')?;
        (!provider.is_empty() && !model.is_empty()).then(|| MemberConfig {
            name: entry.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            effort: None,
            role: String::new(),
        })
    }
}

/// Member names are short and safe to name in a tool call.
fn check_member(member: &MemberConfig) -> Result<(), String> {
    let name = &member.name;
    let usable = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !usable {
        return Err(format!(
            "the member name {name:?} must be 1 to 64 letters, digits, '-', '_' or '.'"
        ));
    }
    if member.provider.is_empty() || member.model.is_empty() {
        return Err(format!("the member {name} needs a provider and a model"));
    }
    if let Some(effort) = &member.effort {
        if crate::reasoning::Effort::parse(effort).is_none() {
            return Err(format!("the member {name}: unknown effort {effort:?}"));
        }
    }
    if member.role.chars().count() > ROLE_CHARS {
        return Err(format!(
            "the member {name}: the role is longer than {ROLE_CHARS} characters"
        ));
    }
    Ok(())
}

/// The repository file's state for one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Repository {
    Absent,
    /// The content the person approved: its entries are used.
    Approved {
        sha256: String,
        entries: Vec<String>,
    },
    /// New or changed since the person approved it: not used until the
    /// person approves this content.
    NotApproved {
        sha256: String,
    },
    /// Approved but not a list the harness can read: not used.
    Invalid {
        sha256: String,
        error: String,
    },
}

/// The allow list in effect for a project, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Effective {
    pub entries: Vec<String>,
    /// `project` when the person's list for the folder or an approved
    /// repository file applies, else `default`.
    pub source: &'static str,
    pub repository: Repository,
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryFile {
    allow: Vec<String>,
}

/// The allow list in effect in the project `project` whose repository file
/// holds `repository` (its bytes, when it has one). A project list, from
/// the person's settings for the folder or an approved repository file,
/// replaces the default list, so a project can also narrow it.
pub fn effective(
    allow: &AllowConfig,
    project: Option<&str>,
    repository: Option<&[u8]>,
) -> Effective {
    let repository = match (project, repository) {
        (Some(project), Some(bytes)) => {
            let sha256 = sha256(bytes);
            if allow.repository.get(project) != Some(&sha256) {
                Repository::NotApproved { sha256 }
            } else {
                match serde_json::from_slice::<RepositoryFile>(bytes) {
                    Ok(file) => Repository::Approved {
                        sha256,
                        entries: file.allow,
                    },
                    Err(error) => Repository::Invalid {
                        sha256,
                        error: error.to_string(),
                    },
                }
            }
        }
        _ => Repository::Absent,
    };
    let listed = project.and_then(|project| allow.projects.get(project));
    let approved = match &repository {
        Repository::Approved { entries, .. } => Some(entries),
        _ => None,
    };
    let (source, lists): (&'static str, Vec<&Vec<String>>) = match (listed, approved) {
        (None, None) => ("default", vec![&allow.default]),
        (listed, approved) => ("project", listed.into_iter().chain(approved).collect()),
    };
    let mut entries: Vec<String> = Vec::new();
    for entry in lists.into_iter().flatten() {
        let entry = entry.trim();
        if !entry.is_empty() && !entries.iter().any(|kept| kept == entry) {
            entries.push(entry.to_string());
        }
    }
    Effective {
        entries,
        source,
        repository,
    }
}

/// What the harness knows of a member for choosing it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Profile {
    pub name: String,
    pub role: String,
    pub provider: String,
    pub model: String,
    pub vendor: String,
    /// List prices in US dollars per million tokens, input and output.
    pub price: Option<(f64, f64)>,
    /// By use, 1 for the best of the models a metric lists, 0 for the
    /// worst; the mean over the use's metrics.
    pub quality: Vec<(&'static str, f64)>,
    pub tokens_per_second: Option<f64>,
}

impl Profile {
    fn quality_for(&self, kind: &str) -> Option<f64> {
        self.quality
            .iter()
            .find(|(use_, _)| *use_ == kind)
            .map(|(_, quality)| *quality)
    }

    /// The quality for `kind`: the general one when the kind has no
    /// score, and 0.5 when nothing is known.
    fn quality(&self, kind: &str) -> f64 {
        self.quality_for(kind)
            .or_else(|| self.quality_for("general"))
            .unwrap_or(0.5)
    }

    fn strict(&self) -> bool {
        let role = self.role.to_lowercase();
        STRICT_WORDS.iter().any(|word| role.contains(word))
    }
}

/// The vendor that made a model, by its name; the provider when the name
/// says nothing.
pub(crate) fn vendor(model: &str, provider: &str) -> String {
    crate::catalog::providers_of(model)
        .first()
        .map_or_else(|| provider.to_string(), |vendor| vendor.to_string())
}

/// The quality by use from catalog scores: `(of - rank) / (of - 1)`.
pub(crate) fn qualities(scores: &[crate::catalog::Score]) -> Vec<(&'static str, f64)> {
    let mut found: Vec<(&'static str, f64, u32)> = Vec::new();
    for score in scores {
        let quality = match score.of {
            0 | 1 => 1.0,
            of => (of - score.rank.min(of)) as f64 / (of - 1) as f64,
        };
        match found.iter_mut().find(|(use_, _, _)| *use_ == score.use_) {
            Some(kept) => {
                kept.1 += quality;
                kept.2 += 1;
            }
            None => found.push((score.use_, quality, 1)),
        }
    }
    found
        .into_iter()
        .map(|(use_, sum, count)| (use_, sum / f64::from(count)))
        .collect()
}

/// Blended list price per million tokens, three parts input to one part
/// output; the median of the known ones stands in for an unknown one.
fn costs(profiles: &[&Profile]) -> Vec<f64> {
    let blended = |profile: &Profile| {
        profile
            .price
            .map(|(input, output)| (3.0 * input + output) / 4.0)
    };
    let mut known: Vec<f64> = profiles
        .iter()
        .filter_map(|profile| blended(profile))
        .collect();
    known.sort_by(f64::total_cmp);
    let median = match known.len() {
        0 => 0.0,
        n if n % 2 == 1 => known[n / 2],
        n => (known[n / 2 - 1] + known[n / 2]) / 2.0,
    };
    profiles
        .iter()
        .map(|profile| blended(profile).unwrap_or(median))
        .collect()
}

/// The lowest quality a member must have for a task of `difficulty`.
fn threshold(difficulty: &str) -> f64 {
    match difficulty {
        "easy" => 0.0,
        "hard" => 0.75,
        _ => 0.5,
    }
}

/// The member for a task of `kind` and `difficulty` when the model named
/// none: the cheapest whose quality for the kind reaches the difficulty's
/// threshold; the best one when none does. Ties go to the better, then the
/// faster, then the first by name.
pub(crate) fn pick<'a>(
    profiles: &'a [Profile],
    kind: &str,
    difficulty: &str,
) -> Option<&'a Profile> {
    let all: Vec<&Profile> = profiles.iter().collect();
    let costs = costs(&all);
    let scored: Vec<(&Profile, f64, f64)> = all
        .iter()
        .zip(costs)
        .map(|(profile, cost)| (*profile, profile.quality(kind), cost))
        .collect();
    let speed = |profile: &Profile| profile.tokens_per_second.unwrap_or(0.0);
    let minimum = threshold(difficulty);
    let adequate: Vec<&(&Profile, f64, f64)> = scored
        .iter()
        .filter(|(_, quality, _)| *quality >= minimum)
        .collect();
    let chosen = if adequate.is_empty() {
        scored.iter().min_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then(a.2.total_cmp(&b.2))
                .then(a.0.name.cmp(&b.0.name))
        })
    } else {
        adequate.into_iter().min_by(|a, b| {
            a.2.total_cmp(&b.2)
                .then(b.1.total_cmp(&a.1))
                .then(speed(b.0).total_cmp(&speed(a.0)))
                .then(a.0.name.cmp(&b.0.name))
        })
    };
    chosen.map(|(profile, _, _)| *profile)
}

/// The reviewer of work by a model of `author_vendor`: a member of another
/// vendor whose role marks a strict reviewer first, then any member of
/// another vendor, then a strict one of the same vendor, then any; within
/// each, the best at coding, then the cheapest.
pub(crate) fn pick_reviewer<'a>(
    profiles: &'a [Profile],
    author_vendor: &str,
) -> Option<&'a Profile> {
    let all: Vec<&Profile> = profiles.iter().collect();
    let costs = costs(&all);
    all.into_iter()
        .zip(costs)
        .min_by(|(a, a_cost), (b, b_cost)| {
            let tier = |profile: &Profile| match (profile.vendor != author_vendor, profile.strict())
            {
                (true, true) => 0,
                (true, false) => 1,
                (false, true) => 2,
                (false, false) => 3,
            };
            tier(a)
                .cmp(&tier(b))
                .then(b.quality("coding").total_cmp(&a.quality("coding")))
                .then(a_cost.total_cmp(b_cost))
                .then(a.name.cmp(&b.name))
        })
        .map(|(profile, _)| profile)
}

/// A path as compared for overlap: `/` separators, no leading `./` or
/// trailing `/`, lower case (a conservative match on any system).
fn path_key(path: &str) -> String {
    let path = path.trim().replace('\\', "/").to_lowercase();
    let path = path.trim_start_matches("./").trim_end_matches('/');
    path.to_string()
}

fn overlap(a: &str, b: &str) -> bool {
    let (a, b) = (path_key(a), path_key(b));
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

/// Whether the delegate calls of one answer, each `(independent, files)`,
/// run at the same time: only when there are several, every one is marked
/// independent and names its files, and no two share a file or folder.
/// Otherwise they run one after another: merging work done in parallel on
/// the same thing ranked last in the evidence.
pub(crate) fn together(calls: &[(bool, Vec<String>)]) -> bool {
    calls.len() >= 2
        && calls
            .iter()
            .all(|(independent, files)| *independent && !files.is_empty())
        && calls.iter().enumerate().all(|(index, (_, files))| {
            calls[index + 1..].iter().all(|(_, others)| {
                files
                    .iter()
                    .all(|file| others.iter().all(|other| !overlap(file, other)))
            })
        })
}

/// The token budget of a delegated task: the one asked for, else the high
/// end of the estimate, else the person's maximum; never over the maximum.
/// A task whose estimate starts above its budget is refused.
pub(crate) fn budget(
    asked: Option<u64>,
    estimate: Option<crate::estimate::Range<u64>>,
    max: u64,
) -> Result<u64, String> {
    if let Some(asked) = asked {
        if asked > max {
            return Err(format!(
                "Not run: budget_tokens {asked} is over the person's maximum of {max} \
                 (company.max_budget_tokens)."
            ));
        }
        if asked == 0 {
            return Err("Not run: budget_tokens must be above 0.".into());
        }
    }
    let budget = asked.unwrap_or_else(|| estimate.map_or(max, |range| range.high.min(max)));
    match estimate {
        Some(range) if range.low > budget => Err(format!(
            "Not run: similar past tasks used at least {} tokens, over this task's budget of \
             {budget}. Give a larger budget_tokens (at most {max}) or a smaller task.",
            range.low
        )),
        _ => Ok(budget),
    }
}

/// A member ready to work: what is known of it and its client.
#[derive(Clone)]
pub(crate) struct Member {
    pub profile: Profile,
    /// The effort the person chose for the member, if any.
    pub effort: Option<String>,
    pub llm: Arc<Llm>,
}

/// The members a thread's turn may call, all on its allow list.
pub struct Company {
    pub(crate) members: Vec<Member>,
    pub(crate) max_budget: u64,
    /// Members left out, and a repository list waiting for approval.
    pub(crate) warnings: Vec<String>,
}

impl Company {
    fn profiles(&self) -> Vec<Profile> {
        self.members
            .iter()
            .map(|member| member.profile.clone())
            .collect()
    }

    fn named(&self, name: &str) -> Option<&Member> {
        self.members
            .iter()
            .find(|member| member.profile.name == name)
    }

    fn by_profile(&self, profile: &Profile) -> Option<&Member> {
        self.named(&profile.name)
    }

    /// The members as the model reads them in the tool's description.
    fn roster(&self) -> String {
        let mut lines = Vec::new();
        for member in &self.members {
            let profile = &member.profile;
            let price = profile
                .price
                .map_or("price unknown".to_string(), |(input, output)| {
                    format!("${input}/${output} per 1M in/out")
                });
            let scores = if profile.quality.is_empty() {
                "no scores".to_string()
            } else {
                profile
                    .quality
                    .iter()
                    .map(|(use_, quality)| format!("{use_} {quality:.2}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let speed = profile
                .tokens_per_second
                .map_or("speed not measured".to_string(), |speed| {
                    format!("{speed:.0} tok/s measured")
                });
            let role = match profile.role.trim() {
                "" => "no role".to_string(),
                role => role.replace('\n', " "),
            };
            lines.push(format!(
                "- {} | {} | {}/{} | vendor {} | {price} | {scores} | {speed}",
                profile.name, role, profile.provider, profile.model, profile.vendor
            ));
        }
        lines.join("\n")
    }
}

/// The `delegate` tool, naming the members.
pub(crate) fn delegate_spec(company: &Company) -> ToolSpec {
    let names: Vec<&str> = company
        .members
        .iter()
        .map(|member| member.profile.name.as_str())
        .collect();
    ToolSpec {
        name: DELEGATE.into(),
        description: format!(
            "Hand a task to a member, who works on it as an agent in this project with the same \
             tools as you (read, edit, shell, tests...), under this thread's approval mode and \
             the project's rules, in a thread of its own. It sees the person's request and the \
             task, not this conversation, so write the task completely. It cannot delegate further; it can ask for \
             review. Returns its final report, the files it edited and the tokens it used. It \
             stops when it uses up its token budget (by default the high end of the estimate \
             for the task, at most {}). Name a member whose role fits; without one the harness \
             picks the cheapest member whose benchmark score for the task's kind reaches the \
             stated difficulty. Several delegate calls in one answer run at the same time only \
             when each is marked independent and names files that no other one names; \
             otherwise one after another. Never give the same work to several members to merge \
             their results. Do small work yourself.\nMembers (name | role | provider/model | \
             vendor | list price | scores 0-1 by benchmark rank, 1 best | speed):\n{}",
            company.max_budget,
            company.roster()
        ),
        parameters: json!({
            "type": "object",
            "required": ["task"],
            "properties": {
                "member": {"type": "string", "enum": names},
                "task": {"type": "string", "description": "The whole task: goal, context, constraints and how to check it."},
                "files": {"type": "array", "items": {"type": "string"}, "description": "Files or folders the task is expected to change."},
                "budget_tokens": {"type": "integer", "description": "Tokens the member may use, input and output."},
                "kind": {"type": "string", "enum": KINDS, "description": "Used to pick a member when none is named."},
                "difficulty": {"type": "string", "enum": DIFFICULTIES, "description": "Used to pick a member when none is named."},
                "independent": {"type": "boolean", "description": "True when this task needs nothing from the other delegate calls of this answer."}
            }
        }),
    }
}

/// The `review` tool.
pub(crate) fn review_spec(company: &Company) -> ToolSpec {
    let names: Vec<&str> = company
        .members
        .iter()
        .map(|member| member.profile.name.as_str())
        .collect();
    ToolSpec {
        name: REVIEW.into(),
        description: "Ask a member to review work in this project: it reads the files and \
            answers with findings. Without member, the harness picks a member of another vendor \
            than the work's author (models favor their own output), preferring one whose role \
            says strict reviewer. Use it before reporting a change done when a second look is \
            worth its cost, not after every step. Weigh the findings; fix what holds."
            .into(),
        parameters: json!({
            "type": "object",
            "required": ["what"],
            "properties": {
                "member": {"type": "string", "enum": names},
                "what": {"type": "string", "description": "What to review: the change, its purpose, and the paths and lines."},
                "author": {"type": "string", "enum": names, "description": "The member that did the work, when not you."}
            }
        }),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegateArguments {
    #[serde(default)]
    member: Option<String>,
    task: String,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    budget_tokens: Option<u64>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    difficulty: Option<String>,
    #[serde(default)]
    independent: bool,
}

/// Whether a delegate call says it is independent, and its files, for
/// [`together`]; a call that cannot be read is not independent.
pub(crate) fn plan(arguments: &str) -> (bool, Vec<String>) {
    serde_json::from_str::<DelegateArguments>(arguments)
        .map(|arguments| (arguments.independent, arguments.files))
        .unwrap_or((false, Vec::new()))
}

/// What a delegating turn hands to its members' runs.
pub(crate) struct Lead<'a> {
    pub store: &'a Store,
    pub session: SessionId,
    pub project: Option<ProjectId>,
    pub mode: Mode,
    pub cancel: watch::Receiver<bool>,
    pub reviewer: Option<Arc<Llm>>,
    pub compactor: Option<Arc<Llm>>,
    pub explorer: Option<Arc<Llm>>,
    pub checker: Option<Arc<Llm>>,
    pub colleague: Option<Arc<Llm>>,
    pub company: Arc<Company>,
    /// The person's request of the delegating turn.
    pub request: String,
}

/// What one delegated run gives back to the delegating turn.
pub(crate) struct Delegated {
    pub answer: Answer,
    pub usage: TurnUsage,
    pub warnings: Vec<String>,
}

impl Delegated {
    fn refused(text: String) -> Self {
        Self {
            answer: (text, true),
            usage: TurnUsage::default(),
            warnings: Vec::new(),
        }
    }
}

const REPORT: &str = "Work on it with your tools in this project. You cannot hand it on; \
review asks another member to check your work. End with a terse report for the lead: what you \
did, the files you changed, the checks you ran and their results, and what is left. Copy paths, \
names, numbers and error text exactly. No greetings, politeness, preamble or closing.";

/// Runs the task of a `delegate` call as a turn of a new thread, linked to
/// the delegating one, on the member's model.
/// The run is boxed as a future that can be sent between threads, since it
/// runs a turn, which may in turn delegate: the compiler cannot see through
/// that cycle.
pub(crate) fn delegate<'a, 'b: 'a>(
    lead: &'a Lead<'b>,
    arguments: &'a str,
) -> Pin<Box<dyn Future<Output = Delegated> + Send + 'a>> {
    Box::pin(run_delegated(lead, arguments))
}

async fn run_delegated(lead: &Lead<'_>, arguments: &str) -> Delegated {
    let arguments: DelegateArguments = match serde_json::from_str(arguments) {
        Ok(arguments) => arguments,
        Err(error) => return Delegated::refused(format!("delegate needs a task: {error}")),
    };
    if arguments.task.trim().is_empty() {
        return Delegated::refused("delegate needs a task".into());
    }
    let company = &lead.company;
    let member = match &arguments.member {
        Some(name) => match company.named(name) {
            Some(member) => member.clone(),
            None => {
                return Delegated::refused(format!(
                    "Not run: {name} is not on this project's allow list of members."
                ))
            }
        },
        None => {
            let profiles = company.profiles();
            let kind = arguments.kind.as_deref().unwrap_or("general");
            let difficulty = arguments.difficulty.as_deref().unwrap_or("medium");
            match pick(&profiles, kind, difficulty).and_then(|profile| company.by_profile(profile))
            {
                Some(member) => member.clone(),
                None => return Delegated::refused("Not run: no member is allowed here.".into()),
            }
        }
    };
    let estimate = crate::estimate::forecast(lead.store, &arguments.task, &member.profile.model)
        .await
        .ok()
        .and_then(|forecast| forecast.remaining(0, 0).tokens);
    let budget = match budget(arguments.budget_tokens, estimate, company.max_budget) {
        Ok(budget) => budget,
        Err(refusal) => return Delegated::refused(refusal),
    };
    let profile = &member.profile;
    let title: String = format!("{}: {}", profile.name, arguments.task.trim())
        .chars()
        .take(80)
        .collect();
    let child = match start(lead, &member, &title, &arguments.task).await {
        Ok(child) => child,
        Err(error) => {
            return Delegated::refused(format!(
                "Not run: the member's thread could not be made: {}",
                error.message
            ))
        }
    };
    let (_model, main) = watch::channel(member.llm.clone());
    let (_mode, mode) = watch::channel(None);
    let models = crate::turn::Models {
        main,
        reviewer: lead.reviewer.clone(),
        compactor: lead.compactor.clone(),
        explorer: lead.explorer.clone(),
        checker: lead.checker.clone(),
        colleague: lead.colleague.clone(),
        mode,
        company: Some(company.clone()),
        budget: Some(budget),
        title: None,
    };
    let (_steer, steer) = mpsc::unbounded_channel();
    let (events, _) = mpsc::channel(1);
    let prompt = crate::turn::Prompt {
        text: task_message(&lead.request, profile, &arguments),
        ..Default::default()
    };
    let approvals = crate::turn::Approvals { mode: lead.mode };
    let run: Pin<Box<dyn Future<Output = Outcome> + Send + '_>> = Box::pin(crate::turn::run(
        models,
        lead.store,
        child,
        prompt,
        approvals,
        steer,
        lead.cancel.clone(),
        events,
    ));
    let outcome = run.await;
    let report = last_words(lead.store, child).await;
    let (ended, failed) = match &outcome.ending {
        _ if outcome.over_budget => (format!("stopped at its token budget of {budget}"), false),
        crate::turn::Ending::Completed => ("completed".to_string(), false),
        crate::turn::Ending::Length => ("ended at the output limit".to_string(), false),
        crate::turn::Ending::Interrupted => ("interrupted".to_string(), true),
        crate::turn::Ending::Failed(error) => (format!("failed: {}", error.message), true),
    };
    let usage = &outcome.usage;
    let edited = match outcome.edited.is_empty() {
        true => "none named by edit tools".to_string(),
        false => outcome.edited.join(", "),
    };
    let mut text = format!(
        "member {} ({}/{}), thread {}: {ended}\ntokens: input {} (cached {}), output {}, \
         requests {}, budget {budget}\nedited: {edited}",
        profile.name,
        profile.provider,
        profile.model,
        child.0,
        usage.input,
        usage.cached,
        usage.output,
        usage.requests,
    );
    if outcome.over_budget {
        text.push_str(
            "\nThe work may be unfinished; the report below is its last answer before the stop.",
        );
    }
    let reviews = reviews_in(lead.store, child).await;
    if !reviews.is_empty() {
        text.push_str(&format!(
            "\nreviews the member already had done (do not ask for them again):\n{}",
            reviews.join("\n---\n")
        ));
    }
    text.push_str(&format!(
        "\nreport:\n{}",
        report.unwrap_or_else(|| "(no answer)".to_string())
    ));
    let warnings = outcome
        .warnings
        .iter()
        .map(|warning| format!("delegated thread {}: {warning}", child.0))
        .collect();
    Delegated {
        answer: (text, failed),
        usage: TurnUsage {
            requests: usage.requests,
            input: usage.input,
            output: usage.output,
            cached: usage.cached,
            records: Vec::new(),
        },
        warnings,
    }
}

/// Makes the member's thread in the delegating thread's project, on the
/// member's model, linked to the delegating thread.
async fn start(
    lead: &Lead<'_>,
    member: &Member,
    title: &str,
    task: &str,
) -> Result<SessionId, ErrorInfo> {
    let child = lead.store.start_session(lead.project, title).await?;
    let profile = &member.profile;
    let mut chosen = json!({"provider": profile.provider, "model": profile.model});
    if let Some(effort) = &member.effort {
        chosen["effort"] = json!(effort);
    }
    lead.store
        .set_thread_settings(child, chosen.to_string())
        .await?;
    lead.store
        .record_delegation(child, lead.session, &profile.name, task)
        .await?;
    Ok(child)
}

/// The member's first message: the person's request, marked as the
/// person's words, then the lead's task, which is not.
fn task_message(request: &str, profile: &Profile, arguments: &DelegateArguments) -> String {
    let asked: String = request
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect();
    let role = match profile.role.trim() {
        "" => String::new(),
        role => format!(" Your role: {role}"),
    };
    let mut text = format!(
        "Task from the lead agent for you, the member {}.{role}\n{}",
        profile.name,
        arguments.task.trim()
    );
    if !arguments.files.is_empty() {
        text.push_str(&format!("\nFiles: {}", arguments.files.join(", ")));
    }
    text.push_str("\n\n");
    text.push_str(REPORT);
    crate::review::from_command_file(DELEGATE, &asked, &text)
}

/// The answers of the reviews `thread` asked for, oldest first, so the
/// delegating model sees that the work was already reviewed.
async fn reviews_in(store: &Store, thread: SessionId) -> Vec<String> {
    const PAGE: usize = 200;
    let mut reviews = Vec::new();
    let mut before = None;
    loop {
        let Ok(page) = store.page(thread, before, PAGE).await else {
            break;
        };
        let Some(first) = page.first() else {
            break;
        };
        before = Some(first.seq);
        let found: Vec<String> = page
            .iter()
            .filter_map(|stored| match &stored.message.body {
                Body::ToolResult {
                    name,
                    content,
                    is_error: false,
                    ..
                } if name == REVIEW => Some(content.clone()),
                _ => None,
            })
            .collect();
        reviews.splice(0..0, found);
        if page.len() < PAGE {
            break;
        }
    }
    reviews
}

/// The text of the last answer of `thread`.
async fn last_words(store: &Store, thread: SessionId) -> Option<String> {
    let messages = store.page(thread, None, 50).await.ok()?;
    messages
        .iter()
        .rev()
        .find_map(|stored| match &stored.message.body {
            Body::Assistant { parts, .. } => {
                let text: String = parts
                    .iter()
                    .filter_map(|part| match part {
                        AssistantPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let text = text.trim();
                (!text.is_empty()).then(|| text.to_string())
            }
            _ => None,
        })
}

const REVIEW_INSTRUCTIONS: &str = "You review another coding agent's work in a project. Read \
the files with find and read; you cannot change anything. Look for bugs, broken behavior, \
missing error handling, security problems and missing tests. Doubt claims the code does not \
back. Answer with findings only, most serious first, one per line: path:line, the problem, the \
evidence. If you find nothing wrong, answer \"No findings.\" Copy paths, names, numbers and \
error text exactly. No greetings, politeness, preamble or closing.";

const REVIEW_LAST_ROUND: &str = "Stop reading. Answer now with the findings so far, in the \
format asked for.";

const REVIEWER: crate::explore::Helper = crate::explore::Helper {
    system: REVIEW_INSTRUCTIONS,
    last_round: REVIEW_LAST_ROUND,
    name: "the reviewer",
    purpose: REVIEW,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewArguments {
    #[serde(default)]
    member: Option<String>,
    what: String,
    #[serde(default)]
    author: Option<String>,
}

/// Has a member review the work `arguments` describes, done by `author`
/// (the calling model unless a member is named), with `find` and `read`.
pub(crate) async fn review(
    company: &Company,
    author: &Llm,
    context: &Context<'_>,
    arguments: &str,
    usage: &mut TurnUsage,
) -> Answer {
    let arguments: ReviewArguments = match serde_json::from_str(arguments) {
        Ok(arguments) => arguments,
        Err(error) => return (format!("review needs what to review: {error}"), true),
    };
    if arguments.what.trim().is_empty() {
        return ("review needs what to review".into(), true);
    }
    let (author_name, author_vendor) = match &arguments.author {
        Some(name) => match company.named(name) {
            Some(member) => (name.clone(), member.profile.vendor.clone()),
            None => return (format!("{name} is not a member allowed here"), true),
        },
        None => (
            format!("the lead agent ({})", author.model()),
            vendor(author.model(), &author.provider_label()),
        ),
    };
    let reviewer = match &arguments.member {
        Some(name) => match company.named(name) {
            Some(member) => member.clone(),
            None => {
                return (
                    format!("Not run: {name} is not on this project's allow list of members."),
                    true,
                )
            }
        },
        None => {
            let profiles = company.profiles();
            match pick_reviewer(&profiles, &author_vendor)
                .and_then(|profile| company.by_profile(profile))
            {
                Some(member) => member.clone(),
                None => return ("Not run: no member is allowed here.".into(), true),
            }
        }
    };
    let profile = &reviewer.profile;
    let role = match profile.role.trim() {
        "" => String::new(),
        role => format!("Your role: {role}\n\n"),
    };
    let question = format!(
        "{role}Work by {author_name} to review:\n{}",
        arguments.what.trim()
    );
    match crate::explore::read_only(&reviewer.llm, context, &REVIEWER, question, usage).await {
        Ok((findings, _)) => {
            let same = match profile.vendor == author_vendor {
                true => ", the author's vendor too",
                false => "",
            };
            (
                format!(
                    "reviewer {} ({}/{}, vendor {}{same}):\n{findings}",
                    profile.name, profile.provider, profile.model, profile.vendor
                ),
                false,
            )
        }
        Err(error) => (error, true),
    }
}

/// The repository file of the project folder `project`, when it has one.
async fn repository_bytes(project: &str) -> Option<Vec<u8>> {
    tokio::fs::read(Path::new(project).join(REPOSITORY_FILE))
        .await
        .ok()
}

fn invalid(message: impl Into<String>) -> ErrorInfo {
    ErrorInfo::new("invalid_params", message)
}

fn text_param<'a>(params: &'a Value, name: &str) -> Result<&'a str, ErrorInfo> {
    params[name]
        .as_str()
        .ok_or_else(|| invalid(format!("\"{name}\" must be a string")))
}

fn entries_param(value: &Value, name: &str) -> Result<Vec<String>, ErrorInfo> {
    serde_json::from_value(value.clone()).map_err(|_| {
        invalid(format!(
            "\"{name}\" must be a list of member names or provider/model"
        ))
    })
}

impl Core {
    /// The `members`, `allow` and `company` sections.
    async fn company_config(&self) -> Result<Config, ErrorInfo> {
        Config::load(&self.settings_path()?)
            .await
            .map_err(|error| ErrorInfo::new("settings_invalid", error))
    }

    /// The members a turn in the project `project` may call, with their
    /// clients; `None` when none is allowed there.
    pub(crate) async fn company(
        &self,
        settings: &settings::Settings,
        project: Option<&str>,
    ) -> Result<Option<Arc<Company>>, ErrorInfo> {
        let Some(store) = &self.store else {
            return Ok(None);
        };
        let config = self.company_config().await?;
        let repository = match project {
            Some(project) => repository_bytes(project).await,
            None => None,
        };
        let effective = effective(&config.allow, project, repository.as_deref());
        let mut warnings = Vec::new();
        match &effective.repository {
            Repository::NotApproved { .. } => warnings.push(format!(
                "the repository's {REPOSITORY_FILE} is new or changed and not approved, so it \
                 is not used; approve its content with /allow approve"
            )),
            Repository::Invalid { error, .. } => warnings.push(format!(
                "the repository's {REPOSITORY_FILE} is not used: {error}"
            )),
            _ => {}
        }
        let folder = store.files_folder().join("catalog");
        let prices = crate::catalog::kept(&folder, "models-dev");
        let boards = crate::catalog::kept_boards(&folder);
        let mut members = Vec::new();
        for entry in &effective.entries {
            let Some(chosen) = config.resolve(entry) else {
                warnings.push(format!(
                    "the allowed {entry} is no member and no provider/model"
                ));
                continue;
            };
            let choice = crate::Choice {
                provider: Some(chosen.provider.clone()),
                model: Some(chosen.model.clone()),
                effort: chosen.effort.clone(),
                summary: None,
            };
            let llm = match self.client(settings, &choice) {
                Ok(llm) => llm,
                Err(error) => {
                    warnings.push(format!(
                        "the member {} is left out: {}",
                        chosen.name, error.message
                    ));
                    continue;
                }
            };
            let price = match llm.billing() {
                "theos" => None,
                _ => prices
                    .as_ref()
                    .and_then(|prices| crate::catalog::price(prices, &chosen.model))
                    .map(|price| (price.input, price.output)),
            };
            let speed = store.model_speed(&chosen.model).await.unwrap_or_default();
            members.push(Member {
                profile: Profile {
                    vendor: vendor(&chosen.model, &chosen.provider),
                    quality: qualities(&crate::catalog::scores(&boards, &chosen.model)),
                    name: chosen.name,
                    role: chosen.role,
                    provider: chosen.provider,
                    model: chosen.model,
                    price,
                    tokens_per_second: speed.tokens_per_second,
                },
                effort: chosen.effort,
                llm,
            });
        }
        if members.is_empty() && warnings.is_empty() {
            return Ok(None);
        }
        Ok(Some(Arc::new(Company {
            members,
            max_budget: config.max_budget(),
            warnings,
        })))
    }

    /// The members, for `members/list`.
    pub async fn members(&self) -> Result<Value, ErrorInfo> {
        self.store()?;
        let config = self.company_config().await?;
        Ok(json!({ "members": config.members, "max_budget_tokens": config.max_budget() }))
    }

    /// Adds the member `{name, provider, model, effort?, role?}`.
    pub async fn add_member(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let member: MemberConfig =
            serde_json::from_value(params.clone()).map_err(|error| invalid(error.to_string()))?;
        self.check_provider(&member.provider).await?;
        let path = self.writable_settings().await?;
        settings::update(&path, |file| {
            let mut config = Config::from_value(&Value::Object(file.clone()))?;
            if config.member(&member.name).is_some() {
                return Err(format!("a member named {} already exists", member.name));
            }
            config.members.push(member.clone());
            write_members(file, &config)
        })
        .await
        .map_err(invalid)?;
        Ok(json!({ "member": member }))
    }

    /// Changes the fields `params` names of the member `name`; `effort`
    /// `null` returns it to the provider's default.
    pub async fn update_member(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let name = text_param(params, "name")?.to_string();
        let fields = params
            .as_object()
            .ok_or_else(|| invalid("the change is not an object"))?
            .clone();
        if let Some(Value::String(provider)) = fields.get("provider") {
            self.check_provider(provider).await?;
        }
        let path = self.writable_settings().await?;
        let changed = settings::update(&path, |file| {
            let mut config = Config::from_value(&Value::Object(file.clone()))?;
            let index = config
                .members
                .iter()
                .position(|member| member.name == name)
                .ok_or_else(|| format!("no member is named {name}"))?;
            let mut value =
                serde_json::to_value(&config.members[index]).map_err(|e| e.to_string())?;
            for (field, new) in &fields {
                if field == "name" {
                    continue;
                }
                if new.is_null() {
                    value.as_object_mut().expect("an object").remove(field);
                } else {
                    value[field] = new.clone();
                }
            }
            let member: MemberConfig =
                serde_json::from_value(value).map_err(|error| error.to_string())?;
            config.members[index] = member.clone();
            write_members(file, &config)?;
            Ok(member)
        })
        .await
        .map_err(invalid)?;
        Ok(json!({ "member": changed }))
    }

    /// Removes the member `name`; allow lists that name it no longer find it.
    pub async fn remove_member(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let name = text_param(params, "name")?.to_string();
        let path = self.writable_settings().await?;
        settings::update(&path, |file| {
            let mut config = Config::from_value(&Value::Object(file.clone()))?;
            let before = config.members.len();
            config.members.retain(|member| member.name != name);
            if config.members.len() == before {
                return Err(format!("no member is named {name}"));
            }
            write_members(file, &config)
        })
        .await
        .map_err(invalid)?;
        Ok(json!({ "removed": name }))
    }

    async fn check_provider(&self, provider: &str) -> Result<(), ErrorInfo> {
        match self.settings().await?.provider(provider) {
            Some(_) => Ok(()),
            None => Err(invalid(format!("unknown provider {provider}"))),
        }
    }

    /// The allow lists, and for `project` (a project folder) its lists, the
    /// repository file's state and the list in effect.
    pub async fn allow(&self, params: &Value) -> Result<Value, ErrorInfo> {
        self.store()?;
        let config = self.company_config().await?;
        let project = params.get("project").and_then(Value::as_str);
        let repository = match project {
            Some(project) => repository_bytes(project).await,
            None => None,
        };
        let effective = effective(&config.allow, project, repository.as_deref());
        let unknown: Vec<&String> = effective
            .entries
            .iter()
            .filter(|entry| config.resolve(entry).is_none())
            .collect();
        let mut result = json!({
            "default": config.allow.default,
            "effective": effective.entries,
            "source": effective.source,
            "unknown": unknown,
        });
        if let Some(project) = project {
            let content = repository
                .as_deref()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned());
            result["project"] = json!({
                "key": project,
                "list": config.allow.projects.get(project),
                "repository": {
                    "path": Path::new(project).join(REPOSITORY_FILE),
                    "status": effective.repository,
                    "content": content,
                },
            });
        }
        Ok(result)
    }

    /// Sets the default list (`{default: [...]}`) and/or a project's list
    /// (`{project, list: [...] | null}`; `null` removes it).
    pub async fn set_allow(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let default = match params.get("default") {
            Some(value) => Some(entries_param(value, "default")?),
            None => None,
        };
        let project = match params.get("project") {
            Some(_) => {
                let key = text_param(params, "project")?.to_string();
                let list = match params.get("list") {
                    Some(Value::Null) => None,
                    Some(value) => Some(entries_param(value, "list")?),
                    None => {
                        return Err(invalid("give the project's \"list\", or null to remove it"))
                    }
                };
                Some((key, list))
            }
            None => None,
        };
        if default.is_none() && project.is_none() {
            return Err(invalid("give \"default\", or \"project\" and \"list\""));
        }
        let path = self.writable_settings().await?;
        settings::update(&path, |file| {
            let mut config = Config::from_value(&Value::Object(file.clone()))?;
            if let Some(default) = &default {
                config.allow.default = default.clone();
            }
            if let Some((key, list)) = &project {
                match list {
                    Some(list) => {
                        config.allow.projects.insert(key.clone(), list.clone());
                    }
                    None => {
                        config.allow.projects.remove(key);
                    }
                }
            }
            write_allow(file, &config)
        })
        .await
        .map_err(invalid)?;
        let mut shown = json!({});
        if let Some((key, _)) = project {
            shown["project"] = json!(key);
        }
        self.allow(&shown).await
    }

    /// Approves the repository file of `project` as it is now: its SHA-256
    /// is kept, and the file is used while its content stays the same.
    /// `sha256`, when given, must be the content's, so that what is
    /// approved is what the person saw.
    pub async fn approve_repository(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let project = text_param(params, "project")?.to_string();
        let bytes = repository_bytes(&project)
            .await
            .ok_or_else(|| invalid(format!("{} has no {REPOSITORY_FILE}", project)))?;
        let sha256 = sha256(&bytes);
        if let Some(seen) = params.get("sha256").and_then(Value::as_str) {
            if seen != sha256 {
                return Err(ErrorInfo::new(
                    "changed",
                    "the file changed since it was shown; read it again before approving",
                ));
            }
        }
        if let Err(error) = serde_json::from_slice::<RepositoryFile>(&bytes) {
            return Err(invalid(format!(
                "{REPOSITORY_FILE} must be {{\"allow\": [names]}}: {error}"
            )));
        }
        let path = self.writable_settings().await?;
        let approved = sha256.clone();
        settings::update(&path, |file| {
            let mut config = Config::from_value(&Value::Object(file.clone()))?;
            config.allow.repository.insert(project.clone(), approved);
            write_allow(file, &config)
        })
        .await
        .map_err(invalid)?;
        self.allow(&json!({ "project": project })).await
    }

    /// `/members` in `thread`.
    pub(crate) async fn members_command(
        &self,
        thread: SessionId,
        sub: Option<&str>,
        values: &[String],
    ) -> Result<Value, ErrorInfo> {
        let value = |index: usize| values.get(index).map(String::as_str);
        match sub {
            Some("add") => {
                let model = value(1).unwrap_or_default();
                let (provider, model) = match model.split_once('/') {
                    Some((provider, model)) => (provider.to_string(), model.to_string()),
                    None => (self.thread_provider(thread).await?, model.to_string()),
                };
                self.add_member(&json!({
                    "name": value(0), "provider": provider, "model": model,
                    "role": value(2).unwrap_or_default(),
                }))
                .await
            }
            Some("set") => {
                let field = value(1).unwrap_or_default();
                let new = match (field, value(2).unwrap_or_default()) {
                    ("effort", "default") => Value::Null,
                    (_, text) => json!(text),
                };
                let mut patch = json!({ "name": value(0) });
                patch[field] = new;
                self.update_member(&patch).await
            }
            Some("remove") => self.remove_member(&json!({ "name": value(0) })).await,
            _ => {
                let store = self.store()?;
                let project = store.session_project(thread).await?.map(|(_, key)| key);
                let mut shown = self.members().await?;
                shown["allowed"] =
                    self.allow(&json!({ "project": project })).await?["effective"].clone();
                let delegated: Vec<Value> = store
                    .delegations(thread)
                    .await?
                    .into_iter()
                    .map(|(child, member, task, created)| {
                        json!({"thread_id": child.0, "member": member, "task": task, "created": created})
                    })
                    .collect();
                shown["delegated"] = json!(delegated);
                Ok(shown)
            }
        }
    }

    /// `/allow` in `thread`.
    pub(crate) async fn allow_command(
        &self,
        thread: SessionId,
        sub: Option<&str>,
        values: &[String],
    ) -> Result<Value, ErrorInfo> {
        let text = values.first().map(String::as_str).unwrap_or_default();
        let entries: Vec<&str> = text
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|entry| !entry.is_empty())
            .collect();
        let project = self
            .store()?
            .session_project(thread)
            .await?
            .map(|(_, key)| key);
        let needs_project = || {
            project.clone().ok_or_else(|| {
                ErrorInfo::new(
                    "no_project",
                    "this thread has no project folder; start it with a project",
                )
            })
        };
        match sub {
            Some("default") => {
                let entries: Vec<&str> = match entries.as_slice() {
                    ["none"] => Vec::new(),
                    _ => entries,
                };
                self.set_allow(&json!({ "default": entries })).await?;
                self.allow(&json!({ "project": project })).await
            }
            Some("project") => {
                let list = match entries.as_slice() {
                    ["default"] => Value::Null,
                    _ => json!(entries),
                };
                self.set_allow(&json!({ "project": needs_project()?, "list": list }))
                    .await
            }
            Some("approve") => {
                self.approve_repository(&json!({ "project": needs_project()? }))
                    .await
            }
            _ => self.allow(&json!({ "project": project })).await,
        }
    }
}

fn write_members(file: &mut serde_json::Map<String, Value>, config: &Config) -> Result<(), String> {
    let members = serde_json::to_value(&config.members).map_err(|error| error.to_string())?;
    file.insert("members".into(), members);
    Config::from_value(&Value::Object(file.clone())).map(drop)
}

fn write_allow(file: &mut serde_json::Map<String, Value>, config: &Config) -> Result<(), String> {
    let allow = serde_json::to_value(&config.allow).map_err(|error| error.to_string())?;
    file.insert("allow".into(), allow);
    Config::from_value(&Value::Object(file.clone())).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate::Range;

    fn profile(
        name: &str,
        vendor: &str,
        price: Option<(f64, f64)>,
        coding: Option<f64>,
    ) -> Profile {
        Profile {
            name: name.into(),
            role: String::new(),
            provider: "p".into(),
            model: name.into(),
            vendor: vendor.into(),
            price,
            quality: coding
                .map(|quality| ("coding", quality))
                .into_iter()
                .collect(),
            tokens_per_second: None,
        }
    }

    fn allow(default: &[&str]) -> AllowConfig {
        AllowConfig {
            default: default.iter().map(|entry| entry.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_project_list_replaces_the_default_and_the_repository_needs_its_exact_content_approved() {
        let mut config = allow(&["cheap", "claude/claude-haiku-5-5"]);
        assert_eq!(
            effective(&config, Some("/p"), None),
            Effective {
                entries: vec!["cheap".into(), "claude/claude-haiku-5-5".into()],
                source: "default",
                repository: Repository::Absent,
            }
        );
        // The repository's file is not used before its content is approved.
        let file = br#"{"allow": ["glm", "cheap"]}"#;
        let found = effective(&config, Some("/p"), Some(file));
        assert_eq!(found.source, "default");
        assert_eq!(
            found.repository,
            Repository::NotApproved {
                sha256: sha256(file)
            }
        );
        config.allow_repository("/p", file);
        let found = effective(&config, Some("/p"), Some(file));
        assert_eq!(
            (found.source, found.entries.clone()),
            ("project", vec!["glm".into(), "cheap".into()])
        );
        // Changed content asks again.
        let changed = br#"{"allow": ["glm", "cheap", "expensive"]}"#;
        assert!(matches!(
            effective(&config, Some("/p"), Some(changed)).repository,
            Repository::NotApproved { .. }
        ));
        // The person's list for the folder comes first, then the file's.
        config
            .projects
            .insert("/p".into(), vec!["local".into(), "glm".into()]);
        assert_eq!(
            effective(&config, Some("/p"), Some(file)).entries,
            ["local", "glm", "cheap"]
        );
        // An empty list for a folder allows nothing there.
        config.projects.insert("/q".into(), Vec::new());
        assert!(effective(&config, Some("/q"), None).entries.is_empty());
        // Another folder keeps the default.
        assert_eq!(effective(&config, Some("/r"), Some(file)).source, "default");
        // Approved content that is not a list is not used.
        let broken = br#"["glm"]"#;
        config.allow_repository("/s", broken);
        assert!(matches!(
            effective(&config, Some("/s"), Some(broken)).repository,
            Repository::Invalid { .. }
        ));
    }

    impl AllowConfig {
        fn allow_repository(&mut self, project: &str, bytes: &[u8]) {
            self.repository.insert(project.into(), sha256(bytes));
        }
    }

    #[test]
    fn entries_name_members_or_provider_models() {
        let config = Config::from_value(&json!({
            "members": [{"name": "glm", "provider": "zai", "model": "glm-5.3", "role": "strict reviewer"}]
        }))
        .unwrap();
        assert_eq!(config.resolve("glm").unwrap().role, "strict reviewer");
        let plain = config.resolve("claude/claude-haiku-5-5").unwrap();
        assert_eq!(
            (plain.provider.as_str(), plain.model.as_str()),
            ("claude", "claude-haiku-5-5")
        );
        assert!(config.resolve("nobody").is_none());
        assert!(Config::from_value(&json!({"members": [
            {"name": "a", "provider": "p", "model": "m"},
            {"name": "a", "provider": "p", "model": "n"}
        ]}))
        .is_err());
        assert!(Config::from_value(
            &json!({"members": [{"name": "a b", "provider": "p", "model": "m"}]})
        )
        .is_err());
        assert!(Config::from_value(
            &json!({"members": [{"name": "a", "provider": "p", "model": "m", "effort": "huge"}]})
        )
        .is_err());
    }

    #[test]
    fn the_cheapest_member_good_enough_for_the_difficulty_is_picked() {
        let profiles = vec![
            profile("big", "anthropic", Some((5.0, 25.0)), Some(0.95)),
            profile("mid", "openai", Some((1.0, 4.0)), Some(0.6)),
            profile("small", "google", Some((0.1, 0.4)), Some(0.2)),
        ];
        assert_eq!(pick(&profiles, "coding", "easy").unwrap().name, "small");
        assert_eq!(pick(&profiles, "coding", "medium").unwrap().name, "mid");
        assert_eq!(pick(&profiles, "coding", "hard").unwrap().name, "big");
        // None reaches the threshold: the best is taken.
        let weak = vec![
            profile("a", "x", Some((1.0, 1.0)), Some(0.3)),
            profile("b", "y", Some((9.0, 9.0)), Some(0.4)),
        ];
        assert_eq!(pick(&weak, "coding", "hard").unwrap().name, "b");
        // No score: the general one, else 0.5; no price: the median price.
        let unknown = vec![
            profile("known", "x", Some((1.0, 1.0)), Some(0.6)),
            profile("unknown", "y", None, None),
            profile("cheap", "z", Some((0.5, 0.5)), Some(0.1)),
        ];
        assert_eq!(pick(&unknown, "coding", "medium").unwrap().name, "unknown");
        assert!(pick(&[], "coding", "easy").is_none());
    }

    #[test]
    fn a_reviewer_of_another_vendor_with_a_strict_role_comes_first() {
        let mut strict_other = profile("glm", "zai", Some((0.6, 2.2)), Some(0.5));
        strict_other.role = "常に他社のコードを疑うレビュー役".into();
        let other = profile("gpt", "openai", Some((1.0, 4.0)), Some(0.9));
        let mut strict_same = profile("opus", "anthropic", Some((5.0, 25.0)), Some(0.99));
        strict_same.role = "strict reviewer".into();
        let same = profile("haiku", "anthropic", Some((1.0, 5.0)), Some(0.7));
        let all = vec![
            same.clone(),
            strict_same.clone(),
            other.clone(),
            strict_other.clone(),
        ];
        assert_eq!(pick_reviewer(&all, "anthropic").unwrap().name, "glm");
        let without = vec![same.clone(), strict_same.clone(), other];
        assert_eq!(pick_reviewer(&without, "anthropic").unwrap().name, "gpt");
        let same_only = vec![same.clone(), strict_same];
        assert_eq!(pick_reviewer(&same_only, "anthropic").unwrap().name, "opus");
        assert_eq!(pick_reviewer(&[same], "anthropic").unwrap().name, "haiku");
    }

    #[test]
    fn delegated_tasks_run_together_only_when_independent_and_apart() {
        let call = |independent: bool, files: &[&str]| {
            (
                independent,
                files
                    .iter()
                    .map(|file| file.to_string())
                    .collect::<Vec<_>>(),
            )
        };
        assert!(together(&[call(true, &["a.rs"]), call(true, &["b.rs"])]));
        assert!(!together(&[call(true, &["a.rs"])]));
        assert!(!together(&[call(true, &["a.rs"]), call(false, &["b.rs"])]));
        assert!(!together(&[call(true, &["a.rs"]), call(true, &[])]));
        assert!(!together(&[
            call(true, &["src/a.rs"]),
            call(true, &["./SRC\\a.rs"])
        ]));
        assert!(!together(&[
            call(true, &["src"]),
            call(true, &["src/b.rs"])
        ]));
        assert!(together(&[
            call(true, &["src/a.rs"]),
            call(true, &["src/ab.rs"]),
            call(true, &["docs"])
        ]));
        assert_eq!(
            plan(r#"{"task": "t", "independent": true, "files": ["x"]}"#),
            call(true, &["x"])
        );
        assert_eq!(plan("not json"), call(false, &[]));
    }

    #[test]
    fn the_budget_defaults_to_the_estimate_and_never_passes_the_maximum() {
        let range = |low, high| Range {
            low,
            mid: low,
            high,
        };
        assert_eq!(budget(None, None, 1_000), Ok(1_000));
        assert_eq!(budget(None, Some(range(100, 400)), 1_000), Ok(400));
        assert_eq!(budget(None, Some(range(100, 4_000)), 1_000), Ok(1_000));
        assert_eq!(budget(Some(500), Some(range(100, 4_000)), 1_000), Ok(500));
        assert!(budget(Some(2_000), None, 1_000)
            .unwrap_err()
            .contains("maximum of 1000"));
        assert!(budget(Some(50), Some(range(100, 400)), 1_000)
            .unwrap_err()
            .contains("at least 100 tokens"));
        assert!(budget(Some(0), None, 1_000).is_err());
    }

    #[test]
    fn qualities_are_the_mean_rank_share_per_use() {
        let score = |use_: &'static str, rank: usize, of: usize| crate::catalog::Score {
            use_,
            metric: "m".into(),
            value: 1.0,
            unit: String::new(),
            rank,
            of,
            observed_at: None,
            source_url: None,
        };
        let found = qualities(&[
            score("coding", 1, 11),
            score("coding", 11, 11),
            score("general", 1, 1),
        ]);
        assert_eq!(found, [("coding", 0.5), ("general", 1.0)]);
        assert_eq!(vendor("glm-5.3", "zai-key"), "zai");
        assert_eq!(vendor("my-local", "ollama"), "ollama");
    }
}
