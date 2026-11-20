//! The `ask` tool: questions to the person, as a form their screen shows.
//! The turn does not wait: the questions are reported when it ends, and the
//! answers come in a later message. Design: docs/evidence/ask-person.md.

use serde_json::{json, Value};

use crate::conversation::ToolSpec;

pub(crate) const ASK: &str = "ask";
const QUESTIONS_AT_ONCE: usize = 4;
const OPTIONS_AT_MOST: usize = 10;
const CHOICES: [&str; 3] = ["choose_one", "choose_many", "order"];
const KINDS: [&str; 9] = [
    "choose_one",
    "choose_many",
    "order",
    "text",
    "number",
    "yes_no",
    "date",
    "color",
    "file",
];

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: ASK.into(),
        description: "Ask the person up to 4 questions, as a form their screen shows. A \
            question is answered by choosing one option (choose_one) or several \
            (choose_many), by putting the options in order (order), or with text, a \
            number, yes or no, a date, a color, or a file of the project. Put the option \
            you recommend first; the person can always answer in their own words instead. \
            Ask when the answer changes what you do and cannot be found in the project \
            or the conversation. The answers come in a later message; until then, go on \
            with work that does not depend on them."
            .into(),
        parameters: json!({
            "type": "object",
            "required": ["questions"],
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": QUESTIONS_AT_ONCE,
                    "items": {
                        "type": "object",
                        "required": ["question", "kind"],
                        "properties": {
                            "question": {"type": "string"},
                            "kind": {"type": "string", "enum": KINDS},
                            "options": {
                                "type": "array",
                                "description": "For choose_one, choose_many and order; the recommended one first.",
                                "items": {
                                    "type": "object",
                                    "required": ["label"],
                                    "properties": {
                                        "label": {"type": "string"},
                                        "description": {"type": "string"},
                                        "preview": {
                                            "type": "string",
                                            "description": "Code or a diff showing what this option changes."
                                        }
                                    }
                                }
                            },
                            "minimum": {"type": "number", "description": "For number."},
                            "maximum": {"type": "number", "description": "For number."},
                            "default": {"description": "The answer taken when the person gives none."},
                            "location": {
                                "type": "string",
                                "description": "path:line the question is about, shown beside it."
                            }
                        }
                    }
                }
            }
        }),
    }
}

/// The questions in `arguments` once they make a form, and what the model
/// is told; why they do not, otherwise.
pub(crate) fn check(arguments: &str) -> Result<(Value, String), String> {
    let arguments: Value =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let questions = arguments
        .get("questions")
        .and_then(Value::as_array)
        .ok_or("invalid arguments: `questions` must be a list")?;
    if questions.is_empty() || questions.len() > QUESTIONS_AT_ONCE {
        return Err(format!(
            "ask takes 1 to {QUESTIONS_AT_ONCE} questions, not {}",
            questions.len()
        ));
    }
    let mut lines = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        let number = index + 1;
        let text = question
            .get("question")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| format!("question {number} has no text"))?;
        let kind = question
            .get("kind")
            .and_then(Value::as_str)
            .filter(|kind| KINDS.contains(kind))
            .ok_or_else(|| {
                format!(
                    "question {number} needs a kind, one of {}",
                    KINDS.join(", ")
                )
            })?;
        let mut answer = kind.replace('_', " ");
        if CHOICES.contains(&kind) {
            let labels = labels(question, number)?;
            answer = format!(
                "{answer}: {} (recommended), {}",
                labels[0],
                labels[1..].join(", ")
            );
        }
        if kind == "number" {
            let bound = |key: &str| question.get(key).and_then(Value::as_f64);
            if let (Some(minimum), Some(maximum)) = (bound("minimum"), bound("maximum")) {
                if minimum > maximum {
                    return Err(format!("question {number}: minimum is above maximum"));
                }
                answer = format!("{answer} from {minimum} to {maximum}");
            }
        }
        lines.push(format!("{number}. {text} ({answer})"));
    }
    let told = format!(
        "Asked the person:\n{}\nTheir answers come in a later message. Go on with work that \
         does not depend on them; if none is left, end your answer and say what you are \
         waiting for.",
        lines.join("\n")
    );
    Ok((Value::Array(questions.clone()), told))
}

/// The option labels of a choice question: at least two, all different.
fn labels(question: &Value, number: usize) -> Result<Vec<String>, String> {
    let options = question
        .get("options")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if options.len() < 2 || options.len() > OPTIONS_AT_MOST {
        return Err(format!(
            "question {number} needs 2 to {OPTIONS_AT_MOST} options, not {}",
            options.len()
        ));
    }
    let mut labels: Vec<String> = Vec::new();
    for option in options {
        let label = option
            .get("label")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .ok_or_else(|| format!("an option of question {number} has no label"))?;
        if labels.iter().any(|known| known == label) {
            return Err(format!("question {number} has the option {label} twice"));
        }
        labels.push(label.to_string());
    }
    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_form_is_checked_and_summed_up_for_the_model() {
        let (questions, told) = check(
            &json!({"questions": [
                {"question": "Which database?", "kind": "choose_one",
                 "options": [{"label": "SQLite", "preview": "rusqlite = \"0.43\""},
                             {"label": "Postgres"}]},
                {"question": "How many workers?", "kind": "number", "minimum": 1, "maximum": 8},
                {"question": "Delete the old table?", "kind": "yes_no", "default": false}
            ]})
            .to_string(),
        )
        .expect("a form");
        assert_eq!(questions.as_array().map(Vec::len), Some(3));
        assert!(told.starts_with("Asked the person:\n1. Which database? (choose one: SQLite (recommended), Postgres)\n2. How many workers? (number from 1 to 8)\n3. Delete the old table? (yes no)\n"), "{told}");
        assert!(told.contains("Their answers come in a later message."));
    }

    #[test]
    fn forms_that_cannot_be_shown_are_refused() {
        let refused =
            |questions: Value| check(&json!({"questions": questions}).to_string()).unwrap_err();
        assert!(refused(json!([])).contains("1 to 4"));
        let five = vec![json!({"question": "q", "kind": "text"}); 5];
        assert!(refused(json!(five)).contains("not 5"));
        assert!(refused(json!([{"question": " ", "kind": "text"}])).contains("no text"));
        assert!(refused(json!([{"question": "q", "kind": "slider"}])).contains("needs a kind"));
        assert!(refused(
            json!([{"question": "q", "kind": "choose_one", "options": [{"label": "a"}]}])
        )
        .contains("2 to 10 options"));
        assert!(refused(json!([{"question": "q", "kind": "order",
            "options": [{"label": "a"}, {"label": "a"}]}]))
        .contains("twice"));
        assert!(
            refused(json!([{"question": "q", "kind": "number", "minimum": 3, "maximum": 1}]))
                .contains("minimum is above maximum")
        );
        assert!(check("{}").unwrap_err().contains("must be a list"));
    }
}
