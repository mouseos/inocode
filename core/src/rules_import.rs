//! Rule import by whole-document extraction. The model reads the entire
//! document and returns each rule with the passages it rests on; the harness
//! finds those passages in the document (ignoring markup and whitespace
//! differences) and stores the original text as the rule. The model's own
//! wording only widens search. Nothing here depends on the document's
//! language. Design and sources: docs/evidence/rules-and-local-issues.md.

use std::collections::HashMap;
use std::sync::Arc;

use rusqlite::params;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::conversation::{Body, Conversation, Message, Speaker, StopReason};
use crate::rules::{insert_rule_row, title_of, ImportReport, RuleRow};
use crate::store::{now, store_error, NoteId, ProjectId, Store};
use crate::{JsonOutput, Llm};

/// Documents larger than this are stored whole instead of extracted.
const DOCUMENT_MAX_BYTES: usize = 64 * 1024;
/// Characters a source may differ by: markup and emphasis marks.
const MARKUP: &[char] = &['*', '_', '`', '#', '>', '~'];

const INSTRUCTIONS: &str = "The text below is a rules document for a coding agent. Read the \
whole document and extract every rule it contains.
- One element per rule. Split a sentence that holds several rules; merge into one rule a rule \
that spans several sentences or list items.
- Where the document later corrects, overrides or narrows an earlier instruction, merge it into \
the original rule and put both passages in sources.
- statement must be understandable without the rest of the document: spell out the subject, \
condition, object, and what words such as \"it\", \"that\" or \"in that case\" refer to, using \
the whole document.
- Do not add anything the document does not say.
- condition: when the rule applies; null if always.
- exceptions: exceptions the document states; empty if none.
- reason: the reason the document gives; null if none.
- strength: \"must\" if the document explicitly forbids or requires it, otherwise \"should\".
- sources: the passages the rule rests on, copied character for character from the document, \
including markup; one entry per passage.
- section: the heading the rule appears under; null if there is none.
- Write statement, condition, exceptions and reason in the same language as the document. \
Never translate sources.
Answer with JSON only.";

/// The answer shape, in the strict subset structured output accepts.
fn schema() -> Value {
    let nullable = json!({"type": ["string", "null"]});
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["rules"],
        "properties": {
            "rules": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["statement", "condition", "exceptions", "reason", "strength", "sources", "section"],
                    "properties": {
                        "statement": {"type": "string"},
                        "condition": nullable,
                        "exceptions": {"type": "array", "items": {"type": "string"}},
                        "reason": nullable,
                        "strength": {"type": "string", "enum": ["must", "should"]},
                        "sources": {"type": "array", "items": {"type": "string"}},
                        "section": nullable
                    }
                }
            }
        }
    })
}

#[derive(Debug, Deserialize)]
struct Extraction {
    rules: Vec<Extracted>,
}

#[derive(Debug, Deserialize)]
struct Extracted {
    statement: String,
    #[serde(default)]
    condition: Option<String>,
    #[serde(default)]
    exceptions: Vec<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    strength: String,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    section: Option<String>,
}

/// A rule ready to store.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Prepared {
    /// The located source passages, in document order; the model's
    /// statement when none could be located.
    pub(crate) body: String,
    pub(crate) title: String,
    /// Search text: the statement, exceptions and reason.
    pub(crate) summary: String,
    pub(crate) condition: String,
    pub(crate) section: String,
    pub(crate) start: i64,
    pub(crate) end: i64,
    pub(crate) details: Value,
    pub(crate) segmented_by: &'static str,
}

/// The document with markup removed and whitespace runs folded to one
/// space, and for every byte of it the byte offset it came from.
struct Folded {
    text: String,
    origin: Vec<usize>,
}

fn fold(text: &str) -> Folded {
    let mut folded = Folded {
        text: String::with_capacity(text.len()),
        origin: Vec::with_capacity(text.len()),
    };
    let mut in_space = false;
    for (offset, c) in text.char_indices() {
        if MARKUP.contains(&c) {
            continue;
        }
        if c.is_whitespace() {
            if !in_space {
                folded.text.push(' ');
                folded.origin.push(offset);
                in_space = true;
            }
            continue;
        }
        in_space = false;
        folded.text.push(c);
        folded
            .origin
            .extend(std::iter::repeat_n(offset, c.len_utf8()));
    }
    folded
}

/// Whether `passage` occurs in `document`, ignoring markup and whitespace
/// differences.
pub(crate) fn quotes(document: &str, passage: &str) -> bool {
    let passage = fold(passage);
    let passage = passage.text.trim();
    !passage.is_empty() && fold(document).text.contains(passage)
}

/// Finds a source passage in the document: verbatim first, then with markup
/// and whitespace differences ignored. Unclaimed occurrences are preferred.
/// Returns the byte range in the original document.
fn locate(
    document: &str,
    folded: &Folded,
    passage: &str,
    claimed: &mut Vec<(usize, usize)>,
) -> Option<(usize, usize)> {
    let passage = passage.trim();
    if passage.is_empty() {
        return None;
    }
    let free = |range: (usize, usize), claimed: &[(usize, usize)]| {
        claimed.iter().all(|&(a, b)| range.1 <= a || b <= range.0)
    };
    let mut candidates: Vec<(usize, usize)> = document
        .match_indices(passage)
        .map(|(start, found)| (start, start + found.len()))
        .collect();
    if candidates.is_empty() {
        let needle = fold(passage).text;
        let needle = needle.trim();
        if needle.is_empty() {
            return None;
        }
        candidates = folded
            .text
            .match_indices(needle)
            .map(|(start, found)| {
                let last = folded.origin[start + found.len() - 1];
                let width = document[last..].chars().next().map_or(1, char::len_utf8);
                (folded.origin[start], last + width)
            })
            .collect();
    }
    let range = candidates
        .iter()
        .copied()
        .find(|&range| free(range, claimed))
        .or_else(|| candidates.first().copied())?;
    claimed.push(range);
    Some(range)
}

fn line_of(document: &str, offset: usize) -> i64 {
    1 + document.as_bytes()[..offset]
        .iter()
        .filter(|&&byte| byte == b'\n')
        .count() as i64
}

/// What [`prepare`] makes of an answer.
pub(crate) struct Outcome {
    pub(crate) rules: Vec<Prepared>,
    /// Sources that could not be found in the document.
    pub(crate) unverified: usize,
    /// Document text no located source covers, trimmed of markers and
    /// punctuation at its ends, when it holds a letter.
    pub(crate) uncovered: Vec<String>,
}

/// Turns the model's answer into rules. Sources that cannot be located are
/// kept in the rule's details and counted.
pub(crate) fn prepare(document: &str, answer: &str) -> Result<Outcome, String> {
    let json = answer
        .find('{')
        .zip(answer.rfind('}'))
        .filter(|(start, end)| start < end)
        .map(|(start, end)| &answer[start..=end])
        .ok_or("the answer holds no JSON object")?;
    let extraction: Extraction = serde_json::from_str(json)
        .map_err(|error| format!("the answer is not the expected JSON: {error}"))?;
    if extraction.rules.is_empty() {
        return Err("the answer holds no rules".to_string());
    }
    let folded = fold(document);
    let mut claimed: Vec<(usize, usize)> = Vec::new();
    let mut unverified_total = 0;
    let mut rules = Vec::with_capacity(extraction.rules.len());
    for rule in extraction.rules {
        let statement = rule.statement.trim().to_string();
        if statement.is_empty() {
            continue;
        }
        let mut located: Vec<(usize, usize)> = Vec::new();
        let mut unverified: Vec<String> = Vec::new();
        for source in &rule.sources {
            match locate(document, &folded, source, &mut claimed) {
                Some(range) => located.push(range),
                None if !source.trim().is_empty() => unverified.push(source.clone()),
                None => {}
            }
        }
        unverified_total += unverified.len();
        located.sort_unstable();
        located.dedup();
        let verified = !located.is_empty();
        let body = if verified {
            located
                .iter()
                .map(|&(start, end)| &document[start..end])
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            statement.clone()
        };
        let (start, end) = match (located.first(), located.last()) {
            (Some(&(start, _)), Some(&(_, end))) => {
                (line_of(document, start), line_of(document, end - 1))
            }
            _ => (0, 0),
        };
        let mut summary = vec![statement.clone()];
        summary.extend(rule.exceptions.iter().cloned());
        summary.extend(rule.reason.iter().cloned());
        rules.push(Prepared {
            title: title_of(&statement),
            summary: summary.join("\n"),
            condition: rule.condition.clone().unwrap_or_default(),
            section: rule.section.clone().unwrap_or_default(),
            start,
            end,
            details: json!({
                "statement": statement,
                "condition": rule.condition,
                "exceptions": rule.exceptions,
                "reason": rule.reason,
                "suggested_strength": rule.strength,
                "section": rule.section,
                "unverified_sources": unverified,
            }),
            segmented_by: if verified {
                "model"
            } else {
                "model_unverified"
            },
            body,
        });
    }
    claimed.sort_unstable();
    let mut uncovered = Vec::new();
    let mut cursor = 0;
    for (start, end) in claimed
        .into_iter()
        .chain(std::iter::once((document.len(), document.len())))
    {
        if start > cursor {
            // Markers and punctuation at the ends are not content.
            let gap = document[cursor..start].trim_matches(|c: char| !c.is_alphanumeric());
            if gap.chars().any(char::is_alphabetic) {
                uncovered.push(gap.to_string());
            }
        }
        cursor = cursor.max(end);
    }
    Ok(Outcome {
        rules,
        unverified: unverified_total,
        uncovered,
    })
}

/// The whole document as one rule, used without a model or when the
/// extraction fails.
fn whole(document: &str) -> Vec<Prepared> {
    let text = document.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let offset = document.find(text).unwrap_or(0);
    vec![Prepared {
        body: text.to_string(),
        title: title_of(text),
        summary: String::new(),
        condition: String::new(),
        section: String::new(),
        start: line_of(document, offset),
        end: line_of(document, offset + text.len() - 1),
        details: json!({}),
        segmented_by: "whole",
    }]
}

async fn extract(llm: &Llm, document: &str) -> Result<String, String> {
    let conversation = Conversation {
        system: Some(INSTRUCTIONS.to_string()),
        messages: vec![Message {
            id: "document".to_string(),
            body: Body::User {
                from: Speaker::Harness("rule-import".to_string()),
                parts: vec![format!(
                    "----- DOCUMENT START -----\n{document}\n----- DOCUMENT END -----"
                )],
            },
        }],
    };
    let output = JsonOutput {
        name: "rule_extraction".to_string(),
        schema: schema(),
    };
    let mut stream = llm
        .converse_json(&conversation, &output)
        .await
        .map_err(|error| error.message)?;
    let mut answer = String::new();
    while let Some(delta) = stream.next().await.map_err(|error| error.message)? {
        answer.push_str(&delta);
    }
    if let Body::Assistant {
        stop: StopReason::Length,
        ..
    } = stream.assistant()
    {
        return Err(format!(
            "the answer reached the output limit after {} characters",
            answer.chars().count()
        ));
    }
    Ok(answer)
}

impl Store {
    /// Imports a rules document. With `llm`, the model extracts the rules
    /// from the whole document in one request and each rule is stored as the
    /// source passages it rests on; without one, or when the extraction
    /// fails, the document is stored as a single rule and the extraction is
    /// retried on the next import. Rules whose text is already stored are
    /// kept, new ones added, and the rest retired. An unchanged document is
    /// not sent again once it has been extracted.
    pub async fn import_rules(
        &self,
        llm: Option<Arc<Llm>>,
        project: Option<ProjectId>,
        path: &str,
        document: &str,
    ) -> Result<ImportReport, crate::ErrorInfo> {
        let scope = project.map_or(0, |p| p.0);
        let hash: String = Sha256::digest(document.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let path = path.to_string();
        let (known_hash, existing) = {
            let path = path.clone();
            self.read(std::time::Duration::from_secs(30), move |db| {
                let known: Option<String> = db
                    .query_row(
                        "SELECT hash FROM rule_sections
                         WHERE scope = ?1 AND path = ?2 AND section = ''",
                        params![scope, path],
                        |row| row.get(0),
                    )
                    .ok();
                let mut statement = db
                    .prepare_cached(
                        "SELECT r.note_id, n.body FROM rules r
                         JOIN notes n ON n.id = r.note_id
                         WHERE r.origin_path = ?1 AND n.project_id IS ?2
                           AND n.superseded_by IS NULL AND n.status != 'retired'",
                    )
                    .map_err(store_error)?;
                let rows = statement
                    .query_map(params![path, project.map(|p| p.0)], |row| {
                        Ok((row.get::<_, String>(1)?, NoteId(row.get(0)?)))
                    })
                    .map_err(store_error)?;
                let mut existing: HashMap<String, Vec<NoteId>> = HashMap::new();
                for row in rows {
                    let (body, id) = row.map_err(store_error)?;
                    existing.entry(body).or_default().push(id);
                }
                Ok((known, existing))
            })
            .await?
        };
        if known_hash.as_deref() == Some(hash.as_str()) {
            return Ok(ImportReport {
                unchanged: existing.values().map(Vec::len).sum(),
                ..Default::default()
            });
        }

        let mut report = ImportReport::default();
        let rules = match llm {
            None => whole(document),
            Some(_) if document.len() > DOCUMENT_MAX_BYTES => {
                report.extraction_error =
                    Some(format!("the document exceeds {DOCUMENT_MAX_BYTES} bytes"));
                whole(document)
            }
            Some(llm) => match extract(&llm, document)
                .await
                .and_then(|answer| prepare(document, &answer))
            {
                Ok(prepared) => {
                    report.unverified_sources = prepared.unverified;
                    report.uncovered = prepared.uncovered;
                    prepared.rules
                }
                Err(error) => {
                    report.extraction_error = Some(error);
                    whole(document)
                }
            },
        };
        let extracted = rules.iter().any(|rule| rule.segmented_by != "whole");

        self.write(move |db| {
            let tx = db.transaction().map_err(store_error)?;
            let time = now();
            let mut existing = existing;
            for (index, rule) in rules.iter().enumerate() {
                let details = rule.details.to_string();
                if let Some(id) = existing.get_mut(&rule.body).and_then(Vec::pop) {
                    tx.execute(
                        "UPDATE notes SET title = ?1, summary = ?2, condition = ?3, updated = ?4
                         WHERE id = ?5",
                        params![rule.title, rule.summary, rule.condition, time, id.0],
                    )
                    .map_err(store_error)?;
                    tx.execute(
                        "UPDATE rules SET origin_start = ?1, origin_end = ?2, ordinal = ?3,
                                          section = ?4, details = ?5, segmented_by = ?6
                         WHERE note_id = ?7",
                        params![
                            rule.start,
                            rule.end,
                            index as i64 + 1,
                            rule.section,
                            details,
                            rule.segmented_by,
                            id.0
                        ],
                    )
                    .map_err(store_error)?;
                    report.unchanged += 1;
                    continue;
                }
                insert_rule_row(
                    &tx,
                    project,
                    &path,
                    &RuleRow {
                        section: &rule.section,
                        ordinal: index as i64 + 1,
                        start: rule.start,
                        end: rule.end,
                        body: &rule.body,
                        title: &rule.title,
                        summary: &rule.summary,
                        condition: &rule.condition,
                        keywords: "",
                        segmented_by: rule.segmented_by,
                        details: &details,
                    },
                    time,
                )?;
                report.added += 1;
            }
            for id in existing.into_values().flatten() {
                tx.execute(
                    "UPDATE notes SET status = 'retired', valid_to = ?1, updated = ?1 WHERE id = ?2",
                    params![time, id.0],
                )
                .map_err(store_error)?;
                report.retired += 1;
            }
            if extracted {
                tx.execute(
                    "INSERT OR REPLACE INTO rule_sections(scope, path, section, hash)
                     VALUES (?1, ?2, '', ?3)",
                    params![scope, path, hash],
                )
            } else {
                tx.execute(
                    "DELETE FROM rule_sections WHERE scope = ?1 AND path = ?2 AND section = ''",
                    params![scope, path],
                )
            }
            .map_err(store_error)?;
            tx.commit().map_err(store_error)?;
            Ok(report)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# Rules\n\n- Never **push** without approval.\n- Write comments in English.\n\nThanks a lot!\n";

    #[test]
    fn sources_are_found_despite_markup_and_spacing() {
        let answer = r#"{"rules":[
            {"statement":"Do not push without approval.","condition":"before pushing","exceptions":[],"reason":null,"strength":"must","sources":["Never push without approval."],"section":"Rules"},
            {"statement":"Comments are in English.","condition":null,"exceptions":[],"reason":null,"strength":"should","sources":["Write   comments in English.", "made up text"],"section":"Rules"}
        ]}"#;
        let prepared = prepare(DOC, answer).expect("prepared");
        let rules = &prepared.rules;
        assert_eq!(
            rules[0].body, "Never **push** without approval.",
            "the original text, markup kept"
        );
        assert_eq!((rules[0].start, rules[0].end), (3, 3));
        assert_eq!(rules[0].condition, "before pushing");
        assert_eq!(rules[1].body, "Write comments in English.");
        assert_eq!(prepared.unverified, 1);
        assert_eq!(
            rules[1].details["unverified_sources"],
            json!(["made up text"])
        );
        assert_eq!(prepared.uncovered, ["Rules", "Thanks a lot"]);
    }

    #[test]
    fn any_language_and_unlocated_rules() {
        let document = "保存する前にバックアップを取る。\nDateien nur nach Rückfrage löschen.\n";
        let answer = r#"{"rules":[
            {"statement":"保存前にバックアップする。","condition":null,"exceptions":[],"reason":null,"strength":"must","sources":["保存する前にバックアップを取る。"],"section":null},
            {"statement":"Dateien nur nach Rückfrage löschen.","condition":null,"exceptions":[],"reason":null,"strength":"must","sources":["Dateien nur nach Rückfrage löschen."],"section":null},
            {"statement":"Something unsupported.","condition":null,"exceptions":[],"reason":null,"strength":"should","sources":["nowhere"],"section":null}
        ]}"#;
        let prepared = prepare(document, answer).expect("prepared");
        let rules = &prepared.rules;
        assert_eq!(rules[0].body, "保存する前にバックアップを取る。");
        assert_eq!((rules[1].start, rules[1].end), (2, 2));
        assert_eq!(rules[2].segmented_by, "model_unverified");
        assert_eq!(rules[2].body, "Something unsupported.");
        assert_eq!(prepared.unverified, 1);
        assert!(prepared.uncovered.is_empty());
    }

    #[test]
    fn bad_answers_are_refused() {
        assert!(prepare(DOC, "no json").is_err());
        assert!(prepare(DOC, r#"{"rules":[]}"#).is_err());
        assert!(prepare(DOC, r#"{"other":1}"#).is_err());
        let fenced =
            "```json\n{\"rules\":[{\"statement\":\"x\",\"sources\":[\"Thanks a lot!\"]}]}\n```";
        assert_eq!(prepare(DOC, fenced).expect("lenient").rules.len(), 1);
    }

    #[test]
    fn the_whole_document_is_one_rule_without_a_model() {
        let rules = whole("\n\n  body text\nmore  \n");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].body, "body text\nmore");
        assert_eq!((rules[0].start, rules[0].end), (3, 4));
        assert!(whole("  \n").is_empty());
    }
}
