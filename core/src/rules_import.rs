//! Rule import with the model splitting each section into rules by quoting
//! it. The harness accepts a split only if every quote occurs verbatim in
//! the section, reports source text assigned to nothing, and falls back to
//! the mechanical split on any failure. Unchanged sections are not sent
//! again. Design and sources: docs/evidence/rules-and-local-issues.md.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use rusqlite::params;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::conversation::{Body, Conversation, Message, Speaker};
use crate::rules::{insert_rule_row, split_rules, title_of, Block, ImportReport, RuleRow};
use crate::store::{now, store_error, NoteId, ProjectId, Store};
use crate::{ErrorInfo, Llm};

/// Sections larger than this are split mechanically.
const SEGMENT_MAX_BYTES: usize = 32 * 1024;

const INSTRUCTIONS: &str = "You split one section of a rules document into individual rules. \
Never paraphrase: every quote, reason and other entry must be copied character for character \
from the text, without the surrounding markers. One rule is one requirement; a rule may combine \
several quotes (for example a list item and its sub-items). Text that explains why a rule exists \
goes into that rule's reasons. Text that is neither a rule nor a reason goes into other. Every \
part of the text should appear in exactly one quote, reason or other entry. title, condition \
and keywords are your own short words in the document's language: condition says when the rule \
applies, keywords are space-separated search terms. Answer with JSON only, in this shape: \
{\"rules\":[{\"quotes\":[\"...\"],\"reasons\":[\"...\"],\"title\":\"...\",\"condition\":\"...\",\
\"keywords\":\"...\"}],\"other\":[\"...\"]}";

#[derive(Debug, Deserialize)]
struct Split {
    rules: Vec<SplitRule>,
    #[serde(default)]
    other: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SplitRule {
    quotes: Vec<String>,
    #[serde(default)]
    reasons: Vec<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    condition: String,
    #[serde(default)]
    keywords: String,
}

/// The blocks under one heading, joined as the model sees them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Section {
    pub(crate) name: String,
    pub(crate) text: String,
    blocks: Vec<Block>,
    /// Byte offset in `text` where each block starts.
    offsets: Vec<usize>,
}

impl Section {
    /// The source line of a byte offset in `text`.
    fn line_at(&self, offset: usize) -> i64 {
        let index = self
            .offsets
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        let block = &self.blocks[index];
        let within = offset
            .saturating_sub(self.offsets[index])
            .min(block.text.len());
        block.start + block.text[..within].matches('\n').count() as i64
    }

    fn hash(&self) -> String {
        Sha256::digest(self.text.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// Groups blocks by heading, keeping first-seen order.
pub(crate) fn sections(blocks: Vec<Block>) -> Vec<Section> {
    let mut order: Vec<Section> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for block in blocks {
        let at = *index.entry(block.section.clone()).or_insert_with(|| {
            order.push(Section {
                name: block.section.clone(),
                text: String::new(),
                blocks: Vec::new(),
                offsets: Vec::new(),
            });
            order.len() - 1
        });
        let section = &mut order[at];
        if !section.text.is_empty() {
            section.text.push_str("\n\n");
        }
        section.offsets.push(section.text.len());
        section.text.push_str(&block.text);
        section.blocks.push(block);
    }
    order
}

/// One rule cut from a section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) body: String,
    pub(crate) reason: String,
    pub(crate) title: String,
    pub(crate) condition: String,
    pub(crate) keywords: String,
    pub(crate) start: i64,
    pub(crate) end: i64,
}

/// Checks the model's answer against the section: every quote, reason and
/// other entry must occur verbatim. Returns the rules and the source text
/// left unassigned (markers and punctuation alone do not count).
pub(crate) fn accept(
    section: &Section,
    answer: &str,
) -> Result<(Vec<Segment>, Vec<String>), String> {
    let json = answer
        .find('{')
        .zip(answer.rfind('}'))
        .filter(|(start, end)| start < end)
        .map(|(start, end)| &answer[start..=end])
        .ok_or("the answer holds no JSON object")?;
    let split: Split = serde_json::from_str(json)
        .map_err(|error| format!("the answer is not the expected JSON: {error}"))?;
    if split.rules.is_empty() {
        return Err("the answer holds no rules".to_string());
    }
    let mut claimed: Vec<(usize, usize)> = Vec::new();
    let mut segments = Vec::with_capacity(split.rules.len());
    for rule in &split.rules {
        if rule.quotes.is_empty() {
            return Err("a rule has no quotes".to_string());
        }
        let mut lines = (i64::MAX, i64::MIN);
        for quote in &rule.quotes {
            let (start, end) = locate(&section.text, quote, &mut claimed)?;
            lines.0 = lines.0.min(section.line_at(start));
            lines.1 = lines.1.max(section.line_at(end.saturating_sub(1)));
        }
        for reason in &rule.reasons {
            locate(&section.text, reason, &mut claimed)?;
        }
        let body = rule.quotes.join("\n");
        segments.push(Segment {
            title: if rule.title.trim().is_empty() {
                title_of(&body)
            } else {
                rule.title.trim().to_string()
            },
            reason: rule.reasons.join("\n"),
            condition: rule.condition.trim().to_string(),
            keywords: rule.keywords.trim().to_string(),
            start: lines.0,
            end: lines.1,
            body,
        });
    }
    for other in &split.other {
        locate(&section.text, other, &mut claimed)?;
    }
    Ok((segments, unassigned(&section.text, claimed)))
}

/// Finds `quote` in `text`, preferring an occurrence not yet claimed.
fn locate(
    text: &str,
    quote: &str,
    claimed: &mut Vec<(usize, usize)>,
) -> Result<(usize, usize), String> {
    if quote.trim().is_empty() {
        return Err("an empty quote".to_string());
    }
    let mut first = None;
    for (start, _) in text.match_indices(quote) {
        let range = (start, start + quote.len());
        first.get_or_insert(range);
        if claimed.iter().all(|&(a, b)| range.1 <= a || b <= range.0) {
            claimed.push(range);
            return Ok(range);
        }
    }
    let range = first.ok_or_else(|| {
        let shown: String = quote.chars().take(60).collect();
        format!("a quote is not in the source: {shown:?}")
    })?;
    claimed.push(range);
    Ok(range)
}

/// Source fragments outside every claimed range that hold more than list
/// markers, heading marks and punctuation.
fn unassigned(text: &str, mut claimed: Vec<(usize, usize)>) -> Vec<String> {
    claimed.sort_unstable();
    let mut gaps = Vec::new();
    let mut cursor = 0;
    for (start, end) in claimed {
        if start > cursor {
            gaps.push(&text[cursor..start]);
        }
        cursor = cursor.max(end);
    }
    gaps.push(&text[cursor.min(text.len())..]);
    gaps.into_iter()
        .map(|gap| {
            gap.trim_matches(|c: char| {
                c.is_whitespace() || "-*+#>`|.:;,()[]0123456789、。".contains(c)
            })
        })
        .filter(|gap| gap.chars().any(char::is_alphanumeric))
        .map(str::to_string)
        .collect()
}

/// The mechanical split of a section, used when the model's split fails.
fn mechanical(section: &Section) -> Vec<Segment> {
    section
        .blocks
        .iter()
        .map(|block| Segment {
            body: block.text.clone(),
            reason: String::new(),
            title: title_of(&block.text),
            condition: block.section.clone(),
            keywords: String::new(),
            start: block.start,
            end: block.end,
        })
        .collect()
}

async fn ask(llm: &Llm, section: &Section) -> Result<String, ErrorInfo> {
    let conversation = Conversation {
        system: Some(INSTRUCTIONS.to_string()),
        messages: vec![Message {
            id: "section".to_string(),
            body: Body::User {
                from: Speaker::Harness("rule-import".to_string()),
                parts: vec![format!(
                    "Section: {}\n\n<<<TEXT\n{}\nTEXT>>>",
                    section.name, section.text
                )],
            },
        }],
    };
    let mut stream = llm.converse(&conversation, &[]).await?;
    let mut answer = String::new();
    while let Some(delta) = stream.next().await? {
        answer.push_str(&delta);
    }
    Ok(answer)
}

enum Plan {
    Unchanged(String),
    Replace {
        name: String,
        hash: Option<String>,
        segments: Vec<Segment>,
        by: &'static str,
    },
}

impl Store {
    /// Imports a rules document with `llm` splitting each changed section
    /// into rules by quotation. A section the model splits wrongly, or one
    /// too large, is split mechanically and retried on the next import.
    /// Within a section, rules with the same text are kept, new ones added
    /// and the rest retired; sections that disappeared are retired.
    pub async fn import_rules_with(
        &self,
        llm: &Llm,
        project: Option<ProjectId>,
        path: &str,
        markdown: &str,
    ) -> Result<ImportReport, ErrorInfo> {
        let sections = sections(split_rules(markdown));
        let scope = project.map_or(0, |p| p.0);
        let lookup_path = path.to_string();
        let known: HashMap<String, String> = self
            .read(Duration::from_secs(30), move |db| {
                let mut statement = db
                    .prepare_cached(
                        "SELECT section, hash FROM rule_sections WHERE scope = ?1 AND path = ?2",
                    )
                    .map_err(store_error)?;
                let rows = statement
                    .query_map(params![scope, lookup_path], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })
                    .map_err(store_error)?;
                rows.collect::<Result<_, _>>().map_err(store_error)
            })
            .await?;

        let mut report = ImportReport::default();
        let mut plans = Vec::with_capacity(sections.len());
        for section in &sections {
            let hash = section.hash();
            if known.get(&section.name) == Some(&hash) {
                plans.push(Plan::Unchanged(section.name.clone()));
                continue;
            }
            let outcome = if section.text.len() > SEGMENT_MAX_BYTES {
                Err(format!("the section exceeds {SEGMENT_MAX_BYTES} bytes"))
            } else {
                match ask(llm, section).await {
                    Ok(answer) => accept(section, &answer),
                    Err(error) => Err(error.message),
                }
            };
            plans.push(match outcome {
                Ok((segments, unassigned)) => {
                    report.unassigned.extend(unassigned);
                    Plan::Replace {
                        name: section.name.clone(),
                        hash: Some(hash),
                        segments,
                        by: "model",
                    }
                }
                Err(reason) => {
                    report.fallbacks.push((section.name.clone(), reason));
                    Plan::Replace {
                        name: section.name.clone(),
                        hash: None,
                        segments: mechanical(section),
                        by: "mechanical",
                    }
                }
            });
        }

        let path = path.to_string();
        self.write(move |db| {
            let tx = db.transaction().map_err(store_error)?;
            let time = now();
            let mut existing: HashMap<String, Vec<(NoteId, String)>> = HashMap::new();
            {
                let mut statement = tx
                    .prepare(
                        "SELECT r.note_id, r.section, n.body FROM rules r
                         JOIN notes n ON n.id = r.note_id
                         WHERE r.origin_path = ?1 AND n.project_id IS ?2
                           AND n.superseded_by IS NULL AND n.status != 'retired'",
                    )
                    .map_err(store_error)?;
                let rows = statement
                    .query_map(params![path, project.map(|p| p.0)], |row| {
                        Ok((row.get::<_, String>(1)?, (NoteId(row.get(0)?), row.get(2)?)))
                    })
                    .map_err(store_error)?;
                for row in rows {
                    let (section, rule) = row.map_err(store_error)?;
                    existing.entry(section).or_default().push(rule);
                }
            }
            let mut seen: HashSet<String> = HashSet::new();
            for plan in &plans {
                match plan {
                    Plan::Unchanged(name) => {
                        seen.insert(name.clone());
                        report.unchanged += existing.remove(name).map_or(0, |rules| rules.len());
                    }
                    Plan::Replace {
                        name,
                        hash,
                        segments,
                        by,
                    } => {
                        seen.insert(name.clone());
                        let mut old = existing.remove(name).unwrap_or_default();
                        for (ordinal, segment) in segments.iter().enumerate() {
                            if let Some(index) = old.iter().position(|(_, body)| *body == segment.body) {
                                let (id, _) = old.swap_remove(index);
                                tx.execute(
                                    "UPDATE rules SET origin_start = ?1, origin_end = ?2, ordinal = ?3
                                     WHERE note_id = ?4",
                                    params![segment.start, segment.end, ordinal as i64 + 1, id.0],
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
                                    section: name,
                                    ordinal: ordinal as i64 + 1,
                                    start: segment.start,
                                    end: segment.end,
                                    body: &segment.body,
                                    title: &segment.title,
                                    reason: &segment.reason,
                                    condition: &segment.condition,
                                    keywords: &segment.keywords,
                                    segmented_by: by,
                                },
                                time,
                            )?;
                            report.added += 1;
                        }
                        report.retired += retire(&tx, &old, time)?;
                        match hash {
                            Some(hash) => tx.execute(
                                "INSERT OR REPLACE INTO rule_sections(scope, path, section, hash)
                                 VALUES (?1, ?2, ?3, ?4)",
                                params![scope, path, name, hash],
                            ),
                            None => tx.execute(
                                "DELETE FROM rule_sections WHERE scope = ?1 AND path = ?2 AND section = ?3",
                                params![scope, path, name],
                            ),
                        }
                        .map_err(store_error)?;
                    }
                }
            }
            for (name, rules) in existing {
                report.retired += retire(&tx, &rules, time)?;
                if !seen.contains(&name) {
                    tx.execute(
                        "DELETE FROM rule_sections WHERE scope = ?1 AND path = ?2 AND section = ?3",
                        params![scope, path, name],
                    )
                    .map_err(store_error)?;
                }
            }
            tx.commit().map_err(store_error)?;
            Ok(report)
        })
        .await
    }
}

fn retire(
    db: &rusqlite::Connection,
    rules: &[(NoteId, String)],
    time: i64,
) -> Result<usize, ErrorInfo> {
    for (id, _) in rules {
        db.execute(
            "UPDATE notes SET status = 'retired', valid_to = ?1, updated = ?1 WHERE id = ?2",
            params![time, id.0],
        )
        .map_err(store_error)?;
    }
    Ok(rules.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# Rules\n\n## Scope\n- Do only what was asked. Keep changes small.\n- Never push without approval.\n  - Ask first, every time.\n\nWhy: pushes are public.\n";

    fn scope() -> Section {
        sections(split_rules(DOC)).remove(0)
    }

    #[test]
    fn sections_join_blocks_and_map_offsets_to_lines() {
        let all = sections(split_rules(DOC));
        assert_eq!(all.len(), 1);
        let section = &all[0];
        assert_eq!(section.name, "Rules > Scope");
        assert_eq!(
            section.text,
            "- Do only what was asked. Keep changes small.\n\n- Never push without approval.\n\n- Ask first, every time.\n\nWhy: pushes are public."
        );
        let ask = section.text.find("Ask first").expect("present");
        assert_eq!(section.line_at(ask), 6);
        assert_eq!(section.line_at(0), 4);
    }

    #[test]
    fn a_verbatim_split_is_accepted_with_reasons_and_lines() {
        let answer = r#"Here you go:
```json
{"rules":[
  {"quotes":["Do only what was asked."],"title":"Scope","condition":"always","keywords":"scope"},
  {"quotes":["Keep changes small."]},
  {"quotes":["Never push without approval.","Ask first, every time."],"reasons":["Why: pushes are public."],
   "title":"No unapproved push","condition":"before git push","keywords":"git push"}
],"other":[]}
```"#;
        let (segments, unassigned) = accept(&scope(), answer).expect("accepted");
        assert!(unassigned.is_empty(), "{unassigned:?}");
        assert_eq!(segments.len(), 3);
        assert_eq!(
            segments[1].title, "Keep changes small.",
            "a missing title falls back to the text"
        );
        let push = &segments[2];
        assert_eq!(
            push.body,
            "Never push without approval.\nAsk first, every time."
        );
        assert_eq!(push.reason, "Why: pushes are public.");
        assert_eq!((push.start, push.end), (5, 6));
        assert_eq!(push.condition, "before git push");
    }

    #[test]
    fn paraphrase_and_omission_are_caught() {
        let paraphrased = r#"{"rules":[{"quotes":["Do not push without asking."]}]}"#;
        assert!(accept(&scope(), paraphrased)
            .unwrap_err()
            .contains("not in the source"));
        assert!(accept(&scope(), r#"{"rules":[]}"#).is_err());
        assert!(accept(&scope(), "no json here").is_err());
        assert!(accept(&scope(), r#"{"rules":[{"quotes":["  "]}]}"#).is_err());

        let partial = r#"{"rules":[{"quotes":["Never push without approval."]}]}"#;
        let (_, unassigned) = accept(&scope(), partial).expect("accepted");
        assert_eq!(
            unassigned,
            [
                "Do only what was asked. Keep changes small",
                "Ask first, every time.\n\nWhy: pushes are public"
            ]
        );
    }
}
