//! The person's standing instructions in a thread, kept by the model in the
//! person's own words and shown after the history with every request.
//! Design and sources: docs/evidence/promise-list.md.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::agent_tools::Context;
use crate::conversation::{Body, Message, Speaker, ToolSpec};
use crate::store::{SessionId, Store};
use crate::ErrorInfo;

pub(crate) const PROMISE: &str = "promise";
/// Instructions the list holds at most: the more rules at once, the fewer
/// are all kept.
const ITEMS: usize = 10;
/// Characters of one quote at most.
const QUOTE_CHARS: usize = 400;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Item {
    quote: String,
    message: i64,
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: PROMISE.into(),
        description: "Replace the list of the person's standing instructions in this \
            thread: what must not be done, when to stop or ask, what may and may not be \
            changed, and other instructions that hold for the rest of the work, not a \
            single request. Quote each in the person's exact words, with the position of \
            the message it is in. The list is shown to you with every request, also after \
            earlier messages are compacted. When the person changes or withdraws one, \
            replace the list at once; an empty list removes them all."
            .into(),
        parameters: json!({
            "type": "object",
            "required": ["items"],
            "properties": {
                "items": {
                    "type": "array",
                    "description": "The COMPLETE list, replacing the previous one.",
                    "maxItems": ITEMS,
                    "items": {
                        "type": "object",
                        "required": ["quote", "message"],
                        "properties": {
                            "quote": {"type": "string", "description": "The person's words, copied exactly."},
                            "message": {"type": "integer", "description": "The position of the person's message the quote is in."}
                        }
                    }
                }
            }
        }),
    }
}

pub(crate) async fn run(context: &Context<'_>, arguments: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Arguments {
        items: Vec<Item>,
    }
    let Arguments { items } =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    if items.len() > ITEMS {
        return Err(format!(
            "the list holds at most {ITEMS} instructions; keep the ones that still apply"
        ));
    }
    for (index, item) in items.iter().enumerate() {
        let quote = item.quote.trim();
        if quote.is_empty() {
            return Err("every instruction needs the person's words".into());
        }
        if quote.chars().count() > QUOTE_CHARS {
            return Err(format!(
                "the quote at message {} is longer than {QUOTE_CHARS} characters; quote only the instruction",
                item.message
            ));
        }
        if items[..index]
            .iter()
            .any(|earlier| same(&earlier.quote, quote))
        {
            return Err(format!("\"{quote}\" is in the list twice"));
        }
        if item.message >= context.seq {
            return Err(format!(
                "message {} is not an earlier message of this thread",
                item.message
            ));
        }
        let stored = context
            .store
            .since(context.session, item.message - 1, 1)
            .await
            .map_err(|error| error.message)?;
        let text = stored
            .first()
            .filter(|stored| stored.seq == item.message)
            .and_then(|stored| person_text(&stored.message))
            .ok_or_else(|| format!("message {} is not one of the person's", item.message))?;
        if !squeezed(&text).contains(&squeezed(quote)) {
            return Err(format!(
                "message {} does not contain \"{quote}\"; copy the person's words exactly, \
                 or read the message with recall",
                item.message
            ));
        }
    }
    let items: Vec<Item> = items
        .into_iter()
        .map(|item| Item {
            quote: item.quote.trim().to_string(),
            ..item
        })
        .collect();
    let json = serde_json::to_string(&items).map_err(|error| error.to_string())?;
    context
        .store
        .set_promises(context.session, json, context.seq)
        .await
        .map_err(|error| error.message)?;
    Ok(if items.is_empty() {
        "The list of standing instructions is empty.".to_string()
    } else {
        format!(
            "Kept {} standing instructions; they are shown to you with every request.",
            items.len()
        )
    })
}

/// The text of a message the person wrote; `None` for any other message.
pub(crate) fn person_text(message: &Message) -> Option<String> {
    match &message.body {
        Body::User {
            from: Speaker::Person,
            parts,
        } => Some(parts.join("\n")),
        _ => None,
    }
}

/// Text with every run of whitespace made one space, so a quote copied
/// across a line break still matches.
fn squeezed(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn same(a: &str, b: &str) -> bool {
    squeezed(a) == squeezed(b)
}

/// The list as the model sees it with each request; `None` when empty.
/// `latest` is the position of the person's latest message: one written
/// after the list was last replaced may change it, so the model is asked
/// to check, since a list that keeps a withdrawn instruction does harm.
pub(crate) async fn render(
    store: &Store,
    session: SessionId,
    latest: Option<i64>,
) -> Result<Option<String>, ErrorInfo> {
    let Some((json, seq)) = store.promises(session).await? else {
        return Ok(None);
    };
    let items: Vec<Item> =
        serde_json::from_str(&json).map_err(|error| ErrorInfo::new("store", error.to_string()))?;
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(text(&items, seq, latest)))
}

fn text(items: &[Item], seq: i64, latest: Option<i64>) -> String {
    let mut text = String::from(
        "The person's standing instructions in this thread, in their words. Keep them \
         until the person changes or withdraws one:\n",
    );
    for item in items {
        text.push_str(&format!(
            "- \"{}\" (message {})\n",
            item.quote, item.message
        ));
    }
    if let Some(latest) = latest.filter(|latest| *latest > seq) {
        text.push_str(&format!(
            "The person has written since this list was made (message {latest}). Before \
             going on, check whether they changed, withdrew or added an instruction, and \
             call promise with the list as it now stands, the same list if nothing changed.\n"
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{Origin, StopReason};
    use crate::ProviderKind;

    fn person(id: &str, text: &str) -> Message {
        Message {
            id: id.into(),
            body: Body::User {
                from: Speaker::Person,
                parts: vec![text.into()],
            },
        }
    }

    fn assistant(id: &str) -> Message {
        Message {
            id: id.into(),
            body: Body::Assistant {
                origin: Origin {
                    format: ProviderKind::Anthropic,
                    model: "m".into(),
                },
                stop: StopReason::Completed,
                parts: Vec::new(),
            },
        }
    }

    async fn thread(store: &Store) -> SessionId {
        let session = store.start_session(None, "t").await.expect("session");
        store
            .append(
                session,
                vec![
                    person("1", "Print the forms.\nStop printing as soon as\nthe camera cannot see the paper."),
                    assistant("2"),
                    person("3", "Faster, please."),
                ],
            )
            .await
            .expect("append");
        session
    }

    #[tokio::test]
    async fn only_the_persons_exact_words_are_kept() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Store::open(&dir.path().join("s.db")).await.expect("store");
        let session = thread(&store).await;
        let (_cancel, receiver) = tokio::sync::watch::channel(false);
        let shown = std::sync::Mutex::new(Vec::new());
        let context = Context {
            store: &store,
            session,
            project: None,
            rules: None,
            folder: None,
            cancel: receiver,
            sandboxed: false,
            seq: 4,
            shown: &shown,
            events: None,
            sampling: None,
        };
        let call = |quote: &str, message: i64| {
            json!({"items": [{"quote": quote, "message": message}]}).to_string()
        };
        // Across a line break, with the spacing changed.
        let kept = call(
            "Stop printing as soon as the camera cannot see the paper.",
            1,
        );
        assert!(run(&context, &kept)
            .await
            .expect("kept")
            .starts_with("Kept 1"));
        // Words the person did not write, the assistant's message, a later one.
        let invented = run(&context, &call("Stop printing when paper jams.", 1)).await;
        assert!(invented.unwrap_err().contains("does not contain"));
        let theirs = run(&context, &call("x", 2)).await;
        assert!(theirs.unwrap_err().contains("not one of the person's"));
        assert!(run(&context, &call("x", 4)).await.is_err());
        let many: Vec<_> = (0..=ITEMS)
            .map(|n| json!({"quote": format!("Faster{n}"), "message": 3}))
            .collect();
        let many = json!({ "items": many }).to_string();
        assert!(run(&context, &many).await.unwrap_err().contains("at most"));

        // The refused calls left the list as it was.
        let shown = render(&store, session, Some(3)).await.expect("render");
        assert_eq!(
            shown.as_deref(),
            Some(
                "The person's standing instructions in this thread, in their words. Keep \
                 them until the person changes or withdraws one:\n- \"Stop printing as soon \
                 as the camera cannot see the paper.\" (message 1)\n"
            )
        );
        // A message of the person's after the list asks for it to be checked.
        let later = render(&store, session, Some(5))
            .await
            .expect("render")
            .expect("list");
        assert!(
            later.contains("has written since this list was made (message 5)"),
            "{later}"
        );
        // An empty list removes it.
        assert!(run(&context, r#"{"items": []}"#)
            .await
            .expect("emptied")
            .contains("empty"));
        assert_eq!(
            render(&store, session, Some(5)).await.expect("render"),
            None
        );
    }
}
