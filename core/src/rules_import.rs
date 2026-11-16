//! Rule import with the model splitting each section into rules. The
//! section is cut into numbered units (sentences and lines) and the model
//! answers with unit numbers only, so it cannot paraphrase and the stored
//! rules are slices of the source. Units assigned to nothing are reported,
//! any failure falls back to the mechanical split, unchanged sections are
//! not sent again, and sections are sent concurrently. Design and sources:
//! docs/evidence/rules-and-local-issues.md.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::params;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::conversation::{Body, Conversation, Message, Speaker};
use crate::rules::{insert_rule_row, split_rules, title_of, Block, ImportReport, RuleRow};
use crate::store::{now, store_error, NoteId, ProjectId, Store};
use crate::{ErrorInfo, Llm};

/// Sections larger than this are split mechanically.
const SEGMENT_MAX_BYTES: usize = 32 * 1024;
/// Sections in flight at once.
const CONCURRENT_SECTIONS: usize = 4;

const INSTRUCTIONS: &str = "You split one section of a rules document into individual rules. \
The section is given as numbered units (sentences or lines). Answer with unit numbers only. One \
rule is one requirement; a rule may combine several units (for example a list item and its \
sub-items). Units that explain why a rule exists go into that rule's reasons. Units that are \
neither a rule nor a reason go into other. Every unit should appear exactly once. title, \
condition and keywords are your own short words in the document's language: condition says when \
the rule applies, keywords are space-separated search terms. Answer with JSON only, in this \
shape: {\"rules\":[{\"units\":[1,2],\"reasons\":[3],\"title\":\"...\",\"condition\":\"...\",\
\"keywords\":\"...\"}],\"other\":[4]}";

#[derive(Debug, Deserialize)]
struct Split {
    rules: Vec<SplitRule>,
    #[serde(default)]
    other: Vec<usize>,
}

#[derive(Debug, Deserialize)]
struct SplitRule {
    units: Vec<usize>,
    #[serde(default)]
    reasons: Vec<usize>,
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
    /// Byte ranges of the numbered units, in order.
    units: Vec<(usize, usize)>,
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
        // Bytes, not `str`: the offset may fall inside a multi-byte character.
        let newlines = block.text.as_bytes()[..within]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count();
        block.start + newlines as i64
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
                units: Vec::new(),
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
    for section in &mut order {
        section.units = units(&section.text);
    }
    order
}

/// Sentences and lines of `text` as byte ranges, trimmed and non-empty. A
/// sentence ends at `。`, `！`, `？`, or a `.` followed by a space.
fn units(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut push = |start: usize, end: usize| {
        let piece = &text[start..end];
        let lead = piece.len() - piece.trim_start().len();
        let trail = piece.len() - piece.trim_end().len();
        if start + lead < end - trail {
            out.push((start + lead, end - trail));
        }
    };
    let mut line_start = 0;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let mut start = line_start;
        let mut chars = content.char_indices().peekable();
        while let Some((index, c)) = chars.next() {
            let end = line_start + index + c.len_utf8();
            let boundary = matches!(c, '。' | '！' | '？')
                || (c == '.' && chars.peek().is_some_and(|&(_, next)| next == ' '));
            if boundary {
                push(start, end);
                start = end;
            }
        }
        push(start, line_start + content.len());
        line_start += line.len();
    }
    out
}

impl Section {
    /// The units as the model sees them: `[n] text`, one per line.
    fn numbered(&self) -> String {
        let mut out = String::with_capacity(self.text.len() + self.units.len() * 6);
        for (index, &(start, end)) in self.units.iter().enumerate() {
            out.push_str(&format!("[{}] ", index + 1));
            out.push_str(&self.text[start..end].replace('\n', " "));
            out.push('\n');
        }
        out
    }

    /// The source text of the given units: runs of consecutive units are one
    /// slice of the original (keeping its markup and line breaks), and runs
    /// are joined by a line break.
    fn slices(&self, ids: &[usize]) -> String {
        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for id in sorted {
            match runs.last_mut() {
                Some(run) if run.1 + 1 == id => run.1 = id,
                _ => runs.push((id, id)),
            }
        }
        runs.iter()
            .map(|&(first, last)| &self.text[self.units[first - 1].0..self.units[last - 1].1])
            .collect::<Vec<_>>()
            .join("\n")
    }
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

/// Checks the model's answer against the section: every unit number must
/// exist. Returns the rules and the units assigned to nothing (markers and
/// punctuation alone do not count).
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
    let count = section.units.len();
    let mut used = vec![false; count + 1];
    let mut check = |ids: &[usize]| -> Result<(), String> {
        for &id in ids {
            if id == 0 || id > count {
                return Err(format!(
                    "unit {id} does not exist (the section has {count})"
                ));
            }
            used[id] = true;
        }
        Ok(())
    };
    let mut segments = Vec::with_capacity(split.rules.len());
    for rule in &split.rules {
        if rule.units.is_empty() {
            return Err("a rule names no units".to_string());
        }
        check(&rule.units)?;
        check(&rule.reasons)?;
        let first = rule.units.iter().min().copied().unwrap_or(1);
        let last = rule.units.iter().max().copied().unwrap_or(1);
        let body = section.slices(&rule.units);
        segments.push(Segment {
            title: if rule.title.trim().is_empty() {
                title_of(&body)
            } else {
                rule.title.trim().to_string()
            },
            reason: if rule.reasons.is_empty() {
                String::new()
            } else {
                section.slices(&rule.reasons)
            },
            condition: rule.condition.trim().to_string(),
            keywords: rule.keywords.trim().to_string(),
            start: section.line_at(section.units[first - 1].0),
            end: section.line_at(section.units[last - 1].1 - 1),
            body,
        });
    }
    check(&split.other)?;
    let unassigned = section
        .units
        .iter()
        .enumerate()
        .filter(|(index, _)| !used[index + 1])
        .map(|(_, &(start, end))| section.text[start..end].to_string())
        .filter(|text| {
            text.trim_matches(|c: char| {
                c.is_whitespace() || "-*+#>`|.:;,()[]0123456789、。".contains(c)
            })
            .chars()
            .any(char::is_alphanumeric)
        })
        .collect();
    Ok((segments, unassigned))
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
                    "Heading (context only, not a unit): {}\n\nUnits:\n{}",
                    section.name,
                    section.numbered()
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
    /// into rules by unit number. A section the model splits wrongly, or one
    /// too large, is split mechanically and retried on the next import.
    /// Within a section, rules with the same text are kept, new ones added
    /// and the rest retired; sections that disappeared are retired.
    pub async fn import_rules_with(
        &self,
        llm: Arc<Llm>,
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

        let mut answers: Vec<Option<Result<String, String>>> = vec![None; sections.len()];
        let slots = Arc::new(Semaphore::new(CONCURRENT_SECTIONS));
        let mut asking = JoinSet::new();
        for (index, section) in sections.iter().enumerate() {
            if known.get(&section.name) == Some(&section.hash()) {
                continue;
            }
            if section.text.len() > SEGMENT_MAX_BYTES {
                answers[index] = Some(Err(format!(
                    "the section exceeds {SEGMENT_MAX_BYTES} bytes"
                )));
                continue;
            }
            let (llm, slots, section) = (Arc::clone(&llm), Arc::clone(&slots), section.clone());
            asking.spawn(async move {
                let _slot = slots.acquire_owned().await;
                (
                    index,
                    ask(&llm, &section).await.map_err(|error| error.message),
                )
            });
        }
        while let Some(done) = asking.join_next().await {
            let (index, answer) = done.map_err(|error| {
                ErrorInfo::new("store", format!("a split task failed: {error}"))
            })?;
            answers[index] = Some(answer);
        }

        let mut report = ImportReport::default();
        let mut plans = Vec::with_capacity(sections.len());
        for (section, answer) in sections.iter().zip(answers) {
            let Some(answer) = answer else {
                plans.push(Plan::Unchanged(section.name.clone()));
                continue;
            };
            let outcome = answer.and_then(|answer| accept(section, &answer));
            plans.push(match outcome {
                Ok((segments, unassigned)) => {
                    report.unassigned.extend(unassigned);
                    Plan::Replace {
                        name: section.name.clone(),
                        hash: Some(section.hash()),
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

    const DOC: &str = "# Rules\n\n## Scope\n- Do only what was asked. Keep changes **small**.\n- Never push without approval.\n  - Ask first, every time.\n\nWhy: pushes are public.\n";

    fn scope() -> Section {
        sections(split_rules(DOC)).remove(0)
    }

    #[test]
    fn sections_are_cut_into_numbered_units() {
        let section = scope();
        assert_eq!(section.name, "Rules > Scope");
        assert_eq!(
            section.numbered(),
            "[1] - Do only what was asked.\n[2] Keep changes **small**.\n[3] - Never push without approval.\n[4] - Ask first, every time.\n[5] Why: pushes are public.\n"
        );
        let ask = section.text.find("Ask first").expect("present");
        assert_eq!(section.line_at(ask), 6);
        assert_eq!(section.line_at(0), 4);
        let japanese = units("依頼したことだけを実装する。正しさを優先する。\n二行目");
        assert_eq!(japanese.len(), 3);
    }

    #[test]
    fn rules_are_slices_of_the_source() {
        let answer = r#"Here you go:
```json
{"rules":[
  {"units":[1],"title":"Scope","condition":"always","keywords":"scope"},
  {"units":[2]},
  {"units":[3,4],"reasons":[5],"title":"No unapproved push","condition":"before git push","keywords":"git push"}
],"other":[]}
```"#;
        let (segments, unassigned) = accept(&scope(), answer).expect("accepted");
        assert!(unassigned.is_empty(), "{unassigned:?}");
        assert_eq!(
            segments[1].body, "Keep changes **small**.",
            "markup survives"
        );
        assert_eq!(
            segments[1].title, "Keep changes **small**.",
            "a missing title falls back"
        );
        let push = &segments[2];
        assert_eq!(
            push.body, "- Never push without approval.\n\n- Ask first, every time.",
            "consecutive units are one slice of the original"
        );
        assert_eq!(push.reason, "Why: pushes are public.");
        assert_eq!((push.start, push.end), (5, 6));
    }

    #[test]
    fn line_numbers_hold_for_multi_byte_text() {
        let section = sections(split_rules(
            "## 目的\n依頼したことだけを実装する。\n正しさを優先する。\n",
        ))
        .remove(0);
        let answer = r#"{"rules":[{"units":[1]},{"units":[2]}]}"#;
        let (segments, unassigned) = accept(&section, answer).expect("accepted");
        assert!(unassigned.is_empty());
        assert_eq!(segments[0].body, "依頼したことだけを実装する。");
        assert_eq!((segments[0].start, segments[0].end), (2, 2));
        assert_eq!((segments[1].start, segments[1].end), (3, 3));
    }

    #[test]
    fn bad_answers_are_refused_and_omissions_reported() {
        assert!(accept(&scope(), r#"{"rules":[{"units":[9]}]}"#)
            .unwrap_err()
            .contains("does not exist"));
        assert!(accept(&scope(), r#"{"rules":[{"units":[0]}]}"#).is_err());
        assert!(accept(&scope(), r#"{"rules":[]}"#).is_err());
        assert!(accept(&scope(), r#"{"rules":[{"units":[]}]}"#).is_err());
        assert!(accept(&scope(), "no json here").is_err());
        let partial = r#"{"rules":[{"units":[3]}]}"#;
        let (_, unassigned) = accept(&scope(), partial).expect("accepted");
        assert_eq!(
            unassigned,
            [
                "- Do only what was asked.",
                "Keep changes **small**.",
                "- Ask first, every time.",
                "Why: pushes are public."
            ]
        );
    }
}
