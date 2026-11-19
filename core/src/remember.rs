//! Facts a turn established, kept for later work in the project. After a
//! turn that ran tools, the model is asked once for such facts with a quote
//! of the tool output that shows each; only facts whose quote the harness
//! finds in that output are kept, as verified claims. Design and sources:
//! docs/evidence/memory-integration.md.

use serde::Deserialize;

use crate::conversation::{Body, Conversation, Message, Speaker, ToolSpec};
use crate::store::{NewNote, NoteKind, NoteQuery, ProjectId, Source, Store};
use crate::{ErrorInfo, Llm, TokenUsage};

/// Speaker kind of the request.
const KIND: &str = "remember";
const PROMPT: &str = "Before this work is closed: list the facts that tool results in this \
turn showed and that will help later work in this project, such as how the environment \
behaves, where something is kept, or what a command needs. Leave out guesses, what the code \
itself plainly says, and the answer to the task. For each fact, copy word for word the part \
of a tool result that shows it. Do not call tools. Answer with only a JSON array, [] when \
there is nothing: [{\"fact\": \"...\", \"when\": \"the situation in which it matters\", \
\"quote\": \"...\"}]";
/// Shorter quotes would be found in almost any output.
const MIN_QUOTE_CHARS: usize = 12;
const TITLE_CHARS: usize = 80;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct Finding {
    fact: String,
    when: String,
    quote: String,
}

/// The findings in the model's answer: the outermost JSON array in it.
fn parse(answer: &str) -> Vec<Finding> {
    let (Some(start), Some(end)) = (answer.find('['), answer.rfind(']')) else {
        return Vec::new();
    };
    if end < start {
        return Vec::new();
    }
    serde_json::from_str::<Vec<serde_json::Value>>(&answer[start..=end])
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect()
}

/// The findings whose quote is in one of the tool results, `observed`.
fn verified(findings: Vec<Finding>, observed: &[String]) -> Vec<Finding> {
    findings
        .into_iter()
        .filter(|finding| {
            !finding.fact.trim().is_empty()
                && finding.quote.trim().chars().count() >= MIN_QUOTE_CHARS
                && observed
                    .iter()
                    .any(|output| crate::rules_import::quotes(output, &finding.quote))
        })
        .collect()
}

fn title(fact: &str) -> String {
    fact.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(TITLE_CHARS)
        .collect()
}

/// Asks the model for the facts of the turn in `conversation` and keeps
/// the verified ones in `project`. Returns the usage of the request and how
/// many facts were kept.
pub(crate) async fn remember(
    llm: &Llm,
    store: &Store,
    project: ProjectId,
    conversation: &Conversation,
    tools: &[ToolSpec],
    observed: &[String],
) -> Result<(Option<TokenUsage>, usize), ErrorInfo> {
    let mut asked = Conversation {
        system: conversation.system.clone(),
        messages: conversation.messages.clone(),
    };
    asked.messages.push(Message {
        id: KIND.to_string(),
        body: Body::User {
            from: Speaker::Harness(KIND.to_string()),
            parts: vec![PROMPT.to_string()],
        },
    });
    // The turn's tools are offered so the request shares the turn's cached
    // prefix; calls in the answer are ignored.
    let mut stream = llm.converse(&asked, tools).await?;
    let mut answer = String::new();
    while let Some(text) = stream.next().await? {
        answer.push_str(&text);
    }
    let usage = stream.usage();
    let mut kept = 0;
    for finding in verified(parse(&answer), observed) {
        let fact = finding.fact.trim();
        let title = title(fact);
        let known = store
            .search_notes(NoteQuery {
                text: fact.to_string(),
                project: Some(project),
                include_shared: false,
                kinds: vec![NoteKind::Claim],
                usable_only: true,
                since: None,
                until: None,
                limit: 3,
                budget: std::time::Duration::from_secs(2),
            })
            .await?;
        if known.items.iter().any(|hit| hit.title == title) {
            continue;
        }
        let when = finding.when.trim();
        store
            .add_note(NewNote {
                project: Some(project),
                kind: NoteKind::Claim,
                title,
                summary: fact.to_string(),
                keywords: format!("{when} {fact}"),
                condition: when.to_string(),
                body: format!(
                    "Fact: {fact}\nWhen: {when}\nShown by tool output: \"{}\"",
                    finding.quote.trim()
                ),
                source: Source::Verified,
                tags: Vec::new(),
            })
            .await?;
        kept += 1;
    }
    Ok((usage, kept))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(fact: &str, quote: &str) -> Finding {
        Finding {
            fact: fact.into(),
            when: "debugging".into(),
            quote: quote.into(),
        }
    }

    #[test]
    fn the_answer_is_read_from_its_json_array() {
        let answer = "Here:\n```json\n[{\"fact\": \"f\", \"when\": \"w\", \"quote\": \"q\"}, {\"fact\": 1}]\n```";
        assert_eq!(
            parse(answer),
            [Finding {
                fact: "f".into(),
                when: "w".into(),
                quote: "q".into()
            }]
        );
        assert!(parse("[]").is_empty());
        assert!(parse("nothing to keep").is_empty());
        assert!(parse("] [").is_empty());
    }

    #[test]
    fn only_facts_quoted_from_tool_output_are_kept() {
        let observed =
            vec!["boot 2\n/sys/fs/pstore/console-ramoops-0  kept   after reboot\n".to_string()];
        let kept = verified(
            vec![
                finding(
                    "pstore survives a reboot",
                    "console-ramoops-0 kept after reboot",
                ),
                finding("made up", "this text is in no output"),
                finding("too short to tell", "boot 2"),
                finding("", "console-ramoops-0  kept"),
            ],
            &observed,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].fact, "pstore survives a reboot");
    }
}
