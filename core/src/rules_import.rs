//! Rule import as verbatim chunks. A rules document is cut into fixed-size
//! overlapping chunks by character count (no grammar is interpreted); each
//! chunk is stored as written and is one rule entry. When a model is given,
//! it writes self-contained rule sentences for each chunk, which serve only
//! as search keys. Design and sources: docs/evidence/rules-and-local-issues.md.

use std::collections::HashMap;
use std::sync::Arc;

use rusqlite::params;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::conversation::{Body, Conversation, Message, Speaker};
use crate::rules::{insert_rule_row, title_of, ImportReport, RuleRow};
use crate::store::{now, store_error, NoteId, ProjectId, Store};
use crate::Llm;

/// Chunk text sent to the model per request.
const REQUEST_MAX_BYTES: usize = 32 * 1024;
/// Bounds on what the model may attach to one chunk.
const SENTENCES_PER_CHUNK: usize = 50;
const SENTENCE_MAX_CHARS: usize = 500;

const INSTRUCTIONS: &str = "Each numbered chunk below is an excerpt of a rules document for a \
coding agent. Chunks overlap and may cut sentences. For each chunk, write the rules it states as \
self-contained sentences in the document's language: one rule per sentence, each understandable \
on its own (spell out the subject, condition and object that the text leaves implicit). Do not \
add anything the chunk does not state. A chunk that states no rule gets an empty list. Answer \
with JSON only: {\"chunks\":[{\"id\":1,\"rules\":[\"...\"]}]}";

/// How a document is cut. Sizes are in characters; the best values are
/// not known, so they are settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunking {
    pub chars: usize,
    pub overlap: usize,
}

impl Default for Chunking {
    fn default() -> Self {
        Self {
            chars: 800,
            overlap: 200,
        }
    }
}

/// A verbatim slice of the document and its source lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Chunk {
    pub(crate) text: String,
    pub(crate) start: i64,
    pub(crate) end: i64,
}

/// Windows of `chars` characters, each starting `chars - overlap` after the
/// previous one; the last ends at the end of the text.
pub(crate) fn chunks(text: &str, chunking: Chunking) -> Vec<Chunk> {
    let boundaries: Vec<usize> = text
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect();
    let count = boundaries.len() - 1;
    let size = chunking.chars.max(1);
    let step = size.saturating_sub(chunking.overlap).max(1);
    let line_of = |offset: usize| {
        1 + text.as_bytes()[..offset]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count() as i64
    };
    let mut out = Vec::new();
    let mut first = 0;
    while first < count {
        let last = (first + size).min(count);
        let (start, end) = (boundaries[first], boundaries[last]);
        let piece = &text[start..end];
        if !piece.trim().is_empty() {
            let lead = piece.len() - piece.trim_start().len();
            let trail = piece.len() - piece.trim_end().len();
            out.push(Chunk {
                text: piece.trim().to_string(),
                start: line_of(start + lead),
                end: line_of(end - trail - 1),
            });
        }
        if last == count {
            break;
        }
        first += step;
    }
    out
}

#[derive(Debug, Deserialize)]
struct Answer {
    chunks: Vec<AnswerChunk>,
}

#[derive(Debug, Deserialize)]
struct AnswerChunk {
    id: usize,
    #[serde(default)]
    rules: Vec<String>,
}

/// Reads the model's sentences for `count` chunks numbered from 1. Unknown
/// ids are an error; chunks the answer skips get no sentences.
pub(crate) fn read_sentences(answer: &str, count: usize) -> Result<Vec<Vec<String>>, String> {
    let json = answer
        .find('{')
        .zip(answer.rfind('}'))
        .filter(|(start, end)| start < end)
        .map(|(start, end)| &answer[start..=end])
        .ok_or("the answer holds no JSON object")?;
    let parsed: Answer = serde_json::from_str(json)
        .map_err(|error| format!("the answer is not the expected JSON: {error}"))?;
    let mut sentences = vec![Vec::new(); count];
    for chunk in parsed.chunks {
        if chunk.id == 0 || chunk.id > count {
            return Err(format!(
                "chunk {} does not exist (there are {count})",
                chunk.id
            ));
        }
        sentences[chunk.id - 1] = chunk
            .rules
            .into_iter()
            .map(|sentence| {
                sentence
                    .trim()
                    .chars()
                    .take(SENTENCE_MAX_CHARS)
                    .collect::<String>()
            })
            .filter(|sentence| !sentence.is_empty())
            .take(SENTENCES_PER_CHUNK)
            .collect();
    }
    Ok(sentences)
}

/// Asks for the sentences of `texts`, in requests of at most
/// [`REQUEST_MAX_BYTES`], one after another.
async fn ask(llm: &Llm, texts: &[&str]) -> Result<Vec<Vec<String>>, String> {
    let mut all = Vec::with_capacity(texts.len());
    let mut batch_start = 0;
    while batch_start < texts.len() {
        let mut batch_end = batch_start;
        let mut bytes = 0;
        while batch_end < texts.len()
            && (batch_end == batch_start || bytes + texts[batch_end].len() <= REQUEST_MAX_BYTES)
        {
            bytes += texts[batch_end].len();
            batch_end += 1;
        }
        let batch = &texts[batch_start..batch_end];
        let mut prompt = String::with_capacity(bytes + batch.len() * 32);
        for (index, text) in batch.iter().enumerate() {
            let id = index + 1;
            prompt.push_str(&format!("<<<CHUNK {id}\n{text}\nCHUNK {id}>>>\n\n"));
        }
        let conversation = Conversation {
            system: Some(INSTRUCTIONS.to_string()),
            messages: vec![Message {
                id: "chunks".to_string(),
                body: Body::User {
                    from: Speaker::Harness("rule-import".to_string()),
                    parts: vec![prompt],
                },
            }],
        };
        let mut stream = llm
            .converse(&conversation, &[])
            .await
            .map_err(|error| error.message)?;
        let mut answer = String::new();
        while let Some(delta) = stream.next().await.map_err(|error| error.message)? {
            answer.push_str(&delta);
        }
        all.extend(read_sentences(&answer, batch.len())?);
        batch_start = batch_end;
    }
    Ok(all)
}

impl Store {
    /// Imports a rules document as verbatim chunks. A chunk whose text is
    /// already stored is kept, a new one is added, and stored chunks the
    /// document no longer contains are retired. With `llm`, chunks without
    /// search sentences get them; a failed request leaves them without and
    /// is retried on the next import. An unchanged document is not read
    /// again once every chunk has its sentences.
    pub async fn import_rules(
        &self,
        llm: Option<Arc<Llm>>,
        project: Option<ProjectId>,
        path: &str,
        text: &str,
        chunking: Chunking,
    ) -> Result<ImportReport, crate::ErrorInfo> {
        let scope = project.map_or(0, |p| p.0);
        let hash: String =
            Sha256::digest(format!("{}:{}\n{text}", chunking.chars, chunking.overlap).as_bytes())
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
                        "SELECT r.note_id, n.body, n.summary FROM rules r
                         JOIN notes n ON n.id = r.note_id
                         WHERE r.origin_path = ?1 AND n.project_id IS ?2
                           AND n.superseded_by IS NULL AND n.status != 'retired'",
                    )
                    .map_err(store_error)?;
                let rows = statement
                    .query_map(params![path, project.map(|p| p.0)], |row| {
                        Ok((
                            row.get::<_, String>(1)?,
                            (NoteId(row.get(0)?), row.get::<_, String>(2)?),
                        ))
                    })
                    .map_err(store_error)?;
                let existing: HashMap<String, (NoteId, String)> =
                    rows.collect::<Result<_, _>>().map_err(store_error)?;
                Ok((known, existing))
            })
            .await?
        };
        if known_hash.as_deref() == Some(hash.as_str()) {
            return Ok(ImportReport {
                unchanged: existing.len(),
                ..Default::default()
            });
        }

        let chunks = chunks(text, chunking);
        // Chunks that need sentences: new ones, and kept ones still without.
        let needing: Vec<usize> = (0..chunks.len())
            .filter(|&index| {
                existing
                    .get(&chunks[index].text)
                    .is_none_or(|(_, summary)| summary.is_empty())
            })
            .collect();
        let mut sentences: HashMap<usize, Vec<String>> = HashMap::new();
        let mut keys_error = None;
        if let Some(llm) = &llm {
            if !needing.is_empty() {
                let texts: Vec<&str> = needing
                    .iter()
                    .map(|&index| chunks[index].text.as_str())
                    .collect();
                match ask(llm, &texts).await {
                    Ok(answers) => sentences.extend(needing.iter().copied().zip(answers)),
                    Err(error) => keys_error = Some(error),
                }
            }
        }

        let complete = llm.is_some() && keys_error.is_none();
        self.write(move |db| {
            let tx = db.transaction().map_err(store_error)?;
            let time = now();
            let mut existing = existing;
            let mut report = ImportReport {
                keys_error,
                ..Default::default()
            };
            for (index, chunk) in chunks.iter().enumerate() {
                let found = sentences.get(&index).filter(|found| !found.is_empty());
                if let Some((id, _)) = existing.remove(&chunk.text) {
                    tx.execute(
                        "UPDATE rules SET origin_start = ?1, origin_end = ?2, ordinal = ?3
                         WHERE note_id = ?4",
                        params![chunk.start, chunk.end, index as i64 + 1, id.0],
                    )
                    .map_err(store_error)?;
                    if let Some(found) = found {
                        tx.execute(
                            "UPDATE notes SET title = ?1, summary = ?2, updated = ?3 WHERE id = ?4",
                            params![found[0], found.join("\n"), time, id.0],
                        )
                        .map_err(store_error)?;
                        tx.execute(
                            "UPDATE rules SET segmented_by = 'model' WHERE note_id = ?1",
                            [id.0],
                        )
                        .map_err(store_error)?;
                    }
                    report.unchanged += 1;
                    continue;
                }
                let title = found.map_or_else(|| title_of(&chunk.text), |found| found[0].clone());
                let summary = found.map(|found| found.join("\n")).unwrap_or_default();
                insert_rule_row(
                    &tx,
                    project,
                    &path,
                    &RuleRow {
                        section: "",
                        ordinal: index as i64 + 1,
                        start: chunk.start,
                        end: chunk.end,
                        body: &chunk.text,
                        title: &title,
                        summary: &summary,
                        condition: "",
                        keywords: "",
                        segmented_by: if found.is_some() {
                            "model"
                        } else {
                            "mechanical"
                        },
                    },
                    time,
                )?;
                report.added += 1;
            }
            for (id, _) in existing.into_values() {
                tx.execute(
                    "UPDATE notes SET status = 'retired', valid_to = ?1, updated = ?1 WHERE id = ?2",
                    params![time, id.0],
                )
                .map_err(store_error)?;
                report.retired += 1;
            }
            if complete {
                tx.execute(
                    "INSERT OR REPLACE INTO rule_sections(scope, path, section, hash)
                     VALUES (?1, ?2, '', ?3)",
                    params![scope, path, hash],
                )
                .map_err(store_error)?;
            } else {
                tx.execute(
                    "DELETE FROM rule_sections WHERE scope = ?1 AND path = ?2 AND section = ''",
                    params![scope, path],
                )
                .map_err(store_error)?;
            }
            tx.commit().map_err(store_error)?;
            Ok(report)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_overlap_and_keep_the_text_as_written() {
        let text = "一二三四五六七八九十\n壱弐参肆伍";
        let pieces = chunks(
            text,
            Chunking {
                chars: 6,
                overlap: 2,
            },
        );
        let texts: Vec<&str> = pieces.iter().map(|chunk| chunk.text.as_str()).collect();
        assert_eq!(
            texts,
            ["一二三四五六", "五六七八九十", "九十\n壱弐参", "弐参肆伍"]
        );
        assert_eq!((pieces[2].start, pieces[2].end), (1, 2));
        assert_eq!((pieces[3].start, pieces[3].end), (2, 2));
        assert!(chunks("  \n ", Chunking::default()).is_empty());
        let whole = chunks("short", Chunking::default());
        assert_eq!(whole.len(), 1);
    }

    #[test]
    fn sentences_are_read_and_bounded() {
        let answer = r#"```json
{"chunks":[{"id":2,"rules":["Cの時はBをやらない。", "  "]},{"id":1,"rules":[]}]}
```"#;
        let sentences = read_sentences(answer, 3).expect("read");
        assert_eq!(sentences[0], Vec::<String>::new());
        assert_eq!(sentences[1], ["Cの時はBをやらない。"]);
        assert!(sentences[2].is_empty(), "a skipped chunk gets none");
        assert!(read_sentences(r#"{"chunks":[{"id":4,"rules":[]}]}"#, 3).is_err());
        assert!(read_sentences(r#"{"chunks":[{"id":0}]}"#, 3).is_err());
        assert!(read_sentences("no json", 3).is_err());
        let long = format!(
            r#"{{"chunks":[{{"id":1,"rules":["{}"]}}]}}"#,
            "x".repeat(900)
        );
        assert_eq!(
            read_sentences(&long, 1).expect("read")[0][0]
                .chars()
                .count(),
            500
        );
    }
}
