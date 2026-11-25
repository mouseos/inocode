//! Estimates of the tokens and minutes a piece of work takes, from the
//! tasks this installation measured: each turn, and each goal as a whole,
//! is recorded when it ends; before a task the similar past tasks give a
//! range, and during it the past tasks that had used at least as much give
//! the remaining range. Design, sources and limits:
//! docs/evidence/effort-estimates.md.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::store::{store_error, SessionId, Store};
use crate::{Core, ErrorInfo};

/// Similar past tasks an estimate is made from.
const NEIGHBORS: usize = 20;
/// Below this many samples the 10th–90th percentiles mean little, and the
/// range is widened instead.
const FEW: usize = 10;
/// The widening: half the smallest to twice the largest. Bai et al. found
/// the costliest of four runs of one task used about twice the cheapest.
const WIDEN: f64 = 2.0;
/// Bonus to the text similarity for the same task type and model family.
const SAME_TYPE: f64 = 0.2;
const SAME_FAMILY: f64 = 0.1;
/// Past tasks read for an estimate, the latest first.
const HISTORY: usize = 2_000;
/// Outcomes whose use shows what the whole work took.
const FINISHED: [&str; 2] = ["completed", "achieved"];

/// Tool calls of a task by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCounts {
    pub read: u64,
    pub edit: u64,
    /// Shell commands that run tests or checks.
    pub test: u64,
    pub shell: u64,
    pub fetch: u64,
    pub other: u64,
}

impl ToolCounts {
    fn add(&mut self, other: &ToolCounts) {
        self.read += other.read;
        self.edit += other.edit;
        self.test += other.test;
        self.shell += other.shell;
        self.fetch += other.fetch;
        self.other += other.other;
    }
}

/// One finished task as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Task {
    pub started_ms: i64,
    pub session: Option<i64>,
    /// `turn` or `goal`.
    pub kind: String,
    /// The goal a turn worked on, or the goal itself: its first message.
    pub goal_seq: Option<i64>,
    /// The person's words: the turn's prompt, or the goal's objective.
    pub request: String,
    pub task_type: String,
    pub provider: String,
    pub model: String,
    pub effort: Option<String>,
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    pub requests: u64,
    pub wall_ms: u64,
    /// Time spent running tool calls.
    pub tool_ms: u64,
    pub tools: ToolCounts,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub files_changed: Option<u64>,
    /// `git` when the changes come from the repository, `tools` when they
    /// are the files named by edit and write calls (no line counts).
    pub changes_from: String,
    /// `completed`, `length`, `interrupted`, `failed`; for a goal
    /// `running`, `achieved` or `abandoned`.
    pub outcome: String,
}

/// What an estimate uses of a past task.
#[derive(Debug, Clone, PartialEq)]
pub struct Past {
    pub request: String,
    pub task_type: String,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub requests: u64,
    pub wall_ms: u64,
    pub tool_ms: u64,
}

impl Past {
    fn tokens(&self) -> u64 {
        self.input + self.output
    }
}

/// The measured speed of a model (docs/evidence/usage-log.md).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Speed {
    pub tokens_per_second: Option<f64>,
    pub first_output_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Range<T> {
    pub low: T,
    pub mid: T,
    pub high: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SoFar {
    pub tokens: u64,
    pub minutes: f64,
}

/// A range of what a task takes, or what is left of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Estimate {
    pub tokens: Option<Range<u64>>,
    pub minutes: Option<Range<f64>>,
    /// The past tasks the range comes from.
    pub samples: usize,
    /// Whether the range was widened for having few samples.
    pub widened: bool,
    /// `similar`, `all` (nothing similar: every past task), `beyond`
    /// (more used than by every similar past task) or `none` (no past
    /// task).
    pub method: &'static str,
    /// Used so far, when the estimate is of what is left.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub so_far: Option<SoFar>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The similar past tasks of one request, ready to estimate from at any
/// point of its work without reading the store again.
#[derive(Debug, Clone, PartialEq)]
pub struct Forecast {
    /// Tokens and seconds of each similar past task on the current model.
    neighbors: Vec<(f64, f64)>,
    basis: &'static str,
}

impl Forecast {
    pub fn new(past: &[Past], text: &str, model: &str, speed: Speed) -> Self {
        let chosen = similar(past, text, model);
        let basis = match chosen.is_empty() {
            true => "all",
            false => "similar",
        };
        let picked: Vec<&Past> = match chosen.is_empty() {
            true => past.iter().collect(),
            false => chosen.iter().map(|&index| &past[index]).collect(),
        };
        let neighbors = picked
            .into_iter()
            .map(|task| (task.tokens() as f64, seconds(task, speed)))
            .collect();
        Self { neighbors, basis }
    }

    pub fn has_history(&self) -> bool {
        !self.neighbors.is_empty()
    }

    /// The range of what is left after `tokens` were used in `elapsed_ms`;
    /// with nothing used, of the whole task. The past tasks that had used
    /// more than `tokens` are the ones still going at this point; what they
    /// used beyond it is the range.
    pub fn remaining(&self, tokens: u64, elapsed_ms: u64) -> Estimate {
        let so_far = (tokens > 0).then(|| SoFar {
            tokens,
            minutes: round(elapsed_ms as f64 / 60_000.0),
        });
        if self.neighbors.is_empty() {
            return Estimate {
                tokens: None,
                minutes: None,
                samples: 0,
                widened: false,
                method: "none",
                so_far,
                note: Some("no finished past task to estimate from yet".into()),
            };
        }
        let used = tokens as f64;
        let going: Vec<(f64, f64)> = match tokens {
            0 => self.neighbors.clone(),
            _ => self
                .neighbors
                .iter()
                .filter(|(total, _)| *total > used)
                .map(|&(total, seconds)| (total - used, seconds * (total - used) / total))
                .collect(),
        };
        if going.is_empty() {
            // Gott's rule for something seen only part-way: with 80%
            // confidence what is left is 1/9 to 9 times what has passed.
            let minutes = elapsed_ms as f64 / 60_000.0;
            return Estimate {
                tokens: Some(Range {
                    low: (used / 9.0) as u64,
                    mid: tokens,
                    high: (used * 9.0) as u64,
                }),
                minutes: Some(Range {
                    low: round(minutes / 9.0),
                    mid: round(minutes),
                    high: round(minutes * 9.0),
                }),
                samples: 0,
                widened: true,
                method: "beyond",
                so_far,
                note: Some(format!(
                    "more was used than by any of the {} similar past tasks: the range of what \
                     is left is 1/9 to 9 times what was used so far",
                    self.neighbors.len()
                )),
            };
        }
        let (tokens_range, widened) = spread(going.iter().map(|(tokens, _)| *tokens).collect());
        let (minutes_range, _) = spread(going.iter().map(|(_, seconds)| seconds / 60.0).collect());
        let note = widened.then(|| {
            format!(
                "only {} past task{} to go by: the range is widened to half the smallest and \
                 twice the largest",
                going.len(),
                if going.len() == 1 { "" } else { "s" }
            )
        });
        Estimate {
            tokens: Some(Range {
                low: tokens_range.low.round() as u64,
                mid: tokens_range.mid.round() as u64,
                high: tokens_range.high.round() as u64,
            }),
            minutes: Some(Range {
                low: round(minutes_range.low),
                mid: round(minutes_range.mid),
                high: round(minutes_range.high),
            }),
            samples: going.len(),
            widened,
            method: self.basis,
            so_far,
            note,
        }
    }
}

/// Minutes to the hundredth, so a task of seconds does not read as none.
fn round(minutes: f64) -> f64 {
    (minutes * 100.0).round() / 100.0
}

/// How long `task` would take on a model of `speed`: its output at that
/// speed, the wait for each answer to start, and its own tool time; its
/// measured wall time when the speed is not known.
fn seconds(task: &Past, speed: Speed) -> f64 {
    match speed.tokens_per_second {
        Some(rate) if rate > 0.0 => {
            let waits = speed.first_output_ms.unwrap_or(0) as f64 * task.requests as f64;
            task.output as f64 / rate + (waits + task.tool_ms as f64) / 1000.0
        }
        _ => task.wall_ms as f64 / 1000.0,
    }
}

/// The 10th, 50th and 90th percentiles of `values`; with fewer than `FEW`,
/// half the smallest, the median and twice the largest (`true`).
fn spread(mut values: Vec<f64>) -> (Range<f64>, bool) {
    values.sort_by(f64::total_cmp);
    if values.len() >= FEW {
        let range = Range {
            low: quantile(&values, 0.1),
            mid: quantile(&values, 0.5),
            high: quantile(&values, 0.9),
        };
        return (range, false);
    }
    let range = Range {
        low: values[0] / WIDEN,
        mid: quantile(&values, 0.5),
        high: values[values.len() - 1] * WIDEN,
    };
    (range, true)
}

/// The `q` quantile of sorted, non-empty `values`, interpolated between
/// the two nearest.
fn quantile(values: &[f64], q: f64) -> f64 {
    let position = q * (values.len() - 1) as f64;
    let below = position.floor() as usize;
    let above = position.ceil() as usize;
    values[below] + (values[above] - values[below]) * (position - below as f64)
}

/// The indexes of up to `NEIGHBORS` past tasks most like `text`: TF-IDF
/// cosine of the request texts, plus a bonus for the same task type and
/// model family. Empty when none shares a word, a type or a family.
fn similar(past: &[Past], text: &str, model: &str) -> Vec<usize> {
    let query = terms(text);
    let documents: Vec<Vec<String>> = past.iter().map(|task| terms(&task.request)).collect();
    let cosines = cosines(&documents, &query);
    let kind = task_type(text);
    let wanted = family(model);
    let mut scored: Vec<(usize, f64)> = past
        .iter()
        .zip(cosines)
        .enumerate()
        .map(|(index, (task, cosine))| {
            let mut score = cosine;
            if kind != OTHER && task.task_type == kind {
                score += SAME_TYPE;
            }
            if !wanted.is_empty() && family(&task.model) == wanted {
                score += SAME_FAMILY;
            }
            (index, score)
        })
        .filter(|(_, score)| *score > 0.0)
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.truncate(NEIGHBORS);
    scored.into_iter().map(|(index, _)| index).collect()
}

/// Cosine similarity of `query` to each document, by TF-IDF weights with
/// the smoothed IDF `ln((n + 1) / (df + 1)) + 1` over the documents and the
/// query.
fn cosines(documents: &[Vec<String>], query: &[String]) -> Vec<f64> {
    let mut frequency: HashMap<&str, f64> = HashMap::new();
    for terms in documents.iter().map(Vec::as_slice).chain([query]) {
        let unique: BTreeSet<&str> = terms.iter().map(String::as_str).collect();
        for term in unique {
            *frequency.entry(term).or_default() += 1.0;
        }
    }
    let total = documents.len() as f64 + 1.0;
    let weights = |terms: &[String]| -> HashMap<String, f64> {
        let mut counts: HashMap<String, f64> = HashMap::new();
        for term in terms {
            *counts.entry(term.clone()).or_default() += 1.0;
        }
        counts
            .into_iter()
            .map(|(term, count)| {
                let df = frequency.get(term.as_str()).copied().unwrap_or(1.0);
                let idf = ((total + 1.0) / (df + 1.0)).ln() + 1.0;
                (term, count * idf)
            })
            .collect()
    };
    let query = weights(query);
    let query_norm = query.values().map(|w| w * w).sum::<f64>().sqrt();
    documents
        .iter()
        .map(|terms| {
            let document = weights(terms);
            let norm = document.values().map(|w| w * w).sum::<f64>().sqrt();
            if norm == 0.0 || query_norm == 0.0 {
                return 0.0;
            }
            let dot: f64 = query
                .iter()
                .filter_map(|(term, weight)| document.get(term).map(|other| weight * other))
                .sum();
            dot / (norm * query_norm)
        })
        .collect()
}

fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30FF}' | '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}' | '\u{AC00}'..='\u{D7AF}' | '\u{FF66}'..='\u{FF9F}')
}

/// The terms of a text: lower-cased words of two or more letters or
/// digits, and for Chinese, Japanese and Korean script, which has no
/// spaces, the pairs of neighboring characters (a lone one by itself).
fn terms(text: &str) -> Vec<String> {
    fn flush_word(word: &mut String, terms: &mut Vec<String>) {
        if word.chars().count() >= 2 {
            terms.push(std::mem::take(word));
        }
        word.clear();
    }
    fn flush_run(run: &mut Vec<char>, terms: &mut Vec<String>) {
        match run.len() {
            0 => {}
            1 => terms.push(run[0].to_string()),
            _ => terms.extend(run.windows(2).map(|pair| pair.iter().collect::<String>())),
        }
        run.clear();
    }
    let mut terms = Vec::new();
    let mut word = String::new();
    let mut run = Vec::new();
    for c in text.to_lowercase().chars() {
        if is_cjk(c) {
            flush_word(&mut word, &mut terms);
            run.push(c);
        } else if c.is_alphanumeric() {
            flush_run(&mut run, &mut terms);
            word.push(c);
        } else {
            flush_word(&mut word, &mut terms);
            flush_run(&mut run, &mut terms);
        }
    }
    flush_word(&mut word, &mut terms);
    flush_run(&mut run, &mut terms);
    terms
}

/// The model's family: its name without the parts holding digits, so
/// `claude-haiku-5-5` and `claude-haiku-4-5` are both `claude-haiku`.
fn family(model: &str) -> String {
    let model = model.rsplit('/').next().unwrap_or(model).to_lowercase();
    model
        .split(['-', '_', '.', ':'])
        .filter(|part| !part.is_empty() && !part.chars().any(|c| c.is_ascii_digit()))
        .collect::<Vec<_>>()
        .join("-")
}

const OTHER: &str = "other";

/// Task types by their words, the first that matches winning: English
/// words, and phrases of Chinese, Japanese and Korean script found
/// anywhere in the text.
const TYPES: &[(&str, &[&str], &[&str])] = &[
    (
        "fix",
        &[
            "fix",
            "fixes",
            "fixed",
            "fixing",
            "bug",
            "bugs",
            "error",
            "errors",
            "fail",
            "fails",
            "failing",
            "failure",
            "crash",
            "crashes",
            "broken",
            "regression",
            "debug",
        ],
        &["直", "修正", "不具合", "バグ", "エラー", "失敗", "誤り"],
    ),
    (
        "test",
        &["test", "tests", "testing", "coverage"],
        &["テスト", "試験"],
    ),
    (
        "review",
        &["review", "reviews", "audit", "inspect"],
        &["レビュー", "点検", "審査", "見直"],
    ),
    (
        "refactor",
        &[
            "refactor",
            "refactoring",
            "rename",
            "cleanup",
            "simplify",
            "reorganize",
        ],
        &["リファクタ", "整理", "改名"],
    ),
    (
        "docs",
        &[
            "doc",
            "docs",
            "document",
            "documentation",
            "readme",
            "changelog",
        ],
        &["文書", "ドキュメント", "説明書"],
    ),
    (
        "implement",
        &[
            "implement",
            "implementation",
            "add",
            "adds",
            "create",
            "build",
            "write",
            "support",
            "feature",
            "port",
            "make",
        ],
        &["実装", "追加", "作", "対応", "機能", "導入"],
    ),
    (
        "question",
        &[
            "what",
            "why",
            "how",
            "where",
            "which",
            "explain",
            "describe",
            "investigate",
            "research",
            "summarize",
            "compare",
        ],
        &[
            "説明", "教え", "なぜ", "何", "どう", "調べ", "調査", "比較", "?", "？",
        ],
    ),
];

/// The type of a task from its words alone, so a request is typed the same
/// before and after it runs; `other` when no word matches.
pub fn task_type(text: &str) -> &'static str {
    let lower = text.to_lowercase();
    let words: BTreeSet<&str> = lower
        .split(|c: char| !c.is_alphanumeric() || is_cjk(c))
        .filter(|word| !word.is_empty())
        .collect();
    TYPES
        .iter()
        .find(|(_, english, phrases)| {
            english.iter().any(|word| words.contains(word))
                || phrases.iter().any(|phrase| lower.contains(phrase))
        })
        .map_or(OTHER, |(kind, _, _)| kind)
}

/// The type recorded for a finished task: its words' type, or for `other`
/// what its tools show, `change` when it edited a file, else `question`.
pub(crate) fn recorded_type(text: &str, tools: &ToolCounts) -> &'static str {
    match task_type(text) {
        OTHER if tools.edit > 0 => "change",
        OTHER => "question",
        kind => kind,
    }
}

/// Words of a shell command that make it a test or check.
const TEST_WORDS: &[&str] = &[
    "test", "tests", "pytest", "jest", "vitest", "mocha", "rspec", "ctest", "nextest", "phpunit",
    "unittest", "tox", "clippy", "check",
];

/// The tool calls, tool time and edited files of a task.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Tally {
    pub counts: ToolCounts,
    pub tool_ms: u64,
    pub edited: BTreeSet<String>,
}

impl Tally {
    /// Counts a call of `name` with `arguments` that ran for `took`.
    pub(crate) fn add(&mut self, name: &str, arguments: &str, took: Duration) {
        self.tool_ms += took.as_millis() as u64;
        let parsed: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
        let counts = &mut self.counts;
        match name {
            crate::read::READ
            | crate::find::FIND
            | crate::agent_tools::RECALL
            | crate::explore::EXPLORE => counts.read += 1,
            crate::edit::EDIT | crate::edit::WRITE => {
                counts.edit += 1;
                if let Some(path) = parsed["path"].as_str() {
                    self.edited.insert(path.to_string());
                }
            }
            crate::edit::EDITS => {
                counts.edit += 1;
                for edit in parsed["edits"].as_array().into_iter().flatten() {
                    if let Some(path) = edit["path"].as_str() {
                        self.edited.insert(path.to_string());
                    }
                }
            }
            crate::shell::SHELL => {
                let command = parsed["command"]
                    .as_str()
                    .unwrap_or_default()
                    .to_lowercase();
                let testing = command
                    .split(|c: char| !c.is_alphanumeric())
                    .any(|word| TEST_WORDS.contains(&word));
                match testing {
                    true => counts.test += 1,
                    false => counts.shell += 1,
                }
            }
            crate::shell::JOB => counts.shell += 1,
            crate::fetch::FETCH | crate::web_search::SEARCH => counts.fetch += 1,
            _ => counts.other += 1,
        }
    }
}

const TASK_COLUMNS: &str = "started, session_id, kind, goal_seq, request, task_type, provider, \
    model, effort, input, output, cached, requests, wall_ms, tool_ms, tools, lines_added, \
    lines_removed, files_changed, changes_from, outcome";

fn task_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    let number =
        |index: usize| -> rusqlite::Result<u64> { Ok(row.get::<_, i64>(index)?.max(0) as u64) };
    let maybe = |index: usize| -> rusqlite::Result<Option<u64>> {
        Ok(row
            .get::<_, Option<i64>>(index)?
            .map(|value| value.max(0) as u64))
    };
    let tools: String = row.get(15)?;
    Ok(Task {
        started_ms: row.get(0)?,
        session: row.get(1)?,
        kind: row.get(2)?,
        goal_seq: row.get(3)?,
        request: row.get(4)?,
        task_type: row.get(5)?,
        provider: row.get(6)?,
        model: row.get(7)?,
        effort: row.get(8)?,
        input: number(9)?,
        output: number(10)?,
        cached: number(11)?,
        requests: number(12)?,
        wall_ms: number(13)?,
        tool_ms: number(14)?,
        tools: serde_json::from_str(&tools).unwrap_or_default(),
        lines_added: maybe(16)?,
        lines_removed: maybe(17)?,
        files_changed: maybe(18)?,
        changes_from: row.get(19)?,
        outcome: row.get(20)?,
    })
}

fn insert_task(db: &rusqlite::Connection, task: &Task) -> Result<(), ErrorInfo> {
    let tools = serde_json::to_string(&task.tools)
        .map_err(|error| ErrorInfo::new("internal", error.to_string()))?;
    db.execute(
        &format!(
            "INSERT INTO tasks({TASK_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
             ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)"
        ),
        params![
            task.started_ms,
            task.session,
            task.kind,
            task.goal_seq,
            task.request,
            task.task_type,
            task.provider,
            task.model,
            task.effort,
            task.input as i64,
            task.output as i64,
            task.cached as i64,
            task.requests as i64,
            task.wall_ms as i64,
            task.tool_ms as i64,
            tools,
            task.lines_added.map(|value| value as i64),
            task.lines_removed.map(|value| value as i64),
            task.files_changed.map(|value| value as i64),
            task.changes_from,
            task.outcome,
        ],
    )
    .map_err(store_error)?;
    Ok(())
}

fn sum(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    Some(a? + b?)
}

impl Store {
    /// Records a finished task.
    pub async fn record_task(&self, task: &Task) -> Result<(), ErrorInfo> {
        let task = task.clone();
        self.write(move |db| insert_task(db, &task)).await
    }

    /// Adds a turn that worked on the goal whose work starts at `goal_seq`
    /// to that goal's own record, with the goal's `outcome` after it.
    pub(crate) async fn add_to_goal_task(
        &self,
        turn: &Task,
        goal_seq: i64,
        objective: String,
        outcome: &'static str,
    ) -> Result<(), ErrorInfo> {
        let turn = turn.clone();
        self.write(move |db| {
            let tx = db.transaction().map_err(store_error)?;
            let found = tx
                .query_row(
                    &format!(
                        "SELECT {TASK_COLUMNS}, id FROM tasks
                         WHERE kind = 'goal' AND session_id = ?1 AND goal_seq = ?2"
                    ),
                    params![turn.session, goal_seq],
                    |row| Ok((row.get::<_, i64>(21)?, task_row(row)?)),
                )
                .optional()
                .map_err(store_error)?;
            let mut goal = match found {
                Some((id, goal)) => {
                    tx.execute("DELETE FROM tasks WHERE id = ?1", [id])
                        .map_err(store_error)?;
                    let mut tools = goal.tools;
                    tools.add(&turn.tools);
                    Task {
                        provider: turn.provider.clone(),
                        model: turn.model.clone(),
                        effort: turn.effort.clone(),
                        input: goal.input + turn.input,
                        output: goal.output + turn.output,
                        cached: goal.cached + turn.cached,
                        requests: goal.requests + turn.requests,
                        wall_ms: goal.wall_ms + turn.wall_ms,
                        tool_ms: goal.tool_ms + turn.tool_ms,
                        tools,
                        lines_added: sum(goal.lines_added, turn.lines_added),
                        lines_removed: sum(goal.lines_removed, turn.lines_removed),
                        files_changed: sum(goal.files_changed, turn.files_changed),
                        changes_from: match goal.changes_from == turn.changes_from {
                            true => goal.changes_from,
                            false => "tools".to_string(),
                        },
                        ..goal
                    }
                }
                None => Task {
                    kind: "goal".into(),
                    goal_seq: Some(goal_seq),
                    request: objective.clone(),
                    ..turn.clone()
                },
            };
            goal.task_type = recorded_type(&goal.request, &goal.tools).to_string();
            goal.outcome = outcome.to_string();
            insert_task(&tx, &goal)?;
            tx.commit().map_err(store_error)
        })
        .await
    }

    /// Marks the goal record of the goal whose work starts at `goal_seq`
    /// with `outcome`, if it is still running.
    pub(crate) async fn end_goal_task(
        &self,
        session: SessionId,
        goal_seq: i64,
        outcome: &'static str,
    ) -> Result<(), ErrorInfo> {
        self.write(move |db| {
            db.execute(
                "UPDATE tasks SET outcome = ?3
                 WHERE kind = 'goal' AND session_id = ?1 AND goal_seq = ?2 AND outcome = 'running'",
                params![session.0, goal_seq, outcome],
            )
            .map(|_| ())
            .map_err(store_error)
        })
        .await
    }

    /// The tokens recorded so far for the goal whose work starts at
    /// `goal_seq`.
    pub(crate) async fn goal_task_tokens(
        &self,
        session: SessionId,
        goal_seq: i64,
    ) -> Result<u64, ErrorInfo> {
        self.read(Duration::from_secs(5), move |db| {
            db.query_row(
                "SELECT input + output FROM tasks
                 WHERE kind = 'goal' AND session_id = ?1 AND goal_seq = ?2",
                params![session.0, goal_seq],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map(|tokens| tokens.unwrap_or(0).max(0) as u64)
            .map_err(store_error)
        })
        .await
    }

    /// The finished tasks an estimate goes by, the latest first: turns
    /// outside goals and goals as wholes, that completed or were achieved.
    pub async fn past_tasks(&self) -> Result<Vec<Past>, ErrorInfo> {
        self.read(Duration::from_secs(10), move |db| {
            let mut statement = db
                .prepare_cached(&format!(
                    "SELECT request, task_type, model, input, output, requests, wall_ms, tool_ms
                     FROM tasks
                     WHERE outcome IN ('{}', '{}')
                       AND NOT (kind = 'turn' AND goal_seq IS NOT NULL)
                     ORDER BY started DESC, id DESC LIMIT {HISTORY}",
                    FINISHED[0], FINISHED[1]
                ))
                .map_err(store_error)?;
            let rows = statement
                .query_map([], |row| {
                    let number = |index: usize| -> rusqlite::Result<u64> {
                        Ok(row.get::<_, i64>(index)?.max(0) as u64)
                    };
                    Ok(Past {
                        request: row.get(0)?,
                        task_type: row.get(1)?,
                        model: row.get(2)?,
                        input: number(3)?,
                        output: number(4)?,
                        requests: number(5)?,
                        wall_ms: number(6)?,
                        tool_ms: number(7)?,
                    })
                })
                .map_err(store_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(store_error)
        })
        .await
    }

    /// The recorded tasks, the latest first.
    pub async fn tasks(&self, limit: usize) -> Result<Vec<Task>, ErrorInfo> {
        self.read(Duration::from_secs(10), move |db| {
            let mut statement = db
                .prepare_cached(&format!(
                    "SELECT {TASK_COLUMNS} FROM tasks ORDER BY started DESC, id DESC LIMIT ?1"
                ))
                .map_err(store_error)?;
            let rows = statement
                .query_map([limit as i64], task_row)
                .map_err(store_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(store_error)
        })
        .await
    }
}

/// The forecast for `text` on `model`, from the store's past tasks and the
/// model's measured speed.
pub(crate) async fn forecast(
    store: &Store,
    text: &str,
    model: &str,
) -> Result<Forecast, ErrorInfo> {
    let past = store.past_tasks().await?;
    let speed = store.model_speed(model).await?;
    Ok(Forecast::new(&past, text, model, speed))
}

impl Core {
    /// The estimate of the work `text` describes, on the model of `thread`
    /// or else the default one.
    pub async fn estimate(
        &self,
        thread: Option<SessionId>,
        text: &str,
    ) -> Result<Value, ErrorInfo> {
        let text = text.trim();
        if text.is_empty() {
            return Err(ErrorInfo::new(
                "invalid_params",
                "describe the work to estimate",
            ));
        }
        let store = self.store()?;
        let model = match thread {
            Some(thread) => {
                if !store.session_exists(thread).await? {
                    return Err(crate::unknown_thread());
                }
                self.thread_client(thread).await?.model().to_string()
            }
            None => self.default_client().await?.model().to_string(),
        };
        let estimate = forecast(store, text, &model).await?.remaining(0, 0);
        Ok(json!({
            "estimate": estimate,
            "task_type": task_type(text),
            "model": model,
        }))
    }

    /// What is left of the active goal `goal` of `thread`, which has run
    /// for `elapsed_ms`; `None` when the goal is not active.
    pub(crate) async fn goal_estimate(
        &self,
        thread: SessionId,
        goal: &crate::goal::Goal,
        elapsed_ms: u64,
    ) -> Result<Option<Estimate>, ErrorInfo> {
        if !goal.is_active() {
            return Ok(None);
        }
        let store = self.store()?;
        let model = self.thread_client(thread).await?.model().to_string();
        let recorded = store.goal_task_tokens(thread, goal.from_seq).await?;
        let used = recorded.max(goal.tokens);
        let forecast = forecast(store, &goal.objective, &model).await?;
        Ok(Some(forecast.remaining(used, elapsed_ms)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn past(request: &str, tokens: u64, minutes: u64) -> Past {
        Past {
            request: request.into(),
            task_type: recorded_type(request, &ToolCounts::default()).into(),
            model: "claude-opus-5-5".into(),
            input: tokens,
            output: 0,
            requests: 0,
            wall_ms: minutes * 60_000,
            tool_ms: 0,
        }
    }

    /// The four tasks measured on 2026-11-25 (docs/evidence/effort-estimates.md),
    /// as a fixture only: they are never written to a real database.
    fn measured() -> Vec<Task> {
        [
            ("Firefox のオフライン翻訳を実装する", 305_000, 40, 2_600),
            ("Chromium の TFLite 翻訳を実装する", 356_000, 36, 4_000),
            ("カタログの署名・更新・一覧を実装する", 386_000, 34, 1_937),
            (
                "Backoff・証明書の取り消し・ロックを実装する",
                279_000,
                28,
                1_018,
            ),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (request, tokens, minutes, lines))| Task {
            started_ms: 1_764_000_000_000 + index as i64,
            session: None,
            kind: "turn".into(),
            goal_seq: None,
            request: request.into(),
            task_type: recorded_type(request, &ToolCounts::default()).into(),
            provider: "claude".into(),
            model: "claude-opus-5-5".into(),
            effort: None,
            input: tokens,
            output: 0,
            cached: 0,
            requests: 0,
            wall_ms: minutes * 60_000,
            tool_ms: 0,
            tools: ToolCounts::default(),
            lines_added: Some(lines),
            lines_removed: None,
            files_changed: None,
            changes_from: "git".into(),
            outcome: "completed".into(),
        })
        .collect()
    }

    #[test]
    fn words_and_japanese_pairs_are_the_terms() {
        assert_eq!(
            terms("Fix the TFLite 翻訳を直す a"),
            ["fix", "the", "tflite", "翻訳", "訳を", "を直", "直す"]
        );
        assert_eq!(terms("翻"), ["翻"]);
    }

    #[test]
    fn the_most_similar_text_scores_highest() {
        let documents = vec![
            terms("translate pages offline in firefox"),
            terms("sign the catalog and list it"),
            terms("translate with tflite in chromium"),
        ];
        let scores = cosines(&documents, &terms("offline translation in firefox"));
        assert!(scores[0] > scores[2] && scores[2] > scores[1], "{scores:?}");
        assert_eq!(scores[1], 0.0);
        let same = cosines(&documents[..1], &documents[0]);
        assert!((same[0] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn quantiles_interpolate_between_neighbors() {
        let values: Vec<f64> = (1..=11).map(f64::from).collect();
        assert_eq!(quantile(&values, 0.5), 6.0);
        assert_eq!(quantile(&values, 0.1), 2.0);
        assert!((quantile(&[1.0, 2.0], 0.25) - 1.25).abs() < 1e-9);
        let (range, widened) = spread(values);
        assert_eq!(
            (range.low, range.mid, range.high, widened),
            (2.0, 6.0, 10.0, false)
        );
    }

    #[test]
    fn few_samples_widen_the_range_and_say_how_many() {
        let (range, widened) = spread(vec![300.0, 100.0, 200.0]);
        assert!(widened);
        assert_eq!((range.low, range.mid, range.high), (50.0, 200.0, 600.0));
        let tasks = vec![
            past("translate a page", 100, 1),
            past("translate a book", 300, 3),
        ];
        let estimate =
            Forecast::new(&tasks, "translate", "claude-opus-5-5", Speed::default()).remaining(0, 0);
        assert_eq!(estimate.samples, 2);
        assert!(estimate.widened);
        assert_eq!(
            estimate.tokens,
            Some(Range {
                low: 50,
                mid: 200,
                high: 600
            })
        );
        assert!(estimate.note.unwrap().contains("only 2 past tasks"));
    }

    #[test]
    fn many_samples_give_the_tenth_to_ninetieth_percentiles() {
        let tasks: Vec<Past> = (1..=11)
            .map(|n| past("rename the field", n * 1_000, n))
            .collect();
        let estimate =
            Forecast::new(&tasks, "rename a field", "gpt-5", Speed::default()).remaining(0, 0);
        assert_eq!(estimate.samples, 11);
        assert!(!estimate.widened);
        assert_eq!(
            estimate.tokens,
            Some(Range {
                low: 2_000,
                mid: 6_000,
                high: 10_000
            })
        );
        assert_eq!(
            estimate.minutes,
            Some(Range {
                low: 2.0,
                mid: 6.0,
                high: 10.0
            })
        );
        assert_eq!(estimate.method, "similar");
    }

    #[test]
    fn without_history_nothing_is_estimated() {
        let estimate = Forecast::new(&[], "anything", "m", Speed::default()).remaining(0, 0);
        assert_eq!(
            (estimate.tokens, estimate.samples, estimate.method),
            (None, 0, "none")
        );
    }

    #[test]
    fn the_remaining_range_comes_from_the_tasks_still_going_at_this_point() {
        let tasks: Vec<Past> = [100, 200, 400, 800]
            .into_iter()
            .map(|tokens| past("port the parser", tokens, 4))
            .collect();
        let forecast = Forecast::new(&tasks, "port the parser", "m", Speed::default());
        // After 300 tokens only the 400 and 800 runs were still going: 100
        // and 500 tokens were left, and of their 4 minutes a quarter and
        // five eighths.
        let left = forecast.remaining(300, 60_000);
        assert_eq!(left.samples, 2);
        assert_eq!(
            left.tokens,
            Some(Range {
                low: 50,
                mid: 300,
                high: 1_000
            })
        );
        assert_eq!(left.minutes.unwrap().mid, 1.75);
        assert_eq!(
            left.so_far,
            Some(SoFar {
                tokens: 300,
                minutes: 1.0
            })
        );
        // Beyond every past run, what is left follows Gott's rule.
        let beyond = forecast.remaining(900, 120_000);
        assert_eq!(beyond.method, "beyond");
        assert_eq!(
            beyond.tokens,
            Some(Range {
                low: 100,
                mid: 900,
                high: 8_100
            })
        );
        assert_eq!(
            beyond.minutes,
            Some(Range {
                low: 0.22,
                mid: 2.0,
                high: 18.0
            })
        );
    }

    #[test]
    fn the_measured_speed_converts_tokens_to_minutes() {
        let mut task = past("explain the cache", 10_000, 99);
        task.output = 6_000;
        task.requests = 4;
        task.tool_ms = 30_000;
        let speed = Speed {
            tokens_per_second: Some(100.0),
            first_output_ms: Some(1_500),
        };
        // 60 s of output, 4 waits of 1.5 s, 30 s of tools.
        assert_eq!(seconds(&task, speed), 96.0);
        assert_eq!(seconds(&task, Speed::default()), 99.0 * 60.0);
    }

    #[test]
    fn types_come_from_words_then_from_tools() {
        assert_eq!(task_type("Fix the failing login test"), "fix");
        assert_eq!(task_type("翻訳を実装する"), "implement");
        assert_eq!(task_type("テストを足す"), "test");
        assert_eq!(task_type("この関数は何をしますか"), "question");
        assert_eq!(task_type("hello there"), OTHER);
        let edited = ToolCounts {
            edit: 1,
            ..ToolCounts::default()
        };
        assert_eq!(recorded_type("hello there", &edited), "change");
        assert_eq!(
            recorded_type("hello there", &ToolCounts::default()),
            "question"
        );
        assert_eq!(family("claude-haiku-5-5"), "claude-haiku");
        assert_eq!(family("openai/gpt-5.4-mini"), "gpt-mini");
    }

    #[test]
    fn tool_calls_are_counted_by_kind() {
        let mut tally = Tally::default();
        let second = Duration::from_secs(1);
        tally.add("read", r#"{"path":"a"}"#, second);
        tally.add("shell", r#"{"command":"cargo test -p core"}"#, second);
        tally.add("shell", r#"{"command":"ls -la"}"#, second);
        tally.add("edits", r#"{"edits":[{"path":"a"},{"path":"b"}]}"#, second);
        tally.add("write", r#"{"path":"c"}"#, second);
        tally.add("fetch", "{}", second);
        tally.add("plan", "{}", second);
        assert_eq!(
            tally.counts,
            ToolCounts {
                read: 1,
                edit: 2,
                test: 1,
                shell: 1,
                fetch: 1,
                other: 1
            }
        );
        assert_eq!(tally.tool_ms, 7_000);
        assert_eq!(tally.edited.len(), 3);
    }

    #[tokio::test]
    async fn the_measured_runs_estimate_a_similar_translation() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path().join("inocode.db"))
            .await
            .expect("store");
        for task in measured() {
            store.record_task(&task).await.expect("record");
        }
        let past = store.past_tasks().await.expect("past");
        assert_eq!(past.len(), 4);
        let estimate = Forecast::new(
            &past,
            "Edge のオフライン翻訳を実装する",
            "claude-opus-5-5",
            Speed::default(),
        )
        .remaining(0, 0);
        assert_eq!((estimate.samples, estimate.widened), (4, true));
        let tokens = estimate.tokens.expect("tokens");
        // Median of the four, half the least, twice the most.
        assert_eq!(
            tokens,
            Range {
                low: 139_500,
                mid: 330_500,
                high: 772_000
            }
        );
        assert_eq!(estimate.minutes.expect("minutes").mid, 35.0);
    }

    #[tokio::test]
    async fn a_goal_adds_up_its_turns_and_counts_once() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(dir.path().join("inocode.db"))
            .await
            .expect("store");
        let session = store.start_session(None, "t").await.expect("session");
        let mut turn = measured().remove(0);
        turn.session = Some(session.0);
        turn.goal_seq = Some(3);
        turn.lines_removed = Some(0);
        store.record_task(&turn).await.expect("turn");
        store
            .add_to_goal_task(&turn, 3, "translate offline".into(), "running")
            .await
            .expect("goal");
        assert!(store.past_tasks().await.expect("past").is_empty());
        assert_eq!(
            store.goal_task_tokens(session, 3).await.expect("tokens"),
            305_000
        );
        store
            .add_to_goal_task(&turn, 3, "translate offline".into(), "achieved")
            .await
            .expect("goal");
        let past = store.past_tasks().await.expect("past");
        assert_eq!(past.len(), 1, "the goal counts once, its turns not again");
        assert_eq!(past[0].request, "translate offline");
        assert_eq!((past[0].input, past[0].wall_ms), (610_000, 80 * 60_000));
        let goal = &store.tasks(10).await.expect("tasks")[0];
        assert_eq!(
            (goal.lines_added, goal.outcome.as_str()),
            (Some(5_200), "achieved")
        );

        store
            .add_to_goal_task(&turn, 9, "another".into(), "running")
            .await
            .expect("goal");
        store
            .end_goal_task(session, 9, "abandoned")
            .await
            .expect("end");
        let tasks = store.tasks(10).await.expect("tasks");
        assert!(tasks
            .iter()
            .any(|task| task.goal_seq == Some(9) && task.outcome == "abandoned"));
        assert!(store.delete_session(session).await.expect("delete"));
        assert!(store
            .tasks(10)
            .await
            .expect("tasks")
            .iter()
            .all(|task| task.session.is_none()));
    }
}
