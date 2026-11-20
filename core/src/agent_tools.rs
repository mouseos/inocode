//! The tools the harness gives the model: `plan`, `recall`, and `lesson` for
//! keeping its place and its lessons (docs/evidence/agent-tools.md), `method`
//! for verified methods reused across projects (docs/evidence/method-memory.md),
//! and `search` (docs/evidence/web-search.md).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::conversation::{AssistantPart, Body, Message, ToolSpec};
use crate::store::{
    HistoryQuery, NewNote, NoteKind, NoteQuery, ProjectId, SessionId, Source, Store,
};
use crate::{edit, fetch, find, outline, read, shell, web_search, ErrorInfo};

/// Messages one `recall` by position returns at most.
const RECALL_RANGE: i64 = 20;
/// Characters of one recalled message or search hit; the rest is cut.
const RECALL_CHARS: usize = 2_000;
const SEARCH_BUDGET: Duration = Duration::from_secs(2);

pub(crate) const PLAN: &str = "plan";
pub(crate) const RECALL: &str = "recall";
pub(crate) const LESSON: &str = "lesson";
pub(crate) const METHOD: &str = "method";

/// The tools; `sandboxed` is whether shell commands run confined.
pub(crate) fn specs(sandboxed: bool) -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: PLAN.into(),
            description: "Replace the work list of this task with the complete list. The \
                current list is shown to you with every request, also after earlier \
                messages are compacted, so keep it current: mark a step in_progress \
                before working on it and completed as soon as it is done."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["items"],
                "properties": {
                    "items": {
                        "type": "array",
                        "description": "The COMPLETE list, replacing the previous one.",
                        "items": {
                            "type": "object",
                            "required": ["step", "status"],
                            "properties": {
                                "step": {"type": "string"},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                            }
                        }
                    }
                }
            }),
        },
        ToolSpec {
            name: RECALL.into(),
            description: "Read earlier work word for word. With from_seq (and to_seq), \
                returns this thread's messages at those positions, including ones \
                replaced by a summary. With query, searches the project's past messages \
                and its knowledge notes (terms of three or more characters)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "from_seq": {"type": "integer"},
                    "to_seq": {"type": "integer"}
                }
            }),
        },
        ToolSpec {
            name: LESSON.into(),
            description: "Propose a lesson for later work in this project: in which \
                situation (when), what to do (do), and the cause (because). Do not record \
                a failure whose cause the error message itself states, and never turn a \
                failure into a general ban: name the condition under which the action \
                fails. If the project rules already say it, set relation to \
                restates_rule; if they say otherwise, contradicts_rule; quote the rule \
                text in either case."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["when", "do", "because", "relation"],
                "properties": {
                    "when": {"type": "string"},
                    "do": {"type": "string"},
                    "because": {"type": "string"},
                    "relation": {"type": "string", "enum": ["new", "restates_rule", "contradicts_rule"]},
                    "rule_quote": {"type": "string"}
                }
            }),
        },
        ToolSpec {
            name: METHOD.into(),
            description: "Save a method you have verified to work, so that later work, in this or any \
                other project, can reuse it instead of investigating again. Call it as soon as \
                a method is shown to work. identifier names exactly what the method applies to \
                (device or chip model, file format and magic, firmware or app version, hash); \
                steps are the exact commands, scripts, offsets and values that worked; failed \
                lists general approaches that did not work here and why. Saved methods are \
                found by recall. tags are chosen from the tags already in use: an unknown \
                one is refused with the similar tags in use listed. Put a tag in new_tags \
                only when none of those fits."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["identifier", "title", "steps"],
                "properties": {
                    "identifier": {"type": "string"},
                    "title": {"type": "string"},
                    "steps": {"type": "string"},
                    "failed": {"type": "string"},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "new_tags": {"type": "array", "items": {"type": "string"}}
                }
            }),
        },
        web_search::spec(),
        read::spec(),
        fetch::spec(),
        find::spec(),
    ]
    .into_iter()
    .chain(edit::specs())
    .chain(shell::specs(sandboxed))
    .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanItem {
    pub step: String,
    pub status: PlanStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

/// What a tool needs from the thread it runs in.
pub(crate) struct Context<'a> {
    pub store: &'a Store,
    pub session: SessionId,
    pub project: Option<ProjectId>,
    /// The project's AGENTS.md, as given to the model.
    pub rules: Option<&'a str>,
    /// The project folder, where commands run.
    pub folder: Option<&'a std::path::Path>,
    pub cancel: tokio::sync::watch::Receiver<bool>,
    /// Whether shell commands run confined.
    pub sandboxed: bool,
}

/// The answer to one call: its text and whether it is an error.
pub(crate) type Answer = (String, bool);

/// Runs one of the harness tools; `None` when `name` is not one of them.
pub(crate) async fn run(context: &Context<'_>, name: &str, arguments: &str) -> Option<Answer> {
    let answer = match name {
        PLAN => plan(context, arguments).await,
        RECALL => recall(context, arguments).await,
        LESSON => lesson(context, arguments).await,
        METHOD => method(context, arguments).await,
        web_search::SEARCH => web_search::run(arguments).await,
        read::READ => read::run(context.folder, arguments).await,
        edit::EDIT => edit::edit(context.folder, arguments).await,
        edit::WRITE => edit::write(context.folder, arguments).await,
        find::FIND => {
            outline::use_grammars(context.store.files_folder().join("grammars"));
            find::run(context.folder, arguments).await
        }
        fetch::FETCH => {
            let folder = context
                .store
                .files_folder()
                .join("fetch")
                .join(format!("thread-{}", context.session.0));
            fetch::run(&folder, arguments).await
        }
        shell::SHELL => shell::shell(place(context), arguments).await,
        shell::JOB => shell::job(place(context), arguments).await,
        _ => return None,
    };
    Some(answer.map_or_else(|message| (message, true), |text| (text, false)))
}

fn place<'a>(context: &Context<'a>) -> shell::Place<'a> {
    shell::Place {
        session: context.session,
        folder: context.folder,
        logs: context.store.files_folder().join("logs"),
        cancel: context.cancel.clone(),
        sandboxed: context.sandboxed,
    }
}

fn parse<T: for<'de> Deserialize<'de>>(arguments: &str) -> Result<T, String> {
    serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))
}

async fn plan(context: &Context<'_>, arguments: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Arguments {
        items: Vec<PlanItem>,
    }
    let Arguments { items } = parse(arguments)?;
    if items.iter().any(|item| item.step.trim().is_empty()) {
        return Err("every step needs text".into());
    }
    let count = |status| items.iter().filter(|item| item.status == status).count();
    if count(PlanStatus::InProgress) > 1 {
        return Err("at most one step may be in_progress".into());
    }
    let summary = format!(
        "Plan updated: {} pending, {} in progress, {} completed.",
        count(PlanStatus::Pending),
        count(PlanStatus::InProgress),
        count(PlanStatus::Completed)
    );
    let json = serde_json::to_string(&items).map_err(|error| error.to_string())?;
    context
        .store
        .set_plan(context.session, json)
        .await
        .map_err(|error| error.message)?;
    Ok(summary)
}

/// The stored plan as the model sees it with each request; `None` when empty.
pub(crate) async fn render_plan(
    store: &Store,
    session: SessionId,
) -> Result<Option<String>, ErrorInfo> {
    let Some(json) = store.plan(session).await? else {
        return Ok(None);
    };
    let items: Vec<PlanItem> =
        serde_json::from_str(&json).map_err(|error| ErrorInfo::new("store", error.to_string()))?;
    if items.is_empty() {
        return Ok(None);
    }
    let mut text = String::from("Current plan:\n");
    for item in items {
        let mark = match item.status {
            PlanStatus::Pending => "[ ]",
            PlanStatus::InProgress => "[>]",
            PlanStatus::Completed => "[x]",
        };
        text.push_str(&format!("{mark} {}\n", item.step));
    }
    Ok(Some(text))
}

async fn recall(context: &Context<'_>, arguments: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Arguments {
        query: Option<String>,
        from_seq: Option<i64>,
        to_seq: Option<i64>,
    }
    let arguments: Arguments = parse(arguments)?;
    if let Some(from) = arguments.from_seq {
        let to = arguments.to_seq.unwrap_or(from).max(from);
        let to = to.min(from + RECALL_RANGE - 1);
        let messages = context
            .store
            .since(context.session, from - 1, (to - from + 1) as usize)
            .await
            .map_err(|error| error.message)?;
        if messages.is_empty() {
            return Ok(format!("No messages at positions {from} to {to}."));
        }
        let mut text = String::new();
        for stored in messages {
            text.push_str(&format!("#{} {}\n", stored.seq, render(&stored.message)));
        }
        return Ok(text);
    }
    let Some(query) = arguments.query.filter(|query| !query.trim().is_empty()) else {
        return Err("give query, or from_seq".into());
    };
    let history = context
        .store
        .search_history(HistoryQuery {
            text: query.clone(),
            project: context.project,
            since: None,
            until: None,
            limit: 10,
            budget: SEARCH_BUDGET,
        })
        .await
        .map_err(|error| error.message)?;
    let notes = context
        .store
        .search_notes(NoteQuery {
            text: query,
            project: context.project,
            include_shared: true,
            kinds: vec![NoteKind::Knowledge, NoteKind::Claim, NoteKind::Procedure],
            usable_only: true,
            since: None,
            until: None,
            limit: 5,
            budget: SEARCH_BUDGET,
        })
        .await
        .map_err(|error| error.message)?;
    if history.items.is_empty() && notes.items.is_empty() {
        return Ok("Nothing found. Search terms need three or more characters.".into());
    }
    let mut text = String::new();
    for hit in notes.items {
        text.push_str(&format!(
            "note {} ({}): {} — {}\n",
            hit.id.0,
            hit.kind.as_str(),
            hit.title,
            cut(&hit.snippet)
        ));
        if hit.kind == NoteKind::Procedure {
            if let Some(note) = context
                .store
                .note(hit.id)
                .await
                .map_err(|error| error.message)?
            {
                text.push_str(&format!("{}\n", note.body));
            }
        }
    }
    for hit in history.items {
        let place = if hit.session == context.session {
            format!("#{}", hit.seq)
        } else {
            format!("thread {} #{}", hit.session.0, hit.seq)
        };
        text.push_str(&format!("{place} {}: {}\n", hit.role, cut(&hit.snippet)));
    }
    Ok(text)
}

fn cut(text: &str) -> String {
    match text.char_indices().nth(RECALL_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// One message as plain text for `recall`.
pub(crate) fn render(message: &Message) -> String {
    match &message.body {
        Body::User { parts, .. } => format!("user: {}", cut(&parts.join("\n"))),
        Body::Assistant { parts, .. } => {
            let mut text = String::from("assistant:");
            for part in parts {
                match part {
                    AssistantPart::Text { text: said } => {
                        text.push(' ');
                        text.push_str(&cut(said));
                    }
                    AssistantPart::ToolCall {
                        name, arguments, ..
                    } => text.push_str(&format!(" [call {name} {}]", cut(arguments))),
                    AssistantPart::Reasoning { .. } => {}
                }
            }
            text
        }
        Body::ToolResult {
            name,
            content,
            is_error,
            ..
        } => {
            let kind = if *is_error { "error" } else { "result" };
            format!("{kind} of {name}: {}", cut(content))
        }
    }
}

async fn lesson(context: &Context<'_>, arguments: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Arguments {
        when: String,
        #[serde(rename = "do")]
        action: String,
        because: String,
        relation: String,
        rule_quote: Option<String>,
    }
    let arguments: Arguments = parse(arguments)?;
    let (when, action, because) = (
        arguments.when.trim(),
        arguments.action.trim(),
        arguments.because.trim(),
    );
    if when.is_empty() || action.is_empty() || because.is_empty() {
        return Err("when, do, and because are all required".into());
    }
    let body = format!("When: {when}\nDo: {action}\nBecause: {because}");
    match arguments.relation.as_str() {
        "new" => {
            let id = context
                .store
                .add_note(NewNote {
                    project: context.project,
                    kind: NoteKind::Lesson,
                    title: cut_title(action),
                    summary: because.to_string(),
                    keywords: format!("{when} {action}"),
                    condition: when.to_string(),
                    body,
                    source: Source::Model,
                    tags: Vec::new(),
                })
                .await
                .map_err(|error| error.message)?;
            Ok(format!(
                "Stored as candidate lesson {}. It is used in later work once verified or approved.",
                id.0
            ))
        }
        relation @ ("restates_rule" | "contradicts_rule") => {
            let quote = arguments.rule_quote.unwrap_or_default();
            if !context
                .rules
                .is_some_and(|rules| crate::rules_import::quotes(rules, &quote))
            {
                return Err("rule_quote must be text from the project rules (AGENTS.md)".into());
            }
            if relation == "restates_rule" {
                return Ok(
                    "Not stored: the project rules already say this. Follow the rule.".into(),
                );
            }
            let id = context
                .store
                .open_issue(
                    context.project,
                    &format!("A lesson contradicts AGENTS.md: {}", cut_title(action)),
                    &format!("{body}\n\nAGENTS.md says:\n{quote}"),
                    0,
                    Vec::new(),
                )
                .await
                .map_err(|error| error.message)?;
            Ok(format!(
                "Not stored as a lesson: it contradicts the project rules. Opened issue {} for the person to decide; keep following the rules meanwhile.",
                id.0
            ))
        }
        other => Err(format!("unknown relation {other}")),
    }
}

/// Saves a verified method as a shared procedure note, usable at once.
async fn method(context: &Context<'_>, arguments: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Arguments {
        identifier: String,
        title: String,
        steps: String,
        #[serde(default)]
        failed: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        new_tags: Vec<String>,
    }
    let arguments: Arguments = parse(arguments)?;
    let (identifier, title, steps) = (
        arguments.identifier.trim(),
        arguments.title.trim(),
        arguments.steps.trim(),
    );
    if identifier.is_empty() || title.is_empty() || steps.is_empty() {
        return Err("identifier, title, and steps must not be empty".into());
    }
    let in_use = context
        .store
        .tag_counts()
        .await
        .map_err(|error| error.message)?;
    let tags = method_tags(&in_use, &arguments.tags, &arguments.new_tags)?;
    let origin = context
        .folder
        .map(|folder| folder.display().to_string())
        .unwrap_or_else(|| "unknown".into());
    let body = format!(
        "Method: {title}\nApplies to: {identifier}\nFound in project: {origin}\n\
         Steps that worked: {steps}\nDid not work: {}",
        arguments.failed.trim()
    );
    let id = context
        .store
        .add_note(NewNote {
            project: None,
            kind: NoteKind::Procedure,
            title: cut_title(title),
            summary: title.to_string(),
            keywords: format!("{identifier} {title} {}", tags.join(" ")),
            condition: identifier.to_string(),
            body,
            source: Source::Model,
            tags,
        })
        .await
        .map_err(|error| error.message)?;
    context
        .store
        .set_status(id, "active")
        .await
        .map_err(|error| error.message)?;
    Ok(format!("Saved method {}.", id.0))
}

/// The tags of a method: `chosen` must be tags in use (`in_use`, with their
/// counts), and `new` ones must not be a tag in use written differently, so
/// that one subject keeps one tag (docs/evidence/method-memory.md).
fn method_tags(
    in_use: &[(String, u64)],
    chosen: &[String],
    new: &[String],
) -> Result<Vec<String>, String> {
    let squash = |tag: &str| {
        let letters: String = tag.chars().filter(|c| c.is_alphanumeric()).collect();
        letters.strip_suffix('s').unwrap_or(&letters).to_string()
    };
    let mut tags: Vec<String> = Vec::new();
    for tag in chosen {
        let tag = crate::store::normalize_tag(tag);
        if tag.is_empty() || tags.contains(&tag) {
            continue;
        }
        if !in_use.iter().any(|(used, _)| *used == tag) {
            return Err(unknown_tag(in_use, &tag));
        }
        tags.push(tag);
    }
    for tag in new {
        let tag = crate::store::normalize_tag(tag);
        if tag.is_empty() || tags.contains(&tag) {
            continue;
        }
        if let Some((used, _)) = in_use
            .iter()
            .find(|(used, _)| *used != tag && squash(used) == squash(&tag))
        {
            return Err(format!(
                "`{tag}` is the tag `{used}` already in use, written differently; put `{used}` \
                 in tags instead."
            ));
        }
        tags.push(tag);
    }
    Ok(tags)
}

/// The refusal of a tag not in use, listing the tags in use most like it.
fn unknown_tag(in_use: &[(String, u64)], tag: &str) -> String {
    if in_use.is_empty() {
        return format!("No tags are in use yet; put `{tag}` in new_tags.");
    }
    let grams = |text: &str| -> std::collections::HashSet<String> {
        let chars: Vec<char> = text.chars().collect();
        chars.windows(3).map(|gram| gram.iter().collect()).collect()
    };
    let wanted = grams(tag);
    let mut scored: Vec<(f64, u64, &str)> = in_use
        .iter()
        .map(|(used, count)| {
            let theirs = grams(used);
            let shared = wanted.intersection(&theirs).count() as f64;
            let all = wanted.union(&theirs).count().max(1) as f64;
            (shared / all, *count, used.as_str())
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.cmp(&a.1)));
    let listed: Vec<&str> = scored.iter().take(10).map(|(_, _, used)| *used).collect();
    format!(
        "`{tag}` is not a tag in use. Tags in use most like it: {}. Put one of them in tags; \
         if none fits, put `{tag}` in new_tags.",
        listed.join(", ")
    )
}

fn cut_title(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(80)
        .collect()
}

/// Lessons that may apply to `context`, as shown with each request.
pub(crate) async fn render_lessons(
    store: &Store,
    project: Option<ProjectId>,
    context: &str,
) -> Result<Option<String>, ErrorInfo> {
    let found = store
        .lessons_for(project, context, 3, SEARCH_BUDGET)
        .await?;
    if found.items.is_empty() {
        return Ok(None);
    }
    let mut text = String::from("Lessons from earlier work that may apply:\n");
    for hit in found.items {
        if let Some(note) = store.note(hit.id).await? {
            text.push_str(&format!("- {}\n", note.body.replace('\n', " / ")));
        }
    }
    Ok(Some(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::Speaker;

    #[test]
    fn method_tags_are_chosen_from_those_in_use() {
        let words = |list: &[&str]| list.iter().map(|tag| tag.to_string()).collect::<Vec<_>>();
        let in_use = vec![
            ("firmware".to_string(), 4),
            ("xor".to_string(), 2),
            ("uart".to_string(), 1),
        ];
        assert_eq!(
            method_tags(
                &in_use,
                &words(&["#Firmware", "xor"]),
                &words(&["Mediatek MT6765"])
            ),
            Ok(words(&["firmware", "xor", "mediatek-mt6765"]))
        );
        let refused = method_tags(&in_use, &words(&["firmwares"]), &[]).unwrap_err();
        assert!(
            refused.starts_with("`firmwares` is not a tag in use"),
            "{refused}"
        );
        assert!(refused.contains("most like it: firmware"), "{refused}");
        let refused = method_tags(&in_use, &[], &words(&["X-O-R"])).unwrap_err();
        assert!(refused.contains("put `xor` in tags"), "{refused}");
        let refused = method_tags(&in_use, &[], &words(&["Firmwares"])).unwrap_err();
        assert!(refused.contains("put `firmware` in tags"), "{refused}");
        // A new tag that only shares letters with one in use is accepted.
        assert_eq!(
            method_tags(&in_use, &[], &words(&["uart-baud"])),
            Ok(words(&["uart-baud"]))
        );
        assert_eq!(
            method_tags(&[], &words(&["boot"]), &[]),
            Err("No tags are in use yet; put `boot` in new_tags.".to_string())
        );
    }

    #[test]
    fn recalled_messages_are_plain_and_cut() {
        let long = "あ".repeat(RECALL_CHARS + 5);
        let message = Message {
            id: "1".into(),
            body: Body::User {
                from: Speaker::Person,
                parts: vec![long],
            },
        };
        let text = render(&message);
        assert!(text.starts_with("user: "));
        assert!(text.ends_with('…'));
        assert_eq!(text.chars().count(), "user: ".len() + RECALL_CHARS + 1);
    }
}
