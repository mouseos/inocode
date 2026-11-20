//! The colleague: another model the person set up, asked for advice only
//! when rules over the turn's steps show the work is stuck, as LivePlan
//! does: judging is done by rules, and the model only advises. Design and
//! sources: docs/evidence/colleague-review.md.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use tokio::sync::watch;

use crate::conversation::{AssistantPart, Body, Conversation, Message, Speaker};
use crate::turn::TurnUsage;
use crate::{agent_tools, edit, explore, fetch, find, read, shell, web_search, Llm};

/// Steps in a row of one kind of work after which the work counts as stuck:
/// LivePlan's threshold, measured on vanilla runs (5.64 at most on average
/// for resolved issues, 7.61 for unresolved ones).
const SAME_KIND_STEPS: usize = 7;
/// An identical call made this many times counts as repeating.
const SAME_CALL: u32 = 3;
/// Steps after advice before the next can be asked for (LivePlan: 5).
const COOLDOWN_STEPS: usize = 5;
/// Advice in one turn after which the next sign goes to the person instead,
/// once: the work stays stuck although the colleague was heard.
pub(crate) const ADVICE_PER_TURN: u32 = 2;
/// Characters of the steps the colleague reads, the newest kept.
const STEPS_CHARS: usize = 24_000;
const ARGUMENTS_CHARS: usize = 600;
const RESULT_CHARS: usize = 800;
pub(crate) const KIND: &str = "colleague";
/// The event the harness records on an issue for a turn that ended with it
/// unfinished.
pub(crate) const UNFINISHED: &str = "unfinished";
/// Unfinished turns in a row on an issue after which the colleague is
/// asked about it, at the start of the next turn.
pub(crate) const UNFINISHED_TURNS: usize = 2;
/// Rounds of advice on an issue after which the person is asked instead.
pub(crate) const ISSUE_ADVICE: usize = 2;

const INSTRUCTIONS: &str = "You advise a coding agent working on a person's request. A \
monitor found a sign that it may be stuck; you get the request, the agent's steps since \
the last advice, that advice if any, and a hint about the sign. Recommend the agent's \
next step: one short, concrete recommendation grounded in what the steps show, not a new \
long plan. If the steps show it is on track, say so in one line. Copy paths, names, \
numbers and error text exactly. No greetings, politeness, preamble or closing.";

/// The colleague as `settings.json` names it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Settings {
    pub model: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
}

/// The colleague of `settings.json` beside the database; `None` when none
/// is set up.
pub(crate) async fn configured(settings: &Path) -> Result<Option<Settings>, String> {
    let text = match tokio::fs::read_to_string(settings).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| error.to_string())?;
    match value.get("colleague") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(colleague) => serde_json::from_value::<Settings>(colleague.clone())
            .map(Some)
            .map_err(|error| format!("\"colleague\": {error}")),
    }
}

/// A sign that the work is stuck.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sign {
    /// The same call, with the same arguments, made `SAME_CALL` times.
    Repeated { tool: String, times: u32 },
    /// `SAME_KIND_STEPS` steps in a row of one kind of work.
    Prolonged { kind: Kind, steps: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Looking,
    Editing,
    Running,
}

impl Sign {
    /// The hint the colleague gets with the steps, in the manner of
    /// LivePlan's predefined advice.
    pub(crate) fn hint(&self) -> String {
        match self {
            Self::Repeated { tool, times } => format!(
                "The agent made the call {tool} with the same arguments {times} times; its \
                 result has likely not changed."
            ),
            Self::Prolonged {
                kind: Kind::Looking,
                steps,
            } => format!(
                "The agent has only searched and read for {steps} steps in a row. Options: \
                 focus on the code that matters, run something that reproduces the \
                 problem, or make the change if enough is known."
            ),
            Self::Prolonged {
                kind: Kind::Editing,
                steps,
            } => format!(
                "The agent has edited for {steps} steps in a row without running anything \
                 to check the changes."
            ),
            Self::Prolonged {
                kind: Kind::Running,
                steps,
            } => format!(
                "The agent has run commands for {steps} steps in a row without changing \
                 anything."
            ),
        }
    }

    /// Why the colleague was asked, for the agent.
    fn reason(&self) -> String {
        match self {
            Self::Repeated { tool, times } => {
                format!("you made the same call to {tool} {times} times")
            }
            Self::Prolonged { kind, steps } => format!(
                "you have {} for {steps} steps in a row",
                match kind {
                    Kind::Looking => "only searched and read",
                    Kind::Editing => "edited without running anything",
                    Kind::Running => "run commands without changing anything",
                }
            ),
        }
    }
}

/// Whom a sign goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Call {
    Colleague(Sign),
    Person(Sign),
}

/// The rules over one turn's steps.
#[derive(Debug, Default)]
pub(crate) struct Monitor {
    calls: HashMap<(String, String), u32>,
    kind: Option<Kind>,
    run: usize,
    since_advice: Option<usize>,
    advice: u32,
    escalated: bool,
}

impl Monitor {
    /// Takes one step, the calls of an assistant message; whom to ask when
    /// the work looks stuck and someone may be asked now.
    pub(crate) fn step(&mut self, assistant: &Body) -> Option<Call> {
        let Body::Assistant { parts, .. } = assistant else {
            return None;
        };
        let calls: Vec<(&str, &str)> = parts
            .iter()
            .filter_map(|part| match part {
                AssistantPart::ToolCall {
                    name, arguments, ..
                } => Some((name.as_str(), arguments.as_str())),
                _ => None,
            })
            .collect();
        if let Some(since) = self.since_advice.as_mut() {
            *since += 1;
        }
        let mut sign = None;
        for (name, arguments) in &calls {
            let times = self
                .calls
                .entry((name.to_string(), arguments.to_string()))
                .or_insert(0);
            *times += 1;
            if *times >= SAME_CALL && sign.is_none() {
                sign = Some(Sign::Repeated {
                    tool: name.to_string(),
                    times: *times,
                });
            }
        }
        match kind(&calls) {
            Some(kind) if self.kind == Some(kind) => self.run += 1,
            Some(kind) => {
                self.kind = Some(kind);
                self.run = 1;
            }
            None => {}
        }
        if sign.is_none() && self.run >= SAME_KIND_STEPS {
            sign = self.kind.map(|kind| Sign::Prolonged {
                kind,
                steps: self.run,
            });
        }
        let cooled = self
            .since_advice
            .is_none_or(|since| since >= COOLDOWN_STEPS);
        let sign = sign.filter(|_| cooled && !self.escalated)?;
        self.since_advice = Some(0);
        self.run = 0;
        self.calls.clear();
        if self.advice >= ADVICE_PER_TURN {
            self.escalated = true;
            return Some(Call::Person(sign));
        }
        self.advice += 1;
        Some(Call::Colleague(sign))
    }
}

/// The kind of work of a step's calls; `None` for steps of other tools
/// (plan, ask, lessons), which neither continue nor break a run.
fn kind(calls: &[(&str, &str)]) -> Option<Kind> {
    let looking = [
        find::FIND,
        read::READ,
        explore::EXPLORE,
        agent_tools::RECALL,
        web_search::SEARCH,
        fetch::FETCH,
    ];
    if calls
        .iter()
        .any(|(name, _)| [edit::EDIT, edit::EDITS, edit::WRITE].contains(name))
    {
        Some(Kind::Editing)
    } else if calls
        .iter()
        .any(|(name, _)| *name == shell::SHELL || *name == shell::JOB)
    {
        Some(Kind::Running)
    } else if !calls.is_empty() && calls.iter().all(|(name, _)| looking.contains(name)) {
        Some(Kind::Looking)
    } else {
        None
    }
}

/// What the colleague is asked about `steps`, the messages since the last
/// advice in this turn.
pub(crate) fn about_steps(
    request: &str,
    steps: &[Message],
    last_advice: Option<&str>,
    sign: &Sign,
) -> (String, String) {
    let mut asked = format!("The person's request:\n{request}\n\n");
    if let Some(last) = last_advice {
        asked.push_str(&format!("Your last advice:\n{last}\n\n"));
    }
    asked.push_str(&format!(
        "Hint: {}\n\nThe agent's steps since then:\n{}",
        sign.hint(),
        render(steps)
    ));
    (asked, sign.reason())
}

/// What the colleague is asked about an issue that earlier turns left
/// unfinished: the issue, what each of those turns ended with, and the
/// last advice on it.
pub(crate) fn about_issue(
    request: &str,
    issue: &str,
    attempts: &[String],
    last_advice: Option<&str>,
) -> (String, String) {
    let mut asked = format!(
        "The person's request now:\n{request}\n\nThe issue the agent works on:\n{issue}\n\n"
    );
    if let Some(last) = last_advice {
        asked.push_str(&format!("Your last advice on it:\n{last}\n\n"));
    }
    asked.push_str(&format!(
        "Hint: the last {} turns on this issue ended with it unfinished. How each ended:\n",
        attempts.len()
    ));
    for (number, attempt) in attempts.iter().enumerate() {
        asked.push_str(&format!("{}. {attempt}\n", number + 1));
    }
    let reason = format!(
        "the issue you work on stayed unfinished in the last {} turns",
        attempts.len()
    );
    (asked, reason)
}

/// Asks the colleague `asked`; the harness message that brings the advice
/// to the agent, saying `reason`, why the colleague was asked.
pub(crate) async fn advise(
    colleague: &Llm,
    (asked, reason): (String, String),
    cancel: &mut watch::Receiver<bool>,
    usage: &mut TurnUsage,
) -> Result<(Body, String), String> {
    let conversation = Conversation {
        system: Some(INSTRUCTIONS.to_string()),
        messages: vec![Message {
            id: String::new(),
            body: Body::User {
                from: Speaker::Person,
                parts: vec![asked],
            },
        }],
    };
    let started = tokio::select! {
        started = colleague.converse(&conversation, &[]) => started,
        () = crate::turn::cancelled(cancel) => return Err("stopped with the turn".into()),
    };
    let mut stream = started.map_err(|error| error.message)?;
    loop {
        tokio::select! {
            next = stream.next() => match next {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    usage.add(stream.usage());
                    return Err(error.message);
                }
            },
            () = crate::turn::cancelled(cancel) => {
                usage.add(stream.usage());
                return Err("stopped with the turn".into());
            }
        }
    }
    usage.add(stream.usage());
    let Body::Assistant { parts, .. } = stream.assistant() else {
        unreachable!("a stream assembles an assistant turn");
    };
    let advice: String = parts
        .into_iter()
        .filter_map(|part| match part {
            AssistantPart::Text { text } => Some(text),
            _ => None,
        })
        .collect();
    let advice = advice.trim();
    if advice.is_empty() {
        return Err("the colleague gave no advice".into());
    }
    let message = Body::User {
        from: Speaker::Harness(KIND.to_string()),
        parts: vec![format!(
            "A colleague ({}) was asked because {reason}. Their advice:\n{advice}\n\nWeigh \
             it against what you know. If you do not follow it, say why in your next answer.",
            colleague.model(),
        )],
    };
    Ok((message, advice.to_string()))
}

/// The steps as text, each part cut, the newest kept within `STEPS_CHARS`.
fn render(steps: &[Message]) -> String {
    let cut = |text: &str, limit: usize| -> String {
        if text.chars().count() <= limit {
            text.to_string()
        } else {
            let kept: String = text.chars().take(limit).collect();
            format!("{kept} [cut]")
        }
    };
    let mut lines = Vec::new();
    for message in steps {
        match &message.body {
            Body::User {
                from: Speaker::Person,
                parts,
            } => lines.push(format!("Person: {}", parts.join("\n"))),
            Body::User { parts, .. } => {
                lines.push(format!("Harness: {}", cut(&parts.join("\n"), RESULT_CHARS)))
            }
            Body::Assistant { parts, .. } => {
                for part in parts {
                    match part {
                        AssistantPart::Text { text } if !text.trim().is_empty() => {
                            lines.push(format!("Agent: {}", text.trim()))
                        }
                        AssistantPart::ToolCall {
                            name, arguments, ..
                        } => lines.push(format!(
                            "Agent called {name} {}",
                            cut(arguments, ARGUMENTS_CHARS)
                        )),
                        _ => {}
                    }
                }
            }
            Body::ToolResult {
                name,
                content,
                is_error,
                ..
            } => lines.push(format!(
                "Result of {name}{}: {}",
                if *is_error { " (failed)" } else { "" },
                cut(content, RESULT_CHARS)
            )),
        }
    }
    let mut text = String::new();
    for line in lines.iter().rev() {
        if text.chars().count() + line.chars().count() > STEPS_CHARS {
            text.insert_str(0, "[earlier steps left out]\n");
            break;
        }
        text.insert_str(0, &format!("{line}\n"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{Origin, StopReason};
    use crate::ProviderKind;

    fn calls(calls: &[(&str, &str)]) -> Body {
        Body::Assistant {
            origin: Origin {
                format: ProviderKind::OpenAi,
                model: "m".into(),
            },
            stop: StopReason::ToolUse,
            parts: calls
                .iter()
                .enumerate()
                .map(|(index, (name, arguments))| AssistantPart::ToolCall {
                    call_id: index.to_string(),
                    name: name.to_string(),
                    arguments: arguments.to_string(),
                    replay: None,
                })
                .collect(),
        }
    }

    #[test]
    fn the_third_identical_call_is_a_sign() {
        let mut monitor = Monitor::default();
        let call = calls(&[("shell", r#"{"command":"make"}"#)]);
        assert_eq!(monitor.step(&call), None);
        assert_eq!(monitor.step(&calls(&[("edit", "{}")])), None);
        assert_eq!(monitor.step(&call), None);
        assert_eq!(
            monitor.step(&call),
            Some(Call::Colleague(Sign::Repeated {
                tool: "shell".into(),
                times: 3
            }))
        );
    }

    #[test]
    fn seven_steps_of_one_kind_are_a_sign_and_other_tools_do_not_break_the_run() {
        let mut monitor = Monitor::default();
        for index in 0..6 {
            let looking = calls(&[("read", &format!("{{\"path\":\"{index}\"}}"))]);
            assert_eq!(monitor.step(&looking), None, "{index}");
            let plan = format!("{{\"step\":{index}}}");
            assert_eq!(monitor.step(&calls(&[("plan", &plan)])), None);
        }
        assert_eq!(
            monitor.step(&calls(&[("find", r#"{"pattern":"x"}"#)])),
            Some(Call::Colleague(Sign::Prolonged {
                kind: Kind::Looking,
                steps: 7
            }))
        );
        // An edit starts a new run.
        let mut monitor = Monitor::default();
        for index in 0..6 {
            monitor.step(&calls(&[("read", &format!("{{\"path\":\"{index}\"}}"))]));
        }
        assert_eq!(monitor.step(&calls(&[("edit", "{}")])), None);
        assert_eq!(monitor.step(&calls(&[("read", "{}")])), None);
    }

    #[test]
    fn advice_waits_five_steps_then_after_two_the_person_is_asked_once() {
        let mut monitor = Monitor::default();
        let call = calls(&[("shell", r#"{"command":"make"}"#)]);
        let mut asked = Vec::new();
        for step in 0..60 {
            match monitor.step(&call) {
                Some(Call::Colleague(_)) => asked.push((step, "colleague")),
                Some(Call::Person(_)) => asked.push((step, "person")),
                None => {}
            }
        }
        // The 3rd call, then 5 steps of waiting and 3 identical calls again.
        assert_eq!(asked, [(2, "colleague"), (7, "colleague"), (12, "person")]);
    }

    #[test]
    fn the_steps_are_rendered_cut_and_newest_first_kept() {
        let long = "y".repeat(RESULT_CHARS + 10);
        let steps = vec![
            Message {
                id: String::new(),
                body: calls(&[("read", r#"{"path":"a"}"#)]),
            },
            Message {
                id: String::new(),
                body: Body::ToolResult {
                    call_id: "0".into(),
                    name: "read".into(),
                    content: long,
                    is_error: true,
                },
            },
        ];
        let text = render(&steps);
        assert!(text.starts_with("Agent called read {\"path\":\"a\"}\nResult of read (failed): y"));
        assert!(text.trim_end().ends_with("[cut]"));
        let many: Vec<Message> = (0..200).flat_map(|_| steps.clone()).collect();
        let text = render(&many);
        assert!(text.starts_with("[earlier steps left out]\n"));
        assert!(text.chars().count() <= STEPS_CHARS + 30);
    }
}
